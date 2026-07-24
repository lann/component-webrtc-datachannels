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

### E7. wasip3-impl provider: three related channel-plumbing bugs

All in `wasip3-impl/src/provider.rs`:

1. **Incoming channels get a disconnected pump waker.** `pump_incoming`
   receives the real waker but binds it as `_waker` and never uses it
   (`provider.rs:581`); each remote-initiated `DataChannel` handle is built
   with `waker: mpsc::unbounded().0` — a sender whose receiver is dropped
   immediately (`provider.rs:613`). `send()` on such a channel nudges nobody,
   so the outbound flush waits for the 50 ms tick (`runtime.rs:40`) or an
   unrelated inbound datagram: up to 50 ms latency per send on answered
   channels. Fix: pass the real waker through (the parameter is already
   threaded; this looks like unfinished wiring). The dead-peer placeholder at
   `provider.rs:370` is a separate, legitimate use of a disconnected sender.
2. **`receive-via-stream` claims are lost while a channel is still
   `Opening`.** The claim logic (`provider.rs:282-306`) does three separate
   `channel_mut` lookups; when the channel is not yet tracked (state
   `Opening` — the normal case right after `create-data-channel`), the
   `stream_claimed` check is skipped *and* the flag is never set, yet a pump
   is spawned and a stream returned. A second call in that window spawns a
   competing pump (messages split arbitrarily between two streams), and later
   `receive()` calls never get `error.receiving-via-stream` — violating the
   once-only contract (`wit/webrtc.wit:237-241`). Fix: record pending claims
   somewhere that survives the channel not being tracked yet (e.g. a
   pending-claims set on `Shared`, applied in `apply_event`'s `ChannelOpen`).
3. **`local_channel` is a single overwritten `Option`.**
   `create_data_channel` overwrites it (`provider.rs:322, 399`) and
   `incoming_data_channels` snapshots it once when the stream is taken
   (`provider.rs:425`, filtered at `provider.rs:598`). With two locally
   created channels the first is no longer filtered and is delivered on
   `incoming-data-channels` as if remote-opened; taking the stream before
   creating a channel mis-delivers the local channel too
   (`wit/webrtc.wit:269-270` promises "channels opened by the remote peer").
   Fix: a live set of locally created ids on `Shared`, consulted by
   `pump_incoming`. The shipped consumer creates-then-takes with one channel,
   which is what masks all three bugs today.

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

### E9. jco host: a failed connection never terminates per the WIT

In `jco-impl/webrtc.js` and its conformance twin
`conformance/adapters/jco/webrtc.js` (see F6):

- `#requireOpen` checks `#closed || connectionState === "closed"` but not
  `"failed"` (`jco-impl/webrtc.js:331-335`), so methods on a
  failed-but-not-closed connection proceed into the browser API and surface
  as `other`/`invalid-signaling` instead of `error.closed`
  (`wit/webrtc.wit:258-260`: the connection is terminally over when "closed
  by `close` **or has failed**").
- The `#channels` incoming stream is ended only from `close()`
  (`jco-impl/webrtc.js:512`); a connection *failure* never ends it, so a
  guest reading `incoming-data-channels` after a failure hangs forever —
  the WIT promises the stream "ends when the connection closes or fails"
  (`wit/webrtc.wit:269-270`). Same for the candidates stream.

`waitConnected` already detects `failed` (`jco-impl/webrtc.js:461-464`), so
the fix is to hoist that detection into a `connectionstatechange` handler
that latches failure and runs the same stream-ending/`#closeHooks` teardown
as `close()`. Coordinate with F5's deferred-teardown work, which touches the
same close path.

### E10. Consolidate scattered configuration; publish the env-var contract

- Two Wasmtime-host knobs live outside `WasiWebrtcCtx` despite its docs
  calling it the stable place to grow configuration: the hardcoded 30 s
  `CONNECT_TIMEOUT` (`wasmtime-impl/src/peer_connection.rs:46`; WIT makes the
  bound implementation-defined, so it should be configurable) and the
  process-global `OnceLock` env read of the inbound buffer bound
  (`wasmtime-impl/src/data_channel.rs:68-81`), which latches its first read
  and forbids per-store configuration. Move both onto `WasiWebrtcCtx`
  (keeping the env var as a default source), and fix the stale "the only
  knob so far" comment at `wasmtime-impl/src/lib.rs:83-85` (there are already
  two).
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

### E11. Dead code and module-placement sweep

- `wasmtime-impl`: `CloseSignal::inert()` has zero callers and a doc
  describing a demo-host architecture that no longer exists
  (`wasmtime-impl/src/data_channel.rs:199`); `PeerConnection::new` is
  uncalled (`peer_connection.rs:177-179`); the `wit-component` and tokio
  `rt-multi-thread` dev-deps are vestigial (`Cargo.toml` — the crate has no
  in-crate tests; add a doc pointer to where its tests actually live: the
  conformance suite and `examples/wasmtime-demo/tests`). The private
  `data_channel` module exposes ~a dozen `pub` items reachable by nobody —
  make them `pub(crate)` — and hosts the peer-connection factory
  (`new_peer_connection_with`, `close_peer_connections`, `CallbackHandler`,
  `data_channel.rs:459-627`) that `peer_connection.rs` then imports from it;
  move those into `peer_connection.rs` so the file names tell the truth. The
  duplicated stream-producer state machines (`LocalCandidateStream` /
  `IncomingChannelStream`, `wasmtime-impl/src/host.rs:580-668`) differ only
  in item conversion — one generic producer removes ~50 lines. Crate docs
  should also show the downstream `bindgen!` `with:` resource-mapping block
  that every consumer currently hand-copies
  (`examples/wasmtime-demo/src/main.rs` ≡
  `conformance/adapters/wasmtime/src/lib.rs:32-40`).
- `wasip3-impl`: `SansIoPeer::offerer` is dead (`wasip3-impl/src/peer.rs:118-134`);
  the `lib.rs:28` re-exports are unreachable on a cdylib-only crate; rename
  `SansIoPeer::answerer()` (it constructs *every* peer, including offerers —
  `provider.rs:352`) to `new()`, and `Runtime::waker()`
  (`runtime.rs:313`, an `mpsc::UnboundedSender<()>`) to something like
  `pump_nudge()` so it stops colliding with `std::task::Waker` used in the
  same file. `dead_shared()` (`provider.rs:716-728`) builds a full
  DTLS-cert-bearing peer just to serve as an always-closed placeholder and
  does it with `.expect("answerer construction is infallible")` — in exactly
  the error-recovery path that exists because construction *can* fail
  (`peer.rs:310-315` propagates two fallible `rtc` calls); make the peer
  optional (`Option<SansIoPeer>` or a small enum) instead.

### E12. Stale-comment sweep (verified comment/code mismatches)

Each of these was verified false against the code; they misdirect exactly the
reader (human or agent) who trusts docs first:

- `wasmtime-impl/wit/world.wit:12-15`: claims the crate does not implement
  `peer-connection` and that its host functions "trap if a guest ever calls
  them" — the crate fully implements it (`wasmtime-impl/src/host.rs:547-576,
  670-783`, `peer_connection.rs`), and the text uses the banned
  "`signaling` design target" phrasing. This is the first doc a contributor
  regenerating bindings reads. (Also `bindings.rs:5` / `peer_connection.rs:5`
  call `peer-connection` "the guest-driven signaling surface".)
- `examples/echo-demo/wit/webrtc-echo-demo.wit`: references
  `lann:webrtc-datachannels/signaling`, an interface that does not exist
  (relic of the `peer-connection` rename).
- `conformance/adapters/jco/webrtc.js:4-8` and `jco-impl/webrtc.js:10-11`:
  see F6.
- `wasip3-impl/src/peer.rs:10-11`: "the `rtc` fork stubs `ifaces()`" — the
  workspace pins upstream `rtc` master, not a fork (`Cargo.toml:38-39`).
- `wasip3-impl/src/runtime.rs:37-39`: "mirroring the reference driver's 50ms
  cap" — no such driver exists in the repo; cite the actual upstream source
  or drop the claim.
- `wasip3-impl/src/provider.rs:328-330`: the doc on `started_pump` describes
  a removed delivered-ids set; the field means "the pump task has been
  spawned".
- `wasmtime-impl/src/lib.rs:83-85`: "the only knob so far is the
  `SettingEngine` hook" — `ice_config` is a second knob, added right below.
- `conformance/adapters/common/src/bin/shadow.rs:12-13`: "drives the corpus
  … with retries" — there are no retries anywhere
  (`common/src/lib.rs:386-388` explicitly forbids them); plus the
  "long-poll retries cover any residual race" claims (see F11).
- Stale "Phase 0/6" build-plan narration:
  `conformance/runner/src/main.rs:40, 83-84`,
  `conformance/runner/src/plan.rs:67`,
  `conformance/adapters/common/src/lab.rs:160` — exactly the process
  commentary AGENTS.md forbids.
- `conformance/guest/src/lib.rs:407-408`: the barrier comment "the channel is
  reliable and ordered" is false for `max-retransmits-accepted`, whose
  channel is created with `max_retransmits = 0`
  (`guest/src/lib.rs:1078-1080`). Latent two-peer hang on lossy paths (the
  barrier sentinel can be dropped and both peers wait out the hang guard);
  run that test's barrier over a reliable channel, or accept and document the
  restriction to lossless paths.
- `examples/echo-demo/src/lib.rs:130-131`: "Trickle each side's candidates"
  describes code that drains the candidate stream to end-of-gathering before
  adding any (batch, not trickle) — fine in-process, misleading as a
  template for real two-peer code.
- `jco-impl/test/browser.mjs:98-101`: the comment credits the allowlist regex
  with blocking `..` traversal; the regex matches `..` fine — the separate
  `pathname.includes("..")` check is what blocks it. A "simplification"
  removing the "redundant" check would open the hole the comment invites.

Beyond the fixes, consider a review-policy line in AGENTS.md: comments should
not make claims about *other* files/implementations unless a test enforces
the claim (the "8 MiB across all implementations" WIT claim survives because
conformance actually pins it; most of the list above rotted because nothing
did).

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

### F6. The jco host exists as two ~720-line copies that have already diverged

`jco-impl/webrtc.js` (749 lines) and `conformance/adapters/jco/webrtc.js`
(721 lines) are ~713 lines byte-identical — the full `DataChannelOptions` /
`DataChannel` / `PeerConnection` surface plus all stream/queue helpers is
copy-pasted, and nothing (test, script, or CI) checks the copies agree.
The real divergences today:

- `[Symbol.dispose]` hooks on `DataChannel` and `PeerConnection`
  (`jco-impl/webrtc.js:225-252, 497-509`) exist **only** in the jco-impl copy,
  so the copy the conformance suite actually executes leaks `@roamhq/wrtc`
  native ICE/DTLS/SCTP resources when a guest drops a resource without
  calling `close`.
- Both header comments are false: `conformance/adapters/jco/webrtc.js:4-8`
  claims its twin "implements only the demo `connect` shortcut" (no such
  shortcut exists in either file), and `jco-impl/webrtc.js:10-11` claims the
  conformance suite asserts behavioral sync (the suite never loads
  `jco-impl/webrtc.js`; `conformance/adapters/jco/run-node.mjs` imports only
  the local copy).
- The only intended difference is one error-message path
  (`resolveRTCPeerConnection`: "run `npm install` in jco-impl" vs "…in
  conformance/adapters/jco").

Fix: extract a single shared module both locations import (parameterize or
genericize the install-hint message), porting the `Symbol.dispose` hooks so
both users get them, and rewrite both headers. If a copy must remain for some
structural reason, add a CI `diff` check with the allowlisted divergence.
Splitting the WIT-stream interop shims (`streamItems` / `collectByteStream` /
`toByteChunk` / `bytesToStream`, `jco-impl/webrtc.js:579-671`) and the
generic stream helpers into their own module(s) first would make the shared
core smaller and the split natural.

### F7. Wire up `list-tests` and make missing results loud

The corpus is hand-mirrored with no consistency check: test ids exist in
**four** places (`conformance/tests.toml`; guest `corpus()` at
`conformance/guest/src/lib.rs:82-123`; `TESTS` at
`conformance/adapters/common/src/lib.rs:164-194`; `TESTS` at
`conformance/adapters/jco/driver.js:17-47`), the orchestration plan in three
(guest `run()` dispatch `guest/src/lib.rs:126-154`; `plan_for()`
`common/src/lib.rs:229-251`; `IN_PROCESS` `driver.js:50-69`), and message
params in two plus stray re-defaults (`params_for()`
`common/src/lib.rs:254-267`; `paramsFor()` `driver.js:80-97`; re-defaulted
`4`/`256` in `conformance/adapters/wasmtime/src/bin/peer.rs:61-66` and
`conformance/adapters/wasip3/driver/src/lib.rs:77-78`). The guest exports
`list-tests` *specifically* so the registry can be cross-checked
(`conformance/wit/world.wit:63-65`, `tests.toml:4`,
`conformance/runner/src/registry.rs:5-7`) — and nothing ever calls it
(`registry.get()` is `#[allow(dead_code)]` "used by later phases",
`registry.rs:64-69`).

