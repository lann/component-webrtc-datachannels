//! The non-wasm reference peer for the conformance suite's two-peer corpus.
//!
//! A native binary — no wasm component, no WIT bindings — driving Google's
//! libwebrtc through [LiveKit's Rust bindings](https://crates.io/crates/libwebrtc).
//! It speaks the shared single-peer contract (`--test`/`--role`/`--server`/
//! `--room`/`--message-count`/`--message-size` plus the ICE flags, one JSON
//! `test-result` line on stdout, exit 0 regardless of outcome) and the
//! conformance guest's signaling blob schema over the `conformance-signalingd`
//! mailbox protocol (`conformance/signaling/PROTOCOL.md`), so it can be paired
//! against any suite target: a failure on the wire implicates the target's
//! stack, not a second wasm guest.

use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use clap::Parser;
use libwebrtc::data_channel::{DataChannel, DataChannelInit, DataChannelState};
use libwebrtc::ice_candidate::IceCandidate;
use libwebrtc::peer_connection::{IceGatheringState, PeerConnection, PeerConnectionState};
use libwebrtc::peer_connection_factory::{
    ContinualGatheringPolicy, IceServer, IceTransportsType, PeerConnectionFactory, RtcConfiguration,
};
use libwebrtc::session_description::{SdpType, SessionDescription};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};

/// The channel label every conformance peer negotiates.
const CHANNEL_LABEL: &str = "conformance";

/// The end-of-test rendezvous sentinel (see the conformance guest's barrier).
const BARRIER_SENTINEL: &[u8] = b"__conformance_barrier__";

/// Wall-clock bound on the whole run.
const RUN_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Parser)]
#[command(name = "conformance-reference-peer", version)]
struct Cli {
    /// Test id from the suite corpus (the two-peer subset).
    #[arg(long)]
    test: String,

    /// This peer's signaling role.
    #[arg(long, value_parser = ["offerer", "answerer"])]
    role: String,

    /// Base URL of the conformance-signalingd mailbox server.
    #[arg(long)]
    server: String,

    /// Signaling room shared with the peer.
    #[arg(long, default_value = "r")]
    room: String,

    #[arg(long, default_value_t = 16)]
    message_count: u32,

    #[arg(long, default_value_t = 512)]
    message_size: u32,

    /// STUN/TURN server URL (e.g. `stun:10.79.3.2:3478`). Absent = host
    /// candidates only.
    #[arg(long)]
    ice_server_url: Option<String>,

    /// TURN long-term-credential username.
    #[arg(long, default_value = "")]
    ice_username: String,

    /// TURN long-term-credential secret.
    #[arg(long, default_value = "")]
    ice_credential: String,

    /// Restrict ICE to relay candidates (requires --ice-server-url).
    #[arg(long, default_value_t = false, requires = "ice_server_url")]
    relay_only: bool,
}

// --- mailbox client (conformance/signaling/PROTOCOL.md) ----------------------

/// A client of one signaling room, bound to a role: publishes to its own
/// mailbox and long-polls the peer's.
struct Mailbox {
    client: reqwest::Client,
    base: String,
    room: String,
    role: &'static str,
    peer_role: &'static str,
    recv_seq: u64,
}

impl Mailbox {
    fn new(server: &str, room: &str, role: &str) -> Self {
        let (role, peer_role) = if role == "offerer" {
            ("offerer", "answerer")
        } else {
            ("answerer", "offerer")
        };
        Self {
            client: reqwest::Client::new(),
            base: server.trim_end_matches('/').to_string(),
            room: room.to_string(),
            role,
            peer_role,
            recv_seq: 0,
        }
    }

    async fn send(&self, signal: &Signal) -> Result<()> {
        let url = format!("{}/rooms/{}/{}", self.base, self.room, self.role);
        let blob = serde_json::to_vec(signal)?;
        let resp = self.client.post(&url).body(blob).send().await?;
        anyhow::ensure!(
            resp.status().is_success(),
            "mailbox send: HTTP {}",
            resp.status()
        );
        Ok(())
    }

    /// The next signal from the peer's mailbox, or `None` once it is done.
    async fn recv(&mut self) -> Result<Option<Signal>> {
        loop {
            let url = format!(
                "{}/rooms/{}/{}?seq={}&wait=10000",
                self.base, self.room, self.peer_role, self.recv_seq
            );
            let resp = self.client.get(&url).send().await?;
            match resp.status().as_u16() {
                200 => {
                    let bytes = resp.bytes().await?;
                    self.recv_seq += 1;
                    return Ok(Some(serde_json::from_slice(&bytes)?));
                }
                204 => return Ok(None),
                304 => continue,
                other => anyhow::bail!("mailbox recv: HTTP {other}"),
            }
        }
    }

