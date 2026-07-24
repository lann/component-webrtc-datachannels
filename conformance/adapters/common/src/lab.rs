//! The conformance netns-lab topology and its provisioning.
//!
//! The lab is a small routed network of Linux network namespaces provisioned
//! entirely with `ip`, `nft`, and coturn's `turnserver` (no containers). It
//! gives the conformance suite a realistic, reproducible topology in which two
//! peers sit on separate subnets behind a router, so an ICE handshake exercises
//! a real (non-loopback) network path — and so the router can selectively block
//! the direct path to force server-reflexive (STUN) or relay (TURN) candidates.
//!
//! Topology (all addresses in 10.79.0.0/16, one /30 per link):
//!
//! ```text
//!        cw-off (offerer)            cw-ans (answerer)
//!        10.79.1.2/30                10.79.2.2/30
//!             | veth                      | veth
//!        10.79.1.1                   10.79.2.1
//!        +----------------- cw-rtr (router, ip_forward=1) ----------------+
//!                                    10.79.3.1
//!                                        | veth
//!                                    10.79.3.2/30
//!                                 cw-sig (signaling + coturn)
//! ```
//!
//! The signaling server (`conformance-signalingd`) and the TURN/STUN server
//! (coturn) both run in `cw-sig`, reachable from either peer through the
//! router.
//!
//! [`LabTopology`] centralizes every name, address, port, and credential so the
//! provisioning and the peer placement in the `conformance-netns` executor share
//! a single source of truth. The provisioning methods shell out to `ip`, `nft`,
//! and `turnserver` (via `sudo` when not already root) and are idempotent:
//! `up` tears down any stale lab first, and `down` ignores errors.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use anyhow::{Context as _, Result};

use crate::peer_command::{PeerIce, PeerRole};

// ----- topology ---------------------------------------------------------------

/// The names, addresses, ports, and credentials of the netns lab (see the module
/// docs for the topology diagram). [`Default`] holds the canonical lab values.
#[derive(Debug, Clone)]
pub struct LabTopology {
    /// Offerer / answerer / signaling / router namespace names.
    pub offerer_ns: String,
    pub answerer_ns: String,
    pub signaling_ns: String,
    pub router_ns: String,
    /// Per-link endpoint addresses (the `.2` side of each /30) and their
    /// router-side gateways (the `.1` side).
    pub offerer_addr: String,
    pub offerer_gw: String,
    pub answerer_addr: String,
    pub answerer_gw: String,
    pub signaling_addr: String,
    pub signaling_gw: String,
    /// The /30 subnets of the three links (used by the nftables direct-path
    /// blocking and NAT rules).
    pub offerer_subnet: String,
    pub answerer_subnet: String,
    pub signaling_subnet: String,
    /// Per-peer "public" SNAT addresses used by the NAT scenarios. The router
    /// source-NATs each peer's forwarded traffic to its own public address, so
    /// a peer's server-reflexive candidate (the mapping the STUN server
    /// observes) differs from its private host candidate — which is what makes
    /// a srflx path meaningful. These addresses are not assigned to any
    /// interface; the router owns them implicitly through connection tracking.
    pub offerer_public: String,
    pub answerer_public: String,
    /// Signaling HTTP port in the signaling namespace.
    pub signaling_port: u16,
    /// TURN/STUN listening port and relay port range in the signaling
    /// namespace.
    pub turn_port: u16,
    pub turn_min_port: u16,
    pub turn_max_port: u16,
    /// TURN long-term credentials and realm (shared by coturn and the peers).
    pub turn_user: String,
    pub turn_pass: String,
    pub turn_realm: String,
    /// Where coturn's generated config, pidfile, and log live.
    pub run_dir: PathBuf,
}

