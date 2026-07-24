//! The shared conformance guest component.
//!
//! One wasm binary, run unchanged against every conformance target. It exports
//! the `conformance:suite/runner` control surface the adapters drive and imports
//! the shared `lann:webrtc-datachannels/connections` surface under test plus the
//! suite-owned `conformance:signaling/mailbox` it signals through.
//!
//! `list-tests` mirrors `conformance/tests.toml`. `run-test` runs one test to a
//! WIT-observable outcome (`pass`/`fail`/`skipped`). Two-peer behavioral tests
//! run as two guest instances (an `offerer` and an `answerer`) sharing one
//! signaling room; peer-connection API tests run as a single `both` instance
//! that stands up two peer connections in-process. Assertions target
//! interoperable behavior only — never SDP text, candidate order, timing, or
//! exact error strings.

use serde::{Deserialize, Serialize};

mod bindings {
    wit_bindgen::generate!({
        path: "../wit",
        world: "conformance",
        generate_all,
    });
}

use bindings::conformance::signaling::mailbox::{Role as MailboxRole, Session};
use bindings::exports::conformance::suite::runner::{
    Guest, Role, TestConfig, TestDescriptor, TestResult,
};
use bindings::lann::webrtc_datachannels::connections::{
    DataChannel, DataChannelOptions, PeerConnection,
};
use bindings::lann::webrtc_datachannels::types::{
    Error, IceCandidate, Message, MessageKind, SdpType, SessionDescription, StreamMessage,
};

/// The negotiated data-channel label used by every behavioral test. Both peers
/// observe it identically.
const CHANNEL_LABEL: &str = "conformance";

struct Component;

impl Guest for Component {
    fn list_tests() -> Vec<TestDescriptor> {
        corpus()
            .iter()
            .map(|(id, tags)| TestDescriptor {
                id: (*id).to_string(),
                tags: tags.iter().map(|t| (*t).to_string()).collect(),
            })
            .collect()
    }

    async fn run_test(test_id: String, config: TestConfig) -> TestResult {
        match run(&test_id, &config).await {
            Outcome::Pass => TestResult::Pass,
            Outcome::Fail(detail) => TestResult::Fail(detail),
        }
    }
}

bindings::export!(Component with_types_in bindings);

/// The internal result of running one test, mapped onto the WIT `test-result`.
/// (The WIT also has a `skipped` case for tests a target cannot run; every
/// registered test currently runs everywhere, so the guest never produces it.)
enum Outcome {
    Pass,
    Fail(String),
}

impl Outcome {
    fn from_result(result: Result<(), String>) -> Self {
        match result {
            Ok(()) => Outcome::Pass,
            Err(detail) => Outcome::Fail(detail),
        }
    }
}

/// The test corpus: `(id, tags)` mirroring `conformance/tests.toml`.
fn corpus() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        ("label-round-trip", &["data-channel"]),
        ("binary-message", &["data-channel"]),
        ("text-message", &["data-channel"]),
        ("message-boundaries", &["data-channel"]),
        ("zero-length-message", &["data-channel"]),
        ("large-message", &["data-channel"]),
        ("ordering", &["data-channel"]),
        ("payload-integrity", &["data-channel"]),
        ("concurrent-send-receive", &["data-channel"]),
        ("send-via-stream", &["data-channel", "streaming"]),
        ("receive-via-stream", &["data-channel", "streaming"]),
        (
            "receive-via-stream-once",
            &["data-channel", "streaming", "errors"],
        ),
        ("post-close-send", &["data-channel", "errors"]),
        (
            "receive-buffer-overflow",
            &["data-channel", "errors", "flow-control"],
        ),
        (
            "max-retransmits-accepted",
            &["data-channel", "unreliable-channels"],
        ),
        ("error-invalid-signaling", &["errors"]),
        ("error-closed", &["errors"]),
        ("error-timed-out", &["errors"]),
        ("peer-offer-answer", &["peer-connection"]),
        ("peer-create-data-channel", &["peer-connection"]),
        ("peer-local-ice-candidates", &["peer-connection"]),
        ("peer-add-ice-candidate", &["peer-connection"]),
        ("peer-wait-connected", &["peer-connection"]),
        ("peer-wait-connected-latch", &["peer-connection"]),
        ("peer-streams-once", &["peer-connection"]),
        ("post-close-signaling", &["peer-connection", "errors"]),
        ("peer-close-releases", &["peer-connection"]),
        ("peer-invalid-sdp", &["peer-connection", "errors"]),
        ("interop-handshake", &["interop", "signaling"]),
    ]
}

/// Dispatch a test by id.
async fn run(test_id: &str, config: &TestConfig) -> Outcome {
    match test_id {
        // Peer-connection API tests, the error-taxonomy probes, and the
        // streaming forms run as a single `both` instance with two in-process
        // peer connections (or, for `error-timed-out`, a single unconnected
        // peer).
        "peer-offer-answer"
        | "peer-create-data-channel"
        | "peer-local-ice-candidates"
        | "peer-add-ice-candidate"
        | "peer-wait-connected"
        | "peer-wait-connected-latch"
        | "peer-streams-once"
        | "post-close-signaling"
        | "peer-close-releases"
        | "peer-invalid-sdp"
        | "error-invalid-signaling"
        | "error-closed"
        | "error-timed-out"
        | "post-close-send"
        | "receive-buffer-overflow"
        | "send-via-stream"
        | "receive-via-stream"
        | "receive-via-stream-once" => run_inproc(test_id, config).await,

        // Everything else is a two-peer behavioral test driven over the mailbox.
        _ => run_two_peer(test_id, config).await,
    }
}

