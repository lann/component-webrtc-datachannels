//! The conformance case bodies, shared by the two suite components.
//!
//! Ported from the incumbent conformance guest (same assertions, same
//! stimulus, same one-line failure details), re-plumbed for the
//! `polymorph:test` harness:
//!
//! - Per-run configuration arrives through the store environment
//!   (`RTC_CT_*`, set by the driver) instead of a WIT-record argument —
//!   the tests contract has no per-case config channel, and the
//!   environment is the wasip2-native equivalent.
//! - A two-peer case derives its signaling room from the driver-issued
//!   run id and its own case id, so the two suite instances of a pair
//!   rendezvous per case without any per-case channel.
//! - Message counts and sizes are the suite's own ([`params`]): every
//!   target runs the identical workload by construction.
//!
//! Assertions target interoperable behavior only — never SDP contents,
//! candidate ordering, timing, or exact error strings.

use serde::{Deserialize, Serialize};

pub mod bindings {
    wit_bindgen::generate!({
        path: "../wit",
        world: "sut-imports",
        generate_all,
    });
}

use bindings::conformance::signaling::mailbox::{Role as MailboxRole, Session};
use bindings::polymorph::webrtc_datachannels::connections::{
    DataChannel, DataChannelOptions, PeerConnection, PeerConnectionConfig,
};
use bindings::polymorph::webrtc_datachannels::types::{
    ConfigError, ConnectionState, DataChannelState, Error, IceCandidate, IceServer,
    IceTransportPolicy, Message, MessageKind, SdpType, SessionDescription, StreamMessage,
};

/// The negotiated data-channel label used by every behavioral test. Both peers
/// observe it identically.
const CHANNEL_LABEL: &str = "conformance";

/// Message count and payload size for a count-parameterized case. The suite
/// owns its stimulus (drivers cannot scale it); the overflow probe's flood
/// (64 x 16 KiB = 1 MiB) is sized against the 512 KiB inbound-buffer bound
/// the drivers set through `WEBRTC_MAX_INBOUND_BUFFER_BYTES`.
fn params(id: &str) -> (u32, u32) {
    match id {
        "large-message" => (1, 16 * 1024),
        "receive-buffer-overflow" => (64, 16 * 1024),
        "message-boundaries"
        | "ordering"
        | "payload-integrity"
        | "concurrent-send-receive"
        | "interop-handshake" => (16, 512),
        _ => (4, 256),
    }
}

/// This instance's half of a two-peer case, read once from the store
/// environment (set by the driver on every pair instance).
struct PairConfig {
    role: MailboxRole,
    signaling_url: String,
    run_id: String,
}

fn pair_config() -> Result<PairConfig, String> {
    let var = |name: &str| {
        std::env::var(name)
            .map_err(|_| format!("harness bug: {name} is not set on a pair instance"))
    };
    let role = match var("RTC_CT_ROLE")?.as_str() {
        "offerer" => MailboxRole::Offerer,
        "answerer" => MailboxRole::Answerer,
        other => return Err(format!("harness bug: RTC_CT_ROLE={other:?}")),
    };
    Ok(PairConfig {
        role,
        signaling_url: var("RTC_CT_SIGNALING_URL")?,
        run_id: var("RTC_CT_RUN_ID")?,
    })
}

