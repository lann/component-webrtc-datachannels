//! A single wasmtime conformance peer, launched as a process by the
//! environment executors (the netns-lab `conformance-netns` orchestrator, inside a
//! network namespace, and the Shadow-lab `conformance-shadow` simulation).
//!
//! Each invocation runs exactly one guest instance — one role of one test —
//! against the wasmtime host, configured with an explicit netns-lab network
//! configuration (the interface address to bind and, for the server-mediated
//! scenarios, the STUN/TURN server and relay-only policy). The single-line JSON
//! `test-result` it prints to stdout matches the shape the orchestrator (and the
//! interop harness) parse: `{ "tag": "pass"|"fail"|"skipped", "val"?: string }`.
//!
//! Running one peer per process is what lets the orchestrator place the two peers
//! of a test in separate namespaces (`ip netns exec`), so their handshake
//! traverses a real network path rather than loopback.

use anyhow::{Context as _, Result};
use clap::Parser;
use serde_json::json;
use wasmtime::component::Component;

use conformance_adapter_wasmtime::{
    build_engine, make_config, run_instance, run_instance_with_ice, Role, TestConfig,
    WebrtcIceConfig, WebrtcIceServer,
};

/// Syscall overrides bridging Shadow's syscall surface to the host's needs
/// (see the module docs). Always compiled in; the stubs engage only when the
/// Shadow executor sets `CONFORMANCE_SHADOW_SYSCALL_SHIM` on the peer.
#[cfg(target_os = "linux")]
#[path = "peer/shadow_shim.rs"]
mod shadow_shim;

/// Run one conformance peer (one role of one test) and print its result.
#[derive(Debug, Parser)]
#[command(name = "conformance-peer", version)]
struct Cli {
    /// Path to the conformance guest component (`*.component.wasm`).
    #[arg(
        long,
        default_value = "conformance/guest/build/conformance-guest.component.wasm"
    )]
    guest: std::path::PathBuf,

    /// Test id to run (must match the registry).
    #[arg(long)]
    test: String,

    /// Which role this peer drives.
    #[arg(long, value_parser = parse_role)]
    role: Role,

    /// Base URL of the signaling server, e.g. `http://10.79.3.2:8080`.
    #[arg(long)]
    server: String,

    /// Signaling room both peers share.
    #[arg(long)]
    room: String,

    /// Messages to exchange for count-parameterized tests.
    #[arg(long, default_value_t = 4)]
    message_count: u32,

    /// Payload size, in bytes, for size-parameterized tests.
    #[arg(long, default_value_t = 256)]
    message_size: u32,

    /// UDP interface address to bind and gather host candidates from (e.g.
    /// `10.79.1.2`). When omitted, the peer uses the default loopback host with
    /// loopback candidates enabled (matching the non-lab adapters).
    #[arg(long)]
    bind_addr: Option<String>,

    /// STUN/TURN server URL to gather server-reflexive/relay candidates from
    /// (e.g. `turn:10.79.3.2:3478?transport=udp` or `stun:10.79.3.2:3478`).
    #[arg(long)]
    ice_server_url: Option<String>,

    /// TURN long-term-credential username (ignored for STUN-only servers).
    #[arg(long, default_value = "")]
    ice_username: String,

    /// TURN long-term-credential secret (ignored for STUN-only servers).
    #[arg(long, default_value = "")]
    ice_credential: String,

    /// Restrict this peer to TURN relay candidates (the `relay` ICE transport
    /// policy). Requires `--ice-server-url` to name a TURN server.
    #[arg(long, default_value_t = false)]
    relay_only: bool,

    /// Disable multicast-DNS candidate gathering. Required under Shadow (the
    /// `conformance-shadow` environment), whose simulated network stack does not
    /// implement the multicast-socket options mDNS binds with; the routed netns
    /// lab leaves it enabled.
    #[arg(long, default_value_t = false)]
    disable_mdns: bool,
}

fn parse_role(s: &str) -> Result<Role, String> {
    match s {
        "offerer" => Ok(Role::Offerer),
        "answerer" => Ok(Role::Answerer),
        "both" => Ok(Role::Both),
        other => Err(format!("unknown role {other:?} (offerer|answerer|both)")),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    conformance_adapter_common::init_tracing();

    let engine = build_engine()?;
    let component = Component::from_file(&engine, &cli.guest)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("loading guest component {}", cli.guest.display()))?;

    let config: TestConfig = make_config(
        cli.role,
        &cli.server,
        &cli.room,
        cli.message_count,
        cli.message_size,
    );

    // A bind address selects the netns-lab path (real interface); its absence keeps
    // the plain loopback behavior so this binary is also usable for local
    // smoke-testing without a provisioned lab.
    let result = match &cli.bind_addr {
        None => run_instance(&engine, &component, &cli.test, config).await,
        Some(addr) => {
            let ice = build_ice_config(&cli, addr);
            run_instance_with_ice(
                &engine,
                &component,
                &cli.test,
                config,
                ice,
                cli.disable_mdns,
            )
            .await
        }
    };

    // A host-side error (e.g. the guest failed to instantiate) is reported as a
    // `fail` result rather than a nonzero exit, so the orchestrator sees a
    // structured outcome for every peer.
    let value = match result {
        Ok(outcome) => outcome.to_json(),
        Err(err) => json!({ "tag": "fail", "val": format!("peer error: {err:#}") }),
    };
    println!("{value}");
    Ok(())
}

/// Assemble the peer's [`WebrtcIceConfig`] from the bind address and optional
/// STUN/TURN server flags.
fn build_ice_config(cli: &Cli, bind_addr: &str) -> WebrtcIceConfig {
    let ice_servers = cli
        .ice_server_url
        .as_ref()
        .map(|url| {
            vec![WebrtcIceServer {
                urls: vec![url.clone()],
                username: cli.ice_username.clone(),
                credential: cli.ice_credential.clone(),
            }]
        })
        .unwrap_or_default();
    WebrtcIceConfig {
        udp_addrs: vec![format!("{bind_addr}:0")],
        ice_servers,
        relay_only: cli.relay_only,
    }
}