The failure mode is silent: a test missing from one mirror renders as
`Missing`, which is neutral (`conformance/runner/src/results.rs:62-79`
renders "—"; the runner exits 0), and a typo'd `--only` filter selects
nothing and passes (`common/src/lib.rs:433`; only the Shadow executor rejects
an empty selection, `common/src/bin/shadow.rs:184-186`).

Fix: (1) have each adapter call `list-tests` once and diff ids/tags against
its local list (or have the runner require every adapter report to cover
every registered test not excused by the manifest — a target that reported
fewer results than the registry should at minimum warn, better fail);
(2) reject empty `--only` selections in `run_corpus` like the Shadow executor
does. With the cross-check in place, the JS/Rust mirrors can shrink to plan +
params only.

### F8. `patch-generated.mjs` fails open against a floating jco version

`conformance/adapters/jco/patch-generated.mjs:21-28` regex-rewrites jco's
*generated* borrow-cleanup loop to work around an upstream codegen bug. Two
fragilities: the regex is whitespace/shape-sensitive against generated code,
and when it matches nothing the script prints `rewrote 0 borrow-cleanup
loop(s)` and **exits 0** — the failure then resurfaces at runtime as a
cryptic `TypeError … Symbol(handle)` far from the cause. Meanwhile both
package.jsons float jco (`"^1.19.0"`: `conformance/adapters/jco/package.json:13`,
`jco-impl/package.json`) across the 1.25.2 the bug was observed on, so a
routine `npm install`/`npm update` can move the resolved codegen out from
under the regex with no signal.