// --- two-peer behavioral tests (mailbox handshake) -------------------------

/// Run a two-peer behavioral test: complete the mailbox-driven handshake for
/// this instance's role, then run the per-test payload exchange over the
/// connected data channel.
async fn run_two_peer(test_id: &str, config: &TestConfig) -> Outcome {
    let role = match config.role {
        Role::Both => return Outcome::Fail("two-peer test invoked with role=both".to_string()),
        Role::Offerer => MailboxRole::Offerer,
        Role::Answerer => MailboxRole::Answerer,
    };

    let session =
        match Session::open(config.signaling_server.clone(), config.room.clone(), role).await {
            Ok(session) => session,
            Err(err) => return Outcome::Fail(format!("mailbox open: {}", describe(&err))),
        };

    let handshake = match role {
        MailboxRole::Offerer => handshake_offerer(test_id, &session).await,
        MailboxRole::Answerer => handshake_answerer(&session).await,
    };
    let (peer, dc) = match handshake {
        Ok(pair) => pair,
        Err(detail) => return Outcome::Fail(detail),
    };

    // Run the per-test assertions, then rendezvous over the data channel before
    // tearing down, so neither peer closes the connection while the other still
    // needs it — to receive the channel (`label-round-trip` transfers no
    // payload) or to drain the last buffered messages.
    let outcome = match exchange(test_id, config, &dc).await {
        Ok(()) => Outcome::from_result(barrier(&dc).await),
        Err(detail) => Outcome::Fail(detail),
    };
    peer.close();
    outcome
}

/// Drive the offerer half of the handshake, returning the connected peer and the
/// data channel it created.
async fn handshake_offerer(
    test_id: &str,
    session: &Session,
) -> Result<(PeerConnection, DataChannel), String> {
    let peer = PeerConnection::new();
    let dc = peer
        .create_data_channel(channel_options(test_id))
        .map_err(|e| format!("create-data-channel: {}", describe(&e)))?;

    let offer = peer
        .create_offer()
        .await
        .map_err(|e| format!("create-offer: {}", describe(&e)))?;
    let offer_sdp = offer.sdp.clone();
    peer.set_local_description(offer)
        .await
        .map_err(|e| format!("set-local-description: {}", describe(&e)))?;

    publish(session, &Signal::Offer { sdp: offer_sdp }).await?;
    publish_candidates(&peer, session).await?;
    done(session).await?;

    // Consume the answer and the peer's trickled candidates.
    consume_signaling(&peer, session).await?;

    peer.wait_connected()
        .await
        .map_err(|e| format!("wait-connected: {}", describe(&e)))?;
    Ok((peer, dc))
}

/// Drive the answerer half of the handshake, returning the connected peer and
/// the data channel the offerer opened.
async fn handshake_answerer(session: &Session) -> Result<(PeerConnection, DataChannel), String> {
    let peer = PeerConnection::new();

    // The offerer publishes its offer first.
    let offer = match recv_signal(session).await? {
        Some(Signal::Offer { sdp }) => sdp,
        other => return Err(format!("expected offer, got {other:?}")),
    };
    peer.set_remote_description(make_sdp(SdpType::Offer, offer))
        .await
        .map_err(|e| format!("set-remote-description offer: {}", describe(&e)))?;

    let answer = peer
        .create_answer()
        .await
        .map_err(|e| format!("create-answer: {}", describe(&e)))?;
    let answer_sdp = answer.sdp.clone();
    peer.set_local_description(answer)
        .await
        .map_err(|e| format!("set-local-description: {}", describe(&e)))?;

    publish(session, &Signal::Answer { sdp: answer_sdp }).await?;
    publish_candidates(&peer, session).await?;
    done(session).await?;

    // Consume the offerer's trickled candidates (the offer was already read).
    consume_signaling(&peer, session).await?;

    peer.wait_connected()
        .await
        .map_err(|e| format!("wait-connected: {}", describe(&e)))?;

    let dc = first_incoming(&peer).await?;
    Ok((peer, dc))
}

/// Drain a peer's local ICE candidates, publishing each and then an explicit
/// end-of-candidates marker.
async fn publish_candidates(peer: &PeerConnection, session: &Session) -> Result<(), String> {
    let candidates = collect_candidates(peer.local_ice_candidates()).await;
    for candidate in candidates {
        publish(
            session,
            &Signal::Candidate {
                candidate: candidate.candidate,
                sdp_mid: candidate.sdp_mid,
                sdp_mline_index: candidate.sdp_mline_index,
            },
        )
        .await?;
    }
    publish(session, &Signal::EndOfCandidates).await
}

