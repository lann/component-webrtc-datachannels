//! The conformance Shadow-lab environment executor.
//!
//! It runs the two-peer behavioral corpus for one target inside the
//! [Shadow](https://github.com/shadow/shadow) network simulator, which gives the
//! same "two peers on separate hosts over a routed, non-loopback path" property
//! as the netns lab (`conformance-netns`) but **without root, network
//! namespaces, or a real kernel network stack**: Shadow runs every process under
//! a deterministic, simulated network, intercepting their syscalls. That makes
//! this environment reproducible and runnable in restricted sandboxes and in CI
//! where raw `ip netns` traffic is unavailable.
//!
//! Unlike the netns executor — which `exec`s each peer into a namespace and
//! drives the corpus itself — Shadow owns the whole run: this binary
//! generates a single Shadow configuration describing, for each test, a signaling
//! host plus an offerer and an answerer host (each on its own simulated IP), runs
//! `shadow` once, then reads each peer's single-line JSON `test-result` from the
//! per-host stdout file Shadow captures. The run is deterministic, so no retries
//! are needed. The result document it writes (`<target>-shadow.json`,
//! `environment = shadow`) is classified by the conformance runner exactly like
//! any other adapter report.
//!
//! The executor is target-neutral: `--peer-kind` selects how each peer host's
//! command line is built, and `--offerer-kind` / `--answerer-kind` override it
//! per role, placing an **interop pair** (e.g. `wasmtime-x-wasip3-guest`) on
//! separate simulated hosts. Any peer that honours the shared single-peer
//! contract (`--test`/`--role`/`--server`/`--room`/…, one JSON result line on
//! stdout) and can bind a configured non-loopback address fits:
//!
//! - `wasmtime` runs the native `conformance-peer` binary. Its peers gather host
//!   candidates from their simulated interface address (`--bind-addr`) and run
//!   with `--disable-mdns`: Shadow's simulated stack does not implement the
//!   multicast-socket options (`SO_REUSEADDR`/`SO_REUSEPORT`) that multicast-DNS
//!   candidate gathering binds with, and the peers connect over their explicit
//!   host candidates rather than `.local` names, so mDNS is unnecessary here.
//!   The executor also arms the peer's Shadow syscall shim by setting
//!   [`SHADOW_SYSCALL_SHIM_ENV`] in each simulated peer's environment.
//! - `wasip3-guest` runs the fully composed wasip3 conformance component under
//!   `wasmtime run` (the same invocation as the loopback adapter), pointing the
//!   in-guest provider at the host's simulated address through the
//!   `WEBRTC_UDP_BIND_ADDR` environment variable. The sans-I/O stack has no
//!   mDNS, so no equivalent of `--disable-mdns` is needed.
//! - `reference` runs the non-wasm reference peer (`conformance-reference-peer`,
//!   Google's libwebrtc via LiveKit's Rust bindings). libwebrtc gathers
//!   candidates from the simulated host's own interface, so no bind address
//!   is passed.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context as _, Result};
use clap::Parser;

use conformance_adapter_common::peer_command::{PeerCommand, PeerKind, PeerRun};
use conformance_adapter_common::{
    fold_two, outcome_to_raw, params_for, parse_result_line, write_report, AdapterReport,
    RawResult, TestOutcome, SHADOW_SYSCALL_SHIM_ENV, TWO_PEER_TESTS,
};

#[derive(Debug, Parser)]
#[command(name = "conformance-shadow", version)]
struct Cli {
    /// Target id recorded in the result document, matching the manifest
    /// `[target].id`.
    #[arg(long, default_value = "wasmtime")]
    target: String,

    /// How each peer host's command line is built (which target's peer runs).
    #[arg(long, value_enum, default_value_t = PeerKind::Wasmtime)]
    peer_kind: PeerKind,