Fix: pin jco to an exact version in both package.jsons, and make the patch
fail (or at minimum warn loudly) when the match count is 0 while a
known-affected jco version is installed, so "fixed upstream" and "regex no
longer matches the still-broken output" are distinguishable. Document in the
script header why the `jco-impl` pipeline does not need the patch (different
async mode) so the asymmetry is a decision, not a mystery.

### F9. Three mailbox clients have drifted; the browser proxy strips protocol headers

The signaling protocol has three independent client implementations with
diverging behavior:

- wasmtime host: `wait=10000`, treats **any** 204 as done, ignores `x-done`
  (`conformance/adapters/wasmtime/src/lib.rs:219-249`).
- jco host: `wait=10000`, ignores `x-done`
  (`conformance/adapters/jco/signaling.js:72-98`).
- wasip3 client: sends **no** `wait` param (falls back to the server's 25 s
  default long-poll) and *requires* `x-done` on 204
  (`conformance/adapters/wasip3/mailbox/src/lib.rs:140, 176-202`).

Separately, the browser adapter's same-origin proxy forwards only
`content-type` (`conformance/adapters/jco/run-browser.mjs:163, 171-173`),
dropping `x-seq`/`x-done` — harmless today only because the jco client
ignores headers; any header-dependent client routed through it would fail
mid-handshake confusingly.

