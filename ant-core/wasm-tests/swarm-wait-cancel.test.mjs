import assert from "node:assert/strict";
import test from "node:test";
import { BrowserNetworkClient } from "./client-fixture.mjs";
import { encryptPublicFile } from "./pkg/ant_core.js";
import { mockWebRtc } from "./mock-webrtc.mjs";

// The client's clock is `performance.now()`. This file skips it forward past
// a room's 30 s turnover instead of waiting; node:test runs each file in a
// process of its own, so no other test sees it.
const realNow = performance.now.bind(performance);
let skipped = 0;
performance.now = () => realNow() + skipped;
const TURNOVER_MS = 30_000;
const skipTurnover = () => { skipped += TURNOVER_MS + 1_000; };
const settle = (ms = 500) => new Promise(resolve => setTimeout(resolve, ms));
const roomMessage = message => /^Swarm room ([0-9a-f]{16})$/u.exec(String(message))?.[1];

function fixture(index) {
  const content = new TextEncoder().encode(`Swarm waiting join fixture ${index}. `.repeat(2000));
  const encrypted = encryptPublicFile(content);
  return { content, encrypted, datamap: encrypted.records.at(-1), chunks: encrypted.records.slice(0, -1) };
}
const files = Array.from({ length: 5 }, (_, index) => fixture(index));

// A read that finds this client in four rooms all read lately makes its
// join wait, to be made by a later read of the file's records once a room
// makes way. A read cancelled before it ends lets go of that wait: its own
// fetch, which read-ahead finishes in the background, makes no room.
test("a cancelled read's waiting join is not made by its fetch finishing after turnover", async () => {
  const slow = new Set(files[0].chunks.map(record => record.address));
  const abort = new AbortController();
  const reason = new Error("cancelled while its record was fetched");
  let armed = false;
  // Only the first file's records: the other readers' read-ahead asks for
  // the records after theirs meanwhile.
  const ours = (method, address) => armed && method === "get_chunk" && slow.has(address);
  const rtc = mockWebRtc(Array.from({ length: 7 }, () => ({
    delay(method, address) {
      if (!ours(method, address)) return 0;
      if (!abort.signal.aborted) abort.abort(reason);
      return 1_500;
    },
  })));
  for (const store of rtc.stores) {
    for (const { encrypted } of files) for (const record of encrypted.records) store.set(record.address, record.content);
  }
  const client = new BrowserNetworkClient(rtc.endpoints);
  const readers = [];
  const tags = [];
  const open = async ({ datamap }) => {
    let tag;
    readers.push(await client.openPublicFile(datamap.address, message => { tag ??= roomMessage(message); }));
    tags.push(tag);
  };
  const joined = () => client.swarmRoomStats().map(room => room.tag);
  try {
    for (const file of files.slice(0, 4)) await open(file);
    skipTurnover();
    await open(files[4]);
    assert.ok(!joined().includes(tags[0]), "the fifth file's room took the first one's place");
    // Every room is read now, so the first file's next read must wait.
    for (const reader of readers.slice(1)) await reader.readRange(0, 10);
    armed = true;
    await assert.rejects(readers[0].readRange(0, 10, { signal: abort.signal }), error => error === reason);
    // The rooms go unread while the cancelled read's record is still coming.
    skipTurnover();
    await settle(2_500);
    assert.ok(rtc.requests.some(request => request.method === "get_chunk" && slow.has(request.address)),
      "the cancelled read had asked for its record");
    assert.ok(!joined().includes(tags[0]), "the cancelled read's waiting join is not made");

    // An ordinary read of the first file still joins its room once one makes way.
    armed = false;
    assert.deepEqual(await readers[0].readRange(0, 10), files[0].content.slice(0, 10));
    await settle();
    assert.ok(joined().includes(tags[0]), "an ordinary read joins it");
  } finally {
    for (const reader of readers) reader.close();
    client.close();
  }
});

// A read's hold is let go of when its own task, which the abort wakes,
// drops the read: tasks the record's answer woke before it, such as a
// read-ahead fetch, must not make the wait first. Here the answer is
// delivered once the rooms have gone unread, then the read is aborted
// `turns` timer turns and `hops` microtasks later, as an application's
// timer or promise callback may. Returns whether the room was joined, or
// "finished" when the read ended before its abort (a read that finished
// may join).
async function abortAsRecordArrives(turns, hops) {
  const slow = new Set(files[0].chunks.map(record => record.address));
  const abort = new AbortController();
  const reason = new Error(`cancelled ${turns} turns and ${hops} microtasks after its record arrived`);
  let armed = false;
  let held;
  // Only the first file's records: the other readers' read-ahead asks for
  // the records after theirs meanwhile.
  const rtc = mockWebRtc(Array.from({ length: 7 }, () => ({
    respond(channel, method, response) {
      if (!armed || method !== "get_chunk" || held || !slow.has(rtc.requests.at(-1)?.address)) return;
      held = { channel, response };
      return false;
    },
  })));
  for (const store of rtc.stores) {
    for (const { encrypted } of files) for (const record of encrypted.records) store.set(record.address, record.content);
  }
  const client = new BrowserNetworkClient(rtc.endpoints);
  const readers = [];
  const tags = [];
  const open = async ({ datamap }) => {
    let tag;
    readers.push(await client.openPublicFile(datamap.address, message => { tag ??= roomMessage(message); }));
    tags.push(tag);
  };
  const joined = () => client.swarmRoomStats().map(room => room.tag);
  try {
    for (const file of files.slice(0, 4)) await open(file);
    skipTurnover();
    await open(files[4]);
    for (const reader of readers.slice(1)) await reader.readRange(0, 10);
    armed = true;
    const read = readers[0].readRange(0, 10, { signal: abort.signal });
    for (let waited = 0; !held && waited < 5_000; waited += 10) await settle(10);
    assert.ok(held, "the read asked for its record");
    skipTurnover();
    for (let offset = 0; offset < held.response.length; offset += 16_384) {
      held.channel.emit(held.response.slice(offset, offset + 16_384).buffer);
    }
    for (let turn = 0; turn < turns; turn++) await new Promise(resolve => setTimeout(resolve, 0));
    let later = Promise.resolve();
    for (let hop = 0; hop < hops; hop++) later = later.then(() => undefined);
    await later;
    abort.abort(reason);
    const finished = await read.then(() => true, error => { assert.equal(error, reason); return false; });
    await settle();
    return finished ? "finished" : joined().includes(tags[0]);
  } finally {
    for (const reader of readers) reader.close();
    client.close();
  }
}

test("a read aborted as its record arrives does not have its waiting join made", { timeout: 120_000 }, async () => {
  const outcomes = [];
  for (const turns of [0, 1]) for (const hops of [0, 1, 2, 4, 8, 12]) {
    outcomes.push([turns, hops, await abortAsRecordArrives(turns, hops)]);
  }
  const cancelled = outcomes.filter(([, , outcome]) => outcome !== "finished");
  assert.ok(cancelled.length > 0, "some reads were aborted before they finished");
  assert.deepEqual(cancelled.filter(([, , joined]) => joined), [],
    "no cancelled read's waiting join is made, whenever it aborts after its record arrives");
});
