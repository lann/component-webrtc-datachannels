//! The netns-lab executor: the pair corpus for one target over real,
//! routed, non-loopback candidate paths built from network namespaces
//! (LAN, STUN server-reflexive, TURN relay, symmetric NAT — see the
//! topology in [`crate::lab`]). Root-only, workstation-only; the CI
//! analogue is the Shadow lab.
//!
//! One provisioned scenario, one signaling server in the signaling
//! namespace, and the two role children — this same binary's `exec`
//! mode wrapped in `ip netns exec` with the scenario's ICE profile —
//! folded into one canonical stream for the `<target>-<scenario>` row.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use anyhow::{bail, Context as _, Result};

use crate::lab::{LabTopology, Scenario};
use crate::lab_types::PeerRole;
use crate::{orchestrate, stream};

pub struct NetnsArgs {
    pub pair_suite: PathBuf,
    pub scenario: String,
    pub target: Option<String>,
    pub signaling_bin: PathBuf,
    pub out: Option<PathBuf>,
}

pub fn run(args: NetnsArgs) -> Result<std::process::ExitCode> {
    let scenario = Scenario::parse(&args.scenario)
        .with_context(|| format!("unknown scenario {:?}", args.scenario))?;
    let target = args
        .target
        .clone()
        .unwrap_or_else(|| format!("wasmtime-{}", scenario.as_str()));
    if !nix_is_root() {
        bail!("the netns lab provisions namespaces and nftables: run under sudo");
    }

    let topology = LabTopology::default();
    topology
        .scenario_up(scenario)
        .with_context(|| format!("provisioning scenario {}", scenario.as_str()))?;
    let result = run_in(&topology, scenario, &target, &args);
    topology.scenario_down();
    result
}

fn run_in(
    topology: &LabTopology,
    scenario: Scenario,
    target: &str,
    args: &NetnsArgs,
) -> Result<std::process::ExitCode> {
    let signaling_url = format!(
        "http://{}:{}",
        topology.signaling_addr, topology.signaling_port
    );
    let mut signalingd = spawn_signalingd(topology, &args.signaling_bin)?;
    let outcome = (|| {
        let run_id = orchestrate::run_id(target);
        let cases = orchestrate::selected_cases(&args.pair_suite, "pair/")?;
        let ice = scenario.ice(topology);

        let child = |role: PeerRole| -> Result<component_test_results::Document> {
            let role_name = match role {
                PeerRole::Offerer => "offerer",
                PeerRole::Answerer => "answerer",
            };
            let mut cmd = Command::new("ip");
            cmd.arg("netns")
                .arg("exec")
                .arg(topology.peer_ns(role))
                .arg(std::env::current_exe()?)
                .arg("exec")
                .arg(std::path::absolute(&args.pair_suite)?)
                .arg("--jsonl")
                .arg("--select")
                .arg("pair/")
                .arg("--role")
                .arg(role_name)
                .arg("--signaling")
                .arg(&signaling_url)
                .arg("--run-id")
                .arg(&run_id)
                .arg("--ice-bind")
                .arg(topology.bind_addr(role));
            if let Some(url) = &ice.server_url {
                // A credentialed server is TURN (which also serves STUN,
                // so the peer gathers srflx and relay candidates alike);
                // credential-less is plain STUN.
                if ice.username.is_empty() {
                    cmd.arg("--ice-stun").arg(url);
                } else {
                    cmd.arg("--ice-turn")
                        .arg(format!("{url},{},{}", ice.username, ice.credential));
                }
                if ice.relay_only {
                    cmd.arg("--ice-relay-only");
                }
            }
            cmd.stdin(Stdio::null()).stderr(Stdio::inherit());
            let out = cmd.output().context("spawning netns peer")?;
            let raw = String::from_utf8(out.stdout).context("child stream is not UTF-8")?;
            stream::parse_stream(&raw, &cases)
        };

        let (off, ans) = std::thread::scope(|scope| {
            let off = scope.spawn(|| child(PeerRole::Offerer));
            let ans = scope.spawn(|| child(PeerRole::Answerer));
            (off.join(), ans.join())
        });
        let off = off.map_err(|_| anyhow::anyhow!("offerer thread panicked"))??;
        let ans = ans.map_err(|_| anyhow::anyhow!("answerer thread panicked"))??;
        let mut run_errors: Vec<String> = Vec::new();
        run_errors.extend(off.run_errors.iter().map(|e| format!("offerer: {e}")));
        run_errors.extend(ans.run_errors.iter().map(|e| format!("answerer: {e}")));
        let folded = stream::fold_pair(off, ans);
        orchestrate::merge_target(
            target,
            &args.pair_suite,
            &run_id,
            None,
            (folded, run_errors),
        )
    })();
    let _ = signalingd.kill();
    let _ = signalingd.wait();
    let merged = outcome?;
    match &args.out {
        Some(path) => std::fs::write(path, &merged)?,
        None => print!("{merged}"),
    }
    Ok(crate::exit_for(&merged))
}

/// The signaling server, inside the signaling namespace, for the run's
/// lifetime.
fn spawn_signalingd(topology: &LabTopology, bin: &PathBuf) -> Result<Child> {
    let child = Command::new("ip")
        .arg("netns")
        .arg("exec")
        .arg(&topology.signaling_ns)
        .arg(std::path::absolute(bin)?)
        .arg("--host")
        .arg(&topology.signaling_addr)
        .arg("--port")
        .arg(topology.signaling_port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawning {}", bin.display()))?;
    // Give it a moment to bind; the mailbox clients treat a refused
    // connection as fatal.
    std::thread::sleep(std::time::Duration::from_millis(500));
    Ok(child)
}

fn nix_is_root() -> bool {
    // Effective uid 0; the lab shells out to ip/nft/coturn regardless,
    // which fail loudly without it — this just fails sooner.
    unsafe { libc::geteuid() == 0 }
}
