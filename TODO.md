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

## D. Examples

### D3. De-duplicate example guest helpers and the wasmtime-demo binaries

- Near-identical helpers are copied across example guests:
  `collect_candidates` (`examples/echo-demo/src/lib.rs:156`,
  `examples/webrtc-consumer/src/lib.rs:187`,
  `examples/echo-remote/src/lib.rs:285`), `first_incoming`
  (`examples/cli-signaling/src/lib.rs:170`,
  `examples/webrtc-consumer/src/lib.rs:177`, plus inline in echo-demo), and
  the wasi-stdout `print` helper (`examples/cli-signaling/src/lib.rs:290` ≡
  `examples/webrtc-consumer/src/lib.rs:204`, doc comment included). A tiny
  shared demo-util crate ends the drift; if the duplication is intentional
  (each example maximally self-contained), say so once in `examples/`.
- `examples/wasmtime-demo/src/lib.rs` is a doc-comment-only empty lib while
  the binaries duplicate the glue it should host: `engine()` and
  `webrtc_ctx()` with the `WEBRTC_INCLUDE_LOOPBACK` hook are repeated in
  `src/main.rs`, `src/bin/cli-signaling.rs`, and `src/bin/echo-remote.rs`.
  Move them into the lib.
- `examples/webrtc-consumer/src/lib.rs:87-90` decides retryability by
  substring-matching a `Debug` rendering (`contains("wait-connected") &&
  contains("TimedOut")`); a context-string or variant rename silently
  disables the retry. Match on the typed WIT error variant before converting
  to `anyhow` (the conversion is the `anyhow!("… wait-connected: {e:?}")`
  formatting at the call sites).
- `examples/wasmtime-demo/src/main.rs`: the default component path is
  CWD-relative (`../echo-demo/build/…`), so `cargo run --bin
  wasmtime-webrtc-host` only works from `examples/wasmtime-demo/`. Resolve
  relative to `CARGO_MANIFEST_DIR` or make the argument required.

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

### E8. Wasmtime host: close observation and `send-via-stream` buffering

All in `wasmtime-impl/src/host.rs` unless noted:

1. **`send` does not observe connection close while in flight.** It checks
   `conn_closed.is_closed()` once *before* `wired.await` (`host.rs:368-370`)
   and never races `conn_closed.fired()` afterwards — unlike `receive`,
   which does exactly that with a documented biased race
   (`host.rs:406-436`; the pattern to copy). Per the crate's own comments
   the `webrtc` 0.20 wrapper neither errors sends nor emits `OnClose` after
   `PeerConnection::close` (`data_channel.rs:184-186`), so a `send` on a
   never-opened channel racing `close()` can pend forever, and a send landing
   just after close can report `Ok` for a silently dropped message — WIT
   requires in-flight operations to fail `error.closed`
   (`wit/webrtc.wit:317-319`). `send_via_stream` has the same unraced
   `wired.await` (`host.rs:446`) and checks close only between messages
   (`host.rs:467`).
2. **`send-via-stream` is a guest-controlled memory amplifier.** All queued
   messages are drained concurrently into `Vec`s via an unbounded mpsc
   (`host.rs:326-345, 459`); `Vec::with_capacity(length)` allocates up to
   4 GiB from a guest-declared `u32` (`host.rs:335`); and the declared-length
   check runs only **after** the payload stream ends (`host.rs:474`), so a
   message declaring `length: 1` can stream unbounded bytes first. This
   undercuts the WIT's stated rationale that `stream-message` exists "to
   bound in-memory buffering" (`wit/webrtc.wit:57-58`). Fix: enforce the
   declared length *during* collection and bound per-channel in-flight
   payload bytes (the inbound side already shows how).
3. **Incoming-channel delivery can violate the WIT's ordering.**
   `on_data_channel` spawns a task per channel that awaits the async
   `label()` before pushing to the incoming queue
   (`wasmtime-impl/src/peer_connection.rs:498-505`), so two channels opened
   in quick succession can be delivered out of order vs "in the order they
   open" (`wit/webrtc.wit:269-270`) — a real divergence risk against the
   browser host, where `ondatachannel` ordering is deterministic. Fix:
   reserve the queue slot synchronously in the callback and fill the label
   afterwards (the `DataChannel::deferred` machinery already supports
   deferred wiring).

