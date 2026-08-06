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

import { envelope, inventoryLookup, runCases } from "@polymorph/component-test-js/harness";
import { Context } from "@polymorph/component-test-js/context";

// The single-attempt wall bound per case, matching the wasmtime leg's
// case timeout: a wedged case is reported (limit-exceeded provenance),
// never retried, and never allowed to hang the leg. JSPI attempts
// cannot be cancelled — the abandoned attempt's promise keeps running
// until the leg exits, which is why every case gets a fresh instance
// (freshCases below): a timed-out instance may be wedged mid-suspension.
const CASE_TIMEOUT_MS = 90_000;

/**
 * The suite's import object, both key spellings (the generated code
 * mixes versioned and unversioned): the SUT host, the suite mailbox,
 * the test-context provider (upstream's), the config environment, and
 * the wasi shims.
 */
export function bindImports({ connections, mailbox, env, cli, clocks, io }) {
  const imports = {};
  const bind = (name, impl) => {
    imports[name] = impl;
    const versioned = name.startsWith("wasi:") ? `${name}@0.2.0` : `${name}@0.1.0`;
    imports[versioned] = impl;
  };
  bind("polymorph:webrtc-datachannels/connections", connections);
  bind("conformance:signaling/mailbox", mailbox);
  bind("polymorph:test/test-context", { Context });
  bind("wasi:cli/environment", envInterface(env));
  bind("wasi:cli/exit", cli.exit);
  bind("wasi:cli/stdin", cli.stdin);
  bind("wasi:cli/stdout", cli.stdout);
  bind("wasi:cli/stderr", cli.stderr);
  bind("wasi:cli/terminal-input", cli.terminalInput);
  bind("wasi:cli/terminal-output", cli.terminalOutput);
  bind("wasi:cli/terminal-stdin", cli.terminalStdin);
  bind("wasi:cli/terminal-stdout", cli.terminalStdout);
  bind("wasi:cli/terminal-stderr", cli.terminalStderr);
  bind("wasi:clocks/monotonic-clock", clocks.monotonicClock);
  bind("wasi:clocks/wall-clock", clocks.wallClock);
  bind("wasi:io/error", io.error);
  bind("wasi:io/poll", io.poll);
  bind("wasi:io/streams", io.streams);
  return imports;
}

/** One environment interface for both legs (explicit > shim-internal). */
export function envInterface(vars) {
  return {
    getEnvironment: () => vars,
    getArguments: () => [],
    initialCwd: () => undefined,
  };
}

/**
 * Run the selected slice of the suite through the upstream case loop.
 * `newInstance()` must return a fresh instantiated suite (exports
 * object); `coreBytes` are the transpiled core wasm bytes carrying the
 * tag inventory; `select` is the case-name prefix this invocation runs
 * (empty = everything); `emit(line)` receives each JSONL line.
 */
export async function runSuite({ newInstance, coreBytes, target, suiteName, select, emit, log }) {
  emit(JSON.stringify(envelope(target, suiteName)));
  const tagsOf = inventoryLookup(coreBytes);
  const census = await (await newInstance()).tests.all();
  const selected = select
    ? census.filter((c) => String(c.name()).startsWith(select))
    : census;
  const counts = await runCases({
    cases: selected,
    Context,
    tagsOf,
    missing: [],
    emit: (event) => {
      emit(JSON.stringify(event));
      log?.(`${event.case} … ${event.status}`);
    },
    caseTimeoutMs: CASE_TIMEOUT_MS,
    freshCases: async () => (await newInstance()).tests.all(),
  });
  if (counts.total === 0) {
    throw new Error("suite enumerated zero cases (empty selection is a run error)");
  }
  emit(JSON.stringify({ "segment-end": true }));
  return { total: counts.total, failed: counts.failed };
}
