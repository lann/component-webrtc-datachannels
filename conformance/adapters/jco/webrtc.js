// The `lann:webrtc-datachannels/connections` host for the jco conformance
// adapters, written against the standard W3C WebRTC API so the exact same module
// runs under Node (backed by `@roamhq/wrtc`) and in a real browser (where the
// classes are globals). It is the conformance counterpart of `jco-impl/webrtc.js`
// — that module implements only the demo `connect` shortcut, whereas this one
// implements the full `connections` surface the conformance guest drives:
// `data-channel-options`, `data-channel`, and `peer-connection` (offer/answer,
// trickle ICE, incoming channels).
//
// jco wires this module in as the component's `connections` import (via
// `instantiate(..., { 'lann:webrtc-datachannels/connections': <this module> })`).
// Errors are surfaced to the guest by throwing the WIT `error` variant value
// (for example `{ tag: 'closed' }` or `{ tag: 'invalid-signaling', val }`), which
// jco lifts into the `result<_, error>` the WIT declares.

// Resolve `RTCPeerConnection` isomorphically: a browser (including headless
// Chromium) exposes the W3C class as a global; under Node it is provided by
// `@roamhq/wrtc`, imported lazily so the bare specifier never has to resolve in
// the browser. A missing Node dependency is surfaced with an actionable message
// rather than a bare module-resolution error.
async function resolveRTCPeerConnection() {
  if (globalThis.RTCPeerConnection) return globalThis.RTCPeerConnection;
  try {
    return (await import("@roamhq/wrtc")).default.RTCPeerConnection;
  } catch (cause) {
    throw new Error(
      "no RTCPeerConnection available: not running in a browser and @roamhq/wrtc " +
        "could not be loaded (run `npm install` in conformance/adapters/jco)",
      { cause },
    );
  }
}

const RTCPeerConnection = await resolveRTCPeerConnection();

// Keep the SCTP send buffer bounded; pause the producer when it fills.
const MAX_BUFFERED_AMOUNT = 8 * 1024 * 1024;

// How long `wait-connected` waits before failing with `error.timed-out`.
const CONNECT_TIMEOUT_MS = 20_000;

// The default bound on buffered inbound payload bytes awaiting `receive`.
// There is no wire-level inbound backpressure (the W3C API has no read-side
// flow control), so this bound is what protects memory from a slow guest
// reader: exceeding it closes the channel and, once the buffered backlog
// drains, `receive` fails with `error.receive-buffer-overflow`. Overridable —
// primarily as a test knob, so the conformance overflow probe needs only a
// small flood — through the `WEBRTC_MAX_INBOUND_BUFFER_BYTES` environment
// variable (Node) or a global of the same name (browsers).
const DEFAULT_MAX_INBOUND_BUFFERED = 8 * 1024 * 1024;

/** The configured inbound buffer bound, resolved lazily per channel. */
function maxInboundBuffered() {
  const configured = Number(
    globalThis.WEBRTC_MAX_INBOUND_BUFFER_BYTES ??
      globalThis.process?.env?.WEBRTC_MAX_INBOUND_BUFFER_BYTES,
  );
  return configured > 0 ? configured : DEFAULT_MAX_INBOUND_BUFFERED;
}

/**
 * The `data-channel-options` resource: a configuration builder for a data
 * channel, mirroring `wasi:http`'s `request-options`. The guest constructs a
 * default value, adjusts fields through the setters, and hands it to
 * `peer-connection.create-data-channel`.
 */
export class DataChannelOptions {
  #label = "";
  #ordered = true;
  #maxRetransmits = undefined;

  /** The channel label. */
  label() {
    return this.#label;
  }
  /** @param {string} label */
  setLabel(label) {
    this.#label = label;
  }

  /** Whether messages are delivered in order. */
  ordered() {
    return this.#ordered;
  }
  /** @param {boolean} ordered */
  setOrdered(ordered) {
    this.#ordered = ordered;
  }

  /** The maximum number of retransmissions, or `undefined` for reliable delivery. */
  maxRetransmits() {
    return this.#maxRetransmits;
  }
  /** @param {number | undefined} maxRetransmits */
  setMaxRetransmits(maxRetransmits) {
    this.#maxRetransmits = maxRetransmits;
  }

