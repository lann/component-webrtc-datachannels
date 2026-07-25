//! Shared building blocks for the wasmtime conformance adapter and the
//! cross-runtime interop orchestrator.
//!
//! This library provides the wasmtime host provisioning (the [`Ctx`] store
//! state, the WebRTC host via [`wasmtime_webrtc_datachannels`], and the native
//! HTTP [`mailbox`] host backed by `conformance-signalingd`), the guest bindings,
//! and the primitive to run one guest instance to a WIT-observable
//! [`TestOutcome`]. The adapter binary ([`crate::main`]) layers the full-corpus
//! orchestration on top; the `conformance-interop` binary reuses the same
//! primitive to drive one wasmtime peer of an interop pair.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use wasmtime::component::{Accessor, Component, HasData, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_webrtc_datachannels::{
    self as webrtc_host, WasiWebrtcCtx, WasiWebrtcCtxView, WasiWebrtcView,
};

pub mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "conformance",
        imports: {
            default: async | store | trappable,
        },
        exports: {
            default: async,
        },
        with: {
            "lann:webrtc-datachannels/connections.data-channel-options":
                wasmtime_webrtc_datachannels::DataChannelOptions,
            "lann:webrtc-datachannels/connections.peer-connection-config":
                wasmtime_webrtc_datachannels::PeerConnectionConfig,
            "lann:webrtc-datachannels/connections.data-channel":
                wasmtime_webrtc_datachannels::DataChannel,
            "lann:webrtc-datachannels/connections.peer-connection":
                wasmtime_webrtc_datachannels::PeerConnection,
            "conformance:signaling/mailbox.session": crate::MailboxSession,
        },
    });
}

use bindings::conformance::signaling::mailbox::{self, Role as MailboxRole};
use bindings::exports::conformance::suite::runner::TestResult;
pub use bindings::exports::conformance::suite::runner::{Role, TestConfig};
use bindings::lann::webrtc_datachannels::types::Error;
pub use bindings::Conformance;
use conformance_adapter_common::TestOutcome;
pub use wasmtime_webrtc_datachannels::{WebrtcIceConfig, WebrtcIceServer};

/// Per-store host state: the WebRTC host context plus the resource table shared
/// by the WebRTC and mailbox host resources.
pub struct Ctx {
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

// ----- mailbox host ---------------------------------------------------------

/// A joined mailbox session: an HTTP client bound to one `{room}` and `{role}`
/// on the signaling server. `Arc`-backed so a handle can be cloned out of the
/// resource table and its async methods driven without holding the store borrow
/// across `.await` (mirroring the WebRTC host's data channel).
#[derive(Clone)]
pub struct MailboxSession {
    client: reqwest::Client,
    base: String,
    room: String,
    role: MailboxRole,
    /// The next sequence number to fetch from the peer's mailbox.
    recv_seq: Arc<AtomicUsize>,
}

impl MailboxSession {
    /// This session's own role path segment.
    fn own_role(&self) -> &'static str {
        role_str(self.role)
    }

    /// The peer's role path segment (the mailbox this session consumes).
    fn peer_role(&self) -> &'static str {
        match self.role {
            MailboxRole::Offerer => "answerer",
            MailboxRole::Answerer => "offerer",
        }
    }
}

/// The path segment for a mailbox role.
fn role_str(role: MailboxRole) -> &'static str {
    match role {
        MailboxRole::Offerer => "offerer",
        MailboxRole::Answerer => "answerer",
    }
}

/// Map any host-side mailbox failure to the guest-visible `error.other`.
fn mailbox_error(detail: impl std::fmt::Display) -> Error {
    Error::Other(format!("mailbox: {detail}"))
}

impl mailbox::Host for Ctx {}

impl mailbox::HostSession for Ctx {}

