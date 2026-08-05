//! `echo-remote`: one peer of a two-process WebRTC echo run.
//!
//! Unlike `echo-demo` (which stands up both peers inside a single component
//! instance), each `echo-remote` instance drives exactly **one** peer and
//! reaches the other through the demo `rendezvous` signaling mailbox: two
//! genuinely separate component instances — separate OS processes, potentially
//! separate machines — exchange their SDP offer/answer and trickled ICE
//! candidates through an HTTP signaling server and form one real WebRTC
//! connection over the standard `polymorph:webrtc-datachannels/connections`
//! interface.
//!
//! The offerer creates the `echo` channel, sends `message-count` deterministic
//! indexed messages, and verifies every echo byte-for-byte and in order; the
//! answerer adopts the incoming channel and echoes each message back verbatim
//! until the channel closes. The opaque rendezvous blobs carry JSON `Signal`
//! values (offer / answer / candidate / end-of-candidates) — the same schema
//! the conformance guest uses over its mailbox.

wit_bindgen::generate!({
    path: "../echo-demo/wit",
    world: "webrtc-echo-remote",
    generate_all,
});

use demo::webrtc_echo::rendezvous::{Role, Session};
use exports::demo::webrtc_echo::remote::{DemoStats, Guest, RemoteConfig};
use polymorph::webrtc_datachannels::connections::{
    DataChannel, DataChannelOptions, PeerConnection,
};
use polymorph::webrtc_datachannels::types::{
    Error, IceCandidate, Message, SdpType, SessionDescription,
};
use serde::{Deserialize, Serialize};

struct Component;

impl Guest for Component {
    async fn run(config: RemoteConfig) -> Result<DemoStats, Error> {
        let session =
            Session::open(config.server.clone(), config.room.clone(), config.role).await?;

        let (peer, channel) = match config.role {
            Role::Offerer => handshake_offerer(&session).await?,
            Role::Answerer => handshake_answerer(&session).await?,
        };

        let stats = match config.role {
            Role::Offerer => {
                offer_exchange(&channel, config.message_count, config.message_size as usize).await
            }
            Role::Answerer => echo_until_closed(&channel).await,
        };
        peer.close();
        stats
    }
}

/// Drive the offerer half of the rendezvous handshake, returning the connected
/// peer and the data channel it created.
async fn handshake_offerer(session: &Session) -> Result<(PeerConnection, DataChannel), Error> {
    let peer = PeerConnection::new(None);
    let options = DataChannelOptions::new();
    options.set_label("echo");
    options.set_ordered(true);
    let channel = peer.create_data_channel(options)?;

    let offer = peer.create_offer().await?;
    let offer_sdp = offer.sdp.clone();
    peer.set_local_description(offer).await?;

    publish(session, &Signal::Offer { sdp: offer_sdp }).await?;
    publish_candidates(&peer, session).await?;
    session.done().await?;

    // Consume the answer and the peer's trickled candidates.
    consume_signaling(&peer, session).await?;

    peer.wait_connected().await?;
    Ok((peer, channel))
}

/// Drive the answerer half of the rendezvous handshake, returning the
/// connected peer and the data channel the offerer opened.
async fn handshake_answerer(session: &Session) -> Result<(PeerConnection, DataChannel), Error> {
    let peer = PeerConnection::new(None);

    // The offerer publishes its offer first.
    let offer = match recv_signal(session).await? {
        Some(Signal::Offer { sdp }) => sdp,
        other => {
            return Err(Error::InvalidSignaling(format!(
                "expected offer, got {other:?}"
            )))
        }
    };
    peer.set_remote_description(SessionDescription {
        kind: SdpType::Offer,
        sdp: offer,
    })
    .await?;

    let answer = peer.create_answer().await?;
    let answer_sdp = answer.sdp.clone();
    peer.set_local_description(answer).await?;

    publish(session, &Signal::Answer { sdp: answer_sdp }).await?;
    publish_candidates(&peer, session).await?;
    session.done().await?;

    // Consume the offerer's trickled candidates (the offer was already read).
    consume_signaling(&peer, session).await?;

    peer.wait_connected().await?;

    let mut incoming = peer.incoming_data_channels();
    let (_status, batch) = incoming.read(Vec::with_capacity(1)).await;
    let channel = batch
        .into_iter()
        .next()
        .ok_or_else(|| Error::Other("no incoming data channel".to_string()))?;
    Ok((peer, channel))
}

