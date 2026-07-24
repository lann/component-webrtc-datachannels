# List the available recipes.
default:
    @just --list

# The whole-run safety cap (seconds) for each conformance adapter invocation.
# Every attempt inside an adapter is already individually bounded (45s), but
# this caps the entire run so a systemic hang fails in minutes, not hours.
conformance-timeout := "600"

# Run every CI check locally, in the same order as .github/workflows/ci.yml.
ci: fmt-check clippy validate-wit build-component transpile test-browser test

# Run the fast pre-commit checks (fmt, clippy, WIT, Rust tests); see AGENTS.md.
check: fmt-check clippy validate-wit test

# Check formatting across all crates.
fmt-check:
    cargo fmt --all -- --check

# Run clippy across all crates.
clippy:
    cargo clippy --workspace --exclude echo-demo --exclude cli-signaling --exclude wasip3-webrtc-datachannels --exclude webrtc-consumer --exclude conformance-guest --exclude conformance-wasip3-mailbox --exclude conformance-wasip3-driver -- -D warnings
    cargo clippy -p echo-demo --target wasm32-unknown-unknown -- -D warnings
    cargo clippy -p conformance-guest --target wasm32-unknown-unknown -- -D warnings
    cargo clippy -p cli-signaling --target wasm32-wasip2 -- -D warnings
    cargo clippy -p wasip3-webrtc-datachannels --target wasm32-wasip2 -- -D warnings
    cargo clippy -p webrtc-consumer --target wasm32-wasip2 -- -D warnings
    cargo clippy -p conformance-wasip3-mailbox --target wasm32-wasip2 -- -D warnings
    cargo clippy -p conformance-wasip3-driver --target wasm32-wasip2 -- -D warnings

# Validate WIT packages.
validate-wit:
    wasm-tools component wit wit
    wasm-tools component wit examples/echo-demo/wit
    wasm-tools component wit examples/cli-signaling/wit
    wasm-tools component wit wasip3-impl/wit
    wasm-tools component wit examples/webrtc-consumer/wit
    wasm-tools component wit conformance/wit

# Run the Rust / Wasmtime tests (includes the cli-signaling integration test).
# nextest runs faster but does not execute doctests, so run those separately.
test:
    cargo nextest run --workspace --exclude echo-demo --exclude cli-signaling --exclude wasip3-webrtc-datachannels --exclude webrtc-consumer --exclude conformance-guest --exclude conformance-wasip3-mailbox --exclude conformance-wasip3-driver
    cargo test --doc --workspace --exclude echo-demo --exclude cli-signaling --exclude wasip3-webrtc-datachannels --exclude webrtc-consumer --exclude conformance-guest --exclude conformance-wasip3-mailbox --exclude conformance-wasip3-driver

# Run the conformance suite over the currently enabled targets (see
# conformance/README.md). Builds the shared conformance guest component, runs each
# adapter to produce conformance/results/<target>.json, then runs the runner
# over conformance/tests.toml + conformance/manifests.toml + those results — also
# spawning the standalone conformance-signalingd binary (ephemeral localhost
# port, gated on /healthz) to exercise its lifecycle — and writes the matrix to
# conformance/matrix.md, exiting nonzero on any fail or unexpected-pass.
#
# Enabled targets: wasmtime (native webrtc-rs), jco-node and jco-browser (the
# browser-first host transpiled by jco, run under Node and headless Chromium),
# wasip3-guest (the whole WebRTC stack in wasm: the guest composed with the
# wasip3-impl provider, run under `wasmtime run`), plus the interop pairs
# wasmtime<->jco-node, wasmtime<->jco-browser, and wasmtime<->wasip3-guest
# (both orders each). The jco
# targets need a JSPI-capable Node (24+; see conformance-jco-node) and, for the
# browser target, a Chrome 137+ binary (auto-detected, or set CHROME_PATH).
conformance: conformance-wasmtime conformance-jco-node conformance-jco-browser conformance-wasip3 conformance-interop build-signalingd
    cargo run -p conformance-runner -- \
        --tests conformance/tests.toml \
        --manifests conformance/manifests.toml \
        --results conformance/results \
        --signaling-bin target/debug/conformance-signalingd \
        --matrix-out conformance/matrix.md

