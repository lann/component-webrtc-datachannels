//! The `webrtc-rs`-backed [`PeerConnection`] host resource.
//!
//! [`PeerConnection`] is the concrete host type mapped onto the
//! `lann:webrtc-datachannels/connections.peer-connection` resource: the
//! guest-driven signaling surface (`create-offer`/`create-answer`,
//! `set-local-description`/`set-remote-description`, trickled ICE candidates)
//! that lets a guest connect two separate peers.
//!
//! ## Deferred wiring
//!
//! The WIT `constructor` and `create-data-channel` are **synchronous**, but a
//! `webrtc-rs` peer connection can only be built on a running Tokio
//! runtime (`webrtc-rs` panics if constructed without one). The constructor
//! therefore spawns a build task and hands back a resource immediately; every
//! async method awaits the shared "built" future before touching the peer
//! connection. `create-data-channel` likewise spawns a task that opens and wires
//! the channel once the peer connection exists, returning a
//! [`DataChannel::deferred`] whose transport is filled in when the channel
//! opens.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::channel::oneshot;
use futures::future::{FutureExt, Shared};
use tokio::runtime::Handle;
use tokio::sync::Notify;
use webrtc::data_channel::RTCDataChannelInit;
use webrtc::peer_connection::{
    PeerConnection as WebrtcPeerConnection, RTCIceCandidateInit, RTCPeerConnectionState,
    RTCSessionDescription,
};

use crate::data_channel::{
    close_peer_connections, close_signal, new_peer_connection_with, spawn_channel_wiring,
    wire_open_channel, wiring_channel, CallbackHandler, CloseSignal, CloseTrigger,
};
use crate::error::{WebrtcError, WebrtcResult};
use crate::{DataChannel, SettingEngineHook};

/// How long [`PeerConnection::wait_connected`] waits before reporting a timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The kind of SDP description passed to `set-local-description` /
/// `set-remote-description`, mirroring the applicable `session-description`
/// variants (`rollback` is rejected before reaching the host).
#[derive(Clone, Copy, Debug)]
pub enum SdpKind {
    /// An SDP offer.
    Offer,
    /// An SDP answer.
    Answer,
    /// A provisional SDP answer.
    Pranswer,
}

/// A locally gathered ICE candidate to trickle to the remote peer.
#[derive(Clone, Debug)]
pub struct LocalCandidate {
    /// The `candidate` attribute value.
    pub candidate: String,
    /// The media stream identification tag, if any.
    pub sdp_mid: Option<String>,
    /// The index of the media description this candidate is associated with.
    pub sdp_mline_index: Option<u16>,
}

/// A future resolving to the built peer connection, or its build error.
/// Shared so every async method observes the same outcome.
type BuiltFuture =
    Shared<Pin<Box<dyn Future<Output = WebrtcResult<Arc<dyn WebrtcPeerConnection>>> + Send>>>;

/// Connection-state signalling shared with the `webrtc-rs` state-change handler.
#[derive(Default)]
struct ConnState {
    /// Set once the connection reaches `connected`.
    connected: AtomicBool,
    /// Set once the connection reaches `failed` or `closed`.
    failed: AtomicBool,
    /// Woken on every state transition so `wait_connected` can re-check.
    notify: Notify,
}

/// Shared inner state behind a `peer-connection` resource.
struct Inner {
    /// Resolves to the built peer connection once the spawned build task runs.
    built: BuiltFuture,
    /// The locally gathered ICE candidates, taken by `local-ice-candidates`.
    candidates: Mutex<Option<UnboundedReceiver<LocalCandidate>>>,
    /// Channels opened by the remote peer, taken by `incoming-data-channels`.
    incoming: Mutex<Option<UnboundedReceiver<DataChannel>>>,
    /// Connection-state signalling for `wait_connected`.
    state: Arc<ConnState>,
    /// The number of `create-data-channel` calls whose spawned registration
    /// task has not yet reached `webrtc-rs`. `create-offer` / `create-answer`
    /// wait for this to reach zero so the produced SDP covers every channel
    /// the guest created before asking for it.
    pending_channels: Arc<PendingOps>,
    /// The built peer connection, retained so `close` (and `Drop`) can tear down
    /// its `webrtc-rs` background tasks. Taken on the first close.
    pc: Arc<Mutex<Option<Arc<dyn WebrtcPeerConnection>>>>,
    /// Fires the connection-close signal every owned data channel observes.
    close_trigger: CloseTrigger,
    /// The signal handed to each data channel this connection creates/adopts.
    close_signal: CloseSignal,
}