/// Run one single-instance case body (two in-process peer connections, or a
/// lone peer for the error probes) to its verdict detail.
pub async fn solo(id: &str) -> Result<(), String> {
    match id {
        "error-invalid-signaling" | "peer-invalid-sdp" => invalid_sdp().await,
        "receive-buffer-overflow" => receive_overflow().await,
        "error-closed" => error_closed().await,
        "error-timed-out" => error_timed_out().await,
        "post-close-send" => post_close_send().await,
        "peer-wait-connected-latch" => wait_connected_latch().await,
        "peer-streams-once" => streams_once().await,
        "post-close-signaling" => post_close_signaling().await,
        "send-via-stream" => send_via_stream_round_trip().await,
        "receive-via-stream" => receive_via_stream_round_trip().await,
        "receive-via-stream-once" => receive_via_stream_once().await,
        "config-defaults" => config_defaults().await,
        "config-setters-contract" => config_setters_contract().await,
        "config-invalid-ice-server" => config_invalid_ice_server().await,
        "connection-state-changes" => connection_state_changes().await,
        "channel-state-changes" => channel_state_changes().await,
        "channel-post-close-receive" => channel_post_close_receive().await,
        "channel-drop-implies-close" => channel_drop_implies_close().await,
        _ => inproc_round_trip(id).await,
    }
}

/// Run one half of a two-peer case body to its verdict detail: complete the
/// mailbox-driven handshake for this instance's role (from the environment),
/// then run the per-case payload exchange over the connected data channel.
///
/// Both instances run this same routine over one signaling room, derived from
/// the driver-issued run id and the case id — unique per (run, case), so
/// re-instantiated instances and concurrent cases never collide.
pub async fn pair(id: &str) -> Result<(), String> {
    let config = pair_config()?;
    let room = format!("{}-{id}", config.run_id);
    let role = config.role;

    let session = match Session::open(config.signaling_url.clone(), room, role).await {
        Ok(session) => session,
        Err(err) => return Err(format!("mailbox open: {}", describe(&err))),
    };

    let handshake = match role {
        MailboxRole::Offerer => handshake_offerer(id, &session).await,
        MailboxRole::Answerer => handshake_answerer(&session).await,
    };
    let (peer, dc) = handshake?;

    // Run the per-case assertions, then rendezvous over the data channel before
    // tearing down, so neither peer closes the connection while the other still
    // needs it — to receive the channel (`label-round-trip` transfers no
    // payload) or to drain the last buffered messages.
    // `channel-close-flush` owns its completion protocol (the channel close it
    // asserts *is* the rendezvous) and needs the channel by value.
    let (count, size) = params(id);
    let outcome = if id == "channel-close-flush" {
        close_flush(role, count, size, dc).await
    } else {
        let outcome = match exchange(id, count, size, &dc).await {
            Ok(()) => barrier(&dc).await,
            Err(detail) => Err(detail),
        };
        if outcome.is_ok() {
            // Graceful channel shutdown before the connection close: the
            // flush-aware `data-channel.close` transmits this side's queued
            // barrier sentinel (an abrupt `peer-connection.close` may drop
            // it, wedging a peer whose stack cannot observe the teardown —
            // the connection-level face of issue #123), and awaiting the
            // `closed` state drives the implementation's event loop through
            // the flush.
            let states = dc.state_changes();
            dc.close();
            let _ = drain_states(states, |s| matches!(s, DataChannelState::Closed)).await;
        }
        outcome
    };
    peer.close();
    outcome
}

/// `channel-close-flush`: the offerer sends the corpus payloads and closes the
/// channel immediately; every payload must still reach the answerer (the WIT's
/// flush-aware close), whose next receive then observes the close. The
/// offerer awaits its own `state-changes` reaching `closed` — the flush
/// signal — before returning, so its connection teardown cannot race the
/// flush.
async fn close_flush(
    role: MailboxRole,
    count: u32,
    size: u32,
    dc: DataChannel,
) -> Result<(), String> {
    let count = count.max(1);
    let size = size.max(16);
    match role {
        MailboxRole::Offerer => {
            send_sequence(&dc, count, size).await?;
            let states = dc.state_changes();
            dc.close();
            let states = drain_states(states, |s| matches!(s, DataChannelState::Closed)).await?;
            if states.last() != Some(&DataChannelState::Closed) {
                return Err(format!(
                    "expected state-changes to end with closed after close, got {states:?}"
                ));
            }
            Ok(())
        }
        MailboxRole::Answerer => {
            let received = recv_sequence(&dc, count).await?;
            verify_all(&received, count)?;
            // The peer closed right after its last send: the flush-aware close
            // means every payload arrived above, and the close arrives here.
            match receive(&dc).await {
                Err(detail) if detail.ends_with("closed") => Ok(()),
                Ok(_) => Err("received past the peer's close".to_string()),
                Err(detail) => Err(format!("waiting for the peer's close: {detail}")),
            }
        }
    }
}

