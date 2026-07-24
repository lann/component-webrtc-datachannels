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
deterministic) and the workstation-only netns lab (`just conformance-netns` /
`just conformance-nat`) covering `lan`, `stun-srflx` (behind a one-to-one
full-cone NAT), `turn-relay`, and `nat-symmetric`. The full netns lab has been
confirmed on a Linux workstation: all four scenarios pass 11/11. Still open:

- **Non-loopback interop.** The interop pairs run over loopback only; the
  labs run single-runtime peers.
- **netns-lab peer coverage.** The lab's `--peer-kind` covers `wasmtime` (all
  scenarios) and `wasip3-guest` (`lan` only — the in-guest sans-I/O stack
  supports no STUN/TURN); a jco-node lab peer (a per-peer Node runner placed
  in a namespace) is deferred.

## C. WIT interface design

### C1. Align the implementations with the documented `peer-connection` contract

The `peer-connection` contract is now specified in `wit/webrtc.wit` doc
comments (end-of-candidates = stream end; take-once streams; latched
`wait-connected`; sync, idempotent `close` with post-close calls failing
`error.closed`). A cross-implementation survey found the implementations
diverge from it, and none of the divergences are visible to the conformance
matrix. Fix the implementations and add a conformance test per behavior:

- **`wait-connected` latch (jco)**: jco does not latch `connected` — a
  connected-then-closed connection hangs the full 20s and rejects
  `timed-out`, and a terminal failure also rejects `timed-out` instead of
  `closed` (`conformance/adapters/jco/webrtc.js`); wasmtime and wasip3 latch
  and classify correctly.
- **Take-once streams (jco, wasip3)**: a second `local-ice-candidates` /
  `incoming-data-channels` call must return an immediately-ended stream
  (wasmtime's behavior). jco returns the *same* stream object each call (a
  live first stream makes the second consumption trap); wasip3's second
  `incoming-data-channels` replays every previously delivered channel as
  duplicate handles over shared state (`pump_incoming` restarts its cursor
  at 0, `wasip3-impl/src/provider.rs`).
- **Post-close calls → `error.closed` (all three)**: none gate signaling
  methods on close — wasmtime/wasip3 surface whatever the underlying stack
  returns (`other`/`invalid-signaling`), and jco's `create-offer` /
  `create-answer` have no error mapping at all, so a closed-pc rejection
  escapes as a raw trap rather than a WIT `error` (a bug regardless of the
  contract).

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
(`conformance/adapters/wasmtime/src/bin/peer/shadow_shim.rs`, cargo feature
`shadow-syscall-shim`): each override forwards the call and stubs only
Shadow's documented failure; anything unexpected aborts the peer. Loopback
and netns paths are unaffected and run shim-free.

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

## F. Examples

### F3. Wire up `rendezvous` end-to-end (tracking)

`demo:webrtc-echo/rendezvous` (`examples/echo-demo/wit/webrtc-echo-demo.wit`) is
defined but imported by no world and implemented by neither host. Per AGENTS.md,
the intended flagship example is two separate component instances (offerer /
answerer) connecting via `peer-connection` (now implemented everywhere) + a
`rendezvous` host that relays SDP/ICE over `wasi:http@0.3` (Wasmtime) / `fetch`
(jco) through a trivial local mailbox server (the conformance
`conformance-signalingd` is a ready-made candidate). This would exercise nearly
every interface at once and would make the echo demo's two peers genuinely
separate components, making it the reference example.

### F4. Drive the sans-I/O `rtc` stack across a real network (tracking)

`wasip3-impl` is now a **component** that runs the sans-I/O `rtc` stack
in-guest and exports the project `connections` interface, composed (`wac plug`)
with `examples/webrtc-consumer` for the same-host round-trip integration test.
The remaining step is a real deployment across separate machines: the consumer
chooses the bind address through `WEBRTC_UDP_BIND_ADDR` (which produces a
routable host candidate, exercised across a non-loopback simulated network by
the conformance Shadow lab); combined with `rendezvous` (item F3), two separate
components can then connect across a network.
Host-candidate gathering must stay explicit (`ifaces()` is `Unsupported` on
wasm).

## G. Development environment / CI

### G1. jco transpile flags are not checked against the WIT

Any interface/method rename must be mirrored by hand in the
`--async-exports` / `--async-imports` / `--map` strings in
`jco-impl/package.json:9` (AGENTS.md documents this), but nothing verifies it —
a mismatch fails only at transpile or runtime. Add a CI check (or generate the
flags from the WIT) so a drifted rename fails fast with a clear message.

## Suggested priority

1. Contract alignment: fix the `peer-connection` divergences and land their
   conformance tests (C1).
2. Strategic build-out: wire `rendezvous` (F3) and take `wasip3`'s
   WIT-speaking component to a real network (F4).
3. Cheap hygiene: the transpile-flag CI check (G1), the remaining
   conformance-matrix gaps (A3).
