//! `echo-remote` host: runs one peer of the two-process echo demo under
//! Wasmtime.
//!
//! It loads the `echo-remote` component (one role of a remote echo run) and
//! provisions its two imports:
//!
//!   * the `connections`/`types` surface via
//!     [`wasmtime_webrtc_datachannels`]'s `add_to_linker` (a real `webrtc-rs`
//!     peer connection), and
//!   * the demo `rendezvous` signaling mailbox, implemented natively here with
//!     an HTTP client speaking `conformance-signalingd`'s mailbox protocol
//!     (`conformance/signaling/PROTOCOL.md`).
//!
//! Run two instances — an offerer and an answerer — pointed at the same room
//! on the same signaling server, and a real WebRTC connection forms between
//! two genuinely separate component instances:
//!
//! ```sh
//! conformance-signalingd --host 127.0.0.1 --port 8080 &
//! echo-remote <component.wasm> --role answerer --server http://127.0.0.1:8080 --room demo &
//! echo-remote <component.wasm> --role offerer  --server http://127.0.0.1:8080 --room demo
//! ```

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use wasmtime::component::{Accessor, Component, HasData, Linker, Resource, ResourceTable};
use wasmtime::{Result, Store};
use wasmtime_webrtc_datachannels::{
    self as webrtc_host, WasiWebrtcCtx, WasiWebrtcCtxView, WasiWebrtcView,
};
use wasmtime_webrtc_host::{engine, webrtc_ctx};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../echo-demo/wit",
        world: "webrtc-echo-remote",
        imports: {
            default: async | store | trappable,
        },
        exports: {
            default: async,
        },
        with: {
            "polymorph:webrtc-datachannels/connections.data-channel-options":
                wasmtime_webrtc_datachannels::DataChannelOptions,
            "polymorph:webrtc-datachannels/connections.peer-connection-config":
                wasmtime_webrtc_datachannels::PeerConnectionConfig,
            "polymorph:webrtc-datachannels/connections.data-channel":
                wasmtime_webrtc_datachannels::DataChannel,
            "polymorph:webrtc-datachannels/connections.peer-connection":
                wasmtime_webrtc_datachannels::PeerConnection,
            "demo:webrtc-echo/rendezvous.session": crate::RendezvousSession,
        },
    });
}

use bindings::demo::webrtc_echo::rendezvous::{self, Role as RendezvousRole};
use bindings::exports::demo::webrtc_echo::remote::RemoteConfig;
use bindings::polymorph::webrtc_datachannels::types::Error;

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

// --- native rendezvous host ---------------------------------------------------

/// A joined rendezvous session: an HTTP client bound to one `{room}` and
/// `{role}` on the signaling server. `Arc`-backed so a handle can be cloned out
/// of the resource table and its async methods driven without holding the
/// store borrow across `.await`.
#[derive(Clone)]
pub struct RendezvousSession {
    client: reqwest::Client,
    base: String,
    room: String,
    role: RendezvousRole,
    /// The next sequence number to fetch from the peer's mailbox.
    recv_seq: Arc<AtomicUsize>,
}

impl RendezvousSession {
    /// This session's own role path segment.
    fn own_role(&self) -> &'static str {
        match self.role {
            RendezvousRole::Offerer => "offerer",
            RendezvousRole::Answerer => "answerer",
        }
    }

    /// The peer's role path segment (the mailbox this session consumes).
    fn peer_role(&self) -> &'static str {
        match self.role {
            RendezvousRole::Offerer => "answerer",
            RendezvousRole::Answerer => "offerer",
        }
    }
}

/// Map any host-side rendezvous failure to the guest-visible `error.other`.
fn rendezvous_error(detail: impl std::fmt::Display) -> Error {
    Error::Other(format!("rendezvous: {detail}"))
}

impl rendezvous::Host for Ctx {}

impl rendezvous::HostSession for Ctx {}

impl rendezvous::HostSessionWithStore<Ctx> for Ctx {
    async fn open(
        accessor: &Accessor<Ctx, Ctx>,
        server: String,
        room: String,
        as_role: RendezvousRole,
    ) -> wasmtime::Result<std::result::Result<Resource<RendezvousSession>, Error>> {
        let session = RendezvousSession {
            client: reqwest::Client::new(),
            base: server.trim_end_matches('/').to_string(),
            room,
            role: as_role,
            recv_seq: Arc::new(AtomicUsize::new(0)),
        };
        accessor.with(|mut access| {
            let resource = access.get().table.push(session)?;
            Ok(Ok(resource))
        })
    }

    async fn send(
        accessor: &Accessor<Ctx, Ctx>,
        self_: Resource<RendezvousSession>,
        blob: Vec<u8>,
    ) -> wasmtime::Result<std::result::Result<(), Error>> {
        let session = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.clone()))?;
        let url = format!(
            "{}/rooms/{}/{}",
            session.base,
            session.room,
            session.own_role()
        );
        Ok(match session.client.post(&url).body(blob).send().await {
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) => Err(rendezvous_error(format!(
                "publish status {}",
                resp.status()
            ))),
            Err(err) => Err(rendezvous_error(err)),
        })
    }

    async fn recv(
        accessor: &Accessor<Ctx, Ctx>,
        self_: Resource<RendezvousSession>,
    ) -> wasmtime::Result<std::result::Result<Option<Vec<u8>>, Error>> {
        let session = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.clone()))?;
        Ok(fetch_next(&session).await)
    }

    async fn done(
        accessor: &Accessor<Ctx, Ctx>,
        self_: Resource<RendezvousSession>,
    ) -> wasmtime::Result<std::result::Result<(), Error>> {
        let session = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.clone()))?;
        let url = format!(
            "{}/rooms/{}/{}/done",
            session.base,
            session.room,
            session.own_role()
        );
        Ok(match session.client.post(&url).send().await {
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) => Err(rendezvous_error(format!("done status {}", resp.status()))),
            Err(err) => Err(rendezvous_error(err)),
        })
    }

    async fn drop(
        accessor: &Accessor<Ctx, Ctx>,
        rep: Resource<RendezvousSession>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            access.get().table.delete(rep)?;
            Ok(())
        })
    }
}

