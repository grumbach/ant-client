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
  const rtc = mockWebRtc(Array.from({ length: 7 }, () => ({
    delay: (method, address) => method === "get_chunk" && armed && slow.has(address) ? 1_500 : 0,
    respond(_channel, method) {
      if (armed && method === "get_chunk" && !abort.signal.aborted) abort.abort(reason);
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
