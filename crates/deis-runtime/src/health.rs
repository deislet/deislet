//! Whether this node is ready to be given traffic.
//!
//! A node is not healthy because its process started. It is healthy once it
//! has asked the control plane what it should be running and has loaded it
//! (docs/implementation-plan.zh.md section 4, principle 8). Between those two
//! moments it is listening - so the control plane can reach it and so an
//! operator gets an answer rather than a refused connection - but it says it
//! is not ready.
//!
//! The third state is the one that matters operationally: a node that could
//! not reach the control plane at all. It keeps serving whatever it recovered
//! from its own disk, and it reports unhealthy for as long as that is true, so
//! that "serving stale versions" never looks the same as "serving what it was
//! told to".
//!
//! # Who is told
//!
//! Two callers, and until now only one of them existed. The `HealthCheck` RPC
//! on the management plane reports [`NodeHealth::details`] to the control
//! plane. The *edge* asks a different question over a different port: deis-proxy
//! health checks each upstream with `GET /health` carrying
//! `x-deis-app-id: _system`, and takes a node out of rotation on anything that
//! is not a `200`.
//!
//! That probe used to be answered by a JavaScript literal in
//! `dispatcher/system_app.js` that consulted nothing, so a node which could not
//! reach its control plane - a fleet restart, a control-plane upgrade, a bad
//! restore - passed the probe for ever while answering `500` to every real
//! request. Measured on a two-node edge: half of every hundred requests failed,
//! indefinitely, with no probe ever failing.
//!
//! The system application cannot read this module. It runs inside an isolate,
//! and `runtime/js/runtime.js` deletes `globalThis.Deno` before a line of
//! application code runs, so there is no op table to reach and no global to
//! read. What an application *can* read is its request, and the platform's own
//! `x-deis-` namespace on a request is swept and re-stamped by
//! `dispatcher/handlers.rs` - which is exactly a channel for "what this node
//! decided about this request", and cannot be forged from outside because the
//! sweep drops the client's copy first. So the readiness travels in on
//! [`READINESS_HEADER`] and [`SERVING_APPS_HEADER`], stamped for
//! [`SYSTEM_APP_ID`] alone.
//!
//! # What the probe is answered, and when
//!
//! The two halves of the answer say different things, and keeping them apart
//! is what lets the node and the edge each decide the part they are entitled
//! to.
//!
//! **The status code answers "can this node serve anything at all?"** It is the
//! node's own veto, and it is what every probe understands - an edge whose
//! readiness policy is `ignored`, a load balancer that is not deis-proxy, a
//! container runtime's readiness probe:
//!
//! * `200` when the node is [`Readiness::Ready`], including a ready node with
//!   no applications: an empty assignment list is a correct answer, and such a
//!   node serves every request it is given exactly as the catalog says to.
//! * `200` when the node is [`Readiness::Degraded`] **and is still serving at
//!   least one application**. It has stale versions rather than none, which is
//!   worth traffic and is the state a whole fleet enters together the moment a
//!   control plane goes down. Answering `503` here would turn a control-plane
//!   outage, which costs nothing while the nodes keep serving, into a total
//!   one caused by the edge.
//! * `503` when the node is [`Readiness::Starting`], and when it is
//!   `Degraded` with nothing loaded. Both are nodes that answer `500` to every
//!   real request - the measured case behind this fix was 50 of 100 requests
//!   failing through a two-node edge, for ever, with the probe passing every
//!   time.
//! * `503` when no readiness reached the application at all. A system
//!   application that cannot see its node's readiness must not vouch for it.
//!
//! **The response header answers "is this node running what it was told to?"**
//! The system application repeats [`READINESS_HEADER`] onto its answer, and
//! `deis-proxy`'s `health_check.readiness` policy decides what to do with a
//! node that says `degraded` - keep its share and warn, by default. That
//! header is the edge's contract and is spelled identically in
//! `deis_proxy::config::system::health::NODE_READINESS_HEADER`.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use arc_swap::{ArcSwap, ArcSwapOption};

/// The application that answers the edge's health probe.
///
/// Loaded by `dispatcher::start_server` before the node serves anything and
/// without a deployment, which is what lets it be asked on a node that has been
/// assigned nothing. deis-proxy spells the same string as
/// `config::system::health::DEFAULT_HEALTH_CHECK_APP_ID`.
pub const SYSTEM_APP_ID: &str = "_system";

