//! On-disk cache for single-node (non-merkle) chunk payment proofs.
//!
//! Why this exists
//! ---------------
//! Single-node uploads break the file into payment waves. Each wave is
//! one EVM transaction that produces N per-chunk payment proofs (one
//! per chunk in the wave). The proof bytes are what the storer needs
//! to accept a PUT — without them, the on-chain payment is "stranded":
//! the chain saw the tokens move but the client can no longer prove to
//! a storer that any specific chunk was paid for.
//!
//! Before this module, those proofs lived only in process memory. If
//! the upload died mid-file (network flake, residual close-K stress,
//! a Ctrl-C), every wave already paid for was unrecoverable and the
//! user had to re-quote and re-pay on the next attempt.
//!
//! This module persists the `(chunk_address, proof_bytes)` pair to
//! disk **immediately after each wave's `batch_pay` confirms**, before
//! the wave's PUT phase begins. On the next upload attempt for the same
//! source file, the cache is loaded and any chunk whose address matches
//! the current encryption skips quote+pay and goes straight to PUT.
//!
//! Lifecycle
//! ---------
//! * **append_wave** — called once per successfully paid wave, before
//!   the PUT phase. Adds the wave's `(addr, proof_bytes)` entries to
//!   the on-disk receipt and updates the cumulative cost figures.
//! * **load_for_file** — called once at the top of the upload. If a
//!   non-expired cached receipt exists for the file, the proofs are
//!   merged into the upload plan and the matching chunks skip quoting
//!   and payment.
//! * **delete_for_file** — called after a fully successful upload to
//!   remove the receipt so a future re-upload of the same path pays
//!   anew.
//! * **cleanup_outdated** — called opportunistically inside
//!   `load_for_file` to garbage-collect receipts past the expiry
//!   window.
//!
//! Filename format
//! ---------------
//! Same as `cached_merkle`: `<timestamp>_<file_hash>` under
//! `<data_dir>/payments/single/`. The subdirectory keeps single-node
//! and merkle caches from colliding (they have different on-disk
//! schemas) and makes it easy for a user to wipe one without touching
//! the other.
//!
//! Expiry
//! ------
//! On-chain quote receipts have a finite validity window
//! (`QUOTE_MAX_AGE_SECS` in `ant-node`, currently 24 h). After that,
//! storers reject the proof even if the file is otherwise resumable.
//! The cache uses a conservative 24 h expiry to match.
//!
//! Failure-mode tolerance
//! ----------------------
//! All public-facing API (`try_*` variants) swallows IO and
//! serialization errors with a `warn!` log. A busted cache never
//! prevents a real upload — at worst the user re-pays.

use crate::config;
use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, DirEntry, File};
use std::hash::{Hash, Hasher};
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

/// Cached single-node receipts older than this are removed from disk.
///
/// Conservative match for `QUOTE_MAX_AGE_SECS` in `ant-node` (24 h).
/// After that window, storers will reject the cached proof even if
/// the file is otherwise resumable, so keeping the cache wouldn't help.
const PAYMENT_EXPIRATION_SECS: u64 = 24 * 60 * 60;

/// Subdirectory under the platform-appropriate data dir.
///
/// `payments/single` rather than `payments/` directly so the merkle
/// cache (in `payments/`) and this cache cannot collide on filename.
const PAYMENTS_SUBDIR: &str = "payments/single";

/// On-disk schema for a single-node (non-merkle) upload receipt.
///
/// Designed to be appended to: each successful wave adds its chunk
/// proofs to `proofs` and bumps the cumulative cost fields. The whole
/// file is rewritten on each append (the size is bounded by the chunk
/// count, so this is fine in practice — a 1 GB upload at 1 MB/chunk
/// gives ~1000 entries × ~1 KB proof ≈ 1 MB receipt file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SingleNodeReceipt {
    /// Per-chunk serialized `PaymentProof` bytes, keyed by content address.
    pub proofs: HashMap<[u8; 32], Vec<u8>>,
    /// Unix timestamp (seconds) the first wave was paid. Used for the
    /// 24 h expiry check.
    pub first_pay_timestamp: u64,
    /// Cumulative storage cost in atto, summed across all paid waves.
    pub storage_cost_atto: String,
    /// Cumulative gas cost in wei, summed across all paid waves.
    pub gas_cost_wei: u128,
}

impl SingleNodeReceipt {
    fn new(now_secs: u64) -> Self {
        Self {
            proofs: HashMap::new(),
            first_pay_timestamp: now_secs,
            storage_cost_atto: "0".to_string(),
            gas_cost_wei: 0,
        }
    }
}