impl Default for LabTopology {
    fn default() -> Self {
        Self {
            offerer_ns: "cw-off".to_string(),
            answerer_ns: "cw-ans".to_string(),
            signaling_ns: "cw-sig".to_string(),
            router_ns: "cw-rtr".to_string(),
            offerer_addr: "10.79.1.2".to_string(),
            offerer_gw: "10.79.1.1".to_string(),
            answerer_addr: "10.79.2.2".to_string(),
            answerer_gw: "10.79.2.1".to_string(),
            signaling_addr: "10.79.3.2".to_string(),
            signaling_gw: "10.79.3.1".to_string(),
            offerer_subnet: "10.79.1.0/30".to_string(),
            answerer_subnet: "10.79.2.0/30".to_string(),
            signaling_subnet: "10.79.3.0/30".to_string(),
            offerer_public: "10.79.11.1".to_string(),
            answerer_public: "10.79.12.1".to_string(),
            signaling_port: 8080,
            turn_port: 3478,
            turn_min_port: 49160,
            turn_max_port: 49200,
            turn_user: "conf".to_string(),
            turn_pass: "conf".to_string(),
            turn_realm: "conformance".to_string(),
            run_dir: PathBuf::from("/tmp/conformance-netns"),
        }
    }
}

impl LabTopology {
    /// The UDP address a peer role binds and gathers host candidates from.
    pub fn bind_addr(&self, role: PeerRole) -> &str {
        match role {
            PeerRole::Offerer => &self.offerer_addr,
            PeerRole::Answerer => &self.answerer_addr,
        }
    }

    /// The namespace a peer role's process is placed in.
    pub fn peer_ns(&self, role: PeerRole) -> &str {
        match role {
            PeerRole::Offerer => &self.offerer_ns,
            PeerRole::Answerer => &self.answerer_ns,
        }
    }

    /// The STUN/TURN server `host:port` in the signaling namespace.
    pub fn turn_server_addr(&self) -> String {
        format!("{}:{}", self.signaling_addr, self.turn_port)
    }
}

// ----- scenarios ---------------------------------------------------------------

/// An netns-lab scenario: the candidate path a run is set up to exercise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Scenario {
    /// Direct host-candidate connectivity over the router (no STUN/TURN server).
    Lan,
    /// Server-reflexive candidates via a STUN server behind a static
    /// one-to-one (full-cone) NAT; the direct path is blocked, and the NAT
    /// delivers traffic addressed to the srflx candidates, so the srflx path
    /// connects (see the NAT matrix in `conformance/README.md`).
    StunSrflx,
    /// Relayed candidates via a TURN server; the direct path is blocked and the
    /// peers are relay-only.
    TurnRelay,
    /// A symmetric NAT with a STUN/TURN server available but no relay-only
    /// policy: the direct path is blocked and the symmetric NAT makes the srflx
    /// candidates unusable, so ICE must fall back to a TURN relay.
    NatSymmetric,
}

impl Scenario {
    /// The scenario id used on the command line, in result documents, and as the
    /// matrix environment column.
    pub fn as_str(self) -> &'static str {
        match self {
            Scenario::Lan => "lan",
            Scenario::StunSrflx => "stun-srflx",
            Scenario::TurnRelay => "turn-relay",
            Scenario::NatSymmetric => "nat-symmetric",
        }
    }

    /// Parse a scenario id (`lan`, `stun-srflx`, `turn-relay`, `nat-symmetric`).
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "lan" => Ok(Scenario::Lan),
            "stun-srflx" => Ok(Scenario::StunSrflx),
            "turn-relay" => Ok(Scenario::TurnRelay),
            "nat-symmetric" => Ok(Scenario::NatSymmetric),
            other => anyhow::bail!(
                "unknown scenario {other:?} (lan|stun-srflx|turn-relay|nat-symmetric)"
            ),
        }
    }

    /// Whether the scenario needs the coturn STUN/TURN server; only `lan`
    /// connects with plain host candidates.
    pub fn needs_coturn(self) -> bool {
        !matches!(self, Scenario::Lan)
    }

    /// The ICE parameters a peer of this scenario runs with: it always binds
    /// its role's interface address, and additionally points at the STUN or
    /// TURN server (and forces relay-only) for the server-mediated scenarios.
    pub fn ice(self, topology: &LabTopology) -> PeerIce {
        let server = topology.turn_server_addr();
        match self {
            Scenario::Lan => PeerIce::default(),
            Scenario::StunSrflx => PeerIce {
                server_url: Some(format!("stun:{server}")),
                ..Default::default()
            },
            Scenario::TurnRelay => PeerIce {
                server_url: Some(format!("turn:{server}?transport=udp")),
                username: topology.turn_user.clone(),
                credential: topology.turn_pass.clone(),
                relay_only: true,
            },
            // A TURN server (which also serves STUN, so the peer gathers both
            // srflx and relay candidates) with no relay-only policy: under a
            // symmetric NAT the srflx pair fails and ICE falls back to relay.
            Scenario::NatSymmetric => PeerIce {
                server_url: Some(format!("turn:{server}?transport=udp")),
                username: topology.turn_user.clone(),
                credential: topology.turn_pass.clone(),
                relay_only: false,
            },
        }
    }
}

