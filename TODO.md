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

## F. Conformance suite

### F5. Interop barrier sentinel can be lost to the winner's immediate close

The interop "attempt timed-out" flake family is now diagnosed (via the
phase-marker logs): in a two-peer test the side that finishes its barrier
first closes immediately, and a close that tears the connection down
without draining can discard the just-sent barrier sentinel before it
reaches the wire, leaving the slower peer waiting for a sentinel that
never arrives (the browser does not surface the dirty teardown as a
channel close within the 90s guard). The **wasmtime host** now defers its
network teardown by a bounded `CLOSE_DRAIN` grace (the close is still
observed locally at once), mirroring `wasip3-impl`'s drain — which covers
every observed instance (wasmtime answerer + jco peer). Still open: the
**jco host**'s `close()` calls `pc.close()` immediately, so the symmetric
race (jco answerer strands a wasmtime offerer) remains possible; a
matching deferred-teardown there must keep the local close observation
immediate (the `#closed` gate) *and* mark the connection's channels closed
at once, or the delayed teardown would regress `post-close-send`.

The same race also reached the Wasmtime host through a second path, now
fixed: dropping the `peer-connection` resource (a guest returning without
calling `close`) tore the network down immediately, skipping the
`CLOSE_DRAIN` grace that `close()` applies — the cli-signaling round trip
flaked (~5-10% locally) with the offerer's `receive` failing `closed`
because the answerer's just-sent reply was discarded with the SCTP send
queue. `Drop` now mirrors `close()` (fire the signal, defer teardown by
the drain), and the cli-signaling host binary lingers briefly after the
guest returns so process exit does not cut the drain short. A
flush-aware teardown (close once the SCTP queue is empty, bounded) would
replace both graces.

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

1. Conformance-suite integrity: wire `list-tests` + loud missing results
   (F7), the jco close-drain half of the barrier race (F5).
2. The rest as touched: jco in-process timeout isolation (F11), the
   remaining conformance-matrix gaps (A3).
