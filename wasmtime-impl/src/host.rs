//! Host trait implementations for the `lann:webrtc-datachannels` imports.
//!
//! Following the split the generated bindings produce (and mirroring
//! `wasmtime_wasi_http::p3`), the store-free traits are implemented for the
//! [`WasiWebrtcCtxView`] "data" type, while the traits whose methods need the
//! async `Accessor` are implemented for the [`WasiWebrtc`] `HasData` marker.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::BytesMut;
use futures::channel::mpsc::{self, UnboundedReceiver};
use futures::channel::oneshot;
use futures::lock::Mutex as AsyncMutex;
use futures::{FutureExt as _, StreamExt};
use wasmtime::component::{
    Access, Accessor, Destination, Resource, Source, StreamConsumer, StreamProducer, StreamReader,
    StreamResult,
};
use wasmtime::{AsContextMut, Result, StoreContextMut};

use crate::bindings::webrtc_datachannels::connections::{
    self, HostDataChannel, HostDataChannelOptions, HostDataChannelWithStore, HostPeerConnection,
    HostPeerConnectionWithStore,
};
use crate::bindings::webrtc_datachannels::types::{
    self, Error, IceCandidate, Message, MessageKind, SdpType, SendViaStreamError,
    SessionDescription, StreamMessage,
};
use crate::data_channel::{CloseSignal, InboundMessage, InboundQueue, WiredFuture};
use crate::error::{WebrtcError, WebrtcResult};
use crate::peer_connection::{LocalCandidate, SdpKind};
use crate::{
    DataChannel, DataChannelOptions, PeerConnection, WasiWebrtc, WasiWebrtcCtxView, WasiWebrtcView,
};

use webrtc::data_channel::DataChannel as WebrtcDataChannel;

// --- types -----------------------------------------------------------------

impl types::Host for WasiWebrtcCtxView<'_> {}

// --- connections -----------------------------------------------------------

impl connections::Host for WasiWebrtcCtxView<'_> {}

impl HostDataChannelOptions for WasiWebrtcCtxView<'_> {
    fn new(&mut self) -> Result<Resource<DataChannelOptions>> {
        Ok(self.table.push(DataChannelOptions::default())?)
    }

    fn label(&mut self, self_: Resource<DataChannelOptions>) -> Result<String> {
        Ok(self.table.get(&self_)?.label.clone())
    }

    fn set_label(&mut self, self_: Resource<DataChannelOptions>, label: String) -> Result<()> {
        self.table.get_mut(&self_)?.label = label;
        Ok(())
    }

    fn ordered(&mut self, self_: Resource<DataChannelOptions>) -> Result<bool> {
        Ok(self.table.get(&self_)?.ordered)
    }

    fn set_ordered(&mut self, self_: Resource<DataChannelOptions>, ordered: bool) -> Result<()> {
        self.table.get_mut(&self_)?.ordered = ordered;
        Ok(())
    }

    fn max_retransmits(&mut self, self_: Resource<DataChannelOptions>) -> Result<Option<u16>> {
        Ok(self.table.get(&self_)?.max_retransmits)
    }

    fn set_max_retransmits(
        &mut self,
        self_: Resource<DataChannelOptions>,
        max_retransmits: Option<u16>,
    ) -> Result<()> {
        self.table.get_mut(&self_)?.max_retransmits = max_retransmits;
        Ok(())
    }
}

impl<T: Send> connections::HostDataChannelOptionsWithStore<T> for WasiWebrtc {
    async fn drop(accessor: &Accessor<T, Self>, rep: Resource<DataChannelOptions>) -> Result<()> {
        accessor.with(|mut access| {
            access.get().table.delete(rep)?;
            Ok(())
        })
    }
}

/// Send one message over a data channel, honoring its `binary`/`string` kind.
///
/// A failed send on a channel that is no longer open is classified as
/// [`WebrtcError::Closed`] (the WIT taxonomy's mid-send-close case); any other
/// failure stays `other`, retaining the `webrtc-rs` source.
async fn send_channel_message(
    channel: &Arc<dyn WebrtcDataChannel>,
    is_string: bool,
    data: Vec<u8>,
) -> WebrtcResult<()> {
    let result = if is_string {
        let text = String::from_utf8(data)
            .map_err(|err| WebrtcError::msg(format!("string message is not valid UTF-8: {err}")))?;
        channel.send_text(&text).await
    } else {
        channel.send(BytesMut::from(&data[..])).await
    };
    match result {
        Ok(_) => Ok(()),
        Err(err) => {
            let open = matches!(
                channel.ready_state().await,
                Ok(webrtc::data_channel::RTCDataChannelState::Open)
            );
            if open {
                Err(WebrtcError::other(err))
            } else {
                Err(WebrtcError::Closed)
            }
        }
    }
}

