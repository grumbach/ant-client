import assert from "node:assert/strict";
import test from "node:test";
import { BrowserNetworkClient } from "./client-fixture.mjs";
import { encryptPublicFile, test_guarded_request_queued_at_stop, test_room_lookup_queued_at_stop, test_swarm_announce_queued_at_stop } from "./pkg/ant_core.js";
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

// The last report of an open or a download comes after its room is joined:
// an abort there still rejects, the open returning no reader.
const lastReports = {
  "openPublicFile": [(client, onProgress, signal) => client.openPublicFile(datamap.address, onProgress, { signal }), /^Ready to stream /u],
  "openPrivateFile": [(client, onProgress, signal) => client.openPrivateFile({ data_map: datamap.content }, onProgress, { signal }), /^Ready to stream /u],
  "downloadPublicFile": [(client, onProgress, signal) => client.downloadPublicFile(datamap.address, undefined, onProgress, { signal }), /^Verified complete /u],
  "downloadPrivateFile": [(client, onProgress, signal) => client.downloadPrivateFile({ data_map: datamap.content }, undefined, onProgress, { signal }), /^Verified complete /u],
};
for (const [name, [read, last]] of Object.entries(lastReports)) {
  test(`${name} aborted in its last progress report rejects`, async () => {
    const rtc = network();
    const client = new BrowserNetworkClient(rtc.endpoints);
    const abort = new AbortController();
    const reason = new Error(`cancelled at ${name}'s last report`);
    const onProgress = message => { if (last.test(String(message))) abort.abort(reason); };
    try {
      await assert.rejects(read(client, onProgress, abort.signal), error => error === reason);
      assert.ok(abort.signal.aborted, "the last report came");
    } finally { client.close(); }
  });
}

// stopSwarm leaves every room and joins none after, while the client's
// transport stays open: the SDK's close() stops sharing at once even while a
// paid write still needs the transport.
test("stopSwarm leaves every room and joins none after, while reads go on", async () => {
  const rtc = network();
  const client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    reader = await client.openPublicFile(datamap.address);
    assert.equal(client.swarmStats().rooms, 1);
    client.stopSwarm();
    assert.equal(client.swarmStats().rooms, 0, "every room left");
    assert.deepEqual(await reader.readRange(0, 10), content.slice(0, 10), "reads go on");
    const downloaded = await client.downloadPublicFile(datamap.address, undefined);
    assert.deepEqual(downloaded.content, content);
    await client.joinSwarm(datamap.address);
    assert.equal(client.swarmStats().rooms, 0, "no room joined after");
  } finally { reader?.close(); client.close(); }
});

// A room's lookup asks several nodes at once, each from a task of its own.
// stopSwarm while those tasks are under way: none of them asks another node
// for the room's key after it. A private file's room key is looked up by
// nothing but its room (its reads ask for its records' addresses).
test("stopSwarm stops a room's lookup: no node is asked for the room after it", async () => {
  const records = encrypted.records.slice(0, -1);
  const roomKey = datamap.address;
  let client;
  let stoppedAt;
  const rtc = mockWebRtc(Array.from({ length: 7 }, () => ({
    respond(_channel, method) {
      // The room's first lookup request reaches a node: sharing stops now.
      if (method !== "find_node" || stoppedAt !== undefined || rtc.requests.at(-1)?.target !== roomKey) return;
      client.stopSwarm();
      stoppedAt = rtc.requests.length;
    },
  })));
  for (const store of rtc.stores) for (const record of records) store.set(record.address, record.content);
  client = new BrowserNetworkClient(rtc.endpoints);
  let reader;
  try {
    reader = await client.openPrivateFile({ data_map: datamap.content });
    assert.deepEqual(await reader.readRange(0, 10), content.slice(0, 10));
    for (let waited = 0; stoppedAt === undefined && waited < 10_000; waited += 20) await settle(20);
    assert.ok(stoppedAt !== undefined, "the room started its lookup");
    await settle(1_500);
    assert.equal(client.swarmStats().rooms, 0);
    const after = rtc.requests.slice(stoppedAt).filter(request => request.method === "find_node" && request.target === roomKey);
    assert.deepEqual(after, [], "no node is asked for the room's key after stopSwarm");
  } finally { reader?.close(); client.close(); }
});

// The schedule the test above cannot force: a room lookup's task is queued
// on an authenticated session when sharing stops, and runs after the stop.
test("a room lookup queued when sharing stops asks the node nothing", async () => {
  const rtc = mockWebRtc([{}]);
  const error = await test_room_lookup_queued_at_stop(rtc.endpoints[0]);
  assert.match(error, /stopped sharing/u);
  assert.deepEqual(rtc.requests.filter(request => request.method === "find_node"), [], "no lookup request sent");
});

// One queue further: the lookup's request is handed to the session's sender
// task, queued when sharing stops, and runs after the stop.
test("a room's request whose sender is queued when sharing stops is not sent", async () => {
  const rtc = mockWebRtc([{}]);
  const before = () => rtc.requests.filter(request => request.method === "find_node").length;
  await test_guarded_request_queued_at_stop(rtc.endpoints[0]);
  assert.equal(before(), 0, "no lookup request sent");
});

// An announce, which tells a node the room's key and joins the client to
// it there, handed to its session's sender task queued when sharing stops.
test("a swarm announce whose sender is queued when sharing stops is not sent", async () => {
  const rtc = mockWebRtc([{ swarm: true }]);
  await test_swarm_announce_queued_at_stop(rtc.endpoints[0]);
  assert.ok(rtc.requests.some(request => request.method === "get_chunk"), "the session was open");
  assert.deepEqual(rtc.requests.filter(request => request.method === "swarm_announce"), [], "no announce sent");
});
