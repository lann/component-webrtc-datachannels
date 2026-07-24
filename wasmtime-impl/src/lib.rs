//! Wasmtime host implementation of the `lann:webrtc-datachannels`
//! interfaces, backed by the pure-Rust
//! [`webrtc-rs`](https://github.com/webrtc-rs/webrtc) stack.
//!
//! This crate factors the host-agnostic part of the Wasmtime WebRTC host out of
//! the demo binaries so any host can satisfy the `lann:webrtc-datachannels`
//! imports with one call to [`add_to_linker`]. It is a wasip3 (component-model
//! async) implementation modeled after [`wasmtime_wasi_http::p3`]: a host embeds
//! a [`WasiWebrtcCtx`] in its store state, implements [`WasiWebrtcView`] to
//! expose it alongside the store's [`ResourceTable`], and calls
//! [`add_to_linker`] to satisfy the `types` and `connections` imports with a
//! real WebRTC/SCTP data channel.
//!
//! [`wasmtime_wasi_http::p3`]: https://docs.rs/wasmtime-wasi-http

pub mod bindings;
mod data_channel;
mod error;
mod host;
mod peer_connection;

pub use data_channel::{DataChannel, DEFAULT_MAX_INBOUND_BUFFER_BYTES, MAX_INBOUND_BUFFER_ENV};
pub use error::{WebrtcError, WebrtcResult};
pub use peer_connection::PeerConnection;

use std::sync::Arc;

use wasmtime::component::{HasData, Linker, ResourceTable};
use webrtc::peer_connection::SettingEngine;

/// A hook run against a fresh [`SettingEngine`] before each peer connection is
/// created. See [`WasiWebrtcCtx::set_setting_engine_hook`].
pub type SettingEngineHook = Arc<dyn Fn(&mut SettingEngine) + Send + Sync>;

/// A STUN/TURN server a peer connection may gather server-reflexive and relay
/// candidates from. Mirrors `webrtc-rs`'s `RTCIceServer`; `username`/`credential`
/// are ignored for STUN-only URLs.
#[derive(Clone, Debug, Default)]
pub struct WebrtcIceServer {
    /// STUN/TURN URLs, e.g. `stun:host:3478` or `turn:host:3478?transport=udp`.
    pub urls: Vec<String>,
    /// TURN long-term-credential username (empty for STUN-only servers).
    pub username: String,
    /// TURN long-term-credential secret (empty for STUN-only servers).
    pub credential: String,
}

/// Network/ICE configuration applied when a peer connection is built.
///
/// The default value reproduces the crate's built-in behavior: bind a single
/// ephemeral UDP socket on IPv4 loopback, no STUN/TURN servers, and the `all`
/// ICE transport policy. The conformance netns lab (see `conformance/README.md`
/// Phase 5) overrides these to bind a scenario-specific interface address and to
/// point at a STUN/TURN server, forcing server-reflexive or relay candidate
/// paths.
#[derive(Clone, Debug, Default)]
pub struct WebrtcIceConfig {
    /// UDP socket addresses to bind and gather host candidates from. When empty
    /// the crate binds its default (`127.0.0.1:0`). Use a `:0` port to let the
    /// OS choose an ephemeral port.
    pub udp_addrs: Vec<String>,
    /// STUN/TURN servers to gather server-reflexive and relay candidates from.
    pub ice_servers: Vec<WebrtcIceServer>,
    /// When `true`, only TURN relay candidates are used (the `relay` ICE
    /// transport policy); requires at least one TURN server in `ice_servers`.
    pub relay_only: bool,
}

impl WebrtcIceConfig {
    /// True when this configuration leaves every field at its default, in which
    /// case the crate's built-in loopback behavior is used unchanged.
    pub fn is_default(&self) -> bool {
        self.udp_addrs.is_empty() && self.ice_servers.is_empty() && !self.relay_only
    }
}

/// Configuration and per-store state for the WebRTC data-channel host.
///
/// This is intentionally minimal (mirroring `wasmtime_wasi_http`'s
/// `WasiHttpCtx`); it exists so hosts have a stable place to grow configuration
/// without changing the [`WasiWebrtcView`] shape.
///
/// The knobs so far are the [`SettingEngine`] hook (see
/// [`set_setting_engine_hook`](Self::set_setting_engine_hook)), the analogue of
/// wasmtime-wasi-http's `WasiHttpHooks`, and the [`WebrtcIceConfig`] ICE
/// server configuration (see [`set_ice_config`](Self::set_ice_config)): the
/// crate itself hardcodes no environment-driven ICE behavior, leaving loopback
/// and similar tweaks to demo/test hosts.
#[derive(Clone, Default)]
#[non_exhaustive]
pub struct WasiWebrtcCtx {
    setting_engine_hook: Option<SettingEngineHook>,
    ice_config: WebrtcIceConfig,
}

impl std::fmt::Debug for WasiWebrtcCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasiWebrtcCtx")
            .field(
                "setting_engine_hook",
                &self.setting_engine_hook.as_ref().map(|_| "<hook>"),
            )
            .field("ice_config", &self.ice_config)
            .finish()
    }
}

