//! The `webrtc-rs`-backed [`DataChannel`] host resource and helpers.
//!
//! [`DataChannel`] is the concrete host type mapped onto the
//! `lann:webrtc-datachannels/connections.data-channel` resource. It wraps an
//! open `webrtc-rs` data channel and its inbound-message stream.
//!
//! A channel's transport is **deferred**: the `peer-connection` resource's
//! synchronous `create-data-channel` hands back a resource right away, then
//! wires it once the peer connection has been built and the channel opened
//! (remote-opened channels are wired the same way). The async methods await
//! [`DataChannel::wired`] before touching the transport.
//!
//! The `webrtc` 0.20 data channel has no `on_open`/`on_message` callbacks;
//! instead each channel is driven by a per-channel **pump** task that loops on
//! [`webrtc::data_channel::DataChannel::poll`] and turns its
//! [`DataChannelEvent`]s into an open signal plus a stream of
//! [`InboundMessage`]s. Because every message (including a zero-length payload)
//! arrives as an `OnMessage` event rather than being conflated with
//! end-of-stream, empty messages are delivered to the guest.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{self, UnboundedReceiver};
use futures::channel::oneshot;
use futures::future::Shared;
use futures::lock::Mutex as AsyncMutex;
use futures::{FutureExt, StreamExt, TryFutureExt};
use webrtc::data_channel::{DataChannel as WebrtcDataChannel, DataChannelEvent};

use crate::error::{WebrtcError, WebrtcResult};

/// A single inbound data-channel message together with its kind.
///
/// WebRTC distinguishes binary from text (UTF-8) messages; the host preserves
/// that distinction so `receive` can surface the correct `message` variant.
#[derive(Clone, Debug)]
pub(crate) struct InboundMessage {
    /// Whether the message was sent as text (UTF-8) rather than binary.
    pub(crate) is_string: bool,
    /// The raw message payload.
    pub(crate) data: Vec<u8>,
}

/// The default bound on inbound payload bytes buffered per channel while
/// waiting for the guest to `receive` them.
///
/// There is no wire-level inbound backpressure (the WIT contract deliberately
/// matches the W3C `RTCDataChannel` floor, where none is possible), so this
/// bound is what protects host memory from a slow guest reader: when it would
/// be exceeded the channel is closed and, once the buffered backlog drains,
/// `receive` fails with `error.receive-buffer-overflow`. Mirrors the outbound
/// SCTP send-buffer bound the jco hosts use. Overridable through
/// [`MAX_INBOUND_BUFFER_ENV`].
pub const DEFAULT_MAX_INBOUND_BUFFER_BYTES: usize = 8 * 1024 * 1024;

/// The environment variable overriding [`DEFAULT_MAX_INBOUND_BUFFER_BYTES`]
/// (a byte count). Primarily a test knob: the conformance suite shrinks the
/// bound so its overflow probe needs only a small flood.
pub const MAX_INBOUND_BUFFER_ENV: &str = "WEBRTC_MAX_INBOUND_BUFFER_BYTES";

/// The configured inbound buffer bound: [`MAX_INBOUND_BUFFER_ENV`] when set to
/// a positive integer, else [`DEFAULT_MAX_INBOUND_BUFFER_BYTES`].
pub(crate) fn max_inbound_buffer_bytes() -> usize {
    static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var(MAX_INBOUND_BUFFER_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|&bytes| bytes > 0)
            .unwrap_or(DEFAULT_MAX_INBOUND_BUFFER_BYTES)
    })
}

/// The buffered-byte accounting shared between a channel's pump (which
/// reserves capacity for each inbound message) and its readers (which release
/// it as messages are consumed).
#[derive(Debug)]
pub(crate) struct InboundBudget {
    /// The bound on buffered payload bytes.
    limit: usize,
    /// Payload bytes currently buffered and not yet consumed by a reader.
    buffered: AtomicUsize,
    /// Latched once an inbound message would have exceeded the bound.
    overflowed: AtomicBool,
}

impl Default for InboundBudget {
    fn default() -> Self {
        Self {
            limit: max_inbound_buffer_bytes(),
            buffered: AtomicUsize::new(0),
            overflowed: AtomicBool::new(false),
        }
    }
}

