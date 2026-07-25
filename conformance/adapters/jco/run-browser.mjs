// The jco browser conformance adapter: runs the same shared conformance guest
// and the same host modules (`jco-impl/webrtc.js` + the adapter's
// `signaling.js`) inside a real, headless
// Chromium — the environment the "browser-first" host actually targets — and
// emits the adapter result document the conformance runner consumes
// (`conformance/results/jco-browser.json`). It is the browser counterpart of the
// Node adapter and shares the corpus orchestration in `driver.js`.
//
// ## Why this needs more than "open a page and press go"
//
// Two obstacles make a naive headless run fail; both are handled below (see the
// `jco-impl/test/browser.mjs` header for the full background).
//
// 1. **JSPI.** jco's async ABI needs JavaScript Promise Integration. Chrome
//    ships JSPI enabled by default from Chrome 137 onward, so a recent Chrome
//    "just works" with no flags — no Node JSPI flag is involved because the
//    guest runs in the browser, not in this driver process.
//
// 2. **ICE candidates are filtered away in headless Chrome** unless the page has
//    a WebRTC-relevant permission. The fix (all applied below): serve the page
//    from `http://127.0.0.1:<port>` (a localhost secure context), launch Chrome
//    with fake media devices and grant camera/microphone, and call
//    `getUserMedia({ audio: true })` before opening any `RTCPeerConnection`.
//
// The page fetches the signaling mailbox from this same origin: the static
// server reverse-proxies `/rooms/*` and `/healthz` to `conformance-signalingd`,
// so the browser makes only same-origin requests and no CORS handling is needed.
import { spawn } from "node:child_process";
import http from "node:http";
import { availableParallelism } from "node:os";
import { access, mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

import { chromium } from "playwright-core";

const ADAPTER_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(ADAPTER_DIR, "..", "..", "..");

// The default test concurrency: 2x the cores available to this process,
// clamped to [2, 8]. The corpus is I/O-bound so the optimum exceeds the core
// count (measured at 3x cores on the lighter Rust adapters), but each test
// here runs inside a heavyweight JS runtime whose flake history is
// load-induced timeouts, so the multiplier stays conservative: 4 jobs on
// 2-core CI runners (the load proven stable there), scaling up on larger
// machines.
function defaultJobs() {
  return Math.min(8, Math.max(2, 2 * availableParallelism()));
}

const { values } = parseArgs({
  options: {
    generated: { type: "string", default: join(ADAPTER_DIR, "generated") },
    out: { type: "string", default: join(REPO_ROOT, "conformance", "results") },
    target: { type: "string", default: "jco-browser" },
    environment: { type: "string", default: "loopback" },
    "signaling-bin": {
      type: "string",
      default: join(REPO_ROOT, "target", "debug", "conformance-signalingd"),
    },
    only: { type: "string", multiple: true, default: [] },
    // How many tests to run concurrently (each test's peers use their own
    // signaling room, so tests are independent); defaults to defaultJobs().
    jobs: { type: "string" },
    // Base URL of an already-running signaling server. When omitted, this
    // adapter spawns its own `conformance-signalingd`.
    server: { type: "string" },
    // Single-instance interop mode: run exactly one guest instance for one
    // `--test`/`--role`/`--room` against an already-running `--server`,
    // printing the raw `test-result` (`{ "tag": ... }`) as JSON to stdout.
    // Used by the cross-runtime interop orchestrator (conformance-interop) to
    // drive the jco-browser half of a wasmtime<->jco-browser pair.
    interop: { type: "boolean", default: false },
    role: { type: "string", default: "offerer" },
    room: { type: "string", default: "interop" },
    test: { type: "string", default: "interop-handshake" },
    "message-count": { type: "string", default: "16" },
    "message-size": { type: "string", default: "512" },
  },
});

// Candidate locations for a Chrome/Chromium binary. CI can override with
// CHROME_PATH (e.g. the output of the setup-chrome action).
const CHROME_CANDIDATES = [
  process.env.CHROME_PATH,
  process.env.CHROME_BIN,
  process.env.PUPPETEER_EXECUTABLE_PATH,
  "/usr/bin/google-chrome",
  "/usr/bin/google-chrome-stable",
  "/usr/bin/chromium",
  "/usr/bin/chromium-browser",
];

const MIME = {
  ".js": "text/javascript",
  ".mjs": "text/javascript",
  ".wasm": "application/wasm",
  ".html": "text/html",
};

async function firstExisting(paths) {
  for (const p of paths) {
    if (!p) continue;
    try {
      await access(p);
      return p;
    } catch {
      // keep looking
    }
  }
  return undefined;
}

/**
 * Start a static file + signaling-proxy server. Serves the adapter's host
 * modules and the transpiled guest, and reverse-proxies `/rooms/*` and
 * `/healthz` to the signaling server so the page makes only same-origin
 * requests.
 */
function startServer(signalingBase) {
  const server = http.createServer(async (req, res) => {
    const pathname = decodeURIComponent(req.url.split("?")[0]);

    // Reverse-proxy the signaling mailbox to `conformance-signalingd`.
    if (pathname === "/healthz" || pathname.startsWith("/rooms/")) {
      await proxy(req, res, signalingBase);
      return;
    }

    if (pathname === "/") {
      res.setHeader("content-type", "text/html");
      res.end("<!doctype html><meta charset=utf-8><title>conformance jco browser adapter</title><body>");
      return;
    }
    if (pathname === "/favicon.ico") {
      res.statusCode = 204;
      res.end();
      return;
    }

    // Strict allowlist: the transpiled bundle under /generated/ and the host
    // modules. Each path is a single, dot-segment-free file name, which
    // scopes the server and rules out path traversal. `/webrtc.js` is served
    // from jco-impl: the connections host is one module shared with the demo
    // hosts (in the browser its `@roamhq/wrtc` fallback import is never
    // reached, so serving the file alone is enough).
    const match =
      /^\/(generated)\/([A-Za-z0-9._-]+)$|^\/(webrtc\.js|signaling\.js|driver\.js)$/.exec(pathname);
    if (!match || pathname.includes("..")) {
      res.statusCode = 404;
      res.end("not found");
      return;
    }
    const file =
      match[3] === "webrtc.js"
        ? join(REPO_ROOT, "jco-impl", "webrtc.js")
        : match[3]
          ? join(ADAPTER_DIR, match[3])
          : join(values.generated, match[2]);
    try {
      const body = await readFile(file);
      res.setHeader("content-type", MIME[extname(file)] ?? "application/octet-stream");
      res.end(body);
    } catch {
      res.statusCode = 404;
      res.end("not found");
    }
  });
  return new Promise((res) => server.listen(0, "127.0.0.1", () => res(server)));
}

/** Forward one request to the signaling server and stream its response back. */
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
  // Forward every upstream header (the protocol's `x-seq`/`x-done` included),
  // skipping the hop-by-hop and length framing Node manages itself.
  for (const [name, value] of upstream.headers) {
    if (["connection", "keep-alive", "transfer-encoding", "content-length"].includes(name)) {
      continue;
    }
    res.setHeader(name, value);
  }
  res.end(Buffer.from(await upstream.arrayBuffer()));
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
      const m = /listening on (http:\/\/\S+)/.exec(buffer);
      if (m) {
        child.stdout.off("data", onData);
        resolveUrl(m[1].trim());
      }
    };
    child.stdout.on("data", onData);
    child.on("exit", (code) => rejectUrl(new Error(`signaling server exited early (${code})`)));
    setTimeout(() => rejectUrl(new Error("signaling server did not report a URL in time")), 10_000);
  });
  return {
    base,
    async shutdown() {
      child.kill("SIGTERM");
    },
  };
}

