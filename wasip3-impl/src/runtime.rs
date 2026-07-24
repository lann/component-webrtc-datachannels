//! The in-guest driver that runs a [`SansIoPeer`] over WASIp3 `wasi:sockets`
//! UDP and `wasi:clocks` timers.
//!
//! A [`Runtime`] owns one peer connection's socket and sans-I/O core. Because
//! the component-model async model is single-threaded and cooperative (no
//! cross-thread `spawn`), the event loop runs as a detached task started with
//! [`wit_bindgen::spawn_local`]: [`Runtime::pump`] repeatedly flushes queued
//! datagrams, drains the core's events into the shared queues that back the
//! exported resources, and parks on the earliest of a timer or an inbound
//! datagram. The exported `data-channel` / `peer-connection` methods observe
//! that shared state and wake the pump through [`futures`] channels.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::{pin, Pin};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use futures::channel::mpsc;
use futures::{select_biased, FutureExt as _, StreamExt as _};

use rtc::data_channel::RTCDataChannelId;

use crate::peer::{PeerEvent, SansIoPeer};

use crate::wasi::clocks::monotonic_clock;
use crate::wasi::sockets::types::{
    ErrorCode, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, Ipv6SocketAddress, UdpSocket,
};

/// The pump's timer-service tick interval. The stack's retransmit and
/// keep-alive deadlines are serviced on the next tick rather than at their
/// exact instant, bounding timer latency at one interval, in exchange for a
/// tick future that is never cancelled mid-flight.
const MAX_WAIT_NANOS: u64 = 50_000_000;

/// How long the pump keeps draining after a local `close` when the core has not
/// yet reported the close complete: long enough for the final queued sends (a
/// last message still in the SCTP queue, the SCTP/DTLS close chunks) and the
/// close handshake to finish on loopback, short enough that a peer that never
/// answers cannot hold the pump open indefinitely.
const CLOSE_DRAIN: Duration = Duration::from_secs(1);

/// A received data-channel message, queued for the owning `data-channel`.
#[derive(Debug, Clone)]
pub struct InboundMessage {
    /// Whether the payload was sent as text (UTF-8) rather than binary.
    pub text: bool,
    /// The message payload.
    pub data: Vec<u8>,
}

/// A single-threaded change-notification primitive over the shared state.
///
/// Waiters capture the current [`version`](Self::version) while holding the
/// state borrow, re-check the state, and — when it is not yet what they need —
/// await [`changed`](StateWatch::changed) with the captured version: the
/// future resolves as soon as the version has advanced past it, so a
/// notification between the check and the await is never lost. The pump
/// notifies after applying core events; `begin_close` and the
/// `receive-via-stream` claim notify directly. This replaces fixed-interval
/// polling, so idle waiters wake only on actual state changes.
#[derive(Default)]
pub struct StateWatch {
    version: Cell<u64>,
    wakers: RefCell<Vec<Waker>>,
}

impl StateWatch {
    /// The current change version. Capture it while holding the state borrow,
    /// before deciding to wait.
    pub fn version(&self) -> u64 {
        self.version.get()
    }

    /// Record a state change and wake every waiter.
    pub fn notify(&self) {
        self.version.set(self.version.get() + 1);
        for waker in self.wakers.borrow_mut().drain(..) {
            waker.wake();
        }
    }

    /// Resolve once the version has advanced past `seen`.
    pub fn changed(self: &Rc<Self>, seen: u64) -> Changed {
        Changed {
            watch: self.clone(),
            seen,
        }
    }
}

/// Future returned by [`StateWatch::changed`].
pub struct Changed {
    watch: Rc<StateWatch>,
    seen: u64,
}

impl Future for Changed {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.watch.version.get() != self.seen {
            return Poll::Ready(());
        }
        let mut wakers = self.watch.wakers.borrow_mut();
        if !wakers.iter().any(|w| w.will_wake(cx.waker())) {
            wakers.push(cx.waker().clone());
        }
        Poll::Pending
    }
}

