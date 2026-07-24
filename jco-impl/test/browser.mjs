// Headless-browser test for the browser-first host.
//
// `src/run.mjs` exercises the host under Node (backed by `@roamhq/wrtc`). This
// test instead runs the *same* transpiled component and the *same* `webrtc.js`
// host module inside a real, headless Chromium, which is the environment the
// "browser-first" host actually targets. It is the browser counterpart to the
// Wasmtime integration test and is what runs in CI.
//
// ## Why this needs more than "open a page and press go"
//
// Two obstacles make a naive headless run fail; both are handled below.
//
// 1. **JSPI.** The component is transpiled with `--async-mode jspi`, so it needs
//    JavaScript Promise Integration. Chrome ships JSPI enabled by default from
//    Chrome 137 onward, so a recent Chrome/Chromium "just works" with no flags.
//
// 2. **ICE candidates are filtered away in headless Chrome.** This is the real
//    trap. Chrome's `FilteringNetworkManager` only exposes host ICE candidates
//    to a page once that page has been granted a WebRTC-relevant permission
//    (camera/microphone). Without it, candidates are gathered internally and
//    then *discarded* ("Discarding candidate because it doesn't match filter"),
//    so the component's in-component loopback handshake never completes and the data
//    channel never opens. mDNS is a red herring here — the candidates are
//    dropped regardless of the mDNS setting.
//
//    The fix has three parts, all applied below:
//      - serve the page from `http://127.0.0.1:<port>` so it is a secure
//        context (localhost) where `navigator.mediaDevices` is available;
//      - launch Chrome with fake media devices
//        (`--use-fake-device-for-media-stream --use-fake-ui-for-media-stream`)
//        and grant the microphone/camera permission; and
//      - call `getUserMedia({ audio: true })` in the page before opening any
//        `RTCPeerConnection`, which flips the network-permission state to
//        "granted" and lets real host candidates (raw loopback/LAN IPs, no
//        mDNS) flow. Only then is the component run.
//
// Run with:  just examples::test-browser   (or: npm run transpile && npm run
// test:browser after `just examples::build-component`)
import { chromium } from "playwright-core";
import http from "node:http";
import { access, readFile } from "node:fs/promises";
import { dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HOST_DIR = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const COMPONENT = join(HOST_DIR, "generated", "echo-demo.js");

const MESSAGE_COUNT = Number(process.env.MESSAGE_COUNT ?? 256);
const MESSAGE_SIZE = Number(process.env.MESSAGE_SIZE ?? 1024);

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

const MIME = {
  ".js": "text/javascript",
  ".mjs": "text/javascript",
  ".wasm": "application/wasm",
  ".html": "text/html",
};

/** Serve the host directory so the transpiled ES module + core wasm can be fetched. */
function startServer() {
  const server = http.createServer(async (req, res) => {
    const pathname = decodeURIComponent(req.url.split("?")[0]);
    if (pathname === "/") {
      res.setHeader("content-type", "text/html");
      res.end("<!doctype html><meta charset=utf-8><title>echo-demo browser host</title><body>");
      return;
    }
    if (pathname === "/favicon.ico") {
      res.statusCode = 204;
      res.end();
      return;
    }
    // Strict allowlist: only the transpiled bundle under /generated/ and the
    // browser host module are served, and each request path must be a single
    // file name. The regex scopes the server to what the test needs; the
    // explicit ".." check below is what rules out path traversal (the regex's
    // character class alone would match "..").
    const match = /^\/(generated)\/([A-Za-z0-9._-]+)$|^\/(webrtc\.js)$/.exec(pathname);
    if (!match || pathname.includes("..")) {
      res.statusCode = 404;
      res.end("not found");
      return;
    }
    const file = match[3]
      ? join(HOST_DIR, "webrtc.js")
      : join(HOST_DIR, "generated", match[2]);
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

async function main() {
  try {
    await access(COMPONENT);
  } catch {
    throw new Error(
      `missing ${COMPONENT}; run "just examples::transpile" first`,
    );
  }

  const executablePath = await firstExisting(CHROME_CANDIDATES);
  if (!executablePath) {
    throw new Error(
      "no Chrome/Chromium binary found; set CHROME_PATH to a Chrome 137+ executable",
    );
  }

  const server = await startServer();
  const base = `http://127.0.0.1:${server.address().port}`;

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

  try {
    const context = await browser.newContext();
    await context.grantPermissions(["microphone", "camera"], { origin: base });
    const page = await context.newPage();
    page.on("console", (msg) => console.log(`[browser] ${msg.text()}`));
    page.on("pageerror", (err) => console.error(`[browser error] ${err.stack ?? err.message}`));
    await page.goto(`${base}/`);

    const started = performance.now();
    const stats = await page.evaluate(
      async ({ base, messageCount, messageSize }) => {
        // Unlock non-filtered host ICE candidates (see file header).
        const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
        stream.getTracks().forEach((t) => t.stop());
        const { demo } = await import(`${base}/generated/echo-demo.js`);
        const result = await demo.run({ messageCount, messageSize });
        return {
          messagesSent: result.messagesSent,
          messagesReceived: result.messagesReceived,
          bytesEchoed: Number(result.bytesEchoed),
        };
      },
      { base, messageCount: MESSAGE_COUNT, messageSize: MESSAGE_SIZE },
    );
    const elapsed = performance.now() - started;

    const mibps = stats.bytesEchoed / (1024 * 1024) / (elapsed / 1000);
    console.log("echo-demo (headless Chromium / browser host) result:");
    console.log(`  chrome:            ${executablePath}`);
    console.log(`  messages sent:     ${stats.messagesSent}`);
    console.log(`  messages received: ${stats.messagesReceived}`);
    console.log(`  bytes echoed:      ${stats.bytesEchoed}`);
    console.log(`  elapsed:           ${elapsed.toFixed(1)} ms  (~${mibps.toFixed(1)} MiB/s round-trip)`);

    const expectedBytes = MESSAGE_COUNT * MESSAGE_SIZE;
    if (stats.messagesSent !== MESSAGE_COUNT) {
      throw new Error(`expected ${MESSAGE_COUNT} sent, got ${stats.messagesSent}`);
    }
    if (stats.messagesReceived !== MESSAGE_COUNT) {
      throw new Error(`expected ${MESSAGE_COUNT} received, got ${stats.messagesReceived}`);
    }
    if (stats.bytesEchoed !== expectedBytes) {
      throw new Error(`expected ${expectedBytes} bytes echoed, got ${stats.bytesEchoed}`);
    }
    console.log("\nOK: every message round-tripped through a WebRTC data channel in the browser.");
  } finally {
    await browser.close();
    server.close();
  }
}

main().then(
  () => process.exit(0),
  (err) => {
    console.error("browser echo-demo failed:", err);
    process.exit(1);
  },
);
