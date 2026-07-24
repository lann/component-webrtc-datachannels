//! Shared building blocks for the wasip3-guest conformance adapter and the
//! cross-runtime interop orchestrator.
//!
//! The wasip3-guest target runs the conformance suite entirely in wasm: the
//! shared conformance guest is composed (`wac plug`) with the `wasip3-impl`
//! provider (which exports `connections` over WASIp3 `wasi:sockets`), an
//! in-guest `wasi:http` mailbox client, and a CLI driver that exports an async
//! `wasi:cli/run`. One `wasmtime run` invocation of that composed component
//! runs one guest instance of one test and reports its raw `test-result` as a
//! single JSON line on stdout.
//!
//! This library provides [`Wasip3Peer`] — the primitive to run one such
//! instance and parse its outcome. The adapter binary layers the full-corpus
//! orchestration on top; the `conformance-interop` binary (in
//! `adapters/wasmtime`) reuses [`Wasip3Peer`] to drive the wasip3 half of the
//! `wasmtime` <-> `wasip3-guest` interop pair.

use std::path::PathBuf;

use anyhow::Result;
use conformance_adapter_common::{run_peer_command, TestOutcome};

/// A runner for one wasip3-guest peer: the composed component plus the
/// `wasmtime` binary that executes it.
pub struct Wasip3Peer {
    /// The `wasmtime` binary (v46+; must support `-W component-model-async` and
    /// `-S p3`).
    pub wasmtime_bin: String,
    /// The fully composed component (guest + provider + mailbox + driver).
    pub component: PathBuf,
}

impl Wasip3Peer {
    /// Run one guest instance of `test_id` as `role` against `room` on the
    /// signaling server at `server`, parsing the driver's single-line JSON
    /// `test-result` from stdout.
    ///
    /// The flags mirror `just examples::test-webrtc-composed` plus `-S http` for the
    /// in-guest mailbox client: the component-model async ABI, the WASIp3 host
    /// APIs, and network access for the provider's `wasi:sockets` UDP and the
    /// mailbox's outgoing HTTP.
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        &self,
        server: &str,
        test_id: &str,
        room: &str,
        role: &str,
        count: u32,
        size: u32,
    ) -> Result<TestOutcome> {
        let mut command = tokio::process::Command::new(&self.wasmtime_bin);
        // Surface host-side wasi:http diagnostics: `wasmtime-wasi-http` logs the
        // underlying hyper error at `warn` before collapsing it into the opaque
        // `ErrorCode::HttpProtocolError` the guest sees. Peer stderr is streamed
        // to the orchestrator's (and CI's) log as it arrives, so these lines
        // survive even a hang-guard kill. An explicit WASMTIME_LOG in the
        // environment wins.
        if std::env::var_os("WASMTIME_LOG").is_none() {
            command.env("WASMTIME_LOG", "wasmtime_wasi_http=warn");
        }
        command
            .arg("run")
            .args(["-W", "component-model-async=y"])
            .args([
                "-S",
                "cli",
                "-S",
                "p3",
                "-S",
                "http",
                "-S",
                "inherit-network",
            ])
            .args([
                "--env",
                &format!(
                    "{}={}",
                    conformance_adapter_common::MAX_INBOUND_BUFFER_ENV,
                    conformance_adapter_common::CONFORMANCE_MAX_INBOUND_BUFFER_BYTES
                ),
            ])
            .arg(&self.component)
            .args(["--test", test_id])
            .args(["--role", role])
            .args(["--server", server])
            .args(["--room", room])
            .args(["--message-count", &count.to_string()])
            .args(["--message-size", &size.to_string()]);
        run_peer_command(command, &format!("wasip3-guest peer {test_id}/{role}")).await
    }
}