### E10. Consolidate scattered configuration; publish the env-var contract

- Two Wasmtime-host knobs live outside `WasiWebrtcCtx` despite its docs
  calling it the stable place to grow configuration: the hardcoded 30 s
  `CONNECT_TIMEOUT` (`wasmtime-impl/src/peer_connection.rs:46`; WIT makes the
  bound implementation-defined, so it should be configurable) and the
  process-global `OnceLock` env read of the inbound buffer bound
  (`wasmtime-impl/src/data_channel.rs:68-81`), which latches its first read
  and forbids per-store configuration. Move both onto `WasiWebrtcCtx`
  (keeping the env var as a default source).
- The same knob has different failure semantics per implementation:
  `WEBRTC_MAX_INBOUND_BUFFER_BYTES` silently falls back to the default on a
  parse failure in wasip3 (`wasip3-impl/src/runtime.rs:141`, `.parse().ok()`)
  while `WEBRTC_UDP_BIND_ADDR` fails loud (`wasip3-impl/src/provider.rs:44-54`
  — though the error text is then swallowed by the `Err(_)` dead-peer arm at
  `provider.rs:367`; surface the message). Align on fail-loud. Also note the
  connect timeouts diverge across implementations (30 s wasmtime vs 20 s
  wasip3, `provider.rs:60`) — permitted by the WIT, but make it a decision.
- The JS host counts string payloads in UTF-16 code units, not bytes
  (`jco-impl/webrtc.js:705`, `data.length`), so the "8 MiB of payload bytes"
  bound (`wit/webrtc.wit:186-187`) diverges up to 2× for non-ASCII text —
  use `Buffer.byteLength`/`TextEncoder` for strings.
- There is no single index of the cross-process environment surface
  (`WEBRTC_UDP_BIND_ADDR`, `WEBRTC_MAX_INBOUND_BUFFER_BYTES`,
  `WEBRTC_INCLUDE_LOOPBACK`, `CONFORMANCE_SHADOW_SYSCALL_SHIM`,
  `CONFORMANCE_WASMTIME`, `CONFORMANCE_NODE`,
  `CHROME_PATH`/`CHROME_BIN`/`PUPPETEER_EXECUTABLE_PATH`, `SKIP_NODE`,
  `SKIP_NETNS_LAB`). Each is documented only at its use site; chasing a knob
  through four codebases is today's discovery path. Add one table (AGENTS.md
  or a doc both it and README link).

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

## G. Development environment / CI

### G1. jco transpile flags are not checked against the WIT

Any interface/method rename must be mirrored by hand in the
`--async-exports` / `--async-imports` / `--map` strings in
`jco-impl/package.json` (`transpile` and `transpile-remote`; AGENTS.md
documents this), but nothing verifies it — a mismatch fails only at
transpile or runtime. It has already drifted: the `--async-imports` lists
omit `data-channel.send-via-stream`, which the WIT declares `async`
(`wit/webrtc.wit:229`), so any guest built through this pipeline that calls
it silently gets the sync ABI. The conformance pipeline avoids the whole
class by transpiling with blanket `-I async`
(`conformance/adapters/jco/package.json:8`) — meaning the two jco pipelines
also exercise *different* async ABIs for the same interface, and only the
conformance one is asserted by the suite. Fix: generate the flags from the
WIT (or adopt blanket async in both pipelines), and add a CI check so a
drifted rename fails fast with a clear message.

## Suggested priority

1. Conformance-suite integrity: wire `list-tests` + loud missing results
   (F7), the jco close-drain half of the barrier race (F5).
2. Implementation contract gaps found incidentally: Wasmtime
   close-observation and `send-via-stream` buffering (E8).
3. Cheap hygiene, high leverage for humans and agents: the transpile-flag
   check (G1).
4. The rest as touched: config consolidation + env-var index (E10), example
   de-duplication (D3), jco in-process timeout isolation (F11), the
   remaining conformance-matrix gaps (A3).
