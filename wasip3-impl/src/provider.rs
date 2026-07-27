//! The exported `lann:webrtc-datachannels/connections` resources, implemented on
//! top of the in-guest [`crate::runtime`] driver.
//!
//! - [`DataChannelOptions`] is a plain configuration builder.
//! - [`PeerConnection`] owns one [`Runtime`] (a bound UDP socket + sans-I/O
//!   core) and a detached pump task; its async methods drive the SDP
//!   offer/answer + trickle-ICE exchange.
//! - [`DataChannel`] is a handle onto a channel tracked by a peer connection's
//!   shared state; `send`/`receive` observe that state and wake the pump.
//!
//! `peer-connection` binds its socket on the IP address named by the
//! `WEBRTC_UDP_BIND_ADDR` environment variable, defaulting to IPv4 loopback —
//! the address the same-host integration and conformance loopback runs use,
//! where both peers reach each other over `127.0.0.1`. A routable address
//! makes the peer's host candidate reachable across a real (non-loopback)
//! network path, as the Shadow lab exercises.

use std::cell::{Cell, RefCell};
use std::net::{IpAddr, Ipv4Addr};
use std::rc::Rc;
use std::time::{Duration, Instant};

use futures::channel::mpsc;

use crate::peer::SansIoPeer;
use crate::runtime::{InboundMessage, Runtime, Shared, CHANNEL_CLOSE_FLUSH_BOUND};

use crate::exports::lann::webrtc_datachannels::connections::{
    Guest, GuestDataChannel, GuestDataChannelOptions, GuestPeerConnection,
    GuestPeerConnectionConfig,
};
use crate::lann::webrtc_datachannels::types::{
    ConfigError, ConnectionState, DataChannelState, Error, IceCandidate, IceServer,
    IceTransportPolicy, Message, MessageKind, SendViaStreamError, SessionDescription,
    StreamMessage,
};

/// IPv4 loopback: same-host peers reach each other over `127.0.0.1`.
const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// The environment variable naming the IP address `peer-connection` binds its
/// UDP socket to (and derives its host candidate from). Unset or empty means
/// [`LOOPBACK`].
const BIND_ADDR_ENV: &str = "WEBRTC_UDP_BIND_ADDR";

/// The bind address chosen through [`BIND_ADDR_ENV`]. A set-but-unparsable
/// value is an error (the connection is constructed dead, so methods fail
/// `closed`) rather than a silent fallback to loopback.
fn bind_ip() -> anyhow::Result<IpAddr> {
    match std::env::var(BIND_ADDR_ENV) {
        Ok(value) if !value.is_empty() => value
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid {BIND_ADDR_ENV} {value:?}: {e}")),
        _ => Ok(LOOPBACK),
    }
}

/// The maximum time `wait-connected` waits for the connection to establish
/// before failing with `error::timed-out`. Without a bound, a connection that
/// never completes (for example, when the sandbox denies loopback UDP) hangs
/// the caller indefinitely.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Component;

impl Guest for Component {
    type DataChannelOptions = DataChannelOptions;
    type PeerConnectionConfig = PeerConnectionConfig;
    type DataChannel = DataChannel;
    type PeerConnection = PeerConnection;
}
// --- data-channel-options ---------------------------------------------------

/// The mutable configuration a data channel is created with.
#[derive(Clone)]
struct DcConfig {
    label: String,
    ordered: bool,
    max_retransmits: Option<u16>,
}

impl Default for DcConfig {
    fn default() -> Self {
        Self {
            label: String::new(),
            ordered: true,
            max_retransmits: None,
        }
    }
}

/// The exported `data-channel-options` builder.
pub struct DataChannelOptions {
    config: RefCell<DcConfig>,
}

impl DataChannelOptions {
    fn snapshot(&self) -> DcConfig {
        self.config.borrow().clone()
    }
}

impl GuestDataChannelOptions for DataChannelOptions {
    fn new() -> Self {
        Self {
            config: RefCell::new(DcConfig::default()),
        }
    }