    /// How the offerer host's command line is built; defaults to --peer-kind.
    /// Set this and --answerer-kind to different kinds to place an interop
    /// pair.
    #[arg(long, value_enum)]
    offerer_kind: Option<PeerKind>,

    /// How the answerer host's command line is built; defaults to --peer-kind.
    #[arg(long, value_enum)]
    answerer_kind: Option<PeerKind>,

    /// Environment id recorded in the result document (the matrix column).
    #[arg(long, default_value = "shadow")]
    environment: String,

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

    /// The `conformance-signalingd` binary, run on each test's signaling host.
    #[arg(long, default_value = "target/debug/conformance-signalingd")]
    signaling_bin: PathBuf,

    /// The `conformance-peer` binary, run on each peer host by the `wasmtime`
    /// peer kind.
    #[arg(long, default_value = "target/release/conformance-peer")]
    peer_bin: PathBuf,

    /// The `conformance-reference-peer` binary, used by the `reference` peer
    /// kind.
    #[arg(long, default_value = "target/release/conformance-reference-peer")]
    reference_peer: PathBuf,

    /// The `shadow` simulator binary.
    #[arg(long, default_value = "shadow")]
    shadow_bin: PathBuf,

    /// Directory Shadow writes its per-host output into. It is removed and
    /// recreated on each run (Shadow refuses to overwrite an existing one).
    #[arg(long, default_value = "target/shadow-data")]
    data_dir: PathBuf,

    /// Simulated stop time. Handshakes settle in a few simulated seconds; this
    /// only bounds a stuck run.
    #[arg(long, default_value = "300s")]
    stop_time: String,

    /// How many worker cores Shadow may use.
    #[arg(long, default_value_t = 4)]
    parallelism: usize,

    /// Signaling HTTP port used on every signaling host (each test has its own
    /// host, so the port never collides).
    #[arg(long, default_value_t = 8080)]
    signaling_port: u16,

    /// Run only these test ids (repeatable). When empty, run the whole corpus.
    #[arg(long = "only")]
    only: Vec<String>,
}

/// One two-peer test placed on three simulated hosts.
struct Placement {
    test_id: &'static str,
    index: usize,
    signaling_ip: String,
    offerer_ip: String,
    answerer_ip: String,
    count: u32,
    size: u32,
}

impl Placement {
    fn new(test_id: &'static str, index: usize) -> Self {
        let (count, size) = params_for(test_id);
        Self {
            test_id,
            index,
            signaling_ip: format!("11.0.{index}.1"),
            offerer_ip: format!("11.0.{index}.2"),
            answerer_ip: format!("11.0.{index}.3"),
            count,
            size,
        }
    }

    fn signaling_url(&self, port: u16) -> String {
        format!("http://{}:{}", self.signaling_ip, port)
    }
}

/// One role's peer placement: the kind that selects its environment (the
/// syscall shim arms only for the native `wasmtime` peer) and the resolved
/// command template.
struct RolePeer {
    kind: PeerKind,
    command: PeerCommand,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    conformance_adapter_common::init_tracing();

    // Shadow runs each managed process with its working directory set to that
    // host's output directory, not the caller's, so every path handed to a
    // process (binaries and components) must be absolute.
    let resolve_role = |kind: PeerKind| -> Result<RolePeer> {
        Ok(RolePeer {
            kind,
            command: PeerCommand::resolve(
                kind,
                &cli.peer_bin,
                &cli.guest,
                &cli.wasmtime_bin,
                &cli.component,
                &cli.reference_peer,
            )?,
        })
    };
    let offerer = resolve_role(cli.offerer_kind.unwrap_or(cli.peer_kind))?;
    let answerer = resolve_role(cli.answerer_kind.unwrap_or(cli.peer_kind))?;
    let signaling_bin = conformance_adapter_common::peer_command::absolute(&cli.signaling_bin)?;