// ----- privileged command helpers ----------------------------------------------

/// Log a provisioning progress line to stderr (keeps stdout clean for
/// machine-readable output).
fn lab_log(msg: impl std::fmt::Display) {
    eprintln!("cw: {msg}");
}

/// Whether the process is already root (in which case privileged commands run
/// directly rather than through `sudo`).
fn is_root() -> bool {
    static ROOT: OnceLock<bool> = OnceLock::new();
    *ROOT.get_or_init(|| {
        Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
            .unwrap_or(false)
    })
}

/// A privileged command: `program args…` when root, `sudo program args…`
/// otherwise.
fn priv_command(program: &str, args: &[&str]) -> Command {
    let mut command = if is_root() {
        Command::new(program)
    } else {
        let mut c = Command::new("sudo");
        c.arg(program);
        c
    };
    command.args(args);
    command
}

/// A privileged command run inside namespace `ns` (`ip netns exec <ns> …`).
fn ns_command(ns: &str, program: &str, args: &[&str]) -> Command {
    let mut command = priv_command("ip", &["netns", "exec", ns, program]);
    command.args(args);
    command
}

/// Run a command to completion, failing on spawn errors and nonzero exits.
fn run(mut command: Command, what: &str) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("spawning {what}"))?;
    if !status.success() {
        anyhow::bail!("{what} exited with {status}");
    }
    Ok(())
}

/// Run a command to completion, ignoring spawn errors and nonzero exits (used
/// by the idempotent teardown paths).
fn run_ignore(mut command: Command) {
    let _ = command.stdout(Stdio::null()).stderr(Stdio::null()).status();
}

// ----- provisioning -------------------------------------------------------------

impl LabTopology {
    /// Provision the namespace topology: the four namespaces, the three veth
    /// links with /30 addressing and default routes, and forwarding in the
    /// router. Idempotent: tears down any stale lab first, so a
    /// half-provisioned lab never wedges a run.
    pub fn netns_up(&self) -> Result<()> {
        self.netns_down();

        lab_log("creating namespaces");
        for ns in [
            &self.router_ns,
            &self.offerer_ns,
            &self.answerer_ns,
            &self.signaling_ns,
        ] {
            run(
                priv_command("ip", &["netns", "add", ns]),
                &format!("ip netns add {ns}"),
            )?;
        }

        run(
            ns_command(&self.router_ns, "ip", &["link", "set", "lo", "up"]),
            "router lo up",
        )?;
        // The router forwards between the three /30 links.
        run(
            ns_command(
                &self.router_ns,
                "sysctl",
                &["-q", "-w", "net.ipv4.ip_forward=1"],
            ),
            "enabling ip_forward in the router",
        )?;

        lab_log("wiring links");
        self.link(
            &self.offerer_ns,
            "veth-off",
            "veth-roff",
            &self.offerer_addr,
            &self.offerer_gw,
        )?;
        self.link(
            &self.answerer_ns,
            "veth-ans",
            "veth-rans",
            &self.answerer_addr,
            &self.answerer_gw,
        )?;
        self.link(
            &self.signaling_ns,
            "veth-sig",
            "veth-rsig",
            &self.signaling_addr,
            &self.signaling_gw,
        )?;

        lab_log("lab ready");
        Ok(())
    }