impl InboundBudget {
    /// Reserve `len` buffered bytes. Returns `false` — latching the overflow —
    /// if the reservation would exceed the bound or an overflow was already
    /// latched.
    pub(crate) fn reserve(&self, len: usize) -> bool {
        if self.overflowed.load(Ordering::SeqCst) {
            return false;
        }
        if self.buffered.load(Ordering::SeqCst).saturating_add(len) > self.limit {
            self.overflowed.store(true, Ordering::SeqCst);
            return false;
        }
        self.buffered.fetch_add(len, Ordering::SeqCst);
        true
    }

    /// Release `len` buffered bytes after a reader consumed a message.
    pub(crate) fn release(&self, len: usize) {
        self.buffered.fetch_sub(len, Ordering::SeqCst);
    }

    /// Whether an inbound message overflowed the buffer bound.
    pub(crate) fn overflowed(&self) -> bool {
        self.overflowed.load(Ordering::SeqCst)
    }
}

/// A channel's inbound-message queue: the receiving half of the pump's message
/// stream plus the shared [`InboundBudget`] its consumption releases.
pub(crate) struct InboundQueue {
    rx: UnboundedReceiver<InboundMessage>,
    budget: Arc<InboundBudget>,
}

impl InboundQueue {
    /// Build a queue over a raw receiver and its budget.
    pub(crate) fn new(rx: UnboundedReceiver<InboundMessage>, budget: Arc<InboundBudget>) -> Self {
        Self { rx, budget }
    }

    /// The next buffered message, or `None` once the pump has stopped (the
    /// channel closed or its inbound buffer overflowed) and the backlog is
    /// drained. Releases the message's bytes from the budget.
    pub(crate) async fn next(&mut self) -> Option<InboundMessage> {
        let message = self.rx.next().await?;
        self.budget.release(message.data.len());
        Some(message)
    }

    /// Whether the channel's inbound buffer overflowed. When `true`, the queue
    /// ends after the pre-overflow backlog and readers should surface
    /// `error.receive-buffer-overflow` rather than `closed`.
    pub(crate) fn overflowed(&self) -> bool {
        self.budget.overflowed()
    }
}

/// The transport-level parts of a wired channel: the open `webrtc-rs` channel
/// and its shared inbound-message receiver. Cheaply cloneable so it can be the
/// resolved value of the shared wiring future.
#[derive(Clone)]
pub(crate) struct Wired {
    /// The open `webrtc-rs` data channel.
    pub(crate) channel: Arc<dyn WebrtcDataChannel>,
    /// Inbound messages, delivered one per `receive` call. Behind an async mutex
    /// so concurrent receivers serialize and each takes the next message.
    pub(crate) incoming: Arc<AsyncMutex<InboundQueue>>,
}

/// A future resolving to a channel's wired transport parts (or a wiring
/// [`WebrtcError`]), shared so every awaiting async method observes the same
/// outcome.
pub(crate) type WiredFuture = Shared<Pin<Box<dyn Future<Output = WebrtcResult<Wired>> + Send>>>;

/// The receiving half of a connection-close signal, shared by every data
/// channel a `peer-connection` resource owns.
///
/// The `webrtc` 0.20 wrapper neither errors sends nor emits a channel
/// `OnClose` after `PeerConnection::close`, so the host propagates the close
/// itself: the peer connection fires its [`CloseTrigger`] (on a local `close`
/// or on reaching the `failed`/`closed` state) and every channel operation
/// observes it — pending `receive`s resolve with `error.closed` and later
/// `send`s fail with it.
#[derive(Clone)]
pub(crate) struct CloseSignal {
    flag: Arc<AtomicBool>,
    fired: Shared<oneshot::Receiver<()>>,
}

