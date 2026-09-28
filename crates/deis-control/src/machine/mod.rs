//! The channel this control plane's own processes talk on.
//!
//! Everything a node or a proxy calls belongs here, and nothing a browser or
//! an operator's token reaches does. The two used to share a socket, which
//! meant the answer carrying every application's decrypted secrets was
//! reachable wherever the dashboard was, guarded only by a bearer secret every
//! machine in the fleet held a copy of.
//!
//! What replaces that secret is a certificate: each process proves one SPIFFE
//! name, and the name is established once per connection, during the TLS
//! handshake, by [`identity_acceptor`].

pub mod accept;
pub(crate) mod issuance;
pub mod listen;
pub mod service;

pub use accept::{PeerIdentity, identity_acceptor};
pub use service::MachineChannelService;
