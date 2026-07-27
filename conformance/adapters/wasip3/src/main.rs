//! Conformance adapter for the `wasip3-guest` target.
//!
//! The target runs the WebRTC stack entirely in wasm: the shared conformance
//! guest composed (`wac plug`) with the `wasip3-impl` provider, an in-guest
//! `wasi:http` mailbox client, and a CLI driver exporting an async
//! `wasi:cli/run`. For each registered test this adapter:
//!
//! - decides how many guest instances the test needs — a single `both` instance
//!   stands up both peers inside one `wasmtime run` (no external signaling) for
//!   the peer-connection API tests, or two `wasmtime run` processes (an
//!   `offerer` and an `answerer`) share one signaling room for the behavioral
//!   tests, connecting over `wasi:sockets` UDP loopback across processes;
//! - parses each process's single-line JSON `test-result` from stdout and folds
//!   the per-instance results into one raw `pass`/`fail`/`skip`.
//!
//! The guest owns every assertion; the adapter only provisions, orchestrates,
//! and records.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use clap::Parser;

use conformance_adapter_common::{
    fold_two, params_for, plan_for, run_corpus, write_report, AdapterReport, Plan, RawResult,
    LOOPBACK_TEST_TIMEOUT, TESTS,
};
use conformance_adapter_wasip3::Wasip3Peer;

// ----- orchestration --------------------------------------------------------

/// The hang guard for one test (see [`LOOPBACK_TEST_TIMEOUT`]).
const TEST_TIMEOUT: Duration = LOOPBACK_TEST_TIMEOUT;

/// Run one test to a raw result (single attempt; no retries).
async fn run_test(
    peer: &Wasip3Peer,
    base_url: &str,
    test_id: &str,
    room_seq: &AtomicU64,
) -> RawResult {
    let (count, size) = params_for(test_id);

    conformance_adapter_common::run_test(test_id, TEST_TIMEOUT, async || {
        let room = format!(
            "wasip3-{}-{}",
            test_id,
            room_seq.fetch_add(1, Ordering::SeqCst)
        );
        match plan_for(test_id) {
            Plan::TwoPeer => {
                let offerer = peer.run(base_url, test_id, &room, "offerer", count, size);
                let answerer = peer.run(base_url, test_id, &room, "answerer", count, size);
                let (offerer, answerer) = tokio::join!(offerer, answerer);
                Ok(fold_two(offerer?, answerer?))
            }
            Plan::InProcess => {
                peer.run(base_url, test_id, &room, "both", count, size)
                    .await
            }
        }
    })
    .await
}

// ----- CLI ------------------------------------------------------------------

/// Run the composed wasip3-guest component under `wasmtime run` and emit a
/// result doc.
#[derive(Debug, Parser)]
#[command(name = "conformance-adapter-wasip3", version)]
struct Cli {
    /// Path to the fully composed component (guest + provider + mailbox +
    /// driver; see `just conformance::build-wasip3`).
    #[arg(
        long,
        default_value = "conformance/adapters/wasip3/build/conformance-wasip3.composed.wasm"
    )]
    component: PathBuf,

    /// Directory to write the adapter result document (`<target>.json`) into.
    #[arg(long, default_value = "conformance/results")]
    out: PathBuf,

    /// Target id, matching the manifest `[target].id`.
    #[arg(long, default_value = "wasip3-guest")]
    target: String,

    /// Environment/scenario label recorded in the result document.
    #[arg(long, default_value = "loopback")]
    environment: String,

    /// The `wasmtime` binary that runs the composed component (v46+).
    #[arg(long, env = "CONFORMANCE_WASMTIME", default_value = "wasmtime")]
    wasmtime_bin: String,

    /// Run only these test ids (repeatable). When empty, run every test.
    #[arg(long = "only")]
    only: Vec<String>,

    /// How many tests to run concurrently. Each test's peers are separate
    /// `wasmtime run` processes with their own signaling room and ephemeral
    /// ports, so tests are independent; the default scales with the cores
    /// available to this process (see
    /// `conformance_adapter_common::default_jobs`).
    #[arg(long, default_value_t = conformance_adapter_common::default_jobs())]
    jobs: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    conformance_adapter_common::init_tracing();

    anyhow::ensure!(
        cli.component.exists(),
        "composed component {} not found (run `just conformance::build-wasip3`)",
        cli.component.display()
    );
    let peer = Wasip3Peer {
        wasmtime_bin: cli.wasmtime_bin.clone(),
        component: cli.component.clone(),
    };

    // Start the signaling server in-process on an ephemeral localhost port.
    let server = conformance_adapter_common::start_signaling_server().await?;
    let base_url = server.base_url();

    // Verify the registered corpus matches the guest's list-tests export
    // before running anything, so a drifted mirror fails fast and loudly.
    let guest_tests = peer.list_tests().await?;
    conformance_adapter_common::verify_corpus(&guest_tests, TESTS)?;

    let room_seq = AtomicU64::new(0);
    let results = run_corpus(TESTS, &cli.only, cli.jobs, |test_id| {
        run_test(&peer, &base_url, test_id, &room_seq)
    })
    .await?;

    server.shutdown().await;

    let report = AdapterReport {
        target: cli.target.clone(),
        environment: cli.environment,
        results,
    };
    write_report(&cli.out, &cli.target, &report)?;

    Ok(())
}