/// Await the next inbound message from a channel's shared queue, reporting
/// `Error::Closed` once the channel has closed and no more messages will
/// arrive — or `Error::ReceiveBufferOverflow` when the queue ended because the
/// channel's bounded inbound buffer overflowed.
async fn next_inbound(incoming: Arc<AsyncMutex<InboundQueue>>) -> WebrtcResult<InboundMessage> {
    let mut queue = incoming.lock().await;
    match queue.next().await {
        Some(message) => Ok(message),
        None if queue.overflowed() => Err(WebrtcError::ReceiveBufferOverflow),
        None => Err(WebrtcError::Closed),
    }
}

/// Convert a host-side inbound message into the WIT `message` variant.
fn to_message(inbound: InboundMessage) -> WebrtcResult<Message> {
    if inbound.is_string {
        String::from_utf8(inbound.data)
            .map(Message::String)
            .map_err(|err| WebrtcError::msg(format!("string message is not valid UTF-8: {err}")))
    } else {
        Ok(Message::Binary(inbound.data))
    }
}

/// A [`StreamConsumer`] that drains every byte of a `stream<u8>` into a buffer,
/// handing the completed buffer back through `done_tx` when the stream ends.
struct ByteCollector {
    buf: Vec<u8>,
    done_tx: Option<oneshot::Sender<Vec<u8>>>,
}

impl ByteCollector {
    fn finish(&mut self) {
        if let Some(tx) = self.done_tx.take() {
            let _ = tx.send(std::mem::take(&mut self.buf));
        }
    }
}

impl<D: Send + 'static> StreamConsumer<D> for ByteCollector {
    type Item = u8;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: StoreContextMut<D>,
        mut source: Source<'_, u8>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        let this = self.get_mut(); // safe: ByteCollector is Unpin

        let available = source.remaining(&mut store);
        if available > 0 {
            let mut chunk = Vec::with_capacity(available);
            source.read(&mut store, &mut chunk)?;
            this.buf.extend_from_slice(&chunk);
            return Poll::Ready(Ok(StreamResult::Completed));
        }

        // No bytes available. When `finish` is set the stream is ending, so hand
        // the collected buffer back; `Drop` covers a normal end-of-stream.
        if finish {
            this.finish();
            Poll::Ready(Ok(StreamResult::Cancelled))
        } else {
            Poll::Pending
        }
    }
}

impl Drop for ByteCollector {
    fn drop(&mut self) {
        self.finish();
    }
}

/// A [`StreamProducer`] that yields one `stream-message` per inbound WebRTC
/// message, wrapping each message's bytes in a fresh `stream<u8>`.
struct InboundStreamMessages {
    /// Resolves to the channel's transport parts (including its inbound-message
    /// receiver) once the channel is wired.
    wired: WiredFuture,
    /// Ends the stream once the owning connection closes.
    conn_closed: CloseSignal,
    /// A future resolving to the next inbound message (or `None` once the
    /// channel is closed or wiring failed), retained across polls so the shared
    /// receiver lock is only held while awaiting the next message.
    pending: Option<Pin<Box<dyn Future<Output = Option<InboundMessage>> + Send>>>,
}

