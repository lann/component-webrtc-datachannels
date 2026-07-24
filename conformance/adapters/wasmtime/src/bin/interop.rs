//! Cross-runtime interop orchestrator for the conformance suite.
//!
//! It runs the interop pairs — `wasmtime` <-> `jco-node`, `wasmtime` <->
//! `jco-browser`, and `wasmtime` <-> `wasip3-guest` — each in both orders. One
//! peer is a native wasmtime guest instance (provisioned by
//! [`conformance_adapter_wasmtime`]) and the other is driven out-of-process:
//! the jco-node peer via `conformance/adapters/jco/run-node.mjs --interop`,
//! the jco-browser peer via `run-browser.mjs --interop` (one headless-Chromium
//! instance per test), and the wasip3-guest peer via `wasmtime run` over the
//! fully composed component ([`conformance_adapter_wasip3::Wasip3Peer`]). Both
//! peers of a pair share one in-process `conformance-signalingd` room and
//! connect over a real WebRTC data channel, so a green result proves the two
//! runtimes are genuinely interoperable — not merely that each passes against
//! itself.
//!
//! It writes one adapter result document per direction
//! (`wasmtime-x-jco-node.json`, `jco-node-x-wasmtime.json`, and so on) that
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

/// One direction of a pair: which runtime the non-wasmtime peer runs on, which
/// role the wasmtime peer plays (the other peer plays the opposite), and the
/// target id the result document records.
struct Direction {
    target: &'static str,
    peer: PeerKind,
    wasmtime_role: Role,
}

/// The runtime driving the non-wasmtime peer of a pair.
#[derive(Clone, Copy)]
enum PeerKind {
    /// The jco-node host, via `run-node.mjs --interop`.
    JcoNode,
    /// The jco host inside headless Chromium, via `run-browser.mjs --interop`.
    JcoBrowser,
    /// The composed wasip3-guest component, via `wasmtime run`.
    Wasip3,
}

/// The non-wasmtime peer's role, given the wasmtime peer's role.
fn peer_role(wasmtime_role: Role) -> &'static str {
    match wasmtime_role {
        Role::Offerer => "answerer",
        Role::Answerer => "offerer",
        Role::Both => "answerer",
    }
}

/// Run the jco-node peer for one test/role/room via `run-node.mjs --interop`,
/// parsing its single-line JSON `test-result` from stdout.
async fn run_jco_peer(
    cli: &Cli,
    base_url: &str,
    test_id: &str,
    room: &str,
    role: &str,
    count: u32,
    size: u32,
) -> Result<TestOutcome> {
    let mut command = tokio::process::Command::new(&cli.node_bin);
    command
        .arg("--experimental-wasm-jspi")
        .arg(&cli.jco_run_node)
        .arg("--interop")
        .args(["--server", base_url])
        .args(["--test", test_id])
        .args(["--room", room])
        .args(["--role", role])
        .args(["--message-count", &count.to_string()])
        .args(["--message-size", &size.to_string()]);
    run_peer_command(command, &format!("jco-node peer {test_id}/{role}")).await
}

/// Run the jco-browser peer for one test/role/room via
/// `run-browser.mjs --interop`, parsing its single-line JSON `test-result`
/// from stdout. Each invocation launches its own headless Chromium; a
/// Chrome/Chromium binary must be discoverable (`CHROME_PATH` or a standard
/// location — see the adapter).
async fn run_jco_browser_peer(
    cli: &Cli,
    base_url: &str,
    test_id: &str,
    room: &str,
    role: &str,
    count: u32,
    size: u32,
) -> Result<TestOutcome> {
    let mut command = tokio::process::Command::new(&cli.node_bin);
    command
        .arg(&cli.jco_run_browser)
        .arg("--interop")
        .args(["--server", base_url])
        .args(["--test", test_id])
        .args(["--room", room])
        .args(["--role", role])
        .args(["--message-count", &count.to_string()])
        .args(["--message-size", &size.to_string()]);
    run_peer_command(command, &format!("jco-browser peer {test_id}/{role}")).await
}

