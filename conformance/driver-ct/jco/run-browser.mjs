// The jco browser child of rtc-ct-driver: the same transpiled suite and
// the same host module (jco-impl/webrtc.js, RTCPeerConnection-backed in
// a real headless Chromium) for one selection and (for pair runs) one
// role. Emits component-test results JSONL on stdout; the driver folds
// and merges.
//
// The static server reverse-proxies `/rooms/*` and `/healthz` to the
// driver's signaling server so the page's mailbox fetches stay
// same-origin, and the page resolves its bare
// @polymorph/component-test-js specifiers through an import map onto
// the served facade files. jco's async ABI needs JSPI; Chrome ships it
// enabled from 137 onward.
import http from "node:http";
import { access, readdir, readFile } from "node:fs/promises";
import { dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

import { chromium } from "playwright-core";

import { findChrome } from "../../../scripts/chrome.mjs";

const JCO_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(JCO_DIR, "..", "..", "..");
const SHIM_BROWSER_DIR = join(
  JCO_DIR,
  "node_modules",
  "@bytecodealliance",
  "preview2-shim",
  "lib",
  "browser",
);
// The upstream runner core's files, wherever the install put them (the
// package exports resolve to js/viewer/ inside the installed tree).
const CT_JS_DIR = dirname(
  fileURLToPath(import.meta.resolve("@polymorph/component-test-js/harness")),
);

const IMPORT_MAP = JSON.stringify({
  imports: {
    "@polymorph/component-test-js/harness": "/ct/harness.mjs",
    "@polymorph/component-test-js/context": "/ct/context.js",
  },
});

const { values } = parseArgs({
  options: {
    generated: { type: "string", default: join(JCO_DIR, "generated") },
    name: { type: "string", default: "conformance-guest-ct" },
    target: { type: "string", default: "jco-browser" },
    select: { type: "string", default: "" },
  },
});

const MIME = {
  ".js": "text/javascript",
  ".mjs": "text/javascript",
  ".wasm": "application/wasm",
  ".html": "text/html",
};

/** Serve the transpiled suite, the harness modules (local glue + the
 * upstream runner core under /ct/), the host module, and the
 * preview2-shim browser build — strict allowlist, no dot segments —
 * and reverse-proxy the signaling mailbox so the page stays
 * same-origin. */
function startServer(wasmNames, signalingBase) {
  const server = http.createServer(async (req, res) => {
    const pathname = decodeURIComponent(req.url.split("?")[0]);
    if (pathname === "/healthz" || pathname.startsWith("/rooms/")) {
      await proxy(req, res, signalingBase);
      return;
    }
    if (pathname === "/") {
      res.setHeader("content-type", "text/html");
      res.end(
        "<!doctype html><meta charset=utf-8><title>conformance-ct jco browser leg</title>" +
          `<script type="importmap">${IMPORT_MAP}</script><body>`,
      );
      return;
    }
    if (pathname === "/favicon.ico") {
      res.statusCode = 204;
      res.end();
      return;
    }
    if (pathname === "/generated-manifest") {
      res.setHeader("content-type", "application/json");
      res.end(JSON.stringify(wasmNames));
      return;
    }
    const match =
      /^\/(generated|shim|ct)\/([A-Za-z0-9._-]+)$|^\/(webrtc\.js|harness\.mjs|signaling\.js)$/.exec(
        pathname,
      );
    if (!match || pathname.includes("..")) {
      res.statusCode = 404;
      res.end("not found");
      return;
    }
    const file = match[3]
      ? match[3] === "webrtc.js"
        ? join(REPO_ROOT, "jco-impl", "webrtc.js")
        : join(JCO_DIR, match[3])
      : match[1] === "shim"
        ? join(SHIM_BROWSER_DIR, match[2])
        : match[1] === "ct"
          ? join(CT_JS_DIR, match[2])
          : join(resolve(values.generated), match[2]);
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

/** The suite run performed inside the page (via page.evaluate). */
async function runInPage({ base, name, target, select, env, maxInboundBufferBytes }) {
  const [{ bindImports, runSuite }, connections, { Session }, { instantiate }, cli, clocks, io] =
    await Promise.all([
      import(`${base}/harness.mjs`),
      import(`${base}/webrtc.js`),
      import(`${base}/signaling.js`),
      import(`${base}/generated/${name}.js`),
      import(`${base}/shim/cli.js`),
      import(`${base}/shim/clocks.js`),
      import(`${base}/shim/io.js`),
    ]);

  connections.setMaxInboundBufferBytes(maxInboundBufferBytes);

  const listing = await (await fetch(`${base}/generated-manifest`)).json();
  const modules = new Map();
  const coreBytes = [];
  for (const file of listing) {
    const bytes = new Uint8Array(
      await (await fetch(`${base}/generated/${file}`)).arrayBuffer(),
    );
    coreBytes.push(bytes);
    modules.set(file, await WebAssembly.compile(bytes));
  }

  const imports = bindImports({ connections, mailbox: { Session }, env, cli, clocks, io });
  const newInstance = () => instantiate((file) => modules.get(file), imports);

  const lines = [];
  const summary = await runSuite({
    newInstance,
    coreBytes,
    target,
    suiteName: name.replaceAll("-", "_"),
    select,
    emit: (line) => lines.push(line),
    log: (msg) => console.log(msg.trimEnd()),
  });
  return { lines, summary };
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

  const executablePath = await findChrome();
  if (!executablePath) {
    throw new Error("no Chrome/Chromium binary found; set CHROME_PATH to a Chrome 137+ executable");
  }

  const wasmNames = (await readdir(generatedDir)).filter((n) => n.endsWith(".wasm"));
  const server = await startServer(wasmNames, signalingBase);
  const base = `http://127.0.0.1:${server.address().port}`;
  process.stderr.write(`page served from ${base} (signaling proxied to ${signalingBase})\n`);

  const maxInboundBufferBytes = 512 * 1024;
  const env = [["WEBRTC_MAX_INBOUND_BUFFER_BYTES", String(maxInboundBufferBytes)]];
  if (process.env.RTC_CT_ROLE) {
    env.push(["RTC_CT_ROLE", process.env.RTC_CT_ROLE]);
  }
  // The page's mailbox fetches go through this server's same-origin proxy.
  env.push(["RTC_CT_SIGNALING_URL", base]);
  if (process.env.RTC_CT_RUN_ID) {
    env.push(["RTC_CT_RUN_ID", process.env.RTC_CT_RUN_ID]);
  }

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

  let outcome;
  try {
    const context = await browser.newContext();
    const page = await context.newPage();
    page.on("console", (msg) => process.stderr.write(`[browser] ${msg.text()}\n`));
    page.on("pageerror", (err) => console.error(`[browser error] ${err.stack ?? err.message}`));
    await page.goto(`${base}/`);
    outcome = await page.evaluate(runInPage, {
      base,
      name: values.name,
      target: values.target,
      select: values.select,
      env,
      maxInboundBufferBytes,
    });
  } finally {
    await browser.close();
    server.close();
  }

  process.stdout.write(`${outcome.lines.join("\n")}\n`);
  process.exit(outcome.summary.failed === 0 && outcome.summary.total > 0 ? 0 : 1);
}

main().then(
  () => {},
  (err) => {
    console.error(err);
    process.exit(2);
  },
);