/// Header the dispatcher stamps on a request for [`SYSTEM_APP_ID`], carrying
/// this node's readiness.
///
/// Inside the platform's own `x-deis-` prefix on purpose: everything under it
/// is dropped from an incoming request before an isolate sees it, so a client
/// cannot send `x-deis-node-readiness: ready` at the node's public port and
/// make an unhealthy node pass the edge's probe. The value is one of
/// [`Readiness::state_name`].
pub const READINESS_HEADER: &str = "x-deis-node-readiness";

/// The one value of [`READINESS_HEADER`] that means "give this node traffic".
///
/// Spelled as a constant because two files have to agree on it and only one of
/// them is Rust: `dispatcher/system_app.js` compares against this exact string,
/// and [`tests::the_system_application_answers_the_probe_from_the_header`] is
/// what holds the two spellings together.
pub const READINESS_READY: &str = "ready";

/// Header carrying how many applications the node is serving at this moment,
/// stamped beside [`READINESS_HEADER`] on a request for [`SYSTEM_APP_ID`].
///
/// It is what separates the two very different nodes that both report
/// `degraded`: one recovered its applications from its own disk and is serving
/// stale versions, which is worth traffic, and one recovered nothing and
/// answers `500` to every request, which is not. The state name alone cannot
/// tell them apart, and treating them alike is how the second kind kept its
/// full share of the fleet's traffic indefinitely.
pub const SERVING_APPS_HEADER: &str = "x-deis-node-apps";

/// The readiness of the node running in this process.
///
/// A process runs one node, so this is not an ambiguity; it exists because the
/// one place that has to read the readiness on the request path -
/// `dispatcher::handlers::handle_request`, stamping [`READINESS_HEADER`] - sits
/// behind an `AppState` that every test in the crate constructs, and threading
/// an `Arc` through all of them to reach one `if` would put the change in more
/// files than the fix.
///
/// Empty until a [`NodeHealth`] is built. An empty one means no node was ever
/// started in this process, and the probe is then answered `503`: a system
/// application that cannot see the node's readiness must not vouch for it.
static SERVING: LazyLock<ArcSwapOption<NodeHealth>> = LazyLock::new(ArcSwapOption::empty);

/// What this process's node currently says about itself, for the request path.
///
/// `None` before any node has been built here.
pub fn serving_readiness() -> Option<Readiness> {
    SERVING.load_full().map(|health| health.snapshot())
}

/// What the node is currently able to say about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    /// Still working through the startup assignment list.
    Starting,
    /// Running exactly what the control plane last said to run.
    Ready { apps: usize },
    /// Serving, but not from an answer the control plane gave.
    Degraded { reason: String, apps: usize },
}

impl Readiness {
    pub fn is_healthy(&self) -> bool {
        matches!(self, Readiness::Ready { .. })
    }

    /// How many applications the node is serving in this state.
    ///
    /// Zero for [`Readiness::Starting`], which is a node that has not been
    /// told what to run yet and therefore has nothing loaded.
    pub fn serving_apps(&self) -> usize {
        match self {
            Readiness::Starting => 0,
            Readiness::Ready { apps } | Readiness::Degraded { apps, .. } => *apps,
        }
    }

    /// The word this state travels as, on the management RPC's `details` and
    /// on [`READINESS_HEADER`].
    pub fn state_name(&self) -> &'static str {
        match self {
            Readiness::Starting => "starting",
            // The one word the system application compares against, so it is
            // the constant and not a second copy of the same letters.
            Readiness::Ready { .. } => READINESS_READY,
            Readiness::Degraded { .. } => "degraded",
        }
    }
}

/// The node's readiness, shared between the startup sync and the health RPC.
#[derive(Debug)]
pub struct NodeHealth {
    state: ArcSwap<Readiness>,
}

impl NodeHealth {
    /// A node that has not finished starting yet.
    ///
    /// Publishes itself as this process's node, so that
    /// [`serving_readiness`] - and through it the edge's health probe - reads
    /// the same value the management RPC reports. A second call replaces the
    /// first: in the binary there is one, and a test process that builds
    /// several is only ever asked about the last.
    pub fn starting() -> Arc<Self> {
        let health = Arc::new(Self {
            state: ArcSwap::from_pointee(Readiness::Starting),
        });
        SERVING.store(Some(Arc::clone(&health)));
        health
    }

    pub fn snapshot(&self) -> Readiness {
        (**self.state.load()).clone()
    }

    pub fn is_healthy(&self) -> bool {
        self.state.load().is_healthy()
    }