    let placements: Vec<Placement> = TWO_PEER_TESTS
        .iter()
        .filter(|id| cli.only.is_empty() || cli.only.iter().any(|o| o == *id))
        .enumerate()
        .map(|(index, id)| Placement::new(id, index))
        .collect();
    if placements.is_empty() {
        anyhow::bail!("no tests selected");
    }

    let config = render_config(&cli, &offerer, &answerer, &signaling_bin, &placements)?;
    let config_path = cli.data_dir.with_extension("yaml");
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&config_path, &config)
        .with_context(|| format!("writing shadow config {}", config_path.display()))?;

    // Shadow refuses to overwrite an existing data directory, so clear it first.
    if cli.data_dir.exists() {
        std::fs::remove_dir_all(&cli.data_dir)
            .with_context(|| format!("clearing {}", cli.data_dir.display()))?;
    }

    eprintln!(
        "== shadow {} / {} ({} test(s)) ==",
        cli.target,
        cli.environment,
        placements.len()
    );
    run_shadow(&cli, &config_path)?;

    let results: Vec<RawResult> = placements.iter().map(|p| collect_result(&cli, p)).collect();

    let report = AdapterReport {
        target: cli.target.clone(),
        environment: cli.environment.clone(),
        results,
    };
    write_report(
        &cli.out,
        &format!("{}-{}", cli.target, cli.environment),
        &report,
    )?;
    Ok(())
}

/// Render the Shadow configuration placing every selected test on its own trio
/// of simulated hosts (signaling, offerer, answerer).
fn render_config(
    cli: &Cli,
    offerer: &RolePeer,
    answerer: &RolePeer,
    signaling_bin: &Path,
    placements: &[Placement],
) -> Result<String> {
    let mut s = String::new();
    let _ = writeln!(s, "general:");
    let _ = writeln!(s, "  stop_time: {}", cli.stop_time);
    // Advance the simulated clock past blocking syscalls so an idle wait (a peer
    // long-polling the mailbox) never spins the wall clock.
    let _ = writeln!(s, "  model_unblocked_syscall_latency: true");
    let _ = writeln!(s, "network:");
    let _ = writeln!(s, "  graph:");
    let _ = writeln!(s, "    type: 1_gbit_switch");
    let _ = writeln!(s, "hosts:");

    for p in placements {
        // Signaling host: a long-lived server, so it is expected to still be
        // running when the simulation stops.
        emit_host(
            &mut s,
            &format!("sig{}", p.index),
            &p.signaling_ip,
            &[
                json_str(&signaling_bin.to_string_lossy()),
                json_str("--host"),
                json_str(&p.signaling_ip),
                json_str("--port"),
                json_str(&cli.signaling_port.to_string()),
            ],
            &[],
            "0s",
            Some("running"),
        );

        for (role, ip, peer) in [
            ("offerer", &p.offerer_ip, offerer),
            ("answerer", &p.answerer_ip, answerer),
        ] {
            // The Shadow simulated stack lacks the multicast-socket options
            // mDNS candidate gathering binds with, so the wasmtime peers run
            // with mDNS disabled (they connect over explicit host candidates,
            // so mDNS is unnecessary here).
            let argv = peer.command.argv(&PeerRun {
                test_id: p.test_id,
                role,
                signaling_url: &p.signaling_url(cli.signaling_port),
                room: "r",
                count: p.count,
                size: p.size,
                bind_addr: ip,
                ice: None,
                disable_mdns: true,
            })?;
            // Each argv element is quoted at emit time as a double-quoted
            // YAML/JSON scalar.
            let argv: Vec<String> = argv.iter().map(|a| json_str(a)).collect();
            // Arm the native peer's Shadow syscall shim: it bridges Shadow's
            // syscall surface to the webrtc driver's quinn-udp UDP layer and
            // engages only when this variable is set (the wasip3-guest peer
            // has no shim; its UDP goes through wasi:sockets).
            let env: &[(&str, &str)] = match peer.kind {
                PeerKind::Wasmtime => &[(SHADOW_SYSCALL_SHIM_ENV, "1")],
                PeerKind::Wasip3Guest | PeerKind::Reference => &[],
            };
            emit_host(
                &mut s,
                &format!("{role}{}", p.index),
                ip,
                &argv,
                env,
                // Give the signaling server a moment to bind: the mailbox
                // clients retry a long-poll that finds no blob, but treat a
                // failed HTTP round trip (server not yet listening) as fatal.
                "2s",
                None,
            );
        }
    }
    Ok(s)
}

