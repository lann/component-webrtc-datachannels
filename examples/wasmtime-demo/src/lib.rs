//! Demo-only host glue for the Wasmtime WebRTC host.
//!
//! The `lann:webrtc-datachannels` host implementation (`types`,
//! `connections`, and the stream/pipe plumbing) lives in the
//! [`wasmtime_webrtc_datachannels`] crate. The binaries in this crate
//! (`wasmtime-webrtc-host`, `cli-signaling`, `echo-remote`) are thin hosts
//! over its `add_to_linker`; this library holds only the engine/context
//! setup they share.

use wasmtime::{Config, Engine, Result};
use wasmtime_webrtc_datachannels::WasiWebrtcCtx;

/// Build the Wasmtime engine every demo binary uses: the component model with
/// component-model async enabled (the `send`/`receive` methods use the async
/// ABI).
pub fn engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    Engine::new(&config)
}

/// Build the WebRTC context from the demo hosts' environment surface: the
/// `WEBRTC_INCLUDE_LOOPBACK` variable opts into loopback ICE candidates
/// (peers running on the same host without another mutually reachable address
/// need it to pair), and the conventional `WEBRTC_MAX_INBOUND_BUFFER_BYTES`
/// variable overrides the inbound buffer bound. These env-driven tweaks are
/// host glue: the host crate itself reads no environment, so a malformed
/// value fails loud here.
pub fn webrtc_ctx() -> WasiWebrtcCtx {
    let mut ctx = WasiWebrtcCtx::new();
    if std::env::var_os("WEBRTC_INCLUDE_LOOPBACK").is_some() {
        ctx.set_setting_engine_hook(|engine| {
            engine.set_include_loopback_candidate(true);
        });
    }
    if let Some(bytes) = wasmtime_webrtc_datachannels::max_inbound_buffer_bytes_from_env()
        .unwrap_or_else(|e| panic!("{e}"))
    {
        ctx.set_max_inbound_buffer_bytes(bytes);
    }
    ctx
}
