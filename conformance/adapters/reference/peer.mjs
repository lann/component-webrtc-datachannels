#!/usr/bin/env node
// The non-wasm reference peer for the conformance suite's two-peer corpus.
//
// A plain Node program — no wasm component, no WIT bindings — driving the
// W3C RTCPeerConnection API (libwebrtc via @roamhq/wrtc) directly. It speaks
// the shared single-peer contract (--test/--role/--server/--room/
// --message-count/--message-size, one JSON test-result line on stdout) and the
// conformance guest's signaling blob schema over the conformance-signalingd
// mailbox protocol (conformance/signaling/PROTOCOL.md), so it can be paired
// against any suite target: a failure on the wire implicates the target's
// stack, not a second wasm guest.
//
// Usage:
//   node peer.mjs --test interop-handshake --role offerer \
//     --server http://127.0.0.1:8080 --room r --message-count 16 --message-size 512

import { parseArgs } from "node:util";
import process from "node:process";

const wrtc = (await import("@roamhq/wrtc")).default;

const CHANNEL_LABEL = "conformance";
const BARRIER_SENTINEL = "__conformance_barrier__";
// Wall-clock bound on the whole run; under Shadow this is simulated time.
const RUN_TIMEOUT_MS = 120_000;

// Phase markers on stderr, enabled with REF_PEER_DEBUG=1.
const dbg = process.env.REF_PEER_DEBUG
  ? (...args) => console.error("[ref-peer]", ...args)
  : () => {};

// --- mailbox client (PROTOCOL.md) -------------------------------------------

/** A fetch-based client of one conformance-signalingd room, bound to a role. */
class Mailbox {
  #base;
  #room;
  #role;
  #peerRole;
  #recvSeq = 0;

  constructor(server, room, role) {
    this.#base = server.replace(/\/+$/, "");
    this.#room = room;
    this.#role = role;
    this.#peerRole = role === "offerer" ? "answerer" : "offerer";
  }

  /** Publish the next blob to this role's mailbox. */
  async send(bytes) {
    const res = await fetch(`${this.#base}/rooms/${this.#room}/${this.#role}`, {
      method: "POST",
      headers: { "content-type": "application/octet-stream" },
      body: bytes,
    });
    if (!res.ok) {
      throw new Error(`mailbox send: HTTP ${res.status}`);
    }
    await res.arrayBuffer();
  }

  /** Fetch the next blob from the peer's mailbox, or undefined at end. */
  async recv() {
    for (;;) {
      const res = await fetch(
        `${this.#base}/rooms/${this.#room}/${this.#peerRole}` +
          `?seq=${this.#recvSeq}&wait=10000`,
      );
      if (res.status === 200) {
        this.#recvSeq += 1;
        return new Uint8Array(await res.arrayBuffer());
      }
      await res.arrayBuffer();
      if (res.status === 204) {
        return undefined; // peer's mailbox is done
      }
      if (res.status === 304) {
        continue; // not yet; retry the same seq
      }
      throw new Error(`mailbox recv: HTTP ${res.status}`);
    }
  }

  /** Mark this role's mailbox done. */
  async done() {
    const res = await fetch(
      `${this.#base}/rooms/${this.#room}/${this.#role}/done`,
      { method: "POST" },
    );
    if (!res.ok) {
      throw new Error(`mailbox done: HTTP ${res.status}`);
    }
    await res.arrayBuffer();
  }
}

// --- signaling blob schema (owned by the conformance guest) ------------------

const encodeSignal = (signal) => new TextEncoder().encode(JSON.stringify(signal));
const decodeSignal = (bytes) => JSON.parse(new TextDecoder().decode(bytes));

// --- handshake ---------------------------------------------------------------

/** Wait for ICE gathering to complete and return every gathered candidate. */
function gatherCandidates(pc) {
  return new Promise((resolve) => {
    const candidates = [];
    pc.addEventListener("icecandidate", ({ candidate }) => {
      if (candidate === null) {
        resolve(candidates);
      } else {
        candidates.push(candidate);
      }
    });
  });
}