/**
 * The corpus run performed inside the browser page. Serialized and evaluated via
 * `page.evaluate`; `base` is this adapter's same-origin static/proxy server.
 */
async function runInPage({ base, only, jobs }) {
  // Unlock non-filtered host ICE candidates (see file header).
  const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
  stream.getTracks().forEach((t) => t.stop());

  const [{ runCorpus, MAX_INBOUND_BUFFER_BYTES }, connections, { Session }, { instantiate }] =
    await Promise.all([
    import(`${base}/driver.js`),
    import(`${base}/webrtc.js`),
    import(`${base}/signaling.js`),
    import(`${base}/generated/conformance-guest.js`),
  ]);

  // Shrink the host's inbound-buffer bound so the `receive-buffer-overflow`
  // probe overflows it with a small flood (channels capture the bound at
  // creation).
  connections.setMaxInboundBufferBytes(MAX_INBOUND_BUFFER_BYTES);

  const names = [
    "conformance-guest.core.wasm",
    "conformance-guest.core2.wasm",
    "conformance-guest.core3.wasm",
  ];
  const modules = new Map();
  for (const name of names) {
    modules.set(name, await WebAssembly.compileStreaming(fetch(`${base}/generated/${name}`)));
  }

  const newInstance = () =>
    instantiate((name) => modules.get(name), {
      "conformance:signaling/mailbox": { Session },
      "lann:webrtc-datachannels/connections": connections,
    });

  return runCorpus({
    base,
    newInstance,
    only,
    jobs,
    log: (msg) => console.log(msg.trimEnd()),
  });
}