impl<D: Send + 'static> StreamProducer<D> for InboundStreamMessages {
    type Item = StreamMessage;
    type Buffer = Option<StreamMessage>;

    fn poll_produce<'a>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<'a, D>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        let this = self.get_mut(); // safe: InboundStreamMessages is Unpin

        let wired = this.wired.clone();
        let conn_closed = this.conn_closed.clone();
        let fut = this.pending.get_or_insert_with(|| {
            Box::pin(async move {
                // Wait for the channel to be wired; if wiring failed the channel
                // never opened, so treat it as end-of-stream. The owning
                // connection closing likewise ends the stream (biased so an
                // already-available message wins).
                let mut next = std::pin::pin!(async move {
                    let wired = wired.await.ok()?;
                    let mut queue = wired.incoming.lock().await;
                    queue.next().await
                }
                .fuse());
                let mut closed = std::pin::pin!(conn_closed.fired().fuse());
                futures::select_biased! {
                    message = next => message,
                    _ = closed => None,
                }
            })
        });

        match fut.as_mut().poll(cx) {
            Poll::Pending => {
                if finish {
                    Poll::Ready(Ok(StreamResult::Cancelled))
                } else {
                    Poll::Pending
                }
            }
            Poll::Ready(None) => {
                this.pending = None;
                Poll::Ready(Ok(StreamResult::Dropped))
            }
            Poll::Ready(Some(inbound)) => {
                this.pending = None;
                let kind = if inbound.is_string {
                    MessageKind::String
                } else {
                    MessageKind::Binary
                };
                let length = inbound.data.len() as u32;
                let data = StreamReader::new(store, inbound.data)?;
                destination.set_buffer(Some(StreamMessage { kind, length, data }));
                Poll::Ready(Ok(StreamResult::Completed))
            }
        }
    }
}

/// One outbound message parsed from a `stream<stream-message>` element: its kind,
/// declared length, and a receiver for its fully-drained payload bytes.
struct PendingSend {
    is_string: bool,
    length: usize,
    done_rx: oneshot::Receiver<Vec<u8>>,
}

/// A [`StreamConsumer`] that reads each `stream-message` from a
/// `stream<stream-message>`, starts draining its `data` payload, and forwards
/// the resulting [`PendingSend`] to the `send-via-stream` driver.
struct OutboundStreamMessages {
    tx: mpsc::UnboundedSender<PendingSend>,
}

impl<D: Send + 'static> StreamConsumer<D> for OutboundStreamMessages {
    type Item = StreamMessage;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: StoreContextMut<D>,
        mut source: Source<'_, StreamMessage>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        let this = self.get_mut(); // safe: OutboundStreamMessages is Unpin

        let available = source.remaining(&mut store);
        if available == 0 {
            // No items are ready. When `finish` is set the writer is done, so
            // report cancellation; otherwise wait to be re-polled once the
            // writer provides more items (returning `Completed` here without
            // taking an item would trap).
            return if finish {
                Poll::Ready(Ok(StreamResult::Cancelled))
            } else {
                Poll::Pending
            };
        }

        // Drain every message the writer has made available, so a writer that
        // queues several messages before closing does not have the trailing
        // ones silently discarded when it finishes. Each message's payload is
        // drained concurrently by a `ByteCollector`; the `send-via-stream`
        // driver then sends the fully-buffered messages one at a time, in order.
        let mut messages: Vec<StreamMessage> = Vec::with_capacity(available);
        source.read(&mut store, &mut messages)?;
        for message in messages {
            let is_string = matches!(message.kind, MessageKind::String);
            let length = message.length as usize;
            let (done_tx, done_rx) = oneshot::channel();
            message.data.pipe(
                store.as_context_mut(),
                ByteCollector {
                    buf: Vec::with_capacity(length),
                    done_tx: Some(done_tx),
                },
            )?;
            let _ = this.tx.unbounded_send(PendingSend {
                is_string,
                length,
                done_rx,
            });
        }
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

impl HostDataChannel for WasiWebrtcCtxView<'_> {
    fn label(&mut self, self_: Resource<DataChannel>) -> Result<String> {
        Ok(self.table.get(&self_)?.label())
    }
}

impl<T: Send> HostDataChannelWithStore<T> for WasiWebrtc {
    async fn send(
        accessor: &Accessor<T, Self>,
        self_: Resource<DataChannel>,
        message: Message,
    ) -> Result<std::result::Result<(), Error>> {
        let (wired, conn_closed) = accessor.with(|mut access| {
            let channel = access.get().table.get(&self_)?;
            Ok::<_, wasmtime::Error>((channel.wired(), channel.conn_closed()))
        })?;

        // The owning peer connection closed: the `webrtc` 0.20 wrapper would
        // silently queue the message, so surface `closed` here.
        if conn_closed.is_closed() {
            return Ok(Err(Error::Closed));
        }

        let (is_string, data) = match message {
            Message::Binary(data) => (false, data),
            Message::String(text) => (true, text.into_bytes()),
        };

        let wired = match wired.await {
            Ok(wired) => wired,
            Err(err) => return Ok(Err(err.into())),
        };
        Ok(send_channel_message(&wired.channel, is_string, data)
            .await
            .map_err(Error::from))
    }

