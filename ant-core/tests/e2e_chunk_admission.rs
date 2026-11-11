//! Paired-branch test: real QUIC nodes, disk storage, and Anvil payments.
//!
//! Requires the enforcing node from WithAutonomi/ant-node#286. Run with:
//! `cargo --config 'patch.crates-io.ant-node.path="../ant-node-encrypted-uploads"'
//! test -p ant-core --test e2e_chunk_admission -- --ignored --nocapture`
//! The Cargo patch changes the lockfile locally; do not commit that change.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use ant_core::data::{compute_address, Client, Error};
use ant_protocol::{
    ChunkMessage, ChunkMessageBody, ChunkPutRequest, ChunkPutResponse, ProtocolError,
};
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use support::{test_client_config, MiniTestnet, DEFAULT_NODE_COUNT};

const REJECTION: &str = "chunk has a recognizable file header; self-encrypt files before uploading";

fn fixtures() -> Vec<(&'static str, Bytes)> {
    let mut headers = vec![
        ("PNG", b"\x89PNG\r\n\x1a\n".to_vec()),
        ("JPEG", b"\xff\xd8\xff\xe0\x00\x10JFIF\x00".to_vec()),
        ("GIF", b"GIF89a".to_vec()),
        ("WAV", b"RIFF\x20\x00\x00\x00WAVE".to_vec()),
        ("MP4", b"\x00\x00\x00\x18ftypisom".to_vec()),
        ("PDF", b"%PDF-1.7".to_vec()),
        ("HTML", b"\xef\xbb\xbf\n<!doctype html>".to_vec()),
        ("gzip", b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\xff".to_vec()),
        ("tar", tar::Header::new_ustar().as_bytes().to_vec()),
        ("SQLite", b"SQLite format 3\x00".to_vec()),
        ("XZ", b"\xfd7zXZ\x00".to_vec()),
        ("XML", b"<?xml version='1.0'?><root/>".to_vec()),
        ("ZIP", b"PK\x03\x04\x14\x00".to_vec()),
    ];
    let mut exe = vec![0; 0x98];
    exe[..2].copy_from_slice(b"MZ");
    exe[0x3c..0x40].copy_from_slice(&0x80_u32.to_le_bytes());
    exe[0x80..0x84].copy_from_slice(b"PE\x00\x00");
    exe[0x96..0x98].copy_from_slice(&0x0002_u16.to_le_bytes());
    let mut dll = exe.clone();
    dll[0x96..0x98].copy_from_slice(&0x2002_u16.to_le_bytes());
    headers.extend([("EXE", exe), ("DLL", dll)]);
    headers
        .into_iter()
        .map(|(name, mut content)| {
            content.resize(4096, 0xa5);
            (name, Bytes::from(content))
        })
        .collect()
}

/// Bypass client preflight as an older/custom client would. Only the node's
/// real network handler can generate the response checked by this probe.
async fn raw_put(client: &Client, testnet: &MiniTestnet, content: Bytes, id: u64) {
    let target = testnet.node(0).expect("target node");
    let message = ChunkMessage {
        request_id: id,
        body: ChunkMessageBody::PutRequest(ChunkPutRequest::new(
            compute_address(&content),
            content,
        )),
    }
    .encode()
    .expect("encode raw PUT");
    let response = ant_protocol::send_and_await_chunk_response(
        client.network().node(),
        target.peer_id(),
        message,
        id,
        Duration::from_secs(30),
        &target.listen_addrs().await,
        |body| match body {
            ChunkMessageBody::PutResponse(response) => Some(Ok(response)),
            _ => None,
        },
        |error| format!("raw PUT send failed: {error}"),
        || "raw PUT timed out".to_string(),
    )
    .await
    .expect("node response over QUIC");
    assert!(
        matches!(&response, ChunkPutResponse::Error(ProtocolError::StorageFailed(message)) if message == REJECTION),
        "expected the actionable header rejection, got {response:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the enforcing companion ant-node branch via a local Cargo patch"]
async fn recognizable_headers_are_refused_on_a_paid_local_testnet() {
    println!("Starting {DEFAULT_NODE_COUNT} real QUIC nodes and an Anvil payment chain");
    let testnet = MiniTestnet::start(DEFAULT_NODE_COUNT).await;
    let node = testnet.node(3).expect("client node");
    let client = Client::from_node(Arc::clone(&node), test_client_config())
        .with_wallet(testnet.wallet().clone());
    let tokens_before = testnet.wallet().balance_of_tokens().await.unwrap();
    let gas_before = testnet.wallet().balance_of_gas_tokens().await.unwrap();
    let headers = fixtures();

    for (index, (name, content)) in headers.iter().enumerate() {
        let error = client.chunk_put(content.clone()).await.unwrap_err();
        assert!(
            matches!(&error, Error::ChunkUploadRejected { address } if address == &hex::encode(compute_address(content)))
        );
        assert!(error.to_string().contains(REJECTION), "{name}: {error}");
        raw_put(&client, &testnet, content.clone(), 1_000_000 + index as u64).await;
        for node in &testnet.nodes {
            assert!(!node
                .protocol
                .as_ref()
                .unwrap()
                .exists(&compute_address(content))
                .unwrap());
        }
        println!(
            "{name}: typed client rejection; exact node error over QUIC; absent from all nodes"
        );
    }
    assert_eq!(
        testnet.wallet().balance_of_tokens().await.unwrap(),
        tokens_before
    );
    assert_eq!(
        testnet.wallet().balance_of_gas_tokens().await.unwrap(),
        gas_before
    );
    println!("All 15 rejected inputs left token and gas balances unchanged");

    // The same PNG-prefixed bytes must work through the supported file API.
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("source.png");
    let output = directory.path().join("downloaded.png");
    let original = &headers[0].1;
    std::fs::write(&input, original).unwrap();
    let uploaded = client
        .file_upload(&input)
        .await
        .expect("self-encrypted upload");
    assert!(uploaded.chunks_stored >= 3);
    // Force the download to retrieve real stored chunks over the network.
    client.chunk_cache().clear();
    let written = client
        .file_download(&uploaded.data_map, &output)
        .await
        .expect("download and decrypt");
    assert_eq!(written, original.len() as u64);
    assert_eq!(std::fs::read(&output).unwrap(), original.as_ref());
    assert!(testnet.wallet().balance_of_tokens().await.unwrap() < tokens_before);
    println!(
        "Self-encrypted file upload paid for {} chunks; download matched all {written} bytes",
        uploaded.chunks_stored
    );

    drop(client);
    drop(node);
    testnet.teardown().await;
}
