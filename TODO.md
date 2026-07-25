# TODO

Findings from a fresh, rigorous review of the repository against the project
goal: *multiple production-quality implementations of the WebRTC
`peer-connection` / `data-channel` component-model interfaces that run the same
wasm component with compatible (not necessarily identical) behavior across
browsers, mobile, and cloud.*

Items are grouped by area and ordered roughly by impact. File references are
relative to the repository root. Resolved items are deleted (their history
lives in the commits and PRs that fixed them); item ids stay stable, so the
lettering has gaps.

## A. Strategic / whole-project

### A3. Cross-host conformance: loopback matrix + labs in place; interop matrix incomplete

The suite (see `conformance/README.md`) is built and green in CI: the shared
conformance guest (37 tests), the `conformance-signalingd` mailbox, adapters
for `wasmtime`, `jco-node`, `jco-browser`, and `wasip3-guest`, the interop
pairs (every target against the non-wasm libwebrtc reference peer in both
orders, the reference self-pair, and the direct `wasmtime`<->`jco-node`
and `wasmtime`<->`wasip3-guest` pairs) — all run in CI over loopback via
`just conformance` — plus the Shadow lab in CI (non-loopback,
deterministic: the single-runtime targets and the
`wasmtime`<->`wasip3-guest` interop pair in both orders via the executor's
per-role `--offerer-kind`/`--answerer-kind`) and the workstation-only netns
lab (`just conformance::netns`
/ `just conformance::nat`) covering `lan`, `stun-srflx` (behind a one-to-one
full-cone NAT), `turn-relay`, and `nat-symmetric`. Still open, each a
concrete extension of the existing machinery:

- **Non-loopback interop beyond the wasm pair.** The Shadow lab covers the
  `wasmtime`<->`wasip3-guest` pair; the reference-anchored directions still
  run over loopback only. Run the reference interop pairs under Shadow
  (libwebrtc already runs there as a single-runtime target), and teach the
  netns executor per-role peer kinds for the STUN/TURN/NAT scenarios the
  simulator cannot model (the target-neutral `PeerCommand` placement in
  `conformance/adapters/common/src/peer_command.rs` already abstracts the
  per-peer invocation; the Shadow executor's per-role flags are the
  pattern).
- **jco-node netns peer.** Add a per-peer Node runner placeable in a
  namespace (the missing `--peer-kind`), unlocking both lab coverage for
  the jco host and jco directions for non-loopback interop.
- **TURN through guest config.** The netns TURN scenarios configure ICE
  host-side (`WebrtcIceConfig`); now that `peer-connection-config` exists,
  add a scenario that passes the TURN server through the guest-facing
  config instead, asserting the accepted-XOR-connects contract end to end —
  and extending TURN coverage to the jco peer (the browser
  `RTCPeerConnection` accepts `iceServers` directly) once the jco lab peer
  lands. (The Wasmtime host's embedder-policy veto over guest-supplied
  servers — surfacing through the same `config-error` channel — remains
  unimplemented; add it when a policy use case appears.)
- **Re-validate the netns lab against the current corpus.** The last full
  workstation confirmation predates the corpus growth (the two-peer subset
  is now 12 tests, including `channel-close-flush`); the lab is
  workstation-only, so this needs a manual `just conformance::netns` /
  `just conformance::nat` pass.

## E. Implementations

### E5. Retire the Shadow syscall shim once upstream closes the gap

