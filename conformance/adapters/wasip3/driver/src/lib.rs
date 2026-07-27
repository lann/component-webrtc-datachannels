//! The CLI driver component for the `wasip3-guest` conformance target.
//!
//! It imports the conformance guest's exported `conformance:suite/runner`
//! control surface and exports an async `wasi:cli/run` (via the `wasip3`
//! crate), so the fully composed component — guest + `wasip3-impl` provider +
//! in-guest `wasi:http` mailbox client + this driver — runs one test per
//! `wasmtime run` invocation.
//!
//! It reads the test id and `test-config` knobs from the command line, drives
//! `run-test` to its WIT-observable outcome, and writes the raw `test-result`
//! as a single JSON line (`{"tag": "pass" | "fail" | "skipped", "val"?}`) on
//! stdout — the same shape the jco interop peer emits — for the native
//! orchestrator to parse.

mod bindings {
    wit_bindgen::generate!({
        path: "../../../wit",
        inline: "
            package conformance:wasip3-driver;
            world driver {
                import conformance:suite/runner@0.1.0;
            }
        ",
        generate_all,
    });
}

use bindings::conformance::suite::runner::{self, Role, TestConfig, TestResult};

struct Component;

impl wasip3::exports::cli::run::Guest for Component {
    async fn run() -> Result<(), ()> {
        // `--list-tests` prints the guest's corpus (one JSON array of test ids)
        // and exits, so the orchestrator can verify its registered test list
        // against the guest's list-tests export before running the corpus.
        if std::env::args().skip(1).any(|arg| arg == "--list-tests") {
            let ids: Vec<String> = runner::list_tests().into_iter().map(|t| t.id).collect();
            println!("{}", serde_json::json!(ids));
            return Ok(());
        }

        let (test_id, config) = match parse_args(std::env::args().skip(1)) {
            Ok(parsed) => parsed,
            Err(usage) => {
                eprintln!("{usage}");
                return Err(());
            }
        };

        let role = config.role;
        let result = runner::run_test(test_id, config).await;
        let json = match &result {
            TestResult::Pass => serde_json::json!({ "tag": "pass" }),
            TestResult::Fail(detail) => serde_json::json!({ "tag": "fail", "val": detail }),
            TestResult::Skipped(reason) => serde_json::json!({ "tag": "skipped", "val": reason }),
        };
        println!("{json}");
        // Linger briefly before returning: the provider's `close` is a sync
        // export, so its detached pump flushes the final sends (the barrier
        // sentinel, the SCTP/DTLS close handshake) only while this task
        // yields. Returning immediately would end the process and cut the
        // pump off mid-teardown, stalling the remote peer. A `both`-role run
        // holds both peers in this one process, so there is no remote peer to
        // protect and the grace is skipped.
        if !matches!(role, Role::Both) {
            wasip3::clocks::monotonic_clock::wait_for(CLOSE_GRACE_NANOS).await;
        }
        Ok(())
    }
}

/// How long `run` yields after the test completes so the provider's pump can
/// finish the connection teardown before the process exits.
const CLOSE_GRACE_NANOS: u64 = 500_000_000;

wasip3::cli::command::export!(Component);

/// Parse `--test <id> --role <offerer|answerer|both> --server <url> --room <id>
/// [--message-count N] [--message-size N]` into a `run-test`
/// invocation.
fn parse_args(args: impl Iterator<Item = String>) -> Result<(String, TestConfig), String> {
    const USAGE: &str = "usage: conformance-wasip3 --test <id> --role <offerer|answerer|both> \
                         --server <url> --room <id> [--message-count N] [--message-size N]";

    let mut test_id = None;
    let mut role = None;
    let mut server = None;
    let mut room = None;
    let mut message_count = 4u32;
    let mut message_size = 256u32;

    let mut args = args.peekable();
    while let Some(flag) = args.next() {
        let mut value = |flag: &str| {
            args.next()
                .ok_or_else(|| format!("missing value for {flag}\n{USAGE}"))
        };
        match flag.as_str() {
            "--test" => test_id = Some(value("--test")?),
            "--role" => {
                role = Some(match value("--role")?.as_str() {
                    "offerer" => Role::Offerer,
                    "answerer" => Role::Answerer,
                    "both" => Role::Both,
                    other => return Err(format!("unknown role {other:?}\n{USAGE}")),
                })
            }
            "--server" => server = Some(value("--server")?),
            "--room" => room = Some(value("--room")?),
            "--message-count" => {
                message_count = value("--message-count")?
                    .parse()
                    .map_err(|e| format!("bad --message-count: {e}\n{USAGE}"))?
            }
            "--message-size" => {
                message_size = value("--message-size")?
                    .parse()
                    .map_err(|e| format!("bad --message-size: {e}\n{USAGE}"))?
            }
            other => return Err(format!("unknown flag {other:?}\n{USAGE}")),
        }
    }

    let test_id = test_id.ok_or_else(|| format!("missing --test\n{USAGE}"))?;
    let config = TestConfig {
        role: role.ok_or_else(|| format!("missing --role\n{USAGE}"))?,
        signaling_server: server.ok_or_else(|| format!("missing --server\n{USAGE}"))?,
        room: room.ok_or_else(|| format!("missing --room\n{USAGE}"))?,
        message_count,
        message_size,
    };
    Ok((test_id, config))
}