Fix: pick one interpretation (per `conformance/signaling/PROTOCOL.md`), align
the three clients on `wait` and `x-done` handling, and forward all upstream
headers in the proxy (one line).

### F10. Delete dead conformance plumbing

- **`Plan::Skip` is triplicated for zero users** and would *hang*, not skip:
  the skip branch runs the guest as a two-peer offerer with no answerer
  (`conformance/adapters/wasmtime/src/main.rs:100-108`,
  `conformance/adapters/wasip3/src/main.rs:66-69`), the jco `SKIP` set is
  empty by construction (`conformance/adapters/jco/driver.js:70, 75,
  179-180`), and the guest never returns `Skipped`
  (`conformance/guest/src/lib.rs:64-70`). Delete the plan variant, the `SKIP`
  set, and the adapter branches (or implement skipping for real if a use is
  imminent).
- **The `trickle` knob is end-to-end dead**: `test-config.trickle`
  (`conformance/wit/world.wit:45-47`) is set `true` by every adapter,
  parseable as `--no-trickle` in the wasip3 driver, and never read by the
  guest; the WIT comment describes a `trickle-variants` tag that does not
  exist in `tests.toml`. Delete the field (or add the tests it was for).
- **The runner's `--signaling-bin` spawns a server no adapter uses**
  (`conformance/runner/src/main.rs:49-87`; every adapter spawns its own);
  `conformance/runner/src/signaling.rs` (118 lines) exists solely to support
  it, and the surrounding "Phase 0 / later phases" comments narrate a build
  plan that already happened. Delete the flag, the module, and the stale
  comments (the `classify` recipe in `conformance/justfile` passes
  `--signaling-bin`; update it too).