# Build the conformance-signalingd mailbox server (shared by every adapter).
build-signalingd:
    cargo build -p conformance-signalingd

# Run the wasmtime conformance adapter over loopback. Writes
# conformance/results/wasmtime.json.
conformance-wasmtime: build-conformance-guest
    cargo build --release -p conformance-adapter-wasmtime --bin conformance-adapter-wasmtime
    timeout {{conformance-timeout}} target/release/conformance-adapter-wasmtime \
        --guest conformance/guest/build/conformance-guest.component.wasm \
        --out conformance/results

# Build the shared conformance guest component into conformance/guest/build/.
build-conformance-guest:
    cargo build --release -p conformance-guest --target wasm32-unknown-unknown
    mkdir -p conformance/guest/build
    wasm-tools component new \
        target/wasm32-unknown-unknown/release/conformance_guest.wasm \
        -o conformance/guest/build/conformance-guest.component.wasm

# Transpile the conformance guest for the jco adapters (Node + browser) into
# conformance/adapters/jco/generated (jco `--instantiation` mode so one process
# can stand up two guest instances). Rebuilds the guest component first.
transpile-conformance-guest: build-conformance-guest
    cd conformance/adapters/jco && npm run transpile

# Build the fully composed wasip3-guest conformance component into
# conformance/adapters/wasip3/build/conformance-wasip3.composed.wasm: the shared
# conformance guest is composed (`wac plug`) with the wasip3-impl provider
# (exports `connections` over wasi:sockets UDP), the in-guest wasi:http mailbox
# client, and the CLI driver that exports an async `wasi:cli/run` and reports
# the single-test result on stdout. Rebuilds the guest component first.
build-conformance-wasip3: build-conformance-guest
    cargo build --release -p wasip3-webrtc-datachannels --target wasm32-wasip2
    cargo build --release -p conformance-wasip3-mailbox --target wasm32-wasip2
    cargo build --release -p conformance-wasip3-driver --target wasm32-wasip2
    mkdir -p conformance/adapters/wasip3/build
    wac plug conformance/guest/build/conformance-guest.component.wasm \
        --plug target/wasm32-wasip2/release/wasip3_webrtc_datachannels.wasm \
        --plug target/wasm32-wasip2/release/conformance_wasip3_mailbox.wasm \
        -o conformance/adapters/wasip3/build/conformance-wasip3.guest.wasm
    wac plug target/wasm32-wasip2/release/conformance_wasip3_driver.wasm \
        --plug conformance/adapters/wasip3/build/conformance-wasip3.guest.wasm \
        -o conformance/adapters/wasip3/build/conformance-wasip3.composed.wasm

# Run the wasip3-guest conformance adapter: the composed component above runs
# under `wasmtime run` (v46+; component-model async + WASIp3 + wasi:http), one
# process per peer, connecting over wasi:sockets UDP loopback across processes
# and signaling through the in-guest wasi:http mailbox client. Writes
# conformance/results/wasip3-guest.json.
conformance-wasip3: build-conformance-wasip3
    cargo build --release -p conformance-adapter-wasip3 --bin conformance-adapter-wasip3
    timeout {{conformance-timeout}} target/release/conformance-adapter-wasip3 \
        --component conformance/adapters/wasip3/build/conformance-wasip3.composed.wasm \
        --out conformance/results

# Run the jco-node conformance adapter (guest transpiled by jco, run under Node
# with @roamhq/wrtc). jco's async ABI needs JavaScript Promise Integration, so
# this uses `node --experimental-wasm-jspi` and requires Node 24+ (which ships
# WebAssembly.Suspending). Writes conformance/results/jco-node.json.
conformance-jco-node: transpile-conformance-guest build-signalingd
    cd conformance/adapters/jco && timeout {{conformance-timeout}} npm run run:node

# Run the jco-browser conformance adapter (the same guest + host modules inside
# headless Chromium; Chrome 137+ ships JSPI). Writes
# conformance/results/jco-browser.json.
conformance-jco-browser: transpile-conformance-guest build-signalingd
    cd conformance/adapters/jco && timeout {{conformance-timeout}} npm run run:browser

