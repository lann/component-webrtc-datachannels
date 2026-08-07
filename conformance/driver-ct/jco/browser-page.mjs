// The in-page run for the jco browser child: the SUT
// (jco-impl/webrtc.js) needs RTCPeerConnection, a Window-only API, so
// the suite runs in the page itself — Web Workers cannot host it,
// which is why this leg does not use the upstream page-runner/worker
// pair. Loaded by the driver-built page; the bare
// @polymorph/component-test-js specifiers in harness.mjs resolve
// through the page's import map onto the driver's self-mount.
import { bindImports, runSuite } from "/conformance/driver-ct/jco/harness.mjs";
import * as connections from "/jco-impl/webrtc.js";
import { Session } from "/conformance/driver-ct/jco/signaling.js";
import * as cli from "/conformance/driver-ct/jco/node_modules/@bytecodealliance/preview2-shim/lib/browser/cli.js";
import * as clocks from "/conformance/driver-ct/jco/node_modules/@bytecodealliance/preview2-shim/lib/browser/clocks.js";
import * as io from "/conformance/driver-ct/jco/node_modules/@bytecodealliance/preview2-shim/lib/browser/io.js";

const beat = (note) => {
  try {
    window.__progress(note)?.catch?.(() => {});
  } catch {
    // A closing page must not turn a heartbeat into an unhandled rejection.
  }
};

/** Run the configured selection to completion and report
 *  `{ lines, summary }` through the page driver. */
export async function run({ genBase, name, target, select, env, maxInboundBufferBytes, cores }) {
  try {
    connections.setMaxInboundBufferBytes(maxInboundBufferBytes);
    // The page's mailbox fetches go through the driver's same-origin
    // proxy.
    const resolvedEnv = [...env, ["RTC_CT_SIGNALING_URL", window.location.origin]];

    const modules = new Map();
    const coreBytes = [];
    for (const file of cores) {
      const res = await fetch(`${genBase}/${file}`);
      if (!res.ok) throw new Error(`fetching ${genBase}/${file}: ${res.status}`);
      const bytes = new Uint8Array(await res.arrayBuffer());
      coreBytes.push(bytes);
      modules.set(file, await WebAssembly.compile(bytes));
    }

    const { instantiate } = await import(`${genBase}/${name}.js`);
    const imports = bindImports({
      connections,
      mailbox: { Session },
      env: resolvedEnv,
      cli,
      clocks,
      io,
    });
    const newInstance = () => instantiate((file) => modules.get(file), imports);

    let rows = 0;
    const lines = [];
    const summary = await runSuite({
      newInstance,
      coreBytes,
      target,
      suiteName: name,
      select,
      emit: (line) => lines.push(line),
      log: (msg) => {
        rows += 1;
        if (rows % 10 === 0) beat(`row ${rows}: ${msg}`);
      },
    });
    window.__report({ lines, summary });
  } catch (err) {
    window.__report({ error: String(err?.stack ?? err) });
  }
}
