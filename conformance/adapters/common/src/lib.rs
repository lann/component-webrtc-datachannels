//! Target-independent building blocks shared by the native conformance
//! adapters and environment executors (the wasmtime and wasip3-guest adapters,
//! the cross-runtime interop orchestrator, and this crate's `conformance-netns`
//! and `conformance-shadow` environment-executor bins).
//!
//! Every adapter follows the same shape — pick each test's orchestration plan,
//! run its guest instances (in-process or as peer subprocesses), and record
//! raw `pass`/`fail`/`skip` outcomes in
//! an adapter result document the conformance runner classifies. This crate
//! owns that shared shape:
//!
//! - the WIT-observable [`TestOutcome`] with [`fold_two`] / [`parse_outcome`],
//! - the raw result document types ([`RawResult`], [`RawStatus`],
//!   [`AdapterReport`]) and [`write_report`],
//! - the test registry ([`TESTS`], [`TWO_PEER_TESTS`]) with per-test
//!   orchestration plans ([`plan_for`]) and message parameters ([`params_for`]),
//! - the peer-subprocess invocation ([`run_peer_command`]) with its subtle but
//!   load-bearing process plumbing,
//! - the single-attempt test runner with its hang guard ([`run_test`]),
//! - the bounded-concurrency corpus runner ([`run_corpus`]),
//! - the in-process signaling server startup ([`start_signaling_server`]),
//! - the per-target peer command templates the environment executors share
//!   ([`peer_command`]), and
//! - the netns-lab topology and its netns/nftables/coturn provisioning ([`lab`]).

pub mod lab;
pub mod peer_command;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use serde::Serialize;
use serde_json::Value;

// ----- tracing ----------------------------------------------------------------

/// Install the stderr `tracing` subscriber every adapter/executor binary uses.
///
/// The filter comes from `RUST_LOG` when set, defaulting to `warn` plus `info`
/// for this crate's `conformance` target, so peer-failure diagnostics and the
/// phase markers (guest instance / mailbox / peer-process progress) are visible
/// in CI logs without any configuration — in particular, when an attempt trips
/// [`run_test`]'s hang guard, the last phase marker identifies the hung phase.
/// Call once at the top of `main`.
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn,conformance=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
}

// ----- test outcomes ---------------------------------------------------------

/// The WIT-observable outcome of one guest instance: the target-independent
/// mirror of the guest's `test-result` (`conformance/wit/world.wit`).
#[derive(Debug, Clone)]
pub enum TestOutcome {
    Pass,
    Fail(String),
    Skipped(String),
}

impl TestOutcome {
    /// The single-line JSON `test-result` shape peer processes print to stdout
    /// and [`parse_outcome`] parses back: `{ "tag": ..., "val"? }`.
    pub fn to_json(&self) -> Value {
        match self {
            TestOutcome::Pass => serde_json::json!({ "tag": "pass" }),
            TestOutcome::Fail(detail) => serde_json::json!({ "tag": "fail", "val": detail }),
            TestOutcome::Skipped(reason) => {
                serde_json::json!({ "tag": "skipped", "val": reason })
            }
        }
    }
}

/// Map a peer's `test-result` JSON value (`{ "tag": ..., "val"? }`) to a
/// [`TestOutcome`].
pub fn parse_outcome(value: &Value) -> Result<TestOutcome> {
    let tag = value
        .get("tag")
        .and_then(Value::as_str)
        .context("result missing tag")?;
    let detail = || {
        value
            .get("val")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Ok(match tag {
        "pass" => TestOutcome::Pass,
        "fail" => TestOutcome::Fail(detail()),
        "skipped" => TestOutcome::Skipped(detail()),
        other => anyhow::bail!("unknown result tag {other:?}"),
    })
}

/// Fold two per-instance outcomes into one: any fail loses, else any skip,
/// else pass.
pub fn fold_two(offerer: TestOutcome, answerer: TestOutcome) -> TestOutcome {
    match (offerer, answerer) {
        (TestOutcome::Fail(a), TestOutcome::Fail(b)) => {
            TestOutcome::Fail(format!("offerer: {a}; answerer: {b}"))
        }
        (TestOutcome::Fail(a), _) => TestOutcome::Fail(format!("offerer: {a}")),
        (_, TestOutcome::Fail(b)) => TestOutcome::Fail(format!("answerer: {b}")),
        (TestOutcome::Skipped(a), _) => TestOutcome::Skipped(a),
        (_, TestOutcome::Skipped(b)) => TestOutcome::Skipped(b),
        (TestOutcome::Pass, TestOutcome::Pass) => TestOutcome::Pass,
    }
}

// ----- adapter result document -----------------------------------------------

/// The raw status vocabulary the runner consumes (`runner/src/results.rs`).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RawStatus {
    Pass,
    Fail,
    Skip,
}