/// Consume the peer's signaling blobs, applying an answer (if any) and each
/// trickled candidate, until the peer's mailbox is done.
async fn consume_signaling(peer: &PeerConnection, session: &Session) -> Result<(), String> {
    while let Some(signal) = recv_signal(session).await? {
        match signal {
            Signal::Answer { sdp } => peer
                .set_remote_description(make_sdp(SdpType::Answer, sdp))
                .await
                .map_err(|e| format!("set-remote-description answer: {}", describe(&e)))?,
            Signal::Offer { .. } => {
                return Err("unexpected second offer".to_string());
            }
            Signal::Candidate {
                candidate,
                sdp_mid,
                sdp_mline_index,
            } => peer
                .add_ice_candidate(IceCandidate {
                    candidate,
                    sdp_mid,
                    sdp_mline_index,
                })
                .await
                .map_err(|e| format!("add-ice-candidate: {}", describe(&e)))?,
            Signal::EndOfCandidates => {}
        }
    }
    Ok(())
}

// --- per-test payload exchange ---------------------------------------------

/// Run the payload exchange for `test_id` over the connected data channel. Both
/// peers run the same routine.
async fn exchange(test_id: &str, config: &TestConfig, dc: &DataChannel) -> Result<(), String> {
    match test_id {
        "label-round-trip" => {
            if dc.label() == CHANNEL_LABEL {
                Ok(())
            } else {
                Err(format!(
                    "label was {:?}, expected {CHANNEL_LABEL:?}",
                    dc.label()
                ))
            }
        }
        "binary-message" => {
            let payload = vec![0u8, 1, 2, 3, 4, 5];
            send(dc, Message::Binary(payload.clone())).await?;
            match receive(dc).await? {
                Message::Binary(bytes) if bytes == payload => Ok(()),
                Message::Binary(_) => Err("binary payload mismatch".to_string()),
                Message::String(_) => Err("binary message arrived as text".to_string()),
            }
        }
        "text-message" => {
            let text = "conformance text message";
            send(dc, Message::String(text.to_string())).await?;
            match receive(dc).await? {
                Message::String(got) if got == text => Ok(()),
                Message::String(_) => Err("text payload mismatch".to_string()),
                Message::Binary(_) => Err("text message arrived as binary".to_string()),
            }
        }
        "zero-length-message" => {
            send(dc, Message::Binary(Vec::new())).await?;
            send(dc, Message::String(String::new())).await?;
            match receive(dc).await? {
                Message::Binary(bytes) if bytes.is_empty() => {}
                _ => return Err("expected empty binary message".to_string()),
            }
            match receive(dc).await? {
                Message::String(text) if text.is_empty() => Ok(()),
                _ => Err("expected empty text message".to_string()),
            }
        }
        "large-message" => {
            let size = config.message_size.max(1024);
            let payload = make_payload(0, size);
            send(dc, Message::Binary(payload.clone())).await?;
            match receive(dc).await? {
                Message::Binary(bytes) if bytes == payload => Ok(()),
                _ => Err("large payload mismatch".to_string()),
            }
        }
        "max-retransmits-accepted" => {
            let payload = vec![9u8, 8, 7, 6];
            send(dc, Message::Binary(payload.clone())).await?;
            match receive(dc).await? {
                Message::Binary(bytes) if bytes == payload => Ok(()),
                _ => Err("unreliable channel payload mismatch".to_string()),
            }
        }
        "concurrent-send-receive" => {
            let count = config.message_count.max(1);
            let size = config.message_size.max(16);
            let sender = send_sequence(dc, count, size);
            let receiver = recv_sequence(dc, count);
            let (sent, received) = futures::join!(sender, receiver);
            sent?;
            verify_all(&received?, count)
        }
        // Count-parameterized payload tests plus the flagship interop handshake.
        "message-boundaries" | "ordering" | "payload-integrity" | "interop-handshake" => {
            let count = config.message_count.max(1);
            let size = config.message_size.max(16);
            let sender = send_sequence(dc, count, size);
            let receiver = recv_sequence(dc, count);
            let (sent, received) = futures::join!(sender, receiver);
            sent?;
            let received = received?;
            if test_id == "ordering" {
                verify_ordered(&received, count)
            } else {
                verify_all(&received, count)
            }
        }
        other => Err(format!("unhandled test id {other:?}")),
    }
}

/// A final rendezvous over the connected data channel: each peer sends a
/// sentinel and waits for the peer's, so neither peer tears down the connection
/// while the other still needs it. The sentinel arrives after any test payloads
/// because the channel is reliable and ordered. A `closed` error counts as the
/// rendezvous: the peer only closes after completing its own exchange, so the
/// close carries the same information as the sentinel (and hosts may drop a
/// final in-flight message when the remote tears down immediately after it).
async fn barrier(dc: &DataChannel) -> Result<(), String> {
    const SENTINEL: &[u8] = b"__conformance_barrier__";
    let send_side = async {
        match dc.send(Message::Binary(SENTINEL.to_vec())).await {
            Ok(()) | Err(Error::Closed) => Ok(()),
            Err(err) => Err(format!("send: {}", describe(&err))),
        }
    };
    let recv_side = async {
        loop {
            match dc.receive().await {
                Ok(Message::Binary(bytes)) if bytes == SENTINEL => return Ok::<(), String>(()),
                // Defensively skip anything still in flight before the sentinel.
                Ok(_) => continue,
                Err(Error::Closed) => return Ok(()),
                Err(err) => return Err(format!("receive: {}", describe(&err))),
            }
        }
    };
    let (sent, received) = futures::join!(send_side, recv_side);
    sent?;
    received
}

