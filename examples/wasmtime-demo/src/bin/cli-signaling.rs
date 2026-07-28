//! `cli-signaling` host: runs the manual-signaling CLI guest under Wasmtime.
//!
//! It provisions three things the guest needs and wires them onto one
//! `Linker`/`Store`:
//!
//!   * `wasi:cli@0.3` (async run + stdio) via `wasmtime_wasi::p3`, so the guest
//!     can prompt the user over stdout and read pasted blobs from stdin,
//!   * `wasi:*@0.2` via `wasmtime_wasi::p2`, which the guest's Rust `std` still
//!     lowers to, and
//!   * the `connections`/`types` imports (provided by
//!     [`wasmtime_webrtc_datachannels`]), which the guest drives with
//!     guest-side vanilla ICE, so the offer/answer exchange drives a real
//!     connection.
//!
//! Usage: `cli-signaling <component.wasm> [offerer|answerer]`.

use wasmtime::component::{Component, HasData, Linker, ResourceTable};
use wasmtime::{Result, Store};
use wasmtime_wasi::p3::bindings::Command;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_webrtc_datachannels::{WasiWebrtcCtx, WasiWebrtcCtxView, WasiWebrtcView};
use wasmtime_webrtc_host::{engine, webrtc_ctx};

struct Ctx {
    wasi: WasiCtx,
    webrtc: WasiWebrtcCtx,
    table: ResourceTable,
}

impl HasData for Ctx {
    type Data<'a> = &'a mut Self;
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiWebrtcView for Ctx {
    fn webrtc(&mut self) -> WasiWebrtcCtxView<'_> {
        WasiWebrtcCtxView {
            ctx: &mut self.webrtc,
            table: &mut self.table,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = env_logger::try_init();
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or_else(|| {
        wasmtime::Error::msg("usage: cli-signaling <component.wasm> [offerer|answerer]")
    })?;
    // Remaining args are forwarded to the guest (e.g. the role).
    let guest_args: Vec<String> = std::iter::once("cli-signaling".to_string())
        .chain(args)
        .collect();

    let engine = engine()?;
    let component = Component::from_file(&engine, &path)?;

    let mut linker: Linker<Ctx> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    // Shared `connections`/`types` imports — the component's only non-wasi ones.
    wasmtime_webrtc_datachannels::add_to_linker(&mut linker)?;

    let mut wasi = WasiCtx::builder();
    wasi.inherit_stdio().inherit_env().args(&guest_args);
    let mut store = Store::new(
        &engine,
        Ctx {
            wasi: wasi.build(),
            webrtc: webrtc_ctx(),
            table: ResourceTable::new(),
        },
    );

    let command = Command::instantiate_async(&mut store, &component, &linker).await?;
    let result = store
        .run_concurrent(async move |store| command.wasi_cli_run().call_run(store).await)
        .await??;

    // Linger briefly before exiting: process exit discards the SCTP send
    // queue, so a message the guest handed to the transport just before
    // returning (its peer may still be waiting on it) gets a bounded window
    // to reach the wire. The flush-aware teardown this papers over is
    // tracked as https://github.com/lann/component-webrtc-datachannels/issues/126.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    match result {
        Ok(()) => Ok(()),
        Err(()) => Err(wasmtime::Error::msg("guest signalled failure")),
    }
}