fn payments_dir() -> Result<PathBuf> {
    let dir = config::data_dir()?.join(PAYMENTS_SUBDIR);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Short non-cryptographic hash of the source file path string, used
/// as the on-disk cache key.
///
/// Same scheme as `cached_merkle::file_hash_key`. Collisions are
/// content-validated against the current encrypted chunk addresses
/// before being trusted.
fn file_hash_key(file_path: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    file_path.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn receipt_path(dir: &Path, ts: u64, key: &str) -> PathBuf {
    dir.join(format!("{ts}_{key}"))
}

/// Append a wave's worth of paid-chunk proofs to the on-disk receipt.
///
/// If no receipt exists yet for this file, one is created with the
/// current time as `first_pay_timestamp`. Otherwise the existing
/// file is loaded, extended with the new proofs, and rewritten.
pub fn append_wave(
    file_path: &str,
    new_proofs: HashMap<[u8; 32], Vec<u8>>,
    wave_storage_cost_atto: &str,
    wave_gas_cost_wei: u128,
) -> Result<PathBuf> {
    let dir = payments_dir()?;
    let key = file_hash_key(file_path);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Find an existing receipt for this file (non-expired) and load
    // it, or create a fresh one stamped with now().
    let (path, mut receipt) = match find_existing(&dir, &key)? {
        Some((p, r)) => (p, r),
        None => (receipt_path(&dir, now, &key), SingleNodeReceipt::new(now)),
    };

    receipt.proofs.extend(new_proofs);
    if let (Ok(prev), Ok(add)) = (
        receipt.storage_cost_atto.parse::<u128>(),
        wave_storage_cost_atto.parse::<u128>(),
    ) {
        receipt.storage_cost_atto = prev.saturating_add(add).to_string();
    }
    receipt.gas_cost_wei = receipt.gas_cost_wei.saturating_add(wave_gas_cost_wei);

    write_receipt(&path, &receipt)?;
    debug!(
        "Appended {} proofs to single-node receipt for {file_path:?} ({})",
        receipt.proofs.len(),
        path.display()
    );
    Ok(path)
}

/// Best-effort `append_wave`. Logs on failure, returns nothing.
///
/// Intended for the hot path: if we can't persist the receipt the
/// upload still proceeds, the user just loses resume capability for
/// that wave.
pub fn try_append_wave(
    file_path: &str,
    new_proofs: HashMap<[u8; 32], Vec<u8>>,
    wave_storage_cost_atto: &str,
    wave_gas_cost_wei: u128,
) {
    if let Err(e) = append_wave(
        file_path,
        new_proofs,
        wave_storage_cost_atto,
        wave_gas_cost_wei,
    ) {
        warn!(
            "Failed to cache single-node payment receipt for {file_path:?}: {e}. \
             Upload will proceed without resume support for this wave."
        );
    }
}

/// Load the cached single-node receipt for a source file path, if any.
///
/// Side-effect: opportunistically removes expired receipts.
pub fn load_for_file(file_path: &str) -> Result<Option<(PathBuf, SingleNodeReceipt)>> {
    cleanup_outdated();
    let dir = payments_dir()?;
    let key = file_hash_key(file_path);
    find_existing(&dir, &key)
}

/// Best-effort load. Logs and returns `None` on error.
pub fn try_load_for_file(file_path: &str) -> Option<(PathBuf, SingleNodeReceipt)> {
    match load_for_file(file_path) {
        Ok(opt) => opt,
        Err(e) => {
            warn!(
                "Failed to look up cached single-node receipt for {file_path:?}: {e}. \
                 Starting a fresh upload."
            );
            None
        }
    }
}

pub fn delete_for_file(file_path: &str) -> Result<()> {
    let dir = payments_dir()?;
    let key = file_hash_key(file_path);
    if let Ok(read_dir) = fs::read_dir(&dir) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.contains(&key) {
                    let _ = fs::remove_file(&path);
                    debug!("Deleted cached single-node receipt {}", path.display());
                }
            }
        }
    }
    Ok(())
}

pub fn try_delete_for_file(file_path: &str) {
    if let Err(e) = delete_for_file(file_path) {
        warn!(
            "Failed to delete cached single-node receipt for {file_path:?}: {e}. \
             Will be cleaned up after expiry."
        );
    }
}

