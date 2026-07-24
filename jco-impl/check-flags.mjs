// Verify the jco transpile flags in package.json against the WIT.
//
// The `--async-imports` / `--async-exports` strings must name exactly the
// functions the WIT declares `async`: a missing entry silently gets the sync
// ABI (a runtime trap when the guest calls it), and a stale entry survives
// renames unnoticed. This check parses the WIT (via `wasm-tools component wit
// --json`) and fails the transpile when either script's flag set diverges.
//
// Run automatically by the `transpile` / `transpile-remote` npm scripts.

import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HOST_DIR = dirname(fileURLToPath(import.meta.url));
// Both packages (lann:webrtc-datachannels via the deps symlink, plus
// demo:webrtc-echo) resolve from the echo-demo WIT directory.
const WIT_DIR = join(HOST_DIR, "..", "examples", "echo-demo", "wit");

/** Parse the WIT directory into wasm-tools' JSON representation. */
function loadWit() {
  const out = execFileSync("wasm-tools", ["component", "wit", WIT_DIR, "--json"], {
    encoding: "utf8",
  });
  return JSON.parse(out);
}

/** Whether a function's `kind` (string or single-key object) is async. */
function isAsync(kind) {
  const name = typeof kind === "string" ? kind : Object.keys(kind)[0];
  return name.startsWith("async");
}

/** The async function names of `pkg`'s interface `iface`, as `pkg/iface#fn` specs. */
function asyncSpecs(wit, pkg, iface) {
  const pkgEntry = wit.packages.find((p) => p.name === pkg);
  if (!pkgEntry) throw new Error(`package ${pkg} not found in ${WIT_DIR}`);
  const index = pkgEntry.interfaces[iface];
  if (index === undefined) throw new Error(`interface ${iface} not found in ${pkg}`);
  return Object.values(wit.interfaces[index].functions ?? {})
    .filter((fn) => isAsync(fn.kind))
    .map((fn) => `${pkg.replace(/@/, `/${iface}@`)}#${fn.name}`);
}

/** Every `--async-imports '<spec>'` / `--async-exports '<spec>'` in a script. */
function flagSpecs(script, flag) {
  return [...script.matchAll(new RegExp(`${flag} '([^']+)'`, "g"))].map((m) => m[1]);
}

/** Compare two spec sets, returning human-readable divergences. */
function diff(kind, script, expected, actual) {
  const want = new Set(expected);
  const have = new Set(actual);
  const problems = [];
  for (const spec of expected) {
    if (!have.has(spec)) problems.push(`${script}: missing ${kind} '${spec}' (would get the sync ABI)`);
  }
  for (const spec of actual) {
    if (!want.has(spec)) problems.push(`${script}: stale ${kind} '${spec}' (not an async function in the WIT)`);
  }
  return problems;
}

const wit = loadWit();
const connections = asyncSpecs(wit, "lann:webrtc-datachannels@0.1.0", "connections");
const rendezvous = asyncSpecs(wit, "demo:webrtc-echo@0.1.0", "rendezvous");
const demoRun = asyncSpecs(wit, "demo:webrtc-echo@0.1.0", "demo");
const remoteRun = asyncSpecs(wit, "demo:webrtc-echo@0.1.0", "remote");

const scripts = JSON.parse(readFileSync(join(HOST_DIR, "package.json"), "utf8")).scripts;
const problems = [
  ...diff("--async-imports", "transpile", connections, flagSpecs(scripts.transpile, "--async-imports")),
  ...diff("--async-exports", "transpile", demoRun, flagSpecs(scripts.transpile, "--async-exports")),
  ...diff(
    "--async-imports",
    "transpile-remote",
    [...connections, ...rendezvous],
    flagSpecs(scripts["transpile-remote"], "--async-imports"),
  ),
  ...diff(
    "--async-exports",
    "transpile-remote",
    remoteRun,
    flagSpecs(scripts["transpile-remote"], "--async-exports"),
  ),
];

if (problems.length) {
  console.error("check-flags: transpile flags diverge from the WIT:");
  for (const problem of problems) console.error(`  ${problem}`);
  process.exit(1);
}
console.error("check-flags: transpile flags match the WIT");
