// The jco Node child of rtc-ct-driver: one suite instance's stream — a
// selection and (for pair runs) a role — against the browser-first host
// (jco-impl/webrtc.js, backed by node-datachannel) and the fetch
// mailbox (signaling.js). Emits component-test results JSONL on stdout;
// the driver folds and merges.
//
// Child contract (set by the driver): RTC_CT_SIGNALING_URL and
// RTC_CT_RUN_ID always; RTC_CT_ROLE on pair instances; `--select`
// carries the case-name prefix.
//
// jco's async ABI needs JSPI: Node 24+ with --experimental-wasm-jspi
// (the driver supplies it when spawning).
import { access, readFile, readdir } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

import { cli, clocks, io } from "@bytecodealliance/preview2-shim";

import { bindImports, runSuite } from "./harness.mjs";
import * as connections from "../../../jco-impl/webrtc.js";
import { Session } from "./signaling.js";

// Shrink the host's inbound-buffer bound so the receive-buffer-overflow
// probe's 1 MiB flood overflows it (channels capture the bound at
// creation, so configuring the module once covers every instance).
const MAX_INBOUND_BUFFER_BYTES = 512 * 1024;
connections.setMaxInboundBufferBytes(MAX_INBOUND_BUFFER_BYTES);

const JCO_DIR = dirname(fileURLToPath(import.meta.url));

const { values } = parseArgs({
  options: {
    generated: { type: "string", default: join(JCO_DIR, "generated") },
    name: { type: "string", default: "conformance-guest-ct" },
    target: { type: "string", default: "jco-node" },
    select: { type: "string", default: "" },
  },
});

async function loadCoreModules(generatedDir) {
  const modules = new Map();
  const coreBytes = [];
  for (const name of await readdir(generatedDir)) {
    if (name.endsWith(".wasm")) {
      const bytes = new Uint8Array(await readFile(join(generatedDir, name)));
      coreBytes.push(bytes);
      modules.set(name, await WebAssembly.compile(bytes));
    }
  }
  return { modules, coreBytes };
}

async function main() {
  const generatedDir = resolve(values.generated);
  try {
    await access(join(generatedDir, `${values.name}.js`));
  } catch {
    throw new Error(`missing transpiled suite in ${generatedDir}; run "npm run transpile" first`);
  }
  const { instantiate } = await import(join(generatedDir, `${values.name}.js`));
  const { modules, coreBytes } = await loadCoreModules(generatedDir);

  const env = [["WEBRTC_MAX_INBOUND_BUFFER_BYTES", String(MAX_INBOUND_BUFFER_BYTES)]];
  for (const name of ["RTC_CT_ROLE", "RTC_CT_SIGNALING_URL", "RTC_CT_RUN_ID"]) {
    if (process.env[name]) {
      env.push([name, process.env[name]]);
    }
  }

  const imports = bindImports({ connections, mailbox: { Session }, env, cli, clocks, io });
  const newInstance = () => instantiate((name) => modules.get(name), imports);

  const summary = await runSuite({
    newInstance,
    coreBytes,
    target: values.target,
    suiteName: values.name.replaceAll("-", "_"),
    select: values.select,
    emit: (line) => process.stdout.write(`${line}\n`),
    log: (msg) => process.stderr.write(`${msg}\n`),
  });
  process.exit(summary.failed === 0 && summary.total > 0 ? 0 : 1);
}

main().then(
  () => {},
  (err) => {
    console.error(err);
    process.exit(2);
  },
);
