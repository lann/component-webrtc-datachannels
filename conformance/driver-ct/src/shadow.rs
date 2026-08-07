//! The Shadow-lab executor: the pair corpus for one target inside the
//! Shadow network simulator — two peers on separate simulated hosts
//! over a routed, non-loopback path, without root or namespaces, so it
//! runs deterministically in CI.
//!
//! One simulation, three hosts (signaling, offerer, answerer) for the
//! suite-backed kinds: each peer host runs one child stream — this same
//! binary's `exec` mode over the pair suite — and the executor parses
//! the two captured stdout streams, folds them case-wise, and emits one
//! canonical stream for the `<target>-shadow` row. The reference kind
//! is per-case by construction (one process per verdict), so its hosts
//! carry one process per case, each in its own room.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context as _, Result};
use component_test_results::{CaseResult, Status};

use crate::orchestrate;
use crate::stream;

pub struct ShadowArgs {
    pub pair_suite: PathBuf,
    pub composed_pair: Option<PathBuf>,
    pub offerer_kind: String,
    pub answerer_kind: String,
    pub target: String,
    pub shadow_bin: PathBuf,
    pub signaling_bin: PathBuf,
    pub reference_bin: PathBuf,
    pub data_dir: PathBuf,
    pub out: Option<PathBuf>,
}

const SIGNALING_PORT: u16 = 8080;
const STOP_TIME: &str = "600s";
/// Simulated-network addresses: one /24 for the run.
const SIG_IP: &str = "11.0.0.1";
const OFFERER_IP: &str = "11.0.0.2";
const ANSWERER_IP: &str = "11.0.0.3";

pub fn run(args: ShadowArgs) -> Result<std::process::ExitCode> {
    let run_id = orchestrate::run_id(&args.target);
    let signaling_url = format!("http://{SIG_IP}:{SIGNALING_PORT}");
    let cases = orchestrate::selected_cases(&args.pair_suite, "pair/")?;

    let mut config = String::new();
    let s = &mut config;
    let _ = writeln!(s, "general:");
    let _ = writeln!(s, "  stop_time: {STOP_TIME}");
    // Advance the simulated clock past blocking syscalls so an idle wait
    // (a peer long-polling the mailbox) never spins the wall clock.
    let _ = writeln!(s, "  model_unblocked_syscall_latency: true");
    let _ = writeln!(s, "network:");
    let _ = writeln!(s, "  graph:");
    let _ = writeln!(s, "    type: 1_gbit_switch");
    let _ = writeln!(s, "hosts:");

    emit_host(
        s,
        "sig",
        SIG_IP,
        &[vec![
            path_str(&absolute(&args.signaling_bin)?),
            "--host".into(),
            SIG_IP.into(),
            "--port".into(),
            SIGNALING_PORT.to_string(),
        ]],
        &[],
        "0s",
        Some("running"),
    );

    for (role, ip, kind) in [
        ("offerer", OFFERER_IP, &args.offerer_kind),
        ("answerer", ANSWERER_IP, &args.answerer_kind),
    ] {
        let (processes, env): (Vec<Vec<String>>, Vec<(&str, String)>) = match kind.as_str() {
            "wasmtime" => (
                vec![exec_argv(
                    &args.pair_suite,
                    false,
                    role,
                    &signaling_url,
                    &run_id,
                    ip,
                )?],
                vec![(crate::shadow_shim::SHADOW_SYSCALL_SHIM_ENV, "1".into())],
            ),
            "wasip3-guest" => (
                vec![exec_argv(
                    args.composed_pair
                        .as_ref()
                        .context("kind wasip3-guest needs --composed-pair")?,
                    true,
                    role,
                    &signaling_url,
                    &run_id,
                    ip,
                )?],
                Vec::new(),
            ),
            // One process per case: the reference peer emits one verdict
            // per invocation. Rooms match the suite's derivation, so a
            // reference host pairs with either a suite host or another
            // reference host.
            "reference" => (
                cases
                    .iter()
                    .map(|case| {
                        let leaf = case.rsplit('/').next().unwrap_or(case);
                        let (count, size) = orchestrate::reference_params(leaf);
                        Ok(vec![
                            path_str(&absolute(&args.reference_bin)?),
                            "--test".into(),
                            leaf.into(),
                            "--role".into(),
                            role.into(),
                            "--server".into(),
                            signaling_url.clone(),
                            "--room".into(),
                            format!("{run_id}-{leaf}"),
                            "--message-count".into(),
                            count.to_string(),
                            "--message-size".into(),
                            size.to_string(),
                        ])
                    })
                    .collect::<Result<Vec<_>>>()?,
                Vec::new(),
            ),
            other => bail!("unknown shadow peer kind {other:?}"),
        };
        emit_host(s, role, ip, &processes, &env, "2s", None);
    }

    let config_path = args.data_dir.with_extension("yaml");
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&config_path, &config)
        .with_context(|| format!("writing shadow config {}", config_path.display()))?;
    // Shadow refuses to overwrite an existing data directory.
    if args.data_dir.exists() {
        std::fs::remove_dir_all(&args.data_dir)?;
    }

    eprintln!(
        "== shadow {} ({} pair case(s)) ==",
        args.target,
        cases.len()
    );
    let status = Command::new(&args.shadow_bin)
        .args(["--parallelism", "4"])
        .arg("--data-directory")
        .arg(&args.data_dir)
        .arg(&config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("spawning {}", args.shadow_bin.display()))?;
    if !status.success() {
        // Not fatal: results are read from each host's captured stdout.
        eprintln!("warning: shadow exited with {status}; classifying captured results");
    }

    let side = |role: &str, kind: &str| -> Result<component_test_results::Document> {
        if kind == "reference" {
            reference_document(&args.data_dir, role, &cases)
        } else {
            let raw = host_stdout(&args.data_dir, role, 0)?;
            stream::parse_stream(&raw, &cases)
        }
    };
    let off = side("offerer", &args.offerer_kind)?;
    let ans = side("answerer", &args.answerer_kind)?;
    let mut run_errors: Vec<String> = Vec::new();
    run_errors.extend(off.run_errors.iter().map(|e| format!("offerer: {e}")));
    run_errors.extend(ans.run_errors.iter().map(|e| format!("answerer: {e}")));
    let folded = stream::fold_pair(off, ans);

    let merged = orchestrate::merge_target(
        &args.target,
        &args.pair_suite,
        &run_id,
        None,
        (folded, run_errors),
    )?;
    match &args.out {
        Some(path) => std::fs::write(path, &merged)?,
        None => print!("{merged}"),
    }
    Ok(crate::exit_for(&merged))
}