/** Resolve once the connection reaches `connected`; reject on `failed`. */
function waitConnected(pc) {
  return new Promise((resolve, reject) => {
    const check = () => {
      if (pc.connectionState === "connected") {
        resolve();
      } else if (["failed", "closed"].includes(pc.connectionState)) {
        reject(new Error(`wait-connected: connection ${pc.connectionState}`));
      }
    };
    pc.addEventListener("connectionstatechange", check);
    check();
  });
}

/** Publish this peer's gathered candidates, an end marker, then done. */
async function publishCandidates(pc, mailbox, gathered) {
  for (const c of await gathered) {
    await mailbox.send(
      encodeSignal({
        type: "candidate",
        candidate: c.candidate,
        sdp_mid: c.sdpMid ?? null,
        sdp_mline_index: c.sdpMLineIndex ?? null,
      }),
    );
  }
  await mailbox.send(encodeSignal({ type: "end-of-candidates" }));
  await mailbox.done();
}

/** Consume the peer's remaining signals (answer and/or trickled candidates). */
async function consumeSignaling(pc, mailbox) {
  for (;;) {
    const blob = await mailbox.recv();
    if (blob === undefined) {
      return;
    }
    const signal = decodeSignal(blob);
    switch (signal.type) {
      case "answer":
        await pc.setRemoteDescription({ type: "answer", sdp: signal.sdp });
        break;
      case "offer":
        throw new Error("unexpected second offer");
      case "candidate":
        await pc.addIceCandidate({
          candidate: signal.candidate,
          sdpMid: signal.sdp_mid ?? null,
          sdpMLineIndex: signal.sdp_mline_index ?? null,
        });
        break;
      case "end-of-candidates":
        break;
      default:
        throw new Error(`unknown signal type ${JSON.stringify(signal.type)}`);
    }
  }
}

function channelInit(testId) {
  return testId === "max-retransmits-accepted" ? { maxRetransmits: 0 } : {};
}

/** Drive one side of the handshake to a connected, open data channel. The
 * receiver is constructed the moment the channel exists, so no message the
 * remote sends early is dropped. */
async function handshake(testId, role, mailbox) {
  const pc = new wrtc.RTCPeerConnection({ iceServers: [] });
  const gathered = gatherCandidates(pc);

  let dc;
  let receiver;
  if (role === "offerer") {
    dc = pc.createDataChannel(CHANNEL_LABEL, channelInit(testId));
    receiver = new Receiver(dc);
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    dbg("offer published");
    await mailbox.send(encodeSignal({ type: "offer", sdp: offer.sdp }));
    await publishCandidates(pc, mailbox, gathered);
    dbg("candidates published; consuming peer signaling");
    await consumeSignaling(pc, mailbox);
  } else {
    const incoming = new Promise((resolve) => {
      pc.addEventListener("datachannel", ({ channel }) => {
        // Attach the message listener synchronously with the event.
        resolve({ channel, receiver: new Receiver(channel) });
      });
    });
    const blob = await mailbox.recv();
    const signal = blob === undefined ? undefined : decodeSignal(blob);
    if (signal?.type !== "offer") {
      throw new Error(`expected offer, got ${JSON.stringify(signal?.type)}`);
    }
    dbg("offer received");
    await pc.setRemoteDescription({ type: "offer", sdp: signal.sdp });
    const answer = await pc.createAnswer();
    await pc.setLocalDescription(answer);
    await mailbox.send(encodeSignal({ type: "answer", sdp: answer.sdp }));
    await publishCandidates(pc, mailbox, gathered);
    dbg("answer + candidates published; consuming peer signaling");
    await consumeSignaling(pc, mailbox);
    ({ channel: dc, receiver } = await incoming);
  }

  dbg("signaling complete; waiting for connection");
  await waitConnected(pc);
  await channelOpen(dc);
  dbg("data channel open");
  return { pc, dc, receiver };
}

// --- data channel plumbing ---------------------------------------------------

function channelOpen(dc) {
  if (dc.readyState === "open") {
    return Promise.resolve();
  }
  return new Promise((resolve, reject) => {
    dc.addEventListener("open", resolve, { once: true });
    dc.addEventListener(
      "error",
      (e) => reject(new Error(`channel error: ${e.error ?? e}`)),
      { once: true },
    );
  });
}