    /// The node is running what it was told to run.
    pub fn mark_ready(&self, apps: usize) {
        let previous = self.snapshot();
        self.state.store(Arc::new(Readiness::Ready { apps }));
        match previous {
            Readiness::Ready { .. } => {}
            Readiness::Starting => tracing::info!(
                apps,
                "Node is ready: it has the control plane's assignment list and has loaded all \
                 {} of its apps",
                apps
            ),
            Readiness::Degraded { .. } => tracing::info!(
                apps,
                "Node reached the control plane again and is now running its assigned versions; \
                 it is healthy"
            ),
        }
    }

    /// The node is serving, but not from an answer the control plane gave.
    pub fn mark_degraded(&self, reason: impl Into<String>, apps: usize) {
        let reason = reason.into();
        let already =
            matches!(self.snapshot(), Readiness::Degraded { reason: ref r, .. } if *r == reason);
        self.state.store(Arc::new(Readiness::Degraded {
            reason: reason.clone(),
            apps,
        }));
        if !already {
            tracing::error!(
                apps,
                reason = %reason,
                "Node is DEGRADED and reports unhealthy: {}. It keeps serving the {} app(s) it \
                 recovered from its own disk, which may be an older version than the one it is \
                 supposed to run, and any secret those apps declare is missing",
                reason,
                apps
            );
        }
    }