/// One raw per-test outcome (`runner/src/results.rs::RawResult`).
#[derive(Debug, Clone, Serialize)]
pub struct RawResult {
    pub test_id: String,
    pub status: RawStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The adapter result document (`runner/src/results.rs::AdapterReport`).
#[derive(Debug, Clone, Serialize)]
pub struct AdapterReport {
    pub target: String,
    pub environment: String,
    pub results: Vec<RawResult>,
}

/// Write `report` to `<out_dir>/<file_stem>.json`, creating `out_dir` as
/// needed, and log the path.
pub fn write_report(out_dir: &Path, file_stem: &str, report: &AdapterReport) -> Result<PathBuf> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("creating results dir {}", out_dir.display()))?;
    let out_path = out_dir.join(format!("{file_stem}.json"));
    std::fs::write(&out_path, serde_json::to_string_pretty(report)?)
        .with_context(|| format!("writing {}", out_path.display()))?;
    eprintln!("wrote {}", out_path.display());
    Ok(out_path)
}

// ----- test registry & planning ----------------------------------------------

/// The registry of test ids, mirroring `conformance/tests.toml`.
pub const TESTS: &[&str] = &[
    "label-round-trip",
    "binary-message",
    "text-message",
    "message-boundaries",
    "zero-length-message",
    "large-message",
    "ordering",
    "payload-integrity",
    "concurrent-send-receive",
    "send-via-stream",
    "receive-via-stream",
    "receive-via-stream-once",
    "post-close-send",
    "receive-buffer-overflow",
    "max-retransmits-accepted",
    "error-invalid-signaling",
    "error-closed",
    "error-timed-out",
    "peer-offer-answer",
    "peer-create-data-channel",
    "peer-local-ice-candidates",
    "peer-add-ice-candidate",
    "peer-wait-connected",
    "peer-wait-connected-latch",
    "peer-streams-once",
    "post-close-signaling",
    "peer-close-releases",
    "peer-invalid-sdp",
    "interop-handshake",
];

/// The two-peer behavioral subset of [`TESTS`]: the tests whose outcome depends
/// on a working data channel between two independent peers. This is the corpus
/// the interop pairs and the netns lab run — the peer-connection API,
/// error-taxonomy, and streaming tests are in-process (single-runtime), so
/// they exercise no runtime boundary or candidate path.
pub const TWO_PEER_TESTS: &[&str] = &[
    "label-round-trip",
    "binary-message",
    "text-message",
    "message-boundaries",
    "zero-length-message",
    "large-message",
    "ordering",
    "payload-integrity",
    "concurrent-send-receive",
    "max-retransmits-accepted",
    "interop-handshake",
];

/// How a test is orchestrated across guest instances.
pub enum Plan {
    /// A single `both` instance stands up both peers in-process (no signaling).
    InProcess,
    /// A single instance that the guest reports `skipped` regardless of role.
    /// (No registered test currently uses this plan; it remains for tests a
    /// future corpus cannot run on any target.)
    Skip,
    /// Two instances — an offerer and an answerer — share one signaling room.
    TwoPeer,
}

/// The orchestration plan for a test id. The plans are target-independent: the
/// guest decides what each test does with the role it is given.
pub fn plan_for(test_id: &str) -> Plan {
    match test_id {
        "peer-offer-answer"
        | "peer-create-data-channel"
        | "peer-local-ice-candidates"
        | "peer-add-ice-candidate"
        | "peer-wait-connected"
        | "peer-wait-connected-latch"
        | "peer-streams-once"
        | "post-close-signaling"
        | "peer-close-releases"
        | "peer-invalid-sdp"
        | "error-invalid-signaling"
        | "error-closed"
        | "error-timed-out"
        | "post-close-send"
        | "receive-buffer-overflow"
        | "send-via-stream"
        | "receive-via-stream"
        | "receive-via-stream-once" => Plan::InProcess,
        _ => Plan::TwoPeer,
    }
}

/// The `(message-count, message-size)` a test runs with.
pub fn params_for(test_id: &str) -> (u32, u32) {
    match test_id {
        "large-message" => (1, 16384),
        // A 1 MiB flood: twice the [`CONFORMANCE_MAX_INBOUND_BUFFER_BYTES`]
        // bound the adapters configure, so the receiving side must overflow.
        "receive-buffer-overflow" => (64, 16384),
        "message-boundaries"
        | "ordering"
        | "payload-integrity"
        | "concurrent-send-receive"
        | "interop-handshake" => (16, 512),
        _ => (4, 256),
    }
}