/// A counter of in-flight spawned operations, awaitable at zero.
#[derive(Default)]
struct PendingOps {
    count: std::sync::atomic::AtomicUsize,
    notify: Notify,
}

impl PendingOps {
    /// Record one newly spawned operation.
    fn begin(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
    }

    /// Record one finished operation, waking any waiters.
    fn end(&self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Resolve once no operations are in flight.
    async fn settled(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            // Arm the notification before checking, so an `end` between the
            // check and the wait is not missed.
            notified.as_mut().enable();
            if self.count.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let pc = self.pc.lock().unwrap().take();
        close_peer_connections(pc.into_iter().collect());
    }
}

/// Host state behind a `peer-connection` resource.
///
/// Cheaply cloneable (an `Arc` around the shared state) so host methods can hold
/// a handle without borrowing the resource table across `.await`.
#[derive(Clone)]
pub struct PeerConnection {
    inner: Arc<Inner>,
}

impl PeerConnection {
    /// Construct a peer connection, spawning the `webrtc-rs` build task.
    ///
    /// `hook` customizes the [`SettingEngine`](webrtc::peer_connection::SettingEngine)
    /// before the connection is built. Requires a running Tokio runtime; without
    /// one every subsequent operation fails.
    pub fn new(hook: Option<SettingEngineHook>) -> Self {
        Self::new_with(hook, crate::WebrtcIceConfig::default())
    }

    /// Like [`PeerConnection::new`] but with an explicit
    /// [`WebrtcIceConfig`](crate::WebrtcIceConfig) applied when the connection is
    /// built (bind addresses, STUN/TURN servers, relay-only policy).
    pub fn new_with(hook: Option<SettingEngineHook>, ice: crate::WebrtcIceConfig) -> Self {
        let (built_tx, built_rx) =
            oneshot::channel::<WebrtcResult<Arc<dyn WebrtcPeerConnection>>>();
        let (cand_tx, cand_rx) = mpsc::unbounded::<LocalCandidate>();
        let (inc_tx, inc_rx) = mpsc::unbounded::<DataChannel>();
        let state = Arc::new(ConnState::default());
        let pc_slot: Arc<Mutex<Option<Arc<dyn WebrtcPeerConnection>>>> = Arc::new(Mutex::new(None));
        let (close_trigger, close_sig) = close_signal();

        if let Ok(handle) = Handle::try_current() {
            let state = state.clone();
            let pc_slot = pc_slot.clone();
            let trigger = close_trigger.clone();
            let signal = close_sig.clone();
            handle.spawn(async move {
                let handler = connection_handler(cand_tx, inc_tx, state, trigger, signal.clone());
                match new_peer_connection_with(
                    |engine| {
                        if let Some(hook) = &hook {
                            hook(engine);
                        }
                    },
                    ice,
                    handler,
                )
                .await
                {
                    Ok(pc) => {
                        *pc_slot.lock().unwrap() = Some(pc.clone());
                        // `close()` may have raced the build: it found the slot
                        // empty, so tear the connection down now that it exists.
                        if signal.is_closed() {
                            let taken = pc_slot.lock().unwrap().take();
                            close_peer_connections(taken.into_iter().collect());
                        }
                        let _ = built_tx.send(Ok(pc));
                    }
                    Err(err) => {
                        let _ = built_tx.send(Err(WebrtcError::other(err)));
                    }
                }
            });
        } else {
            let _ = built_tx.send(Err(WebrtcError::msg(
                "peer connection requires a running tokio runtime",
            )));
        }

        let built = async move {
            built_rx
                .await
                .unwrap_or_else(|_| Err(WebrtcError::msg("peer connection build was cancelled")))
        }
        .boxed()
        .shared();

        Self {
            inner: Arc::new(Inner {
                built,
                candidates: Mutex::new(Some(cand_rx)),
                incoming: Mutex::new(Some(inc_rx)),
                state,
                pending_channels: Arc::new(PendingOps::default()),
                pc: pc_slot,
                close_trigger,
                close_signal: close_sig,
            }),
        }
    }

    /// Await the built peer connection (or its build error).
    async fn pc(&self) -> WebrtcResult<Arc<dyn WebrtcPeerConnection>> {
        self.inner.built.clone().await
    }

    /// Whether the connection is terminally over: closed by [`close`] or
    /// failed. Per the WIT contract, methods called after that point fail
    /// with `error.closed`.
    pub fn is_closed(&self) -> bool {
        self.inner.close_signal.is_closed()
    }

