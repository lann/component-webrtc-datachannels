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

The suite (see `conformance/README.md`) is built and green in CI: a shared
conformance guest, the `conformance-signalingd` mailbox, adapters for
`wasmtime`, `jco-node`, `jco-browser`, and `wasip3-guest`, the interop pairs
`wasmtime`<->`jco-node`, `wasmtime`<->`jco-browser`, and
`wasmtime`<->`wasip3-guest` (both orders each) — all run in CI over loopback
via `just conformance` — plus the Shadow lab in CI (non-loopback,
deterministic) and the workstation-only netns lab (`just conformance::netns` /
`just conformance::nat`) covering `lan`, `stun-srflx` (behind a one-to-one
full-cone NAT), `turn-relay`, and `nat-symmetric`. The full netns lab has been
confirmed on a Linux workstation: all four scenarios pass 11/11. Still open:

- **Non-loopback interop.** The interop pairs run over loopback only; the
  labs run single-runtime peers.
- **netns-lab peer coverage.** The lab's `--peer-kind` covers `wasmtime` (all
  scenarios) and `wasip3-guest` (`lan` only — the in-guest sans-I/O stack
  supports no STUN/TURN); a jco-node lab peer (a per-peer Node runner placed
  in a namespace) is deferred.

### A4. Guest-facing `peer-connection` configuration (ICE servers, transport policy)

The `peer-connection` constructor takes no configuration, so connectivity
config is host-side only: the Wasmtime host's `WasiWebrtcCtx` ICE config,
nothing at all on the jco host, and the deployment-side
`WEBRTC_UDP_BIND_ADDR` for the in-guest provider. Real deployments need
*application-owned* ICE configuration — TURN credentials are typically
ephemeral, fetched by the app at runtime — which no host-side channel can
express, and `RTCConfiguration.iceServers` is the most load-bearing field of
the W3C constructor this package mirrors.

Agreed design, following `wasi:http`'s `request-options` precedent of
**fallible setters**:

- A `peer-connection-config` builder resource with
  `set-ice-servers: func(servers: list<ice-server>) -> result<_, config-error>`
  and `set-ice-transport-policy` (`all | relay`), where
  `config-error = not-supported | invalid(string)`. Getters reflect what a
  successful set stored. `peer-connection`'s constructor takes
  `option<peer-connection-config>` by ownership, like `create-data-channel`
  takes its options.
- **Accepted ⇒ honored**: rejection happens eagerly at the setter, so any
  config that was successfully built is binding — an implementation may never
  silently ignore a field. `data-channel-options` keeps its infallible
  setters (its fields are universally supportable); capability-gated options
  get the fallible variant of the same builder pattern.
- The wasip3 provider returns `not-supported` from `set-ice-servers` (the
  in-guest sans-I/O stack has no STUN/TURN client) until `rtc`'s stun/turn
  crates are wired into the driver; the conformance manifest records this
  with an `unsupported` tag so the matrix shows it and forces cleanup when
  support lands.
- The same error channel carries **host policy**: on the Wasmtime host, a
  `WasiWebrtcCtx` hook may reject guest-supplied servers
  (allowlist/deny), surfacing as `not-supported`/`invalid` — capability and
  policy are indistinguishable to the guest, deliberately.
- Conformance contract: per target, `set-ice-servers` returns
  `not-supported` XOR the TURN scenarios pass. This extends the netns
  TURN coverage beyond the wasmtime target (the browser `RTCPeerConnection`
  accepts `iceServers` directly) — see A3's netns-lab coverage gap.

Out of scope, deliberately host/deployment-side: the bind address
(topology), loopback candidates (demo glue), buffer bounds and timeouts
(host resource policy). Do this when the first non-LAN use case lands or
when extending lab coverage to jco — it is an interface change, not hygiene.

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

### E12. `rtc` sender breaks ordered delivery observed by libwebrtc receivers