/**
 * An async receive queue over a data channel: `next()` yields
 * `{ kind: "text", text }` or `{ kind: "binary", bytes }` per message, and
 * `{ kind: "closed" }` once the channel closes.
 */
class Receiver {
  #queue = [];
  #waiters = [];

  constructor(dc) {
    dc.binaryType = "arraybuffer";
    dc.addEventListener("message", ({ data }) => {
      this.#push(
        typeof data === "string"
          ? { kind: "text", text: data }
          : { kind: "binary", bytes: new Uint8Array(data) },
      );
    });
    dc.addEventListener("close", () => this.#push({ kind: "closed" }));
  }

  #push(item) {
    const waiter = this.#waiters.shift();
    if (waiter) {
      waiter(item);
    } else {
      this.#queue.push(item);
    }
  }

  next() {
    const item = this.#queue.shift();
    if (item !== undefined) {
      // A closed marker is terminal: leave it visible to later next() calls.
      if (item.kind === "closed") {
        this.#queue.unshift(item);
      }
      return Promise.resolve(item);
    }
    return new Promise((resolve) => this.#waiters.push(resolve));
  }
}

async function receiveMessage(receiver) {
  const item = await receiver.next();
  if (item.kind === "closed") {
    throw new Error("receive: closed");
  }
  return item;
}

// --- per-test payload exchange (mirrors the conformance guest) ---------------

/** An indexed, verifiable payload: 4-byte LE index + (index+offset) % 251. */
function makePayload(index, size) {
  const bytes = new Uint8Array(Math.max(size, 4));
  new DataView(bytes.buffer).setUint32(0, index, true);
  for (let offset = 0; offset < bytes.length - 4; offset += 1) {
    bytes[4 + offset] = (index + offset) % 251;
  }
  return bytes;
}

function payloadIndex(bytes) {
  if (bytes.length < 4) {
    return undefined;
  }
  return new DataView(bytes.buffer, bytes.byteOffset).getUint32(0, true);
}

function verifyPayload(bytes) {
  const index = payloadIndex(bytes);
  if (index === undefined) {
    return false;
  }
  for (let offset = 0; offset < bytes.length - 4; offset += 1) {
    if (bytes[4 + offset] !== (index + offset) % 251) {
      return false;
    }
  }
  return true;
}

function verifyAll(received, count, ordered = false) {
  if (received.length !== count) {
    throw new Error(`received ${received.length} messages, expected ${count}`);
  }
  received.forEach((bytes, position) => {
    if (!verifyPayload(bytes)) {
      throw new Error("payload failed integrity check");
    }
    if (ordered && payloadIndex(bytes) !== position) {
      throw new Error(`message ${position} carried index ${payloadIndex(bytes)}`);
    }
  });
}

const bytesEqual = (a, b) =>
  a.length === b.length && a.every((byte, i) => byte === b[i]);

async function expectBinary(receiver, expected, what) {
  const item = await receiveMessage(receiver);
  if (item.kind !== "binary" || !bytesEqual(item.bytes, expected)) {
    throw new Error(`${what} mismatch`);
  }
}

async function sendSequence(dc, count, size) {
  for (let index = 0; index < count; index += 1) {
    dc.send(makePayload(index, size));
  }
}

async function recvSequence(receiver, count) {
  const out = [];
  for (let i = 0; i < count; i += 1) {
    const item = await receiveMessage(receiver);
    out.push(
      item.kind === "binary" ? item.bytes : new TextEncoder().encode(item.text),
    );
  }
  return out;
}