  /** The `RTCDataChannelInit` these options describe. */
  toInit() {
    const init = { ordered: this.#ordered };
    if (this.#maxRetransmits != null) {
      init.maxRetransmits = this.#maxRetransmits;
    }
    return init;
  }
}

/**
 * The `data-channel` resource, implemented over an `RTCDataChannel`.
 *
 * `send`/`receive` each carry exactly one data-channel message, preserving
 * WebRTC message boundaries. A message is a variant: `{ tag: 'binary', val:
 * Uint8Array }` or `{ tag: 'string', val: string }`.
 */
export class DataChannel {
  #channel;
  #incoming;
  // Set once `receive-via-stream` has claimed the inbound messages; further
  // `receive`/`receive-via-stream` calls fail with `receiving-via-stream`.
  #streamClaimed = false;

  constructor(channel) {
    this.#channel = channel;
    channel.binaryType = "arraybuffer";
    this.#incoming = incomingQueue(channel);
  }

  /** The negotiated channel label. */
  label() {
    return this.#channel.label;
  }

  /**
   * Send a single message on the channel, resolving once it has been handed to
   * the transport or rejecting with `{ tag: 'closed' }` if the channel closed.
   * @param {{ tag: 'binary', val: Uint8Array } | { tag: 'string', val: string }} message
   */
  async send(message) {
    await this.#waitOpen();
    await this.#waitForDrain();
    try {
      this.#channel.send(message.val);
    } catch {
      throw { tag: "closed" };
    }
  }

  /**
   * Receive a single message, resolving with the next inbound `message` variant
   * or rejecting with `{ tag: 'closed' }` once the channel closes (or with
   * `{ tag: 'receiving-via-stream' }` once `receiveViaStream` has claimed the
   * inbound messages).
   */
  async receive() {
    if (this.#streamClaimed) throw { tag: "receiving-via-stream" };
    return this.#incoming.next();
  }

  /**
   * Send a stream of messages whose payloads are each streamed as bytes.
   * `messages` is a `ReadableStream` of `stream-message` records whose `data`
   * is a byte `ReadableStream`. Rejects with the WIT `send-via-stream-error`
   * record `{ error, sent }` if the channel closes early or a message's
   * payload does not match its declared `length`.
   * @param {ReadableStream<{ kind: 'binary'|'string', length: number, data: ReadableStream }>} messages
   */
  async sendViaStream(messages) {
    let sent = 0n;
    try {
      for await (const item of streamItems(messages)) {
        const bytes = await collectByteStream(item.data);
        if (bytes.length !== item.length) {
          throw {
            tag: "other",
            val: `stream-message payload was ${bytes.length} bytes but length declared ${item.length}`,
          };
        }
        const message =
          item.kind === "string"
            ? { tag: "string", val: new TextDecoder().decode(bytes) }
            : { tag: "binary", val: bytes };
        await this.send(message);
        sent += 1n;
      }
    } catch (error) {
      throw { error: typeof error?.tag === "string" ? error : { tag: "closed" }, sent };
    }
  }

  /**
   * Take over the channel's inbound messages, delivering each as a
   * `stream-message` whose payload is a byte `ReadableStream`. Once-only: a
   * second call (or any later `receive`) throws
   * `{ tag: 'receiving-via-stream' }`, and any pending `receive` is resolved
   * with it. The stream ends when the channel closes.
   */
  receiveViaStream() {
    if (this.#streamClaimed) throw { tag: "receiving-via-stream" };
    this.#streamClaimed = true;
    const incoming = this.#incoming;
    incoming.rejectWaiters({ tag: "receiving-via-stream" });
    return new ReadableStream({
      async pull(controller) {
        let message;
        try {
          message = await incoming.next();
        } catch {
          // The channel closed (or its inbound buffer overflowed): the
          // stream simply ends, per the WIT contract.
          controller.close();
          return;
        }
        const bytes =
          message.tag === "string" ? new TextEncoder().encode(message.val) : message.val;
        controller.enqueue({
          kind: message.tag,
          length: bytes.length,
          data: bytesToStream(bytes),
        });
      },
    });
  }

  /** Resolve once the channel is open, or reject `{ tag: 'closed' }` if it closes. */
  #waitOpen() {
    const channel = this.#channel;
    if (channel.readyState === "open") return Promise.resolve();
    if (channel.readyState === "closing" || channel.readyState === "closed") {
      return Promise.reject({ tag: "closed" });
    }
    return new Promise((resolve, reject) => {
      channel.addEventListener("open", () => resolve(), { once: true });
      channel.addEventListener("close", () => reject({ tag: "closed" }), { once: true });
      channel.addEventListener("error", () => reject({ tag: "closed" }), { once: true });
    });
  }

  /** Apply backpressure so a fast producer cannot overrun the SCTP buffer. */
  #waitForDrain() {
    const channel = this.#channel;
    if (channel.bufferedAmount <= MAX_BUFFERED_AMOUNT) return Promise.resolve();
    return new Promise((resolve) => {
      channel.bufferedAmountLowThreshold = MAX_BUFFERED_AMOUNT / 2;
      const onLow = () => {
        channel.removeEventListener("bufferedamountlow", onLow);
        resolve();
      };
      channel.addEventListener("bufferedamountlow", onLow);
    });
  }
}

/**
 * A single WebRTC peer connection driving the full `RTCPeerConnection`-style
 * signaling surface: offer/answer, trickle ICE, and in-band data channels.
 */
export class PeerConnection {
  #pc;
  #candidates;
  #channels;
  /** Latched true once the connection has ever reached `connected`. */
  #everConnected = false;
  /** True once `close()` has been called. */
  #closed = false;
  /** Take-once claims for the resource's two streams (see the WIT contract). */
  #candidatesTaken = false;
  #channelsTaken = false;
  /**
   * Pending `waitConnected` rejecters, woken by a local `close()` — the W3C
   * `close()` transitions the state without firing `connectionstatechange`,
   * so a pending waiter would otherwise hang to its timeout.
   */
  #closeHooks = new Set();