impl mailbox::HostSessionWithStore<Ctx> for Ctx {
    async fn open(
        accessor: &Accessor<Ctx, Ctx>,
        server: String,
        room: String,
        as_role: MailboxRole,
    ) -> wasmtime::Result<std::result::Result<Resource<MailboxSession>, Error>> {
        tracing::info!(target: "conformance", room, role = role_str(as_role), "mailbox opened");
        let session = MailboxSession {
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
        self_: Resource<MailboxSession>,
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
        let len = blob.len();
        Ok(match session.client.post(&url).body(blob).send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!(target: "conformance", room = session.room, role = session.own_role(), len, "mailbox sent");
                Ok(())
            }
            Ok(resp) => Err(mailbox_error(format!("publish status {}", resp.status()))),
            Err(err) => Err(mailbox_error(err)),
        })
    }

    async fn recv(
        accessor: &Accessor<Ctx, Ctx>,
        self_: Resource<MailboxSession>,
    ) -> wasmtime::Result<std::result::Result<Option<Vec<u8>>, Error>> {
        let session = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.clone()))?;
        let result = fetch_next(&session).await;
        if let Ok(blob) = &result {
            tracing::info!(
                target: "conformance",
                room = session.room,
                role = session.own_role(),
                len = ?blob.as_ref().map(Vec::len),
                "mailbox received"
            );
        }
        Ok(result)
    }

    async fn done(
        accessor: &Accessor<Ctx, Ctx>,
        self_: Resource<MailboxSession>,
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
            Ok(resp) if resp.status().is_success() => {
                tracing::info!(target: "conformance", room = session.room, role = session.own_role(), "mailbox done");
                Ok(())
            }
            Ok(resp) => Err(mailbox_error(format!("done status {}", resp.status()))),
            Err(err) => Err(mailbox_error(err)),
        })
    }

    async fn drop(
        accessor: &Accessor<Ctx, Ctx>,
        rep: Resource<MailboxSession>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            access.get().table.delete(rep)?;
            Ok(())
        })
    }
}

/// Fetch the next blob from the peer's mailbox, long-polling and retrying `304`
/// until a blob arrives (`some`) or the peer marks its mailbox done (`none`).
async fn fetch_next(session: &MailboxSession) -> std::result::Result<Option<Vec<u8>>, Error> {
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
            .map_err(mailbox_error)?;
        match resp.status().as_u16() {
            // A blob is available: advance our read cursor and return it.
            200 => {
                let bytes = resp.bytes().await.map_err(mailbox_error)?.to_vec();
                session.recv_seq.store(seq + 1, Ordering::SeqCst);
                return Ok(Some(bytes));
            }
            // The peer marked its mailbox done at or before this seq.
            204 => return Ok(None),
            // Not yet available; retry the same seq.
            304 => continue,
            other => return Err(mailbox_error(format!("fetch status {other}"))),
        }
    }
}

/// Add the native mailbox host to `linker`.
pub fn add_mailbox_to_linker(linker: &mut Linker<Ctx>) -> Result<()> {
    mailbox::add_to_linker::<Ctx, Ctx>(linker, |c| c)?;
    Ok(())
}

// ----- guest orchestration --------------------------------------------------

/// A wasmtime engine configured for the component model with async support.
pub fn build_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    Ok(Engine::new(&config)?)
}

/// A fresh store whose WebRTC host restricts ICE to loopback so two same-host
/// peers pair deterministically. Peers bind on IPv4 loopback, so only loopback
/// candidates are gathered, and `set_include_loopback_candidate` keeps them
/// rather than discarding them; this avoids the flaky LAN / container /
/// link-local candidate pairs that otherwise stall the handshake.
pub fn new_store(engine: &Engine) -> Store<Ctx> {
    let mut webrtc = WasiWebrtcCtx::new();
    webrtc.set_setting_engine_hook(|engine| {
        engine.set_include_loopback_candidate(true);
    });
    apply_env_buffer_bound(&mut webrtc);
    Store::new(
        engine,
        Ctx {
            webrtc,
            table: ResourceTable::new(),
        },
    )
}

/// A fresh store whose WebRTC host uses an explicit [`WebrtcIceConfig`] — the ICE
/// lab's per-scenario network configuration (bind address, STUN/TURN servers,
/// relay-only policy; see the netns lab in `conformance/README.md`). Unlike [`new_store`],
/// loopback candidates are *not* forced: lab peers connect over real interface
/// addresses, so gathering the loopback host candidate would only add a
/// never-connectable pair.
///
/// When `disable_mdns` is set, multicast-DNS candidate gathering is turned off.
/// The Shadow environment (`conformance-shadow`) needs this because Shadow's
/// simulated network stack does not implement the multicast-socket options
/// (`SO_REUSEADDR`/`SO_REUSEPORT`) mDNS binds with; the routed netns lab leaves
/// it on, so both environments keep their own behavior.
pub fn new_store_with_ice(engine: &Engine, ice: WebrtcIceConfig, disable_mdns: bool) -> Store<Ctx> {
    let mut webrtc = WasiWebrtcCtx::new();
    webrtc.set_ice_config(ice);
    if disable_mdns {
        webrtc.set_setting_engine_hook(|engine| {
            engine.set_multicast_dns_mode(rtc::ice::mdns::MulticastDnsMode::Disabled);
        });
    }
    apply_env_buffer_bound(&mut webrtc);
    Store::new(
        engine,
        Ctx {
            webrtc,
            table: ResourceTable::new(),
        },
    )
}

