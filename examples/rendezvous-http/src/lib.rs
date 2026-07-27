//! The in-guest `wasi:http` rendezvous client for the `echo-remote` demo.
//!
//! It exports `demo:webrtc-echo/rendezvous` — the demo's signaling mailbox —
//! implemented over the WASIp3 async HTTP client (`wasip3::http::client::send`)
//! against the HTTP mailbox protocol served by `conformance-signalingd`
//! (`conformance/signaling/PROTOCOL.md`), so a composed echo-remote peer
//! signals through `wasi:http` entirely in-guest; the host provisions the
//! client with `wasmtime run -S http`.
//!
//! Composed (`wac plug`) under the `echo-remote` guest's `rendezvous` import.

use std::cell::Cell;

use http_body_util::{BodyExt as _, Empty, Full};

mod bindings {
    wit_bindgen::generate!({
        path: "../echo-demo/wit",
        inline: "
            package demo:rendezvous-http;
            world rendezvous-http {
                export demo:webrtc-echo/rendezvous@0.1.0;
            }
        ",
        generate_all,
    });
}

use bindings::exports::demo::webrtc_echo::rendezvous::{
    Guest, GuestSession, Role, Session as SessionResource,
};
use bindings::lann::webrtc_datachannels::types::Error;

struct Component;

impl Guest for Component {
    type Session = RendezvousSession;
}

bindings::export!(Component with_types_in bindings);

/// A joined rendezvous session: one `{room}` and `{role}` on one server. It
/// publishes to its own role's mailbox and consumes the peer's mailbox in
/// publish order, tracking the next sequence number to fetch (reads are
/// idempotent, so a retried fetch observes the same blob).
struct RendezvousSession {
    base: String,
    room: String,
    role: Role,
    /// The next sequence number to fetch from the peer's mailbox.
    recv_seq: Cell<u64>,
}

impl RendezvousSession {
    /// This session's own role path segment.
    fn own_role(&self) -> &'static str {
        match self.role {
            Role::Offerer => "offerer",
            Role::Answerer => "answerer",
        }
    }

    /// The peer's role path segment (the mailbox this session consumes).
    fn peer_role(&self) -> &'static str {
        match self.role {
            Role::Offerer => "answerer",
            Role::Answerer => "offerer",
        }
    }
}

/// Map any client-side rendezvous failure to the guest-visible `error.other`.
fn rendezvous_error(detail: impl std::fmt::Display) -> Error {
    Error::Other(format!("rendezvous: {detail}"))
}

/// The outcome of one mailbox HTTP round trip.
struct HttpOutcome {
    status: http::StatusCode,
    done: bool,
    body: Vec<u8>,
}

/// Send one request through the WASIp3 HTTP client and collect the response.
///
/// Every request carries an explicit `content-length` (the body is always
/// fully known here, and usually empty). Without it, the wasip3 compat layer
/// presents even an empty body as an open-ended stream and the host sends the
/// request with `Transfer-Encoding: chunked`, whose late-written terminator
/// races a server that responds without reading the request body (the RST it
/// elicits can destroy the buffered response). With a declared length the
/// host frames the request by `content-length` and a zero-length body is
/// complete on the wire immediately.
async fn round_trip(
    method: http::Method,
    url: &str,
    body: Option<Vec<u8>>,
) -> Result<HttpOutcome, Error> {
    let body_len = body.as_ref().map_or(0, Vec::len);
    let builder = http::Request::builder()
        .method(method)
        .uri(url)
        .header(http::header::CONTENT_LENGTH, body_len);
    let request = match body {
        Some(bytes) => builder
            .body(Full::new(bytes::Bytes::from(bytes)).boxed())
            .map_err(|e| rendezvous_error(format!("build: {e}")))?,
        None => builder
            .body(Empty::<bytes::Bytes>::new().boxed())
            .map_err(|e| rendezvous_error(format!("build: {e}")))?,
    };

    let wasi_request = wasip3::http_compat::http_into_wasi_request(request)
        .map_err(|e| rendezvous_error(format!("convert: {e:?}")))?;
    let wasi_response = wasip3::http::client::send(wasi_request)
        .await
        .map_err(|e| rendezvous_error(format!("send: {e:?}")))?;
    let response = wasip3::http_compat::http_from_wasi_response(wasi_response)
        .map_err(|e| rendezvous_error(format!("response-headers: {e:?}")))?;

    let status = response.status();
    let done = response
        .headers()
        .get("x-done")
        .is_some_and(|v| v.as_bytes() == b"true");
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| rendezvous_error(format!("collect: {e:?}")))?
        .to_bytes()
        .to_vec();
    Ok(HttpOutcome { status, done, body })
}

impl GuestSession for RendezvousSession {
    async fn open(server: String, room: String, as_role: Role) -> Result<SessionResource, Error> {
        Ok(SessionResource::new(RendezvousSession {
            base: server.trim_end_matches('/').to_string(),
            room,
            role: as_role,
            recv_seq: Cell::new(0),
        }))
    }

    async fn send(&self, blob: Vec<u8>) -> Result<(), Error> {
        let url = format!("{}/rooms/{}/{}", self.base, self.room, self.own_role());
        let outcome = round_trip(http::Method::POST, &url, Some(blob)).await?;
        if outcome.status.is_success() {
            Ok(())
        } else {
            Err(rendezvous_error(format!(
                "publish returned {}",
                outcome.status
            )))
        }
    }

    async fn recv(&self) -> Result<Option<Vec<u8>>, Error> {
        let seq = self.recv_seq.get();
        let url = format!(
            "{}/rooms/{}/{}?seq={}",
            self.base,
            self.room,
            self.peer_role(),
            seq
        );
        // Long-poll until blob `seq` arrives or the peer's mailbox is done at
        // or before it. `304 Not Modified` means "not yet; retry the same seq"
        // and is safe to retry indefinitely (the demo's host or harness bounds
        // the whole run).
        loop {
            let outcome = round_trip(http::Method::GET, &url, None).await?;
            match outcome.status.as_u16() {
                200 => {
                    self.recv_seq.set(seq + 1);
                    return Ok(Some(outcome.body));
                }
                204 if outcome.done => return Ok(None),
                304 => continue,
                other => return Err(rendezvous_error(format!("fetch returned {other}"))),
            }
        }
    }

    async fn done(&self) -> Result<(), Error> {
        let url = format!("{}/rooms/{}/{}/done", self.base, self.room, self.own_role());
        let outcome = round_trip(http::Method::POST, &url, Some(Vec::new())).await?;
        if outcome.status.is_success() {
            Ok(())
        } else {
            Err(rendezvous_error(format!(
                "done returned {}",
                outcome.status
            )))
        }
    }
}
