//! The two small types the netns lab topology speaks (inlined from the
//! incumbent adapter's peer-command layer at the migration).

/// Which half of a pair a peer drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerRole {
    Offerer,
    Answerer,
}

/// The ICE configuration a lab scenario hands a peer.
#[derive(Clone, Debug, Default)]
pub struct PeerIce {
    /// STUN or TURN server URL, when the scenario is server-mediated.
    pub server_url: Option<String>,
    pub username: String,
    pub credential: String,
    /// Restrict ICE to relay candidates (TURN scenarios).
    pub relay_only: bool,
}
