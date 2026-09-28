//! A stand-in for the control plane's machine channel, for the tests of
//! services that verify capabilities.
//!
//! A verifier refuses every capability until it has adopted a signed
//! revocation list, so a test that starts deis-store, deis-queue or
//! deis-telemetry needs something that serves one. [`FakeControl`] answers
//! [`crate::revocation::REVOCATION_LIST_PATH`] with a list freshly signed by
//! the key the test signs its capabilities with, and [`crate::JWKS_PATH`] with
//! that key, over plain HTTP on loopback - the one transport a service
//! accepts without a machine identity of the control plane's.
//!
//! It runs on its own threads rather than a tokio task, so one instance can
//! be shared by every test in a binary even though each test has a runtime of
//! its own.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use ed25519_dalek::SigningKey;

use crate::keys::Jwks;
use crate::revocation::{REVOCATION_LIST_PATH, RevocationList, SignedRevocationList, sign};

/// A loopback HTTP server playing the control plane's machine channel for
/// the revocation list and the JWKS. Lives until the process exits.
#[derive(Clone)]
pub struct FakeControl {
    url: String,
    state: Arc<Mutex<Served>>,
}

struct Served {
    signing: SigningKey,
    auth_epoch: u64,
    revoked_jtis: Vec<String>,
    revoked_nodes: Vec<String>,
}

impl FakeControl {
    /// Start serving lists signed with `signing`, revoking nothing.
    pub fn start(signing: SigningKey) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let url = format!(
            "http://{}",
            listener
                .local_addr()
                .expect("a bound listener has an address")
        );
        let state = Arc::new(Mutex::new(Served {
            signing,
            auth_epoch: 0,
            revoked_jtis: Vec::new(),
            revoked_nodes: Vec::new(),
        }));
        let served = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let served = Arc::clone(&served);
                std::thread::spawn(move || answer(stream, &served));
            }
        });
        Self { url, state }
    }

    /// The base URL a service's `control_endpoint` names.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// A client for this stand-in, as a service would build from its
    /// `[machine_identity]` table.
    pub fn client(&self) -> crate::ControlClient {
        crate::ControlClient::loopback(&self.url).expect("a loopback URL")
    }

    /// Name `nodes` as revoked in every list served from now on.
    pub fn revoke_nodes(&self, nodes: Vec<String>) {
        self.lock().revoked_nodes = nodes;
    }

    /// Name `jtis` as revoked in every list served from now on.
    pub fn revoke_jtis(&self, jtis: Vec<String>) {
        self.lock().revoked_jtis = jtis;
    }

    /// Serve `epoch` as the auth epoch from now on.
    pub fn set_auth_epoch(&self, epoch: u64) {
        self.lock().auth_epoch = epoch;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Served> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn answer(stream: TcpStream, served: &Mutex<Served>) {
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    // Drain the headers; nothing here reads a body.
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) if line == "\r\n" || line == "\n" => break,
            Ok(_) => {}
        }
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("");
    let body = {
        let served = served.lock().unwrap_or_else(|e| e.into_inner());
        if path == REVOCATION_LIST_PATH {
            let list = RevocationList::new(
                "spiffe://deislet/control/test",
                served.auth_epoch,
                served.revoked_jtis.clone(),
            )
            .with_revoked_nodes(served.revoked_nodes.clone());
            Some(
                serde_json::to_string(&SignedRevocationList {
                    signed_list: sign(&list, &served.signing),
                })
                .expect("a signed list serializes"),
            )
        } else if path == crate::JWKS_PATH {
            Some(
                serde_json::to_string(&Jwks::from_keys(&[served.signing.verifying_key()]))
                    .expect("a JWKS serializes"),
            )
        } else {
            None
        }
    };
    let response = match body {
        Some(body) => format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ),
        None => {
            "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string()
        }
    };
    let mut stream = stream;
    let _ = stream.write_all(response.as_bytes());
}
