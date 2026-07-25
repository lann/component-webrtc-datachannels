//! `webrtc-consumer`: a minimal consumer of the `lann:webrtc-datachannels`
//! `connections` interface, used to exercise (and integration-test) the
//! `wasip3-impl` provider component after composition with `wac plug`.
//!
//! It imports `peer-connection` and, on `wasi:cli/run`, stands up two peers
//! backed by the composed provider, exchanges their SDP + host candidates
//! directly (no external signaling), waits for both to connect, and exchanges
//! one message each way over a data channel — proving the whole in-guest WebRTC
//! stack works end to end through the exported interface.
//!
//! Build it as a `cdylib` for `wasm32-wasip2`; compose it against the provider:
//!
//! ```sh
//! wac plug webrtc_consumer.wasm --plug wasip3_webrtc_datachannels.wasm -o composed.wasm
//! wasmtime run -W component-model-async=y -S cli -S p3 -S inherit-network composed.wasm
//! ```

use anyhow::{anyhow, Result};

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "consumer",
        generate_all,
    });
}

use bindings::lann::webrtc_datachannels::connections::{
    DataChannel, DataChannelOptions, PeerConnection,
};
use bindings::lann::webrtc_datachannels::types::{Error, IceCandidate, Message};

/// The label of the negotiated data channel. Both peers observe it.
const CHANNEL_LABEL: &str = "webrtc-consumer-demo";

struct Component;

impl wasip3::exports::cli::run::Guest for Component {
    async fn run() -> Result<(), ()> {
        match demo().await {
            Ok((offerer_got, answerer_got)) => {
                print(&format!(
                    "\nConnected through the composed provider.\n\
                     Offerer received: {offerer_got:?}\n\
                     Answerer received: {answerer_got:?}\n"
                ))
                .await;
                Ok(())
            }
            Err(err) => {
                print(&format!("webrtc-consumer failed: {err:?}\n")).await;
                Err(())
            }
        }
    }
}

wasip3::cli::command::export!(Component);

/// The number of connection attempts before giving up. The in-guest WebRTC
/// handshake occasionally stalls (an upstream sans-I/O timing flake in `rtc`
/// surfaces as `error::timed-out` from `wait-connected`); each
/// attempt uses fresh peer connections (fresh sockets and ICE state), so a
/// bounded retry keeps the integration test reliable while still asserting a
/// real end-to-end round trip.
const MAX_ATTEMPTS: u32 = 5;