    async fn receive(
        accessor: &Accessor<T, Self>,
        self_: Resource<DataChannel>,
    ) -> Result<std::result::Result<Message, Error>> {
        let (wired, stream_started, stream_receiving, conn_closed) =
            accessor.with(|mut access| {
                let channel = access.get().table.get(&self_)?;
                Ok::<_, wasmtime::Error>((
                    channel.wired(),
                    channel.stream_started(),
                    channel.is_stream_receiving(),
                    channel.conn_closed(),
                ))
            })?;

        // `receive-via-stream` has already taken over the inbound messages.
        if stream_receiving {
            return Ok(Err(Error::ReceivingViaStream));
        }

        // Race receiving the next inbound message (once the channel is wired)
        // against `receive-via-stream` being called and against the owning
        // connection closing: a pending receiver is woken and fails with
        // `receiving-via-stream` the moment the stream is claimed, or with
        // `closed` when the connection closes (the `webrtc` 0.20 wrapper emits
        // no channel close of its own). Biased order: an already-available
        // message wins over both signals.
        let mut receive = std::pin::pin!(async move {
            let wired = wired.await?;
            next_inbound(wired.incoming).await
        }
        .fuse());
        let mut started = std::pin::pin!(stream_started.fuse());
        let mut closed = std::pin::pin!(conn_closed.fired().fuse());
        Ok(futures::select_biased! {
            result = receive => match result {
                Ok(inbound) => to_message(inbound).map_err(Error::from),
                Err(err) => Err(err.into()),
            },
            // The stream-started signal fired; when it was actually sent
            // (rather than cancelled by the channel being dropped) report the
            // takeover.
            signal = started => {
                if signal.is_ok() {
                    Err(Error::ReceivingViaStream)
                } else {
                    Err(Error::Closed)
                }
            }
            _ = closed => Err(Error::Closed),
        })
    }

    async fn send_via_stream(
        accessor: &Accessor<T, Self>,
        self_: Resource<DataChannel>,
        messages: StreamReader<StreamMessage>,
    ) -> Result<std::result::Result<(), SendViaStreamError>> {
        let wired = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.wired()))?;
        let channel = match wired.await {
            Ok(wired) => wired.channel,
            Err(err) => {
                return Ok(Err(SendViaStreamError {
                    error: err.into(),
                    sent: 0,
                }))
            }
        };

        // Drain each element's payload concurrently (via `OutboundStreamMessages`)
        // while this driver sends the fully-buffered messages one at a time, in
        // stream order.
        let (tx, mut rx) = mpsc::unbounded::<PendingSend>();
        accessor.with(|access| messages.pipe(access, OutboundStreamMessages { tx }))?;

        let conn_closed = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.conn_closed())
        })?;
        let mut sent: u64 = 0;
        while let Some(pending) = rx.next().await {
            if conn_closed.is_closed() {
                return Ok(Err(SendViaStreamError {
                    error: Error::Closed,
                    sent,
                }));
            }
            let data = pending.done_rx.await.unwrap_or_default();
            if data.len() != pending.length {
                return Ok(Err(SendViaStreamError {
                    error: WebrtcError::msg(format!(
                        "stream-message payload was {} bytes but length declared {}",
                        data.len(),
                        pending.length
                    ))
                    .into(),
                    sent,
                }));
            }
            if let Err(error) = send_channel_message(&channel, pending.is_string, data).await {
                return Ok(Err(SendViaStreamError {
                    error: error.into(),
                    sent,
                }));
            }
            sent += 1;
        }
        Ok(Ok(()))
    }

    fn receive_via_stream(
        mut access: wasmtime::component::Access<'_, T, Self>,
        self_: Resource<DataChannel>,
    ) -> Result<std::result::Result<StreamReader<StreamMessage>, Error>> {
        // Claim the inbound messages for this stream. A second call (or any
        // concurrent `receive`) observes the claim and fails.
        let (claimed, wired, conn_closed) = {
            let channel = access.get().table.get(&self_)?;
            (
                channel.begin_stream_receiving(),
                channel.wired(),
                channel.conn_closed(),
            )
        };
        if !claimed {
            return Ok(Err(Error::ReceivingViaStream));
        }
        let reader = StreamReader::new(
            access,
            InboundStreamMessages {
                wired,
                conn_closed,
                pending: None,
            },
        )?;
        Ok(Ok(reader))
    }

    async fn drop(accessor: &Accessor<T, Self>, rep: Resource<DataChannel>) -> Result<()> {
        accessor.with(|mut access| {
            access.get().table.delete(rep)?;
            Ok(())
        })
    }
}