    /// Remove the lab namespaces. Deleting a namespace removes the veth ends
    /// inside it (the peer ends go with them); errors are ignored so teardown
    /// is idempotent.
    pub fn netns_down(&self) {
        for ns in [
            &self.offerer_ns,
            &self.answerer_ns,
            &self.signaling_ns,
            &self.router_ns,
        ] {
            run_ignore(priv_command("ip", &["netns", "del", ns]));
        }
    }

    /// Create one router<->endpoint link: a veth pair with the router end in the
    /// router namespace and the endpoint end in `ns`, addressed and routed so
    /// the endpoint reaches the whole lab through the router.
    fn link(
        &self,
        ns: &str,
        veth_ep: &str,
        veth_rtr: &str,
        ep_addr: &str,
        gw_addr: &str,
    ) -> Result<()> {
        run(
            priv_command(
                "ip",
                &[
                    "link", "add", veth_ep, "type", "veth", "peer", "name", veth_rtr,
                ],
            ),
            &format!("creating veth pair {veth_ep}/{veth_rtr}"),
        )?;
        run(
            priv_command("ip", &["link", "set", veth_ep, "netns", ns]),
            &format!("moving {veth_ep} into {ns}"),
        )?;
        run(
            priv_command("ip", &["link", "set", veth_rtr, "netns", &self.router_ns]),
            &format!("moving {veth_rtr} into {}", self.router_ns),
        )?;

        // Endpoint side.
        let ep_cidr = format!("{ep_addr}/30");
        run(
            ns_command(ns, "ip", &["addr", "add", &ep_cidr, "dev", veth_ep]),
            &format!("addressing {veth_ep}"),
        )?;
        run(
            ns_command(ns, "ip", &["link", "set", veth_ep, "up"]),
            &format!("bringing {veth_ep} up"),
        )?;
        run(
            ns_command(ns, "ip", &["link", "set", "lo", "up"]),
            &format!("bringing lo up in {ns}"),
        )?;
        run(
            ns_command(ns, "ip", &["route", "add", "default", "via", gw_addr]),
            &format!("default route in {ns}"),
        )?;

        // Router side.
        let gw_cidr = format!("{gw_addr}/30");
        run(
            ns_command(
                &self.router_ns,
                "ip",
                &["addr", "add", &gw_cidr, "dev", veth_rtr],
            ),
            &format!("addressing {veth_rtr}"),
        )?;
        run(
            ns_command(&self.router_ns, "ip", &["link", "set", veth_rtr, "up"]),
            &format!("bringing {veth_rtr} up"),
        )?;
        Ok(())
    }