# Run the interop pairs (each in both orders): wasmtime<->jco-node,
# wasmtime<->jco-browser (one headless-Chromium instance per test), and
# wasmtime<->wasip3-guest — one peer per runtime shares a signaling room and a
# real WebRTC data channel. Writes conformance/results/<pair>.json for each of
# the six pair directions.
conformance-interop: transpile-conformance-guest build-conformance-wasip3 build-signalingd
    cargo build --release -p conformance-adapter-wasmtime --bin conformance-interop
    timeout {{conformance-timeout}} target/release/conformance-interop

# Run the conformance netns lab for one scenario (lan | stun-srflx | turn-relay |
# nat-symmetric; see conformance/README.md). The target-neutral
# environment executor (conformance-netns, in conformance/adapters/common)
# provisions a routed network-namespace topology in Rust (netns + nftables +
# coturn; the `lab` module), places the two peers of each two-peer test in
# separate namespaces, and — for the server-mediated scenarios — routes them
# through coturn while the router blocks the direct path (and, for the NAT
# scenarios, source-NATs each peer), so the handshake exercises a real
# (non-loopback) candidate path. Writes conformance/results/<target>-<scenario>.json
# (environment column in the matrix). The optional peer_kind argument selects
# the peer (wasmtime | wasip3-guest); the wasip3-guest peer's in-guest sans-I/O
# stack supports no STUN/TURN, so only `lan` works for it. Needs root for
# `ip netns exec` (hence sudo) and `turnserver` on PATH for the non-`lan`
# scenarios (installed by scripts/setup.sh). The lab is always torn down on
# exit. `stun-srflx` runs behind a static one-to-one (full-cone) NAT so its
# srflx path is meaningful; `nat-symmetric` runs behind a symmetric NAT so ICE
# must fall back to a TURN relay.
conformance-netns scenario="lan" peer_kind="wasmtime": build-conformance-guest build-signalingd
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "{{peer_kind}}" = "wasip3-guest" ]; then
        just build-conformance-wasip3
    fi
    cargo build --release -p conformance-adapter-wasmtime --bin conformance-peer
    cargo build --release -p conformance-adapter-common --bin conformance-netns
    sudo timeout {{conformance-timeout}} target/release/conformance-netns \
        --scenario {{scenario}} \
        --peer-kind {{peer_kind}} \
        --guest conformance/guest/build/conformance-guest.component.wasm \
        --component conformance/adapters/wasip3/build/conformance-wasip3.composed.wasm \
        --signaling-bin target/debug/conformance-signalingd \
        --peer-bin target/release/conformance-peer \
        --out conformance/results

# Run the NAT matrix (see conformance/README.md): the srflx scenario behind a
# one-to-one (full-cone) NAT, where the server-reflexive candidates connect, and
# the symmetric-NAT scenario, where srflx fails and ICE must fall back to a TURN
# relay. Both write conformance/results/wasmtime-<scenario>.json. Like the rest
# of the netns lab it is workstation-only (not run in CI). Requires the same
# privileges/tools as `conformance-netns`.
conformance-nat: (conformance-netns "stun-srflx") (conformance-netns "nat-symmetric")