    /// Gate a method on the connection being open (see [`is_closed`]).
    fn ensure_open(&self) -> WebrtcResult<()> {
        if self.is_closed() {
            return Err(WebrtcError::Closed);
        }
        Ok(())
    }

    /// Create a data channel to negotiate in-band with the peer.
    ///
    /// Returns immediately with a [`DataChannel`] whose transport is wired once
    /// the peer connection is built and the channel opens.
    pub fn create_data_channel(
        &self,
        label: String,
        ordered: bool,
        max_retransmits: Option<u16>,
    ) -> DataChannel {
        let (wire_tx, wired) = wiring_channel();
        let built = self.inner.built.clone();
        let channel_label = label.clone();
        if let Ok(handle) = Handle::try_current() {
            let pending = self.inner.pending_channels.clone();
            pending.begin();
            handle.spawn(async move {
                let pc = match built.await {
                    Ok(pc) => pc,
                    Err(err) => {
                        pending.end();
                        let _ = wire_tx.send(Err(err));
                        return;
                    }
                };
                let init = RTCDataChannelInit {
                    ordered,
                    max_retransmits,
                    ..Default::default()
                };
                let created = pc.create_data_channel(&channel_label, Some(init)).await;
                // The channel is registered with the peer connection (or has
                // failed) as soon as `create_data_channel` returns, so an offer
                // produced from here on covers it.
                pending.end();
                match created {
                    Ok(channel) => spawn_channel_wiring(channel, wire_tx),
                    Err(err) => {
                        let _ = wire_tx.send(Err(WebrtcError::other(err)));
                    }
                }
            });
        } else {
            let _ = wire_tx.send(Err(WebrtcError::msg(
                "peer connection requires a running tokio runtime",
            )));
        }
        DataChannel::deferred(label, wired, self.inner.close_signal.clone())
    }

    /// Take the locally gathered ICE candidate stream. Returns `None` if it has
    /// already been taken (`local-ice-candidates` is meant to be called once).
    pub fn take_local_candidates(&self) -> Option<UnboundedReceiver<LocalCandidate>> {
        self.inner.candidates.lock().unwrap().take()
    }

    /// Take the remote-opened data-channel stream. Returns `None` if it has
    /// already been taken (`incoming-data-channels` is meant to be called once).
    pub fn take_incoming_channels(&self) -> Option<UnboundedReceiver<DataChannel>> {
        self.inner.incoming.lock().unwrap().take()
    }

    /// Produce an SDP offer. The caller applies it via `set-local-description`.
    pub async fn create_offer(&self) -> WebrtcResult<String> {
        self.ensure_open()?;
        let pc = self.pc().await?;
        // Wait for any spawned `create-data-channel` registrations, so the
        // offer's SDP covers every channel created before this call.
        self.inner.pending_channels.settled().await;
        pc.create_offer(None)
            .await
            .map(|desc| desc.sdp)
            .map_err(WebrtcError::other)
    }

    /// Produce an SDP answer to a previously set remote offer.
    pub async fn create_answer(&self) -> WebrtcResult<String> {
        self.ensure_open()?;
        let pc = self.pc().await?;
        // Wait for any spawned `create-data-channel` registrations, so the
        // answer's SDP covers every channel created before this call.
        self.inner.pending_channels.settled().await;
        pc.create_answer(None)
            .await
            .map(|desc| desc.sdp)
            .map_err(WebrtcError::other)
    }

    /// Apply a local description, starting ICE gathering (and, in turn, the
    /// trickled `local-ice-candidates`).
    pub async fn set_local_description(&self, kind: SdpKind, sdp: String) -> WebrtcResult<()> {
        self.ensure_open()?;
        let pc = self.pc().await?;
        let desc = to_rtc_description(kind, sdp)?;
        pc.set_local_description(desc)
            .await
            .map_err(WebrtcError::other)
    }

    /// Apply the remote peer's description.
    pub async fn set_remote_description(&self, kind: SdpKind, sdp: String) -> WebrtcResult<()> {
        self.ensure_open()?;
        let pc = self.pc().await?;
        let desc = to_rtc_description(kind, sdp)?;
        pc.set_remote_description(desc)
            .await
            .map_err(WebrtcError::other)
    }

    /// Add an ICE candidate received from the remote peer.
    pub async fn add_ice_candidate(
        &self,
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    ) -> WebrtcResult<()> {
        self.ensure_open()?;
        let pc = self.pc().await?;
        let init = RTCIceCandidateInit {
            candidate,
            sdp_mid,
            sdp_mline_index,
            username_fragment: None,
            url: None,
        };
        pc.add_ice_candidate(init)
            .await
            .map_err(WebrtcError::invalid_signaling)
    }

