import assert from "node:assert/strict";
import test from "node:test";
import { BrowserNetworkClient } from "./client-fixture.mjs";
import { encryptPublicFile } from "./pkg/ant_core.js";
import { mockWebRtc } from "./mock-webrtc.mjs";

// An AbortSignal can abort inside one of the operation's own callbacks, which
// run in the middle of a poll: that operation must stop there, neither going
// on to further records, writes or a destination's close, nor resolving.

const content = new TextEncoder().encode("Cancellation inside a callback fixture. ".repeat(2000));
const encrypted = encryptPublicFile(content);
const datamap = encrypted.records.at(-1);
const settle = (ms = 500) => new Promise(resolve => setTimeout(resolve, ms));
const chunkMessage = message => /^Downloaded chunk (\d+)\/(\d+)/u.exec(String(message));

// Seven nodes holding every record of the file; `options` applies to each.
function network(options = {}) {
  const rtc = mockWebRtc(Array.from({ length: 7 }, () => ({ ...options })));
  for (const store of rtc.stores) for (const record of encrypted.records) store.set(record.address, record.content);
  return rtc;
}

test("a download aborted in its chunk progress stops there and rejects", async () => {
  const rtc = network();
  const client = new BrowserNetworkClient(rtc.endpoints);
  try {
    // A first download leaves the records in the client's cache, so the
    // second delivers every chunk within one poll.
    const first = await client.downloadPublicFile(datamap.address, undefined);
    assert.deepEqual(first.content, content);
    const abort = new AbortController();
    const reason = new Error("cancelled at the first chunk");
    const after = [];
    const onProgress = message => {
      const chunk = chunkMessage(message);
      if (!chunk || chunk[1] === "0") return;
      if (abort.signal.aborted) after.push(String(message));
      else abort.abort(reason);
    };
    await assert.rejects(client.downloadPublicFile(datamap.address, undefined, onProgress, { signal: abort.signal }),
      error => error === reason);
    assert.ok(abort.signal.aborted, "the download reported a chunk");
    assert.deepEqual(after, [], "no chunk is delivered after the abort");
  } finally { client.close(); }
});

test("a pipeTo aborted in its last progress callback aborts the destination and does not close it", async () => {
  const rtc = network();
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    reader = await client.openPublicFile(datamap.address);
    const abort = new AbortController();
    const reason = new Error("cancelled at the last write");
    let closed = false, aborted;
    const sink = new WritableStream({ write() {}, close() { closed = true; }, abort(error) { aborted = error; } });
    const onProgress = (written, total) => { if (written === total) abort.abort(reason); };
    await assert.rejects(reader.pipeTo(sink, { signal: abort.signal, onProgress }), error => error === reason);
    await settle(50);
    assert.equal(closed, false, "the destination is not closed");
    assert.equal(aborted, reason, "the destination is aborted with the reason");
  } finally { reader?.close(); client.close(); }
});

// A read that continues no other has read-ahead fetch the record after it
// once the read has finished; a cancelled read has not finished.
test("a readRange cancelled while it fetches has read-ahead fetch nothing after it", async () => {
  const abort = new AbortController();
  const reason = new Error("cancelled while the record was fetched");
  let armed = false;
  const rtc = network({
    respond(_channel, method) {
      if (armed && method === "get_chunk" && !abort.signal.aborted) abort.abort(reason);
    },
  });
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    reader = await client.openPublicFile(datamap.address);
    const before = rtc.requests.length;
    armed = true;
    await assert.rejects(reader.readRange(0, 10, { signal: abort.signal }), error => error === reason);
    await settle();
    const fetched = new Set(rtc.requests.slice(before)
      .filter(request => request.method === "get_chunk")
      .map(request => request.address));
    assert.equal(fetched.size, 1, "only the cancelled read's own record was asked for");
  } finally { reader?.close(); client.close(); }
});