`webrtc` is at `0.20.0-rc.4`; its quinn-udp GSO/GRO UDP batching
([`webrtc-rs/webrtc#820`](https://github.com/webrtc-rs/webrtc/pull/820))
needs syscalls the Shadow simulator does not implement — Shadow rejects the
`IPPROTO_IP` receive-metadata `setsockopt`s (`IP_PKTINFO` et al.) with
`ENOPROTOOPT`, which quinn-udp treats as fatal to socket construction, and
does not implement `recvmmsg` (`ENOSYS`), which quinn-udp's Linux receive
path calls with no fallback. The conformance Shadow lab bridges this with
an in-binary syscall shim compiled into its `conformance-peer` build
(`conformance/adapters/wasmtime/src/bin/peer/shadow_shim.rs`, armed by the
`CONFORMANCE_SHADOW_SYSCALL_SHIM` environment variable the Shadow executor
sets on its simulated peers): each override forwards the call and stubs only
Shadow's documented failure; anything unexpected aborts the peer. Loopback
and netns paths never set the variable and get pure pass-through.

The shim is a bridge, not a fix. Retire it when any upstream lands and
reaches a published release:

- **quinn-udp**: tolerate the receive-metadata option failures (a branch
  exists:
  [`lann/quinn#tolerate-unsupported-recv-cmsg-options`](https://github.com/lann/quinn/tree/tolerate-unsupported-recv-cmsg-options))
  *and* restore a `recvmmsg` `ENOSYS` fallback (existed pre-0.6; both are
  needed).
- **webrtc**: degrade `wrap_udp_socket` to a plain socket when
  `UdpSocketState::new` fails, honoring #820's per-packet-fallback promise.
- **Shadow**: implement `recvmmsg` (loop the existing `recvmsg` handler)
  and the `IP_PKTINFO`/`IP_MTU_DISCOVER`/`IP_RECVTOS` options.

If a future `webrtc` bump grows the syscall surface again, the shim aborts
(unexpected `setsockopt` errno) or the lab hangs with Shadow's "unsupported
syscall" warning — extend the shim or fix upstream, per its module docs.

### E6. Unwind the `rtc` git pin once upstream ships a release

The `rtc` dependency is pinned to an upstream `master` commit (`Cargo.toml`
`[patch.crates-io]`) because the srflx source-address fix
([`webrtc-rs/rtc#136`](https://github.com/webrtc-rs/rtc/pull/136), merged
upstream; the netns lab's `stun-srflx` scenario passes with zero dropped
srflx-sourced transmits with it, vs ~100 drops without) is not yet in any
published release. Drop the patch and return to a plain crates.io version
once a release including it ships.

### E12. node-webrtc receive-side reordering: latent in jco-node

`@roamhq/wrtc` (node-webrtc) can dispatch an ordered channel's messages to
JS with the head of the sequence displaced past later messages: its
`RTCDataChannel` constructor re-registers the libwebrtc observer (new
messages then dispatch directly to the JS loop) *before* re-dispatching the
backlog the temporary pre-wrapper observer cached, so arrivals adjacent to
channel open overtake every earlier message (`rtc_data_channel.cc`; a
standalone reproduction with sender-side SCTP instrumentation proved the
senders' wire metadata correct). The reference peer was moved off
node-webrtc (to Google's libwebrtc via LiveKit's Rust bindings) and no
longer exposes it, and the `ordering` manifest skips were dropped — but the
**jco-node host still runs on `@roamhq/wrtc`**, so the bug is latent behind
the jco-node target and its pairs, historically masked by the JSPI startup
delay between channel open and first sends. If `ordering` ever flakes on a
jco-node row with a head-displaced index sequence, this is the cause: the
`ordering` test retains its `ordered-delivery` tag so a manifest entry can
scope the skip immediately. Upstream write-ups and the reproduction live
with the local rtc checkout (`wrtc-receive-reordering-bug.md`,
`rtc-ordered-delivery-bug.md` — the latter a distinct `rtc` default-options
bug found in the same investigation — and `rtc-ordering-repro/`); file them
at WonderInventions/node-webrtc and webrtc-rs/rtc.

### E14. `webrtc-rs` drops inbound channel events on a full event queue

The `webrtc` 0.20 driver forwards each inbound message into a bounded
(256-entry) per-channel event queue with `try_send`, logging and
**discarding** the event when the queue is full
(`peer_connection/driver.rs`, "Failed to send DataChannelMessage … Full").
For reliable channels the loss is usually repaired by SCTP retransmission,
but under load it surfaces as message loss: `channel-close-flush` at
16x512 flaked in the `reference-x-wasmtime` direction under the full
interop corpus (the wasmtime receiver's pump lags, the queue fills, and
payloads vanish between SCTP and the application). The corpus works around
it by running that test at the default 4x256. Upstream fix would be a
blocking send (backpressure into SCTP) or a receive-window tie-in; track
alongside E12's reference-pair findings.

### E15. `rtc` emits no SCTP stream reset on data-channel close

The sans-I/O `rtc` stack's channel-level close is local-only: the
handler-level `close()` is a stub (`src/peer_connection/handler/
datachannel.rs`, `fn close … Ok(())`), so `RTCDataChannel::close()` marks
the local state and emits **no** RECONFIG/stream reset. A remote peer never
observes the close — `channel-close-flush` with a wasip3 offerer hangs a
libwebrtc answerer past the attempt guard (recorded as the
`wasip3-guest-x-reference` expected-fail in `conformance/manifests.toml`),
and only passes against `rtc`/`webrtc-rs` answerers because those detect
the offerer's process exit as a connection death instead. Incoming resets
are handled (a libwebrtc offerer's close is observed fine — the
`reference-x-wasip3-guest` direction passes). Fix upstream by emitting the
stream reset from the handler close; drop the manifest entry when a fixed
pin lands (the expected-fail's unexpected-pass tripwire enforces this).

## F. Conformance suite

### F5. Replace the bounded close-drain graces with flush-aware teardown

The interop "attempt timed-out" flake family (a peer closing immediately
after its last send, discarding the just-sent barrier sentinel with the
SCTP send queue) is now handled in every implementation: the Wasmtime host
defers network teardown by a bounded `CLOSE_DRAIN` grace on `close()` and
on resource drop, the in-guest `wasip3-impl` driver drains before its
rtc-level close, and the jco host closes its channels at once (keeping the
post-close contract) but tears the connection down only after every
channel's `bufferedAmount` drains, bounded by `CLOSE_DRAIN_MS`.

Remaining: the **channel-level** close is now flush-aware in all three
implementations (`bufferedAmount` in the jco host; `outstanding_bytes`
from `webrtc`/`rtc` in the Rust implementations, polled bounded before the
transport close). The **connection-level** graces are still blind timers
(`CLOSE_DRAIN` on close/drop in the Wasmtime host and the wasip3 pump's
drain window): a too-slow path can still lose the final message at the
bound, and every close pays the full grace even when nothing is queued.
Summing the per-channel outstanding bytes before the connection teardown
would give the connection path the same shape.

### F7. Unify the remaining corpus mirrors (plan + params)

The test ids are now cross-checked (each full-corpus adapter verifies its
registered list against the guest's `list-tests` export before running; the
runner rejects results for unregistered ids and warns on missing cells in
report-backed rows; empty `--only` selections are errors). Still mirrored by
hand with no consistency check:

- the orchestration plan (guest `run()` dispatch, `plan_for()` in
  `conformance/adapters/common/src/lib.rs`, `IN_PROCESS` in
  `conformance/adapters/jco/driver.js`), and
- the message params (`params_for()` / `paramsFor()`, plus re-defaulted
  `4`/`256` in the peer binaries).

The natural next step is to make `list-tests` authoritative for these too:
extend `test-descriptor` with the plan (and params), have the adapters
consume it, and delete the mirrors. Separately, `Missing` in a full-corpus
loopback row could be escalated from a warning to a failure once expected
coverage is expressible per target (the interop pairs legitimately run the
two-peer subset).

### F11. jco in-process test timeouts cannot cancel the timed-out attempt

The jco adapters' `withTimeout` (`conformance/adapters/jco/driver.js`)
abandons but cannot cancel a timed-out test attempt: the wedged guest
instances, their `RTCPeerConnection`s and pending long-polls keep running in
the same Node/browser process for the rest of the corpus and can degrade
later tests with no attribution (contrast: the wasmtime adapter drops the
`Store`, and subprocess peers are `kill_on_drop`). Consider per-test child
processes for jco-node (matching the other adapters' isolation) or at least
noting the contamination risk in the result document.

## Suggested priority

1. Flush-aware teardown at the connection level (F5, upstream-gated) and
   the upstream reports it depends on (E14, E15).
2. The rest as touched: the remaining corpus-mirror unification (F7), jco
   in-process timeout isolation (F11), the remaining conformance-matrix
   gaps (A3).