    /// Resolve once the connection reaches `connected`, or report a timeout /
    /// failure.
    pub async fn wait_connected(&self) -> WebrtcResult<()> {
        self.pc().await?;
        let state = self.inner.state.clone();
        let deadline = tokio::time::sleep(CONNECT_TIMEOUT);
        tokio::pin!(deadline);
        loop {
            let notified = state.notify.notified();
            tokio::pin!(notified);
            // Arm the notification before checking, so a transition between the
            // check and the wait is not missed.
            notified.as_mut().enable();
            if state.connected.load(Ordering::SeqCst) {
                return Ok(());
            }
            if state.failed.load(Ordering::SeqCst) {
                return Err(WebrtcError::Closed);
            }
            tokio::select! {
                _ = &mut notified => continue,
                _ = &mut deadline => return Err(WebrtcError::TimedOut),
            }
        }
    }

    /// Close the peer connection, tearing down its `webrtc-rs` background tasks.
    /// Idempotent.
    pub fn close(&self) {
        // Fire the close signal first so pending channel operations resolve
        // with `error.closed` (the `webrtc` 0.20 wrapper reports nothing to the
        // channels itself), then tear down the connection.
        self.inner.close_trigger.fire();
        let pc = self.inner.pc.lock().unwrap().take();
        close_peer_connections(pc.into_iter().collect());
    }
}

/// Build the [`PeerConnectionEventHandler`](webrtc::peer_connection::PeerConnectionEventHandler)
/// that feeds the guest-facing streams and connection-state signalling.
///
/// The `webrtc` 0.20 builder takes one handler at build time, so all callbacks
/// are assembled here into a single [`CallbackHandler`]:
///
/// - each locally gathered ICE candidate is trickled onto `cand_tx`, and the
///   stream is ended (the sender dropped) once ICE gathering completes;
/// - each remote-opened data channel is wired and pushed onto `inc_tx`;
/// - connection-state transitions drive `wait_connected` via `state`.
fn connection_handler(
    cand_tx: UnboundedSender<LocalCandidate>,
    inc_tx: UnboundedSender<DataChannel>,
    state: Arc<ConnState>,
    close_trigger: CloseTrigger,
    close_sig: CloseSignal,
) -> Arc<CallbackHandler> {
    let cand_tx = Arc::new(Mutex::new(Some(cand_tx)));
    let gather_cand_tx = cand_tx.clone();
    Arc::new(
        CallbackHandler::new()
            .on_ice_candidate(move |event| {
                if let Ok(init) = event.candidate.to_json() {
                    if let Some(tx) = cand_tx.lock().unwrap().as_ref() {
                        let _ = tx.unbounded_send(LocalCandidate {
                            candidate: init.candidate,
                            sdp_mid: init.sdp_mid,
                            sdp_mline_index: init.sdp_mline_index,
                        });
                    }
                }
            })
            .on_gathering_complete(move || {
                gather_cand_tx.lock().unwrap().take();
            })
            .on_data_channel(move |channel| {
                let inc_tx = inc_tx.clone();
                let signal = close_sig.clone();
                tokio::spawn(async move {
                    let label = channel.label().await.unwrap_or_default();
                    let wired = wire_open_channel(channel);
                    let _ = inc_tx.unbounded_send(DataChannel::deferred(label, wired, signal));
                });
            })
            .on_connection_state(move |s| {
                match s {
                    RTCPeerConnectionState::Connected => {
                        state.connected.store(true, Ordering::SeqCst);
                    }
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                        state.failed.store(true, Ordering::SeqCst);
                        // A failed/closed connection closes its channels too.
                        close_trigger.fire();
                    }
                    _ => {}
                }
                state.notify.notify_waiters();
            }),
    )
}

/// Build a `webrtc-rs` session description from a [`SdpKind`] and SDP string.
/// A description that fails to parse is invalid signaling.
fn to_rtc_description(kind: SdpKind, sdp: String) -> WebrtcResult<RTCSessionDescription> {
    let result = match kind {
        SdpKind::Offer => RTCSessionDescription::offer(sdp),
        SdpKind::Answer => RTCSessionDescription::answer(sdp),
        SdpKind::Pranswer => RTCSessionDescription::pranswer(sdp),
    };
    result.map_err(WebrtcError::invalid_signaling)
}