    async fn done(&self) -> Result<()> {
        let url = format!("{}/rooms/{}/{}/done", self.base, self.room, self.role);
        let resp = self.client.post(&url).send().await?;
        anyhow::ensure!(
            resp.status().is_success(),
            "mailbox done: HTTP {}",
            resp.status()
        );
        Ok(())
    }
}

/// The opaque signaling blob schema, owned by the conformance guest.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum Signal {
    Offer {
        sdp: String,
    },
    Answer {
        sdp: String,
    },
    Candidate {
        candidate: String,
        #[serde(default)]
        sdp_mid: Option<String>,
        #[serde(default)]
        sdp_mline_index: Option<u16>,
    },
    EndOfCandidates,
}

// --- peer events -------------------------------------------------------------

/// One inbound data-channel item, forwarded from libwebrtc's callback thread.
enum Inbound {
    Message { binary: bool, data: Vec<u8> },
    Closed,
}

/// The queues and watches one peer's libwebrtc callbacks feed.
struct Wiring {
    /// Locally gathered candidates, ended by gathering-complete (`None`).
    candidates: mpsc::UnboundedReceiver<Option<IceCandidate>>,
    /// The latest peer connection state.
    connection: watch::Receiver<PeerConnectionState>,
    /// The remote-opened channel (answerer side), wired for receive.
    incoming: mpsc::UnboundedReceiver<(DataChannel, mpsc::UnboundedReceiver<Inbound>)>,
}

/// Register the message/close forwarding for a channel. Attached synchronously
/// inside the callback that surfaces the channel, so no early message is lost.
fn wire_channel(channel: &DataChannel) -> mpsc::UnboundedReceiver<Inbound> {
    let (tx, rx) = mpsc::unbounded_channel();
    let msg_tx = tx.clone();
    channel.on_message(Some(Box::new(move |buffer| {
        let _ = msg_tx.send(Inbound::Message {
            binary: buffer.binary,
            data: buffer.data.to_vec(),
        });
    })));
    channel.on_state_change(Some(Box::new(move |state| {
        if state == DataChannelState::Closed {
            let _ = tx.send(Inbound::Closed);
        }
    })));
    rx
}

/// Build a peer connection and wire its callbacks into owned queues.
fn build_peer(cli: &Cli) -> Result<(PeerConnection, Wiring)> {
    let factory = PeerConnectionFactory::default();
    let mut ice_servers = Vec::new();
    if let Some(url) = &cli.ice_server_url {
        ice_servers.push(IceServer {
            urls: vec![url.clone()],
            username: cli.ice_username.clone(),
            password: cli.ice_credential.clone(),
        });
    }
    let pc = factory.create_peer_connection(RtcConfiguration {
        ice_servers,
        continual_gathering_policy: ContinualGatheringPolicy::GatherOnce,
        ice_transport_type: if cli.relay_only {
            IceTransportsType::Relay
        } else {
            IceTransportsType::All
        },
    })?;

    let (cand_tx, candidates) = mpsc::unbounded_channel();
    let end_tx = cand_tx.clone();
    pc.on_ice_candidate(Some(Box::new(move |candidate| {
        let _ = cand_tx.send(Some(candidate));
    })));
    pc.on_ice_gathering_state_change(Some(Box::new(move |state| {
        if state == IceGatheringState::Complete {
            let _ = end_tx.send(None);
        }
    })));

    let (conn_tx, connection) = watch::channel(PeerConnectionState::New);
    pc.on_connection_state_change(Some(Box::new(move |state| {
        let _ = conn_tx.send(state);
    })));

    let (incoming_tx, incoming) = mpsc::unbounded_channel();
    pc.on_data_channel(Some(Box::new(move |channel| {
        let inbound = wire_channel(&channel);
        let _ = incoming_tx.send((channel, inbound));
    })));

    Ok((
        pc,
        Wiring {
            candidates,
            connection,
            incoming,
        },
    ))
}

/// The data-channel options for a test.
fn channel_init(test_id: &str) -> DataChannelInit {
    DataChannelInit {
        ordered: true,
        max_retransmits: if test_id == "max-retransmits-accepted" {
            Some(0)
        } else {
            None
        },
        ..Default::default()
    }
}