/// Send `count` indexed, checksummable payloads of `size` bytes each.
async fn send_sequence(dc: &DataChannel, count: u32, size: u32) -> Result<(), String> {
    for index in 0..count {
        send(dc, Message::Binary(make_payload(index, size))).await?;
    }
    Ok(())
}

/// Receive `count` messages, returning their raw bytes.
async fn recv_sequence(dc: &DataChannel, count: u32) -> Result<Vec<Vec<u8>>, String> {
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        match receive(dc).await? {
            Message::Binary(bytes) => out.push(bytes),
            Message::String(text) => out.push(text.into_bytes()),
        }
    }
    Ok(out)
}

/// Verify every payload is well-formed and `count` messages arrived.
fn verify_all(received: &[Vec<u8>], count: u32) -> Result<(), String> {
    if received.len() != count as usize {
        return Err(format!(
            "received {} messages, expected {count}",
            received.len()
        ));
    }
    for bytes in received {
        if !verify_payload(bytes) {
            return Err("payload failed integrity check".to_string());
        }
    }
    Ok(())
}

/// Verify payloads arrived in index order 0..count, each well-formed.
fn verify_ordered(received: &[Vec<u8>], count: u32) -> Result<(), String> {
    verify_all(received, count)?;
    for (position, bytes) in received.iter().enumerate() {
        match payload_index(bytes) {
            Some(index) if index as usize == position => {}
            Some(index) => {
                return Err(format!("message {position} carried index {index}"));
            }
            None => return Err("payload too short to carry an index".to_string()),
        }
    }
    Ok(())
}

// --- in-process peer-connection API tests ----------------------------------

/// Run a single-instance peer-connection test: stand up two peers in-process
/// (no external signaling) and exercise the targeted API surface.
async fn run_inproc(test_id: &str, config: &TestConfig) -> Outcome {
    match test_id {
        "error-invalid-signaling" | "peer-invalid-sdp" => Outcome::from_result(invalid_sdp().await),
        "receive-buffer-overflow" => Outcome::from_result(receive_overflow(config).await),
        "error-closed" => Outcome::from_result(error_closed().await),
        "error-timed-out" => Outcome::from_result(error_timed_out().await),
        "post-close-send" => Outcome::from_result(post_close_send().await),
        "peer-wait-connected-latch" => Outcome::from_result(wait_connected_latch().await),
        "peer-streams-once" => Outcome::from_result(streams_once().await),
        "post-close-signaling" => Outcome::from_result(post_close_signaling().await),
        "send-via-stream" => Outcome::from_result(send_via_stream_round_trip(config).await),
        "receive-via-stream" => Outcome::from_result(receive_via_stream_round_trip(config).await),
        "receive-via-stream-once" => Outcome::from_result(receive_via_stream_once().await),
        _ => Outcome::from_result(inproc_round_trip(test_id).await),
    }
}

/// Feed a malformed SDP into a fresh peer connection and require an error
/// classified as `invalid-signaling`.
async fn invalid_sdp() -> Result<(), String> {
    let peer = PeerConnection::new();
    let bogus = make_sdp(SdpType::Offer, "this is not valid sdp".to_string());
    match peer.set_remote_description(bogus).await {
        Ok(()) => Err("malformed SDP was accepted".to_string()),
        Err(Error::InvalidSignaling(_)) => Ok(()),
        Err(other) => Err(format!(
            "expected invalid-signaling, got {}",
            describe(&other)
        )),
    }
}

/// Stand up two in-process peers, connect them, and exercise `test_id`'s
/// peer-connection surface over the connection.
async fn inproc_round_trip(test_id: &str) -> Result<(), String> {
    let (offerer, answerer, offer_dc, answer_dc) = inproc_connect(test_id).await?;

    // A message each way proves the channel surfaced by `create-data-channel` /
    // `incoming-data-channels` is usable.
    if !exchange_once(&offer_dc, &answer_dc).await? {
        return Err("data channel round trip failed".to_string());
    }

    offerer.close();
    answerer.close();
    Ok(())
}