/// The default bound on inbound payload bytes buffered per channel while
/// waiting for the guest to `receive` them.
///
/// There is no wire-level inbound backpressure (the WIT contract deliberately
/// matches the W3C `RTCDataChannel` floor, where none is possible), so this
/// bound is what protects memory from a slow reader: when it would be exceeded
/// the channel is closed and, once the buffered backlog drains, `receive`
/// fails with `error.receive-buffer-overflow`. Matches the other hosts' bound.
pub const DEFAULT_MAX_INBOUND_BUFFER_BYTES: usize = 8 * 1024 * 1024;

/// The environment variable overriding [`DEFAULT_MAX_INBOUND_BUFFER_BYTES`]
/// (a byte count). Primarily a test knob: the conformance suite shrinks the
/// bound so its overflow probe needs only a small flood.
pub const MAX_INBOUND_BUFFER_ENV: &str = "WEBRTC_MAX_INBOUND_BUFFER_BYTES";

/// The configured inbound buffer bound: [`MAX_INBOUND_BUFFER_ENV`] when set to
/// a positive integer, else [`DEFAULT_MAX_INBOUND_BUFFER_BYTES`].
fn max_inbound_buffer_bytes() -> usize {
    static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var(MAX_INBOUND_BUFFER_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|&bytes| bytes > 0)
            .unwrap_or(DEFAULT_MAX_INBOUND_BUFFER_BYTES)
    })
}

/// A data channel observed by the peer connection: its id plus the label and an
/// inbound-message queue that the exported `data-channel` drains.
pub struct Channel {
    /// The channel's sans-I/O id.
    pub id: RTCDataChannelId,
    /// The negotiated channel label.
    pub label: String,
    /// Messages received on this channel, oldest first.
    pub inbox: VecDeque<InboundMessage>,
    /// Payload bytes currently held in `inbox`, bounded by the configured
    /// [`max_inbound_buffer_bytes`].
    pub inbox_bytes: usize,
    /// True once an inbound message overflowed the buffer bound. The channel is
    /// closed; the `inbox` backlog stays deliverable, after which `receive`
    /// surfaces `error.receive-buffer-overflow` rather than `closed`.
    pub overflowed: bool,
    /// True once `receive-via-stream` has claimed this channel's inbound
    /// messages. While set, `receive` and further `receive-via-stream` calls
    /// fail with `error.receiving-via-stream`.
    pub stream_claimed: bool,
    /// True once the channel (or its connection) has closed.
    pub closed: bool,
}

impl Channel {
    fn new(id: RTCDataChannelId, label: String) -> Self {
        Self {
            id,
            label,
            inbox: VecDeque::new(),
            inbox_bytes: 0,
            overflowed: false,
            stream_claimed: false,
            closed: false,
        }
    }

    /// Pop the oldest buffered message, releasing its bytes from the bound.
    pub fn pop(&mut self) -> Option<InboundMessage> {
        let message = self.inbox.pop_front()?;
        self.inbox_bytes -= message.data.len();
        Some(message)
    }
}

/// The connection-level state shared between the pump and the exported
/// resources.
pub struct Shared {
    /// The sans-I/O core.
    pub peer: SansIoPeer,
    /// Every channel seen so far, in open order.
    pub channels: Vec<Channel>,
    /// The ids of channels created locally via `create-data-channel`, so
    /// `incoming-data-channels` can deliver only remote-created channels.
    pub local_channels: Vec<RTCDataChannelId>,
    /// Channels whose inbound messages `receive-via-stream` claimed before the
    /// pump observed their open (they are not yet in `channels`); the claim is
    /// applied to the tracked channel when its open event drains.
    pub pending_stream_claims: Vec<RTCDataChannelId>,
    /// Whether the connection reached `connected`.
    pub connected: bool,
    /// Whether the connection failed.
    pub failed: bool,
    /// Whether the connection closed.
    pub closed: bool,
    /// Set by [`begin_close`](Self::begin_close) and consumed by the pump: the
    /// rtc-level close is deferred to the pump so it can flush the core's
    /// pending transmits **first**. Closing the core immediately would discard
    /// any queued-but-untransmitted application data (for example a message
    /// sent just before `close()` from a guest task that never yielded to the
    /// pump in between).
    pub close_requested: bool,
    /// Whether the core reported the close complete (its connection state
    /// reached `disconnected`/`closed`), as opposed to a local `close()` that
    /// is still draining.
    pub shutdown_complete: bool,
    /// After a local `close()`, how long the pump keeps draining (flushing the
    /// final sends and completing the close handshake) before giving up on a
    /// peer that never acknowledges.
    pub drain_deadline: Option<Instant>,
    /// Wakes the exported resources' waiters when the state changes.
    pub watch: Rc<StateWatch>,
}

