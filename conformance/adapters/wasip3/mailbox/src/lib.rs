//! The in-guest `wasi:http` mailbox client for the `wasip3-guest` conformance
//! target.
//!
//! It exports `conformance:signaling/mailbox` — the guest-facing view of the
//! HTTP mailbox served by `conformance-signalingd` (see
//! `conformance/signaling/PROTOCOL.md`) — implemented over the WASIp3 async
//! HTTP client (`wasip3::http::client::send`), so the composed conformance
//! guest signals through `wasi:http` entirely in-guest. The host provisions
//! the client with `wasmtime run -S http`.
//!
//! Composed (`wac plug`) under the conformance guest's `mailbox` import,
//! alongside the `wasip3-impl` provider satisfying `connections`.

use std::cell::Cell;

use http_body_util::{BodyExt as _, Empty, Full};

mod bindings {
    wit_bindgen::generate!({
        path: "../../../wit",
        inline: "
            package conformance:wasip3-mailbox;
            world mailbox-client {
                export conformance:signaling/mailbox@0.1.0;
            }
        ",
        generate_all,
    });
}

use bindings::exports::conformance::signaling::mailbox::{
    Guest, GuestSession, Role, Session as SessionResource,
};
use bindings::polymorph::webrtc_datachannels::types::Error;

struct Component;

impl Guest for Component {
    type Session = MailboxSession;
}

bindings::export!(Component with_types_in bindings);

/// A joined mailbox session: one `{room}` and `{role}` on one server. It
/// publishes to its own role's mailbox and consumes the peer's mailbox in
/// publish order, tracking the next sequence number to fetch (reads are
/// idempotent, so a retried fetch observes the same blob).
struct MailboxSession {
    base: String,
    room: String,
    role: Role,
    /// The next sequence number to fetch from the peer's mailbox.
    recv_seq: Cell<u64>,
}

impl MailboxSession {
    /// This session's own role path segment.
    fn own_role(&self) -> &'static str {
        role_str(self.role)
    }

    /// The peer's role path segment (the mailbox this session consumes).
    fn peer_role(&self) -> &'static str {
        match self.role {
            Role::Offerer => "answerer",
            Role::Answerer => "offerer",
        }
    }
}

/// The path segment for a mailbox role.
fn role_str(role: Role) -> &'static str {
    match role {
        Role::Offerer => "offerer",
        Role::Answerer => "answerer",
    }
}

/// Map any client-side mailbox failure to the guest-visible `error.other`.
fn mailbox_error(detail: impl std::fmt::Display) -> Error {
    Error::Other(format!("mailbox: {detail}"))
}

/// The outcome of one mailbox HTTP round trip.
struct HttpOutcome {
    status: http::StatusCode,
    body: Vec<u8>,
}

/// Send one request through the WASIp3 HTTP client and collect the response.
///
/// Every request carries an explicit `content-length` (the body is always
/// fully known here, and usually empty). Without it, the wasip3 compat layer
/// presents even an empty body as an open-ended stream, the host sends the
/// request with `Transfer-Encoding: chunked`, and the chunk terminator is
/// only written when the guest's executor runs the spawned body pump — which
/// races a server that responds without reading the request body: the late
/// terminator arriving after the server's close elicits a TCP RST that can
/// destroy the buffered response (`hyper::Error(IncompleteMessage)` →
/// `error-code.http-protocol-error`). With a declared length the host frames
/// the request by `content-length` and a zero-length body is complete on the
/// wire immediately.
///
/// Each fallible stage is tagged in the error detail (`build` / `convert` /
/// `send` / `response-headers` / `collect`) so a raw `ErrorCode` in a test
/// failure pinpoints where in the round trip it arose — in particular whether
/// the request never got out (`send`) or the response could not be read back
/// (`collect`).
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
            .map_err(|e| mailbox_error(format!("build: {e}")))?,
        None => builder
            .body(Empty::<bytes::Bytes>::new().boxed())
            .map_err(|e| mailbox_error(format!("build: {e}")))?,
    };

    let wasi_request = wasip3::http_compat::http_into_wasi_request(request)
        .map_err(|e| mailbox_error(format!("convert: {e:?}")))?;
    let wasi_response = wasip3::http::client::send(wasi_request)
        .await
        .map_err(|e| mailbox_error(format!("send: {e:?}")))?;
    let response = wasip3::http_compat::http_from_wasi_response(wasi_response)
        .map_err(|e| mailbox_error(format!("response-headers: {e:?}")))?;

    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| mailbox_error(format!("collect: {e:?}")))?
        .to_bytes()
        .to_vec();
    Ok(HttpOutcome { status, body })
}

impl GuestSession for MailboxSession {
    async fn open(server: String, room: String, as_role: Role) -> Result<SessionResource, Error> {
        Ok(SessionResource::new(MailboxSession {
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
            Err(mailbox_error(format!(
                "publish returned {}",
                outcome.status
            )))
        }
    }

    async fn recv(&self) -> Result<Option<Vec<u8>>, Error> {
        let seq = self.recv_seq.get();
        let url = format!(
            "{}/rooms/{}/{}?seq={}&wait=10000",
            self.base,
            self.room,
            self.peer_role(),
            seq
        );
        // Long-poll until blob `seq` arrives or the peer's mailbox is done at or
        // before it. `304 Not Modified` means "not yet; retry the same seq" and
        // is safe to retry indefinitely (the runner bounds the whole test).
        // Transport-level failures are *not* retried: everything runs over
        // loopback, so a failed round trip indicates a real bug and must
        // surface as a failure rather than be masked.
        loop {
            let outcome = round_trip(http::Method::GET, &url, None).await?;
            match outcome.status.as_u16() {
                200 => {
                    self.recv_seq.set(seq + 1);
                    return Ok(Some(outcome.body));
                }
                // The peer marked its mailbox done at or before this seq (the
                // protocol's only 204).
                204 => return Ok(None),
                304 => continue,
                other => return Err(mailbox_error(format!("fetch returned {other}"))),
            }
        }
    }

    async fn done(&self) -> Result<(), Error> {
        let url = format!("{}/rooms/{}/{}/done", self.base, self.room, self.own_role());
        let outcome = round_trip(http::Method::POST, &url, Some(Vec::new())).await?;
        if outcome.status.is_success() {
            Ok(())
        } else {
            Err(mailbox_error(format!("done returned {}", outcome.status)))
        }
    }
}