/// Run the non-wasmtime peer for one test/role/room, parsing its single-line
/// JSON `test-result` from stdout.
#[allow(clippy::too_many_arguments)]
async fn run_peer(
    cli: &Cli,
    kind: PeerKind,
    base_url: &str,
    test_id: &str,
    room: &str,
    role: &str,
    count: u32,
    size: u32,
) -> Result<TestOutcome> {
    match kind {
        PeerKind::JcoNode => run_jco_peer(cli, base_url, test_id, room, role, count, size).await,
        PeerKind::JcoBrowser => {
            run_jco_browser_peer(cli, base_url, test_id, room, role, count, size).await
        }
        PeerKind::Wasip3 => {
            let peer = Wasip3Peer {
                wasmtime_bin: cli.wasmtime_bin.clone(),
                component: cli.wasip3_component.clone(),
            };
            peer.run(base_url, test_id, room, role, count, size).await
        }
    }
}

/// Fold results into the raw offerer/answerer order the [`fold_two`] helper
/// expects, then classify: any fail loses, else any skip, else pass.
fn fold_pair(wasmtime_role: Role, wasmtime: TestOutcome, peer: TestOutcome) -> TestOutcome {
    match wasmtime_role {
        Role::Offerer | Role::Both => fold_two(wasmtime, peer),
        Role::Answerer => fold_two(peer, wasmtime),
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
    let peer_role = peer_role(direction.wasmtime_role);

    run_test(test_id, TEST_TIMEOUT, async || {
        let room = format!(
            "interop-{}-{}-{}",
            direction.target,
            test_id,
            room_seq.fetch_add(1, Ordering::SeqCst)
        );

        let wasmtime_peer = run_instance(
            engine,
            component,
            test_id,
            make_config(direction.wasmtime_role, base_url, &room, count, size),
        );
        let other_peer = run_peer(
            cli,
            direction.peer,
            base_url,
            test_id,
            &room,
            peer_role,
            count,
            size,
        );

        let (wasmtime_result, peer_result) = tokio::join!(wasmtime_peer, other_peer);
        let wasmtime_result = wasmtime_result.context("wasmtime peer")?;
        let peer_result = peer_result.context("interop peer")?;
        Ok(fold_pair(
            direction.wasmtime_role,
            wasmtime_result,
            peer_result,
        ))
    })
    .await
}

/// Run the wasmtime <-> jco-node interop pair in both orders.
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

    /// The Node binary that drives the jco-node peer. Must be JSPI-capable
    /// (Node 24+). Overridable so CI can point at a specific toolchain node.
    #[arg(long, env = "CONFORMANCE_NODE", default_value = "node")]
    node_bin: String,

    /// Path to the jco-node adapter's `run-node.mjs`.
    #[arg(long, default_value = "conformance/adapters/jco/run-node.mjs")]
    jco_run_node: PathBuf,

    /// Path to the jco-browser adapter's `run-browser.mjs`.
    #[arg(long, default_value = "conformance/adapters/jco/run-browser.mjs")]
    jco_run_browser: PathBuf,

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
    /// independent; the default keeps the number of concurrent peer processes
    /// modest.
    #[arg(long, default_value_t = 4)]
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
        Direction {
            target: "wasmtime-x-jco-node",
            peer: PeerKind::JcoNode,
            wasmtime_role: Role::Offerer,
        },
        Direction {
            target: "jco-node-x-wasmtime",
            peer: PeerKind::JcoNode,
            wasmtime_role: Role::Answerer,
        },
        Direction {
            target: "wasmtime-x-jco-browser",
            peer: PeerKind::JcoBrowser,
            wasmtime_role: Role::Offerer,
        },
        Direction {
            target: "jco-browser-x-wasmtime",
            peer: PeerKind::JcoBrowser,
            wasmtime_role: Role::Answerer,
        },
        Direction {
            target: "wasmtime-x-wasip3-guest",
            peer: PeerKind::Wasip3,
            wasmtime_role: Role::Offerer,
        },
        Direction {
            target: "wasip3-guest-x-wasmtime",
            peer: PeerKind::Wasip3,
            wasmtime_role: Role::Answerer,
        },
    ];

    let room_seq = AtomicU64::new(0);
    for direction in &directions {
        if !cli.pairs.is_empty() && !cli.pairs.iter().any(|p| p == direction.target) {
            continue;
        }
        eprintln!("== interop {} ==", direction.target);
        let results = run_corpus(TWO_PEER_TESTS, &cli.only, cli.jobs, |test_id| {
            run_interop_test(
                &cli, &engine, &component, &base_url, direction, test_id, &room_seq,
            )
        })
        .await;

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