  constructor() {
    this.#pc = new RTCPeerConnection();

    // Latch `connected` as soon as it is reached, independent of any
    // `waitConnected` caller: the WIT contract keeps reporting a
    // once-connected connection as connected even after a later close.
    const latch = () => {
      if (this.#isConnectedNow()) this.#everConnected = true;
    };
    this.#pc.addEventListener("connectionstatechange", latch);
    this.#pc.addEventListener("iceconnectionstatechange", latch);

    // Local ICE candidates: a `null` (or empty) candidate ends the stream.
    this.#candidates = eventStream((push, end) => {
      this.#pc.addEventListener("icecandidate", ({ candidate }) => {
        if (candidate == null || candidate.candidate === "") {
          end();
          return;
        }
        push({
          candidate: candidate.candidate,
          sdpMid: candidate.sdpMid ?? undefined,
          sdpMlineIndex: candidate.sdpMLineIndex ?? undefined,
        });
      });
    });

    // Data channels opened by the remote peer.
    this.#channels = eventStream((push) => {
      this.#pc.addEventListener("datachannel", ({ channel }) => {
        push(new DataChannel(channel));
      });
    });
  }

  /**
   * Throw `{ tag: 'closed' }` when the connection is terminally over, per the
   * WIT contract for method calls made after `close` (the gate precedes any
   * input handling, so a malformed argument after close is still `closed`).
   */
  #requireOpen() {
    if (this.#closed || this.#pc.connectionState === "closed") {
      throw { tag: "closed" };
    }
  }

  #isConnectedNow() {
    return (
      this.#pc.connectionState === "connected" ||
      this.#pc.iceConnectionState === "connected" ||
      this.#pc.iceConnectionState === "completed"
    );
  }

  /**
   * Create a data channel negotiated in-band with the peer.
   * @param {DataChannelOptions} options
   */
  createDataChannel(options) {
    this.#requireOpen();
    try {
      const channel = this.#pc.createDataChannel(options.label(), options.toInit());
      return new DataChannel(channel);
    } catch (err) {
      throw { tag: "other", val: String(err) };
    }
  }

  /**
   * A stream of data channels opened by the remote peer. Take-once per the
   * WIT contract: later calls return a stream that ends immediately, and
   * channels are never re-delivered.
   */
  incomingDataChannels() {
    if (this.#channelsTaken) return emptyStream();
    this.#channelsTaken = true;
    return this.#channels.stream;
  }

  /** Produce an SDP offer describing the local peer. */
  async createOffer() {
    this.#requireOpen();
    try {
      const offer = await this.#pc.createOffer();
      return { kind: "offer", sdp: offer.sdp };
    } catch (err) {
      // Map to a WIT error rather than letting the rejection escape as a trap.
      throw { tag: "other", val: String(err) };
    }
  }

  /** Produce an SDP answer in response to a previously set remote offer. */
  async createAnswer() {
    this.#requireOpen();
    try {
      const answer = await this.#pc.createAnswer();
      return { kind: "answer", sdp: answer.sdp };
    } catch (err) {
      // Map to a WIT error rather than letting the rejection escape as a trap.
      throw { tag: "other", val: String(err) };
    }
  }

  /**
   * Apply a local description produced by `createOffer`/`createAnswer`.
   * @param {{ kind: string, sdp: string }} description
   */
  async setLocalDescription(description) {
    this.#requireOpen();
    try {
      await this.#pc.setLocalDescription({ type: description.kind, sdp: description.sdp });
    } catch (err) {
      throw { tag: "invalid-signaling", val: String(err) };
    }
  }

  /**
   * Apply the remote peer's description.
   * @param {{ kind: string, sdp: string }} description
   */
  async setRemoteDescription(description) {
    this.#requireOpen();
    try {
      await this.#pc.setRemoteDescription({ type: description.kind, sdp: description.sdp });
    } catch (err) {
      throw { tag: "invalid-signaling", val: String(err) };
    }
  }

  /**
   * A stream of locally gathered ICE candidates to trickle to the peer.
   * Take-once per the WIT contract: later calls return a stream that ends
   * immediately, and candidates are never re-delivered. End-of-candidates is
   * the stream ending.
   */
  localIceCandidates() {
    if (this.#candidatesTaken) return emptyStream();
    this.#candidatesTaken = true;
    return this.#candidates.stream;
  }

  /**
   * Add an ICE candidate received from the remote peer.
   * @param {{ candidate: string, sdpMid?: string, sdpMlineIndex?: number }} candidate
   */
  async addIceCandidate(candidate) {
    this.#requireOpen();
    try {
      await this.#pc.addIceCandidate({
        candidate: candidate.candidate,
        sdpMid: candidate.sdpMid ?? null,
        sdpMLineIndex: candidate.sdpMlineIndex ?? null,
      });
    } catch (err) {
      throw { tag: "invalid-signaling", val: String(err) };
    }
  }

  /**
   * Resolve once the connection reaches `connected`.
   *
   * `connected` is latched per the WIT contract: once the connection has ever
   * connected this resolves immediately — including after a later `close` —
   * and may be awaited repeatedly. If the connection closes or fails without
   * ever having connected it rejects `{ tag: 'closed' }`; a handshake that
   * can never complete (for example with no remote peer) rejects
   * `{ tag: 'timed-out' }` after `CONNECT_TIMEOUT_MS`.
   */
  async waitConnected() {
    const pc = this.#pc;
    const isFailed = () =>
      pc.connectionState === "failed" ||
      pc.iceConnectionState === "failed" ||
      pc.connectionState === "closed";

    if (this.#isConnectedNow()) this.#everConnected = true;
    if (this.#everConnected) return;
    if (this.#closed || isFailed()) throw { tag: "closed" };
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        cleanup();
        reject({ tag: "timed-out" });
      }, CONNECT_TIMEOUT_MS);
      const check = () => {
        if (this.#isConnectedNow()) {
          this.#everConnected = true;
          cleanup();
          resolve();
        } else if (isFailed()) {
          cleanup();
          reject({ tag: "closed" });
        }
      };
      const onClose = () => {
        cleanup();
        reject({ tag: "closed" });
      };
      const cleanup = () => {
        clearTimeout(timer);
        this.#closeHooks.delete(onClose);
        pc.removeEventListener("connectionstatechange", check);
        pc.removeEventListener("iceconnectionstatechange", check);
      };
      this.#closeHooks.add(onClose);
      pc.addEventListener("connectionstatechange", check);
      pc.addEventListener("iceconnectionstatechange", check);
    });
  }

  /**
   * Close the peer connection and any of its data channels. Idempotent; wakes
   * pending `waitConnected` callers (the W3C `close()` transitions the state
   * without firing events).
   */
  close() {
    if (this.#closed) return;
    this.#closed = true;
    this.#pc.close();
    for (const hook of this.#closeHooks) hook();
    this.#closeHooks.clear();
    this.#candidates.end();
    this.#channels.end();
  }
}

/**
 * A `ReadableStream` fed by an event source. `setup(push, end)` wires the source
 * to `push` each value and `end` to close the stream; values pushed before the
 * stream starts pulling are buffered.
 */
function eventStream(setup) {
  let controller;
  let ended = false;
  const buffer = [];
  const stream = new ReadableStream({
    start(c) {
      controller = c;
      for (const item of buffer) c.enqueue(item);
      buffer.length = 0;
      if (ended) c.close();
    },
  });
  const push = (item) => {
    if (controller) controller.enqueue(item);
    else buffer.push(item);
  };
  const end = () => {
    if (ended) return;
    ended = true;
    if (controller) {
      try {
        controller.close();
      } catch {
        // Already closed.
      }
    }
  };
  setup(push, end);
  return { stream, end };
}

/** A `ReadableStream` that ends immediately without yielding anything. */
function emptyStream() {
  return new ReadableStream({
    start(controller) {
      controller.close();
    },
  });
}

/**
 * Iterate a guest-provided WIT stream: jco hands the host its own async-iterable
 * `Stream` object (a web `ReadableStream` is also tolerated). Yields one stream
 * element per iteration.
 */
async function* streamItems(stream) {
  if (globalThis.ReadableStream && stream instanceof ReadableStream) {
    const reader = stream.getReader();
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        yield value;
      }
    } finally {
      reader.releaseLock();
    }
    return;
  }
  for await (const value of stream) {
    // A batched read yields an array of elements.
    if (Array.isArray(value)) {
      yield* value;
    } else {
      yield value;
    }
  }
}