# Run the two-peer corpus inside the Shadow network simulator
# (https://github.com/shadow/shadow). Like the netns lab it places the two peers
# of each test on separate hosts over a routed, non-loopback path — but Shadow
# simulates the network in user space, so it needs NO root, network namespaces,
# or real kernel networking, which makes it reproducible and runnable in
# restricted sandboxes and CI. The target-neutral environment executor
# (conformance-shadow, in conformance/adapters/common) generates a single
# Shadow config per target, runs `shadow` once per target, and writes
# conformance/results/<target>-shadow.json (the `shadow` environment column):
# the wasmtime target runs the native conformance-peer and the wasip3-guest
# target runs the composed wasip3 conformance component under `wasmtime run`,
# with each in-guest provider bound to its host's simulated address via
# WEBRTC_UDP_BIND_ADDR. Needs `shadow` on PATH; install it with
# scripts/download-shadow.sh (prebuilt, from the `shadow-dev` release) or
# scripts/build-shadow.sh (from source).
conformance-shadow: build-conformance-guest build-signalingd build-conformance-wasip3
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v shadow >/dev/null 2>&1; then
        cat >&2 <<'EOF'
    ==> ERROR: the `shadow` binary is required by the Shadow lab but was not found
        on PATH.

        Shadow ships no upstream prebuilt binary, so install it one of these ways:
          * Download the prebuilt binary from the `shadow-dev` GitHub prerelease:
              ./scripts/download-shadow.sh
          * Build it from source (slow; needs the Debian/Ubuntu build deps):
              ./scripts/build-shadow.sh

        Both install to ~/.local (bin/shadow + lib/libshadow_*.so); make sure
        ~/.local/bin is on PATH afterward.
    EOF
        exit 1
    fi
    # The Shadow peer carries the in-binary syscall shim bridging Shadow's
    # syscall surface to the webrtc driver's quinn-udp UDP layer (see
    # conformance/adapters/wasmtime/src/bin/peer/shadow_shim.rs); the executor
    # arms it via CONFORMANCE_SHADOW_SYSCALL_SHIM on the simulated peers only.
    cargo build --release -p conformance-adapter-wasmtime --bin conformance-peer
    cargo build --release -p conformance-adapter-common --bin conformance-shadow
    timeout {{conformance-timeout}} target/release/conformance-shadow \
        --target wasmtime --peer-kind wasmtime \
        --guest conformance/guest/build/conformance-guest.component.wasm \
        --signaling-bin target/debug/conformance-signalingd \
        --peer-bin target/release/conformance-peer \
        --out conformance/results
    timeout {{conformance-timeout}} target/release/conformance-shadow \
        --target wasip3-guest --peer-kind wasip3-guest \
        --component conformance/adapters/wasip3/build/conformance-wasip3.composed.wasm \
        --signaling-bin target/debug/conformance-signalingd \
        --data-dir target/shadow-data-wasip3 \
        --out conformance/results

# Build the echo-demo guest component into examples/echo-demo/build/.
build-component:
    cd jco-impl && npm run build:component

# Transpile the echo-demo component for the Node host (runs build-component).
transpile: build-component
    cd jco-impl && npm run transpile

# Run the browser host test in headless Chrome (set CHROME_PATH to override).
test-browser: transpile
    cd jco-impl && npm run test:browser

# Run the Node (browser-first) host demo.
demo-node: transpile
    cd jco-impl && node --experimental-wasm-jspi src/run.mjs

# Run the Wasmtime (native) host demo.
demo-wasmtime count="1000" size="4096": build-component
    cargo run --release --bin wasmtime-webrtc-host -- \
        examples/echo-demo/build/echo-demo.component.wasm {{count}} {{size}}

# Build the wasip3 provider component (the whole WebRTC stack runs in-guest;
# it exports `lann:webrtc-datachannels/connections`) into
# target/wasm32-wasip2/release/wasip3_webrtc_datachannels.wasm.
build-wasip3-provider:
    cargo build --release -p wasip3-webrtc-datachannels --target wasm32-wasip2

# Build the webrtc-consumer component (imports `connections`) into
# target/wasm32-wasip2/release/webrtc_consumer.wasm.
build-webrtc-consumer:
    cargo build --release -p webrtc-consumer --target wasm32-wasip2

# Compose the consumer with the provider (`wac plug`) into
# target/webrtc-composed.wasm. The consumer's `connections` import is satisfied
# by the provider's export, yielding one self-contained component.
compose-webrtc: build-wasip3-provider build-webrtc-consumer
    wac plug target/wasm32-wasip2/release/webrtc_consumer.wasm \
        --plug target/wasm32-wasip2/release/wasip3_webrtc_datachannels.wasm \
        -o target/webrtc-composed.wasm

# Basic in-guest integration test: run the composed consumer+provider under
# `wasmtime`, standing up two peers that connect over loopback entirely inside
# wasm and exchange a message each way. Requires `wasmtime` (v46+) and `wac`.
# `timeout` bounds the whole run so a stuck handshake fails the recipe instead
# of hanging CI; the guest itself also bounds `wait-connected` internally.
test-webrtc-composed: compose-webrtc
    timeout 120 wasmtime run -W component-model-async=y -S cli -S p3 -S inherit-network \
        target/webrtc-composed.wasm
