// The jco Node conformance adapter: runs the shared conformance guest against
// the browser-first host (`webrtc.js` + `signaling.js`) under Node, backed by
// `@roamhq/wrtc`, and emits the adapter result document the conformance runner
// consumes (`conformance/results/jco-node.json`).
//
// The transpiled guest (produced by `npm run transpile`) is instantiated in
// jco's `--instantiation` mode so this one process can stand up two independent
// guest instances — an offerer and an answerer — for the two-peer behavioral
// tests, exactly as the wasmtime adapter runs two stores. The shared driver
// (`driver.js`) owns the plan/fold orchestration.
//
// jco's async ABI needs JavaScript Promise Integration (JSPI), so this must run
// under a JSPI-capable runtime: Node 24+ with `--experimental-wasm-jspi`. The
// `just conformance::jco-node` recipe supplies both.
import { spawn } from "node:child_process";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { availableParallelism } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

import { runCorpus, MAX_INBOUND_BUFFER_BYTES } from "./driver.js";
import * as connections from "./webrtc.js";
import { Session } from "./signaling.js";

// Shrink the host's inbound-buffer bound so the `receive-buffer-overflow`
// probe overflows it with a small flood (webrtc.js resolves the bound lazily
// per channel, so setting the global here covers every instance).
globalThis.WEBRTC_MAX_INBOUND_BUFFER_BYTES = MAX_INBOUND_BUFFER_BYTES;

const ADAPTER_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(ADAPTER_DIR, "..", "..", "..");

// The default test concurrency: 2x the cores available to this process,
// clamped to [2, 8]. The corpus is I/O-bound so the optimum exceeds the core
// count (measured at 3x cores on the lighter Rust adapters), but each test
// here runs inside a heavyweight JS runtime whose flake history is
// load-induced timeouts, so the multiplier stays conservative: the previously
// proven load (4) on 2-core CI runners, scaling up on larger machines.
function defaultJobs() {
  return Math.min(8, Math.max(2, 2 * availableParallelism()));
}

const { values } = parseArgs({
  options: {
    generated: { type: "string", default: join(ADAPTER_DIR, "generated") },
    out: { type: "string", default: join(REPO_ROOT, "conformance", "results") },
    target: { type: "string", default: "jco-node" },
    environment: { type: "string", default: "loopback" },
    // Base URL of an already-running signaling server. When omitted, this
    // adapter spawns its own `conformance-signalingd`.
    server: { type: "string" },
    "signaling-bin": {
      type: "string",
      default: join(REPO_ROOT, "target", "debug", "conformance-signalingd"),
    },
    only: { type: "string", multiple: true, default: [] },
    // How many tests to run concurrently (each test's peers use their own
    // signaling room, so tests are independent); defaults to defaultJobs().
    jobs: { type: "string" },
    // Single-instance interop mode: run exactly one guest instance for one
    // `--test`/`--role`/`--room` against an already-running `--server`, printing
    // the raw `test-result` (`{ "tag": ... }`) as JSON to stdout. Used by the
    // cross-runtime interop orchestrator (conformance-interop) to drive the
    // jco-node half of a wasmtime<->jco-node pair.
    interop: { type: "boolean", default: false },
    role: { type: "string", default: "offerer" },
    room: { type: "string", default: "interop" },
    test: { type: "string", default: "interop-handshake" },
    "message-count": { type: "string", default: "16" },
    "message-size": { type: "string", default: "512" },
  },
});

/** Compile the guest's core wasm modules so `instantiate` can resolve them synchronously. */
async function loadCoreModules(generatedDir) {
  const names = [
    "conformance-guest.core.wasm",
    "conformance-guest.core2.wasm",
    "conformance-guest.core3.wasm",
  ];
  const modules = new Map();
  for (const name of names) {
    modules.set(name, await WebAssembly.compile(await readFile(join(generatedDir, name))));
  }
  return modules;
}

/** Start `conformance-signalingd`, returning its base URL and a shutdown handle. */
async function spawnSignaling(bin) {
  const child = spawn(bin, ["--host", "127.0.0.1", "--port", "0"], {
    stdio: ["ignore", "pipe", "inherit"],
  });
  const base = await new Promise((resolveUrl, rejectUrl) => {
    let buffer = "";
    const onData = (chunk) => {
      buffer += chunk;
      const match = /listening on (http:\/\/\S+)/.exec(buffer);
      if (match) {
        child.stdout.off("data", onData);
        resolveUrl(match[1].trim());
      }
    };
    child.stdout.on("data", onData);
    child.on("exit", (code) =>
      rejectUrl(new Error(`signaling server exited before reporting a URL (exit code ${code})`)),
    );
    setTimeout(() => rejectUrl(new Error("signaling server did not report a URL in time")), 10_000);
  });
  await waitHealthy(base);
  return {
    base,
    async shutdown() {
      child.kill("SIGTERM");
    },
  };
}

/** Poll `${base}/healthz` until it responds `200` or the deadline elapses. */
async function waitHealthy(base) {
  const deadline = Date.now() + 10_000;
  for (;;) {
    try {
      const resp = await fetch(`${base}/healthz`);
      if (resp.ok) return;
    } catch {
      // not up yet
    }
    if (Date.now() > deadline) throw new Error("signaling server never became healthy");
    await new Promise((r) => setTimeout(r, 100));
  }
}

async function main() {
  const generatedDir = values.generated;
  const { instantiate } = await import(join(generatedDir, "conformance-guest.js"));
  const modules = await loadCoreModules(generatedDir);

  const newInstance = () =>
    instantiate((name) => modules.get(name), {
      "conformance:signaling/mailbox": { Session },
      "lann:webrtc-datachannels/connections": connections,
    });

  // Single-instance interop mode: one guest instance, one test/role/room against
  // an already-running server; emit just the raw result to stdout. The `[phase]`
  // stderr markers are streamed live by the orchestrator, so if its hang guard
  // kills this process the last marker identifies the hung phase.
  if (values.interop) {
    if (!values.server) throw new Error("--interop requires --server");
    const phase = (msg) =>
      process.stderr.write(`[phase] ${values.test}/${values.role}: ${msg}\n`);
    const config = {
      role: values.role,
      signalingServer: values.server,
      room: values.room,
      messageCount: Number(values["message-count"]),
      messageSize: Number(values["message-size"]),
    };
    phase("modules compiled; instantiating guest");
    const instance = await newInstance();
    phase(`guest instantiated; running test (room ${values.room})`);
    const result = await instance.runner.runTest(values.test, config);
    phase("test finished");
    process.stdout.write(`${JSON.stringify(result)}\n`);
    return;
  }

  const owned = values.server ? null : await spawnSignaling(values["signaling-bin"]);
  const base = values.server ?? owned.base;
  process.stderr.write(`signaling server ready at ${base}\n`);

  try {
    const results = await runCorpus({
      base,
      newInstance,
      only: values.only,
      jobs: values.jobs ? Number(values.jobs) : defaultJobs(),
      log: (msg) => process.stderr.write(msg),
    });

    const report = { target: values.target, environment: values.environment, results };
    await mkdir(values.out, { recursive: true });
    const outPath = join(values.out, `${values.target}.json`);
    await writeFile(outPath, `${JSON.stringify(report, null, 2)}\n`);
    process.stderr.write(`wrote ${outPath}\n`);
  } finally {
    if (owned) await owned.shutdown();
  }
}

main().then(
  () => process.exit(0),
  (err) => {
    console.error("jco-node adapter failed:", err);
    process.exit(1);
  },
);
