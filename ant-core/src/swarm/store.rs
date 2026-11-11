//! Records this client holds for other clients: every one checked against
//! its address, at most a fixed number of bytes in all, oldest dropped first.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, PoisonError};

use bytes::Bytes;

use crate::data::XorName;

/// What a record costs of the store: its bytes, but never less than
/// [`MIN_CHARGE`], so a file listing many tiny records cannot fill memory
/// with entries.
const MIN_CHARGE: usize = 4 * 1024;

fn charge(record: &Bytes) -> usize {
    record.len().max(MIN_CHARGE)
}

/// Verified records this client can serve.
#[derive(Debug)]
pub(crate) struct RecordStore {
    inner: Mutex<Inner>,
    max_bytes: usize,
}

#[derive(Debug, Default)]
struct Inner {
    records: HashMap<XorName, Bytes>,
    order: VecDeque<XorName>,
    bytes: usize,
}

impl RecordStore {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            max_bytes,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keeps a record the caller has checked against its address: `None` if
    /// it is held already, or larger than the whole store; otherwise the
    /// addresses of the records dropped to make room for it, oldest first,
    /// which links told of them must be told are gone.
    pub(crate) fn insert(&self, address: XorName, record: Bytes) -> Option<Vec<XorName>> {
        let cost = charge(&record);
        if cost > self.max_bytes {
            return None;
        }
        let mut inner = self.lock();
        if inner.records.contains_key(&address) {
            return None;
        }
        let mut dropped = Vec::new();
        while inner.bytes + cost > self.max_bytes {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            if let Some(record) = inner.records.remove(&oldest) {
                inner.bytes = inner.bytes.saturating_sub(charge(&record));
                dropped.push(oldest);
            }
        }
        inner.bytes += cost;
        inner.order.push_back(address);
        inner.records.insert(address, record);
        Some(dropped)
    }

    pub(crate) fn get(&self, address: &XorName) -> Option<Bytes> {
        self.lock().records.get(address).cloned()
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, address: &XorName) -> bool {
        self.lock().records.contains_key(address)
    }

    /// Up to `limit` addresses held that `keep` accepts, newest first.
    pub(crate) fn newest_where(
        &self,
        limit: usize,
        keep: impl Fn(&XorName) -> bool,
    ) -> Vec<XorName> {
        self.lock()
            .order
            .iter()
            .rev()
            .filter(|address| keep(address))
            .take(limit)
            .copied()
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn bytes(&self) -> usize {
        self.lock().bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_oldest_records_go_first_when_full() {
        let record = |byte: u8| Bytes::from(vec![byte; MIN_CHARGE]);
        let store = RecordStore::new(2 * MIN_CHARGE + MIN_CHARGE / 2);
        assert_eq!(store.insert([1; 32], record(1)), Some(vec![]));
        assert_eq!(store.insert([2; 32], record(2)), Some(vec![]));
        assert_eq!(store.insert([2; 32], record(2)), None);
        // The oldest makes room, and the caller learns which it was.
        assert_eq!(store.insert([3; 32], record(3)), Some(vec![[1; 32]]));
        assert_eq!(store.get(&[1; 32]), None);
        assert!(!store.contains(&[1; 32]));
        assert!(store.contains(&[3; 32]));
        assert_eq!(store.newest_where(8, |_| true), vec![[3; 32], [2; 32]]);
        assert_eq!(store.newest_where(1, |_| true), vec![[3; 32]]);
        assert_eq!(
            store.newest_where(8, |address| *address == [2; 32]),
            vec![[2; 32]]
        );
        assert_eq!(store.bytes(), 2 * MIN_CHARGE);
        assert_eq!(
            store.insert([4; 32], Bytes::from(vec![0u8; 3 * MIN_CHARGE])),
            None
        );
        // One record larger than those it pushes out drops them all.
        assert_eq!(
            store.insert([5; 32], Bytes::from(vec![0u8; 2 * MIN_CHARGE])),
            Some(vec![[2; 32], [3; 32]])
        );
        assert_eq!(store.bytes(), 2 * MIN_CHARGE);
    }

    #[test]
    fn a_tiny_record_costs_as_much_as_the_least_charge() {
        let store = RecordStore::new(4 * MIN_CHARGE);
        for seed in 0..100u8 {
            assert!(store
                .insert([seed; 32], Bytes::from_static(b"abc"))
                .is_some());
        }
        // However small, at most four fit.
        assert_eq!(store.newest_where(usize::MAX, |_| true).len(), 4);
        assert_eq!(store.bytes(), 4 * MIN_CHARGE);
    }
}