// --- peer-connection -------------------------------------------------------

/// Map a WIT `session-description` `kind` onto the host [`SdpKind`], rejecting
/// `rollback` (which `webrtc-rs`'s `set-*-description` cannot express here).
fn to_sdp_kind(kind: SdpType) -> WebrtcResult<SdpKind> {
    match kind {
        SdpType::Offer => Ok(SdpKind::Offer),
        SdpType::Answer => Ok(SdpKind::Answer),
        SdpType::Pranswer => Ok(SdpKind::Pranswer),
        SdpType::Rollback => Err(WebrtcError::invalid_signaling(anyhow::anyhow!(
            "rollback descriptions are not supported"
        ))),
    }
}

impl HostPeerConnection for WasiWebrtcCtxView<'_> {
    fn new(&mut self) -> Result<Resource<PeerConnection>> {
        let hook = self.ctx.setting_engine_hook();
        let ice = self.ctx.ice_config();
        Ok(self.table.push(PeerConnection::new_with(hook, ice))?)
    }

    fn create_data_channel(
        &mut self,
        self_: Resource<PeerConnection>,
        options: Resource<DataChannelOptions>,
    ) -> Result<std::result::Result<Resource<DataChannel>, Error>> {
        // `create-data-channel` takes ownership of the options resource; read
        // its configuration, then drop it from the table.
        let options = self.table.delete(options)?;
        let pc = self.table.get(&self_)?;
        // Per the WIT contract, methods on a closed connection fail `closed`.
        if pc.is_closed() {
            return Ok(Err(crate::WebrtcError::Closed.into()));
        }
        let channel =
            pc.create_data_channel(options.label, options.ordered, options.max_retransmits);
        Ok(Ok(self.table.push(channel)?))
    }

    fn close(&mut self, self_: Resource<PeerConnection>) -> Result<()> {
        self.table.get(&self_)?.close();
        Ok(())
    }
}

/// A [`StreamProducer`] yielding one `ice-candidate` per locally gathered ICE
/// candidate; the stream ends once gathering completes.
struct LocalCandidateStream {
    /// The candidate receiver, or `None` if `local-ice-candidates` was already
    /// claimed (in which case the stream is empty).
    rx: Option<UnboundedReceiver<LocalCandidate>>,
}

impl<D: Send + 'static> StreamProducer<D> for LocalCandidateStream {
    type Item = IceCandidate;
    type Buffer = Option<IceCandidate>;

    fn poll_produce<'a>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _store: StoreContextMut<'a, D>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        let this = self.get_mut(); // safe: LocalCandidateStream is Unpin
        let Some(rx) = this.rx.as_mut() else {
            return Poll::Ready(Ok(StreamResult::Dropped));
        };
        match rx.poll_next_unpin(cx) {
            Poll::Pending => {
                if finish {
                    Poll::Ready(Ok(StreamResult::Cancelled))
                } else {
                    Poll::Pending
                }
            }
            Poll::Ready(None) => {
                this.rx = None;
                Poll::Ready(Ok(StreamResult::Dropped))
            }
            Poll::Ready(Some(candidate)) => {
                destination.set_buffer(Some(IceCandidate {
                    candidate: candidate.candidate,
                    sdp_mid: candidate.sdp_mid,
                    sdp_mline_index: candidate.sdp_mline_index,
                }));
                Poll::Ready(Ok(StreamResult::Completed))
            }
        }
    }
}

/// A [`StreamProducer`] yielding one `data-channel` resource per channel opened
/// by the remote peer. Producing a resource needs table access, so it is bound
/// on [`WasiWebrtcView`].
struct IncomingChannelStream {
    /// The incoming-channel receiver, or `None` if `incoming-data-channels` was
    /// already claimed (in which case the stream is empty).
    rx: Option<UnboundedReceiver<DataChannel>>,
}

