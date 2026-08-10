// In-process loopback tests for the `deltic-impl` host module: two
// instantiations of the port's `PeerConnection`/`DataChannel` classes wired
// directly to each other (no signaling server), buffering trickled
// candidates until the peer has its remote description.
//
// Sanitizer note: `node-datachannel` keeps background native threads alive
// across the whole process. A `deno test` process cannot exit
// mid-suite, so every test below disables `sanitizeResources`/`sanitizeOps` —
// the leaked timers/ops belong to node-datachannel's native worker pool, not
// to unclosed resources this test forgot to close (every test does close its
// peer connections). `node-datachannel`'s own `cleanup()` is called once at
// the end of the whole suite (see the final `Deno.test` below);
// per-test cleanup would tear down the shared native
// context out from under any test that runs after it.

import { assert, assertEquals, assertRejects } from "jsr:@std/assert@1";
import {
  DataChannel,
  DataChannelOptions,
  PeerConnection,
  resetMaxInboundBufferBytes,
  setMaxInboundBufferBytes,
} from "../src/webrtc.ts";
import { WitError } from "@deltic/runtime/embedder";
import type { IceCandidate, Message, WebrtcError } from "../src/types.ts";

const NO_SANITIZE = { sanitizeResources: false, sanitizeOps: false };

/**
 * Wire two freshly constructed `PeerConnection`s through a full offer/answer
 * + trickle-ICE exchange, in-process (no network signaling server),
 * buffering each side's candidates until the peer has its remote
 * description.
 */
async function connectPair(): Promise<{ a: PeerConnection; b: PeerConnection }> {
  const a = await PeerConnection.create();
  const b = await PeerConnection.create();

  // Candidates start flowing as soon as a local description is set — on a
  // slow machine that is BEFORE the counterpart has its remote description,
  // and an addIceCandidate delivered that early is rejected
  // (invalid-signaling) and lost (the stream delivers each candidate once).
  // Hold each pump until its receiving peer is ready.
  let aHasRemote!: () => void;
  let bHasRemote!: () => void;
  const ready = {
    a: new Promise<void>((r) => (aHasRemote = r)),
    b: new Promise<void>((r) => (bHasRemote = r)),
  };
  const pumpCandidates = (
    from: PeerConnection,
    to: PeerConnection,
    toReady: Promise<void>,
  ) => {
    (async () => {
      // `localIceCandidates()` returns a plain `ReadableStream<IceCandidate>`
      // (one candidate per element — see src/webrtc.ts's
      // "Module wiring"/streams note), not a batched `Stream<T>` handle.
      for await (const candidate of from.localIceCandidates()) {
        await toReady;
        try {
          await to.addIceCandidate(candidate);
        } catch {
          // Connection may have moved on/closed; ignore stray trickles.
        }
      }
    })();
  };
  pumpCandidates(a, b, ready.b);
  pumpCandidates(b, a, ready.a);

  const offer = await a.createOffer();
  await a.setLocalDescription(offer);
  await b.setRemoteDescription(offer);
  bHasRemote();
  const answer = await b.createAnswer();
  await b.setLocalDescription(answer);
  await a.setRemoteDescription(answer);
  aHasRemote();

  await Promise.all([a.waitConnected(), b.waitConnected()]);
  return { a, b };
}

/** The first data channel `pc` receives via `incomingDataChannels`. */
async function firstIncoming(pc: PeerConnection): Promise<DataChannel> {
  for await (const ch of pc.incomingDataChannels()) return ch;
  throw new Error("incomingDataChannels ended with no channel");
}

Deno.test("loopback: text echo both directions", NO_SANITIZE, async () => {
  const { a, b } = await connectPair();
  try {
    const options = new DataChannelOptions();
    options.setLabel("chat");
    const chA = a.createDataChannel(options);
    const chB = await firstIncoming(b);

    await chA.send({ tag: "string", val: "hello from a" });
    const gotAtB = await chB.receive();
    assertEquals(gotAtB, { tag: "string", val: "hello from a" });

    await chB.send({ tag: "string", val: "hello from b" });
    const gotAtA = await chA.receive();
    assertEquals(gotAtA, { tag: "string", val: "hello from b" });
  } finally {
    a.close();
    b.close();
  }
});