/** Run the test's payload exchange; both peers run the same routine. */
async function exchange(testId, config, dc, receiver) {
  switch (testId) {
    case "label-round-trip": {
      if (dc.label !== CHANNEL_LABEL) {
        throw new Error(
          `label was ${JSON.stringify(dc.label)}, expected ${JSON.stringify(CHANNEL_LABEL)}`,
        );
      }
      return;
    }
    case "binary-message": {
      const payload = new Uint8Array([0, 1, 2, 3, 4, 5]);
      dc.send(payload);
      await expectBinary(receiver, payload, "binary payload");
      return;
    }
    case "text-message": {
      const text = "conformance text message";
      dc.send(text);
      const item = await receiveMessage(receiver);
      if (item.kind !== "text" || item.text !== text) {
        throw new Error("text payload mismatch");
      }
      return;
    }
    case "zero-length-message": {
      dc.send(new Uint8Array(0));
      dc.send("");
      await expectBinary(receiver, new Uint8Array(0), "empty binary message");
      const item = await receiveMessage(receiver);
      if (item.kind !== "text" || item.text !== "") {
        throw new Error("expected empty text message");
      }
      return;
    }
    case "large-message": {
      const payload = makePayload(0, Math.max(config.messageSize, 1024));
      dc.send(payload);
      await expectBinary(receiver, payload, "large payload");
      return;
    }
    case "max-retransmits-accepted": {
      const payload = new Uint8Array([9, 8, 7, 6]);
      dc.send(payload);
      await expectBinary(receiver, payload, "unreliable channel payload");
      return;
    }
    case "concurrent-send-receive":
    case "message-boundaries":
    case "ordering":
    case "payload-integrity":
    case "interop-handshake": {
      const count = Math.max(config.messageCount, 1);
      const size = Math.max(config.messageSize, 16);
      const [, received] = await Promise.all([
        sendSequence(dc, count, size),
        recvSequence(receiver, count),
      ]);
      verifyAll(received, count, testId === "ordering");
      return;
    }
    default:
      throw new Error(`unhandled test id ${JSON.stringify(testId)}`);
  }
}

/**
 * Final rendezvous: send a sentinel and wait for the peer's, so neither side
 * tears down while the other still needs the channel. A close counts as the
 * rendezvous, matching the conformance guest.
 */
async function barrier(dc, receiver) {
  const sentinel = new TextEncoder().encode(BARRIER_SENTINEL);
  try {
    dc.send(sentinel);
  } catch {
    return; // closed: the peer already completed its exchange
  }
  for (;;) {
    const item = await receiver.next();
    if (item.kind === "closed") {
      return;
    }
    if (item.kind === "binary" && bytesEqual(item.bytes, sentinel)) {
      return;
    }
    // Defensively skip anything still in flight before the sentinel.
  }
}

// --- entry point --------------------------------------------------------------

async function runTest(config) {
  const mailbox = new Mailbox(config.server, config.room, config.role);
  const { pc, dc, receiver } = await handshake(config.test, config.role, mailbox);
  try {
    await exchange(config.test, config, dc, receiver);
    dbg("exchange complete; entering barrier");
    await barrier(dc, receiver);
  } finally {
    pc.close();
  }
}

function parseCli() {
  const { values } = parseArgs({
    options: {
      test: { type: "string" },
      role: { type: "string" },
      server: { type: "string" },
      room: { type: "string", default: "r" },
      "message-count": { type: "string", default: "16" },
      "message-size": { type: "string", default: "512" },
    },
  });
  for (const flag of ["test", "role", "server"]) {
    if (!values[flag]) {
      throw new Error(`missing required --${flag}`);
    }
  }
  if (!["offerer", "answerer"].includes(values.role)) {
    throw new Error(`--role must be offerer or answerer, got ${values.role}`);
  }
  return {
    test: values.test,
    role: values.role,
    server: values.server,
    room: values.room,
    messageCount: Number(values["message-count"]),
    messageSize: Number(values["message-size"]),
  };
}

const timeout = new Promise((_, reject) => {
  setTimeout(() => reject(new Error("timed-out")), RUN_TIMEOUT_MS).unref();
});

let result;
try {
  await Promise.race([runTest(parseCli()), timeout]);
  result = { tag: "pass" };
} catch (err) {
  result = { tag: "fail", val: String(err?.message ?? err) };
}
console.log(JSON.stringify(result));
// wrtc's worker threads keep the event loop alive; exit explicitly.
process.exit(result.tag === "pass" ? 0 : 1);