    fn label(&self) -> String {
        self.config.borrow().label.clone()
    }
    fn set_label(&self, label: String) {
        self.config.borrow_mut().label = label;
    }

    fn ordered(&self) -> bool {
        self.config.borrow().ordered
    }
    fn set_ordered(&self, ordered: bool) {
        self.config.borrow_mut().ordered = ordered;
    }

    fn max_retransmits(&self) -> Option<u16> {
        self.config.borrow().max_retransmits
    }
    fn set_max_retransmits(&self, max_retransmits: Option<u16>) {
        self.config.borrow_mut().max_retransmits = max_retransmits;
    }
}

// --- peer-connection-config ---------------------------------------------------

/// The exported `peer-connection-config` builder.
///
/// The sans-I/O stack has no STUN/TURN client, so `set-ice-servers` (and the
/// `relay` policy, which requires TURN) fail `not-supported` — per the WIT
/// contract this surfaces at the setter, so a config this provider accepted
/// is always honored by the connection built with it.
pub struct PeerConnectionConfig {
    policy: Cell<IceTransportPolicy>,
}

impl GuestPeerConnectionConfig for PeerConnectionConfig {
    fn new() -> Self {
        Self {
            policy: Cell::new(IceTransportPolicy::All),
        }
    }

    fn ice_servers(&self) -> Vec<IceServer> {
        // `set-ice-servers` never succeeds here, so the stored list is empty.
        Vec::new()
    }

    fn set_ice_servers(&self, _servers: Vec<IceServer>) -> Result<(), ConfigError> {
        Err(ConfigError::NotSupported)
    }

    fn ice_transport_policy(&self) -> IceTransportPolicy {
        self.policy.get()
    }

    fn set_ice_transport_policy(&self, policy: IceTransportPolicy) -> Result<(), ConfigError> {
        match policy {
            IceTransportPolicy::All => {
                self.policy.set(policy);
                Ok(())
            }
            // Relaying requires TURN support.
            IceTransportPolicy::Relay => Err(ConfigError::NotSupported),
        }
    }
}

// --- data-channel -----------------------------------------------------------

/// A handle onto a channel tracked by a peer connection's shared state.
pub struct DataChannel {
    shared: Rc<RefCell<Shared>>,
    nudge: mpsc::UnboundedSender<()>,
    id: rtc::data_channel::RTCDataChannelId,
    label: String,
    /// Take-once claim for `state-changes` (the WIT contract).
    state_taken: Cell<bool>,
}

/// The channel's state as observed through the shared state: usable, still
/// opening, or gone.
enum ChannelState {
    /// The channel opened and has not closed.
    Open,
    /// The channel is not yet tracked but the connection is alive: a locally
    /// created channel whose open the pump has not yet observed.
    Opening,
    /// The channel (or its connection) closed or failed.
    Closed,
}

/// Observe `id`'s state. A channel enters `Shared::channels` only when the pump
/// drains its open event, so a locally created channel is `Opening` — not
/// closed — until then (or until the connection itself dies).
fn channel_state(s: &mut Shared, id: rtc::data_channel::RTCDataChannelId) -> ChannelState {
    let dead = s.closed || s.failed;
    match s.channel_mut(id) {
        Some(channel) if channel.closed => ChannelState::Closed,
        Some(_) => ChannelState::Open,
        None if dead => ChannelState::Closed,
        None => ChannelState::Opening,
    }
}