impl CloseSignal {
    /// Whether the owning connection has closed.
    pub(crate) fn is_closed(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// A future resolving once the owning connection closes.
    pub(crate) fn fired(&self) -> Shared<oneshot::Receiver<()>> {
        self.fired.clone()
    }
}

/// The firing half of a connection-close signal; held by the owning
/// `peer-connection`. Cloneable so both the resource's `close` and the
/// connection-state handler can fire it. Idempotent.
#[derive(Clone)]
pub(crate) struct CloseTrigger {
    flag: Arc<AtomicBool>,
    tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl CloseTrigger {
    /// Mark the connection closed and wake every waiter.
    pub(crate) fn fire(&self) {
        self.flag.store(true, Ordering::SeqCst);
        if let Some(tx) = self.tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }
}

/// Create a connected [`CloseTrigger`]/[`CloseSignal`] pair.
pub(crate) fn close_signal() -> (CloseTrigger, CloseSignal) {
    let (tx, rx) = oneshot::channel();
    let flag = Arc::new(AtomicBool::new(false));
    (
        CloseTrigger {
            flag: flag.clone(),
            tx: Arc::new(Mutex::new(Some(tx))),
        },
        CloseSignal {
            flag,
            fired: rx.shared(),
        },
    )
}

/// The open signal and inbound-message stream produced by a channel's pump task.
pub(crate) struct ChannelPump {
    /// Inbound messages drained from the channel, in arrival order, bounded by
    /// the configured [`max_inbound_buffer_bytes`] bound.
    pub(crate) incoming: InboundQueue,
    /// Resolves once the channel reports `open`.
    pub(crate) open: oneshot::Receiver<()>,
}

/// Spawn the per-channel pump task that drives a `webrtc` 0.20 data channel.
///
/// The task loops on [`webrtc::data_channel::DataChannel::poll`] and translates
/// its [`DataChannelEvent`]s: `OnOpen` fires the open signal, each `OnMessage`
/// (including a zero-length payload) is forwarded as an [`InboundMessage`], and
/// `OnClose` (or a `None` poll) ends the pump, dropping the inbound sender so
/// receivers observe end-of-stream.
///
/// Inbound buffering is bounded by [`max_inbound_buffer_bytes`]: a message that
/// would exceed it latches the overflow on the shared [`InboundBudget`], closes
/// the channel, and discards that and any later messages; readers drain the
/// pre-overflow backlog and then surface `error.receive-buffer-overflow`.
pub(crate) fn spawn_channel_pump(channel: Arc<dyn WebrtcDataChannel>) -> ChannelPump {
    let (in_tx, in_rx) = mpsc::unbounded::<InboundMessage>();
    let (open_tx, open_rx) = oneshot::channel::<()>();
    let budget = Arc::new(InboundBudget::default());
    let pump_budget = budget.clone();
    tokio::spawn(async move {
        let mut open_tx = Some(open_tx);
        while let Some(event) = channel.poll().await {
            match event {
                DataChannelEvent::OnOpen => {
                    if let Some(tx) = open_tx.take() {
                        let _ = tx.send(());
                    }
                }
                DataChannelEvent::OnMessage(message) => {
                    if !pump_budget.reserve(message.data.len()) {
                        // The bounded inbound buffer overflowed: close the
                        // channel and discard this and any later messages.
                        let _ = channel.close().await;
                        continue;
                    }
                    let _ = in_tx.unbounded_send(InboundMessage {
                        is_string: message.is_string,
                        data: message.data.to_vec(),
                    });
                }
                DataChannelEvent::OnClose => break,
                _ => {}
            }
        }
    });
    ChannelPump {
        incoming: InboundQueue::new(in_rx, budget),
        open: open_rx,
    }
}

/// Drive an open (or soon-to-open) channel into an existing wiring `oneshot`,
/// fulfilling it with the channel's transport parts once it opens, or
/// [`WebrtcError::Closed`] if it closes first.
pub(crate) fn spawn_channel_wiring(
    channel: Arc<dyn WebrtcDataChannel>,
    wire_tx: oneshot::Sender<WebrtcResult<Wired>>,
) {
    let pump = spawn_channel_pump(channel.clone());
    let incoming = Arc::new(AsyncMutex::new(pump.incoming));
    tokio::spawn(async move {
        match pump.open.await {
            Ok(()) => {
                let _ = wire_tx.send(Ok(Wired { channel, incoming }));
            }
            Err(_) => {
                let _ = wire_tx.send(Err(WebrtcError::Closed));
            }
        }
    });
}

/// Wire an open (or soon-to-open) channel into a [`WiredFuture`] that resolves
/// with the channel's transport parts once it opens, or [`WebrtcError::Closed`]
/// if it closes first. Used by the `peer-connection` resource's deferred and
/// remote-opened channel paths.
pub(crate) fn wire_open_channel(channel: Arc<dyn WebrtcDataChannel>) -> WiredFuture {
    let (wire_tx, wired) = wiring_channel();
    spawn_channel_wiring(channel, wire_tx);
    wired
}

/// Host state behind a `data-channel` resource.
///
/// A connected (or soon-to-be-connected), bidirectional WebRTC data channel plus
/// its inbound-message stream. The `receive-via-stream` claim machinery lives
/// here (not in [`Wired`]) so `receive-via-stream` can be claimed synchronously
/// even before the channel has finished wiring.
pub struct DataChannel {
    /// The negotiated label, known as soon as the resource is created (the
    /// deferred path takes it from the `data-channel-options`).
    label: String,
    /// Resolves to the channel's transport parts once it is wired.
    wired: WiredFuture,
    /// Set once `receive-via-stream` has claimed the inbound messages. While set,
    /// `receive` and `receive-via-stream` both fail with `receiving-via-stream`.
    stream_receiving: Arc<AtomicBool>,
    /// Sender fired when `receive-via-stream` is first called. Held in a mutex so
    /// the first caller takes it (claiming the channel) and all later callers
    /// observe `None`.
    stream_started_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    /// Resolves once `receive-via-stream` is called, so pending `receive` calls
    /// can be woken and fail with `receiving-via-stream`.
    stream_started: Shared<oneshot::Receiver<()>>,
    /// Fired once the owning `peer-connection` resource closes. Send/receive
    /// observe it so operations on a closed connection fail with
    /// `error.closed` even though the `webrtc` 0.20 wrapper reports nothing
    /// itself.
    conn_closed: CloseSignal,
}

impl DataChannel {
    /// Create a channel whose transport is wired later (the synchronous
    /// `peer-connection` `create-data-channel` path). `label` is known up front;
    /// `wired` resolves once the peer connection has built and opened the
    /// channel. The owning `peer-connection` resource supplies `conn_closed`,
    /// its connection-close signal.
    pub(crate) fn deferred(label: String, wired: WiredFuture, conn_closed: CloseSignal) -> Self {
        let (started_tx, started_rx) = oneshot::channel();
        Self {
            label,
            wired,
            stream_receiving: Arc::new(AtomicBool::new(false)),
            stream_started_tx: Arc::new(Mutex::new(Some(started_tx))),
            stream_started: started_rx.shared(),
            conn_closed,
        }
    }