impl<D: WasiWebrtcView + 'static> StreamProducer<D> for IncomingChannelStream {
    type Item = Resource<DataChannel>;
    type Buffer = Option<Resource<DataChannel>>;

    fn poll_produce<'a>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, D>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        let this = self.get_mut(); // safe: IncomingChannelStream is Unpin
        let Some(rx) = this.rx.as_mut() else {
            return Poll::Ready(Ok(StreamResult::Dropped));
        };
        match rx.poll_next_unpin(cx) {
            Poll::Pending => {
                if finish {
                    Poll::Ready(Ok(StreamResult::Cancelled))
                } else {
                    Poll::Pending
                }
            }
            Poll::Ready(None) => {
                this.rx = None;
                Poll::Ready(Ok(StreamResult::Dropped))
            }
            Poll::Ready(Some(channel)) => {
                let resource = store.data_mut().webrtc().table.push(channel)?;
                destination.set_buffer(Some(resource));
                Poll::Ready(Ok(StreamResult::Completed))
            }
        }
    }
}

impl<T: WasiWebrtcView + 'static> HostPeerConnectionWithStore<T> for WasiWebrtc {
    fn incoming_data_channels(
        mut access: Access<'_, T, Self>,
        self_: Resource<PeerConnection>,
    ) -> Result<StreamReader<Resource<DataChannel>>> {
        let rx = access.get().table.get(&self_)?.take_incoming_channels();
        StreamReader::new(access, IncomingChannelStream { rx })
    }

    fn local_ice_candidates(
        mut access: Access<'_, T, Self>,
        self_: Resource<PeerConnection>,
    ) -> Result<StreamReader<IceCandidate>> {
        let rx = access.get().table.get(&self_)?.take_local_candidates();
        StreamReader::new(access, LocalCandidateStream { rx })
    }

    async fn create_offer(
        accessor: &Accessor<T, Self>,
        self_: Resource<PeerConnection>,
    ) -> Result<std::result::Result<SessionDescription, Error>> {
        let peer = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.clone()))?;
        Ok(match peer.create_offer().await {
            Ok(sdp) => Ok(SessionDescription {
                kind: SdpType::Offer,
                sdp,
            }),
            Err(err) => Err(err.into()),
        })
    }

    async fn create_answer(
        accessor: &Accessor<T, Self>,
        self_: Resource<PeerConnection>,
    ) -> Result<std::result::Result<SessionDescription, Error>> {
        let peer = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.clone()))?;
        Ok(match peer.create_answer().await {
            Ok(sdp) => Ok(SessionDescription {
                kind: SdpType::Answer,
                sdp,
            }),
            Err(err) => Err(err.into()),
        })
    }

    async fn set_local_description(
        accessor: &Accessor<T, Self>,
        self_: Resource<PeerConnection>,
        description: SessionDescription,
    ) -> Result<std::result::Result<(), Error>> {
        let kind = match to_sdp_kind(description.kind) {
            Ok(kind) => kind,
            Err(err) => return Ok(Err(err.into())),
        };
        let peer = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.clone()))?;
        Ok(peer
            .set_local_description(kind, description.sdp)
            .await
            .map_err(Error::from))
    }

    async fn set_remote_description(
        accessor: &Accessor<T, Self>,
        self_: Resource<PeerConnection>,
        description: SessionDescription,
    ) -> Result<std::result::Result<(), Error>> {
        let kind = match to_sdp_kind(description.kind) {
            Ok(kind) => kind,
            Err(err) => return Ok(Err(err.into())),
        };
        let peer = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.clone()))?;
        Ok(peer
            .set_remote_description(kind, description.sdp)
            .await
            .map_err(Error::from))
    }

    async fn add_ice_candidate(
        accessor: &Accessor<T, Self>,
        self_: Resource<PeerConnection>,
        candidate: IceCandidate,
    ) -> Result<std::result::Result<(), Error>> {
        let peer = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.clone()))?;
        Ok(peer
            .add_ice_candidate(
                candidate.candidate,
                candidate.sdp_mid,
                candidate.sdp_mline_index,
            )
            .await
            .map_err(Error::from))
    }

    async fn wait_connected(
        accessor: &Accessor<T, Self>,
        self_: Resource<PeerConnection>,
    ) -> Result<std::result::Result<(), Error>> {
        let peer = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.clone()))?;
        Ok(peer.wait_connected().await.map_err(Error::from))
    }

    async fn drop(accessor: &Accessor<T, Self>, rep: Resource<PeerConnection>) -> Result<()> {
        accessor.with(|mut access| {
            access.get().table.delete(rep)?;
            Ok(())
        })
    }
}