impl DataChannel {
    /// Close the channel: mark it closed at once (in-flight and later
    /// operations observe `error.closed`; the unread inbound backlog is
    /// discarded per the WIT contract), then hand the transport-level close to
    /// the core and nudge the pump so the closing handshake flushes.
    fn close_impl(&self) {
        {
            let mut s = self.shared.borrow_mut();
            match s.channel_mut(self.id) {
                Some(channel) => {
                    if channel.closed {
                        return;
                    }
                    channel.closed = true;
                    channel.inbox.clear();
                    channel.inbox_bytes = 0;
                    // The rtc-level close is deferred to the pump so pending
                    // sends flush first (see `Channel::local_close_pending`).
                    channel.local_close_pending = true;
                    channel.close_flush_deadline = Some(Instant::now() + CHANNEL_CLOSE_FLUSH_BOUND);
                }
                // Not yet tracked (still opening): record the close so the
                // open event drains into an already-closed channel.
                None => {
                    if s.pending_closes.contains(&self.id) {
                        return;
                    }
                    s.pending_closes.push(self.id);
                }
            }
            s.watch.notify();
        }
        let _ = self.nudge.unbounded_send(());
    }

    /// Wait until the channel is open (or learn it closed), then run `op` on the
    /// shared state.
    async fn when_open<T>(
        &self,
        mut op: impl FnMut(&mut Shared) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let watch = self.shared.borrow().watch.clone();
        loop {
            let seen;
            {
                let mut s = self.shared.borrow_mut();
                seen = s.watch.version();
                match channel_state(&mut s, self.id) {
                    ChannelState::Open => return op(&mut s),
                    ChannelState::Closed => return Err(Error::Closed),
                    ChannelState::Opening => {}
                }
            }
            watch.changed(seen).await;
        }
    }
}

impl GuestDataChannel for DataChannel {
    fn label(&self) -> String {
        self.label.clone()
    }

    async fn send(&self, message: Message) -> Result<(), Error> {
        self.when_open(|s| {
            let result = match &message {
                Message::Binary(data) => s.peer.send_binary(self.id, data),
                Message::String(text) => s.peer.send_text(self.id, text),
            };
            result.map_err(|e| Error::Other(e.to_string()))
        })
        .await?;
        let _ = self.nudge.unbounded_send(());
        Ok(())
    }

    async fn receive(&self) -> Result<Message, Error> {
        let watch = self.shared.borrow().watch.clone();
        loop {
            let seen;
            {
                let mut s = self.shared.borrow_mut();
                seen = s.watch.version();
                let dead = s.closed || s.failed;
                let pending_claim = s.pending_stream_claims.contains(&self.id);
                match s.channel_mut(self.id) {
                    Some(channel) => {
                        // `receive-via-stream` has claimed the inbound
                        // messages; this also resolves receives that were
                        // pending when the claim was made.
                        if channel.stream_claimed {
                            return Err(Error::ReceivingViaStream);
                        }
                        if let Some(msg) = channel.pop() {
                            return Ok(if msg.text {
                                Message::String(String::from_utf8_lossy(&msg.data).into_owned())
                            } else {
                                Message::Binary(msg.data)
                            });
                        }
                        if channel.overflowed {
                            return Err(Error::ReceiveBufferOverflow);
                        }
                        if channel.closed {
                            return Err(Error::Closed);
                        }
                    }
                    // `receive-via-stream` claimed the channel while it was
                    // still opening.
                    None if pending_claim => return Err(Error::ReceivingViaStream),
                    // Not yet tracked: still opening unless the connection died.
                    None if dead => return Err(Error::Closed),
                    None => {}
                }
            }
            watch.changed(seen).await;
        }
    }

    async fn send_via_stream(
        &self,
        messages: wit_bindgen::StreamReader<StreamMessage>,
    ) -> Result<(), SendViaStreamError> {
        let mut messages = messages;
        let mut sent: u64 = 0u64;
        loop {
            let (status, batch) = messages.read(Vec::with_capacity(1)).await;
            for stream_message in batch {
                let bytes = drain_stream(stream_message.data, stream_message.length as usize).await;
                let is_text = matches!(stream_message.kind, MessageKind::String);
                let result = self
                    .when_open(|s| {
                        let result = if is_text {
                            s.peer.send_text(self.id, &String::from_utf8_lossy(&bytes))
                        } else {
                            s.peer.send_binary(self.id, &bytes)
                        };
                        result.map_err(|e| Error::Other(e.to_string()))
                    })
                    .await;
                if let Err(error) = result {
                    return Err(SendViaStreamError { error, sent });
                }
                let _ = self.nudge.unbounded_send(());
                sent += 1;
            }
            if matches!(
                status,
                wit_bindgen::StreamResult::Dropped | wit_bindgen::StreamResult::Cancelled
            ) {
                break;
            }
        }
        Ok(())
    }