/**
 * Coerce one chunk of a WIT byte stream (a number, an array of numbers, or a
 * typed array, depending on how the runtime batched the read) to a
 * `Uint8Array`.
 */
function toByteChunk(value) {
  if (typeof value === "number") return Uint8Array.of(value);
  if (value instanceof Uint8Array) return value;
  return Uint8Array.from(value);
}

/** A single-chunk byte `ReadableStream` over `bytes`. */
function bytesToStream(bytes) {
  return new ReadableStream({
    start(controller) {
      if (bytes.length) controller.enqueue(bytes);
      controller.close();
    },
  });
}

/** Collect every byte of a WIT byte stream into one `Uint8Array`. */
async function collectByteStream(stream) {
  const chunks = [];
  let total = 0;
  const push = (value) => {
    if (value === undefined || value === null) return;
    const chunk = toByteChunk(value);
    if (chunk.length) {
      chunks.push(chunk);
      total += chunk.length;
    }
  };
  if (globalThis.ReadableStream && stream instanceof ReadableStream) {
    const reader = stream.getReader();
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        push(value);
      }
    } finally {
      reader.releaseLock();
    }
  } else if (typeof stream.read === "function") {
    // jco's own Stream object: read in batches rather than per element.
    for (;;) {
      const { value, done } = await stream.read({ count: 65536 });
      push(value);
      if (done) break;
    }
  } else {
    for await (const value of stream) {
      push(value);
    }
  }
  return concatChunks(chunks, total);
}

