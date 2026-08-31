//! Answering the `http-01` challenge on the data plane.
//!
//! The authority proves that whoever asked for a certificate controls the
//! name by fetching `http://<name>/.well-known/acme-challenge/<token>` over
//! plain HTTP on port 80 and comparing what comes back with a value only the
//! account key can produce. This proxy already owns port 80 for every name
//! it serves, so it answers that request itself: no second listener, no
//! webroot shared with something else, nothing to keep in step.
//!
//! Two places are consulted, in this order:
//!
//! 1. the in-memory map, which the renewal task fills just before it tells
//!    the authority to go and look;
//! 2. the challenge directory in the certificate store, which exists because
//!    a zero-downtime upgrade has two processes alive at once and the
//!    outgoing one still owns the listening sockets. The fetch can land on
//!    the process that did not create the order, and without the file that
//!    process would answer 404 and fail an order that was perfectly correct.
//!
//! The path is also served on the TLS listener. An authority follows a
//! redirect to HTTPS, and this proxy redirects to HTTPS as soon as a name
//! has any certificate at all - so a renewal for a name that already works
//! would otherwise chase a redirect to a listener that answered 404.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tracing::{debug, info};

use super::store::{AcmeStore, is_challenge_token};

/// The path prefix RFC 8555 section 8.3 fixes. Not configurable, by the RFC.
pub const CHALLENGE_PREFIX: &str = "/.well-known/acme-challenge/";

/// The published answers, readable from the request path.
pub struct ChallengeResponder {
    /// Token to key authorization. Swapped whole; a renewal pass publishes
    /// one or two at a time and the request path only ever reads.
    published: ArcSwap<HashMap<String, String>>,
    store: AcmeStore,
}

impl ChallengeResponder {
    pub fn new(store: AcmeStore) -> Self {
        Self {
            published: ArcSwap::from_pointee(HashMap::new()),
            store,
        }
    }

    /// The answer for a request path, or `None` when this instance never
    /// published that token.
    ///
    /// `path` is the whole request path; the caller has already established
    /// that it starts with [`CHALLENGE_PREFIX`].
    pub fn answer_for_path(&self, path: &str) -> Option<String> {
        let token = path.strip_prefix(CHALLENGE_PREFIX)?;
        // Checked here rather than only in the store, because this is the
        // boundary the value crosses from a request into a file name. A
        // token is base64url by the RFC, which contains neither `/` nor `.`,
        // so a path that walked anywhere is not a token and is refused
        // before it is used for anything at all.
        if !is_challenge_token(token) {
            debug!(
                token_len = token.len(),
                "an ACME challenge path that is not a token was refused without being looked up"
            );
            return None;
        }
        if let Some(answer) = self.published.load().get(token) {
            return Some(answer.clone());
        }
        // Not in this process. During an upgrade handover that is expected
        // rather than exceptional, which is what the file is for.
        self.store.published_challenge(token)
    }

    /// Publish an answer, in memory and on disk, before the authority is
    /// told to look for it.
    pub fn publish(&self, token: &str, key_authorization: &str) {
        let mut next = (**self.published.load()).clone();
        next.insert(token.to_string(), key_authorization.to_string());
        self.published.store(Arc::new(next));

        if let Err(e) = self.store.publish_challenge(token, key_authorization) {
            // Not fatal: the in-memory copy answers every request this
            // process receives, and that is every request unless an upgrade
            // is happening at this exact moment. Worth saying, because the
            // one case it does not cover is the one that is hard to debug.
            info!(
                error = %e,
                "the challenge answer could not be written to the certificate store, so only \
                 this process can answer it; an order that overlaps a zero-downtime upgrade may \
                 fail and be retried"
            );
        }
    }

    /// Forget an answer once the authority has finished with it.
    pub fn withdraw(&self, token: &str) {
        let mut next = (**self.published.load()).clone();
        next.remove(token);
        self.published.store(Arc::new(next));
        self.store.withdraw_challenge(token);
    }

    /// How many answers are currently published in this process.
    pub fn published_count(&self) -> usize {
        self.published.load().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn responder(dir: &Path) -> ChallengeResponder {
        let store = AcmeStore::new(dir, "https://ca.test/dir");
        store.prepare().unwrap();
        ChallengeResponder::new(store)
    }

    #[test]
    fn a_published_token_is_answered_and_an_unknown_one_is_not() {
        let temp = tempfile::tempdir().unwrap();
        let responder = responder(temp.path());
        responder.publish("tok-1", "tok-1.thumbprint");

        assert_eq!(
            responder.answer_for_path("/.well-known/acme-challenge/tok-1"),
            Some("tok-1.thumbprint".to_string())
        );
        assert_eq!(
            responder.answer_for_path("/.well-known/acme-challenge/tok-2"),
            None
        );
        assert_eq!(responder.answer_for_path("/index.html"), None);
    }

    #[test]
    fn a_second_process_answers_from_the_store_the_first_wrote() {
        // The zero-downtime upgrade case: the process holding the listening
        // sockets is not the process that created the order. Without this,
        // an upgrade that happened to overlap a renewal would fail the order
        // with a 404 that looked like a DNS problem.
        let temp = tempfile::tempdir().unwrap();
        let outgoing = responder(temp.path());
        let incoming = responder(temp.path());

        incoming.publish("tok-handover", "tok-handover.thumbprint");
        assert_eq!(
            outgoing.answer_for_path("/.well-known/acme-challenge/tok-handover"),
            Some("tok-handover.thumbprint".to_string()),
            "the process that owns the socket must be able to answer an order the other one made"
        );
    }

    #[test]
    fn a_path_that_is_not_a_token_never_reaches_the_file_system() {
        // The token is a path segment chosen by whoever sent the request.
        // These are the shapes that would otherwise be joined onto the
        // challenge directory.
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("secret"), "top secret").unwrap();
        let responder = responder(temp.path());

        for hostile in [
            "/.well-known/acme-challenge/../../secret",
            "/.well-known/acme-challenge/..",
            "/.well-known/acme-challenge/",
            "/.well-known/acme-challenge/a/b",
        ] {
            assert_eq!(responder.answer_for_path(hostile), None, "{hostile}");
        }
    }

    #[test]
    fn a_withdrawn_answer_stops_being_served_in_both_places() {
        let temp = tempfile::tempdir().unwrap();
        let other_process = responder(temp.path());
        let responder = responder(temp.path());

        responder.publish("tok-3", "tok-3.thumbprint");
        assert_eq!(responder.published_count(), 1);
        responder.withdraw("tok-3");

        assert_eq!(responder.published_count(), 0);
        assert_eq!(
            responder.answer_for_path("/.well-known/acme-challenge/tok-3"),
            None
        );
        assert_eq!(
            other_process.answer_for_path("/.well-known/acme-challenge/tok-3"),
            None,
            "a spent answer must not be left behind for another process to keep serving"
        );
    }
}