    fn close(&self) {
        self.close_impl();
    }

    fn state_changes(&self) -> wit_bindgen::StreamReader<DataChannelState> {
        let (tx, rx) = crate::wit_stream::new();
        // Take-once per the WIT contract.
        if self.state_taken.replace(true) {
            drop(tx);
            return rx;
        }
        wit_bindgen::spawn_local(pump_channel_states(self.shared.clone(), self.id, tx));
        rx
    }

    fn receive_via_stream(&self) -> Result<wit_bindgen::StreamReader<StreamMessage>, Error> {
        {
            let mut s = self.shared.borrow_mut();
            // Once-only: the first call claims the channel's inbound messages.
            // A channel the pump has not yet tracked (state `Opening`) records
            // the claim in `pending_stream_claims`, applied when its open
            // event drains.
            match channel_state(&mut s, self.id) {
                ChannelState::Closed => return Err(Error::Closed),
                ChannelState::Open => {
                    let channel = s.channel_mut(self.id).expect("open channel is tracked");
                    if channel.stream_claimed {
                        return Err(Error::ReceivingViaStream);
                    }
                    channel.stream_claimed = true;
                }
                ChannelState::Opening => {
                    if s.pending_stream_claims.contains(&self.id) {
                        return Err(Error::ReceivingViaStream);
                    }
                    s.pending_stream_claims.push(self.id);
                }
            }
            // Wake pending `receive`s so they resolve with
            // `receiving-via-stream`.
            s.watch.notify();
        }
        let shared = self.shared.clone();
        let id = self.id;
        let (tx, rx) = crate::wit_stream::new();
        wit_bindgen::spawn_local(pump_receive(shared, id, tx));
        Ok(rx)
    }
}

impl Drop for DataChannel {
    fn drop(&mut self) {
        // Dropping the resource without calling `close` implies `close`, per
        // the WIT contract.
        self.close_impl();
    }
}

// --- peer-connection --------------------------------------------------------

/// The exported `peer-connection`: owns a bound socket + sans-I/O core, driven
/// by a detached pump. `None` when construction failed (the bind address was
/// invalid or the socket could not bind): a dead connection, observed by every
/// method as already closed.
pub struct PeerConnection {
    inner: RefCell<Option<PeerState>>,
}

struct PeerState {
    shared: Rc<RefCell<Shared>>,
    nudge: mpsc::UnboundedSender<()>,
    local_candidate: String,
    /// The local host candidate, delivered once through `local-ice-candidates`.
    candidate_taken: bool,
    /// Whether `incoming-data-channels` has been taken: the stream is
    /// take-once, and channels are never re-delivered on a later stream.
    incoming_taken: bool,
    /// Whether `state-changes` has been taken (take-once, per the WIT).
    state_taken: bool,
    /// Whether the pump task has been spawned (see `ensure_pump`).
    started_pump: bool,
    runtime: Option<(Runtime, mpsc::UnboundedReceiver<()>)>,
}

impl PeerConnection {
    /// Ensure the pump task is running (started lazily on first async method).
    fn ensure_pump(state: &mut PeerState) {
        if state.started_pump {
            return;
        }
        if let Some((runtime, wake_rx)) = state.runtime.take() {
            wit_bindgen::spawn_local(runtime.pump(wake_rx));
            state.started_pump = true;
        }
    }

    /// The live state, or `error::closed` for a dead connection.
    fn live(&self) -> Result<std::cell::RefMut<'_, PeerState>, Error> {
        std::cell::RefMut::filter_map(self.inner.borrow_mut(), |state| state.as_mut())
            .map_err(|_| Error::Closed)
    }
}

