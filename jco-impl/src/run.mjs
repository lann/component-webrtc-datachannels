// Driver for the Node host: transpile output is imported and the component's
// exported async `run` is invoked, then the round-trip stats are asserted.
//
// Run with:  just examples::demo-node   (or: npm run transpile && npm start
// after `just examples::build-component`)
import { demo } from "../generated/echo-demo.js";
import { setMaxInboundBufferBytes } from "../webrtc.js";

// Honor the conventional inbound-buffer knob: the host module itself reads no
// environment, so this Node host reads the variable, fails loud on a
// malformed value, and applies it through the module's configure hook.
const rawBufferBound = process.env.WEBRTC_MAX_INBOUND_BUFFER_BYTES;
if (rawBufferBound !== undefined && rawBufferBound !== "") {
  const bytes = Number(rawBufferBound);
  if (!(bytes > 0)) {
    throw new Error(
      `invalid WEBRTC_MAX_INBOUND_BUFFER_BYTES ${JSON.stringify(rawBufferBound)}: expected a positive byte count`,
    );
  }
  setMaxInboundBufferBytes(bytes);
}

const MESSAGE_COUNT = 1000;
const MESSAGE_SIZE = 4096;

async function main() {
  const started = performance.now();
  const stats = await demo.run({
    messageCount: MESSAGE_COUNT,
    messageSize: MESSAGE_SIZE,
  });
  const elapsed = performance.now() - started;

  const bytes = Number(stats.bytesEchoed);
  const mibps = bytes / (1024 * 1024) / (elapsed / 1000);
  console.log("echo-demo (Node / node-datachannel host) result:");
  console.log(`  messages sent:     ${stats.messagesSent}`);
  console.log(`  messages received: ${stats.messagesReceived}`);
  console.log(`  bytes echoed:      ${bytes}`);
  console.log(`  elapsed:           ${elapsed.toFixed(1)} ms  (~${mibps.toFixed(1)} MiB/s round-trip)`);

  const expectedBytes = MESSAGE_COUNT * MESSAGE_SIZE;
  if (stats.messagesSent !== MESSAGE_COUNT) {
    throw new Error(`expected ${MESSAGE_COUNT} sent, got ${stats.messagesSent}`);
  }
  if (stats.messagesReceived !== MESSAGE_COUNT) {
    throw new Error(`expected ${MESSAGE_COUNT} received, got ${stats.messagesReceived}`);
  }
  if (bytes !== expectedBytes) {
    throw new Error(`expected ${expectedBytes} bytes echoed, got ${bytes}`);
  }
  console.log("\nOK: every message round-tripped through the WebRTC data channel.");
}

main().then(
  () => process.exit(0),
  (err) => {
    console.error("echo-demo failed:", err);
    process.exit(1);
  },
);