impl WasiWebrtcCtx {
    /// Create a new, default context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a hook run against a fresh [`SettingEngine`] before each peer
    /// connection this context creates.
    ///
    /// This is the customization point for `webrtc-rs` behavior the crate does
    /// not opt into itself (mirroring wasmtime-wasi-http's `WasiHttpHooks`). For
    /// example, two peers sharing one host may only reach each other over
    /// loopback, so a demo/test host can enable loopback ICE candidates:
    ///
    /// ```
    /// # use wasmtime_webrtc_datachannels::WasiWebrtcCtx;
    /// let mut ctx = WasiWebrtcCtx::new();
    /// ctx.set_setting_engine_hook(|engine| {
    ///     engine.set_include_loopback_candidate(true);
    /// });
    /// ```
    pub fn set_setting_engine_hook(
        &mut self,
        hook: impl Fn(&mut SettingEngine) + Send + Sync + 'static,
    ) {
        self.setting_engine_hook = Some(Arc::new(hook));
    }

    /// The registered [`SettingEngine`] hook, if any, cheaply cloned so callers
    /// can apply it without holding a borrow of the context.
    pub fn setting_engine_hook(&self) -> Option<SettingEngineHook> {
        self.setting_engine_hook.clone()
    }

    /// Set the network/ICE configuration applied to every peer connection this
    /// context creates (bind addresses, STUN/TURN servers, relay-only policy).
    ///
    /// The default leaves the crate's built-in loopback behavior unchanged; the
    /// conformance netns lab overrides it per scenario (see `conformance/README.md`
    /// Phase 5).
    pub fn set_ice_config(&mut self, config: WebrtcIceConfig) {
        self.ice_config = config;
    }

    /// The configured network/ICE configuration, cheaply cloned so callers can
    /// apply it without holding a borrow of the context.
    pub fn ice_config(&self) -> WebrtcIceConfig {
        self.ice_config.clone()
    }
}

/// A borrowed view into a host's [`WasiWebrtcCtx`] and its [`ResourceTable`].
///
/// Returned by [`WasiWebrtcView::webrtc`], this is the [`HasData::Data`] the
/// generated host bindings operate on.
pub struct WasiWebrtcCtxView<'a> {
    /// Mutable reference to the WebRTC host context.
    pub ctx: &'a mut WasiWebrtcCtx,
    /// Mutable reference to the table used to manage host resources.
    pub table: &'a mut ResourceTable,
}

/// A trait that provides access to the [`WasiWebrtcCtx`] host state.
///
/// Implement this for your store's data type so [`add_to_linker`] can wire the
/// `lann:webrtc-datachannels` imports onto your linker.
pub trait WasiWebrtcView: Send {
    /// Return a [`WasiWebrtcCtxView`] from a mutable reference to `self`.
    fn webrtc(&mut self) -> WasiWebrtcCtxView<'_>;
}

/// The type for which this crate implements the `lann:webrtc-datachannels`
/// interfaces. Used as the [`HasData`] marker for the generated bindings.
pub struct WasiWebrtc;

impl HasData for WasiWebrtc {
    type Data<'a> = WasiWebrtcCtxView<'a>;
}

/// Backing type for the `connections.data-channel-options` resource.
///
/// A plain configuration builder (mirroring `wasi:http`'s `request-options`):
/// the guest constructs a default value through the imported constructor,
/// adjusts the fields through the setters, then hands the resource to a
/// data-channel-creating function such as
/// `peer-connection.create-data-channel`. The host that receives the resource
/// reads these fields back to configure the `webrtc-rs` channel.
#[derive(Clone, Debug)]
pub struct DataChannelOptions {
    /// The channel label. Both peers observe the same label.
    pub label: String,
    /// Whether messages are delivered in order.
    pub ordered: bool,
    /// The maximum number of retransmissions before a message is dropped, or
    /// `None` for fully reliable delivery.
    pub max_retransmits: Option<u16>,
}

impl Default for DataChannelOptions {
    fn default() -> Self {
        Self {
            label: String::new(),
            ordered: true,
            max_retransmits: None,
        }
    }
}

/// Add the `lann:webrtc-datachannels` interfaces implemented by this crate
/// (`types` and `connections`) to the provided [`Linker`].
///
/// The store's data type `T` must implement [`WasiWebrtcView`]. The engine's
/// [`Config`](wasmtime::Config) must have `wasm_component_model_async` enabled,
/// since the `send`/`receive` methods use the component-model async ABI.
///
/// # Example
///
/// ```no_run
/// use wasmtime::component::{Linker, ResourceTable};
/// use wasmtime::{Engine, Result};
/// use wasmtime_webrtc_datachannels::{
///     add_to_linker, WasiWebrtcCtx, WasiWebrtcCtxView, WasiWebrtcView,
/// };
///
/// struct MyState {
///     webrtc: WasiWebrtcCtx,
///     table: ResourceTable,
/// }
///
/// impl WasiWebrtcView for MyState {
///     fn webrtc(&mut self) -> WasiWebrtcCtxView<'_> {
///         WasiWebrtcCtxView {
///             ctx: &mut self.webrtc,
///             table: &mut self.table,
///         }
///     }
/// }
///
/// fn wire(linker: &mut Linker<MyState>) -> Result<()> {
///     add_to_linker(linker)
/// }
/// ```
pub fn add_to_linker<T>(linker: &mut Linker<T>) -> wasmtime::Result<()>
where
    T: WasiWebrtcView + 'static,
{
    bindings::webrtc_datachannels::types::add_to_linker::<_, WasiWebrtc>(linker, T::webrtc)?;
    bindings::webrtc_datachannels::connections::add_to_linker::<_, WasiWebrtc>(linker, T::webrtc)?;
    Ok(())
}