/// The inbound-buffer bound (in bytes) the conformance adapters configure
/// through the implementations' `WEBRTC_MAX_INBOUND_BUFFER_BYTES` knob
/// ([`MAX_INBOUND_BUFFER_ENV`]): small enough that the
/// `receive-buffer-overflow` probe overflows it with a ~1 MiB flood instead of
/// flooding the default 8 MiB bound (which starves concurrently running tests
/// of the corpus).
pub const CONFORMANCE_MAX_INBOUND_BUFFER_BYTES: u32 = 512 * 1024;

/// The environment variable naming the implementations' inbound-buffer bound.
pub const MAX_INBOUND_BUFFER_ENV: &str = "WEBRTC_MAX_INBOUND_BUFFER_BYTES";

// ----- peer subprocess invocation --------------------------------------------

/// Run one peer subprocess to a [`TestOutcome`], parsing its single-line JSON
/// `test-result` from stdout. `label` names the peer in error details and in
/// the forwarded-stderr prefix.
///
/// This owns the subtle process plumbing every out-of-process peer needs:
/// stdin is closed; the child is killed if this future is dropped
/// (`kill_on_drop`), so an attempt abandoned by [`run_test`]'s timeout reaps
/// its peers instead of leaking them to hold TURN allocations and CPU; and the
/// child's stderr is **streamed** to the orchestrator's stderr line by line as
/// it arrives, each line prefixed `[{label}]`. Streaming (rather than
/// capturing and forwarding after exit) is what keeps the diagnostics when the
/// hang guard abandons the attempt: the kill arrives mid-run, and everything
/// the peer printed up to that point — phase markers included — is already on
/// the orchestrator's stderr instead of dying captured in a buffer.
pub async fn run_peer_command(
    mut command: tokio::process::Command,
    label: &str,
) -> Result<TestOutcome> {
    tracing::debug!(target: "conformance", %label, command = ?command.as_std(), "spawning peer");
    let started = std::time::Instant::now();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning {label}"))?;
    tracing::info!(target: "conformance", %label, "peer spawned");
    // The forwarding task is detached: it ends at stderr EOF, whether the child
    // exits on its own or is killed by the hang guard dropping this future.
    let stderr = child.stderr.take().expect("child stderr is piped");
    let stderr_label = label.to_string();
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt as _;
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[{stderr_label}] {line}");
        }
    });
    let output = child
        .wait_with_output()
        .await
        .with_context(|| format!("waiting for {label}"))?;
    let elapsed = started.elapsed();
    if !output.status.success() {
        tracing::warn!(target: "conformance", %label, status = %output.status, ?elapsed, "peer exited nonzero");
        anyhow::bail!("{label} exited with {}", output.status);
    }
    tracing::info!(target: "conformance", %label, ?elapsed, "peer completed");
    let stdout =
        String::from_utf8(output.stdout).with_context(|| format!("{label} stdout not UTF-8"))?;
    parse_result_line(&stdout).with_context(|| format!("reading {label} result"))
}

/// Parse a peer's captured stdout to its [`TestOutcome`]: the last non-empty
/// line is the single-line JSON `test-result` (trailing blank lines, e.g. from
/// output capture, are tolerated).
pub fn parse_result_line(text: &str) -> Result<TestOutcome> {
    let line = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .context("no result line in output")?;
    let value: Value = serde_json::from_str(line.trim())
        .with_context(|| format!("parsing result line {line:?}"))?;
    parse_outcome(&value)
}

/// Convert a folded [`TestOutcome`] into the runner's raw result vocabulary.
pub fn outcome_to_raw(test_id: &str, outcome: TestOutcome) -> RawResult {
    match outcome {
        TestOutcome::Pass => RawResult {
            test_id: test_id.to_string(),
            status: RawStatus::Pass,
            detail: None,
        },
        TestOutcome::Skipped(reason) => RawResult {
            test_id: test_id.to_string(),
            status: RawStatus::Skip,
            detail: Some(reason),
        },
        TestOutcome::Fail(detail) => RawResult {
            test_id: test_id.to_string(),
            status: RawStatus::Fail,
            detail: Some(detail),
        },
    }
}

// ----- single-attempt test runner ----------------------------------------------

/// Run one test to a [`RawResult`] in a single attempt.
///
/// `attempt` runs the test (allocating its own fresh room) to the folded
/// [`TestOutcome`] of its instances. An attempt that outlives `timeout` is
/// dropped — cancelling in-process instances and killing peer subprocesses
/// (see [`run_peer_command`]) — and reported as a failure. The timeout must
/// exceed the host's `wait-connected` timeout so a genuine connection failure
/// surfaces as a WIT outcome rather than tripping this guard. There are no
/// retries: a nondeterministic failure is a real signal and must surface, not
/// be masked by a second attempt.
pub async fn run_test(
    test_id: &str,
    timeout: Duration,
    attempt: impl AsyncFnOnce() -> Result<TestOutcome>,
) -> RawResult {
    let outcome = match tokio::time::timeout(timeout, attempt()).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(err)) => {
            tracing::warn!(target: "conformance", test_id, error = format!("{err:#}"), "adapter error");
            TestOutcome::Fail(format!("adapter error: {err:#}"))
        }
        Err(_) => {
            tracing::warn!(target: "conformance", test_id, ?timeout, "attempt timed out");
            TestOutcome::Fail("attempt timed-out".to_string())
        }
    };
    if let TestOutcome::Fail(detail) = &outcome {
        tracing::warn!(target: "conformance", test_id, %detail, "test failed");
    }
    outcome_to_raw(test_id, outcome)
}