/**
 * The single interop test run performed inside the browser page. Serialized and
 * evaluated via `page.evaluate`; the signaling server is reached through this
 * adapter's same-origin proxy at `base`.
 */
async function runInteropInPage({ base, testId, config }) {
  // Unlock non-filtered host ICE candidates (see file header).
  const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
  stream.getTracks().forEach((t) => t.stop());

  const [{ MAX_INBOUND_BUFFER_BYTES }, connections, { Session }, { instantiate }] =
    await Promise.all([
      import(`${base}/driver.js`),
      import(`${base}/webrtc.js`),
      import(`${base}/signaling.js`),
      import(`${base}/generated/conformance-guest.js`),
    ]);
  connections.setMaxInboundBufferBytes(MAX_INBOUND_BUFFER_BYTES);

  const names = [
    "conformance-guest.core.wasm",
    "conformance-guest.core2.wasm",
    "conformance-guest.core3.wasm",
  ];
  const modules = new Map();
  for (const name of names) {
    modules.set(name, await WebAssembly.compileStreaming(fetch(`${base}/generated/${name}`)));
  }
  // Forwarded to the adapter's stderr via the page console hook, so the
  // orchestrator's live stderr stream carries the in-page phase progression.
  console.log(`[phase] ${testId}: modules compiled; instantiating guest`);

  const instance = await instantiate((name) => modules.get(name), {
    "conformance:signaling/mailbox": { Session },
    "lann:webrtc-datachannels/connections": connections,
  });
  console.log(`[phase] ${testId}: guest instantiated; running test`);
  const result = await instance.runner.runTest(testId, config);
  console.log(`[phase] ${testId}: test finished`);
  return result;
}

async function main() {
  try {
    await access(join(values.generated, "conformance-guest.js"));
  } catch {
    throw new Error(
      `missing transpiled guest in ${values.generated}; run "npm run transpile" first`,
    );
  }

  const executablePath = await firstExisting(CHROME_CANDIDATES);
  if (!executablePath) {
    throw new Error("no Chrome/Chromium binary found; set CHROME_PATH to a Chrome 137+ executable");
  }

  if (values.interop && !values.server) {
    throw new Error("--interop requires --server");
  }

  // Use the given already-running signaling server, or spawn our own. Either
  // way the page reaches it through this adapter's same-origin proxy.
  const owned = values.server ? null : await spawnSignaling(values["signaling-bin"]);
  const signalingBase = values.server ?? owned.base;
  const server = await startServer(signalingBase);
  const base = `http://127.0.0.1:${server.address().port}`;
  process.stderr.write(`signaling server at ${signalingBase} (proxied via ${base})\n`);

  const browser = await chromium.launch({
    executablePath,
    headless: true,
    args: [
      "--no-sandbox",
      "--disable-dev-shm-usage",
      "--use-fake-device-for-media-stream",
      "--use-fake-ui-for-media-stream",
    ],
  });
  if (values.interop) {
    process.stderr.write(`[phase] ${values.test}/${values.role}: browser launched\n`);
  }

  let results;
  try {
    const context = await browser.newContext();
    await context.grantPermissions(["microphone", "camera"], { origin: base });
    const page = await context.newPage();
    page.on("console", (msg) => process.stderr.write(`[browser] ${msg.text()}\n`));
    page.on("pageerror", (err) => console.error(`[browser error] ${err.stack ?? err.message}`));
    await page.goto(`${base}/`);

    // Single-instance interop mode: one guest instance, one test/role/room;
    // emit just the raw result to stdout.
    if (values.interop) {
      const config = {
        role: values.role,
        signalingServer: base,
        room: values.room,
        messageCount: Number(values["message-count"]),
        messageSize: Number(values["message-size"]),
      };
      const result = await page.evaluate(runInteropInPage, {
        base,
        testId: values.test,
        config,
      });
      process.stdout.write(`${JSON.stringify(result)}\n`);
      return;
    }

    results = await page.evaluate(runInPage, { base, only: values.only, jobs: values.jobs ? Number(values.jobs) : defaultJobs() });
  } finally {
    await browser.close();
    server.close();
    if (owned) await owned.shutdown();
  }

  const report = { target: values.target, environment: values.environment, results };
  await mkdir(values.out, { recursive: true });
  const outPath = join(values.out, `${values.target}.json`);
  await writeFile(outPath, `${JSON.stringify(report, null, 2)}\n`);
  process.stderr.write(`wrote ${outPath}\n`);
}

main().then(
  () => process.exit(0),
  (err) => {
    console.error("jco-browser adapter failed:", err);
    process.exit(1);
  },
);
