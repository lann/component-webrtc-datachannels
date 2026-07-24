//! Wasmtime host for `lann:webrtc-datachannels`, backed by the
//! pure-Rust `webrtc-rs` stack.
//!
//! It is the non-browser counterpart to the Node host: it loads the same
//! `echo-demo` component and invokes the component's exported async `run`. The
//! component stands up both peers itself through the standard `connections`
//! interface, so this binary provisions nothing beyond
//! [`wasmtime_webrtc_datachannels`]'s `add_to_linker`.

use wasmtime::component::{Accessor, Component, HasData, Linker, ResourceTable};
use wasmtime::{Config, Engine, Result, Store};
use wasmtime_webrtc_datachannels::{
    self as webrtc_host, WasiWebrtcCtx, WasiWebrtcCtxView, WasiWebrtcView,
};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../echo-demo/wit",
        world: "webrtc-echo-demo",
        imports: {
            default: async | store | trappable,
        },
        exports: {
            default: async,
        },
        with: {
            "lann:webrtc-datachannels/connections.data-channel-options":
                wasmtime_webrtc_datachannels::DataChannelOptions,
            "lann:webrtc-datachannels/connections.data-channel":
                wasmtime_webrtc_datachannels::DataChannel,
            "lann:webrtc-datachannels/connections.peer-connection":
                wasmtime_webrtc_datachannels::PeerConnection,
        },
    });
}

struct Ctx {
    webrtc: WasiWebrtcCtx,
    table: ResourceTable,
}

impl HasData for Ctx {
    type Data<'a> = &'a mut Self;
}

impl WasiWebrtcView for Ctx {
    fn webrtc(&mut self) -> WasiWebrtcCtxView<'_> {
        WasiWebrtcCtxView {
            ctx: &mut self.webrtc,
            table: &mut self.table,
        }
    }
}

fn engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    Engine::new(&config)
}

/// Build the WebRTC context, opting into loopback ICE candidates when the
/// `WEBRTC_INCLUDE_LOOPBACK` environment variable is set. The component stands
/// up both peers in this one process, so on hosts without another mutually
/// reachable address this is required for them to pair.
fn webrtc_ctx() -> WasiWebrtcCtx {
    let mut ctx = WasiWebrtcCtx::new();
    if std::env::var_os("WEBRTC_INCLUDE_LOOPBACK").is_some() {
        ctx.set_setting_engine_hook(|engine| {
            engine.set_include_loopback_candidate(true);
        });
    }
    ctx
}

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "../echo-demo/build/echo-demo.component.wasm".to_string());
    let message_count: u32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);
    let message_size: u32 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(4096);

    let engine = engine()?;
    let component = Component::from_file(&engine, &path)?;
    let mut linker: Linker<Ctx> = Linker::new(&engine);
    // Shared `lann:webrtc-datachannels` imports — the component's only ones.
    webrtc_host::add_to_linker(&mut linker)?;

    let mut store = Store::new(
        &engine,
        Ctx {
            webrtc: webrtc_ctx(),
            table: ResourceTable::new(),
        },
    );
    let demo = bindings::WebrtcEchoDemo::instantiate_async(&mut store, &component, &linker).await?;

    let started = std::time::Instant::now();
    let stats = store
        .run_concurrent(async move |accessor: &Accessor<Ctx>| {
            demo.demo_webrtc_echo_demo()
                .call_run(
                    accessor,
                    bindings::exports::demo::webrtc_echo::demo::DemoConfig {
                        message_count,
                        message_size,
                    },
                )
                .await
        })
        .await??;

    let elapsed = started.elapsed();
    match stats {
        Ok(stats) => {
            let mib = stats.bytes_echoed as f64 / (1024.0 * 1024.0);
            println!("echo-demo (Wasmtime / webrtc-rs host) result:");
            println!("  messages sent:     {}", stats.messages_sent);
            println!("  messages received: {}", stats.messages_received);
            println!("  bytes echoed:      {}", stats.bytes_echoed);
            println!(
                "  elapsed:           {:.1} ms  (~{:.1} MiB/s round-trip)",
                elapsed.as_secs_f64() * 1000.0,
                mib / elapsed.as_secs_f64()
            );
            if stats.messages_received != message_count {
                return Err(wasmtime::Error::msg(format!(
                    "expected {message_count} messages, got {}",
                    stats.messages_received
                )));
            }
            let expected_bytes = u64::from(message_count) * u64::from(message_size);
            if stats.bytes_echoed != expected_bytes {
                return Err(wasmtime::Error::msg(format!(
                    "expected {expected_bytes} echoed bytes, got {}",
                    stats.bytes_echoed
                )));
            }
            println!("\nOK: every message round-tripped through the WebRTC data channel.");
        }
        Err(err) => {
            return Err(wasmtime::Error::msg(format!(
                "demo returned error: {err:?}"
            )))
        }
    }

    Ok(())
}
