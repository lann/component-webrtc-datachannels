// Environment-agnostic conformance corpus driver shared by the Node and browser
// jco runners. Given a factory that produces fresh guest instances and the base
// URL of a signaling server, it runs each registered test to a raw
// `pass`/`fail`/`skip` result and returns the adapter result rows.
//
// It mirrors the wasmtime adapter's orchestration (`conformance/adapters/
// wasmtime/src/main.rs`): a test is run either as a single in-process `both`
// instance (peer-connection API, error-taxonomy, and streaming probes), a
// single instance the guest reports `skipped` regardless of role (currently
// none), or two instances — an offerer and an answerer sharing one signaling
// room — for the behavioral/interop tests. Tests run in a single
// attempt (no retries): a nondeterministic failure is a real signal and must
// surface, not be masked by a second attempt. The guest owns every assertion;
// the driver only orchestrates and records.

/** The registry of test ids, mirroring `conformance/tests.toml`. */
export const TESTS = [
  "label-round-trip",
  "binary-message",
  "text-message",
  "message-boundaries",
  "zero-length-message",
  "large-message",
  "ordering",
  "payload-integrity",
  "concurrent-send-receive",
  "send-via-stream",
  "receive-via-stream",
  "receive-via-stream-once",
  "post-close-send",
  "receive-buffer-overflow",
  "max-retransmits-accepted",
  "error-invalid-signaling",
  "error-closed",
  "error-timed-out",
  "peer-offer-answer",
  "peer-create-data-channel",
  "peer-local-ice-candidates",
  "peer-add-ice-candidate",
  "peer-wait-connected",
  "peer-wait-connected-latch",
  "peer-streams-once",
  "post-close-signaling",
  "peer-close-releases",
  "peer-invalid-sdp",
  "interop-handshake",
];

// How a test is orchestrated across guest instances.
const IN_PROCESS = new Set([
  "peer-offer-answer",
  "peer-create-data-channel",
  "peer-local-ice-candidates",
  "peer-add-ice-candidate",
  "peer-wait-connected",
  "peer-wait-connected-latch",
  "peer-streams-once",
  "post-close-signaling",
  "peer-close-releases",
  "peer-invalid-sdp",
  "error-invalid-signaling",
  "error-closed",
  "error-timed-out",
  "post-close-send",
  "receive-buffer-overflow",
  "send-via-stream",
  "receive-via-stream",
  "receive-via-stream-once",
]);
const SKIP = new Set([]);

/** The orchestration plan for a test id: `in-process`, `skip`, or `two-peer`. */
export function planFor(testId) {
  if (IN_PROCESS.has(testId)) return "in-process";
  if (SKIP.has(testId)) return "skip";
  return "two-peer";
}

/** The `[messageCount, messageSize]` a test runs with. */
export function paramsFor(testId) {
  switch (testId) {
    case "large-message":
      return [1, 16384];
    // A 1 MiB flood: twice the MAX_INBOUND_BUFFER_BYTES bound the runners
    // configure, so the receiving side must overflow.
    case "receive-buffer-overflow":
      return [64, 16384];
    case "message-boundaries":
    case "ordering":
    case "payload-integrity":
    case "concurrent-send-receive":
    case "interop-handshake":
      return [16, 512];
    default:
      return [4, 256];
  }
}

// The inbound-buffer bound (in bytes) the jco runners configure through the
// host's `WEBRTC_MAX_INBOUND_BUFFER_BYTES` knob: small enough that the
// `receive-buffer-overflow` probe overflows it with a ~1 MiB flood instead of
// flooding the default 8 MiB bound (which starves concurrently running tests
// of the corpus). Mirrors the native adapters'
// CONFORMANCE_MAX_INBOUND_BUFFER_BYTES.
export const MAX_INBOUND_BUFFER_BYTES = 512 * 1024;

// The hang guard for one test, bounding a run whose data-channel wait never
// resolves. Generous: the whole attempt is on the clock under 4-wide CI
// contention, while the host's shorter `wait-connected` timeout fires first,
// so a genuine connection failure still surfaces as a WIT outcome rather than
// tripping this bound. Mirrors the native adapters' TEST_TIMEOUT.
const TEST_TIMEOUT_MS = 90_000;

