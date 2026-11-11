import assert from "node:assert/strict";
import test from "node:test";
import { BrowserNetworkClient } from "./client-fixture.mjs";
import { encryptPublicFile } from "./pkg/ant_core.js";
import { mockWebRtc } from "./mock-webrtc.mjs";

// Every read joins its file's room as it resolves the file, after reporting
// `Swarm room <tag>`. A read cancelled before it joins must join no room, so
// nothing looks the room's key up: whether its signal had aborted already,
// aborts while the file resolves, or aborts in that progress callback.

function fixture(index) {
  const content = new TextEncoder().encode(`Swarm read cancellation fixture ${index}. `.repeat(2000));
  const encrypted = encryptPublicFile(content);
  return { content, encrypted, datamap: encrypted.records.at(-1) };
}
const files = Array.from({ length: 5 }, (_, index) => fixture(index));
const [file] = files;
const settle = (ms = 500) => new Promise(resolve => setTimeout(resolve, ms));
const roomMessage = message => /^Swarm room [0-9a-f]{16}$/u.exec(String(message));

// Seven nodes holding every record of every fixture; `options` applies to each.
function network(options = {}) {
  const rtc = mockWebRtc(Array.from({ length: 7 }, () => ({ ...options })));
  for (const store of rtc.stores) {
    for (const { encrypted } of files) for (const record of encrypted.records) store.set(record.address, record.content);
  }
  return rtc;
}

// Each read, called with its progress callback and an AbortSignal.
const reads = {
  "openPublicFile": (client, onProgress, signal) => client.openPublicFile(file.datamap.address, onProgress, { signal }),
  "openPrivateFile": (client, onProgress, signal) => client.openPrivateFile({ data_map: file.datamap.content }, onProgress, { signal }),
  "downloadPublicFile": (client, onProgress, signal) => client.downloadPublicFile(file.datamap.address, undefined, onProgress, { signal }),
  "downloadPrivateFile": (client, onProgress, signal) => client.downloadPrivateFile({ data_map: file.datamap.content }, undefined, onProgress, { signal }),
};

async function assertNoRoom(client) {
  await settle();
  assert.equal(client.swarmStats().rooms, 0, "no room joined");
  assert.equal(client.swarmStats().discovery.lookups, 0, "no room's key looked up");
}

for (const [name, read] of Object.entries(reads)) {
  test(`${name} whose signal has already aborted joins no room`, async () => {
    const rtc = network();
    const client = new BrowserNetworkClient(rtc.endpoints);
    const abort = new AbortController();
    const reason = new Error(`cancelled before ${name}`);
    abort.abort(reason);
    try {
      await assert.rejects(read(client, undefined, abort.signal), error => error === reason);
      assert.equal(rtc.connections.length, 0, "nothing dialled");
      await assertNoRoom(client);
    } finally { client.close(); }
  });

  test(`${name} aborted as it reports its swarm room joins no room`, async () => {
    const rtc = network();
    const client = new BrowserNetworkClient(rtc.endpoints);
    const abort = new AbortController();
    const reason = new Error(`cancelled in ${name}'s progress`);
    let reported = false;
    const onProgress = message => {
      if (roomMessage(message) && !reported) {
        reported = true;
        abort.abort(reason);
      }
    };
    try {
      await assert.rejects(read(client, onProgress, abort.signal), error => error === reason);
      assert.ok(reported, "the read reported its room");
      await assertNoRoom(client);
    } finally { client.close(); }
  });
}

test("openPublicFile aborted while it reads the DataMap joins no room", async () => {
  const abort = new AbortController();
  const reason = new Error("cancelled while the DataMap was read");
  let asked = false;
  const rtc = network({
    respond(_channel, method) {
      if (method === "get_chunk" && !asked) {
        asked = true;
        abort.abort(reason);
      }
    },
  });
  const client = new BrowserNetworkClient(rtc.endpoints);
  try {
    await assert.rejects(client.openPublicFile(file.datamap.address, undefined, { signal: abort.signal }), error => error === reason);
    assert.ok(asked, "the open had asked a node for the DataMap");
    await assertNoRoom(client);
  } finally { client.close(); }
});

test("openPublicFile refuses a signal that is not an AbortSignal", async () => {
  const rtc = network();
  const client = new BrowserNetworkClient(rtc.endpoints);
  try {
    await assert.rejects(client.openPublicFile(file.datamap.address, undefined, { signal: {} }), /AbortSignal/u);
    assert.equal(rtc.connections.length, 0, "nothing dialled");
  } finally { client.close(); }
});

// A read joins its file's room again if reading other files has left it
// since the file was opened; a read cancelled first must not. The room goes
// through ordinary turnover: four rooms are joined, and a fifth file's open,
// 30 s later, takes the place of the one unread longest.
test("a readRange cancelled before it starts does not join its file's room again", { timeout: 90_000 }, async () => {
  const rtc = network();
  const client = new BrowserNetworkClient(rtc.endpoints);
  const readers = [];
  const tags = [];
  const open = async ({ datamap }) => {
    let tag;
    readers.push(await client.openPublicFile(datamap.address, message => { tag ??= roomMessage(message)?.[0].slice(-16); }));
    tags.push(tag);
  };
  const joined = () => client.swarmRoomStats().map(room => room.tag);
  try {
    for (const each of files.slice(0, 4)) await open(each);
    assert.deepEqual(joined().sort(), tags.slice().sort(), "the four files' rooms are joined");
    await settle(31_000);
    await open(files[4]);
    assert.ok(!joined().includes(tags[0]), "the fifth file's room took the first one's place");

    const abort = new AbortController();
    const reason = new Error("cancelled before the read");
    abort.abort(reason);
    await assert.rejects(readers[0].readRange(0, 10, { signal: abort.signal }), error => error === reason);
    await settle();
    assert.ok(!joined().includes(tags[0]), "the cancelled read did not join the room again");
    await assert.rejects(readers[0].readRange(0, 10, { signal: {} }), /AbortSignal/u);

    assert.deepEqual(await readers[0].readRange(0, 10), files[0].content.slice(0, 10));
    assert.ok(joined().includes(tags[0]), "an ordinary read joins it again");
  } finally {
    for (const reader of readers) reader.close();
    client.close();
  }
});