Deno.test("loopback: binary echo + message-boundary preservation", NO_SANITIZE, async () => {
  const { a, b } = await connectPair();
  try {
    const chA = a.createDataChannel(new DataChannelOptions());
    const chB = await firstIncoming(b);

    const msg1 = new Uint8Array([1, 2, 3]);
    const msg2 = new Uint8Array([4, 5]);
    await chA.send({ tag: "binary", val: msg1 });
    await chA.send({ tag: "binary", val: msg2 });

    const got1 = await chB.receive();
    const got2 = await chB.receive();
    assertEquals(got1, { tag: "binary", val: msg1 });
    assertEquals(got2, { tag: "binary", val: msg2 });
  } finally {
    a.close();
    b.close();
  }
});

Deno.test("loopback: unordered/maxRetransmits options accepted", NO_SANITIZE, async () => {
  const { a, b } = await connectPair();
  try {
    const options = new DataChannelOptions();
    options.setOrdered(false);
    options.setMaxRetransmits(3);
    assertEquals(options.ordered(), false);
    assertEquals(options.maxRetransmits(), 3);

    const chA = a.createDataChannel(options);
    const chB = await firstIncoming(b);
    await chA.send({ tag: "string", val: "unordered ok" });
    assertEquals(await chB.receive(), { tag: "string", val: "unordered ok" });
  } finally {
    a.close();
    b.close();
  }
});

Deno.test("loopback: receive-via-stream consumes a burst", NO_SANITIZE, async () => {
  const { a, b } = await connectPair();
  try {
    const chA = a.createDataChannel(new DataChannelOptions());
    const chB = await firstIncoming(b);

    const received: Message[] = [];
    const streamDone = (async () => {
      const stream = chB.receiveViaStream();
      for await (const sm of stream) {
        const bytes = await collectU8(sm.data as unknown as AsyncIterable<Uint8Array>);
        received.push(
          sm.kind === "string"
            ? { tag: "string", val: new TextDecoder().decode(bytes) }
            : { tag: "binary", val: bytes },
        );
        if (received.length === 3) return;
      }
    })();

    for (let i = 0; i < 3; i++) {
      await chA.send({ tag: "string", val: `msg-${i}` });
    }
    await streamDone;
    assertEquals(received, [
      { tag: "string", val: "msg-0" },
      { tag: "string", val: "msg-1" },
      { tag: "string", val: "msg-2" },
    ]);
  } finally {
    a.close();
    b.close();
  }
});

async function collectU8(stream: AsyncIterable<Uint8Array>): Promise<Uint8Array> {
  const chunks: Uint8Array[] = [];
  let total = 0;
  for await (const chunk of stream) {
    chunks.push(chunk);
    total += chunk.length;
  }
  const out = new Uint8Array(total);
  let off = 0;
  for (const c of chunks) {
    out.set(c, off);
    off += c.length;
  }
  return out;
}

Deno.test("loopback: single-use violation -> receiving-via-stream error", NO_SANITIZE, async () => {
  const { a, b } = await connectPair();
  try {
    const chA = a.createDataChannel(new DataChannelOptions());
    const chB = await firstIncoming(b);
    void chA; // keep `a`'s channel referenced for symmetry/documentation

    chB.receiveViaStream();
    const err = await assertRejects(() => chB.receive(), WitError);
    assertEquals((err as WitError<WebrtcError>).payload, { tag: "receiving-via-stream" });

    // A second `receiveViaStream` call after the first also violates the
    // once-only rule (thrown synchronously, per the WIT contract).
    let threw: unknown;
    try {
      chB.receiveViaStream();
    } catch (e) {
      threw = e;
    }
    assert(threw instanceof WitError);
    assertEquals((threw as WitError<WebrtcError>).payload, { tag: "receiving-via-stream" });
  } finally {
    a.close();
    b.close();
  }
});