    /// Apply the router's nftables policy that shapes which ICE candidate paths
    /// can carry data for `scenario`.
    ///
    /// All of `stun-srflx`, `turn-relay`, and `nat-symmetric` drop the direct
    /// path between the two peer subnets while leaving each peer's path to the
    /// signaling/coturn subnet open, so a successful connection must have
    /// traversed the server (server-reflexive or relayed candidates) rather
    /// than a direct host-candidate pair.
    ///
    /// The two NAT scenarios additionally source-NAT each peer's forwarded
    /// traffic to its own "public" address, so the address the STUN server
    /// observes (the srflx candidate) differs from the peer's private host
    /// address. The mapping style decides whether srflx is usable:
    ///
    /// - `stun-srflx` — `snat … persistent` plus a static one-to-one DNAT of
    ///   each public address back to its peer (together: a full-cone,
    ///   one-to-one NAT), so packets addressed to a peer's srflx candidate are
    ///   delivered and the srflx path connects. The DNAT is what makes inbound
    ///   delivery possible at all on this topology: both peers' mappings live
    ///   on the *same* router, and conntrack cannot hairpin a packet from one
    ///   peer's fresh flow into the other peer's reverse mapping (a packet
    ///   matches exactly one conntrack entry, and its source is still private
    ///   at lookup time), so without the DNAT the "public" addresses are
    ///   unroutable phantoms. Distinct per-peer NAT devices would not need
    ///   this, but a single-router lab does.
    /// - `nat-symmetric` — `snat … random` picks a fresh source port per
    ///   destination (an endpoint-dependent, symmetric NAT) and no inbound
    ///   mapping exists, so the mapping the STUN server saw is useless to the
    ///   peer and ICE must fall back to a TURN relay.
    pub fn nftables_apply(&self, scenario: Scenario) -> Result<()> {
        self.nftables_clear();
        match scenario {
            Scenario::Lan => Ok(()),
            Scenario::TurnRelay => self.nft_load(&self.nft_table(None, false)),
            Scenario::StunSrflx => self.nft_load(&self.nft_table(Some("persistent"), true)),
            Scenario::NatSymmetric => self.nft_load(&self.nft_table(Some("random"), false)),
        }
    }

    /// Remove the router's nftables policy, ignoring errors (idempotent).
    pub fn nftables_clear(&self) {
        run_ignore(ns_command(
            &self.router_ns,
            "nft",
            &["delete", "table", "inet", NFT_TABLE],
        ));
    }

    /// The `cw_ice` nftables table body: a forward chain that drops traffic
    /// directly between the offerer and answerer subnets (both directions),
    /// forcing a server-mediated path, plus — when `snat_mode` is set — a
    /// postrouting source-NAT (and, when `one_to_one` is set, a prerouting
    /// destination-NAT back from each public address) rewriting each peer's
    /// forwarded traffic to its
    /// own public address in that mapping style.
    fn nft_table(&self, snat_mode: Option<&str>, one_to_one: bool) -> String {
        // The forward filter sees packets *after* prerouting DNAT, so traffic
        // that arrived addressed to a public (NAT) address has already been
        // rewritten back to a private peer address and would match the
        // direct-path drop. Exempt it: `ct status dnat` is only set on
        // translated flows, never on packets sent straight to a peer's
        // private address, so the direct path stays blocked.
        let dnat_exempt = if one_to_one {
            "\x20       ct status dnat accept\n"
        } else {
            ""
        };
        let mut ruleset = format!(
            "table inet {NFT_TABLE} {{\n\
             \x20   chain forward {{\n\
             \x20       type filter hook forward priority 0; policy accept;\n\
             {dnat_exempt}\
             \x20       ip saddr {off} ip daddr {ans} drop\n\
             \x20       ip saddr {ans} ip daddr {off} drop\n\
             \x20   }}\n",
            off = self.offerer_subnet,
            ans = self.answerer_subnet,
        );
        if let Some(mode) = snat_mode {
            ruleset.push_str(&format!(
                "\x20   chain postrouting {{\n\
                 \x20       type nat hook postrouting priority srcnat; policy accept;\n\
                 \x20       ip saddr {off} snat ip to {off_pub} {mode}\n\
                 \x20       ip saddr {ans} snat ip to {ans_pub} {mode}\n\
                 \x20   }}\n",
                off = self.offerer_subnet,
                ans = self.answerer_subnet,
                off_pub = self.offerer_public,
                ans_pub = self.answerer_public,
            ));
        }
        if one_to_one {
            ruleset.push_str(&format!(
                "\x20   chain prerouting {{\n\
                 \x20       type nat hook prerouting priority dstnat; policy accept;\n\
                 \x20       ip daddr {off_pub} dnat ip to {off_addr}\n\
                 \x20       ip daddr {ans_pub} dnat ip to {ans_addr}\n\
                 \x20   }}\n",
                off_pub = self.offerer_public,
                ans_pub = self.answerer_public,
                off_addr = self.offerer_addr,
                ans_addr = self.answerer_addr,
            ));
        }
        ruleset.push_str("}\n");
        ruleset
    }