// --- handshake ---------------------------------------------------------------

/// Drain the local candidate queue to gathering-complete, publishing each
/// candidate and then the end marker, and mark this mailbox done.
async fn publish_candidates(wiring: &mut Wiring, mailbox: &Mailbox) -> Result<()> {
    while let Some(Some(candidate)) = wiring.candidates.recv().await {
        mailbox
            .send(&Signal::Candidate {
                candidate: candidate.candidate(),
                sdp_mid: Some(candidate.sdp_mid()),
                sdp_mline_index: u16::try_from(candidate.sdp_mline_index()).ok(),
            })
            .await?;
    }
    mailbox.send(&Signal::EndOfCandidates).await?;
    mailbox.done().await
}

/// Consume the peer's remaining signals (answer and/or trickled candidates)
/// until its mailbox is done.
async fn consume_signaling(pc: &PeerConnection, mailbox: &mut Mailbox) -> Result<()> {
    while let Some(signal) = mailbox.recv().await? {
        match signal {
            Signal::Answer { sdp } => {
                let desc = SessionDescription::parse(&sdp, SdpType::Answer)
                    .map_err(|e| anyhow!("parse answer: {e:?}"))?;
                pc.set_remote_description(desc).await?;
            }
            Signal::Offer { .. } => anyhow::bail!("unexpected second offer"),
            Signal::Candidate {
                candidate,
                sdp_mid,
                sdp_mline_index,
            } => {
                let parsed = IceCandidate::parse(
                    sdp_mid.as_deref().unwrap_or(""),
                    sdp_mline_index.map(i32::from).unwrap_or(0),
                    &candidate,
                )
                .map_err(|e| anyhow!("parse candidate: {e:?}"))?;
                pc.add_ice_candidate(parsed).await?;
            }
            Signal::EndOfCandidates => {}
        }
    }
    Ok(())
}

/// Wait until the connection reaches `connected` (failed/closed is an error).
async fn wait_connected(wiring: &mut Wiring) -> Result<()> {
    loop {
        match *wiring.connection.borrow() {
            PeerConnectionState::Connected => return Ok(()),
            PeerConnectionState::Failed | PeerConnectionState::Closed => {
                anyhow::bail!("connection failed before connecting")
            }
            _ => {}
        }
        wiring
            .connection
            .changed()
            .await
            .context("connection state watch ended")?;
    }
}