/** Build a test config for one instance. */
function makeConfig(role, base, room, count, size) {
  return {
    role,
    signalingServer: base,
    room,
    messageCount: count,
    messageSize: size,
    trickle: true,
  };
}

/** Reject `promise` if it has not settled within `ms` milliseconds. */
function withTimeout(promise, ms, message) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(message)), ms);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

/**
 * Fold two per-instance results into one: any fail loses, else any skip, else
 * pass. Mirrors the wasmtime adapter's `fold_two`.
 */
function foldTwo(offerer, answerer) {
  if (offerer.tag === "fail" && answerer.tag === "fail") {
    return { tag: "fail", val: `offerer: ${offerer.val}; answerer: ${answerer.val}` };
  }
  if (offerer.tag === "fail") return { tag: "fail", val: `offerer: ${offerer.val}` };
  if (answerer.tag === "fail") return { tag: "fail", val: `answerer: ${answerer.val}` };
  if (offerer.tag === "skipped") return offerer;
  if (answerer.tag === "skipped") return answerer;
  return { tag: "pass" };
}

/** Run one guest instance to a `test-result`. */
async function runInstance(newInstance, testId, config) {
  const instance = await newInstance();
  return instance.runner.runTest(testId, config);
}

/**
 * Run one test to a raw result (single attempt; no retries). `roomSeq` is a
 * mutable `{ n }` counter shared across the corpus so rooms never collide
 * between concurrent tests.
 */
async function runTest(newInstance, base, testId, roomSeq) {
  const [count, size] = paramsFor(testId);
  const plan = planFor(testId);
  const room = `conf-${testId}-${roomSeq.n++}`;

  let result;
  try {
    const run = (async () => {
      switch (plan) {
        case "two-peer": {
          const [offerer, answerer] = await Promise.all([
            runInstance(newInstance, testId, makeConfig("offerer", base, room, count, size)),
            runInstance(newInstance, testId, makeConfig("answerer", base, room, count, size)),
          ]);
          return foldTwo(offerer, answerer);
        }
        case "in-process":
          return runInstance(newInstance, testId, makeConfig("both", base, room, count, size));
        default: // skip
          return runInstance(newInstance, testId, makeConfig("offerer", base, room, count, size));
      }
    })();
    result = await withTimeout(run, TEST_TIMEOUT_MS, "attempt timed-out");
  } catch (err) {
    return { test_id: testId, status: "fail", detail: String(err && err.message ? err.message : err) };
  }

  if (result.tag === "pass") {
    return { test_id: testId, status: "pass" };
  }
  if (result.tag === "skipped") {
    return { test_id: testId, status: "skip", detail: result.val };
  }
  return { test_id: testId, status: "fail", detail: result.val };
}

/**
 * Run the corpus and return the adapter result rows.
 * @param {object} opts
 * @param {string} opts.base signaling server base URL
 * @param {() => Promise<{ runner: { runTest: Function } }>} opts.newInstance
 *   factory producing a fresh guest instance
 * @param {string[]} [opts.only] run only these test ids (empty => all)
 * @param {number} [opts.jobs] how many tests to run concurrently; each test's
 *   peers use their own signaling room, so tests are independent
 * @param {(msg: string) => void} [opts.log] progress logger
 * @returns {Promise<Array<{ test_id: string, status: string, detail?: string }>>}
 */
export async function runCorpus({ base, newInstance, only = [], jobs = 4, log = () => {} }) {
  const roomSeq = { n: 0 };
  const ids = TESTS.filter((testId) => !only.length || only.includes(testId));
  const results = new Array(ids.length);
  let next = 0;
  const worker = async () => {
    for (;;) {
      const index = next++;
      if (index >= ids.length) return;
      const testId = ids[index];
      const result = await runTest(newInstance, base, testId, roomSeq);
      log(`${testId} … ${result.status}\n`);
      results[index] = result;
    }
  };
  await Promise.all(
    Array.from({ length: Math.max(1, jobs) }, () => worker()),
  );
  return results;
}
