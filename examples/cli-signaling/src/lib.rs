//! `cli-signaling`: a manual-signaling WebRTC demo driven entirely over
//! `wasi:cli@0.3` stdio.
//!
//! The component drives the standard `polymorph:webrtc-datachannels/connections`
//! `peer-connection` with guest-side *vanilla* (non-trickle) ICE: it drains
//! `local-ice-candidates` to its end (the end-of-gathering signal) and embeds
//! the candidates into the SDP, so a whole connection needs just two messages:
//! the offerer prints one complete SDP offer (with all ICE candidates already
//! embedded) and the answerer prints one complete SDP answer. Each blob is
//! base64-encoded onto a single line so a user can copy/paste it into the other
//! peer.
//!
//! It is a `wasm32-wasip2` `cdylib` that exports an *async* `wasi:cli/run` via
//! the `wasip3` crate (a synchronous `run` cannot await the async signaling
//! imports). User I/O goes through the `wasi:cli@0.3` stdio streams
//! (`stdin::read-via-stream` / `stdout::write-via-stream`).

use base64::Engine as _;
use wit_bindgen::StreamResult;

wit_bindgen::generate!({
    path: "wit",
    world: "cli-signaling",
    generate_all,
});

use polymorph::webrtc_datachannels::connections::{DataChannel, DataChannelOptions, PeerConnection};
use polymorph::webrtc_datachannels::types::{Error, IceCandidate, Message, SdpType, SessionDescription};

/// The label used for the negotiated data channel. Both peers observe it.
const CHANNEL_LABEL: &str = "manual-signaling";

struct Component;

impl wasip3::exports::cli::run::Guest for Component {
    async fn run() -> Result<(), ()> {
        // Role selection: `offerer` (default) or `answerer`.
        let role = match wasip3::cli::environment::get_arguments()
            .get(1)
            .map(String::as_str)
        {
            Some("answerer") | Some("answer") => Role::Answerer,
            Some("offerer") | Some("offer") | None => Role::Offerer,
            Some(other) => {
                eprintln!("usage: cli-signaling [offerer|answerer]  (got {other:?})");
                return Err(());
            }
        };

        match drive(role).await {
            Ok(peer_message) => {
                print(&format!(
                    "\nConnected. Message received from peer: {peer_message:?}\n"
                ))
                .await;
                Ok(())
            }
            Err(err) => {
                eprintln!("cli-signaling failed: {err:?}");
                Err(())
            }
        }
    }
}

wasip3::cli::command::export!(Component);

#[derive(Clone, Copy)]
enum Role {
    Offerer,
    Answerer,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Offerer => "offerer",
            Role::Answerer => "answerer",
        }
    }

    /// The peer's role.
    fn opposite(self) -> Role {
        match self {
            Role::Offerer => Role::Answerer,
            Role::Answerer => Role::Offerer,
        }
    }
}

/// Drive the manual-signaling exchange for `role`, returning the message the
/// peer sent over the established data channel.
async fn drive(role: Role) -> Result<String, Error> {
    let pc = PeerConnection::new(None);
    let mut stdin = Stdin::new();

    let channel = match role {
        Role::Offerer => {
            let options = DataChannelOptions::new();
            options.set_label(CHANNEL_LABEL);
            options.set_ordered(true);
            let channel = pc.create_data_channel(options)?;

            let offer = pc.create_offer().await?;
            pc.set_local_description(offer.clone()).await?;
            let offer_sdp = with_candidates(&pc, offer.sdp).await;
            present("offer", &offer_sdp).await;

            let answer = request(&mut stdin, "answer").await;
            pc.set_remote_description(SessionDescription {
                kind: SdpType::Answer,
                sdp: answer,
            })
            .await?;

            print("Applying answer and waiting for the connection to open…\n").await;
            pc.wait_connected().await?;
            channel
        }
        Role::Answerer => {
            let offer = request(&mut stdin, "offer").await;
            pc.set_remote_description(SessionDescription {
                kind: SdpType::Offer,
                sdp: offer,
            })
            .await?;

            let answer = pc.create_answer().await?;
            pc.set_local_description(answer.clone()).await?;
            let answer_sdp = with_candidates(&pc, answer.sdp).await;
            present("answer", &answer_sdp).await;

            print("Waiting for the connection to open…\n").await;
            pc.wait_connected().await?;
            first_incoming(&pc).await?
        }
    };

    exchange(&channel, role).await
}

