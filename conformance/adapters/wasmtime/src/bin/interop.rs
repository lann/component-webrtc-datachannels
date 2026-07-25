//! Cross-runtime interop orchestrator for the conformance suite.
//!
//! It anchors every target against the suite's **non-wasm reference peer**
//! (`conformance/adapters/reference`: a native binary driving Google's
//! libwebrtc via LiveKit's Rust bindings) in both orders — `wasmtime`, `jco-node`, and
//! `wasip3-guest` over the full two-peer corpus, `jco-browser` over the
//! interop-handshake smoke test (one headless-Chromium instance per test).
//! Because the reference side runs no wasm component and no WIT bindings, a
//! green row proves the target's wire behavior against the ecosystem-defining
//! WebRTC stack — not merely against another instance of this repository's
//! shared guest — and a red one implicates the target, not the pair.
//!
//! Two implementation-vs-implementation pairs are retained: `wasmtime` <->
//! `wasip3-guest` (webrtc-rs and the sans-I/O `rtc` stack, the two native
//! stacks in this repository, meeting each other) and `wasmtime` <->
//! `jco-node` (the wasm guest crossing the host-language boundary). The
//! `reference` self-pair validates the reference peer itself over the same
//! corpus, backing the `reference` matrix row.
//!
//! One peer per side runs either as a native wasmtime guest instance
//! (provisioned by [`conformance_adapter_wasmtime`]) or out-of-process: the
//! jco-node peer via `conformance/adapters/jco/run-node.mjs --interop`, the
//! jco-browser peer via `run-browser.mjs --interop`, the wasip3-guest peer via
//! `wasmtime run` over the fully composed component
//! ([`conformance_adapter_wasip3::Wasip3Peer`]), and the reference peer as the
//! `conformance-reference-peer` binary. Both peers of a pair share one in-process
//! `conformance-signalingd` room and connect over a real WebRTC data channel.
//!
//! It writes one adapter result document per direction
//! (`wasmtime-x-reference.json`, `reference-x-wasmtime.json`, and so on) that
//! the conformance runner classifies against the matching manifests, exactly
//! like a single-target adapter.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Parser;
use wasmtime::component::Component;
use wasmtime::Engine;

use conformance_adapter_common::{
    fold_two, params_for, run_corpus, run_peer_command, run_test, write_report, AdapterReport,
    RawResult, TestOutcome, TWO_PEER_TESTS,
};
use conformance_adapter_wasip3::Wasip3Peer;
use conformance_adapter_wasmtime::{build_engine, make_config, run_instance, Role};

/// The hang guard for one test. Generous: everything is on the clock —
/// including out-of-process peer startup (a fresh Node process compiling the
/// JSPI wasm, or a whole headless Chromium) under 4-wide CI contention — while
/// the hosts' shorter `wait-connected` timeouts (20-30s) fire first, so a
/// genuine connection failure still surfaces as a WIT outcome rather than
/// tripping this bound.
const TEST_TIMEOUT: Duration = Duration::from_secs(90);

/// The corpus subset for the browser-backed reference pairs: each test boots
/// its own headless Chromium, so they run the flagship handshake only, keeping
/// the per-test browser cost flat while still anchoring Chrome's stack against
/// the reference peer across the wire.
const HANDSHAKE_ONLY: &[&str] = &["interop-handshake"];

/// One side of a pair: the runtime driving that peer.
#[derive(Clone, Copy)]
enum Side {
    /// An in-process native wasmtime guest instance.
    Wasmtime,
    /// The jco-node host, via `run-node.mjs --interop`.
    JcoNode,
    /// The jco host inside headless Chromium, via `run-browser.mjs --interop`.
    JcoBrowser,
    /// The composed wasip3-guest component, via `wasmtime run`.
    Wasip3,
    /// The non-wasm reference peer, via `conformance-reference-peer`.
    Reference,
}

/// One result document: a pair direction (`<offerer>-x-<answerer>`) or the
/// reference self-pair, with the corpus subset it runs.
struct Direction {
    target: &'static str,
    offerer: Side,
    answerer: Side,
    tests: &'static [&'static str],
}

/// The shared parameters of one interop test attempt (both sides see the same
/// signaling server, room, and message corpus).
struct Attempt<'a> {
    base_url: &'a str,
    test_id: &'a str,
    room: &'a str,
    count: u32,
    size: u32,
}