impl GuestPeerConnection for PeerConnection {
    fn new(
        config: Option<
            crate::exports::lann::webrtc_datachannels::connections::PeerConnectionConfig,
        >,
    ) -> Self {
        // Every option a supplied config carries was accepted by its setter,
        // and this provider only ever accepts the defaults (no ICE servers,
        // the `all` policy — see `PeerConnectionConfig`), so there is nothing
        // to apply beyond taking ownership.
        drop(config);
        // Bind on construction so `create-data-channel` and signaling work; the
        // pump starts lazily on the first async call. If binding fails, the
        // connection is constructed dead (constructors cannot fail here) and
        // every method observes it as already closed.
        let peer = SansIoPeer::new();
        let built = peer.and_then(|peer| Runtime::bind(peer, bind_ip()?));
        if let Err(err) = &built {
            // The construction error itself cannot be returned (constructors
            // are infallible here); make the cause visible before the dead
            // connection surfaces it as `error.closed`.
            eprintln!("peer-connection construction failed: {err:#}");
        }
        PeerConnection {
            inner: RefCell::new(built.ok().map(|(runtime, wake_rx, candidate)| PeerState {
                shared: runtime.shared(),
                nudge: runtime.pump_nudge(),
                local_candidate: candidate,
                candidate_taken: false,
                incoming_taken: false,
                state_taken: false,
                started_pump: false,
                runtime: Some((runtime, wake_rx)),
            })),
        }
    }

    fn create_data_channel(
        &self,
        options: crate::exports::lann::webrtc_datachannels::connections::DataChannelOptions,
    ) -> Result<crate::exports::lann::webrtc_datachannels::connections::DataChannel, Error> {
        let config = options.get::<DataChannelOptions>().snapshot();
        let state = self.live()?;
        let id = {
            let mut s = state.shared.borrow_mut();
            // Per the WIT contract, methods on a closed connection fail
            // `closed` (the gate precedes any input handling).
            if s.closed || s.failed {
                return Err(Error::Closed);
            }
            let id = s
                .peer
                .create_data_channel(&config.label, config.ordered, config.max_retransmits)
                .map_err(|e| Error::Other(e.to_string()))?;
            // Record the id as locally created so `incoming-data-channels`
            // never delivers it.
            s.local_channels.push(id);
            id
        };
        let dc = DataChannel {
            shared: state.shared.clone(),
            nudge: state.nudge.clone(),
            id,
            label: config.label,
            state_taken: Cell::new(false),
        };
        Ok(crate::exports::lann::webrtc_datachannels::connections::DataChannel::new(dc))
    }

    fn incoming_data_channels(
        &self,
    ) -> wit_bindgen::StreamReader<
        crate::exports::lann::webrtc_datachannels::connections::DataChannel,
    > {
        let (tx, rx) = crate::wit_stream::new();
        // Take-once per the WIT contract: a later call (or any call on a dead
        // connection) returns a stream that ends immediately, and channels are
        // never re-delivered.
        let mut guard = self.inner.borrow_mut();
        let Some(state) = guard.as_mut().filter(|state| !state.incoming_taken) else {
            drop(tx);
            return rx;
        };
        state.incoming_taken = true;
        let shared = state.shared.clone();
        let nudge = state.nudge.clone();
        wit_bindgen::spawn_local(pump_incoming(shared, nudge, tx));
        rx
    }

    fn state_changes(&self) -> wit_bindgen::StreamReader<ConnectionState> {
        let (mut tx, rx) = crate::wit_stream::new();
        let mut guard = self.inner.borrow_mut();
        let Some(state) = guard.as_mut() else {
            // A dead connection is terminally over: deliver the terminal state
            // and end.
            wit_bindgen::spawn_local(async move {
                let _ = tx.write_all(vec![ConnectionState::Closed]).await;
                drop(tx);
            });
            return rx;
        };
        // Take-once per the WIT contract.
        if state.state_taken {
            drop(tx);
            return rx;
        }
        state.state_taken = true;
        wit_bindgen::spawn_local(pump_connection_states(state.shared.clone(), tx));
        rx
    }

