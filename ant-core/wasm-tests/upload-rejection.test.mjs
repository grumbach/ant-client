import assert from "node:assert/strict";
import test from "node:test";
import { BrowserNetworkClient } from "./client-fixture.mjs";
import { contentAddress, encryptPublicFile, parseWebRtcDirectMultiaddr } from "./pkg/ant_core.js";
import { mockWebRtc, paymentNetwork } from "./mock-webrtc.mjs";

const rejection = "chunk has a recognizable file header; self-encrypt files before uploading";
const receipt = (_, quotes) => ({
  transactionHash: `0x${"ab".repeat(32)}`,
  totalAmount: quotes.reduce((sum, quote) => sum + BigInt(quote.amount), 0n).toString(),
});
const batch = bytes => ({ records: [{ address: contentAddress(bytes), size: bytes.length }] });

for (const [name, bytes] of [
  ["PNG", Uint8Array.from([0x89, 0x50, 0x4e, 0x47, 13, 10, 26, 10])],
  ["SQLite", new TextEncoder().encode("SQLite format 3\0")],
  ["XML", new TextEncoder().encode("<?xml version='1.0'?><root/>")],
  ["gzip", Uint8Array.from([0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255])],
]) {
  test(`raw ${name} records return an actionable JavaScript error before quotes or payment`, async () => {
    const rtc = mockWebRtc(Array.from({ length: 7 }, () => ({})));
    const client = new BrowserNetworkClient(rtc.endpoints);
    let payments = 0;
    const pay = (...args) => { payments++; return receipt(...args); };
    try {
      await assert.rejects(client.uploadRecords(batch(bytes), paymentNetwork, () => bytes, pay), error => {
        assert.match(String(error), /recognizable file header/);
        assert.match(String(error), /self-encrypt files before uploading/);
        return true;
      });
      assert.equal(payments, 0);
      assert.equal(rtc.requests.filter(({ method }) =>
        ["quote_chunk", "merkle_quote", "put_chunk"].includes(method)).length, 0);
    } finally { client.close(); }
  });
}

function encryptedRecord() {
  const source = new TextEncoder().encode("Self-encrypted upload review fixture. ".repeat(100));
  return encryptPublicFile(source).records[0].content;
}

test("a corroborated node rejection reaches JavaScript clearly and is not retried", async () => {
  const bytes = encryptedRecord(); // Pass local preflight to exercise the node-response path.
  const rtc = mockWebRtc(Array.from({ length: 7 }, () => ({
    putError: { code: "put_failed", message: rejection },
  })));
  const client = new BrowserNetworkClient(rtc.endpoints);
  let payments = 0;
  const pay = (...args) => { payments++; return receipt(...args); };
  try {
    await assert.rejects(client.uploadRecords(batch(bytes), paymentNetwork, () => bytes, pay), error => {
      const message = String(error);
      assert.match(message, /partial upload: 0\/1 stored/);
      assert.match(message, /recognizable file header/);
      assert.match(message, /self-encrypt files before uploading/);
      assert.doesNotMatch(message, /disk.full|insufficient peers/i);
      return true;
    });
    assert.equal(payments, 1, "the already-submitted payment must not be repeated");
    const puts = rtc.requests.filter(({ method }) => method === "put_chunk");
    assert.equal(puts.length, 7, "one bounded peer sweep, without whole-record retries");
    assert.equal(new Set(puts.map(({ node }) => node)).size, 7);
  } finally { client.close(); }
});

test("one node's header rejection does not veto a successful encrypted upload", async () => {
  const bytes = encryptedRecord();
  const options = Array.from({ length: 7 }, () => ({}));
  const rtc = mockWebRtc(options);
  const address = contentAddress(bytes);
  const closest = rtc.endpoints.map((endpoint, index) => ({
    index,
    distance: BigInt(`0x${parseWebRtcDirectMultiaddr(endpoint).peerId}`) ^ BigInt(`0x${address}`),
  })).sort((a, b) => a.distance < b.distance ? -1 : a.distance > b.distance ? 1 : 0)[0].index;
  options[closest].putError = { code: "put_failed", message: rejection };
  const client = new BrowserNetworkClient(rtc.endpoints);
  let payments = 0;
  const pay = (...args) => { payments++; return receipt(...args); };
  try {
    const result = await client.uploadRecords(batch(bytes), paymentNetwork, () => bytes, pay);
    assert.equal(result.replicas, 4);
    assert.equal(payments, 1);
    const puts = rtc.requests.filter(({ method }) => method === "put_chunk");
    assert.equal(puts.filter(({ node }) => node === closest).length, 1);
    assert.equal(puts.filter(({ node }) => node !== closest).length, 4);
  } finally { client.close(); }
});