/// Run one out-of-process peer for one test/role/room, parsing its single-line
/// JSON `test-result` from stdout.
async fn run_external_peer(
    cli: &Cli,
    side: Side,
    attempt: &Attempt<'_>,
    role: &str,
) -> Result<TestOutcome> {
    let test_id = attempt.test_id;
    let shared_args = |command: &mut tokio::process::Command| {
        command
            .args(["--server", attempt.base_url])
            .args(["--test", test_id])
            .args(["--room", attempt.room])
            .args(["--role", role])
            .args(["--message-count", &attempt.count.to_string()])
            .args(["--message-size", &attempt.size.to_string()]);
    };
    match side {
        Side::Wasmtime => unreachable!("the wasmtime peer runs in-process"),
        Side::JcoNode => {
            let mut command = tokio::process::Command::new(&cli.node_bin);
            command
                .arg("--experimental-wasm-jspi")
                .arg(&cli.jco_run_node)
                .arg("--interop");
            shared_args(&mut command);
            run_peer_command(command, &format!("jco-node peer {test_id}/{role}")).await
        }
        Side::JcoBrowser => {
            let mut command = tokio::process::Command::new(&cli.node_bin);
            command.arg(&cli.jco_run_browser).arg("--interop");
            shared_args(&mut command);
            run_peer_command(command, &format!("jco-browser peer {test_id}/{role}")).await
        }
        Side::Wasip3 => {
            let peer = Wasip3Peer {
                wasmtime_bin: cli.wasmtime_bin.clone(),
                component: cli.wasip3_component.clone(),
            };
            peer.run(
                attempt.base_url,
                test_id,
                attempt.room,
                role,
                attempt.count,
                attempt.size,
            )
            .await
        }
        Side::Reference => {
            let mut command = tokio::process::Command::new(&cli.reference_peer);
            shared_args(&mut command);
            run_peer_command(command, &format!("reference peer {test_id}/{role}")).await
        }
    }
}

/// Run one side of a pair in the given role.
async fn run_side(
    cli: &Cli,
    engine: &Engine,
    component: &Component,
    side: Side,
    attempt: &Attempt<'_>,
    role: Role,
) -> Result<TestOutcome> {
    match side {
        Side::Wasmtime => {
            run_instance(
                engine,
                component,
                attempt.test_id,
                make_config(
                    role,
                    attempt.base_url,
                    attempt.room,
                    attempt.count,
                    attempt.size,
                ),
            )
            .await
        }
        _ => {
            let role = match role {
                Role::Offerer => "offerer",
                Role::Answerer => "answerer",
                Role::Both => unreachable!("interop peers always run a single role"),
            };
            run_external_peer(cli, side, attempt, role).await
        }
    }
}

/// Run one interop test to a raw result (single attempt; no retries).
async fn run_interop_test(
    cli: &Cli,
    engine: &Engine,
    component: &Component,
    base_url: &str,
    direction: &Direction,
    test_id: &str,
    room_seq: &AtomicU64,
) -> RawResult {
    let (count, size) = params_for(test_id);

    run_test(test_id, TEST_TIMEOUT, async || {
        let room = format!(
            "interop-{}-{}-{}",
            direction.target,
            test_id,
            room_seq.fetch_add(1, Ordering::SeqCst)
        );
        let attempt = Attempt {
            base_url,
            test_id,
            room: &room,
            count,
            size,
        };

        let offerer = run_side(
            cli,
            engine,
            component,
            direction.offerer,
            &attempt,
            Role::Offerer,
        );
        let answerer = run_side(
            cli,
            engine,
            component,
            direction.answerer,
            &attempt,
            Role::Answerer,
        );

        let (offerer_result, answerer_result) = tokio::join!(offerer, answerer);
        Ok(fold_two(
            offerer_result.context("offerer peer")?,
            answerer_result.context("answerer peer")?,
        ))
    })
    .await
}

