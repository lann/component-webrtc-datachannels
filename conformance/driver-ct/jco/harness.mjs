// Leg-shared glue for the jco conformance legs: the SUT import wiring
// (bindImports) and the thin suite loop over the upstream runner core
// (@polymorph/component-test-js — one harness for every runner; the
// case loop, verdict mapping, and tag inventory live there, not here).
// Runs in Node and inside the browser page unchanged (the browser page
// maps the bare specifiers through an import map; see run-browser.mjs).
//
// Scheduling: the upstream loop mark-schedules against the tag
// inventory read from the transpiled core wasm. The suite is untagged
// and the manifests declare no features, so `missing` is always empty —
// but the inventory lookup still gates drift: a case the tags section
// does not cover fails the run as unsound (the jco analog of the
// wasmtime runner's cross-check). The name hierarchy carries the
// topology instead: the caller selects `solo/` or `pair/` by prefix,
// and pair instances read their role from the instance environment.
//
// Cases run sequentially: the corpus is loopback-I/O-bound, and
// sequential execution keeps the two sides of a pair run in lockstep
// (same enumeration order, per-case mailbox rendezvous).

import { inventoryLookup, runSuiteJsonl } from "@polymorph/component-test-js/harness";
import { bindImports as bindCoreImports } from "@polymorph/component-test-js/imports";

// The single-attempt wall bound per case, matching the wasmtime leg's
// case timeout: a wedged case is reported (limit-exceeded provenance),
// never retried, and never allowed to hang the leg. JSPI attempts
// cannot be cancelled — the abandoned attempt's promise keeps running
// until the leg exits, which is why every case gets a fresh instance
// (freshCases below): a timed-out instance may be wedged mid-suspension.
const CASE_TIMEOUT_MS = 90_000;

/**
 * The suite's import object over the upstream builder: the SUT host,
 * the suite mailbox, the test-context provider, the config
 * environment, and the wasi shims.
 */
export function bindImports({ connections, mailbox, env, cli, clocks, io }) {
  return bindCoreImports({
    wasi: { cli, clocks, io },
    env,
    sut: {
      "polymorph:webrtc-datachannels/connections": connections,
      "conformance:signaling/mailbox": mailbox,
    },
  });
}

/** One environment interface for both legs (explicit > shim-internal). */
/**
 * Run the selected slice of the suite through the upstream case loop.
 * `newInstance()` must return a fresh instantiated suite (exports
 * object); `coreBytes` are the transpiled core wasm bytes carrying the
 * tag inventory; `select` is the case-name prefix this invocation runs
 * (empty = everything); `emit(line)` receives each JSONL line.
 */
export async function runSuite({ newInstance, coreBytes, target, suiteName, select, emit, log }) {
  // The prefix selection (solo/ vs pair/) wraps the tests interface so
  // the upstream loop's census and per-case fresh instances both see
  // the same filtered view. The suites export the bare `tests`
  // spelling; this module also runs in the browser page, where the
  // node-runner helpers cannot load.
  const newTests = async () => {
    const tests = (await newInstance()).tests;
    if (!select) return tests;
    return {
      all: async () =>
        (await tests.all()).filter((c) => String(c.name()).startsWith(select)),
    };
  };
  const counts = await runSuiteJsonl({
    newTests,
    tagsOf: inventoryLookup(coreBytes),
    target,
    suiteName,
    emit,
    log,
    caseTimeoutMs: CASE_TIMEOUT_MS,
  });
  return { total: counts.total, failed: counts.failed };
}