- Minor: the dead `Role::Both => "answerer"` arm in
  `conformance/adapters/wasmtime/src/bin/interop.rs:66-72`; the
  `#[allow(dead_code)]` `description` / `ExpectedFail::reason` fields if they
  stay unused after F7 (or render them into the matrix, which would make the
  skip/xfail reasons visible — today they never reach the artifact).

### F11. Replace fixed sleeps with the health-poll pattern the suite already has

- `conformance/adapters/common/src/bin/netns.rs:274` sleeps a fixed 500 ms
  per test for signaling-server bind; `conformance/adapters/common/src/lab.rs:611`
  sleeps 1 s for coturn. The suite already has the right pattern
  (`waitHealthy` polling `/healthz`, `conformance/adapters/jco/run-node.mjs:99-121`;
  `conformance/runner/src/signaling.rs:55-72`) — use it. Related: the
  netns/Shadow comments claiming "peer-side long-poll retries cover any
  residual race" (`netns.rs:273-274`, `shadow.rs:297-298`) are wrong — all
  three mailbox clients treat transport failure as fatal, so slow server
  startup fails the test.
- The wasip3 driver lingers `CLOSE_GRACE_NANOS` = 500 ms on **every**
  invocation (`conformance/adapters/wasip3/driver/src/lib.rs:54, 61`),
  including in-process `both`-role tests with no remote peer to protect —
  ~20 s of pure sleep per corpus run. Skip the grace for `both`-role runs,
  or replace the guess with an ack over the channel.
- The jco in-process `withTimeout` (`conformance/adapters/jco/driver.js:127-133`)
  abandons but cannot cancel the timed-out promise; wedged guest instances,
  their `RTCPeerConnection`s and pending long-polls keep running in the same
  process for the rest of the corpus and can degrade later tests with no
  attribution. Consider per-test child processes for jco-node (matching the
  other adapters' isolation) or at least noting the contamination risk in the
  result document.

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

### G4. jco-impl's npm entry points are broken or misleading

- `npm start` and `npm test` run `node src/run.mjs` **without**
  `--experimental-wasm-jspi` (`jco-impl/package.json:10-11`; same for
  `start-remote`, line 15), which fails on Node 24 for a component
  transpiled with `--async-mode jspi`; the justfile recipes knowingly bypass
  them to add the flag (`examples/justfile:23, 89, 92`), and
  `jco-impl/src/run.mjs:4`'s header recommends the broken `npm start` path.
  Add the flag via the scripts (`node --experimental-wasm-jspi …` or
  `NODE_OPTIONS`), and fix the header.
- `test` ≡ `start` — `npm test` runs the demo, not a test; the actual test is
  `test:browser`. Rename or repoint.
- `build:component` / `build:component-remote` (`jco-impl/package.json`) are
  npm scripts that `cd` out of the package to run cargo + wasm-tools,
  wrapped by `just examples::build-component` (`examples/justfile:10-11`)
  which `cd`s *into* jco-impl to run them — three layers where top and
  bottom are both non-JS. Move the cargo/wasm-tools invocations directly
  into the justfile recipes and keep npm scripts JS-only.

## Suggested priority

1. Conformance-suite integrity: de-duplicate the jco host and port the
   dispose hooks (F6), wire `list-tests` + loud missing results (F7), make
   `patch-generated.mjs` fail closed and pin jco (F8), the jco close-drain
   half of the barrier race (F5).
2. Implementation contract gaps found incidentally: the wasip3 provider trio
   (E7), Wasmtime close-observation and `send-via-stream` buffering (E8),
   jco failed-state termination (E9).
3. Cheap hygiene, high leverage for humans and agents: the stale-comment
   sweep (E12), dead plumbing deletion (F10), transpile-flag check (G1),
   broken npm scripts (G4).
4. The rest as touched: mailbox-client convergence (F9), sleep→health-poll
   (F11), config consolidation + env-var index (E10), dead-code/module sweep
   (E11), example de-duplication (D3), the remaining conformance-matrix gaps
   (A3).