/// Run the interop pairs (each target against the reference peer in both
/// orders, plus the retained implementation pairs).
#[derive(Debug, Parser)]
#[command(name = "conformance-interop", version)]
struct Cli {
    /// Path to the conformance guest component (`*.component.wasm`).
    #[arg(
        long,
        default_value = "conformance/guest/build/conformance-guest.component.wasm"
    )]
    guest: PathBuf,

    /// Directory to write the adapter result documents into.
    #[arg(long, default_value = "conformance/results")]
    out: PathBuf,

    /// Environment/scenario label recorded in the result documents.
    #[arg(long, default_value = "loopback")]
    environment: String,

    /// The Node binary that drives the jco-node and jco-browser peers. Must
    /// be JSPI-capable (Node 24+) for the jco-node peer. Overridable so CI
    /// can point at a specific toolchain node.
    #[arg(long, env = "CONFORMANCE_NODE", default_value = "node")]
    node_bin: String,

    /// Path to the jco-node adapter's `run-node.mjs`.
    #[arg(long, default_value = "conformance/adapters/jco/run-node.mjs")]
    jco_run_node: PathBuf,

    /// Path to the jco-browser adapter's `run-browser.mjs`.
    #[arg(long, default_value = "conformance/adapters/jco/run-browser.mjs")]
    jco_run_browser: PathBuf,

    /// Path to the `conformance-reference-peer` binary.
    #[arg(long, default_value = "target/release/conformance-reference-peer")]
    reference_peer: PathBuf,

    /// The `wasmtime` binary that drives the wasip3-guest peer (v46+).
    #[arg(long, env = "CONFORMANCE_WASMTIME", default_value = "wasmtime")]
    wasmtime_bin: String,

    /// Path to the fully composed wasip3-guest component
    /// (see `just conformance::build-wasip3`).
    #[arg(
        long,
        default_value = "conformance/adapters/wasip3/build/conformance-wasip3.composed.wasm"
    )]
    wasip3_component: PathBuf,

    /// Run only these pair target ids (repeatable). When empty, run every pair.
    #[arg(long = "pair")]
    pairs: Vec<String>,

    /// Run only these test ids (repeatable). When empty, run every test.
    #[arg(long = "only")]
    only: Vec<String>,

    /// How many tests to run concurrently within a pair direction. Each test's
    /// peers use their own signaling room and ephemeral ports, so tests are
    /// independent; the default scales conservatively with the available cores
    /// because every attempt boots a peer runtime (Node or headless Chromium)
    /// on the hang-guard clock (see
    /// `conformance_adapter_common::default_jobs_process_heavy`).
    #[arg(long, default_value_t = conformance_adapter_common::default_jobs_process_heavy())]
    jobs: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    conformance_adapter_common::init_tracing();

    let engine = build_engine()?;
    let component = Component::from_file(&engine, &cli.guest)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("loading guest component {}", cli.guest.display()))?;

    let server = conformance_adapter_common::start_signaling_server().await?;
    let base_url = server.base_url();

    let directions = [
        // Reference anchoring: each target against the non-wasm reference
        // peer, both orders.
        Direction {
            target: "wasmtime-x-reference",
            offerer: Side::Wasmtime,
            answerer: Side::Reference,
            tests: TWO_PEER_TESTS,
        },
        Direction {
            target: "reference-x-wasmtime",
            offerer: Side::Reference,
            answerer: Side::Wasmtime,
            tests: TWO_PEER_TESTS,
        },
        Direction {
            target: "jco-node-x-reference",
            offerer: Side::JcoNode,
            answerer: Side::Reference,
            tests: TWO_PEER_TESTS,
        },
        Direction {
            target: "reference-x-jco-node",
            offerer: Side::Reference,
            answerer: Side::JcoNode,
            tests: TWO_PEER_TESTS,
        },
        Direction {
            target: "jco-browser-x-reference",
            offerer: Side::JcoBrowser,
            answerer: Side::Reference,
            tests: HANDSHAKE_ONLY,
        },
        Direction {
            target: "reference-x-jco-browser",
            offerer: Side::Reference,
            answerer: Side::JcoBrowser,
            tests: HANDSHAKE_ONLY,
        },
        Direction {
            target: "wasip3-guest-x-reference",
            offerer: Side::Wasip3,
            answerer: Side::Reference,
            tests: TWO_PEER_TESTS,
        },
        Direction {
            target: "reference-x-wasip3-guest",
            offerer: Side::Reference,
            answerer: Side::Wasip3,
            tests: TWO_PEER_TESTS,
        },
        // The reference self-pair: validates the reference peer itself and
        // backs the `reference` matrix row (its Shadow-lab sibling is the
        // `reference-shadow.json` document).
        Direction {
            target: "reference",
            offerer: Side::Reference,
            answerer: Side::Reference,
            tests: TWO_PEER_TESTS,
        },
        // Retained implementation-vs-implementation pairs.
        Direction {
            target: "wasmtime-x-jco-node",
            offerer: Side::Wasmtime,
            answerer: Side::JcoNode,
            tests: TWO_PEER_TESTS,
        },
        Direction {
            target: "jco-node-x-wasmtime",
            offerer: Side::JcoNode,
            answerer: Side::Wasmtime,
            tests: TWO_PEER_TESTS,
        },
        Direction {
            target: "wasmtime-x-wasip3-guest",
            offerer: Side::Wasmtime,
            answerer: Side::Wasip3,
            tests: TWO_PEER_TESTS,
        },
        Direction {
            target: "wasip3-guest-x-wasmtime",
            offerer: Side::Wasip3,
            answerer: Side::Wasmtime,
            tests: TWO_PEER_TESTS,
        },
    ];

    let room_seq = AtomicU64::new(0);
    for direction in &directions {
        if !cli.pairs.is_empty() && !cli.pairs.iter().any(|p| p == direction.target) {
            continue;
        }
        // A global --only filter that misses this direction's subset entirely
        // (e.g. a non-handshake test with the browser smoke pairs) skips the
        // direction rather than erroring the run.
        if !cli.only.is_empty()
            && !direction
                .tests
                .iter()
                .any(|t| cli.only.iter().any(|o| o == t))
        {
            continue;
        }
        eprintln!("== interop {} ==", direction.target);
        let results = run_corpus(direction.tests, &cli.only, cli.jobs, |test_id| {
            run_interop_test(
                &cli, &engine, &component, &base_url, direction, test_id, &room_seq,
            )
        })
        .await?;

        let report = AdapterReport {
            target: direction.target.to_string(),
            environment: cli.environment.clone(),
            results,
        };
        write_report(&cli.out, direction.target, &report)?;
    }

    server.shutdown().await;
    Ok(())
}