/// Wait until a locally created channel reports open, consuming (and
/// buffering back) nothing: open is observed via the channel's state.
async fn wait_open(channel: &DataChannel) -> Result<()> {
    // Poll the state: the open transition may have raced callback
    // registration, and state reads are cheap and race-free.
    for _ in 0..1200 {
        match channel.state() {
            DataChannelState::Open => return Ok(()),
            DataChannelState::Closing | DataChannelState::Closed => {
                anyhow::bail!("channel closed before open")
            }
            DataChannelState::Connecting => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
    anyhow::bail!("channel did not open")
}

/// Drive one side of the handshake to a connected, open data channel.
async fn handshake(
    cli: &Cli,
    pc: &PeerConnection,
    wiring: &mut Wiring,
    mailbox: &mut Mailbox,
) -> Result<(DataChannel, mpsc::UnboundedReceiver<Inbound>)> {
    let (channel, inbound) = if cli.role == "offerer" {
        let channel = pc.create_data_channel(CHANNEL_LABEL, channel_init(&cli.test))?;
        let inbound = wire_channel(&channel);

        let offer = pc.create_offer(Default::default()).await?;
        let offer_sdp = offer.to_string();
        pc.set_local_description(offer).await?;
        mailbox.send(&Signal::Offer { sdp: offer_sdp }).await?;
        publish_candidates(wiring, mailbox).await?;
        consume_signaling(pc, mailbox).await?;
        (channel, inbound)
    } else {
        let offer = match mailbox.recv().await? {
            Some(Signal::Offer { sdp }) => sdp,
            other => anyhow::bail!("expected offer, got {other:?}"),
        };
        let desc = SessionDescription::parse(&offer, SdpType::Offer)
            .map_err(|e| anyhow!("parse offer: {e:?}"))?;
        pc.set_remote_description(desc).await?;
        let answer = pc.create_answer(Default::default()).await?;
        let answer_sdp = answer.to_string();
        pc.set_local_description(answer).await?;
        mailbox.send(&Signal::Answer { sdp: answer_sdp }).await?;
        publish_candidates(wiring, mailbox).await?;
        consume_signaling(pc, mailbox).await?;

        wiring
            .incoming
            .recv()
            .await
            .context("no incoming data channel")?
    };

    wait_connected(wiring).await?;
    wait_open(&channel).await?;
    Ok((channel, inbound))
}

// --- per-test payload exchange (mirrors the conformance guest) ---------------

/// An indexed, verifiable payload: 4-byte LE index + `(index+offset) % 251`.
fn make_payload(index: u32, size: u32) -> Vec<u8> {
    let size = size.max(4) as usize;
    let mut bytes = Vec::with_capacity(size);
    bytes.extend_from_slice(&index.to_le_bytes());
    for offset in 0..size - 4 {
        bytes.push(((index as usize + offset) % 251) as u8);
    }
    bytes
}

fn payload_index(bytes: &[u8]) -> Option<u32> {
    bytes
        .get(0..4)
        .map(|head| u32::from_le_bytes(head.try_into().unwrap()))
}

fn verify_payload(bytes: &[u8]) -> bool {
    let Some(index) = payload_index(bytes) else {
        return false;
    };
    bytes[4..]
        .iter()
        .enumerate()
        .all(|(offset, byte)| *byte == ((index as usize + offset) % 251) as u8)
}

fn verify_all(received: &[Vec<u8>], count: u32, ordered: bool) -> Result<()> {
    anyhow::ensure!(
        received.len() == count as usize,
        "received {} messages, expected {count}",
        received.len()
    );
    for (position, bytes) in received.iter().enumerate() {
        anyhow::ensure!(verify_payload(bytes), "payload failed integrity check");
        if ordered {
            let index = payload_index(bytes).unwrap();
            anyhow::ensure!(
                index as usize == position,
                "message {position} carried index {index}"
            );
        }
    }
    Ok(())
}

/// The next inbound message (an inbound close is an error here).
async fn receive(inbound: &mut mpsc::UnboundedReceiver<Inbound>) -> Result<(bool, Vec<u8>)> {
    match inbound.recv().await {
        Some(Inbound::Message { binary, data }) => Ok((binary, data)),
        Some(Inbound::Closed) | None => anyhow::bail!("receive: closed"),
    }
}

async fn expect_binary(
    inbound: &mut mpsc::UnboundedReceiver<Inbound>,
    expected: &[u8],
    what: &str,
) -> Result<()> {
    let (binary, data) = receive(inbound).await?;
    anyhow::ensure!(binary && data == expected, "{what} mismatch");
    Ok(())
}

fn send(channel: &DataChannel, data: &[u8], binary: bool) -> Result<()> {
    channel
        .send(data, binary)
        .map_err(|e| anyhow!("send: {e}"))?;
    Ok(())
}

/// Run the test's payload exchange; both peers run the same routine.
async fn exchange(
    cli: &Cli,
    channel: &DataChannel,
    inbound: &mut mpsc::UnboundedReceiver<Inbound>,
) -> Result<()> {
    match cli.test.as_str() {
        "label-round-trip" => {
            anyhow::ensure!(
                channel.label() == CHANNEL_LABEL,
                "label was {:?}, expected {CHANNEL_LABEL:?}",
                channel.label()
            );
            Ok(())
        }
        "binary-message" => {
            let payload = [0u8, 1, 2, 3, 4, 5];
            send(channel, &payload, true)?;
            expect_binary(inbound, &payload, "binary payload").await
        }
        "text-message" => {
            let text = "conformance text message";
            send(channel, text.as_bytes(), false)?;
            let (binary, data) = receive(inbound).await?;
            anyhow::ensure!(!binary && data == text.as_bytes(), "text payload mismatch");
            Ok(())
        }
        "zero-length-message" => {
            send(channel, &[], true)?;
            send(channel, &[], false)?;
            let (binary, data) = receive(inbound).await?;
            anyhow::ensure!(binary && data.is_empty(), "expected empty binary message");
            let (binary, data) = receive(inbound).await?;
            anyhow::ensure!(!binary && data.is_empty(), "expected empty text message");
            Ok(())
        }
        "large-message" => {
            let payload = make_payload(0, cli.message_size.max(1024));
            send(channel, &payload, true)?;
            expect_binary(inbound, &payload, "large payload").await
        }
        "max-retransmits-accepted" => {
            let payload = [9u8, 8, 7, 6];
            send(channel, &payload, true)?;
            expect_binary(inbound, &payload, "unreliable channel payload").await
        }
        "message-boundaries"
        | "ordering"
        | "payload-integrity"
        | "concurrent-send-receive"
        | "interop-handshake" => {
            let count = cli.message_count.max(1);
            let size = cli.message_size.max(16);
            // Sends are synchronous enqueues; interleave them with receiving
            // by sending first, then draining (both peers do the same).
            for index in 0..count {
                send(channel, &make_payload(index, size), true)?;
            }
            let mut received = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let (_binary, data) = receive(inbound).await?;
                received.push(data);
            }
            verify_all(&received, count, cli.test == "ordering")
        }
        "channel-close-flush" => {
            let count = cli.message_count.max(1);
            let size = cli.message_size.max(16);
            if cli.role == "offerer" {
                // Let the answerer's channel-open callback land before the
                // burst: the case asserts flush-on-close, and an offerer
                // whose first send races the remote open turns it into an
                // open-ordering probe instead.
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                // Send the corpus, close immediately, and wait for the close
                // to complete (the graceful close transmits the buffered
                // payloads first). The sends are lightly paced so the flush
                // assertion concentrates on the close racing the final
                // payload, not on a whole burst sitting in the receiving
                // stack's delivery queue when the reset lands.
                for index in 0..count {
                    send(channel, &make_payload(index, size), true)?;
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                channel.close();
                loop {
                    match inbound.recv().await {
                        Some(Inbound::Closed) | None => break,
                        Some(Inbound::Message { .. }) => continue,
                    }
                }
                // Keep the transport up briefly after the close is
                // observed: exiting immediately tears the UDP path down
                // while the answerer's stack may still be draining the
                // flushed payloads to the application, turning a passing
                // flush into a racy tail-drop.
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                Ok(())
            } else {
                // Every payload must arrive despite the peer's immediate
                // close, after which the close itself is observed.
                let mut received = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    let (_binary, data) = receive(inbound).await?;
                    received.push(data);
                }
                verify_all(&received, count, false)?;
                match inbound.recv().await {
                    Some(Inbound::Closed) | None => Ok(()),
                    Some(Inbound::Message { .. }) => {
                        anyhow::bail!("received past the peer's close")
                    }
                }
            }
        }
        other => anyhow::bail!("unhandled test id {other:?}"),
    }
}

/// Final rendezvous: send a sentinel and wait for the peer's, so neither side
/// tears down while the other still needs the channel. A close counts as the
/// rendezvous, matching the conformance guest.
async fn barrier(
    channel: &DataChannel,
    inbound: &mut mpsc::UnboundedReceiver<Inbound>,
) -> Result<()> {
    if channel.send(BARRIER_SENTINEL, true).is_err() {
        return Ok(()); // closed: the peer already completed its exchange
    }
    loop {
        match inbound.recv().await {
            Some(Inbound::Message { data, .. }) if data == BARRIER_SENTINEL => return Ok(()),
            // Defensively skip anything still in flight before the sentinel.
            Some(Inbound::Message { .. }) => continue,
            Some(Inbound::Closed) | None => return Ok(()),
        }
    }
}

// --- entry point --------------------------------------------------------------

async fn run(cli: &Cli) -> Result<()> {
    let mut mailbox = Mailbox::new(&cli.server, &cli.room, &cli.role);
    let (pc, mut wiring) = build_peer(cli)?;
    let (channel, mut inbound) = handshake(cli, &pc, &mut wiring, &mut mailbox).await?;
    let outcome = async {
        exchange(cli, &channel, &mut inbound).await?;
        barrier(&channel, &mut inbound).await
    }
    .await;
    pc.close();
    outcome
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = match tokio::time::timeout(RUN_TIMEOUT, run(&cli)).await {
        Ok(Ok(())) => serde_json::json!({ "tag": "pass" }),
        Ok(Err(err)) => serde_json::json!({ "tag": "fail", "val": format!("{err:#}") }),
        Err(_) => serde_json::json!({ "tag": "fail", "val": "timed-out" }),
    };
    // The single-peer contract reports pass/fail in the result line, not the
    // exit status. libwebrtc's own threads may outlive the peer connection;
    // exit explicitly.
    println!("{result}");
    std::process::exit(0);
}
