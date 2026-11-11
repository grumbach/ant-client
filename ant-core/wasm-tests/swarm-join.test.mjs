import assert from "node:assert/strict";
import test from "node:test";
import { BrowserNetworkClient } from "./client-fixture.mjs";
import { encryptPublicFile } from "./pkg/ant_core.js";
import { mockWebRtc } from "./mock-webrtc.mjs";

const content = new TextEncoder().encode("Swarm join regression fixture. ".repeat(2000));
const encrypted = encryptPublicFile(content);
const datamap = encrypted.records.at(-1);
const settle = () => new Promise(resolve => setTimeout(resolve, 500));

// Seven nodes holding every record of the file; `options` applies to each.
function network(options = {}) {
  const rtc = mockWebRtc(Array.from({ length: 7 }, () => ({ ...options })));
  for (const store of rtc.stores) for (const record of encrypted.records) store.set(record.address, record.content);
  return rtc;
}

test("joinSwarm joins the public file's room", async () => {
  const rtc = network();
  const client = new BrowserNetworkClient(rtc.endpoints);
  try {
    await client.joinSwarm(datamap.address);
    assert.equal(client.swarmStats().rooms, 1);
  } finally { client.close(); }
});

// The join reads the file's DataMap before it joins: a signal that aborts
// while that read runs stops the read and joins no room, so the room's key
// is never looked up, even once the DataMap's reply arrives.
test("a joinSwarm aborted while it reads the DataMap joins no room", async () => {
  const abort = new AbortController();
  const reason = new Error("the caller cancelled the join");
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
    await assert.rejects(client.joinSwarm(datamap.address, { signal: abort.signal }), error => error === reason);
    assert.ok(asked, "the join had asked a node for the DataMap");
    await settle();
    assert.equal(client.swarmStats().rooms, 0, "no room joined");
    assert.equal(client.swarmStats().discovery.lookups, 0, "the room's key was never looked up");
  } finally { client.close(); }
});

test("a joinSwarm whose signal has already aborted asks the network nothing", async () => {
  const rtc = network();
  const client = new BrowserNetworkClient(rtc.endpoints);
  const abort = new AbortController();
  const reason = new Error("cancelled before the join");
  abort.abort(reason);
  try {
    await assert.rejects(client.joinSwarm(datamap.address, { signal: abort.signal }), error => error === reason);
    await settle();
    assert.equal(rtc.connections.length, 0, "nothing dialled");
    assert.equal(client.swarmStats().rooms, 0, "no room joined");
  } finally { client.close(); }
});

test("joinSwarm refuses a signal that is not an AbortSignal", async () => {
  const rtc = network();
  const client = new BrowserNetworkClient(rtc.endpoints);
  try {
    await assert.rejects(client.joinSwarm(datamap.address, { signal: {} }), /AbortSignal/u);
    assert.equal(rtc.connections.length, 0, "nothing dialled");
    assert.equal(client.swarmStats().rooms, 0, "no room joined");
  } finally { client.close(); }
});