impl Shared {
    /// Find a channel by id.
    pub fn channel_mut(&mut self, id: RTCDataChannelId) -> Option<&mut Channel> {
        self.channels.iter_mut().find(|c| c.id == id)
    }

    /// Mark the connection locally closed and start the pump's bounded drain.
    ///
    /// The rtc-level close itself is **deferred to the pump** (via
    /// [`close_requested`](Self::close_requested)): the pump first flushes the
    /// core's pending transmits — so a message queued immediately before the
    /// close still reaches the wire — then closes the core and keeps draining
    /// until the close completes or the bounded window lapses.
    pub fn begin_close(&mut self) {
        if self.closed || self.failed {
            return;
        }
        self.closed = true;
        self.close_requested = true;
        for c in &mut self.channels {
            c.closed = true;
        }
        self.drain_deadline = Some(Instant::now() + CLOSE_DRAIN);
        self.watch.notify();
    }
}

/// One peer connection's driver: the shared state plus the socket the pump uses.
pub struct Runtime {
    /// The connection state, shared with the exported resources and the pump.
    pub shared: Rc<RefCell<Shared>>,
    socket: Rc<UdpSocket>,
    local: SocketAddr,
    /// Sends a unit each time an exported method changes the core (a queued
    /// send, a new channel, a close), so the pump re-flushes promptly.
    wake_tx: mpsc::UnboundedSender<()>,
}

impl Runtime {
    /// Bind a UDP socket on loopback-capable `local_ip` (an ephemeral port),
    /// pair it with `peer`, and supply the resulting host candidate to the
    /// core. Returns the runtime and a wake receiver for [`pump`](Self::pump).
    pub fn bind(
        mut peer: SansIoPeer,
        local_ip: IpAddr,
    ) -> Result<(Self, mpsc::UnboundedReceiver<()>, String)> {
        let family = match local_ip {
            IpAddr::V4(_) => IpAddressFamily::Ipv4,
            IpAddr::V6(_) => IpAddressFamily::Ipv6,
        };
        let socket = UdpSocket::create(family).map_err(net_err("create UDP socket"))?;
        socket
            .bind(to_wasi_addr(SocketAddr::new(local_ip, 0)))
            .map_err(net_err("bind UDP socket"))?;
        let local = from_wasi_addr(
            socket
                .get_local_address()
                .map_err(net_err("read bound address"))?,
        );

        let candidate = peer.add_local_host_candidate(local)?;

        let (wake_tx, wake_rx) = mpsc::unbounded();
        let shared = Rc::new(RefCell::new(Shared {
            peer,
            channels: Vec::new(),
            local_channels: Vec::new(),
            pending_stream_claims: Vec::new(),
            connected: false,
            failed: false,
            closed: false,
            close_requested: false,
            shutdown_complete: false,
            drain_deadline: None,
            watch: Rc::new(StateWatch::default()),
        }));

        Ok((
            Self {
                shared,
                socket: Rc::new(socket),
                local,
                wake_tx,
            },
            wake_rx,
            candidate,
        ))
    }

    /// A cheap handle the exported resources use to nudge the pump after they
    /// mutate the core.
    pub fn pump_nudge(&self) -> mpsc::UnboundedSender<()> {
        self.wake_tx.clone()
    }

