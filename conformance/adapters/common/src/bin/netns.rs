//! The conformance netns-lab environment executor.
//!
//! It runs the two-peer behavioral corpus for one target in one ICE scenario
//! (`lan`, `stun-srflx`, `turn-relay`, `nat-symmetric`; see
//! `conformance/README.md`) over a provisioned network-namespace
//! topology. Each test's two peers run as separate processes, one placed in
//! the offerer namespace and one in the answerer namespace (`ip netns exec`),
//! so their handshake traverses a real routed path — and, for the
//! server-mediated scenarios, is forced through the STUN/TURN server because
//! the router blocks the direct peer-to-peer path (and, for the NAT scenarios,
//! rewrites each peer's address).
//!
//! The signaling server (and, for the server-mediated scenarios, coturn) run
//! in the signaling namespace, reachable from both peers through the router.
//! The result document it writes (`<target>-<scenario>.json`,
//! `environment = <scenario>`) is classified by the conformance runner exactly
//! like any other adapter report.
//!
//! The executor is target-neutral: `--peer-kind` selects how each peer's
//! command line is built (see [`conformance_adapter_common::peer_command`]):
//!
//! - `wasmtime` runs the native `conformance-peer` binary, supporting every
//!   scenario;
//! - `wasip3-guest` runs the fully composed wasip3 conformance component under
//!   `wasmtime run`, binding the in-guest provider to its namespace address
//!   through `WEBRTC_UDP_BIND_ADDR`. The in-guest sans-I/O stack supports no
//!   STUN/TURN, so only the `lan` scenario fits this kind.
//! - `reference` runs the non-wasm reference peer (`node peer.mjs`, libwebrtc
//!   via `@roamhq/wrtc`), supporting every scenario: its ICE flags map onto
//!   `RTCPeerConnection`'s `iceServers` / `iceTransportPolicy`.
//!
//! Requires root (for `ip netns exec`); the lab topology
//! ([`conformance_adapter_common::lab`]) is provisioned and torn down by this
//! binary itself unless `--no-provision` is passed.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Parser;

use conformance_adapter_common::lab::{LabTopology, Scenario};
use conformance_adapter_common::peer_command::{PeerCommand, PeerKind, PeerRole, PeerRun};
use conformance_adapter_common::{
    fold_two, params_for, run_peer_command, run_test, write_report, AdapterReport, RawResult,
    TestOutcome, TWO_PEER_TESTS,
};

/// The hang guard for one test. Lab handshakes (real routing, and a TURN relay
/// for `turn-relay`) are slower to establish than loopback, but the lab runs
/// native peers at low concurrency on a dedicated workstation, so it needs
/// less headroom than the loopback adapters' 90s (which absorbs peer-process
/// startup under 4-wide CI contention).
const TEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Parser)]
#[command(name = "conformance-netns", version)]
struct Cli {
    /// Scenario to run.
    #[arg(long, value_enum)]
    scenario: Scenario,

    /// How each peer's command line is built (which target's peer runs).
    #[arg(long, value_enum, default_value_t = PeerKind::Wasmtime)]
    peer_kind: PeerKind,

    /// Target id recorded in the result document, matching the manifest
    /// `[target].id`. Defaults to the peer kind's conventional target id.
    #[arg(long)]
    target: Option<String>,