/// Drive the offerer half of the handshake, returning the connected peer and the
/// data channel it created.
async fn handshake_offerer(
    id: &str,
    session: &Session,
) -> Result<(PeerConnection, DataChannel), String> {
    let peer = PeerConnection::new(None);
    let dc = peer
        .create_data_channel(channel_options(id))
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
    let peer = PeerConnection::new(None);

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

// --- per-case payload exchange ----------------------------------------------

/// Run the payload exchange for `id` over the connected data channel. Both
/// peers run the same routine.
async fn exchange(id: &str, count: u32, size: u32, dc: &DataChannel) -> Result<(), String> {
    match id {
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
            let size = size.max(1024);
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
            let count = count.max(1);
            let size = size.max(16);
            let sender = send_sequence(dc, count, size);
            let receiver = recv_sequence(dc, count);
            let (sent, received) = futures::join!(sender, receiver);
            sent?;
            verify_all(&received?, count)
        }
        // Count-parameterized payload tests plus the flagship interop handshake.
        "message-boundaries" | "ordering" | "payload-integrity" | "interop-handshake" => {
            let count = count.max(1);
            let size = size.max(16);
            let sender = send_sequence(dc, count, size);
            let receiver = recv_sequence(dc, count);
            let (sent, received) = futures::join!(sender, receiver);
            sent?;
            let received = received?;
            if id == "ordering" {
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
/// while the other still needs it. On a reliable, ordered channel the sentinel
/// arrives after any test payloads. (`max-retransmits-accepted` runs the
/// barrier over its `max-retransmits = 0` channel, whose sentinel a lossy path
/// could drop; the corpus currently runs only on lossless paths, where SCTP
/// still delivers it.) A `closed` error counts as the
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

// --- in-process peer-connection API cases -------------------------------------

/// `peer-connection-config` defaults: the getters report no ICE servers and
/// the `all` policy, and a connection constructed with a default config (like
/// one constructed with `none`) is usable.
async fn config_defaults() -> Result<(), String> {
    let config = PeerConnectionConfig::new();
    if !config.ice_servers().is_empty() {
        return Err("default config has ice servers".to_string());
    }
    if !matches!(config.ice_transport_policy(), IceTransportPolicy::All) {
        return Err("default config policy is not `all`".to_string());
    }
    let peer = PeerConnection::new(Some(config));
    let options = DataChannelOptions::new();
    options.set_label(CHANNEL_LABEL);
    peer.create_data_channel(options)
        .map_err(|e| format!("create-data-channel: {}", describe(&e)))?;
    peer.create_offer()
        .await
        .map_err(|e| format!("create-offer: {}", describe(&e)))?;
    peer.close();
    Ok(())
}

/// The fallible-setter contract: each capability-gated setter either accepts
/// (and its getter reflects the stored value) or fails `not-supported` (and
/// the getter is unchanged) — never a trap, never a silent ignore. Whatever
/// was accepted must then construct a working connection (accepted implies
/// honored).
async fn config_setters_contract() -> Result<(), String> {
    let config = PeerConnectionConfig::new();

    // A syntactically valid STUN server on an unroutable (TEST-NET-1)
    // address: acceptance is about capability, not reachability.
    let server = IceServer {
        urls: vec!["stun:192.0.2.1:3478".to_string()],
        username: String::new(),
        credential: String::new(),
    };
    match config.set_ice_servers(std::slice::from_ref(&server)) {
        Ok(()) => {
            let stored = config.ice_servers();
            if stored.len() != 1 || stored[0].urls != server.urls {
                return Err("accepted ice servers not reflected by the getter".to_string());
            }
        }
        Err(ConfigError::NotSupported) => {
            if !config.ice_servers().is_empty() {
                return Err("rejected ice servers changed the getter".to_string());
            }
        }
        Err(ConfigError::Invalid(detail)) => {
            return Err(format!("valid ice server rejected as invalid: {detail}"));
        }
    }

    match config.set_ice_transport_policy(IceTransportPolicy::Relay) {
        Ok(()) => {
            if !matches!(config.ice_transport_policy(), IceTransportPolicy::Relay) {
                return Err("accepted relay policy not reflected by the getter".to_string());
            }
            // A relay-only connection without a reachable TURN server cannot
            // connect, but constructing it must work; put the policy back so
            // the construction probe below exercises the accepted servers.
            config
                .set_ice_transport_policy(IceTransportPolicy::All)
                .map_err(|e| format!("resetting policy failed: {e:?}"))?;
        }
        Err(ConfigError::NotSupported) => {
            if !matches!(config.ice_transport_policy(), IceTransportPolicy::All) {
                return Err("rejected relay policy changed the getter".to_string());
            }
        }
        Err(ConfigError::Invalid(detail)) => {
            return Err(format!("relay policy rejected as invalid: {detail}"));
        }
    }

    // Accepted implies honored: the config constructs a connection that can
    // produce an offer.
    let peer = PeerConnection::new(Some(config));
    peer.create_offer()
        .await
        .map_err(|e| format!("create-offer with accepted config: {}", describe(&e)))?;
    peer.close();
    Ok(())
}

/// Malformed ICE-server entries are rejected eagerly (`invalid`, or
/// `not-supported` where servers are unsupported altogether), and a rejected
/// value is never latently fatal: the config still constructs a working
/// connection afterwards.
async fn config_invalid_ice_server() -> Result<(), String> {
    let config = PeerConnectionConfig::new();

    let no_urls = IceServer {
        urls: Vec::new(),
        username: String::new(),
        credential: String::new(),
    };
    if config.set_ice_servers(&[no_urls]).is_ok() {
        return Err("ice server without urls was accepted".to_string());
    }

    let bad_scheme = IceServer {
        urls: vec!["http://example.com".to_string()],
        username: String::new(),
        credential: String::new(),
    };
    if config.set_ice_servers(&[bad_scheme]).is_ok() {
        return Err("ice server with a non-ICE scheme was accepted".to_string());
    }

    let peer = PeerConnection::new(Some(config));
    peer.create_offer()
        .await
        .map_err(|e| format!("create-offer after rejected sets: {}", describe(&e)))?;
    peer.close();
    Ok(())
}

/// Read every element of a state stream until it ends, verifying the
/// coalescing-watch contract as it goes: no consecutive duplicates, and
/// nothing after the first terminal element.
async fn drain_states<T: Copy + PartialEq + core::fmt::Debug>(
    mut stream: wit_bindgen::StreamReader<T>,
    is_terminal: impl Fn(T) -> bool,
) -> Result<Vec<T>, String> {
    let mut states = Vec::new();
    loop {
        let (status, batch) = stream.read(Vec::with_capacity(1)).await;
        for state in batch {
            if states.last() == Some(&state) {
                return Err(format!("consecutive duplicate state {state:?}"));
            }
            if states.last().copied().is_some_and(&is_terminal) {
                return Err(format!("state {state:?} delivered after a terminal state"));
            }
            states.push(state);
        }
        if matches!(
            status,
            wit_bindgen::StreamResult::Dropped | wit_bindgen::StreamResult::Cancelled
        ) {
            return Ok(states);
        }
    }
}

/// `peer-connection.state-changes`: on a connected pair the watch reads
/// `connected`, ends with `closed` after a local close, never duplicates
/// consecutively, and is take-once.
async fn connection_state_changes() -> Result<(), String> {
    let (offerer, answerer, _offer_dc, _answer_dc) =
        inproc_connect("connection-state-changes").await?;

    let mut states = offerer.state_changes();
    let (_, first) = states.read(Vec::with_capacity(1)).await;
    if first.first() != Some(&ConnectionState::Connected) {
        return Err(format!(
            "expected connected as the state at first read, got {first:?}"
        ));
    }

    // Take-once: a second stream ends immediately without yielding.
    let taken_again = drain_states(offerer.state_changes(), |s| {
        matches!(s, ConnectionState::Failed | ConnectionState::Closed)
    })
    .await?;
    if !taken_again.is_empty() {
        return Err("second state-changes stream yielded elements".to_string());
    }

    offerer.close();
    let rest = drain_states(states, |s| {
        matches!(s, ConnectionState::Failed | ConnectionState::Closed)
    })
    .await?;
    if rest.last() != Some(&ConnectionState::Closed) {
        return Err(format!(
            "expected the stream to end with closed, got {rest:?}"
        ));
    }

    answerer.close();
    Ok(())
}

/// `data-channel.state-changes`: an open channel's watch reads `open`, ends
/// with `closed` after a local close, and is take-once.
async fn channel_state_changes() -> Result<(), String> {
    let (offerer, answerer, offer_dc, answer_dc) = inproc_connect("channel-state-changes").await?;
    if !exchange_once(&offer_dc, &answer_dc).await? {
        return Err("data channel round trip failed".to_string());
    }

    let mut states = offer_dc.state_changes();
    let (_, first) = states.read(Vec::with_capacity(1)).await;
    if first.first() != Some(&DataChannelState::Open) {
        return Err(format!(
            "expected open as the state at first read, got {first:?}"
        ));
    }

    let taken_again = drain_states(offer_dc.state_changes(), |s| {
        matches!(s, DataChannelState::Closed)
    })
    .await?;
    if !taken_again.is_empty() {
        return Err("second state-changes stream yielded elements".to_string());
    }

    offer_dc.close();
    let rest = drain_states(states, |s| matches!(s, DataChannelState::Closed)).await?;
    if rest.last() != Some(&DataChannelState::Closed) {
        return Err(format!(
            "expected the stream to end with closed, got {rest:?}"
        ));
    }

    offerer.close();
    answerer.close();
    Ok(())
}

/// `data-channel.close` locally: idempotent; calls made after fail `closed`
/// (the unread backlog is discarded, per the WIT contract).
async fn channel_post_close_receive() -> Result<(), String> {
    let (offerer, answerer, offer_dc, answer_dc) =
        inproc_connect("channel-post-close-receive").await?;
    if !exchange_once(&offer_dc, &answer_dc).await? {
        return Err("data channel round trip failed".to_string());
    }

    // Leave a message unread on the offerer side, then close: the backlog is
    // discarded and every later call fails `closed`.
    send(&answer_dc, Message::Binary(vec![1, 2, 3])).await?;
    offer_dc.close();
    offer_dc.close(); // idempotent

    match receive(&offer_dc).await {
        Err(detail) if detail.ends_with("closed") => {}
        Ok(_) => return Err("receive after close delivered a message".to_string()),
        Err(detail) => return Err(format!("receive after close: {detail}")),
    }
    match offer_dc.send(Message::Binary(vec![4])).await {
        Err(Error::Closed) => {}
        Ok(()) => return Err("send after close succeeded".to_string()),
        Err(other) => return Err(format!("send after close: {}", describe(&other))),
    }
    match offer_dc.receive_via_stream() {
        Err(Error::Closed) => {}
        Ok(_) => return Err("receive-via-stream after close returned a stream".to_string()),
        Err(other) => {
            return Err(format!(
                "receive-via-stream after close: {}",
                describe(&other)
            ))
        }
    }

    offerer.close();
    answerer.close();
    Ok(())
}

/// Dropping a `data-channel` resource without calling `close` implies
/// `close`: the remote end observes the channel closing while the connection
/// itself stays alive (attributing the close to the drop, not to teardown).
async fn channel_drop_implies_close() -> Result<(), String> {
    let (offerer, answerer, offer_dc, answer_dc) =
        inproc_connect("channel-drop-implies-close").await?;

    // A probe proves the channel worked before the drop (and, flushed by the
    // implied close, must still arrive).
    send(&offer_dc, Message::Binary(vec![7u8; 8])).await?;
    drop(offer_dc);

    match receive(&answer_dc).await? {
        Message::Binary(bytes) if bytes == vec![7u8; 8] => {}
        other => return Err(format!("probe mismatch: {other:?}")),
    }
    // The implied close reaches the remote end as this channel closing.
    match receive(&answer_dc).await {
        Err(detail) if detail.ends_with("closed") => {}
        Ok(_) => return Err("received past the dropped channel's close".to_string()),
        Err(detail) => return Err(format!("waiting for the drop-close: {detail}")),
    }

    offerer.close();
    answerer.close();
    Ok(())
}

/// Feed a malformed SDP into a fresh peer connection and require an error
/// classified as `invalid-signaling`.
async fn invalid_sdp() -> Result<(), String> {
    let peer = PeerConnection::new(None);
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

/// Stand up two in-process peers, connect them, and exercise `id`'s
/// peer-connection surface over the connection.
async fn inproc_round_trip(id: &str) -> Result<(), String> {
    let (offerer, answerer, offer_dc, answer_dc) = inproc_connect(id).await?;

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
    id: &str,
) -> Result<(PeerConnection, PeerConnection, DataChannel, DataChannel), String> {
    let offerer = PeerConnection::new(None);
    let answerer = PeerConnection::new(None);

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
    if id == "peer-local-ice-candidates"
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
/// The flood volume is [`params`]' count x size (1 MiB), sized to exceed the
/// 512 KiB inbound buffer bound the drivers set through
/// `WEBRTC_MAX_INBOUND_BUFFER_BYTES`. If the flood never overflows the
/// buffer, the flood-side receive below never resolves and the case's wall
/// bound trips.
async fn receive_overflow() -> Result<(), String> {
    let (count, size) = params("receive-buffer-overflow");
    let (offerer, answerer, offer_dc, answer_dc) =
        inproc_connect("receive-buffer-overflow").await?;

    // Flood without the answerer receiving. Sends may start failing once the
    // receiving side overflows and closes the channel; that ends the flood.
    let payload = vec![0xABu8; size.max(1) as usize];
    for _ in 0..count.max(1) {
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

// --- error-taxonomy probes -----------------------------------------------------

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
    let peer = PeerConnection::new(None);
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
    let peer = PeerConnection::new(None);
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

// --- streaming probes ------------------------------------------------------------

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
async fn send_via_stream_round_trip() -> Result<(), String> {
    let (offerer, answerer, offer_dc, answer_dc) = inproc_connect("send-via-stream").await?;
    let (count, size) = params("send-via-stream");
    let count = count.max(1);
    let size = size.max(16);

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
async fn receive_via_stream_round_trip() -> Result<(), String> {
    let (offerer, answerer, offer_dc, answer_dc) = inproc_connect("receive-via-stream").await?;
    let (count, size) = params("receive-via-stream");
    let count = count.max(1);
    let size = size.max(16);

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

// --- helpers -----------------------------------------------------------------

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

/// The data-channel options for a case (label, plus case-specific knobs).
fn channel_options(id: &str) -> DataChannelOptions {
    let options = DataChannelOptions::new();
    options.set_label(CHANNEL_LABEL);
    if id == "max-retransmits-accepted" {
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