/// Garbage-collect cached receipts past the expiry window.
pub fn cleanup_outdated() {
    let Ok(dir) = payments_dir() else {
        return;
    };
    let Ok(read_dir) = fs::read_dir(&dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        if is_expired_entry(&entry) {
            let path = entry.path();
            info!(
                "Removing expired cached single-node payment file: {}",
                path.display()
            );
            let _ = fs::remove_file(path);
        }
    }
}

fn find_existing(dir: &Path, key: &str) -> Result<Option<(PathBuf, SingleNodeReceipt)>> {
    let read_dir = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            debug!("Could not read payments dir {}: {e}", dir.display());
            return Ok(None);
        }
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.contains(key) {
            continue;
        }
        if is_expired_filename(name) {
            continue;
        }
        match read_receipt(&path) {
            Ok(receipt) => {
                info!(
                    "Found previous single-node upload attempt, resuming with \
                     {} cached proofs from {}",
                    receipt.proofs.len(),
                    path.display()
                );
                return Ok(Some((path, receipt)));
            }
            Err(e) => {
                warn!(
                    "Cached single-node receipt at {} is unreadable ({e}). \
                     Ignoring and starting a fresh upload.",
                    path.display()
                );
            }
        }
    }
    Ok(None)
}

fn is_expired_entry(entry: &DirEntry) -> bool {
    let path = entry.path();
    if !path.is_file() {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    is_expired_filename(name)
}

fn is_expired_filename(name: &str) -> bool {
    let ts_str = match name.split_once('_') {
        Some((ts, _)) => ts,
        None => return false,
    };
    let Ok(ts) = ts_str.parse::<u64>() else {
        return false;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now > ts.saturating_add(PAYMENT_EXPIRATION_SECS)
}

fn read_receipt(path: &Path) -> Result<SingleNodeReceipt> {
    let handle = File::open(path)?;
    let receipt: SingleNodeReceipt = rmp_serde::decode::from_read(BufReader::new(handle))
        .map_err(|e| crate::error::Error::Io(std::io::Error::other(e.to_string())))?;
    Ok(receipt)
}

fn write_receipt(path: &Path, receipt: &SingleNodeReceipt) -> Result<()> {
    let handle = File::create(path)?;
    rmp_serde::encode::write(&mut BufWriter::new(handle), receipt)
        .map_err(|e| crate::error::Error::Io(std::io::Error::other(e.to_string())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_receipt(ts: u64) -> SingleNodeReceipt {
        let mut proofs: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        proofs.insert([1u8; 32], vec![1, 2, 3]);
        SingleNodeReceipt {
            proofs,
            first_pay_timestamp: ts,
            storage_cost_atto: "100".to_string(),
            gas_cost_wei: 200,
        }
    }

    #[test]
    fn file_hash_key_is_stable() {
        assert_eq!(file_hash_key("/tmp/a"), file_hash_key("/tmp/a"));
        assert_ne!(file_hash_key("/tmp/a"), file_hash_key("/tmp/b"));
    }

    #[test]
    fn expired_filename_detected() {
        let stale = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(PAYMENT_EXPIRATION_SECS + 60);
        assert!(is_expired_filename(&format!("{stale}_abc")));

        let fresh = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(60);
        assert!(!is_expired_filename(&format!("{fresh}_abc")));
    }

    #[test]
    fn malformed_filename_is_not_expired() {
        assert!(!is_expired_filename("nonsense"));
        assert!(!is_expired_filename("not_a_number_abc"));
    }

    #[test]
    fn roundtrip_save_load_delete() -> Result<()> {
        let file_path = format!(
            "/tmp/anselme-resumable-single-test-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        );
        let mut wave1: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        wave1.insert([2u8; 32], vec![10, 20]);
        let path1 = append_wave(&file_path, wave1, "50", 100)?;
        assert!(path1.exists());

        let mut wave2: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        wave2.insert([3u8; 32], vec![30, 40]);
        let path2 = append_wave(&file_path, wave2, "70", 50)?;
        // Same file path: should be the same on-disk path (we don't
        // create a new timestamped file per wave).
        assert_eq!(path1, path2);

        let (loaded_path, loaded) = load_for_file(&file_path)?.expect("receipt should load");
        assert_eq!(loaded_path, path1);
        assert_eq!(loaded.proofs.len(), 2);
        assert!(loaded.proofs.contains_key(&[2u8; 32]));
        assert!(loaded.proofs.contains_key(&[3u8; 32]));
        // Cumulative cost summed across waves.
        assert_eq!(loaded.storage_cost_atto, "120");
        assert_eq!(loaded.gas_cost_wei, 150);

        delete_for_file(&file_path)?;
        assert!(load_for_file(&file_path)?.is_none());
        Ok(())
    }

}