/** Concatenate `chunks` (totalling `total` bytes) into one `Uint8Array`. */
function concatChunks(chunks, total) {
  const out = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.length;
  }
  return out;
}

/**
 * Build a per-message inbound queue over `channel`. Each received message is
 * tagged as a `message` variant (`{ tag: 'binary', val: Uint8Array }` for binary
 * frames, `{ tag: 'string', val: string }` for text frames). `next()` resolves
 * with the next message, or rejects with `{ tag: 'closed' }` once the channel
 * closes with no more messages pending.
 *
 * Buffering is bounded by `maxInboundBuffered()` payload bytes: a message that
 * would exceed it closes the channel and discards that and any later messages;
 * the pre-overflow backlog stays deliverable, after which `next()` rejects with
 * `{ tag: 'receive-buffer-overflow' }`.
 */
function incomingQueue(channel) {
  const limit = maxInboundBuffered();
  const messages = [];
  const waiters = [];
  let buffered = 0;
  let overflowed = false;
  let closed = false;

  const push = (message, size) => {
    const waiter = waiters.shift();
    if (waiter) {
      waiter.resolve(message);
    } else {
      buffered += size;
      messages.push({ message, size });
    }
  };

  channel.addEventListener("message", ({ data }) => {
    if (overflowed) return;
    const size = typeof data === "string" ? data.length : data.byteLength;
    if (buffered + size > limit && !waiters.length) {
      // The bounded inbound buffer overflowed: close the channel and discard
      // this and any later messages. Already-buffered messages stay deliverable.
      overflowed = true;
      channel.close();
      return;
    }
    const message =
      typeof data === "string"
        ? { tag: "string", val: data }
        : { tag: "binary", val: new Uint8Array(data) };
    push(message, size);
  });

  const endError = () => (overflowed ? { tag: "receive-buffer-overflow" } : { tag: "closed" });
  const end = () => {
    if (closed) return;
    closed = true;
    while (waiters.length) {
      waiters.shift().reject(endError());
    }
  };
  channel.addEventListener("close", end);
  channel.addEventListener("error", end);

  return {
    next() {
      if (messages.length) {
        const { message, size } = messages.shift();
        buffered -= size;
        return Promise.resolve(message);
      }
      if (overflowed) return Promise.reject({ tag: "receive-buffer-overflow" });
      if (closed) return Promise.reject({ tag: "closed" });
      return new Promise((resolve, reject) => waiters.push({ resolve, reject }));
    },
    /** Reject every pending waiter with `error` (a WIT `error` variant value). */
    rejectWaiters(error) {
      while (waiters.length) {
        waiters.shift().reject(error);
      }
    },
  };
}