/// Emit one Shadow host running a single process with the given environment.
fn emit_host(
    s: &mut String,
    name: &str,
    ip: &str,
    args: &[String],
    env: &[(&str, &str)],
    start_time: &str,
    expected_running: Option<&str>,
) {
    let _ = writeln!(s, "  {name}:");
    let _ = writeln!(s, "    ip_addr: {ip}");
    let _ = writeln!(s, "    network_node_id: 0");
    let _ = writeln!(s, "    processes:");
    let _ = writeln!(s, "    - path: {}", args[0]);
    let _ = writeln!(s, "      args: [{}]", args[1..].join(", "));
    if !env.is_empty() {
        let pairs: Vec<String> = env
            .iter()
            .map(|(k, v)| format!("{k}: {}", json_str(v)))
            .collect();
        let _ = writeln!(s, "      environment: {{ {} }}", pairs.join(", "));
    }
    let _ = writeln!(s, "      start_time: {start_time}");
    if let Some(state) = expected_running {
        let _ = writeln!(s, "      expected_final_state: {state}");
    }
}

/// A string rendered as a double-quoted scalar (valid in YAML flow context).
fn json_str(value: &str) -> String {
    serde_json::Value::String(value.to_string()).to_string()
}

/// Run `shadow` once over the generated config, inheriting stdio and failing
/// only on spawn errors — a nonzero exit (e.g. a peer that did not reach its
/// expected final state) is diagnosed per-test from the parsed results instead.
fn run_shadow(cli: &Cli, config_path: &Path) -> Result<()> {
    let status = std::process::Command::new(&cli.shadow_bin)
        .args(["--parallelism", &cli.parallelism.to_string()])
        .arg("--data-directory")
        .arg(&cli.data_dir)
        .arg(config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("spawning {}", cli.shadow_bin.display()))?;
    if !status.success() {
        // Not fatal: results are read from each host's stdout regardless.
        eprintln!("warning: shadow exited with {status}; classifying per-test results");
    }
    Ok(())
}

/// Read one test's offerer and answerer outcomes and fold them into a raw result.
fn collect_result(cli: &Cli, p: &Placement) -> RawResult {
    let offerer = read_outcome(&cli.data_dir, &format!("offerer{}", p.index));
    let answerer = read_outcome(&cli.data_dir, &format!("answerer{}", p.index));
    let outcome = match (offerer, answerer) {
        (Ok(o), Ok(a)) => fold_two(o, a),
        (Err(e), _) | (_, Err(e)) => {
            TestOutcome::Fail(format!("shadow: missing peer result: {e:#}"))
        }
    };
    outcome_to_raw(p.test_id, outcome)
}

/// Parse the single-line JSON `test-result` a peer printed to its stdout, which
/// Shadow captures at `<data_dir>/hosts/<host>/<proc>.<pid>.stdout`.
fn read_outcome(data_dir: &Path, host: &str) -> Result<TestOutcome> {
    let host_dir = data_dir.join("hosts").join(host);
    let stdout_path = std::fs::read_dir(&host_dir)
        .with_context(|| format!("reading {}", host_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|e| e == "stdout"))
        .with_context(|| format!("no stdout file in {}", host_dir.display()))?;
    let text = std::fs::read_to_string(&stdout_path)
        .with_context(|| format!("reading {}", stdout_path.display()))?;
    parse_result_line(&text)
        .with_context(|| format!("reading result from {}", stdout_path.display()))
}
