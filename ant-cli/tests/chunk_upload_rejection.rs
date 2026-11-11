//! The actual CLI must explain recognizable-file rejection before setup errors.

use std::io::Write as _;
use std::process::{Command, Output, Stdio};

fn command(manifest: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ant"));
    command
        .env_remove("SECRET_KEY")
        .env_remove("RUST_LOG")
        .arg("--devnet-manifest")
        .arg(manifest);
    command
}

fn assert_clear_rejection(output: &Output) {
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("recognizable file header"), "{stderr}");
    assert!(stderr.contains("self-encrypt"), "{stderr}");
    assert!(stderr.contains("ant file upload"), "{stderr}");
    assert!(!stderr.contains("SECRET_KEY"), "{stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
fn known_file_headers_get_clear_errors_without_a_wallet_or_network() {
    let directory = tempfile::tempdir().expect("temporary input directory");
    let manifest = directory.path().join("does-not-exist.json");
    for (name, content) in [
        ("image.png", b"\x89PNG\r\n\x1a\n".as_slice()),
        ("database.sqlite", b"SQLite format 3\x00"),
        ("document.xml", b"<?xml version='1.0'?><root/>"),
        ("archive.gz", b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\xff"),
    ] {
        let input = directory.path().join(name);
        std::fs::write(&input, content).expect("write raw file");
        let output = command(&manifest)
            .args(["chunk", "put"])
            .arg(&input)
            .output()
            .expect("run ant");
        assert_clear_rejection(&output);
        // Even a configured wallet must not cause the nonexistent manifest to
        // hide the content error or make the CLI try to connect.
        let output = command(&manifest)
            .env("SECRET_KEY", "01".repeat(32))
            .args(["chunk", "put"])
            .arg(&input)
            .output()
            .expect("run ant with wallet");
        assert_clear_rejection(&output);
    }
}

#[test]
fn stdin_gets_the_same_clear_rejection() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let manifest = directory.path().join("does-not-exist.json");
    let mut child = command(&manifest)
        .args(["chunk", "put"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run ant");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"\x89PNG\r\n\x1a\n")
        .expect("write raw file to stdin");
    let output = child.wait_with_output().expect("CLI output");
    assert_clear_rejection(&output);
}

#[test]
fn self_encrypted_content_reaches_the_normal_wallet_check() {
    let directory = tempfile::tempdir().expect("temporary input directory");
    let manifest = directory.path().join("does-not-exist.json");
    let mut file = b"\x89PNG\r\n\x1a\n".to_vec();
    file.resize(4096, 0xa5);
    let encrypted = ant_core::browser::encrypt_public_file(&file).expect("self-encrypt file");
    let record = encrypted.records.first().expect("encrypted record");
    let input = directory.path().join("encrypted.chunk");
    std::fs::write(&input, &record.content).expect("write encrypted chunk");
    let output = command(&manifest)
        .args(["chunk", "put"])
        .arg(&input)
        .output()
        .expect("run ant");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("SECRET_KEY"), "{stderr}");
    assert!(!stderr.contains("recognizable file header"), "{stderr}");
}
