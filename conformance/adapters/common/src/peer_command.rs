//! The per-target peer command templates shared by the environment executors.
//!
//! An environment executor (the Shadow lab's `conformance-shadow`, the netns
//! netns lab's `conformance-netns`) never runs a guest itself; it launches
//! **peers** — one process per role of a test — that all honour the same
//! single-peer contract (`--test`/`--role`/`--server`/`--room`/…, one
//! single-line JSON `test-result` on stdout). This module owns how each
//! target's peer process is invoked, so supporting a peer in a new environment
//! means teaching the executor a placement, not the peer the environment:
//!
//! - [`PeerKind`] selects the target whose peer runs,
//! - [`PeerCommand`] resolves that kind's binaries/components to absolute
//!   paths (executors hand the argv to processes that run with a different
//!   cwd) and builds one peer's argv from a [`PeerRun`], and
//! - [`PeerRole`] / [`PeerIce`] carry the per-role placement and ICE
//!   parameters as plain data.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

/// A peer's role in a two-peer test. Environment executors always run one
/// offerer and one answerer (the in-process `both` role never appears in a
/// lab).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRole {
    Offerer,
    Answerer,
}

impl PeerRole {
    /// The role string passed on the peer command line.
    pub fn as_str(self) -> &'static str {
        match self {
            PeerRole::Offerer => "offerer",
            PeerRole::Answerer => "answerer",
        }
    }
}

/// The ICE parameters one peer runs with, as plain data: an optional STUN/TURN
/// server (with long-term credentials) and the relay-only policy. The default
/// is host-candidates only.
#[derive(Debug, Clone, Default)]
pub struct PeerIce {
    /// STUN/TURN server URL, e.g. `stun:10.79.3.2:3478` or
    /// `turn:10.79.3.2:3478?transport=udp`. `None` gathers host candidates
    /// only.
    pub server_url: Option<String>,
    /// TURN long-term-credential username (ignored for STUN-only servers).
    pub username: String,
    /// TURN long-term-credential secret (ignored for STUN-only servers).
    pub credential: String,
    /// Restrict the peer to TURN relay candidates (the `relay` ICE transport
    /// policy). Requires `server_url` to name a TURN server.
    pub relay_only: bool,
}

impl PeerIce {
    /// Whether these parameters ask for anything beyond plain host candidates.
    fn is_default(&self) -> bool {
        self.server_url.is_none() && !self.relay_only
    }
}

/// How a peer host's command line is built (which target's peer runs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PeerKind {
    /// The native `conformance-peer` binary (the wasmtime host + webrtc-rs).
    Wasmtime,
    /// The composed wasip3 conformance component under `wasmtime run`.
    Wasip3Guest,
    /// The non-wasm reference peer: plain Node driving `RTCPeerConnection`
    /// (libwebrtc via `@roamhq/wrtc`) directly.
    Reference,
}

impl PeerKind {
    /// The target id conventionally recorded in result documents for this kind.
    pub fn default_target(self) -> &'static str {
        match self {
            PeerKind::Wasmtime => "wasmtime",
            PeerKind::Wasip3Guest => "wasip3-guest",
            PeerKind::Reference => "reference",
        }
    }
}

/// The parameters of one peer invocation: one role of one test, placed at a
/// bind address with its ICE parameters.
#[derive(Debug, Clone)]
pub struct PeerRun<'a> {
    pub test_id: &'a str,
    pub role: &'a str,
    pub signaling_url: &'a str,
    pub room: &'a str,
    pub count: u32,
    pub size: u32,
    /// UDP interface address the peer binds and gathers its host candidate
    /// from.
    pub bind_addr: &'a str,
    /// STUN/TURN parameters; `None` behaves like [`PeerIce::default`].
    pub ice: Option<&'a PeerIce>,
    /// Disable multicast-DNS candidate gathering (`wasmtime` kind only; the
    /// sans-I/O wasip3 stack has no mDNS).
    pub disable_mdns: bool,
}

/// The resolved per-role peer command template: how one peer process is
/// invoked, with binaries and components resolved to absolute paths.
pub enum PeerCommand {
    /// `conformance-peer --guest … --bind-addr <ip> [--disable-mdns] …`
    Wasmtime { peer_bin: PathBuf, guest: PathBuf },
    /// `wasmtime run … --env WEBRTC_UDP_BIND_ADDR=<ip> <component> …`
    Wasip3Guest {
        wasmtime_bin: PathBuf,
        component: PathBuf,
    },
    /// `node <peer.mjs> …` — libwebrtc gathers candidates from the host's own
    /// interfaces, so the placement's bind address is not passed (each lab
    /// host has exactly one routable address).
    Reference { node_bin: PathBuf, script: PathBuf },
}

