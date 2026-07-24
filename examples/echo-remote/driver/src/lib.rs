//! The CLI driver component for the fully in-guest `echo-remote` demo.
//!
//! It imports the echo-remote guest's exported `demo:webrtc-echo/remote`
//! interface and exports an async `wasi:cli/run` (via the `wasip3` crate), so
//! the fully composed component — echo-remote guest + `rendezvous-http`
//! signaling client + `wasip3-impl` connections provider + this driver — runs
//! one peer per `wasmtime run` invocation, with every capability satisfied by
//! plain WASI (`-S cli -S p3 -S http -S inherit-network`).
//!
//! It reads the role and rendezvous knobs from the command line, drives the
//! run to its stats, and prints them (or the error) on stdout.

mod bindings {
    wit_bindgen::generate!({
        path: "../../echo-demo/wit",
        inline: "
            package demo:echo-remote-driver;
            world driver {
                import demo:webrtc-echo/remote@0.1.0;
            }
        ",
        generate_all,
    });
}

use bindings::demo::webrtc_echo::remote::{self, RemoteConfig};
use bindings::demo::webrtc_echo::rendezvous::Role;

struct Component;

impl wasip3::exports::cli::run::Guest for Component {
    async fn run() -> Result<(), ()> {
        let config = match parse_args(std::env::args().skip(1)) {
            Ok(config) => config,
            Err(usage) => {
                eprintln!("{usage}");
                return Err(());
            }
        };
        let role = config.role;
        let (expect_count, expect_size) = (config.message_count, config.message_size);

        match remote::run(config).await {
            Ok(stats) => {
                let role = match role {
                    Role::Offerer => "offerer",
                    Role::Answerer => "answerer",
                };
                println!(
                    "echo-remote ({role}): sent {} received {} bytes {}",
                    stats.messages_sent, stats.messages_received, stats.bytes_echoed
                );
                if role == "offerer"
                    && (stats.messages_received != expect_count
                        || stats.bytes_echoed != u64::from(expect_count) * u64::from(expect_size))
                {
                    eprintln!("echo-remote: stats mismatch");
                    return Err(());
                }
                println!("OK: {role} finished.");
                // Linger briefly before returning: the provider's `close` is a
                // sync export, so its detached pump flushes the final sends
                // (the SCTP/DTLS close handshake) only while this task yields.
                // Returning immediately would end the process and cut the pump
                // off mid-teardown, stalling the remote peer.
                wasip3::clocks::monotonic_clock::wait_for(CLOSE_GRACE_NANOS).await;
                Ok(())
            }
            Err(err) => {
                eprintln!("echo-remote failed: {err:?}");
                Err(())
            }
        }
    }
}

/// How long `run` yields after the exchange completes so the provider's pump
/// can finish the connection teardown before the process exits.
const CLOSE_GRACE_NANOS: u64 = 500_000_000;

wasip3::cli::command::export!(Component);

/// Parse `--role <offerer|answerer> --server <url> --room <id> [--count N]
/// [--size BYTES]` into a `remote-config`.
fn parse_args(args: impl Iterator<Item = String>) -> Result<RemoteConfig, String> {
    const USAGE: &str = "usage: echo-remote --role <offerer|answerer> --server <url> \
                         --room <id> [--count N] [--size BYTES]";

    let mut role = None;
    let mut server = None;
    let mut room = None;
    let mut message_count = 100u32;
    let mut message_size = 1024u32;

    let mut args = args.peekable();
    while let Some(flag) = args.next() {
        let mut value = |flag: &str| {
            args.next()
                .ok_or_else(|| format!("missing value for {flag}\n{USAGE}"))
        };
        match flag.as_str() {
            "--role" => {
                role = Some(match value("--role")?.as_str() {
                    "offerer" => Role::Offerer,
                    "answerer" => Role::Answerer,
                    other => return Err(format!("unknown role {other:?}\n{USAGE}")),
                })
            }
            "--server" => server = Some(value("--server")?),
            "--room" => room = Some(value("--room")?),
            "--count" => {
                message_count = value("--count")?
                    .parse()
                    .map_err(|e| format!("bad --count: {e}\n{USAGE}"))?
            }
            "--size" => {
                message_size = value("--size")?
                    .parse()
                    .map_err(|e| format!("bad --size: {e}\n{USAGE}"))?
            }
            other => return Err(format!("unknown flag {other:?}\n{USAGE}")),
        }
    }

    Ok(RemoteConfig {
        role: role.ok_or_else(|| format!("missing --role\n{USAGE}"))?,
        server: server.ok_or_else(|| format!("missing --server\n{USAGE}"))?,
        room: room.ok_or_else(|| format!("missing --room\n{USAGE}"))?,
        message_count,
        message_size,
    })
}
