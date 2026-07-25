# Conformance test suite

A conformance suite for the `lann:webrtc-datachannels` implementations. It runs
the **same wasm conformance guest component** against each target and asserts
**WIT-observable interoperable behavior only** — never SDP contents, candidate
ordering, timing, or exact error strings. The suite makes the project's promise
testable in two ways:

- **Behavioral conformance:** the shared conformance guest runs against each
  implementation and must observe the semantics the WIT documents.
- **Interop conformance:** pairs of implementations connect to each other over
  real signaling and must establish connections and exchange data.

The suite owns its own signaling mechanism (the mailbox server below) —
deliberately separate from the demo `rendezvous` proposal and from any future
standardized signaling interface, so it can evolve with the tests and be
discarded without API cost.

## What exists today

- **Registry + runner:** the test registry (`tests.toml`), the manifest schema,
  and the `conformance-runner` that aggregates adapter results, applies
  expected-fail policy, and renders the matrix.
- **Signaling server:** `conformance-signalingd`, the suite-owned HTTP
  mailbox that relays opaque SDP/ICE blobs between two peers. The runner starts
  it (ephemeral localhost port, gated on `/healthz`) and tears it down.
- **Conformance guest + wasmtime adapter:** the shared conformance
  guest component (`guest/`, exporting `conformance:suite/runner`) and the
  `wasmtime` adapter (`adapters/wasmtime/`), which runs that guest against the
  native Wasmtime host (loopback ICE, in-process signaling) and emits the
  adapter result document the runner classifies against its `manifests.toml`
  entry.
