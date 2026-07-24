#!/usr/bin/env node
// Shadow-spike stage 2: two RTCPeerConnections in one Node process, connected
// over the host's own interfaces with direct (in-memory) signaling, exchanging
// one message each way. Exercises V8, the wrtc native addon, and libwebrtc's
// ICE/DTLS/SCTP stack without any external network dependency — the smallest
// meaningful "does libwebrtc work in this environment" check.
//
// Prints the suite's test-result JSON line ({"tag":"pass"} / {"tag":"fail"}).

import process from "node:process";

const wrtc = (await import("@roamhq/wrtc")).default;

const TIMEOUT_MS = 60_000;

async function run() {
  const a = new wrtc.RTCPeerConnection({ iceServers: [] });
  const b = new wrtc.RTCPeerConnection({ iceServers: [] });
  a.addEventListener("icecandidate", ({ candidate }) => {
    if (candidate) b.addIceCandidate(candidate);
  });
  b.addEventListener("icecandidate", ({ candidate }) => {
    if (candidate) a.addIceCandidate(candidate);
  });

  const dcA = a.createDataChannel("loopback");
  const incoming = new Promise((resolve) => {
    b.addEventListener("datachannel", ({ channel }) => resolve(channel));
  });

  const offer = await a.createOffer();
  await a.setLocalDescription(offer);
  await b.setRemoteDescription(offer);
  const answer = await b.createAnswer();
  await b.setLocalDescription(answer);
  await a.setRemoteDescription(answer);

  const dcB = await incoming;
  const open = (dc) =>
    dc.readyState === "open"
      ? Promise.resolve()
      : new Promise((r) => dc.addEventListener("open", r, { once: true }));
  await Promise.all([open(dcA), open(dcB)]);

  const roundTrip = new Promise((resolve, reject) => {
    dcB.addEventListener("message", ({ data }) => {
      if (data === "ping") {
        dcB.send("pong");
      } else {
        reject(new Error(`b received ${JSON.stringify(data)}`));
      }
    });
    dcA.addEventListener("message", ({ data }) => {
      if (data === "pong") {
        resolve();
      } else {
        reject(new Error(`a received ${JSON.stringify(data)}`));
      }
    });
    dcA.send("ping");
  });
  await roundTrip;

  a.close();
  b.close();
}

const timeout = new Promise((_, reject) => {
  setTimeout(() => reject(new Error("timed-out")), TIMEOUT_MS).unref();
});

let result;
try {
  await Promise.race([run(), timeout]);
  result = { tag: "pass" };
} catch (err) {
  result = { tag: "fail", val: String(err?.message ?? err) };
}
console.log(JSON.stringify(result));
// Report pass/fail in the result line, not the exit status (matching the
// single-peer contract); wrtc's worker threads keep the event loop alive, so
// exit explicitly.
process.exit(0);