    /// Feed `ruleset` to `nft -f -` in the router namespace.
    fn nft_load(&self, ruleset: &str) -> Result<()> {
        let mut command = ns_command(&self.router_ns, "nft", &["-f", "-"]);
        let mut child = command
            .stdin(Stdio::piped())
            .spawn()
            .context("spawning nft in the router namespace")?;
        child
            .stdin
            .take()
            .context("nft stdin unavailable")?
            .write_all(ruleset.as_bytes())
            .context("writing nftables ruleset")?;
        let status = child.wait().context("waiting for nft")?;
        if !status.success() {
            anyhow::bail!("nft -f - exited with {status}");
        }
        Ok(())
    }

    /// Generate the coturn config under the run dir and start `turnserver`
    /// (daemonized) in the signaling namespace, stopping any previous instance
    /// first. Requires `turnserver` on `PATH` (installed by
    /// `scripts/setup.sh`).
    pub fn coturn_up(&self) -> Result<()> {
        self.coturn_down();
        std::fs::create_dir_all(&self.run_dir)
            .with_context(|| format!("creating {}", self.run_dir.display()))?;
        // A minimal long-term-credential TURN/STUN server bound to the
        // signaling namespace's address. The relay port range is small (the lab
        // runs a handful of allocations at a time) and no TLS is configured
        // (the lab is a closed, ephemeral network).
        let config = format!(
            "listening-ip={addr}\n\
             listening-port={port}\n\
             relay-ip={addr}\n\
             min-port={min}\n\
             max-port={max}\n\
             fingerprint\n\
             lt-cred-mech\n\
             realm={realm}\n\
             user={user}:{pass}\n\
             no-tls\n\
             no-dtls\n\
             no-cli\n\
             pidfile={pid}\n\
             log-file={log}\n\
             simple-log\n",
            addr = self.signaling_addr,
            port = self.turn_port,
            min = self.turn_min_port,
            max = self.turn_max_port,
            realm = self.turn_realm,
            user = self.turn_user,
            pass = self.turn_pass,
            pid = self.turn_pidfile().display(),
            log = self.run_dir.join("turnserver.log").display(),
        );
        let conf = self.turn_conf();
        std::fs::write(&conf, config).with_context(|| format!("writing {}", conf.display()))?;

        lab_log(format!(
            "starting coturn in {} on {}:{}",
            self.signaling_ns, self.signaling_addr, self.turn_port
        ));
        // Run detached inside the signaling namespace; coturn daemonizes with -o.
        run(
            ns_command(
                &self.signaling_ns,
                "turnserver",
                &["-c", &conf.to_string_lossy(), "-o"],
            ),
            "starting turnserver (install coturn; see scripts/setup.sh)",
        )?;
        // Give it a moment to bind before the orchestrator points peers at it.
        std::thread::sleep(std::time::Duration::from_secs(1));
        Ok(())
    }

    /// Stop the coturn server, ignoring errors (idempotent).
    pub fn coturn_down(&self) {
        if let Ok(pid) = std::fs::read_to_string(self.turn_pidfile()) {
            let pid = pid.trim();
            if !pid.is_empty() {
                run_ignore(priv_command("kill", &[pid]));
            }
        }
        // Belt and suspenders: kill any turnserver bound to our config.
        run_ignore(priv_command(
            "pkill",
            &[
                "-f",
                &format!("turnserver -c {}", self.turn_conf().display()),
            ],
        ));
        run_ignore(priv_command(
            "rm",
            &["-f", &self.turn_pidfile().to_string_lossy()],
        ));
    }

    /// Bring the whole lab up for `scenario`: the namespace topology, coturn
    /// for the server-mediated scenarios, then the router policy.
    pub fn scenario_up(&self, scenario: Scenario) -> Result<()> {
        self.netns_up()?;
        if scenario.needs_coturn() {
            self.coturn_up()?;
        }
        self.nftables_apply(scenario)?;
        lab_log(format!("scenario '{}' ready", scenario.as_str()));
        Ok(())
    }