    /// Path to the conformance guest component (`*.component.wasm`), used by
    /// the `wasmtime` peer kind.
    #[arg(
        long,
        default_value = "conformance/guest/build/conformance-guest.component.wasm"
    )]
    guest: PathBuf,

    /// The fully composed wasip3 conformance component (guest + provider +
    /// mailbox + driver), used by the `wasip3-guest` peer kind.
    #[arg(
        long,
        default_value = "conformance/adapters/wasip3/build/conformance-wasip3.composed.wasm"
    )]
    component: PathBuf,

    /// The `wasmtime` binary that runs the composed component (v46+), used by
    /// the `wasip3-guest` peer kind.
    #[arg(long, env = "CONFORMANCE_WASMTIME", default_value = "wasmtime")]
    wasmtime_bin: String,

    /// Directory to write the adapter result document into.
    #[arg(long, default_value = "conformance/results")]
    out: PathBuf,

    /// The `conformance-signalingd` binary, started inside the signaling
    /// namespace.
    #[arg(long, default_value = "target/debug/conformance-signalingd")]
    signaling_bin: PathBuf,

    /// The `conformance-peer` binary, launched inside each peer namespace by
    /// the `wasmtime` peer kind.
    #[arg(long, default_value = "target/release/conformance-peer")]
    peer_bin: PathBuf,

    /// The Node binary (a path or a bare name looked up on `PATH`) that runs
    /// the reference peer, used by the `reference` peer kind.
    #[arg(long, env = "CONFORMANCE_NODE", default_value = "node")]
    node_bin: String,

    /// The reference peer script, used by the `reference` peer kind.
    #[arg(long, default_value = "conformance/adapters/reference/peer.mjs")]
    reference_peer: PathBuf,

    /// Base signaling HTTP port in the signaling namespace. Each test uses its
    /// own signaling server on a distinct port (base + attempt index) so tests
    /// stay independent and can run concurrently. The remaining lab parameters
    /// (namespaces, addresses, TURN port and credentials) are the canonical
    /// [`LabTopology`] values the provisioning and peer placement share.
    #[arg(long, default_value_t = 8080)]
    signaling_port: u16,

    /// Provision (and tear down) the lab topology. On by default; pass
    /// `--no-provision` when the lab is already up (e.g. interactive
    /// debugging).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    provision: bool,

    /// Run only these test ids (repeatable). When empty, run the whole corpus.
    #[arg(long = "only")]
    only: Vec<String>,

    /// How many tests to run concurrently. Each test's peers use their own
    /// signaling room, so tests are independent.
    #[arg(long, default_value_t = 2)]
    jobs: usize,
}

/// A live child process wrapped so it is killed when dropped (used for the
/// per-attempt signaling server).
struct Guard {
    child: Option<std::process::Child>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Tears the lab down when dropped, so a provisioned lab is always cleaned up
/// regardless of how the run ends.
struct TeardownOnDrop<'a> {
    topology: &'a LabTopology,
    enabled: bool,
}