    async fn create_offer(&self) -> Result<SessionDescription, Error> {
        let mut state = self.live()?;
        PeerConnection::ensure_pump(&mut state);
        let mut s = state.shared.borrow_mut();
        if s.closed || s.failed {
            return Err(Error::Closed);
        }
        let sdp = s
            .peer
            .create_offer()
            .map_err(|e| Error::Other(e.to_string()))?;
        Ok(SessionDescription {
            kind: crate::lann::webrtc_datachannels::types::SdpType::Offer,
            sdp,
        })
    }

    async fn create_answer(&self) -> Result<SessionDescription, Error> {
        let mut state = self.live()?;
        PeerConnection::ensure_pump(&mut state);
        let mut s = state.shared.borrow_mut();
        if s.closed || s.failed {
            return Err(Error::Closed);
        }
        let sdp = s
            .peer
            .create_answer()
            .map_err(|e| Error::Other(e.to_string()))?;
        Ok(SessionDescription {
            kind: crate::lann::webrtc_datachannels::types::SdpType::Answer,
            sdp,
        })
    }

    async fn set_local_description(&self, _description: SessionDescription) -> Result<(), Error> {
        // `create-offer` / `create-answer` already apply the local description
        // (the sans-I/O core produces and sets it in one step), so this is a
        // no-op that exists for API symmetry — but the post-close contract
        // still applies.
        let mut state = self.live()?;
        PeerConnection::ensure_pump(&mut state);
        let s = state.shared.borrow();
        if s.closed || s.failed {
            return Err(Error::Closed);
        }
        Ok(())
    }

    async fn set_remote_description(&self, description: SessionDescription) -> Result<(), Error> {
        let mut state = self.live()?;
        PeerConnection::ensure_pump(&mut state);
        let mut s = state.shared.borrow_mut();
        if s.closed || s.failed {
            return Err(Error::Closed);
        }
        let result = match description.kind {
            crate::lann::webrtc_datachannels::types::SdpType::Offer => {
                s.peer.set_remote_offer(description.sdp)
            }
            crate::lann::webrtc_datachannels::types::SdpType::Answer
            | crate::lann::webrtc_datachannels::types::SdpType::Pranswer => {
                s.peer.set_remote_answer(description.sdp)
            }
            crate::lann::webrtc_datachannels::types::SdpType::Rollback => {
                return Err(Error::InvalidSignaling("rollback is not supported".into()))
            }
        };
        result.map_err(|e| Error::InvalidSignaling(e.to_string()))?;
        drop(s);
        let _ = state.nudge.unbounded_send(());
        Ok(())
    }

    fn local_ice_candidates(&self) -> wit_bindgen::StreamReader<IceCandidate> {
        let (mut tx, rx) = crate::wit_stream::new();
        let mut guard = self.inner.borrow_mut();
        let candidate = match guard.as_mut().filter(|state| !state.candidate_taken) {
            Some(state) => {
                state.candidate_taken = true;
                Some(IceCandidate {
                    candidate: state.local_candidate.clone(),
                    sdp_mid: None,
                    sdp_mline_index: None,
                })
            }
            // Already taken, or a dead connection: the stream ends immediately.
            None => None,
        };
        wit_bindgen::spawn_local(async move {
            if let Some(candidate) = candidate {
                let _ = tx.write_all(vec![candidate]).await;
            }
            drop(tx);
        });
        rx
    }

    async fn add_ice_candidate(&self, candidate: IceCandidate) -> Result<(), Error> {
        let mut state = self.live()?;
        PeerConnection::ensure_pump(&mut state);
        let mut s = state.shared.borrow_mut();
        if s.closed || s.failed {
            return Err(Error::Closed);
        }
        s.peer
            .add_remote_candidate(candidate.candidate)
            .map_err(|e| Error::InvalidSignaling(e.to_string()))?;
        drop(s);
        let _ = state.nudge.unbounded_send(());
        Ok(())
    }