- **jco adapters:** the browser-first host
  transpiled by jco (`adapters/jco/`), run two ways — under Node with
  `@roamhq/wrtc` (`jco-node`, `run-node.mjs`) and inside headless Chromium via
  playwright-core (`jco-browser`, `run-browser.mjs`). The jco host
  drives real signaling over the suite mailbox with `fetch` (`signaling.js`)
  and implements the full `connections` surface (`webrtc.js`); the shared
  corpus orchestration lives in `driver.js`. Classified against the `jco-node`
  and `jco-browser` entries in `manifests.toml`.

  The package pins `@bytecodealliance/jco-transpile` to 0.5.0 through an npm
  `overrides` entry (jco 1.25.2's own range stops at 0.4.x):
  js-component-bindgen 2.1.0 fixed a borrow-cleanup codegen bug in
  stream-carrying imports on host-implemented resources that otherwise
  crashes the streaming data-channel tests. Drop the override once a jco
  release depends on jco-transpile >= 0.5.0 itself.

  jco's async ABI always uses JSPI (`WebAssembly.Suspending`), so the Node-driven
  targets (`jco-node` and the jco-node half of the interop pair) require **Node
  24+** run with `--experimental-wasm-jspi`; the `jco-browser` target runs the
  guest in headless Chrome, which has JSPI natively.
- **wasip3-guest adapter:** the whole WebRTC
  stack in wasm (`adapters/wasip3/`): the shared conformance guest is composed
  (`wac plug`) with the `wasip3-impl` provider component (the sans-I/O `rtc`
  stack driven over WASIp3 `wasi:sockets` UDP), an in-guest `wasi:http` mailbox
  client (`mailbox/`), and a CLI driver exporting an async `wasi:cli/run`
  (`driver/`) that reports each test's single-line JSON result on stdout. The
  `conformance-adapter-wasip3` orchestrator runs the composed component under
  `wasmtime run` (v46+; component-model async + WASIp3 + `wasi:http`) — one
  process per peer for the two-peer behavioral tests, connecting over
  `wasi:sockets` UDP loopback across processes and signaling through the suite
  mailbox — and writes `results/wasip3-guest.json`.
- **Reference peer + interop pairs:** the **non-wasm reference peer**
  (`adapters/reference/`, the `conformance-reference-peer` binary): a native
  program driving Google's libwebrtc — the stack browsers ship — through
  [LiveKit's Rust bindings](https://crates.io/crates/libwebrtc), with no wasm
  component or WIT bindings, speaking the same single-peer contract, mailbox
  protocol, and guest-owned signal-blob schema as every other peer. It is the
  suite's wire-level anchor: the `conformance-interop` binary (in
  `adapters/wasmtime/`) pairs every target against it in both orders
  (`wasmtime`, `jco-node`, and `wasip3-guest` over the full two-peer corpus;
  `jco-browser` over the `interop-handshake` smoke test, one headless-Chromium
  instance per test), so a red pair cell implicates the target's stack rather
  than a second guest instance. It also runs the `reference` self-pair
  (validating the reference peer itself) and the retained
  implementation-vs-implementation pairs `wasmtime`<->`jco-node` and
  `wasmtime`<->`wasip3-guest`, each in both orders. Classified against the
  `reference` and pair entries in `manifests.toml`. The first build downloads
  LiveKit's prebuilt static libwebrtc (`webrtc-sys`).

## Layering: adapters vs. environment executors

The suite separates *what runs a peer* from *where its network lives*:

- **Adapters** are environment-agnostic. Each target has one way to run a
  single guest instance of one test, configured entirely through
  flags/environment (test id, role, signaling URL, room, message parameters,
  bind address, ICE servers): the in-process adapters
  (`conformance-adapter-wasmtime`, the jco drivers,
  `conformance-adapter-wasip3`) and the per-process peers — the native
  `conformance-peer` binary, the composed wasip3 component under
  `wasmtime run`, and the `conformance-reference-peer` binary. A peer knows nothing
  about namespaces, simulators, or how its
  addresses were provisioned; every out-of-process peer honours the same
  single-peer contract (`--test`/`--role`/`--server`/`--room`/…, one JSON
  `test-result` line on stdout).
- **Environment executors** own the network environment and drive the corpus
  through those peers: `conformance-netns` (in `adapters/common`) provisions
  the netns lab and places each peer with `ip netns exec`;
  `conformance-shadow` (also in `adapters/common`) renders a Shadow simulation
  config and lets the simulator launch the peers. Executors decide addresses,
  scenarios, and process placement, and pass everything a peer needs as
  configuration; both build each peer's command line from the shared
  per-target templates in `conformance-adapter-common`'s `peer_command`
  module.

Supporting a peer in a new environment therefore means teaching the executor a
per-role command template — not the peer about the environment.

## Running

From the repository root:

```sh
just conformance
```

This builds the conformance guest component and `conformance-signalingd`, runs
the `wasmtime` adapter (which starts its own in-process signaling server and
writes `conformance/results/wasmtime.json`), transpiles the guest for the jco
adapters and runs the `jco-node` and `jco-browser` targets, composes and runs
the `wasip3-guest` target under `wasmtime run`, runs the interop pairs (every
target against the reference peer in both orders, the `reference` self-pair,
and the retained `wasmtime`<->`jco-node` and `wasmtime`<->`wasip3-guest`
implementation pairs), then invokes
`conformance-runner`, which reads the test registry, the target manifests,
and those adapter result documents, starts and health-checks a standalone
signaling server, applies the expected-fail / unexpected-pass policy, tears the
server down, and writes the markdown matrix to `conformance/matrix.md`. It exits
nonzero on any `fail` or `unexpected-pass`.

CI ([`.github/workflows/conformance.yml`](../.github/workflows/conformance.yml))
runs the same recipes split build-once/run-many: a `build` job compiles every
artifact (`just conformance::build-all`) and hands them to the runner jobs as a
tarball artifact, so the `matrix` job (`just conformance::run-matrix`) and the
`shadow-lab` job (`just conformance::run-shadow` + `classify-shadow`) execute
prebuilt binaries without compiling — the Rust dependency tree is compiled and
cached exactly once per run.

The jco targets and their interop pairs invoke `node --experimental-wasm-jspi`,
so a JSPI-capable **Node 24+** must be on `PATH` (see the jco note above); the
reference peer is a native binary and needs no Node at all. The
`conformance-interop` binary honours the `CONFORMANCE_NODE` environment variable
if a specific node binary is needed. The `wasip3-guest` target and its interop
pairs invoke `wasmtime run` (v46+, installed by `scripts/setup.sh`; overridable
via `CONFORMANCE_WASMTIME`). To run a single target in isolation, use the
per-target recipes (`just conformance::jco-node`, `just conformance::jco-browser`,
`just conformance::wasip3`, `just conformance::interop`).

Within each adapter, tests run **in parallel by default**: every test's peers
use fresh guest instances (or processes) and their own signaling room, so
tests are independent. The default concurrency scales with the cores available
to the adapter process (respecting its CPU affinity mask): 3 × cores clamped
to [2, 12] for the wasmtime and wasip3-guest adapters, and a more conservative
2 × cores clamped to [2, 8] for the jco targets and the interop pairs, whose
per-test Node/Chromium startup runs on the hang-guard clock. The multipliers
come from measuring the loopback corpus pinned to 2 CPUs (matching hosted CI
runners): wall time improves steeply up to 3 × cores — where it sits within
~2s of the corpus's intrinsic floor, the fixed-length `error-timed-out`
probe — and higher values buy <2s while adding contention. Each adapter
exposes a `--jobs` flag to override the default (`--jobs 1` restores serial
execution). Every test attempt is
individually bounded by a hang guard (90s, one attempt — no retries; generous
because peer-process startup and 4-wide CI contention are on the clock, while
the hosts' shorter `wait-connected` timeouts classify genuine connection
failures first), and the `just` recipes additionally cap each whole adapter
run (`conformance-timeout`, 600s by default) so a systemic hang fails in
minutes rather than stalling CI.

Run the runner's unit tests and the signaling server's integration tests with
the rest of the workspace:

```sh
just test
# or just the signaling server:
cargo nextest run -p conformance-signalingd
```

## Signaling server (`conformance-signalingd`)

A small standalone HTTP mailbox server used to relay signaling blobs between two
peers over plain HTTP/1.1 long-poll (no WebSockets), reachable identically from
native Rust, browser/Node `fetch`, and `wasi:http`. The full wire contract is in
[`signaling/PROTOCOL.md`](signaling/PROTOCOL.md).

Run it standalone (binds an ephemeral localhost port and prints its URL):

```sh
cargo run -p conformance-signalingd
# or bind a fixed routable address for cross-machine / NAT-lab runs:
cargo run -p conformance-signalingd -- --host 0.0.0.0 --port 8080
```

## netns lab (`just conformance::netns`)

The default adapters connect their peers over the loopback interface. The **ICE
lab** instead runs the two peers of each test over a real routed
network path, so the ICE handshake exercises non-loopback candidates — and, for
the server-mediated scenarios, is forced through a STUN/TURN server. It is a
small routed network of Linux **network namespaces** provisioned entirely with
`ip`, `nft`, and coturn's `turnserver` (no containers): an offerer, an answerer,
and a signaling namespace, each on its own `/30` subnet behind a router
namespace. The topology (namespace names, addresses, ports, TURN credentials)
and its provisioning live in Rust, in `conformance-adapter-common`'s `lab`
module (see its docs for the topology diagram and addresses).

Because the two peers live in separate namespaces, each runs as its own process,
placed with `ip netns exec`; the target-neutral environment executor
(`conformance-netns`, in `adapters/common`) provisions the lab, runs the signaling
server (and coturn) in the signaling namespace, drives the corpus, and tears the
lab down. Like the Shadow lab, its `--peer-kind` flag selects the peer: the
native `conformance-peer` binary (`wasmtime`, the default), the composed
wasip3 conformance component under `wasmtime run` (`wasip3-guest`, whose
in-guest sans-I/O stack supports no STUN/TURN — only the `lan` scenario fits),
or the non-wasm reference peer binary (`reference`, whose ICE flags map onto
libwebrtc's ICE configuration and support every scenario).
Results are written to `conformance/results/<target>-<scenario>.json` with the
scenario as the report's `environment`, so each scenario is its own matrix
row.

Scenarios:

| Scenario | What it exercises |
| --- | --- |
| `lan` | Direct host-candidate connectivity over the router (no server). |
| `stun-srflx` | coturn as a STUN server behind a static one-to-one (full-cone) NAT; the router blocks the direct peer↔peer path so a server-reflexive path must be used, and the NAT delivers traffic addressed to the srflx candidates. |
| `turn-relay` | coturn as a TURN server; the direct path is blocked and the peers are relay-only, so data is relayed by coturn. |
| `nat-symmetric` | coturn as a STUN/TURN server behind a symmetric NAT; the direct path is blocked and the symmetric NAT makes srflx unusable, so ICE falls back to a TURN relay. |

Run a scenario from the repository root (requires **root**, for `ip netns
exec`, and `turnserver` on `PATH` for the non-`lan` scenarios — both provided by
[`scripts/setup.sh`](../scripts/setup.sh)):

```sh
just conformance::netns lan
just conformance::netns stun-srflx
just conformance::netns turn-relay
just conformance::netns nat-symmetric
# or run both NAT scenarios (the NAT matrix) at once:
just conformance::nat
# or run the lan scenario with the wasip3-guest or reference peer:
just conformance::netns lan wasip3-guest
just conformance::netns lan reference
```

For interactive debugging, a provisioned lab can be kept up across runs: run the
executor once, then re-run it with `--no-provision` (it neither provisions nor
tears down) while inspecting the namespaces with `ip netns exec` by hand.

### NAT matrix

Server-reflexive candidates are only meaningful when a peer's mapped address
differs from its host address, which requires NAT between the peers and the
router. The NAT scenarios add an nftables source-NAT on the router (applied by the
`lab` module's nftables provisioning) that rewrites each peer's forwarded
traffic to its own "public" address:

- `stun-srflx` uses a **static one-to-one (full-cone) NAT** (`snat … persistent`
  plus a prerouting DNAT of each public address back to its peer): the mapping
  is endpoint-independent and inbound traffic addressed to a peer's public
  address is delivered, so the srflx path connects — the meaningful
  server-reflexive path. (Both peers' mappings live on the *same* router
  namespace, and conntrack cannot hairpin one peer's fresh flow into the other
  peer's reverse mapping, so hole punching alone cannot deliver the traffic on
  this topology; the static DNAT is what stands in for the second NAT device.)
- `nat-symmetric` uses a **symmetric NAT** (`snat … random`): the mapping is
  endpoint-dependent, so the address the STUN server observed is useless to the
  peer and ICE must fall back to a TURN relay.

The NAT scenarios are part of the workstation-only netns lab (see below); run
them with `just conformance::nat`.

The netns lab is **workstation-only**: CI does not run it. CI's non-loopback
coverage comes from the Shadow lab (`shadow-lab` in
[`.github/workflows/conformance.yml`](../.github/workflows/conformance.yml)),
which needs no root or network namespaces; the netns lab remains the
higher-fidelity environment for exercising the STUN/TURN/NAT candidate paths
on a real kernel. When adding non-loopback coverage, prefer the Shadow lab —
whatever runs there runs in CI, gated on every change — and reserve the netns
lab for what genuinely needs a real kernel network stack: the NAT behaviors
(nftables conntrack) and a real coturn relay.

## Shadow lab (`just conformance::shadow`)

The **Shadow lab** gives the same "two peers on separate hosts over a
non-loopback path" property as the netns lab, but runs the peers inside the
[Shadow](https://github.com/shadow/shadow) discrete-event network simulator
instead of network namespaces. Shadow runs the *unmodified* peer and
`conformance-signalingd` binaries under a single deterministic simulation,
intercepting their syscalls to model the network in user space — so it needs
**no root and no network namespaces**, which makes it reproducible in
sandboxes and hosted CI where the netns lab cannot run.

A target-neutral environment executor (`conformance-shadow`, in
`adapters/common`) generates a Shadow YAML config with three hosts per test (a
signaling server, an offerer, and an answerer, each on its own simulated IP),
runs `shadow` once over the whole corpus, parses the per-host process stdout,
folds the two peer results, and writes
`conformance/results/<target>-shadow.json` (environment `shadow`), so each
target appears as its own matrix row. Its `--peer-kind` flag selects the
per-role peer command template:

- `wasmtime` runs the native `conformance-peer` binary, gathering each peer's
  host candidate from its simulated interface address (`--bind-addr`);
- `wasip3-guest` runs the fully composed wasip3 conformance component under
  `wasmtime run` (the same invocation as the loopback adapter), pointing the
  in-guest provider at each host's simulated address through the
  `WEBRTC_UDP_BIND_ADDR` environment variable;
- `reference` runs the non-wasm reference peer (`conformance-reference-peer`);
  libwebrtc gathers candidates from the simulated host's own interface, so no
  bind address is passed.

The `--offerer-kind` / `--answerer-kind` flags override `--peer-kind` per
role, placing an **interop pair** — the two peers of each test running
different targets on separate simulated hosts. The `shadow` recipe uses this
to run the wasmtime <-> wasip3-guest pair in both orders
(`wasmtime-x-wasip3-guest-shadow.json` / `wasip3-guest-x-wasmtime-shadow.json`),
giving the implementation pair non-loopback coverage in CI; the loopback
interop matrix (`conformance-interop`) remains the pair coverage for the
Node-hosted and reference targets.

Run the Shadow targets from the repository root (needs `shadow` on `PATH`). Shadow
ships no upstream prebuilt binary, so install it into `~/.local` by downloading
the prebuilt binary from this repository's `shadow-dev` GitHub prerelease
([`scripts/download-shadow.sh`](../scripts/download-shadow.sh)) or by building it
from source ([`scripts/build-shadow.sh`](../scripts/build-shadow.sh)).
`scripts/setup.sh` does not install Shadow; the recipe below prints this guidance
and fails if the binary is missing:

```sh
just conformance::shadow
```

Shadow does not implement UDP `SO_REUSEADDR`/`SO_REUSEPORT`, which webrtc's mDNS
multicast socket sets, so the `wasmtime` peers run with `--disable-mdns` (they
connect over explicit host candidates, so mDNS is unused); the sans-I/O wasip3
stack has no mDNS at all. Only the Shadow peers pass the flag; the netns
lab, which runs on a real kernel, is unchanged.

The `wasmtime` peer additionally carries a Shadow syscall shim
([`adapters/wasmtime/src/bin/peer/shadow_shim.rs`](adapters/wasmtime/src/bin/peer/shadow_shim.rs)):
in-binary overrides of `setsockopt` and `recvmmsg` that forward each call as
received and stub only Shadow's documented failure (`ENOPROTOOPT` for the
`IPPROTO_IP` receive-metadata options, `ENOSYS` for `recvmmsg` — emulated via
`recvmsg`); an unexpected error aborts the peer instead of being masked. The
stubs engage only when the `CONFORMANCE_SHADOW_SYSCALL_SHIM` environment
variable is set, which the executor does for the peer processes it places
inside the simulation — every other environment gets pure pass-through. TODO
item E5 tracks the upstream fixes that would retire the shim.

CI runs the Shadow lab in a dedicated job (`shadow-lab` in
[`.github/workflows/conformance.yml`](../.github/workflows/conformance.yml));
Shadow ships no prebuilt binary, so the job downloads the prebuilt binary from
the `shadow-dev` prerelease (built on demand by the `shadow-build` workflow)
rather than rebuilding from source.

## Layout

```
conformance/
  README.md                # this file
  tests.toml               # test registry: id, tags, description
  manifests.toml           # target capability manifests ([target.<id>] tables)
  runner/                  # conformance-runner (Rust workspace member)
  signaling/
    PROTOCOL.md            # mailbox wire protocol spec
    server/                # conformance-signalingd (Rust workspace member)
  wit/                     # conformance WIT; deps symlink to root wit/
  guest/                   # the shared conformance guest component
  adapters/                # per-target adapters (wasmtime / jco / wasip3);
                           #   common/ holds the shared native building blocks
                           #   (registry/plans, peer subprocess invocation,
                           #   single-attempt runner, corpus runner, result
                           #   document),
                           #   the netns-lab topology/provisioning (netns +
                           #   nftables + coturn, in Rust), and the
                           #   target-neutral netns-lab and Shadow-lab executors
```

## Test registry (`tests.toml`)

Every conformance test is declared once in [`tests.toml`](tests.toml) with a
stable `id`, a set of `tags`, and a one-line `description`. The conformance
guest mirrors these ids/tags via its `list-tests` export. See the
comments at the top of the file for the schema. Grow the corpus by adding
`[[test]]` entries; keep tags stable across growth so manifests remain valid.

## Capability manifests (`manifests.toml`)

Every target is declared as a `[target.<id>]` table in the single
[`manifests.toml`](manifests.toml). An entry-less table is still load-bearing:
it registers the target, so the matrix gets a row (and a target that stops
reporting shows up as a visible all-missing row instead of vanishing). Under
its table a target may declare:

- `[[target.<id>.unsupported]]` entries referencing **tags** — every matching
  test is reported `skip-unsupported` (visible in the matrix, never a failure).
  A mandatory `reason` explains why.
- `[[target.<id>.expected-fail]]` entries referencing **test ids** — a known
  divergence that keeps the run green while staying visible. A mandatory
  `tracking` reference (e.g. a `TODO.md` item) records the follow-up. An
  expected-fail that **passes** becomes `unexpected-pass` and **fails** the
  run, forcing the manifest to be cleaned up.

Either kind of entry may carry an optional `environments = ["shadow", ...]`
list scoping it to specific environments (the same strings adapters report,
e.g. `loopback`, `lan`, `stun-srflx`, `turn-relay`, `nat-symmetric`, `shadow`).
An entry without `environments` applies to every environment (the original
behavior, so existing manifests are unchanged); a scoped entry applies only
when the report's environment is in the list, and takes precedence over an
unscoped entry for the same tag/test in its environments. An empty
`environments = []` list is a manifest error.

See the schema comment at the top of [`manifests.toml`](manifests.toml) for
the full entry shapes.

## Reading the matrix

`conformance-runner` renders a markdown table with one row per
`(target, environment)` and one column per test. Most targets run only in the
`loopback` environment; the netns lab (above) adds `lan` / `stun-srflx` /
`turn-relay` / `nat-symmetric` rows for the `wasmtime` target, and the Shadow
lab adds a `shadow` row for the `wasmtime` and `wasip3-guest` targets. Cell
values:

| Symbol | Meaning |
| --- | --- |
| `pass` | Passed and expected to pass. |
| `FAIL` | Failed and not expected to — **fails the run**. |
| `skip` | Skipped: the target's manifest declares the test's tag unsupported. |
| `xfail` | Expected-fail: failed as the manifest predicted (does not fail the run). |
| `UNEXPECTED-PASS` | An expected-fail that passed — **fails the run**; update the manifest. |
| `—` | No adapter reported a result for this (target, test). |

The runner exits nonzero if any cell is `FAIL` or `UNEXPECTED-PASS`.

## Adding a target

A new target = a new adapter (under `adapters/`) that emits a JSON result
document plus a new `[target.<id>]` table in `manifests.toml`. No change to
the runner or the registry is required. Result document shape:

```json
{ "target": "wasmtime", "environment": "loopback",
  "results": [ { "test_id": "ordering", "status": "pass", "detail": null } ] }
```

Adapters report raw `pass` / `fail` / `skip`; the runner applies manifest policy
and reclassifies.

## Rules for evolving the suite

- Never assert implementation-identical behavior (SDP text, candidate order,
  error strings, timing) in tests — WIT-observable outcomes only.
- When a target genuinely cannot support a feature, add a manifest
  `unsupported` entry with a reason instead of weakening the test.
- When a test fails due to a known divergence, add an `expected-fail` entry
  with a tracking reference (e.g. a `TODO.md` item) instead of skipping or
  deleting the test. An expected-fail that starts passing fails the run until
  the manifest is cleaned up, so manifests stay honest.
- Grow the corpus by adding `[[test]]` entries to `tests.toml`; keep tags
  stable across growth so manifests remain valid. The conformance guest's
  corpus, `tests.toml`, and the adapters' registries must stay in sync.
- Never copy the root `wit/` package into `conformance/wit`; always use the
  `deps` symlink, per the repo-wide convention.
- Conformance work must not change production host behavior except where a
  test deliberately drives a fix; keep the hosts' behavior in sync — this
  suite is what asserts it.