/// Fetch the next blob from the peer's mailbox, long-polling and retrying `304`
/// until a blob arrives (`some`) or the peer marks its mailbox done (`none`).
async fn fetch_next(session: &RendezvousSession) -> std::result::Result<Option<Vec<u8>>, Error> {
    loop {
        let seq = session.recv_seq.load(Ordering::SeqCst);
        let url = format!(
            "{}/rooms/{}/{}?seq={}&wait=10000",
            session.base,
            session.room,
            session.peer_role(),
            seq
        );
        let resp = session
            .client
            .get(&url)
            .send()
            .await
            .map_err(rendezvous_error)?;
        match resp.status().as_u16() {
            // A blob is available: advance our read cursor and return it.
            200 => {
                let bytes = resp.bytes().await.map_err(rendezvous_error)?.to_vec();
                session.recv_seq.store(seq + 1, Ordering::SeqCst);
                return Ok(Some(bytes));
            }
            // The peer marked its mailbox done at or before this seq.
            204 => return Ok(None),
            // Not yet available; retry the same seq.
            304 => continue,
            other => return Err(rendezvous_error(format!("fetch status {other}"))),
        }
    }
}

// --- host entry point ----------------------------------------------------------

struct Cli {
    component: String,
    role: RendezvousRole,
    server: String,
    room: String,
    message_count: u32,
    message_size: u32,
}

fn usage() -> wasmtime::Error {
    wasmtime::Error::msg(
        "usage: echo-remote <component.wasm> --role <offerer|answerer> \
         --server <base-url> --room <room> [--count N] [--size BYTES]",
    )
}

fn parse_args() -> Result<Cli> {
    let mut args = std::env::args().skip(1);
    let component = args.next().ok_or_else(usage)?;
    let mut role = None;
    let mut server = None;
    let mut room = None;
    let mut message_count = 100u32;
    let mut message_size = 1024u32;
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(usage);
        match flag.as_str() {
            "--role" => {
                role = Some(match value()?.as_str() {
                    "offerer" => RendezvousRole::Offerer,
                    "answerer" => RendezvousRole::Answerer,
                    _ => return Err(usage()),
                })
            }
            "--server" => server = Some(value()?),
            "--room" => room = Some(value()?),
            "--count" => message_count = value()?.parse().map_err(|_| usage())?,
            "--size" => message_size = value()?.parse().map_err(|_| usage())?,
            _ => return Err(usage()),
        }
    }
    Ok(Cli {
        component,
        role: role.ok_or_else(usage)?,
        server: server.ok_or_else(usage)?,
        room: room.ok_or_else(usage)?,
        message_count,
        message_size,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = env_logger::try_init();
    let cli = parse_args()?;

    let engine = engine()?;
    let component = Component::from_file(&engine, &cli.component)?;
    let mut linker: Linker<Ctx> = Linker::new(&engine);
    // Shared `polymorph:webrtc-datachannels` imports.
    webrtc_host::add_to_linker(&mut linker)?;
    // The demo rendezvous mailbox, implemented natively above.
    rendezvous::add_to_linker::<Ctx, Ctx>(&mut linker, |c| c)?;

    let mut store = Store::new(
        &engine,
        Ctx {
            webrtc: webrtc_ctx(),
            table: ResourceTable::new(),
        },
    );
    let demo =
        bindings::WebrtcEchoRemote::instantiate_async(&mut store, &component, &linker).await?;

    let role = cli.role;
    let config = RemoteConfig {
        server: cli.server,
        room: cli.room,
        role,
        message_count: cli.message_count,
        message_size: cli.message_size,
    };
    let stats = store
        .run_concurrent(async move |accessor: &Accessor<Ctx>| {
            demo.demo_webrtc_echo_remote()
                .call_run(accessor, config)
                .await
        })
        .await??;

    match stats {
        Ok(stats) => {
            let role = match role {
                RendezvousRole::Offerer => "offerer",
                RendezvousRole::Answerer => "answerer",
            };
            println!(
                "echo-remote ({role}): sent {} received {} bytes {}",
                stats.messages_sent, stats.messages_received, stats.bytes_echoed
            );
            if role == "offerer" {
                let expected_bytes = u64::from(cli.message_count) * u64::from(cli.message_size);
                if stats.messages_received != cli.message_count
                    || stats.bytes_echoed != expected_bytes
                {
                    return Err(wasmtime::Error::msg(format!(
                        "expected {} messages / {expected_bytes} bytes, got {} / {}",
                        cli.message_count, stats.messages_received, stats.bytes_echoed
                    )));
                }
            }
            println!("OK: {role} finished.");
        }
        Err(err) => {
            return Err(wasmtime::Error::msg(format!(
                "echo-remote returned error: {err:?}"
            )))
        }
    }
    Ok(())
}
