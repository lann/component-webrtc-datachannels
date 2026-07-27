//! Conformance adapter for the wasmtime (native `webrtc-rs`) target.
//!
//! It runs the shared conformance guest component against the wasmtime host
//! ([`wasmtime_webrtc_datachannels`], provisioned by
//! [`conformance_adapter_wasmtime`]) and emits an adapter result document the
//! conformance runner consumes. For each registered test it:
//!
//! - decides how many guest instances the test needs — a single `both` instance
//!   stands up both peers in-process (no external signaling) for the
//!   peer-connection API tests, or two instances (an `offerer` and an
//!   `answerer`) share one signaling room for the behavioral/interop tests;
//! - provisions each instance's store with the wasmtime WebRTC host (loopback
//!   ICE enabled so two same-host peers pair) and a native HTTP `mailbox` host
//!   backed by an in-process `conformance-signalingd`;
//! - drives the guest's exported `run-test` to a WIT-observable outcome and
//!   folds the per-instance results into one raw `pass`/`fail`/`skip`.
//!
//! The guest owns every assertion; the adapter only provisions, orchestrates,
//! and records.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Parser;
use wasmtime::component::Component;
use wasmtime::Engine;

use conformance_adapter_common::{
    fold_two, params_for, plan_for, run_corpus, write_report, AdapterReport, Plan, RawResult,
    TestOutcome, LOOPBACK_TEST_TIMEOUT, TESTS,
};
use conformance_adapter_wasmtime::{build_engine, make_config, run_instance, Role};

// ----- guest orchestration --------------------------------------------------

/// Run a two-peer test: an offerer and an answerer share `room`, driven
/// concurrently so each can consume the other's mailbox as it publishes.
async fn run_two_peer(
    engine: &Engine,
    component: &Component,
    test_id: &str,
    base_url: &str,
    room: &str,
    count: u32,
    size: u32,
) -> Result<TestOutcome> {
    let offerer = run_instance(
        engine,
        component,
        test_id,
        make_config(Role::Offerer, base_url, room, count, size),
    );
    let answerer = run_instance(
        engine,
        component,
        test_id,
        make_config(Role::Answerer, base_url, room, count, size),
    );
    let (offerer, answerer) = futures::join!(offerer, answerer);
    Ok(fold_two(offerer?, answerer?))
}

/// The hang guard for one test (see [`LOOPBACK_TEST_TIMEOUT`]).
const TEST_TIMEOUT: Duration = LOOPBACK_TEST_TIMEOUT;

/// Run one test to a raw result (single attempt; no retries).
async fn run_test(
    engine: &Engine,
    component: &Component,
    base_url: &str,
    test_id: &str,
    room_seq: &AtomicU64,
) -> RawResult {
    let (count, size) = params_for(test_id);

    conformance_adapter_common::run_test(test_id, TEST_TIMEOUT, async || {
        let room = format!(
            "conf-{}-{}",
            test_id,
            room_seq.fetch_add(1, Ordering::SeqCst)
        );
        match plan_for(test_id) {
            Plan::TwoPeer => {
                run_two_peer(engine, component, test_id, base_url, &room, count, size).await
            }
            Plan::InProcess => {
                run_instance(
                    engine,
                    component,
                    test_id,
                    make_config(Role::Both, base_url, &room, count, size),
                )
                .await
            }
        }
    })
    .await
}

// ----- CLI ------------------------------------------------------------------

/// Run the conformance guest against the wasmtime host and emit a result doc.
#[derive(Debug, Parser)]
#[command(name = "conformance-adapter-wasmtime", version)]
struct Cli {
    /// Path to the conformance guest component (`*.component.wasm`).
    #[arg(
        long,
        default_value = "conformance/guest/build/conformance-guest.component.wasm"
    )]
    guest: PathBuf,

    /// Directory to write the adapter result document (`<target>.json`) into.
    #[arg(long, default_value = "conformance/results")]
    out: PathBuf,

    /// Target id, matching the manifest `[target].id`.
    #[arg(long, default_value = "wasmtime")]
    target: String,

    /// Environment/scenario label recorded in the result document.
    #[arg(long, default_value = "loopback")]
    environment: String,

    /// Run only these test ids (repeatable). When empty, run every test.
    #[arg(long = "only")]
    only: Vec<String>,

    /// How many tests to run concurrently. Each test's peers use their own
    /// signaling room and ephemeral ports, so tests are independent; the
    /// default scales with the cores available to this process (see
    /// `conformance_adapter_common::default_jobs`).
    #[arg(long, default_value_t = conformance_adapter_common::default_jobs())]
    jobs: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    conformance_adapter_common::init_tracing();

    // Shrink the host's inbound-buffer bound so the `receive-buffer-overflow`
    // probe overflows it with a small flood (the host pumps run in this
    // process, so the process environment is the knob).
    std::env::set_var(
        conformance_adapter_common::MAX_INBOUND_BUFFER_ENV,
        conformance_adapter_common::CONFORMANCE_MAX_INBOUND_BUFFER_BYTES.to_string(),
    );

    let engine = build_engine()?;
    let component = Component::from_file(&engine, &cli.guest)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("loading guest component {}", cli.guest.display()))?;

    // Start the signaling server in-process on an ephemeral localhost port.
    let server = conformance_adapter_common::start_signaling_server().await?;
    let base_url = server.base_url();

    // Verify the registered corpus matches the guest's list-tests export
    // before running anything, so a drifted mirror fails fast and loudly.
    let guest_tests = conformance_adapter_wasmtime::list_guest_tests(&engine, &component).await?;
    conformance_adapter_common::verify_corpus(&guest_tests, TESTS)?;

    let room_seq = AtomicU64::new(0);
    let results = run_corpus(TESTS, &cli.only, cli.jobs, |test_id| {
        run_test(&engine, &component, &base_url, test_id, &room_seq)
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