    /// Key/value form for the `HealthCheck` RPC, so an operator sees *why*
    /// rather than just `healthy: false`.
    pub fn details(&self) -> HashMap<String, String> {
        let mut details = HashMap::new();
        let state = self.snapshot();
        details.insert("state".to_string(), state.state_name().to_string());
        match state {
            Readiness::Starting => {
                details.insert(
                    "reason".to_string(),
                    "still loading the assignment list from the control plane".to_string(),
                );
            }
            Readiness::Ready { apps } => {
                details.insert("apps".to_string(), apps.to_string());
            }
            Readiness::Degraded { reason, apps } => {
                details.insert("reason".to_string(), reason);
                details.insert("apps".to_string(), apps.to_string());
            }
        }
        details
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_starting_node_is_not_healthy() {
        let health = NodeHealth::starting();
        assert!(!health.is_healthy());
        assert_eq!(health.details().get("state").unwrap(), "starting");
    }

    #[test]
    fn a_node_is_healthy_only_after_it_loaded_its_assignments() {
        let health = NodeHealth::starting();
        health.mark_ready(2);
        assert!(health.is_healthy());
        assert_eq!(health.details().get("apps").unwrap(), "2");
    }

    #[test]
    fn a_node_serving_from_its_own_disk_reports_unhealthy_with_the_reason() {
        let health = NodeHealth::starting();
        health.mark_degraded("control plane unreachable", 1);
        assert!(!health.is_healthy());
        let details = health.details();
        assert_eq!(details.get("state").unwrap(), "degraded");
        assert_eq!(details.get("reason").unwrap(), "control plane unreachable");
        assert_eq!(details.get("apps").unwrap(), "1");
    }

    /// The dispatcher has something to stamp on the probe as soon as a node
    /// exists in this process.
    ///
    /// It does not assert *which* readiness comes back: `SERVING` is
    /// process-wide and several tests in this crate build a `NodeHealth`, so
    /// the value read here belongs to whichever of them published last. What
    /// the request path needs from this is only that a node having been built
    /// is never indistinguishable from no node at all - the second is what
    /// makes the system application answer 503.
    #[test]
    fn a_node_publishes_itself_for_the_request_path() {
        let _health = NodeHealth::starting();
        let published = serving_readiness().expect("a node was built in this process");
        assert!(
            ["starting", READINESS_READY, "degraded"].contains(&published.state_name()),
            "the header would carry {:?}, which the system application cannot read",
            published.state_name()
        );
    }

    /// The probe the edge actually makes, answered by the file that actually
    /// answers it.
    ///
    /// This runs `dispatcher/system_app.js` in a real isolate, because the
    /// half of this fix that can go wrong silently is the JavaScript: the two
    /// header names, the word "ready", the status codes and the response
    /// header the edge reads its rotation decision off all have to match what
    /// `handlers.rs` stamps and what `deis-proxy` looks for, and nothing else
    /// in the build compares the spellings. Before the fix this file answered
    /// 200 with no readiness header to every one of the cases below.
    ///
    /// The guard is held across the awaits on purpose: it is what keeps two
    /// tests off the shared isolate host at once, which is exactly this span.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn the_system_application_answers_the_probe_from_the_header() {
        use crate::app::{test_host, AppConfig, AppKey};

        let _turn = test_host::take();
        let apps = test_host::host();
        // Not `_system`: the shared host is used by other tests, and this one
        // must not collide with a node's own copy.
        let key = AppKey::production("system-probe-under-test");
        apps.load_app(
            key.clone(),
            include_str!("dispatcher/system_app.js").to_string(),
            AppConfig::default(),
            None,
        )
        .await
        .expect("the system application loads");

        /// Status, the readiness the answer states back, and the body.
        async fn probe(
            apps: &std::sync::Arc<crate::app::ApplicationManager>,
            key: &AppKey,
            readiness: Option<&Readiness>,
        ) -> (u16, Option<String>, String) {
            let headers = match readiness {
                Some(readiness) => vec![
                    (
                        READINESS_HEADER.to_string(),
                        readiness.state_name().to_string(),
                    ),
                    (
                        SERVING_APPS_HEADER.to_string(),
                        readiness.serving_apps().to_string(),
                    ),
                ],
                None => Vec::new(),
            };
            let (status, headers, body) = apps
                .execute_app(
                    key,
                    "http://node.invalid/health".to_string(),
                    "GET".to_string(),
                    headers,
                    None,
                )
                .await
                .expect("the probe reaches the handler");
            let stated = crate::runtime::header_list::first_value(&headers, READINESS_HEADER)
                .map(str::to_string);
            (
                status,
                stated,
                String::from_utf8(body.into_bytes().unwrap()).unwrap(),
            )
        }

        // A ready node, including one the catalog has assigned nothing: an
        // empty assignment list is a correct answer and such a node serves
        // every request it is given exactly as it should.
        for ready in [Readiness::Ready { apps: 2 }, Readiness::Ready { apps: 0 }] {
            let (status, stated, body) = probe(&apps, &key, Some(&ready)).await;
            assert_eq!(status, 200, "a ready node has to stay in rotation: {body}");
            assert_eq!(stated.as_deref(), Some(READINESS_READY), "{body}");
        }

        // The state this whole fix exists for. Degraded and still serving is
        // worth traffic - a whole fleet enters it together when a control
        // plane goes down - so it answers 200 and states `degraded`, which is
        // what lets the edge's own policy decide. Degraded with nothing
        // loaded is the node that answered 500 to every request while passing
        // this probe for ever, and it is refused here rather than left to a
        // policy that may keep it.
        let (status, stated, body) = probe(
            &apps,
            &key,
            Some(&Readiness::Degraded {
                reason: "control plane unreachable".to_string(),
                apps: 3,
            }),
        )
        .await;
        assert_eq!(
            status, 200,
            "a degraded node that is still serving must keep its share, or a control-plane \
             outage becomes a traffic outage: {body}"
        );
        assert_eq!(
            stated.as_deref(),
            Some("degraded"),
            "the edge decides what degraded is worth, so it has to be told: {body}"
        );

        let (status, stated, body) = probe(
            &apps,
            &key,
            Some(&Readiness::Degraded {
                reason: "control plane unreachable".to_string(),
                apps: 0,
            }),
        )
        .await;
        assert_eq!(
            status, 503,
            "a degraded node with nothing loaded fails every request it is given: {body}"
        );
        assert_eq!(stated.as_deref(), Some("degraded"), "{body}");

        let (status, stated, body) = probe(&apps, &key, Some(&Readiness::Starting)).await;
        assert_eq!(
            status, 503,
            "a node that has not been told what to run must not be given traffic: {body}"
        );
        assert_eq!(stated.as_deref(), Some("starting"), "{body}");

        // No headers at all: a node whose dispatcher did not stamp them is a
        // node this application knows nothing about, and it must not vouch
        // for it.
        let (status, stated, body) = probe(&apps, &key, None).await;
        assert_eq!(status, 503, "{body}");
        assert_eq!(stated.as_deref(), Some("unknown"), "{body}");
        assert!(body.contains("unknown"), "{body}");

        let _ = apps.unload_app(&key).await;
    }

    #[test]
    fn recovering_contact_with_the_control_plane_clears_the_degraded_state() {
        let health = NodeHealth::starting();
        health.mark_degraded("control plane unreachable", 1);
        health.mark_ready(1);
        assert_eq!(health.snapshot(), Readiness::Ready { apps: 1 });
    }
}
