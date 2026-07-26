// Driver for one peer of the two-process echo demo under the Node host: the
// transpiled `echo-remote` component connects to a genuinely separate peer
// instance through `rendezvous.js` (fetch against the local signaling server)
// and `webrtc.js` (node-datachannel).
//
// Run two of these — an offerer and an answerer — against the same room
// (or run both at once with `just examples::demo-node-remote`):
//
//   cargo run -p conformance-signalingd &   # or any server speaking the protocol
//   npm run start-remote -- --role answerer --server http://127.0.0.1:8080 --room demo &
//   npm run start-remote -- --role offerer  --server http://127.0.0.1:8080 --room demo
import { parseArgs } from "node:util";

import { remote } from "../generated-remote/echo-remote.js";
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

const { values } = parseArgs({
  options: {
    role: { type: "string" },
    server: { type: "string" },
    room: { type: "string" },
    count: { type: "string", default: "100" },
    size: { type: "string", default: "1024" },
  },
});

async function main() {
  const { role, server, room } = values;
  if (!role || !server || !room) {
    throw new Error("usage: run-remote.mjs --role <offerer|answerer> --server <url> --room <id>");
  }
  const messageCount = Number(values.count);
  const messageSize = Number(values.size);

  const stats = await remote.run({
    server,
    room,
    role,
    messageCount,
    messageSize,
  });

  const bytes = Number(stats.bytesEchoed);
  console.log(
    `echo-remote (${role}): sent ${stats.messagesSent} received ${stats.messagesReceived} bytes ${bytes}`,
  );
  if (role === "offerer") {
    const expectedBytes = messageCount * messageSize;
    if (stats.messagesReceived !== messageCount || bytes !== expectedBytes) {
      throw new Error(
        `expected ${messageCount} messages / ${expectedBytes} bytes, got ${stats.messagesReceived} / ${bytes}`,
      );
    }
  }
  console.log(`OK: ${role} finished.`);
}

main().then(
  () => process.exit(0),
  (err) => {
    console.error("echo-remote failed:", err);
    process.exit(1);
  },
);