    /// The shared state handle.
    pub fn shared(&self) -> Rc<RefCell<Shared>> {
        self.shared.clone()
    }

    /// Run the event loop until the connection closes or fails. Started once per
    /// peer connection via [`wit_bindgen::spawn_local`].
    pub async fn pump(self, mut wake_rx: mpsc::UnboundedReceiver<()>) {
        let shared = self.shared;
        let socket = self.socket;
        let local = self.local;
        let watch = shared.borrow().watch.clone();

        // The in-flight `receive()` and `wait-for` import futures live across
        // loop iterations: each is a component-model subtask, and dropping one
        // mid-flight cancels it in the host. Cancelling a pending `receive()`
        // can discard a datagram the host has already dequeued into it, so it
        // is replaced only after it completes. The timer is a fixed
        // [`MAX_WAIT_NANOS`] tick that is likewise never cancelled: the stack's
        // deadlines are simply serviced on the next tick, trading at most the
        // safety-net interval of timer latency (which the sans-I/O stack's
        // retransmit/keep-alive granularity already tolerates) for an event
        // loop that never cancels an in-flight import subtask.
        let mut recv = pin!(socket.receive().fuse());
        let mut timer = pin!(monotonic_clock::wait_for(MAX_WAIT_NANOS).fuse());

        loop {
            // Flush + drain while holding the borrow only between awaits. The
            // flush runs *before* a requested close is performed, so pending
            // application data reaches the wire before the core discards its
            // queues.
            flush(&shared, &socket).await;
            let (done, had_events) = {
                let mut s = shared.borrow_mut();
                if s.close_requested {
                    s.close_requested = false;
                    s.peer.close();
                }
                let events = s.peer.drain_events();
                let had_events = !events.is_empty();
                for event in events {
                    apply_event(&mut s, event);
                }
                // Run until the connection fails or closes. A local `close()`
                // keeps the pump draining — flushing final sends and completing
                // the close handshake — until the core reports the close
                // complete or the bounded drain window lapses.
                let done = s.failed
                    || (s.closed
                        && (s.shutdown_complete
                            || s.drain_deadline.is_none_or(|d| Instant::now() >= d)));
                (done, had_events)
            };
            if had_events {
                watch.notify();
            }
            flush(&shared, &socket).await;
            if done {
                // The pump is this state's only writer; after it returns
                // nothing would ever change the state again, so leave it
                // terminally closed. In particular the drain-deadline exit (a
                // local `close()` whose close handshake never completed) must
                // mark every channel closed itself — including one whose open
                // was drained after `begin_close` — or a `send` loop on such a
                // channel would keep succeeding forever.
                let mut s = shared.borrow_mut();
                s.closed = true;
                for c in &mut s.channels {
                    c.closed = true;
                }
                drop(s);
                watch.notify();
                return;
            }

            // Park on the earliest of an inbound datagram, the tick, or a wake.
            select_biased! {
                received = recv.as_mut() => {
                    // A receive error just means no datagram this round.
                    if let Ok((data, from)) = received {
                        shared.borrow_mut().peer.handle_input(
                            &data,
                            from_wasi_addr(from),
                            local,
                            Instant::now(),
                        );
                    }
                    recv.set(socket.receive().fuse());
                }
                _ = timer.as_mut() => {
                    // Service any stack deadline that has come due. `now` is at
                    // least one full tick past the previous service point, so a
                    // deadline that landed between ticks is strictly in the
                    // past and expires; there is no zero-delay spin.
                    timer.set(monotonic_clock::wait_for(MAX_WAIT_NANOS).fuse());
                    shared.borrow_mut().peer.handle_timeout(Instant::now());
                }
                // A wake (an exported method mutated the core): loop to flush.
                _ = wake_rx.next() => {}
            }
        }
    }
}