/// Stand up two in-process peers (no external signaling), connect them, and
/// return both peers with the two ends of the offerer-created data channel.
async fn inproc_connect(
    test_id: &str,
) -> Result<(PeerConnection, PeerConnection, DataChannel, DataChannel), String> {
    let offerer = PeerConnection::new();
    let answerer = PeerConnection::new();

    let options = DataChannelOptions::new();
    options.set_label(CHANNEL_LABEL);
    let offer_dc = offerer
        .create_data_channel(options)
        .map_err(|e| format!("create-data-channel: {}", describe(&e)))?;

    let offer = offerer
        .create_offer()
        .await
        .map_err(|e| format!("create-offer: {}", describe(&e)))?;
    offerer
        .set_local_description(offer.clone())
        .await
        .map_err(|e| format!("offerer set-local: {}", describe(&e)))?;
    answerer
        .set_remote_description(offer)
        .await
        .map_err(|e| format!("answerer set-remote offer: {}", describe(&e)))?;
    let answer = answerer
        .create_answer()
        .await
        .map_err(|e| format!("create-answer: {}", describe(&e)))?;
    answerer
        .set_local_description(answer.clone())
        .await
        .map_err(|e| format!("answerer set-local: {}", describe(&e)))?;
    offerer
        .set_remote_description(answer)
        .await
        .map_err(|e| format!("offerer set-remote answer: {}", describe(&e)))?;

    // Trickle each side's candidates to the other. The stream ending is the
    // end-of-candidates signal.
    let offerer_candidates = collect_candidates(offerer.local_ice_candidates()).await;
    let answerer_candidates = collect_candidates(answerer.local_ice_candidates()).await;

    // `peer-local-ice-candidates` additionally asserts the local stream yielded
    // at least one candidate before ending.
    if test_id == "peer-local-ice-candidates"
        && (offerer_candidates.is_empty() || answerer_candidates.is_empty())
    {
        return Err("no local ICE candidates were gathered".to_string());
    }

    for candidate in answerer_candidates {
        offerer
            .add_ice_candidate(candidate)
            .await
            .map_err(|e| format!("offerer add-ice-candidate: {}", describe(&e)))?;
    }
    for candidate in offerer_candidates {
        answerer
            .add_ice_candidate(candidate)
            .await
            .map_err(|e| format!("answerer add-ice-candidate: {}", describe(&e)))?;
    }

    let (offerer_connected, answerer_connected) =
        futures::join!(offerer.wait_connected(), answerer.wait_connected());
    offerer_connected.map_err(|e| format!("offerer wait-connected: {}", describe(&e)))?;
    answerer_connected.map_err(|e| format!("answerer wait-connected: {}", describe(&e)))?;

    let answer_dc = first_incoming(&answerer).await?;
    Ok((offerer, answerer, offer_dc, answer_dc))
}

/// Assert the bounded-inbound-buffer contract: flood one side of a channel
/// while the other side never receives, and require that the receiving side's
/// buffer overflow closes the channel and surfaces
/// `error.receive-buffer-overflow` (not `closed`, and not unbounded buffering).
///
/// The flood volume is `message-count` messages of `message-size` bytes; the
/// runner configures it to exceed the target's inbound buffer bound (which the
/// adapters shrink through the `WEBRTC_MAX_INBOUND_BUFFER_BYTES` knob so the
/// probe needs only a small flood). If the flood never overflows the buffer,
/// the flood-side receive below never resolves and the attempt times out.
async fn receive_overflow(config: &TestConfig) -> Result<(), String> {
    let (offerer, answerer, offer_dc, answer_dc) =
        inproc_connect("receive-buffer-overflow").await?;

    // Flood without the answerer receiving. Sends may start failing once the
    // receiving side overflows and closes the channel; that ends the flood.
    let payload = vec![0xABu8; config.message_size.max(1) as usize];
    for _ in 0..config.message_count.max(1) {
        if offer_dc
            .send(Message::Binary(payload.clone()))
            .await
            .is_err()
        {
            break;
        }
    }

    // Wait for the overflow-triggered close to reach this side: the receiving
    // side closes the channel when its bounded inbound buffer overflows, and
    // nothing is ever sent toward the flooder, so a receive here resolves with
    // `closed` once the close arrives. (This wait is also what lets an
    // in-guest implementation drive its event loop while the flood drains.)
    match offer_dc.receive().await {
        Ok(_) => return Err("unexpected message on the flooding side".to_string()),
        Err(Error::Closed | Error::ReceiveBufferOverflow) => {}
        Err(other) => return Err(format!("flood-side receive: {}", describe(&other))),
    }

    // Drain the receiving side: the pre-overflow backlog (bounded by the
    // buffer) stays deliverable, after which receive must fail with
    // `receive-buffer-overflow` rather than `closed`.
    loop {
        match answer_dc.receive().await {
            Ok(_) => {}
            Err(Error::ReceiveBufferOverflow) => break,
            Err(other) => {
                return Err(format!(
                    "expected receive-buffer-overflow, got {}",
                    describe(&other)
                ))
            }
        }
    }

    offerer.close();
    answerer.close();
    Ok(())
}

// --- error-taxonomy probes ---------------------------------------------------

/// Assert that a `receive` on a locally closed channel yields `error.closed`.
async fn error_closed() -> Result<(), String> {
    let (offerer, answerer, offer_dc, _answer_dc) = inproc_connect("error-closed").await?;
    offerer.close();
    // Drain anything already in flight; the close must then surface as
    // `closed`, not any other variant.
    loop {
        match offer_dc.receive().await {
            Ok(_) => continue,
            Err(Error::Closed) => break,
            Err(other) => return Err(format!("expected closed, got {}", describe(&other))),
        }
    }
    answerer.close();
    Ok(())
}

/// Assert that a handshake that can never complete (no remote peer) surfaces
/// `error.timed-out` from `wait-connected` rather than hanging or failing with
/// another variant.
async fn error_timed_out() -> Result<(), String> {
    let peer = PeerConnection::new();
    let options = DataChannelOptions::new();
    options.set_label(CHANNEL_LABEL);
    let _dc = peer
        .create_data_channel(options)
        .map_err(|e| format!("create-data-channel: {}", describe(&e)))?;
    let offer = peer
        .create_offer()
        .await
        .map_err(|e| format!("create-offer: {}", describe(&e)))?;
    peer.set_local_description(offer)
        .await
        .map_err(|e| format!("set-local-description: {}", describe(&e)))?;

    let result = match peer.wait_connected().await {
        Ok(()) => Err("wait-connected resolved without a remote peer".to_string()),
        Err(Error::TimedOut) => Ok(()),
        Err(other) => Err(format!("expected timed-out, got {}", describe(&other))),
    };
    peer.close();
    result
}