impl Drop for TeardownOnDrop<'_> {
    fn drop(&mut self) {
        if self.enabled {
            self.topology.scenario_down();
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    conformance_adapter_common::init_tracing();
    let scenario = cli.scenario;

    // The in-guest sans-I/O stack supports no STUN/TURN servers, so the
    // server-mediated scenarios cannot run a wasip3-guest peer.
    if cli.peer_kind == PeerKind::Wasip3Guest && scenario != Scenario::Lan {
        anyhow::bail!(
            "scenario {:?} is not supported for --peer-kind wasip3-guest: the in-guest \
             sans-I/O stack supports no STUN/TURN servers (only `lan` is supported)",
            scenario.as_str()
        );
    }

    let topology = LabTopology {
        signaling_port: cli.signaling_port,
        ..LabTopology::default()
    };

    // Resolve the peer command up front (absolute paths survive `ip netns exec`
    // running from any cwd).
    let peer_command = PeerCommand::resolve(
        cli.peer_kind,
        &cli.peer_bin,
        &cli.guest,
        &cli.wasmtime_bin,
        &cli.component,
        &cli.node_bin,
        &cli.reference_peer,
    )?;

    if cli.provision {
        topology
            .scenario_up(scenario)
            .with_context(|| format!("provisioning scenario {}", scenario.as_str()))?;
    }
    // Ensure the lab is torn down even if the run fails, when we provisioned it.
    let _teardown = TeardownOnDrop {
        topology: &topology,
        enabled: cli.provision,
    };

    // Each test starts its own short-lived signaling server (in the signaling
    // namespace, on its own port) around its handshake, so a server only ever
    // brokers one room — matching the mailbox's per-room lifecycle.
    run_corpus(&cli, scenario, &topology, &peer_command).await
}

/// Run the whole corpus for the scenario and write the result document.
async fn run_corpus(
    cli: &Cli,
    scenario: Scenario,
    topology: &LabTopology,
    peer_command: &PeerCommand,
) -> Result<()> {
    let target = cli
        .target
        .clone()
        .unwrap_or_else(|| cli.peer_kind.default_target().to_string());
    let room_seq = AtomicU64::new(0);

    eprintln!(
        "== ice {} / {} (offerer={} answerer={}) ==",
        target,
        scenario.as_str(),
        topology.offerer_addr,
        topology.answerer_addr
    );

    let results =
        conformance_adapter_common::run_corpus(TWO_PEER_TESTS, &cli.only, cli.jobs, |test_id| {
            run_ice_test(cli, scenario, topology, peer_command, test_id, &room_seq)
        })
        .await?;

    let report = AdapterReport {
        target: target.clone(),
        environment: scenario.as_str().to_string(),
        results,
    };
    write_report(
        &cli.out,
        &format!("{target}-{}", scenario.as_str()),
        &report,
    )?;
    Ok(())
}

/// Run one test to a raw result (single attempt; no retries).
///
/// Each test gets its own signaling server (in the signaling namespace, on
/// its own port) brokering a single room, so a server never has to survive more
/// than one handshake and concurrent tests never share one.
async fn run_ice_test(
    cli: &Cli,
    scenario: Scenario,
    topology: &LabTopology,
    peer_command: &PeerCommand,
    test_id: &str,
    room_seq: &AtomicU64,
) -> RawResult {
    let (count, size) = params_for(test_id);

    run_test(test_id, TEST_TIMEOUT, async || {
        let n = room_seq.fetch_add(1, Ordering::SeqCst);
        let room = format!("ice-{}-{}-{}", scenario.as_str(), test_id, n);
        let port = topology.signaling_port.wrapping_add(n as u16);
        let signaling_url = format!("http://{}:{}", topology.signaling_addr, port);

        // Bring up this attempt's signaling server; killed when `_signaling`
        // drops at the end of the attempt. `start_signaling` waits for the
        // server's readiness line, so the peers never race the bind (the
        // mailbox clients treat a failed HTTP round trip as fatal).
        let _signaling = start_signaling(cli, topology, port)
            .await
            .context("signaling server")?;

        let offerer = run_peer(
            scenario,
            topology,
            peer_command,
            &signaling_url,
            test_id,
            &room,
            PeerRole::Offerer,
            count,
            size,
        );
        let answerer = run_peer(
            scenario,
            topology,
            peer_command,
            &signaling_url,
            test_id,
            &room,
            PeerRole::Answerer,
            count,
            size,
        );

        let (offerer, answerer) = tokio::join!(offerer, answerer);
        Ok(fold_two(offerer?, answerer?))
    })
    .await
}

/// Run one peer of a test inside its namespace, parsing its single-line JSON
/// `test-result` from stdout.
#[allow(clippy::too_many_arguments)]
async fn run_peer(
    scenario: Scenario,
    topology: &LabTopology,
    peer_command: &PeerCommand,
    signaling_url: &str,
    test_id: &str,
    room: &str,
    role: PeerRole,
    count: u32,
    size: u32,
) -> Result<TestOutcome> {
    let ns = topology.peer_ns(role);
    let ice = scenario.ice(topology);
    let argv = peer_command.argv(&PeerRun {
        test_id,
        role: role.as_str(),
        signaling_url,
        room,
        count,
        size,
        bind_addr: topology.bind_addr(role),
        ice: Some(&ice),
        // The routed netns lab runs on a real kernel, so mDNS gathering stays
        // enabled (unlike the Shadow lab).
        disable_mdns: false,
    })?;

    let mut command = tokio::process::Command::new("ip");
    command.args(["netns", "exec", ns]).args(&argv);

    run_peer_command(command, &format!("{} peer in {ns}", role.as_str())).await
}

/// Start a signaling server on `port` inside the signaling namespace.
/// Spawn this attempt's signaling server inside the signaling namespace and
/// wait — bounded — for the `listening on <url>` line it prints once bound
/// (kept stable by `conformance-signalingd` for exactly this kind of
/// consumer). Its stdout is forwarded to stderr so diagnostics stay visible.
async fn start_signaling(cli: &Cli, topology: &LabTopology, port: u16) -> Result<Guard> {
    let mut child = std::process::Command::new("ip")
        .args(["netns", "exec", &topology.signaling_ns])
        .arg(&cli.signaling_bin)
        .args(["--host", &topology.signaling_addr])
        .args(["--port", &port.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| {
            format!(
                "spawning signaling server ({}) in {}",
                cli.signaling_bin.display(),
                topology.signaling_ns
            )
        })?;

    let stdout = child.stdout.take().expect("stdout is piped");
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            eprintln!("[signalingd] {line}");
            if line.contains("listening on") {
                let _ = ready_tx.send(());
            }
        }
    });
    tokio::task::spawn_blocking(move || ready_rx.recv_timeout(Duration::from_secs(10)))
        .await
        .context("joining signaling readiness wait")?
        .map_err(|_| {
            anyhow::anyhow!("signaling server did not report `listening on` within 10s")
        })?;
    Ok(Guard { child: Some(child) })
}