/// Vanilla (non-trickle) ICE: drain `local-ice-candidates` to its end — the
/// end-of-gathering signal — and embed every candidate into `sdp` as
/// `a=candidate` attributes on its (single) media section, so the returned SDP
/// is the one complete blob the peer needs.
async fn with_candidates(pc: &PeerConnection, sdp: String) -> String {
    let mut candidates = pc.local_ice_candidates();
    let mut full = sdp;
    if !full.ends_with('\n') {
        full.push_str("\r\n");
    }
    loop {
        let (status, batch) = candidates.read(Vec::with_capacity(4)).await;
        for IceCandidate { candidate, .. } in batch {
            if !candidate.is_empty() {
                full.push_str("a=");
                full.push_str(&candidate);
                full.push_str("\r\n");
            }
        }
        if matches!(status, StreamResult::Dropped | StreamResult::Cancelled) {
            break;
        }
    }
    full.push_str("a=end-of-candidates\r\n");
    full
}

/// Adopt the data channel the offerer opened.
async fn first_incoming(pc: &PeerConnection) -> Result<DataChannel, Error> {
    let mut incoming = pc.incoming_data_channels();
    let (_status, batch) = incoming.read(Vec::with_capacity(1)).await;
    batch
        .into_iter()
        .next()
        .ok_or_else(|| Error::Other("no incoming data channel".to_string()))
}

/// Send one greeting and receive the peer's greeting over the data channel,
/// verifying the peer's message is exactly the greeting its role sends.
async fn exchange(channel: &DataChannel, role: Role) -> Result<String, Error> {
    let greeting = format!("hello from the {}", role.name());
    let expected = format!("hello from the {}", role.opposite().name());

    let send_fut = channel.send(Message::Binary(greeting.into_bytes()));
    let recv_fut = channel.receive();

    let (send_result, peer_message) = futures::join!(send_fut, recv_fut);
    send_result?;

    let received = match peer_message {
        Ok(Message::Binary(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
        Ok(Message::String(text)) => text,
        Err(err) => return Err(err),
    };
    if received != expected {
        return Err(Error::Other(format!(
            "peer message mismatch: expected {expected:?}, got {received:?}"
        )));
    }
    Ok(received)
}

/// Present an outgoing signaling blob (base64-encoded SDP) on stdout, alone on
/// its own line so the user (or a test harness) can copy it verbatim.
async fn present(title: &str, sdp: &str) {
    let blob = base64::engine::general_purpose::STANDARD.encode(sdp.as_bytes());
    print(&format!(
        "\nCopy this {title} to the other peer (a single line):\n{blob}\n"
    ))
    .await;
}

/// Prompt for and read an incoming signaling blob (base64-encoded SDP) from
/// stdin, returning the decoded SDP text.
async fn request(stdin: &mut Stdin, title: &str) -> String {
    print(&format!(
        "\nPaste the {title} from the other peer, then press Enter:\n"
    ))
    .await;
    loop {
        let Some(line) = stdin.read_line().await else {
            return String::new();
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match base64::engine::general_purpose::STANDARD.decode(line.as_bytes()) {
            Ok(bytes) => return String::from_utf8_lossy(&bytes).into_owned(),
            Err(_) => {
                print("That did not look like a base64 blob; try again:\n").await;
            }
        }
    }
}

// --- `wasi:cli@0.3` stdio helpers -----------------------------------------

/// A single stdin reader plus any bytes read past the last newline.
struct Stdin {
    reader: wit_bindgen::StreamReader<u8>,
    buffer: Vec<u8>,
    done: bool,
}

impl Stdin {
    fn new() -> Self {
        let (reader, _future) = wasip3::cli::stdin::read_via_stream();
        Self {
            reader,
            buffer: Vec::new(),
            done: false,
        }
    }

    /// Read a single line (without the trailing newline) from `wasi:cli@0.3`
    /// stdin. Returns `None` once stdin is exhausted with no more data.
    async fn read_line(&mut self) -> Option<String> {
        loop {
            if let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=pos).collect();
                return Some(
                    String::from_utf8_lossy(&line[..line.len() - 1])
                        .trim_end_matches('\r')
                        .to_string(),
                );
            }
            if self.done {
                if self.buffer.is_empty() {
                    return None;
                }
                let rest = std::mem::take(&mut self.buffer);
                return Some(
                    String::from_utf8_lossy(&rest)
                        .trim_end_matches('\r')
                        .to_string(),
                );
            }
            let (status, chunk) = self.reader.read(Vec::with_capacity(4096)).await;
            self.buffer.extend_from_slice(&chunk);
            if matches!(status, StreamResult::Dropped | StreamResult::Cancelled) {
                self.done = true;
            }
        }
    }
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