/// Assert that peer-connection methods called after `close` fail with
/// `error.closed`, and that the gate precedes input validation (a malformed
/// description after close is `closed`, not `invalid-signaling`).
async fn post_close_signaling() -> Result<(), String> {
    let peer = PeerConnection::new();
    peer.close();

    let expect_closed = |what: &str, result: Result<(), Error>| match result {
        Err(Error::Closed) => Ok(()),
        Ok(()) => Err(format!("{what} succeeded after close")),
        Err(other) => Err(format!(
            "{what} after close: expected closed, got {}",
            describe(&other)
        )),
    };

    expect_closed("create-offer", peer.create_offer().await.map(|_| ()))?;
    expect_closed("create-answer", peer.create_answer().await.map(|_| ()))?;
    expect_closed(
        "set-local-description",
        peer.set_local_description(make_sdp(SdpType::Offer, "not sdp".to_string()))
            .await,
    )?;
    expect_closed(
        "set-remote-description",
        peer.set_remote_description(make_sdp(SdpType::Offer, "not sdp".to_string()))
            .await,
    )?;
    expect_closed(
        "add-ice-candidate",
        peer.add_ice_candidate(IceCandidate {
            candidate: "not a candidate".to_string(),
            sdp_mid: None,
            sdp_mline_index: None,
        })
        .await,
    )?;
    expect_closed(
        "create-data-channel",
        peer.create_data_channel(DataChannelOptions::new())
            .map(|_| ()),
    )?;
    Ok(())
}

/// Assert the take-once stream contract: `inproc_connect` consumed both
/// peers' `local-ice-candidates` and the answerer's `incoming-data-channels`,
/// so second calls must return streams that end immediately without yielding
/// anything (and must not re-deliver prior items).
async fn streams_once() -> Result<(), String> {
    let (offerer, answerer, _offer_dc, _answer_dc) = inproc_connect("peer-streams-once").await?;

    let candidates = collect_candidates(offerer.local_ice_candidates()).await;
    if !candidates.is_empty() {
        return Err(format!(
            "second local-ice-candidates call yielded {} candidate(s); expected an \
             immediately-ended empty stream",
            candidates.len()
        ));
    }

    let mut incoming = answerer.incoming_data_channels();
    let (_status, channels) = incoming.read(Vec::with_capacity(1)).await;
    if !channels.is_empty() {
        return Err(format!(
            "second incoming-data-channels call yielded {} channel(s); expected an \
             immediately-ended empty stream",
            channels.len()
        ));
    }

    offerer.close();
    answerer.close();
    Ok(())
}

/// Assert `wait-connected`'s latch semantics: once the connection has ever
/// connected it may be re-awaited any number of times — including after
/// `close` — and keeps resolving `ok`.
async fn wait_connected_latch() -> Result<(), String> {
    let (offerer, answerer, _offer_dc, _answer_dc) =
        inproc_connect("peer-wait-connected-latch").await?;
    offerer
        .wait_connected()
        .await
        .map_err(|e| format!("re-await after connect: {}", describe(&e)))?;
    offerer.close();
    offerer.wait_connected().await.map_err(|e| {
        format!(
            "wait-connected after close: expected ok (connected is latched), got {}",
            describe(&e)
        )
    })?;
    answerer.close();
    Ok(())
}

/// Assert that a `send` after the peer connection closes yields `error.closed`.
async fn post_close_send() -> Result<(), String> {
    let (offerer, answerer, offer_dc, _answer_dc) = inproc_connect("post-close-send").await?;
    offerer.close();
    // The close may propagate asynchronously on some hosts, so send until it
    // surfaces (each awaited send is a yield point for the host to progress).
    let mut sent = 0u32;
    let result = loop {
        match offer_dc.send(Message::Binary(vec![7u8; 8])).await {
            Ok(()) => {
                sent += 1;
                if sent > 1000 {
                    break Err("send never failed after close".to_string());
                }
            }
            Err(Error::Closed) => break Ok(()),
            Err(other) => break Err(format!("expected closed, got {}", describe(&other))),
        }
    };
    answerer.close();
    result
}

// --- streaming probes ----------------------------------------------------------

/// Read every byte of a `stream-message` payload stream until it ends.
async fn drain_byte_stream(reader: wit_bindgen::StreamReader<u8>) -> Vec<u8> {
    let mut reader = reader;
    let mut out = Vec::new();
    loop {
        let (status, chunk) = reader.read(Vec::with_capacity(8192)).await;
        out.extend_from_slice(&chunk);
        if matches!(
            status,
            wit_bindgen::StreamResult::Dropped | wit_bindgen::StreamResult::Cancelled
        ) {
            break;
        }
    }
    out
}