    async fn wait_connected(&self) -> Result<(), Error> {
        let shared = {
            let mut state = self.live()?;
            PeerConnection::ensure_pump(&mut state);
            state.shared.clone()
        };
        let watch = shared.borrow().watch.clone();
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        loop {
            let seen;
            {
                let s = shared.borrow();
                seen = s.watch.version();
                if s.connected {
                    return Ok(());
                }
                if s.failed || s.closed {
                    return Err(Error::Closed);
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::TimedOut);
            }
            // Wake on the next state change, or at the deadline.
            let remaining = deadline.saturating_duration_since(now).as_nanos() as u64;
            let changed = watch.changed(seen);
            let timer = crate::wasi::clocks::monotonic_clock::wait_for(remaining);
            futures::pin_mut!(changed, timer);
            let _ = futures::future::select(changed, timer).await;
        }
    }

    fn close(&self) {
        // Closing a dead connection is a no-op (it is already terminally over).
        if let Some(state) = self.inner.borrow().as_ref() {
            state.shared.borrow_mut().begin_close();
            let _ = state.nudge.unbounded_send(());
        }
    }
}

/// Feed `incoming-data-channels`: emit every channel the peer opens that this
/// side did not create locally, until the connection closes.
async fn pump_incoming(
    shared: Rc<RefCell<Shared>>,
    nudge: mpsc::UnboundedSender<()>,
    mut tx: wit_bindgen::StreamWriter<
        crate::exports::lann::webrtc_datachannels::connections::DataChannel,
    >,
) {
    let watch = shared.borrow().watch.clone();
    let mut cursor = 0usize;
    loop {
        let (next, seen) = {
            let s = shared.borrow_mut();
            let seen = s.watch.version();
            let mut found = None;
            while cursor < s.channels.len() {
                let id = s.channels[cursor].id;
                let label = s.channels[cursor].label.clone();
                cursor += 1;
                if !s.local_channels.contains(&id) {
                    found = Some((id, label));
                    break;
                }
            }
            if found.is_none() && (s.closed || s.failed) {
                drop(s);
                break;
            }
            (found, seen)
        };

        if let Some((id, label)) = next {
            let dc = DataChannel {
                shared: shared.clone(),
                nudge: nudge.clone(),
                id,
                label,
                state_taken: Cell::new(false),
            };
            let handle =
                crate::exports::lann::webrtc_datachannels::connections::DataChannel::new(dc);
            if !tx.write_all(vec![handle]).await.is_empty() {
                break;
            }
        } else {
            watch.changed(seen).await;
        }
    }
    drop(tx);
}

/// The connection's current lifecycle state as observed through the shared
/// state. `closed` wins over `failed` (a local `close` after a failure is
/// still a close); transients narrower than `connected` are not derived, so
/// this watch reports `new` until the connection connects (skipping states is
/// within the `state-changes` contract).
fn connection_state(s: &Shared) -> ConnectionState {
    if s.closed {
        ConnectionState::Closed
    } else if s.failed {
        ConnectionState::Failed
    } else if s.connected {
        ConnectionState::Connected
    } else {
        ConnectionState::New
    }
}

/// Feed `peer-connection.state-changes`: a coalescing watch over the shared
/// state, yielding the current state on demand and ending after a terminal
/// state.
async fn pump_connection_states(
    shared: Rc<RefCell<Shared>>,
    mut tx: wit_bindgen::StreamWriter<ConnectionState>,
) {
    let watch = shared.borrow().watch.clone();
    let mut delivered = None;
    loop {
        let (state, seen) = {
            let s = shared.borrow();
            (connection_state(&s), s.watch.version())
        };
        if delivered != Some(state) {
            if !tx.write_all(vec![state]).await.is_empty() {
                break;
            }
            delivered = Some(state);
        }
        if matches!(state, ConnectionState::Failed | ConnectionState::Closed) {
            break;
        }
        watch.changed(seen).await;
    }
    drop(tx);
}