// ----- corpus runner ----------------------------------------------------------

/// Run `tests` (each via `run`, filtered by `only` when non-empty) concurrently,
/// bounded by `jobs`, logging each result as it lands.
///
/// Tests are independent — fresh instances/processes, a fresh room per test —
/// so they can safely overlap; `buffered` preserves the registry order of the
/// results.
pub async fn run_corpus<F, Fut>(
    tests: &[&'static str],
    only: &[String],
    jobs: usize,
    run: F,
) -> Vec<RawResult>
where
    F: Fn(&'static str) -> Fut,
    Fut: Future<Output = RawResult>,
{
    futures::stream::iter(
        tests
            .iter()
            .copied()
            .filter(|test_id| only.is_empty() || only.iter().any(|t| t == test_id)),
    )
    .map(|test_id| {
        let fut = run(test_id);
        async move {
            let result = fut.await;
            eprintln!("{test_id} … {:?}", result.status);
            result
        }
    })
    .buffered(jobs.max(1))
    .collect()
    .await
}

// ----- signaling server -------------------------------------------------------

/// Start the in-process signaling server on an ephemeral localhost port and log
/// its base URL.
pub async fn start_signaling_server() -> Result<conformance_signalingd::RunningServer> {
    let server = conformance_signalingd::spawn(
        "127.0.0.1:0".parse().expect("valid loopback address"),
        conformance_signalingd::Config::default(),
    )
    .await
    .context("starting in-process signaling server")?;
    eprintln!("signaling server ready at {}", server.base_url());
    Ok(server)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_json_round_trips() {
        for outcome in [
            TestOutcome::Pass,
            TestOutcome::Fail("boom".to_string()),
            TestOutcome::Skipped("later".to_string()),
        ] {
            let parsed = parse_outcome(&outcome.to_json()).unwrap();
            match (&outcome, &parsed) {
                (TestOutcome::Pass, TestOutcome::Pass) => {}
                (TestOutcome::Fail(a), TestOutcome::Fail(b)) if a == b => {}
                (TestOutcome::Skipped(a), TestOutcome::Skipped(b)) if a == b => {}
                other => panic!("round trip mismatch: {other:?}"),
            }
        }
        assert!(parse_outcome(&serde_json::json!({ "tag": "nope" })).is_err());
        assert!(parse_outcome(&serde_json::json!({})).is_err());
    }

    #[test]
    fn fold_two_prefers_fail_then_skip() {
        let fail = || TestOutcome::Fail("f".to_string());
        let skip = || TestOutcome::Skipped("s".to_string());
        assert!(matches!(
            fold_two(fail(), TestOutcome::Pass),
            TestOutcome::Fail(d) if d == "offerer: f"
        ));
        assert!(matches!(
            fold_two(skip(), fail()),
            TestOutcome::Fail(d) if d == "answerer: f"
        ));
        assert!(matches!(
            fold_two(skip(), TestOutcome::Pass),
            TestOutcome::Skipped(_)
        ));
        assert!(matches!(
            fold_two(TestOutcome::Pass, TestOutcome::Pass),
            TestOutcome::Pass
        ));
    }

    #[test]
    fn result_line_is_last_non_empty_line() {
        let text = "log noise\n{ \"tag\": \"pass\" }\n\n  \n";
        assert!(matches!(
            parse_result_line(text).unwrap(),
            TestOutcome::Pass
        ));
        assert!(matches!(
            parse_result_line("{\"tag\":\"fail\",\"val\":\"boom\"}").unwrap(),
            TestOutcome::Fail(d) if d == "boom"
        ));
        assert!(parse_result_line("").is_err());
        assert!(parse_result_line("\n  \n").is_err());
        assert!(parse_result_line("not json").is_err());
    }

    #[test]
    fn two_peer_tests_are_registered_two_peer_plans() {
        for test_id in TWO_PEER_TESTS {
            assert!(TESTS.contains(test_id), "{test_id} missing from TESTS");
            assert!(
                matches!(plan_for(test_id), Plan::TwoPeer),
                "{test_id} is not a two-peer plan"
            );
        }
    }
}