    /// Tear the whole lab down, ignoring errors: the router policy, coturn,
    /// then the namespaces.
    pub fn scenario_down(&self) {
        self.nftables_clear();
        self.coturn_down();
        self.netns_down();
        lab_log("lab torn down");
    }

    fn turn_conf(&self) -> PathBuf {
        self.run_dir.join("turnserver.conf")
    }

    fn turn_pidfile(&self) -> PathBuf {
        self.run_dir.join("turnserver.pid")
    }
}

/// The nftables table owning the lab's router policy.
const NFT_TABLE: &str = "cw_ice";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_ids_round_trip() {
        for s in [
            Scenario::Lan,
            Scenario::StunSrflx,
            Scenario::TurnRelay,
            Scenario::NatSymmetric,
        ] {
            assert_eq!(Scenario::parse(s.as_str()).unwrap(), s);
        }
        assert!(Scenario::parse("nope").is_err());
    }

    #[test]
    fn lan_binds_role_address_without_servers() {
        let topology = LabTopology::default();
        assert_eq!(topology.bind_addr(PeerRole::Offerer), "10.79.1.2");
        assert_eq!(topology.bind_addr(PeerRole::Answerer), "10.79.2.2");
        let ice = Scenario::Lan.ice(&topology);
        assert!(ice.server_url.is_none());
        assert!(!ice.relay_only);
        assert!(!Scenario::Lan.needs_coturn());
    }

    #[test]
    fn stun_srflx_adds_stun_server_only() {
        let ice = Scenario::StunSrflx.ice(&LabTopology::default());
        assert_eq!(ice.server_url.as_deref(), Some("stun:10.79.3.2:3478"));
        assert!(ice.username.is_empty());
        assert!(!ice.relay_only);
        assert!(Scenario::StunSrflx.needs_coturn());
    }

    #[test]
    fn turn_relay_is_relay_only_with_credentials() {
        let ice = Scenario::TurnRelay.ice(&LabTopology::default());
        assert!(ice.relay_only);
        assert_eq!(
            ice.server_url.as_deref(),
            Some("turn:10.79.3.2:3478?transport=udp")
        );
        assert_eq!(ice.username, "conf");
        assert_eq!(ice.credential, "conf");
    }

    #[test]
    fn nat_symmetric_offers_turn_without_relay_only() {
        // The symmetric-NAT scenario configures a TURN server (which also serves
        // STUN) but leaves relay-only off, so the peer gathers srflx *and* relay
        // candidates and falls back to relay when srflx fails under symmetric NAT.
        let ice = Scenario::NatSymmetric.ice(&LabTopology::default());
        assert!(!ice.relay_only);
        assert_eq!(
            ice.server_url.as_deref(),
            Some("turn:10.79.3.2:3478?transport=udp")
        );
        assert_eq!(ice.username, "conf");
        assert_eq!(ice.credential, "conf");
    }

    #[test]
    fn nft_rulesets_match_scenarios() {
        let topology = LabTopology::default();
        let block = topology.nft_table(None, false);
        assert!(block.contains("ip saddr 10.79.1.0/30 ip daddr 10.79.2.0/30 drop"));
        assert!(!block.contains("snat"));
        let cone = topology.nft_table(Some("persistent"), true);
        assert!(cone.contains("snat ip to 10.79.11.1 persistent"));
        assert!(cone.contains("ct status dnat accept"));
        assert!(cone.contains("ip daddr 10.79.11.1 dnat ip to 10.79.1.2"));
        assert!(cone.contains("ip daddr 10.79.12.1 dnat ip to 10.79.2.2"));
        let symmetric = topology.nft_table(Some("random"), false);
        assert!(symmetric.contains("snat ip to 10.79.12.1 random"));
        assert!(!symmetric.contains("dnat ip to"));
        assert!(!symmetric.contains("ct status dnat"));
    }
}