/// Round-trip `count` indexed payloads through `send-via-stream` on one side
/// and plain `receive` on the other, verifying payload integrity.
async fn send_via_stream_round_trip(config: &TestConfig) -> Result<(), String> {
    let (offerer, answerer, offer_dc, answer_dc) = inproc_connect("send-via-stream").await?;
    let count = config.message_count.max(1);
    let size = config.message_size.max(16);

    let send_side = async {
        let (mut tx, rx) = bindings::wit_stream::new();
        let send = offer_dc.send_via_stream(rx);
        let feed = async {
            for index in 0..count {
                let payload = make_payload(index, size);
                let length = payload.len() as u32;
                let (mut data_tx, data_rx) = bindings::wit_stream::new();
                let message = StreamMessage {
                    kind: MessageKind::Binary,
                    length,
                    data: data_rx,
                };
                if !tx.write_all(vec![message]).await.is_empty() {
                    return Err("stream-message writer closed early".to_string());
                }
                if !data_tx.write_all(payload).await.is_empty() {
                    return Err("payload writer closed early".to_string());
                }
                drop(data_tx);
            }
            drop(tx);
            Ok(())
        };
        let (sent, fed) = futures::join!(send, feed);
        fed?;
        sent.map_err(|e| {
            format!(
                "send-via-stream: {} after {} message(s)",
                describe(&e.error),
                e.sent
            )
        })
    };
    // Drain the receiving side only after the send completes: the halves are
    // deliberately not concurrent so the probe exercises the streaming send
    // form itself rather than import concurrency (which
    // `concurrent-send-receive` covers).
    send_side.await?;
    let received = recv_sequence(&answer_dc, count).await?;
    verify_all(&received, count)?;

    offerer.close();
    answerer.close();
    Ok(())
}

/// Round-trip `count` indexed payloads through plain `send` on one side and
/// `receive-via-stream` on the other, verifying the `stream-message`
/// kind/length invariants and payload integrity.
async fn receive_via_stream_round_trip(config: &TestConfig) -> Result<(), String> {
    let (offerer, answerer, offer_dc, answer_dc) = inproc_connect("receive-via-stream").await?;
    let count = config.message_count.max(1);
    let size = config.message_size.max(16);

    // Send everything first, then claim and read the stream: the two halves
    // are deliberately not concurrent so the probe exercises the streaming
    // receive form itself rather than import concurrency (which
    // `concurrent-send-receive` covers).
    send_sequence(&offer_dc, count, size).await?;
    let recv_side = async {
        let mut stream = answer_dc
            .receive_via_stream()
            .map_err(|e| format!("receive-via-stream: {}", describe(&e)))?;
        let mut received: Vec<Vec<u8>> = Vec::with_capacity(count as usize);
        while received.len() < count as usize {
            let (status, batch) = stream.read(Vec::with_capacity(1)).await;
            for message in batch {
                let is_text = matches!(message.kind, MessageKind::String);
                let declared = message.length as usize;
                let bytes = drain_byte_stream(message.data).await;
                if bytes.len() != declared {
                    return Err(format!(
                        "stream-message declared {declared} bytes but carried {}",
                        bytes.len()
                    ));
                }
                if is_text && String::from_utf8(bytes.clone()).is_err() {
                    return Err("text stream-message payload is not UTF-8".to_string());
                }
                received.push(bytes);
            }
            if matches!(
                status,
                wit_bindgen::StreamResult::Dropped | wit_bindgen::StreamResult::Cancelled
            ) && received.len() < count as usize
            {
                return Err(format!(
                    "stream ended after {} of {count} message(s)",
                    received.len()
                ));
            }
        }
        Ok(received)
    };
    let received = recv_side.await?;
    verify_all(&received, count)?;

    offerer.close();
    answerer.close();
    Ok(())
}

/// Assert `receive-via-stream`'s once-only semantics: the first call claims the
/// inbound messages (resolving any pending `receive` with
/// `error.receiving-via-stream`), and every later `receive-via-stream` or
/// `receive` fails with the same variant.
async fn receive_via_stream_once() -> Result<(), String> {
    let (offerer, answerer, _offer_dc, answer_dc) =
        inproc_connect("receive-via-stream-once").await?;

    // A receive pending when the stream claims the channel must resolve with
    // `receiving-via-stream`. `join!` polls in order: the receive starts first,
    // then the claim is made.
    let pending = answer_dc.receive();
    let claim = async {
        answer_dc
            .receive_via_stream()
            .map_err(|e| format!("first receive-via-stream: {}", describe(&e)))
    };
    let (pending, stream) = futures::join!(pending, claim);
    let _stream = stream?;
    match pending {
        Err(Error::ReceivingViaStream) => {}
        Ok(_) => return Err("pending receive yielded a message".to_string()),
        Err(other) => {
            return Err(format!(
                "pending receive: expected receiving-via-stream, got {}",
                describe(&other)
            ))
        }
    }

    // A second claim fails.
    match answer_dc.receive_via_stream() {
        Err(Error::ReceivingViaStream) => {}
        Ok(_) => return Err("second receive-via-stream succeeded".to_string()),
        Err(other) => {
            return Err(format!(
                "second receive-via-stream: expected receiving-via-stream, got {}",
                describe(&other)
            ))
        }
    }

    // And so does any later receive.
    match answer_dc.receive().await {
        Err(Error::ReceivingViaStream) => {}
        Ok(_) => return Err("receive after claim yielded a message".to_string()),
        Err(other) => {
            return Err(format!(
                "receive after claim: expected receiving-via-stream, got {}",
                describe(&other)
            ))
        }
    }

    offerer.close();
    answerer.close();
    Ok(())
}