/// Apply the `WEBRTC_MAX_INBOUND_BUFFER_BYTES` override to the host context
/// when set (the host crate itself reads no environment). A malformed value
/// panics: silently reverting to the default bound would invalidate exactly
/// the overflow test that set it.
fn apply_env_buffer_bound(webrtc: &mut WasiWebrtcCtx) {
    let env = conformance_adapter_common::MAX_INBOUND_BUFFER_ENV;
    match std::env::var(env) {
        Ok(value) if !value.is_empty() => {
            let bytes = value
                .parse::<usize>()
                .ok()
                .filter(|&bytes| bytes > 0)
                .unwrap_or_else(|| {
                    panic!("invalid {env} {value:?}: expected a positive byte count")
                });
            webrtc.set_max_inbound_buffer_bytes(bytes);
        }
        _ => {}
    }
}

/// Build a test config for one instance.
pub fn make_config(role: Role, base_url: &str, room: &str, count: u32, size: u32) -> TestConfig {
    TestConfig {
        role,
        signaling_server: base_url.to_string(),
        room: room.to_string(),
        message_count: count,
        message_size: size,
    }
}

/// Run one guest instance to a [`TestOutcome`].
pub async fn run_instance(
    engine: &Engine,
    component: &Component,
    test_id: &str,
    config: TestConfig,
) -> Result<TestOutcome> {
    run_instance_in_store(new_store(engine), engine, component, test_id, config).await
}

/// Run one guest instance to a [`TestOutcome`] with an explicit netns-lab network
/// configuration (bind address, STUN/TURN servers, relay-only policy). Used by
/// the single-peer `conformance-peer` binary the netns-lab orchestrator launches
/// inside a network namespace.
///
/// `disable_mdns` turns off multicast-DNS candidate gathering; see
/// [`new_store_with_ice`] for why the Shadow environment needs it.
pub async fn run_instance_with_ice(
    engine: &Engine,
    component: &Component,
    test_id: &str,
    config: TestConfig,
    ice: WebrtcIceConfig,
    disable_mdns: bool,
) -> Result<TestOutcome> {
    run_instance_in_store(
        new_store_with_ice(engine, ice, disable_mdns),
        engine,
        component,
        test_id,
        config,
    )
    .await
}

/// Instantiate the guest in `store` and drive one `run-test` call to its outcome.
async fn run_instance_in_store(
    mut store: Store<Ctx>,
    engine: &Engine,
    component: &Component,
    test_id: &str,
    config: TestConfig,
) -> Result<TestOutcome> {
    let mut linker: Linker<Ctx> = Linker::new(engine);
    webrtc_host::add_to_linker(&mut linker)?;
    add_mailbox_to_linker(&mut linker)?;

    let role = config.role;
    let room = config.room.clone();
    let started = std::time::Instant::now();
    tracing::info!(target: "conformance", test_id, ?role, room, "guest instance starting");
    let instance = Conformance::instantiate_async(&mut store, component, &linker).await?;
    let test_id_owned = test_id.to_string();
    let result = store
        .run_concurrent(async move |accessor: &Accessor<Ctx>| {
            instance
                .conformance_suite_runner()
                .call_run_test(accessor, test_id_owned, config)
                .await
        })
        .await??;
    tracing::info!(
        target: "conformance",
        test_id,
        ?role,
        room,
        elapsed = ?started.elapsed(),
        outcome = match &result {
            TestResult::Pass => "pass",
            TestResult::Fail(_) => "fail",
            TestResult::Skipped(_) => "skipped",
        },
        "guest instance finished"
    );
    Ok(match result {
        TestResult::Pass => TestOutcome::Pass,
        TestResult::Fail(detail) => TestOutcome::Fail(detail),
        TestResult::Skipped(reason) => TestOutcome::Skipped(reason),
    })
}

/// Instantiate the guest once and return the test ids from its `list-tests`
/// export, so an adapter can verify its registered corpus mirrors the corpus
/// the guest actually implements (see
/// [`verify_corpus`](conformance_adapter_common::verify_corpus)).
pub async fn list_guest_tests(engine: &Engine, component: &Component) -> Result<Vec<String>> {
    let mut store = new_store(engine);
    let mut linker: Linker<Ctx> = Linker::new(engine);
    webrtc_host::add_to_linker(&mut linker)?;
    add_mailbox_to_linker(&mut linker)?;
    let instance = Conformance::instantiate_async(&mut store, component, &linker).await?;
    let descriptors = instance
        .conformance_suite_runner()
        .call_list_tests(&mut store)
        .await?;
    Ok(descriptors.into_iter().map(|d| d.id).collect())
}