The non-wasm reference peer (libwebrtc via `@roamhq/wrtc`) intermittently
(~10-25% of runs over loopback) receives an `ordered: true` channel's
messages block-rotated — e.g. indexes `8..15,0..7` or `2..7,0,1,8..15` —
from *both* `rtc`-based senders (the Wasmtime host on `webrtc` 0.20.0-rc.4
and the in-guest `wasip3-impl` provider, so the defect is in the shared
sans-I/O `rtc` core, not a driver). All messages arrive in a single 0-1 ms
burst (no retransmission gap, so no loss involved), whole contiguous blocks
swap rather than individual messages, and loopback UDP is FIFO — so the
sender is emitting SCTP DATA whose ordering metadata cannot restore the
application order (a per-chunk `U`-bit or stream-sequence-number assignment
defect). `rtc`-based *receivers* mask it (rtc <-> rtc pairs pass `ordering`
consistently), which is why no pre-reference pair ever caught it. Reproduce
with `conformance-interop --pair wasmtime-x-reference --only ordering` in a
loop; the reference peer logs the received index order under
`REF_PEER_DEBUG=1`. Until it is fixed upstream, the `ordering` test carries
the `ordered-delivery` tag and the four affected pair directions
(`wasmtime`/`wasip3-guest` x `reference`, both orders) declare it
unsupported in `conformance/manifests.toml` (a flaky failure cannot be an
`expected-fail`: it would `unexpected-pass` on green runs). File the
upstream `webrtc-rs/rtc` issue with the repro above, and drop the manifest
entries once a fixed pin lands.
||||||| parent of 3da5c1e (Record the peer-connection config design and env-var follow-ups (A4, E13))

### E13. Unify and shrink the implementations' environment-variable surface

The env-var surface is now indexed (AGENTS.md) and fails loud on invalid
values, but three refinements remain:

- **Latching semantics differ per implementation.** The Rust
  implementations latch `WEBRTC_MAX_INBOUND_BUFFER_BYTES` at first read
  (`OnceLock`), for the process lifetime; the jco host resolves it lazily
  per channel (`maxInboundBuffered` in `jco-impl/webrtc.js`). A harness
  that changes the value mid-process observes different behavior per
  target. Align on read-once-at-first-use everywhere.
- **The Wasmtime host library still reads ambient env.**
  `WasiWebrtcCtx::default()` sources the buffer bound from the env var, so
  a malformed value panics even when the embedder immediately overrides it
  via `set_max_inbound_buffer_bytes` — and a library panicking on ambient
  state is harsh for embedders in general. Consider making `wasmtime-impl`
  env-free: move the env read to its consumers (the demo binaries and the
  conformance adapter) through the existing ctx accessor, keeping the one
  shared variable name working across all three implementations for the
  conformance suite.
- **Policy: no new implementation-level env vars** without first
  considering the proper channel — `WasiWebrtcCtx` on the Wasmtime host, an
  exported configure hook for the jco module (which `jco --map` gives no
  instantiation-time config channel), the WIT surface for in-guest
  configuration (see A4). Every env var is an undeclared API that each
  deployer must discover; the AGENTS.md table is its only registry.

`WEBRTC_UDP_BIND_ADDR` stays environment-shaped on purpose: the bind
address is deployment topology, owned by whoever runs the process (see A4).


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

Remaining: the two Rust implementations' graces are blind timers — a
too-slow path can still lose the final message at the bound, and every
close pays the full grace even when nothing is queued. The jco host shows
the better shape (tear down as soon as the send buffers drain, with the
timer only as the cap); doing the same in the Rust implementations needs
SCTP send-queue introspection from `webrtc`/`rtc` (an upstream capability
— track alongside E5/E6's upstream items).

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

1. Flush-aware teardown in the Rust implementations (F5, upstream-gated).
2. When the first non-LAN use case (or jco lab coverage) calls for it:
   guest-facing peer-connection configuration (A4).
3. The rest as touched: the remaining corpus-mirror unification (F7), jco
   in-process timeout isolation (F11), env-var latching/library hygiene
   (E13), the remaining conformance-matrix gaps (A3).