impl PeerCommand {
    /// Resolve `kind`'s binaries and components to absolute paths. `guest` and
    /// `peer_bin` serve the `wasmtime` kind; `wasmtime_bin` (a path or a bare
    /// name looked up on `PATH`) and `component` serve the `wasip3-guest`
    /// kind; `node_bin` (path or bare name) and `reference_peer` serve the
    /// `reference` kind.
    pub fn resolve(
        kind: PeerKind,
        peer_bin: &Path,
        guest: &Path,
        wasmtime_bin: &str,
        component: &Path,
        node_bin: &str,
        reference_peer: &Path,
    ) -> Result<Self> {
        Ok(match kind {
            PeerKind::Wasmtime => PeerCommand::Wasmtime {
                peer_bin: absolute(peer_bin)?,
                guest: absolute(guest)?,
            },
            PeerKind::Wasip3Guest => PeerCommand::Wasip3Guest {
                wasmtime_bin: resolve_bin(wasmtime_bin)?,
                component: absolute(component)?,
            },
            PeerKind::Reference => PeerCommand::Reference {
                node_bin: resolve_bin(node_bin)?,
                script: absolute(reference_peer)?,
            },
        })
    }

    /// The full argv (element 0 is the executable) for one peer process, as
    /// plain unquoted strings. Callers embedding the argv in a structured
    /// format (e.g. the Shadow YAML config) apply their own quoting.
    ///
    /// Fails for a `wasip3-guest` peer with non-default ICE parameters: the
    /// in-guest sans-I/O stack supports no STUN/TURN, so only the `lan`
    /// scenario (host candidates) fits that kind.
    pub fn argv(&self, run: &PeerRun<'_>) -> Result<Vec<String>> {
        let shared_peer_args = [
            "--test".to_string(),
            run.test_id.to_string(),
            "--role".to_string(),
            run.role.to_string(),
            "--server".to_string(),
            run.signaling_url.to_string(),
            "--room".to_string(),
            run.room.to_string(),
            "--message-count".to_string(),
            run.count.to_string(),
            "--message-size".to_string(),
            run.size.to_string(),
        ];
        Ok(match self {
            PeerCommand::Wasmtime { peer_bin, guest } => {
                let mut args = vec![
                    peer_bin.to_string_lossy().into_owned(),
                    "--guest".to_string(),
                    guest.to_string_lossy().into_owned(),
                ];
                args.extend(shared_peer_args);
                args.extend(["--bind-addr".to_string(), run.bind_addr.to_string()]);
                if let Some(ice) = run.ice {
                    if let Some(url) = &ice.server_url {
                        args.extend([
                            "--ice-server-url".to_string(),
                            url.clone(),
                            "--ice-username".to_string(),
                            ice.username.clone(),
                            "--ice-credential".to_string(),
                            ice.credential.clone(),
                        ]);
                    }
                    if ice.relay_only {
                        args.push("--relay-only".to_string());
                    }
                }
                if run.disable_mdns {
                    args.push("--disable-mdns".to_string());
                }
                args
            }
            PeerCommand::Wasip3Guest {
                wasmtime_bin,
                component,
            } => {
                if run.ice.is_some_and(|ice| !ice.is_default()) {
                    anyhow::bail!(
                        "the wasip3-guest peer's in-guest sans-I/O stack supports no \
                         STUN/TURN servers; only the `lan` scenario (host candidates) \
                         is supported for this peer kind"
                    );
                }
                // Mirror the loopback wasip3 adapter's `wasmtime run`
                // invocation, plus the provider's bind-address environment
                // variable pointing it at this peer's address.
                let mut args = vec![
                    wasmtime_bin.to_string_lossy().into_owned(),
                    "run".to_string(),
                    "-W".to_string(),
                    "component-model-async=y".to_string(),
                    "-S".to_string(),
                    "cli".to_string(),
                    "-S".to_string(),
                    "p3".to_string(),
                    "-S".to_string(),
                    "http".to_string(),
                    "-S".to_string(),
                    "inherit-network".to_string(),
                    "--env".to_string(),
                    format!("WEBRTC_UDP_BIND_ADDR={}", run.bind_addr),
                    component.to_string_lossy().into_owned(),
                ];
                args.extend(shared_peer_args);
                args
            }
            PeerCommand::Reference { node_bin, script } => {
                let mut args = vec![
                    node_bin.to_string_lossy().into_owned(),
                    script.to_string_lossy().into_owned(),
                ];
                args.extend(shared_peer_args);
                if let Some(ice) = run.ice {
                    if let Some(url) = &ice.server_url {
                        args.extend([
                            "--ice-server-url".to_string(),
                            url.clone(),
                            "--ice-username".to_string(),
                            ice.username.clone(),
                            "--ice-credential".to_string(),
                            ice.credential.clone(),
                        ]);
                    }
                    if ice.relay_only {
                        args.push("--relay-only".to_string());
                    }
                }
                args
            }
        })
    }
}