/// The offerer's exchange: send `count` deterministic indexed messages and
/// concurrently verify every echo byte-for-byte and in order.
async fn offer_exchange(
    channel: &DataChannel,
    count: u32,
    size: usize,
) -> Result<DemoStats, Error> {
    let send_fut = async {
        for i in 0..count {
            channel.send(Message::Binary(make_message(size, i))).await?;
        }
        Ok::<(), Error>(())
    };
    let recv_fut = async {
        let mut messages_received: u32 = 0;
        let mut bytes_echoed: u64 = 0;
        while messages_received < count {
            let message = channel.receive().await?;
            let expected = make_message(size, messages_received);
            match &message {
                Message::Binary(bytes) if *bytes == expected => {}
                other => {
                    return Err(Error::Other(format!(
                        "message {messages_received} corrupted or out of order (got {} bytes)",
                        message_len(other),
                    )));
                }
            }
            messages_received += 1;
            bytes_echoed += message_len(&message) as u64;
        }
        Ok((messages_received, bytes_echoed))
    };

    let (send_result, recv_result) = futures::join!(send_fut, recv_fut);
    send_result?;
    let (messages_received, bytes_echoed) = recv_result?;
    Ok(DemoStats {
        messages_sent: count,
        messages_received,
        bytes_echoed,
    })
}

/// The answerer's exchange: echo every received message back verbatim until
/// the channel closes (the offerer closes once it has verified every echo).
async fn echo_until_closed(channel: &DataChannel) -> Result<DemoStats, Error> {
    let mut echoed: u32 = 0;
    let mut bytes: u64 = 0;
    loop {
        match channel.receive().await {
            Ok(message) => {
                bytes += message_len(&message) as u64;
                channel.send(message).await?;
                echoed += 1;
            }
            // The offerer verified every echo and closed the connection.
            Err(Error::Closed) => break,
            Err(other) => return Err(other),
        }
    }
    Ok(DemoStats {
        messages_sent: echoed,
        messages_received: echoed,
        bytes_echoed: bytes,
    })
}

// --- rendezvous signaling ----------------------------------------------------

/// The opaque signaling blob schema (JSON over the rendezvous mailbox) — the
/// same schema the conformance guest uses.
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

/// Publish one signal blob to the session's own mailbox.
async fn publish(session: &Session, signal: &Signal) -> Result<(), Error> {
    let blob =
        serde_json::to_vec(signal).map_err(|e| Error::Other(format!("encode signal: {e}")))?;
    session.send(blob).await
}

/// Fetch and decode the next signal from the peer's mailbox, or `None` at end.
async fn recv_signal(session: &Session) -> Result<Option<Signal>, Error> {
    match session.recv().await? {
        Some(blob) => {
            let signal = serde_json::from_slice(&blob)
                .map_err(|e| Error::InvalidSignaling(format!("decode signal: {e}")))?;
            Ok(Some(signal))
        }
        None => Ok(None),
    }
}

/// Drain the peer connection's local ICE candidates, publishing each and then
/// an explicit end-of-candidates marker.
async fn publish_candidates(peer: &PeerConnection, session: &Session) -> Result<(), Error> {
    for candidate in collect_candidates(peer.local_ice_candidates()).await {
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
async fn consume_signaling(peer: &PeerConnection, session: &Session) -> Result<(), Error> {
    while let Some(signal) = recv_signal(session).await? {
        match signal {
            Signal::Answer { sdp } => {
                peer.set_remote_description(SessionDescription {
                    kind: SdpType::Answer,
                    sdp,
                })
                .await?
            }
            Signal::Offer { .. } => {
                return Err(Error::InvalidSignaling(
                    "unexpected second offer".to_string(),
                ));
            }
            Signal::Candidate {
                candidate,
                sdp_mid,
                sdp_mline_index,
            } => {
                peer.add_ice_candidate(IceCandidate {
                    candidate,
                    sdp_mid,
                    sdp_mline_index,
                })
                .await?
            }
            Signal::EndOfCandidates => {}
        }
    }
    Ok(())
}

// --- helpers ------------------------------------------------------------------

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

/// The byte length of a received message, regardless of kind.
fn message_len(message: &Message) -> usize {
    match message {
        Message::Binary(bytes) => bytes.len(),
        Message::String(text) => text.len(),
    }
}

/// Build a deterministic `size`-byte message tagged with its index; the
/// offerer verifies each echoed payload against it byte-for-byte.
fn make_message(size: usize, index: u32) -> Vec<u8> {
    let mut message = vec![0u8; size];
    let tag = index.to_le_bytes();
    for (slot, byte) in message.iter_mut().zip(tag.iter().cycle()) {
        *slot = *byte;
    }
    message
}

export!(Component);