/// Fold one drained core event into the shared state.
fn apply_event(s: &mut Shared, event: PeerEvent) {
    match event {
        PeerEvent::Connected => s.connected = true,
        PeerEvent::Failed => {
            s.failed = true;
            for c in &mut s.channels {
                c.closed = true;
            }
        }
        PeerEvent::Closed => {
            s.closed = true;
            s.shutdown_complete = true;
            for c in &mut s.channels {
                c.closed = true;
            }
        }
        PeerEvent::ChannelOpen { id, label } => {
            if s.channel_mut(id).is_none() {
                let mut channel = Channel::new(id, label);
                // An open drained after the connection closed or failed (e.g.
                // the core surfacing a pending open during close teardown) is
                // tracked already-closed, so `send`/`receive` on its handle
                // observe `closed` rather than a spuriously usable channel.
                channel.closed = s.closed || s.failed;
                // Apply a `receive-via-stream` claim made while the channel was
                // still opening (see `Shared::pending_stream_claims`).
                channel.stream_claimed = s.pending_stream_claims.contains(&id);
                s.pending_stream_claims.retain(|&claimed| claimed != id);
                s.channels.push(channel);
            }
        }
        PeerEvent::ChannelClosed { id } => {
            if let Some(channel) = s.channel_mut(id) {
                channel.closed = true;
            }
        }
        PeerEvent::Message { id, text, data } => {
            let overflow = match s.channel_mut(id) {
                Some(channel) if channel.overflowed || channel.closed => false,
                Some(channel) if channel.inbox_bytes + data.len() > max_inbound_buffer_bytes() => {
                    // The bounded inbound buffer overflowed: close the channel
                    // and discard this and any later messages. The `inbox`
                    // backlog stays deliverable.
                    channel.overflowed = true;
                    channel.closed = true;
                    true
                }
                Some(channel) => {
                    channel.inbox_bytes += data.len();
                    channel.inbox.push_back(InboundMessage { text, data });
                    false
                }
                None => false,
            };
            if overflow {
                s.peer.close_data_channel(id);
            }
        }
    }
}

/// Send every currently queued outbound datagram, borrowing the core only to
/// pull each one (never across the `send` await).
async fn flush(shared: &Rc<RefCell<Shared>>, socket: &UdpSocket) {
    loop {
        let transmit = shared.borrow_mut().peer.poll_transmit();
        let Some(transmit) = transmit else { break };
        let _ = socket
            .send(transmit.payload, Some(to_wasi_addr(transmit.destination)))
            .await;
    }
}

/// Convert a WASIp3 socket error into an `anyhow` error carrying `what`.
fn net_err(what: &'static str) -> impl Fn(ErrorCode) -> anyhow::Error {
    move |code| anyhow!("{what}: {code:?}")
}

/// Convert a `std` [`SocketAddr`] into a WASIp3 [`IpSocketAddress`].
fn to_wasi_addr(addr: SocketAddr) -> IpSocketAddress {
    match addr {
        SocketAddr::V4(v4) => {
            let [a, b, c, d] = v4.ip().octets();
            IpSocketAddress::Ipv4(Ipv4SocketAddress {
                port: v4.port(),
                address: (a, b, c, d),
            })
        }
        SocketAddr::V6(v6) => {
            let [a, b, c, d, e, f, g, h] = v6.ip().segments();
            IpSocketAddress::Ipv6(Ipv6SocketAddress {
                port: v6.port(),
                flow_info: v6.flowinfo(),
                address: (a, b, c, d, e, f, g, h),
                scope_id: v6.scope_id(),
            })
        }
    }
}

/// Convert a WASIp3 [`IpSocketAddress`] into a `std` [`SocketAddr`].
fn from_wasi_addr(addr: IpSocketAddress) -> SocketAddr {
    match addr {
        IpSocketAddress::Ipv4(v4) => {
            let (a, b, c, d) = v4.address;
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, d)), v4.port)
        }
        IpSocketAddress::Ipv6(v6) => {
            let (a, b, c, d, e, f, g, h) = v6.address;
            SocketAddr::new(IpAddr::V6(Ipv6Addr::new(a, b, c, d, e, f, g, h)), v6.port)
        }
    }
}