/// Exchange one message each way; return whether both arrived intact.
async fn exchange_once(a: &DataChannel, b: &DataChannel) -> Result<bool, String> {
    let a_side = async {
        send(a, Message::Binary(vec![1, 2, 3])).await?;
        match receive(a).await? {
            Message::Binary(bytes) => Ok::<bool, String>(bytes == vec![4, 5, 6]),
            Message::String(_) => Ok(false),
        }
    };
    let b_side = async {
        match receive(b).await? {
            Message::Binary(bytes) if bytes == vec![1, 2, 3] => {}
            _ => return Ok::<bool, String>(false),
        }
        send(b, Message::Binary(vec![4, 5, 6])).await?;
        Ok(true)
    };
    let (a_ok, b_ok) = futures::join!(a_side, b_side);
    Ok(a_ok? && b_ok?)
}

// --- helpers ---------------------------------------------------------------

/// The opaque signaling blob schema the guest owns (JSON over the mailbox).
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

/// Build a `session-description` from a kind and SDP string.
fn make_sdp(kind: SdpType, sdp: String) -> SessionDescription {
    SessionDescription { kind, sdp }
}

/// The data-channel options for a test (label, plus test-specific knobs).
fn channel_options(test_id: &str) -> DataChannelOptions {
    let options = DataChannelOptions::new();
    options.set_label(CHANNEL_LABEL);
    if test_id == "max-retransmits-accepted" {
        options.set_max_retransmits(Some(0));
    }
    options
}

/// Publish one signal blob to the session's own mailbox.
async fn publish(session: &Session, signal: &Signal) -> Result<(), String> {
    let blob = serde_json::to_vec(signal).map_err(|e| format!("encode signal: {e}"))?;
    session
        .send(blob)
        .await
        .map_err(|e| format!("mailbox send: {}", describe(&e)))
}

/// Mark the session's own mailbox as done.
async fn done(session: &Session) -> Result<(), String> {
    session
        .done()
        .await
        .map_err(|e| format!("mailbox done: {}", describe(&e)))
}

/// Fetch and decode the next signal from the peer's mailbox, or `None` at end.
async fn recv_signal(session: &Session) -> Result<Option<Signal>, String> {
    match session
        .recv()
        .await
        .map_err(|e| format!("mailbox recv: {}", describe(&e)))?
    {
        Some(blob) => {
            let signal =
                serde_json::from_slice(&blob).map_err(|e| format!("decode signal: {e}"))?;
            Ok(Some(signal))
        }
        None => Ok(None),
    }
}

/// Send a message, mapping the WIT error to a detail string.
async fn send(dc: &DataChannel, message: Message) -> Result<(), String> {
    dc.send(message)
        .await
        .map_err(|e| format!("send: {}", describe(&e)))
}

/// Receive a message, mapping the WIT error to a detail string.
async fn receive(dc: &DataChannel) -> Result<Message, String> {
    dc.receive()
        .await
        .map_err(|e| format!("receive: {}", describe(&e)))
}

/// Adopt the first data channel the remote peer opens.
async fn first_incoming(peer: &PeerConnection) -> Result<DataChannel, String> {
    let mut stream = peer.incoming_data_channels();
    let (_status, batch) = stream.read(Vec::with_capacity(1)).await;
    batch
        .into_iter()
        .next()
        .ok_or_else(|| "no incoming data channel".to_string())
}

/// Drain a `local-ice-candidates` stream to its end.
async fn collect_candidates(stream: wit_bindgen::StreamReader<IceCandidate>) -> Vec<IceCandidate> {
    let mut stream = stream;
    let mut out = Vec::new();
    loop {
        let (status, batch) = stream.read(Vec::with_capacity(4)).await;
        out.extend(batch);
        if matches!(
            status,
            wit_bindgen::StreamResult::Dropped | wit_bindgen::StreamResult::Cancelled
        ) {
            break;
        }
    }
    out
}

/// A short, non-matched description of a WIT `error` for failure details.
fn describe(error: &Error) -> String {
    match error {
        Error::Closed => "closed".to_string(),
        Error::TimedOut => "timed-out".to_string(),
        Error::InvalidSignaling(detail) => format!("invalid-signaling: {detail}"),
        Error::ReceivingViaStream => "receiving-via-stream".to_string(),
        Error::ReceiveBufferOverflow => "receive-buffer-overflow".to_string(),
        Error::Other(detail) => format!("other: {detail}"),
    }
}

/// Build an indexed, verifiable payload of `size` bytes (minimum 4).
fn make_payload(index: u32, size: u32) -> Vec<u8> {
    let size = size.max(4) as usize;
    let mut bytes = Vec::with_capacity(size);
    bytes.extend_from_slice(&index.to_le_bytes());
    for offset in 0..(size - 4) {
        bytes.push(((index as usize + offset) % 251) as u8);
    }
    bytes
}

/// The index stored in a payload's first four bytes, if present.
fn payload_index(bytes: &[u8]) -> Option<u32> {
    bytes
        .get(0..4)
        .map(|head| u32::from_le_bytes(head.try_into().unwrap()))
}

/// Whether a payload matches the pattern [`make_payload`] produced.
fn verify_payload(bytes: &[u8]) -> bool {
    let Some(index) = payload_index(bytes) else {
        return false;
    };
    bytes[4..]
        .iter()
        .enumerate()
        .all(|(offset, byte)| *byte == ((index as usize + offset) % 251) as u8)
}
