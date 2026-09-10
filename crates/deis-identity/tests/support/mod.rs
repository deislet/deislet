//! Re-exports the test certificate authority the handshake tests mint from.
//!
//! The CA used to live here, reachable by nothing outside this crate's own
//! integration tests. It is now `deis-identity-test-pki`, a crate of its own,
//! so any harness in the workspace that has to hold a real mutual TLS
//! handshake can reach it too. This module stays so `tests/handshake.rs` does
//! not have to change its imports along with the move.

pub use deis_identity_test_pki::{TestCa, write_files};