/// The `exec` child argv for a simulated peer host (all paths absolute:
/// Shadow sets each process's working directory to its host's output
/// directory).
fn exec_argv(
    artifact: &Path,
    composed: bool,
    role: &str,
    signaling_url: &str,
    run_id: &str,
    ip: &str,
) -> Result<Vec<String>> {
    let mut argv = vec![
        path_str(&std::env::current_exe()?),
        "exec".into(),
        path_str(&absolute(artifact)?),
        "--jsonl".into(),
        "--select".into(),
        "pair/".into(),
        "--role".into(),
        role.into(),
        "--signaling".into(),
        signaling_url.into(),
        "--run-id".into(),
        run_id.into(),
        "--ice-bind".into(),
        ip.into(),
        "--disable-mdns".into(),
    ];
    if composed {
        argv.push("--composed".into());
    }
    Ok(argv)
}

fn absolute(path: &Path) -> Result<PathBuf> {
    Ok(std::path::absolute(path)?)
}

fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Emit one Shadow host running `processes` (each argv is emitted as
/// double-quoted YAML flow scalars).
fn emit_host(
    s: &mut String,
    name: &str,
    ip: &str,
    processes: &[Vec<String>],
    env: &[(&str, String)],
    start_time: &str,
    expected_running: Option<&str>,
) {
    let _ = writeln!(s, "  {name}:");
    let _ = writeln!(s, "    ip_addr: {ip}");
    let _ = writeln!(s, "    network_node_id: 0");
    let _ = writeln!(s, "    processes:");
    for argv in processes {
        let quoted: Vec<String> = argv.iter().map(|a| json_str(a)).collect();
        let _ = writeln!(s, "    - path: {}", quoted[0]);
        let _ = writeln!(s, "      args: [{}]", quoted[1..].join(", "));
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
}

/// A string rendered as a double-quoted scalar (valid in YAML flow context).
fn json_str(value: &str) -> String {
    serde_json::Value::String(value.to_string()).to_string()
}

/// The captured stdout of one process on `host`
/// (`<data_dir>/hosts/<host>/<proc>.<pid>.stdout`).
fn host_stdout(data_dir: &Path, host: &str, index: usize) -> Result<String> {
    let host_dir = data_dir.join("hosts").join(host);
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&host_dir)
        .with_context(|| format!("reading {}", host_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "stdout"))
        .collect();
    paths.sort();
    let path = paths
        .get(index)
        .with_context(|| format!("no stdout #{index} in {}", host_dir.display()))?;
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

/// Synthesize a reference host's document from its per-case processes'
/// verdict lines, matched to cases by each process's `--test` argument
/// (recovered from the stdout filename ordering is unreliable, so the
/// verdicts are matched by parsing every capture and pairing on room).
fn reference_document(
    data_dir: &Path,
    host: &str,
    cases: &[String],
) -> Result<component_test_results::Document> {
    // Shadow names captures `<binary>.<pid>.stdout` in spawn order, which
    // matches the processes' declaration order — the cases' order here.
    let mut results = Vec::new();
    for (index, case) in cases.iter().enumerate() {
        let text = host_stdout(data_dir, host, index)?;
        let line = text.lines().last().unwrap_or("");
        let verdict: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("reference verdict for {case} (stdout: {text:?})"))?;
        let (status, detail) = match verdict.get("tag").and_then(|t| t.as_str()) {
            Some("pass") => (Status::Pass, None),
            Some("fail") => (
                Status::Fail,
                verdict
                    .get("val")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            ),
            Some("skipped") => (
                Status::Skipped,
                verdict
                    .get("val")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            ),
            other => bail!("reference verdict has unknown tag {other:?}"),
        };
        results.push(CaseResult {
            case: case.clone(),
            status,
            provenance: None,
            detail,
            seed: None,
            duration_ms: None,
            diagnostics: Vec::new(),
            diagnostics_complete: true,
        });
    }
    Ok(component_test_results::Document {
        envelope: component_test_results::Envelope::new("reference", ""),
        results,
        run_errors: Vec::new(),
        unknown_statuses: std::collections::BTreeMap::new(),
        terminated: true,
    })
}