Deno.test("loopback: inbound-buffer overflow -> overflow-close semantics", NO_SANITIZE, async () => {
  setMaxInboundBufferBytes(16); // small bound: a handful of short messages overflow it
  try {
    const { a, b } = await connectPair();
    try {
      const chA = a.createDataChannel(new DataChannelOptions());
      const chB = await firstIncoming(b);

      // Send enough payload bytes to exceed the 16-byte bound; the receiver
      // never calls `receive()` while these arrive, so they all buffer up
      // and the overflow-close fires on the sender or receiver's channel.
      for (let i = 0; i < 20; i++) {
        try {
          await chA.send({ tag: "string", val: `0123456789-${i}` });
        } catch {
          break; // sender side observed the close once b's channel closed.
        }
      }

      // Drain whatever buffered before the overflow, then expect the
      // terminal `receive-buffer-overflow` error (per the WIT `data-channel`
      // resource doc: "messages buffered before the overflow remain
      // receivable, after which `receive` fails with
      // `error.receive-buffer-overflow`").
      let overflowed = false;
      for (let i = 0; i < 20; i++) {
        try {
          await chB.receive();
        } catch (e) {
          assert(e instanceof WitError);
          assertEquals((e as WitError<WebrtcError>).payload, {
            tag: "receive-buffer-overflow",
          });
          overflowed = true;
          break;
        }
      }
      assert(overflowed, "expected receive-buffer-overflow after draining the backlog");
    } finally {
      a.close();
      b.close();
    }
  } finally {
    resetMaxInboundBufferBytes();
  }
});

Deno.test("loopback: close propagation + post-close error cases", NO_SANITIZE, async () => {
  const { a, b } = await connectPair();
  const chA = a.createDataChannel(new DataChannelOptions());
  const chB = await firstIncoming(b);

  chA.close();
  const err = await assertRejects(() => chA.send({ tag: "string", val: "x" }), WitError);
  assertEquals((err as WitError<WebrtcError>).payload, { tag: "closed" });

  // The peer observes the remote close too (eventually `receive` fails).
  let sawClosed = false;
  for (let i = 0; i < 50; i++) {
    try {
      await chB.receive();
    } catch (e) {
      assert(e instanceof WitError);
      sawClosed = true;
      break;
    }
  }
  assert(sawClosed, "expected the peer's receive() to observe the remote close");

  a.close();
  b.close();
  const connErr = await assertRejects(() => a.createOffer(), WitError);
  assertEquals((connErr as WitError<WebrtcError>).payload, { tag: "closed" });
});

Deno.test(
  "loopback: peer-connection close latches its channels synchronously",
  NO_SANITIZE,
  async () => {
    const { a, b } = await connectPair();
    const chA = a.createDataChannel(new DataChannelOptions());
    const chB = await firstIncoming(b);

    // Closing the PEER CONNECTION closes its owned channels with the close
    // observed locally at once (the WIT contract): the very first send()
    // after close() fails `closed`, on the locally created channel and on
    // the incoming (remote-created) one alike — regardless of how lazily
    // the backend transitions the native readyState.
    a.close();
    const errA = await assertRejects(
      () => chA.send({ tag: "string", val: "after-close" }),
      WitError,
    );
    assertEquals((errA as WitError<WebrtcError>).payload, { tag: "closed" });

    b.close();
    const errB = await assertRejects(
      () => chB.send({ tag: "string", val: "after-close" }),
      WitError,
    );
    assertEquals((errB as WitError<WebrtcError>).payload, { tag: "closed" });
  },
);

Deno.test("loopback: wait-connected resolves and is latched", NO_SANITIZE, async () => {
  const { a, b } = await connectPair();
  try {
    // Already connected; awaiting again resolves immediately (latched).
    await a.waitConnected();
    await b.waitConnected();
  } finally {
    a.close();
    b.close();
    // Latch survives close, per the WIT contract.
    await a.waitConnected();
    await b.waitConnected();
  }
});

// Run node-datachannel's cleanup once, after every test has finished, per
// the probe's discipline (probe.mjs:74/89-90) — it tears down the shared
// native ICE/DTLS/SCTP worker context so the process can exit. This must be
// the LAST registered test so it runs after all the loopback tests above.
Deno.test({
  name: "cleanup: node-datachannel native workers",
  ...NO_SANITIZE,
  fn: async () => {
    try {
      const nodeDatachannel = await import("node-datachannel");
      nodeDatachannel.cleanup?.();
      // Give libdatachannel's poll/worker threads time to join before the
      // test runner tears the isolate down: an exit that races the joins
      // can die in native teardown (SIGSEGV) after every test has already
      // passed. (A hard process exit right after cleanup() is fine — this
      // settle exists for harnesses that keep the runtime alive.)
      await new Promise((resolve) => setTimeout(resolve, 1000));
    } catch {
      // Not resolved to node-datachannel in this run (e.g. werift-forced
      // test environment, or a browser-like global) — nothing to clean up.
    }
  },
});