/// Resolve `path` to an absolute path (canonicalizing when it exists), so it
/// survives being handed to a process that runs with a different cwd.
pub fn absolute(path: &Path) -> Result<PathBuf> {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Ok(canonical);
    }
    let cwd = std::env::current_dir().context("getting current directory")?;
    Ok(cwd.join(path))
}

/// Resolve a binary named by path or bare name: a bare name (no path
/// separator) is searched for on `PATH`; anything else is made absolute like
/// [`absolute`].
pub fn resolve_bin(bin: &str) -> Result<PathBuf> {
    if !bin.contains(std::path::MAIN_SEPARATOR) {
        let path = std::env::var_os("PATH").context("reading PATH")?;
        return std::env::split_paths(&path)
            .map(|dir| dir.join(bin))
            .find(|candidate| candidate.is_file())
            .with_context(|| format!("{bin} not found on PATH"));
    }
    absolute(Path::new(bin))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<'a>(ice: Option<&'a PeerIce>) -> PeerRun<'a> {
        PeerRun {
            test_id: "ordering",
            role: "offerer",
            signaling_url: "http://10.79.3.2:8080",
            room: "r",
            count: 16,
            size: 512,
            bind_addr: "10.79.1.2",
            ice,
            disable_mdns: false,
        }
    }

    #[test]
    fn wasmtime_argv_includes_ice_flags() {
        let command = PeerCommand::Wasmtime {
            peer_bin: PathBuf::from("/bin/conformance-peer"),
            guest: PathBuf::from("/g.wasm"),
        };
        let ice = PeerIce {
            server_url: Some("turn:10.79.3.2:3478?transport=udp".to_string()),
            username: "conf".to_string(),
            credential: "conf".to_string(),
            relay_only: true,
        };
        let argv = command.argv(&run(Some(&ice))).unwrap();
        assert_eq!(argv[0], "/bin/conformance-peer");
        assert!(argv.contains(&"--ice-server-url".to_string()));
        assert!(argv.contains(&"--relay-only".to_string()));
        assert!(!argv.contains(&"--disable-mdns".to_string()));
    }

    #[test]
    fn wasip3_guest_rejects_ice_servers() {
        let command = PeerCommand::Wasip3Guest {
            wasmtime_bin: PathBuf::from("/bin/wasmtime"),
            component: PathBuf::from("/c.wasm"),
        };
        let ice = PeerIce {
            server_url: Some("stun:10.79.3.2:3478".to_string()),
            ..Default::default()
        };
        assert!(command.argv(&run(Some(&ice))).is_err());
        let argv = command.argv(&run(Some(&PeerIce::default()))).unwrap();
        assert!(argv.contains(&"--env".to_string()));
        assert!(argv.contains(&"WEBRTC_UDP_BIND_ADDR=10.79.1.2".to_string()));
    }

    #[test]
    fn reference_argv_omits_bind_addr_and_maps_ice() {
        let command = PeerCommand::Reference {
            node_bin: PathBuf::from("/bin/node"),
            script: PathBuf::from("/peer.mjs"),
        };
        let argv = command.argv(&run(None)).unwrap();
        assert_eq!(argv[0], "/bin/node");
        assert_eq!(argv[1], "/peer.mjs");
        assert!(!argv.contains(&"--bind-addr".to_string()));
        assert!(!argv.contains(&"--ice-server-url".to_string()));

        let ice = PeerIce {
            server_url: Some("turn:10.79.3.2:3478?transport=udp".to_string()),
            username: "conf".to_string(),
            credential: "conf".to_string(),
            relay_only: true,
        };
        let argv = command.argv(&run(Some(&ice))).unwrap();
        assert!(argv.contains(&"--ice-server-url".to_string()));
        assert!(argv.contains(&"--relay-only".to_string()));
    }
}
