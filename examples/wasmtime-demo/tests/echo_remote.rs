//! End-to-end integration test for the two-process `echo-remote` demo.
//!
//! It builds the `echo-remote` guest component (`wasm32-unknown-unknown` +
//! `wasm-tools component new`), starts an in-process signaling server, spawns
//! two instances of the `echo-remote` host binary — an offerer and an answerer
//! — pointed at the same room, and requires both to verify their halves of the
//! echo exchange and exit cleanly. This exercises the demo `rendezvous`
//! mailbox (host-native HTTP client), the crate's `peer-connection` host
//! implementation, and a real `webrtc-rs` connection between two separate
//! host processes signaling over HTTP.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

#[tokio::test(flavor = "multi_thread")]
async fn echo_remote_round_trip() {
    let component = guest_component();

    let server = conformance_signalingd::spawn(
        "127.0.0.1:0".parse().expect("valid loopback address"),
        conformance_signalingd::Config::default(),
    )
    .await
    .expect("starting in-process signaling server");
    let base_url = server.base_url();

    let spawn_peer = |role: &str| {
        Command::new(env!("CARGO_BIN_EXE_echo-remote"))
            .arg(component)
            .args(["--role", role])
            .args(["--server", &base_url])
            .args(["--room", "echo-remote-test"])
            .args(["--count", "50"])
            .args(["--size", "2048"])
            // The two peers share this machine; loopback candidates guarantee
            // a mutually reachable address.
            .env("WEBRTC_INCLUDE_LOOPBACK", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawning echo-remote peer")
    };

    let answerer = spawn_peer("answerer");
    let offerer = spawn_peer("offerer");

    let offerer = tokio::task::spawn_blocking(move || offerer.wait_with_output())
        .await
        .unwrap()
        .expect("waiting for offerer");
    let answerer = tokio::task::spawn_blocking(move || answerer.wait_with_output())
        .await
        .unwrap()
        .expect("waiting for answerer");

    let offerer_out = String::from_utf8_lossy(&offerer.stdout).into_owned();
    let answerer_out = String::from_utf8_lossy(&answerer.stdout).into_owned();
    assert!(
        offerer.status.success(),
        "offerer failed ({}):\n{offerer_out}",
        offerer.status
    );
    assert!(
        answerer.status.success(),
        "answerer failed ({}):\n{answerer_out}",
        answerer.status
    );
    assert!(
        offerer_out.contains("sent 50 received 50 bytes 102400"),
        "offerer stats missing:\n{offerer_out}"
    );
    assert!(
        answerer_out.contains("OK: answerer finished."),
        "answerer did not finish cleanly:\n{answerer_out}"
    );

    server.shutdown().await;
}

/// Build the `echo-remote` guest component once, shared by the test.
fn guest_component() -> &'static PathBuf {
    static COMPONENT: OnceLock<PathBuf> = OnceLock::new();
    COMPONENT.get_or_init(|| {
        let guest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../echo-remote");
        let target_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("echo-remote-guest");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());

        let mut command = Command::new(cargo);
        command
            .current_dir(&guest_dir)
            .arg("build")
            .arg("--release")
            .arg("--target")
            .arg("wasm32-unknown-unknown")
            .arg("--target-dir")
            .arg(&target_dir);

        // The guest cross-compiles to wasm; strip env that leaks from the
        // outer `cargo test` invocation and would otherwise break the build.
        for (key, _) in std::env::vars() {
            if key.starts_with("CARGO_") || key == "RUSTFLAGS" {
                command.env_remove(key);
            }
        }

        let status = command
            .status()
            .expect("failed to spawn cargo to build the echo-remote guest");
        assert!(
            status.success(),
            "building the echo-remote guest failed; ensure the wasm32-unknown-unknown \
             target is installed (rustup target add wasm32-unknown-unknown)"
        );

        let module = target_dir.join("wasm32-unknown-unknown/release/echo_remote.wasm");
        let component = target_dir.join("echo-remote.component.wasm");
        let status = Command::new("wasm-tools")
            .arg("component")
            .arg("new")
            .arg(&module)
            .arg("-o")
            .arg(&component)
            .status()
            .expect("failed to spawn wasm-tools (is it on PATH?)");
        assert!(status.success(), "wasm-tools component new failed");
        component
    })
}