/// Run [`connect_once`] up to [`MAX_ATTEMPTS`] times, retrying only when a
/// connection attempt times out. Any other failure is returned immediately.
async fn demo() -> Result<(String, String)> {
    for attempt in 1..=MAX_ATTEMPTS {
        match connect_once().await {
            Ok(result) => return Ok(result),
            Err(err) if is_timeout(&err) && attempt < MAX_ATTEMPTS => {
                print(&format!(
                    "attempt {attempt} timed out before connecting; retrying…\n"
                ))
                .await;
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!("the loop returns on the final attempt")
}

/// Marker error for a `wait-connected` timeout (the only retryable failure),
/// carried through `anyhow` so retryability is decided by type, not by
/// substring-matching a rendered message.
#[derive(Debug)]
struct ConnectTimeout;

impl std::fmt::Display for ConnectTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("wait-connected timed out")
    }
}

impl std::error::Error for ConnectTimeout {}

/// Whether `err` is a `wait-connected` timeout (the only retryable failure).
fn is_timeout(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ConnectTimeout>().is_some()
}

/// Convert a `wait-connected` result into `anyhow`, preserving the timeout
/// variant as the typed [`ConnectTimeout`] marker.
fn connect_result(side: &str, result: Result<(), Error>) -> Result<()> {
    result.map_err(|e| match e {
        Error::TimedOut => anyhow::Error::new(ConnectTimeout),
        e => anyhow!("{side} wait-connected: {e:?}"),
    })
}

/// Stand up an offerer and an answerer over the imported `connections`
/// interface, exchange their signaling directly, connect, and round-trip one
/// message each way.
async fn connect_once() -> Result<(String, String)> {
    print("Creating the offerer and answerer via the imported provider…\n").await;

    let offerer = PeerConnection::new(None);
    let answerer = PeerConnection::new(None);

    // The offerer creates the channel and produces an offer.
    let options = DataChannelOptions::new();
    options.set_label(CHANNEL_LABEL);
    let offer_dc = offerer
        .create_data_channel(options)
        .map_err(|e| anyhow!("create-data-channel: {e:?}"))?;

    let offer = offerer
        .create_offer()
        .await
        .map_err(|e| anyhow!("create-offer: {e:?}"))?;

    // The answerer applies the offer and produces an answer.
    answerer
        .set_remote_description(offer)
        .await
        .map_err(|e| anyhow!("answerer set-remote offer: {e:?}"))?;
    let answer = answerer
        .create_answer()
        .await
        .map_err(|e| anyhow!("create-answer: {e:?}"))?;
    offerer
        .set_remote_description(answer)
        .await
        .map_err(|e| anyhow!("offerer set-remote answer: {e:?}"))?;

    // Trickle each side's single host candidate to the other.
    let offerer_candidates = collect_candidates(offerer.local_ice_candidates()).await;
    let answerer_candidates = collect_candidates(answerer.local_ice_candidates()).await;
    for candidate in answerer_candidates {
        offerer
            .add_ice_candidate(candidate)
            .await
            .map_err(|e| anyhow!("offerer add-ice-candidate: {e:?}"))?;
    }
    for candidate in offerer_candidates {
        answerer
            .add_ice_candidate(candidate)
            .await
            .map_err(|e| anyhow!("answerer add-ice-candidate: {e:?}"))?;
    }

    print("Connecting…\n").await;

    // Wait for both peers to connect, concurrently.
    let (o, a) = futures::join!(offerer.wait_connected(), answerer.wait_connected());
    connect_result("offerer", o)?;
    connect_result("answerer", a)?;

    // The answerer adopts the channel the offerer created.
    let answer_dc = first_incoming(&answerer).await?;

    // Exchange one message each way.
    let (offerer_got, answerer_got) = futures::join!(
        exchange(&offer_dc, "hello from the offerer"),
        exchange(&answer_dc, "hello from the answerer"),
    );

    offerer.close();
    answerer.close();

    Ok((offerer_got?, answerer_got?))
}

/// Send `greeting` and receive one message on `dc`, returning what was received.
async fn exchange(dc: &DataChannel, greeting: &str) -> Result<String> {
    dc.send(Message::String(greeting.to_string()))
        .await
        .map_err(|e| anyhow!("send: {e:?}"))?;
    match dc.receive().await.map_err(|e| anyhow!("receive: {e:?}"))? {
        Message::String(text) => Ok(text),
        Message::Binary(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
    }
}

/// Adopt the first data channel the remote peer opens.
async fn first_incoming(peer: &PeerConnection) -> Result<DataChannel> {
    let mut stream = peer.incoming_data_channels();
    let (_status, batch) = stream.read(Vec::with_capacity(1)).await;
    batch
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no incoming data channel"))
}

/// Drain a peer's `local-ice-candidates` stream to end.
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

/// Write a string to `wasi:cli@0.3` stdout, awaiting until it is fully flushed.
async fn print(text: &str) {
    let bytes = text.as_bytes().to_vec();
    let (mut tx, rx) = wasip3::wit_stream::new::<u8>();
    let write = wasip3::cli::stdout::write_via_stream(rx);
    let producer = async move {
        let _ = tx.write_all(bytes).await;
        drop(tx);
    };
    let writer = async move {
        let _ = write.await;
    };
    futures::join!(producer, writer);
}