    /// The negotiated channel label.
    pub fn label(&self) -> String {
        self.label.clone()
    }

    /// A clone of the shared wiring future, so an async method can await the
    /// channel's transport parts without holding the store borrow.
    pub(crate) fn wired(&self) -> WiredFuture {
        self.wired.clone()
    }

    /// Claim the channel's inbound messages for `receive-via-stream`.
    ///
    /// Returns `true` for the first caller (which takes ownership of the inbound
    /// stream) and `false` for every later caller. On the first call it also
    /// wakes any pending `receive` calls so they can fail with
    /// `receiving-via-stream` before `receive-via-stream` returns.
    pub(crate) fn begin_stream_receiving(&self) -> bool {
        let mut guard = self.stream_started_tx.lock().unwrap();
        match guard.take() {
            Some(tx) => {
                self.stream_receiving.store(true, Ordering::SeqCst);
                let _ = tx.send(());
                true
            }
            None => false,
        }
    }

    /// Whether `receive-via-stream` has claimed the inbound messages.
    pub(crate) fn is_stream_receiving(&self) -> bool {
        self.stream_receiving.load(Ordering::SeqCst)
    }

    /// A future that resolves once `receive-via-stream` is called, used to wake
    /// pending `receive` calls.
    pub(crate) fn stream_started(&self) -> Shared<oneshot::Receiver<()>> {
        self.stream_started.clone()
    }

    /// The owning connection's close signal.
    pub(crate) fn conn_closed(&self) -> CloseSignal {
        self.conn_closed.clone()
    }
}

/// Build a [`WiredFuture`] from a `oneshot` receiver, returning the sender the
/// wiring task fulfills. If the sender is dropped (the peer connection was torn
/// down before the channel opened), the future resolves to
/// [`WebrtcError::Closed`].
pub(crate) fn wiring_channel() -> (oneshot::Sender<WebrtcResult<Wired>>, WiredFuture) {
    let (tx, rx) = oneshot::channel::<WebrtcResult<Wired>>();
    let fut: Pin<Box<dyn Future<Output = WebrtcResult<Wired>> + Send>> =
        Box::pin(rx.unwrap_or_else(|_| Err(WebrtcError::Closed)));
    (tx, fut.shared())
}