/// Feed `data-channel.state-changes`: a coalescing watch over the channel's
/// observed state, ending after `closed`.
async fn pump_channel_states(
    shared: Rc<RefCell<Shared>>,
    id: rtc::data_channel::RTCDataChannelId,
    mut tx: wit_bindgen::StreamWriter<DataChannelState>,
) {
    let watch = shared.borrow().watch.clone();
    let mut delivered = None;
    loop {
        let (state, seen) = {
            let mut s = shared.borrow_mut();
            let flushing = s
                .channel_mut(id)
                .is_some_and(|c| c.closed && c.local_close_pending);
            let state = match channel_state(&mut s, id) {
                ChannelState::Opening => DataChannelState::Connecting,
                ChannelState::Open => DataChannelState::Open,
                // A local close still flushing its pending sends reports
                // `closing`; `closed` follows once the pump performs the
                // rtc-level close.
                ChannelState::Closed if flushing => DataChannelState::Closing,
                ChannelState::Closed => DataChannelState::Closed,
            };
            (state, s.watch.version())
        };
        if delivered != Some(state) {
            if !tx.write_all(vec![state]).await.is_empty() {
                break;
            }
            delivered = Some(state);
        }
        if matches!(state, DataChannelState::Closed) {
            break;
        }
        watch.changed(seen).await;
    }
    drop(tx);
}

/// Read exactly `length` bytes (or until end-of-stream) from a
/// `stream-message`'s payload stream.
async fn drain_stream(reader: wit_bindgen::StreamReader<u8>, length: usize) -> Vec<u8> {
    let mut reader = reader;
    let mut out = Vec::with_capacity(length.max(1));
    loop {
        if length != 0 && out.len() >= length {
            break;
        }
        let want = length.saturating_sub(out.len()).max(1);
        let (status, buf) = reader.read(Vec::with_capacity(want)).await;
        out.extend_from_slice(&buf);
        if matches!(
            status,
            wit_bindgen::StreamResult::Dropped | wit_bindgen::StreamResult::Cancelled
        ) {
            break;
        }
    }
    out
}

/// Feed `receive-via-stream`: deliver each inbound message on channel `id` as a
/// `stream-message` whose payload is a fresh byte stream, until the channel
/// closes.
async fn pump_receive(
    shared: Rc<RefCell<Shared>>,
    id: rtc::data_channel::RTCDataChannelId,
    mut tx: wit_bindgen::StreamWriter<StreamMessage>,
) {
    let watch = shared.borrow().watch.clone();
    loop {
        // Pull the next message (or learn the channel is gone/closed) without
        // holding the borrow across the await below.
        enum Next {
            Message(InboundMessage),
            Wait,
            Done,
        }
        let (next, seen) = {
            let mut s = shared.borrow_mut();
            let seen = s.watch.version();
            let dead = s.closed || s.failed;
            let next = match s.channel_mut(id) {
                Some(channel) => match channel.pop() {
                    Some(msg) => Next::Message(msg),
                    None if channel.closed => Next::Done,
                    None => Next::Wait,
                },
                // Not yet tracked: still opening unless the connection died.
                None if dead => Next::Done,
                None => Next::Wait,
            };
            (next, seen)
        };

        let msg = match next {
            Next::Message(msg) => msg,
            Next::Wait => {
                watch.changed(seen).await;
                continue;
            }
            Next::Done => break,
        };

        let (mut data_tx, data_rx) = crate::wit_stream::new();
        let stream_message = StreamMessage {
            kind: if msg.text {
                MessageKind::String
            } else {
                MessageKind::Binary
            },
            length: msg.data.len() as u32,
            data: data_rx,
        };
        if !tx.write_all(vec![stream_message]).await.is_empty() {
            break;
        }
        let _ = data_tx.write_all(msg.data).await;
        drop(data_tx);
    }
    drop(tx);
}
