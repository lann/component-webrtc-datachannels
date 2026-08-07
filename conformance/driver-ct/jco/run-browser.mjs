// The jco browser child of rtc-ct-driver: the same transpiled suite and
// the same host module (jco-impl/webrtc.js, RTCPeerConnection-backed in
// a real headless Chromium) for one selection and (for pair runs) one
// role. Emits component-test results JSONL on stdout; the driver folds
// and merges. The server, launch, stall watchdog, and Chrome ladder
// live in the upstream browser driver; the run itself happens in the
// page (browser-page.mjs — RTCPeerConnection is Window-only, so the
// upstream Web Worker pool cannot host this SUT). This file is the
// frame: the signaling reverse-proxy, the role environment, target
// configuration, and the stdout contract.
//
// The proxy keeps the page's mailbox fetches same-origin (`/rooms/*`
// and `/healthz` forward to the driver's signaling server). jco's
// async ABI needs JSPI; Chrome ships it enabled from 137 onward.
import { access, readdir } from "node:fs/promises";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

import {
  componentTestImportMap,
  findChrome,
  runPageHarness,
} from "@polymorph/component-test-js/browser-driver";

const JCO_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(JCO_DIR, "..", "..", "..");
const BASE = "/conformance/driver-ct/jco";
const MAX_INBOUND_BUFFER_BYTES = 512 * 1024;
// The single-attempt wall bound per case, matching the wasmtime leg.
const CASE_TIMEOUT_MS = 90_000;
// The pool heartbeats per suite and per 25 rows; pair cases rendezvous
// through the signaling server, so quiet time is bounded by the peer's
// slowest handshakes.
const STALL_TIMEOUT_MS = 120_000;

const { values } = parseArgs({
  options: {
    generated: { type: "string", default: join(JCO_DIR, "generated") },
    name: { type: "string", default: "conformance-guest-ct" },
    target: { type: "string", default: "jco-browser" },
    select: { type: "string", default: "" },
  },
});

/** Reverse-proxy one mailbox request to the driver's signaling server. */
async function proxy(req, res, signalingBase) {
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  const body = Buffer.concat(chunks);
  let upstream;
  try {
    upstream = await fetch(`${signalingBase}${req.url}`, {
      method: req.method,
      headers: { "content-type": req.headers["content-type"] ?? "application/octet-stream" },
      body: req.method === "GET" || req.method === "HEAD" ? undefined : body,
    });
  } catch (err) {
    res.statusCode = 502;
    res.end(`proxy error: ${err}`);
    return;
  }
  res.statusCode = upstream.status;
  for (const [name, value] of upstream.headers) {
    if (["connection", "keep-alive", "transfer-encoding", "content-length"].includes(name)) {
      continue;
    }
    res.setHeader(name, value);
  }
  res.end(Buffer.from(await upstream.arrayBuffer()));
}

async function main() {
  const generatedDir = resolve(values.generated);
  try {
    await access(join(generatedDir, `${values.name}.js`));
  } catch {
    throw new Error(`missing transpiled suite in ${generatedDir}; run "npm run transpile" first`);
  }
  const signalingBase = process.env.RTC_CT_SIGNALING_URL;
  if (!signalingBase) {
    throw new Error("RTC_CT_SIGNALING_URL is not set (this child is spawned by rtc-ct-driver)");
  }

  const env = [["WEBRTC_MAX_INBOUND_BUFFER_BYTES", String(MAX_INBOUND_BUFFER_BYTES)]];
  if (process.env.RTC_CT_ROLE) env.push(["RTC_CT_ROLE", process.env.RTC_CT_ROLE]);
  if (process.env.RTC_CT_RUN_ID) env.push(["RTC_CT_RUN_ID", process.env.RTC_CT_RUN_ID]);

  const cores = (await readdir(generatedDir))
    .filter((n) => n.startsWith(`${values.name}.core`) && n.endsWith(".wasm"))
    .sort();
  // The generated tree's server path: the interop children point
  // --generated at the pair transpile, and the repository-root server
  // has no aliases.
  const genBase = `/${relative(REPO_ROOT, generatedDir).split("\\").join("/")}`;

  // The SUT needs RTCPeerConnection — a Window-only API — so the run
  // happens in the page (browser-page.mjs), not in the upstream
  // page-runner's Web Workers; the import map still resolves the
  // harness core's bare specifiers onto the driver's self-mount.
  const config = {
    genBase,
    name: values.name,
    target: values.target,
    select: values.select,
    env,
    maxInboundBufferBytes: MAX_INBOUND_BUFFER_BYTES,
    cores,
  };
  const importMap = JSON.stringify({ imports: componentTestImportMap() });
  const html = `<!doctype html>
<link rel="icon" href="data:,">
<title>conformance-ct jco browser leg</title>
<script type="importmap">${importMap}</script>
<script type="module">
import { run } from "${BASE}/browser-page.mjs";
await run(${JSON.stringify(config)});
</script>`;

  const playwright = await import("playwright-core");
  const outcome = await runPageHarness({
    playwright,
    engine: "chromium",
    executablePath: await findChrome(),
    repoRoot: REPO_ROOT,
    html,
    routes: async (req, res) => {
      const pathname = decodeURIComponent(req.url.split("?")[0]);
      if (pathname === "/healthz" || pathname.startsWith("/rooms/")) {
        await proxy(req, res, signalingBase);
        return true;
      }
      return false;
    },
    stallTimeoutMs: STALL_TIMEOUT_MS,
    launchArgs: [
      "--no-sandbox",
      "--disable-dev-shm-usage",
      "--use-fake-device-for-media-stream",
      "--use-fake-ui-for-media-stream",
    ],
  });

  process.stdout.write(`${outcome.lines.join("\n")}\n`);
  const { summary } = outcome;
  process.exit(summary.failed === 0 && summary.total > 0 ? 0 : 1);
}

main().then(
  () => {},
  (err) => {
    console.error(err);
    process.exit(2);
  },
);
