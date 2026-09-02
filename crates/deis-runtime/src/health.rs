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
//! [`READINESS_HEADER`], [`SERVING_APPS_HEADER`] and [`REASON_HEADER`],
//! stamped for [`SYSTEM_APP_ID`] alone.
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
//!
//! **The body answers "why", and only ever in the node's own words.** A node
//! is degraded for more than one reason - a control plane nobody can reach, a
//! version that would not load, an environment that would not stop, a pushed
//! deployment this node refused - and the state name alone used to be all that
//! travelled, so the system application filled the gap by naming the first of
//! those for every degraded node. An operator reading that during an incident
//! is sent to the one part of the system that is working, which is worse than
//! being told nothing. [`REASON_HEADER`] carries the reason the node actually
//! recorded, and the answer says `"reason": null` when there is none rather
//! than choosing one.
//!
//! # When the answer changes
//!
//! Not only once a reconcile pass ends. The pass is the slowest of the events
//! that change what this node can serve, and between two of them a pushed
//! deployment can be taken or refused and an application can be stopped - so a
//! node that had just refused a push went on answering `ready`, and taking its
//! full share of that application's traffic, for a whole reconcile interval.
//! On a node with no control plane configured there is no next pass at all and
//! it answered `ready` for ever. Every such event goes through
//! [`NodeHealth::note_serving`] or [`NodeHealth::note_refusal`] as it happens;
//! `deployment::run_control_loop` is what hands the deployment manager the
//! same handle it reports its own passes through.

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

/// Header carrying *why* the node is degraded, stamped beside the two above on
/// a request for [`SYSTEM_APP_ID`].
///
/// Absent for a node that is not degraded, because the other two states have
/// no reason to carry.
///
/// It exists because `degraded` has more than one cause and the probe's answer
/// used to name only one of them. The state name alone travelled, so the
/// system application had nothing to go on and said "this node cannot reach
/// its control plane" for every degraded node - including the ones whose
/// control plane had answered perfectly well and whose real trouble was a
/// bundle that would not load or an environment that would not stop. An
/// operator reading that during an incident is sent to the one part of the
/// system that is working.
///
/// The value is [`Readiness::reason`] put through [`header_safe_reason`],
/// because a reason is built from an error message and a header value is not
/// allowed to be arbitrary bytes.
///
/// # What this puts on an unauthenticated answer
///
/// The probe is answered on the node's public port to anyone who asks for
/// `/health` with `x-deis-app-id: _system`, and the reason travels into the
/// body. So a reason built from an error carries what that error carries -
/// an application id, an environment, the control plane's own address out of
/// a transport error - to whoever can reach the port. That is a deliberate
/// trade and not an oversight: the reader this answer exists for is the
/// operator holding a terminal during an incident, the audience that can
/// reach a node's data port can already enumerate application ids by routing
/// to them, and the alternative on offer was the invented cause this header
/// replaced. What is kept out of it deliberately is any text that came from
/// inside an isolate: a module that throws while it is being evaluated writes
/// its own message, and `throw new Error(env.STRIPE_KEY)` would otherwise put
/// a secret on an unauthenticated answer. So a refusal names which step
/// refused ([`crate::deployment::DeployError::kind`]) and never the message
/// that step produced, which stays in the log and in the answer the control
/// plane gets.
pub const REASON_HEADER: &str = "x-deis-node-reason";

/// Longest reason that travels on [`REASON_HEADER`].
///
/// Long enough for the reasons this node builds - the longest is a transport
/// error with a URL in it, around 150 characters - and short enough that a
/// pathological error message cannot push the probe's request headers past
/// what a server will accept.
const MAX_REASON_BYTES: usize = 256;

/// One line of visible ASCII, at most [`MAX_REASON_BYTES`] bytes.
///
/// A reason is `format!("...: {e}")` over somebody else's error type, so it
/// can hold anything: a newline out of a nested error, a NUL, a non-ASCII
/// path. None of that may reach a header value - a carriage return in one is
/// how a header becomes two - so everything outside printable ASCII becomes a
/// space here, and a reason too long to carry is cut and ends in three dots so
/// a truncated one is never mistaken for the whole of what the node said.
pub fn header_safe_reason(reason: &str) -> String {
    /// What a cut reason ends with. Three ASCII dots and not an ellipsis
    /// character: the point of this function is that nothing outside printable
    /// ASCII leaves it.
    const CUT: &str = "...";

    let mut out = String::with_capacity(reason.len().min(MAX_REASON_BYTES));
    for c in reason.chars() {
        // One space per character and not per byte, so a path with non-ASCII
        // in it keeps its shape rather than growing.
        let c = if c.is_ascii_graphic() || c == ' ' {
            c
        } else {
            ' '
        };
        // Everything this writes is one byte, so the room left for `CUT` is
        // counted once here rather than after the fact.
        if out.len() + 1 > MAX_REASON_BYTES - CUT.len() {
            out.push_str(CUT);
            return out;
        }
        out.push(c);
    }
    out
}

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

    /// Why the node is in this state, for the states that have a why.
    ///
    /// `None` for [`Readiness::Ready`] and [`Readiness::Starting`]: neither is
    /// a fault, and inventing a reason for them would be the defect this
    /// method exists to end.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Readiness::Starting | Readiness::Ready { .. } => None,
            Readiness::Degraded { reason, .. } => Some(reason),
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

    /// The set of applications this node serves changed between reconcile
    /// passes, and nothing else about its situation did.
    ///
    /// Moves the count and keeps the state and its reason, because a push that
    /// landed or an application that was stopped says nothing about whether
    /// this node is running what the control plane last told it to. The count
    /// is not decoration: it is what decides `200` against `503` at the edge
    /// for a degraded node, so a node that unloaded its last application while
    /// degraded has to stop claiming it can serve before the next pass, and a
    /// node that took delivery of one has to stop claiming it cannot.
    ///
    /// [`Readiness::Starting`] is left exactly as it is. Such a node has not
    /// been told what it should be running yet, so no count of it is an
    /// answer - and `Degraded` with applications loaded is a *more* permissive
    /// answer at the edge than `Starting` is, so an intermediate count must
    /// never be the thing that lifts it.
    ///
    /// Read-modify-write through `rcu`, not a load and a store: a push and an
    /// unload for two different applications run side by side, and a plain
    /// store would let the slower of the two put its own count back.
    pub fn note_serving(&self, apps: usize) {
        let previous = self.state.rcu(|current| match &**current {
            Readiness::Starting => Readiness::Starting,
            Readiness::Ready { .. } => Readiness::Ready { apps },
            Readiness::Degraded { reason, .. } => Readiness::Degraded {
                reason: reason.clone(),
                apps,
            },
        });

        let was = previous.serving_apps();
        match &*previous {
            // Nothing was changed above, so nothing is reported.
            Readiness::Starting => {}
            _ if was == apps => {}
            // The moment the edge stops sending this node traffic: the probe
            // answers 503 from here, because a degraded node with nothing
            // loaded fails every request it is given.
            Readiness::Degraded { reason, .. } if apps == 0 => tracing::warn!(
                apps,
                previously = was,
                reason = %reason,
                "This node is degraded and has just stopped serving the last of its {} \
                 application(s); the edge's health probe answers 503 from now on and this node \
                 leaves rotation, which is right - it has nothing left to answer with",
                was
            ),
            Readiness::Degraded { reason, .. } => tracing::info!(
                apps,
                previously = was,
                reason = %reason,
                "This node is degraded and now serves {} application(s) rather than {}; the \
                 edge's health probe answers 200 while that count is above zero",
                apps,
                was
            ),
            Readiness::Ready { .. } => tracing::debug!(
                apps,
                previously = was,
                "This node now serves {} application(s) rather than {}, and is still running \
                 what the control plane last assigned it",
                apps,
                was
            ),
        }
    }

    /// An instruction reached this node between reconcile passes and this node
    /// did not carry it out.
    ///
    /// Degrades the node there and then. Without this, a node that had just
    /// refused a pushed deployment went on answering the edge `ready` - and
    /// taking its full share of the traffic for that application - until the
    /// next reconcile pass, which is a whole `reconcile_interval` away and
    /// never arrives at all on a node with no control plane configured.
    ///
    /// A node that is still [`Readiness::Starting`] is left starting. It has
    /// not claimed to be running the assignment list yet, so it is not falling
    /// short of one; and `Degraded` with applications loaded is the more
    /// permissive answer of the two at the edge, so a refusal must never be
    /// what promotes a node into rotation.
    pub fn note_refusal(&self, reason: impl Into<String>, apps: usize) {
        let reason = reason.into();
        if matches!(self.snapshot(), Readiness::Starting) {
            tracing::warn!(
                apps,
                reason = %reason,
                "This node refused an instruction while it is still loading its assignment \
                 list: {}. It keeps reporting `starting`, which already keeps it out of \
                 rotation; the first reconcile pass is what decides what it reports next",
                reason
            );
            return;
        }
        self.mark_degraded(reason, apps);
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

    /// Ask `system_app.js` the edge's probe with exactly the headers a node in
    /// `readiness` would have stamped.
    ///
    /// Returns the status, the readiness the answer states back on
    /// [`READINESS_HEADER`] - which is the only part of the answer the edge
    /// reads - and the body an operator would see.
    async fn probe(
        apps: &std::sync::Arc<crate::app::ApplicationManager>,
        key: &crate::app::AppKey,
        readiness: Option<&Readiness>,
    ) -> (u16, Option<String>, String) {
        let headers = match readiness {
            Some(readiness) => {
                let mut headers = vec![
                    (
                        READINESS_HEADER.to_string(),
                        readiness.state_name().to_string(),
                    ),
                    (
                        SERVING_APPS_HEADER.to_string(),
                        readiness.serving_apps().to_string(),
                    ),
                ];
                // Exactly as `dispatcher::handlers` stamps it: present for the
                // one state that has a reason, absent otherwise.
                if let Some(reason) = readiness.reason() {
                    headers.push((REASON_HEADER.to_string(), header_safe_reason(reason)));
                }
                headers
            }
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

    /// The probe states the reason the node recorded, not a guess at it.
    ///
    /// `Degraded` has more than one cause and only one of them is a control
    /// plane nobody can reach. A bundle that will not load, or an environment
    /// that would not stop, degrades a node whose control plane answered
    /// perfectly well - and the answer that told an operator to go and look at
    /// the control plane sent them to the one part of the system that was
    /// working. During an incident that is worse than saying nothing.
    ///
    /// The guard is held across the awaits on purpose: it is what keeps two
    /// tests off the shared isolate host at once, which is exactly this span.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn the_probe_states_the_reason_the_node_recorded_rather_than_guessing_it() {
        use crate::app::{test_host, AppConfig, AppKey};

        let _turn = test_host::take();
        let apps = test_host::host();
        let key = AppKey::production("system-probe-reason");
        apps.load_app(
            key.clone(),
            include_str!("dispatcher/system_app.js").to_string(),
            AppConfig::default(),
            None,
        )
        .await
        .expect("the system application loads");

        // Exactly what `run_control_loop` records when a reconcile pass
        // reached the control plane and could not put the node right.
        let reason = "2 of 5 assigned versions could not be loaded and 0 could not be stopped";
        let (status, _stated, body) = probe(
            &apps,
            &key,
            Some(&Readiness::Degraded {
                reason: reason.to_string(),
                apps: 3,
            }),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains(reason),
            "the operator reading this probe has to be told what actually degraded the node: \
             {body}"
        );
        assert!(
            !body.contains("cannot reach its control plane"),
            "the control plane answered this node; sending an operator to look at it during an \
             incident is worse than saying nothing: {body}"
        );

        let _ = apps.unload_app(&key).await;
    }

    /// A reason is built from somebody else's error message, so it has to be
    /// made fit for a header before it is one.
    #[test]
    fn a_reason_is_cut_down_to_something_a_header_may_hold() {
        assert_eq!(
            header_safe_reason("control plane unreachable"),
            "control plane unreachable"
        );

        // A carriage return in a header value is how one header becomes two.
        assert_eq!(
            header_safe_reason("connect failed:\r\nno route\tto host\0"),
            "connect failed:  no route to host "
        );

        // Non-ASCII out of a path or a locale-translated OS error: one space
        // per character, not per byte, so the reason keeps its shape.
        assert_eq!(
            header_safe_reason("bundle /srv/\u{5e94}\u{7528}/x.zip"),
            "bundle /srv/  /x.zip"
        );

        let long = "e".repeat(4096);
        let cut = header_safe_reason(&long);
        assert!(
            cut.len() <= MAX_REASON_BYTES,
            "a pathological error message must not push the probe's headers past what a server \
             accepts, and this one is {} bytes",
            cut.len()
        );
        assert!(
            cut.ends_with("..."),
            "a cut reason has to look cut, or it reads as the whole of what the node said: {cut}"
        );
        assert!(
            cut.bytes().all(|b| b.is_ascii_graphic() || b == b' '),
            "a header value may not hold anything else: {cut}"
        );
    }

    /// A node that states no reason gets none invented for it.
    ///
    /// This is the fleet-upgrade case as much as anything: a node built before
    /// [`REASON_HEADER`] existed stamps two headers and not three, and the
    /// answer has to stay honest about what it was not told.
    ///
    /// The guard is held across the awaits on purpose: it is what keeps two
    /// tests off the shared isolate host at once, which is exactly this span.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_node_that_states_no_reason_has_none_made_up_for_it() {
        use crate::app::{test_host, AppConfig, AppKey};

        let _turn = test_host::take();
        let apps = test_host::host();
        let key = AppKey::production("system-probe-no-reason");
        apps.load_app(
            key.clone(),
            include_str!("dispatcher/system_app.js").to_string(),
            AppConfig::default(),
            None,
        )
        .await
        .expect("the system application loads");

        let (status, headers, body) = apps
            .execute_app(
                &key,
                "http://node.invalid/health".to_string(),
                "GET".to_string(),
                vec![
                    (READINESS_HEADER.to_string(), "degraded".to_string()),
                    (SERVING_APPS_HEADER.to_string(), "2".to_string()),
                ],
                None,
            )
            .await
            .expect("the probe reaches the handler");
        let body = String::from_utf8(body.into_bytes().unwrap()).unwrap();

        assert_eq!(status, 200, "{body}");
        assert_eq!(
            crate::runtime::header_list::first_value(&headers, READINESS_HEADER),
            Some("degraded"),
            "{body}"
        );
        assert!(
            body.contains("did not say why"),
            "an answer that was told no reason has to say so: {body}"
        );
        assert!(
            body.contains("\"reason\":null"),
            "null is the fact that nothing was stated, which no invented reason can stand in \
             for: {body}"
        );

        let _ = apps.unload_app(&key).await;
    }

    /// A count that arrives between two reconcile passes moves the answer and
    /// leaves the reason alone.
    ///
    /// Both directions matter and for the same reason: the count is what the
    /// edge's probe turns into 200 or 503 for a degraded node. A node that has
    /// just stopped the last thing it had must stop claiming it can serve
    /// before the next pass, and one that has just taken delivery must stop
    /// claiming it cannot.
    #[test]
    fn a_push_or_an_unload_moves_the_count_and_keeps_the_reason() {
        let health = NodeHealth::starting();
        health.mark_degraded("control plane unreachable: connection refused", 2);

        health.note_serving(3);
        assert_eq!(
            health.snapshot(),
            Readiness::Degraded {
                reason: "control plane unreachable: connection refused".to_string(),
                apps: 3,
            },
            "taking delivery of one more application says nothing about why the node is degraded"
        );

        health.note_serving(0);
        assert_eq!(
            health.snapshot(),
            Readiness::Degraded {
                reason: "control plane unreachable: connection refused".to_string(),
                apps: 0,
            },
            "a degraded node with nothing left loaded fails every request it is given, and the \
             probe answers 503 off this count"
        );

        health.mark_ready(1);
        health.note_serving(2);
        assert_eq!(
            health.snapshot(),
            Readiness::Ready { apps: 2 },
            "a node running what it was told to is still running what it was told to when one \
             more version arrives"
        );
    }

    /// Nothing that happens between passes lifts a node out of `Starting`.
    ///
    /// `Starting` keeps a node out of rotation entirely, and `Degraded` with
    /// applications loaded does not - so if a refusal or a stray count could
    /// move it, a *failure* would be the thing that put a node which has never
    /// seen an assignment list into the edge's rotation.
    #[test]
    fn nothing_between_passes_lifts_a_node_that_is_still_starting() {
        let health = NodeHealth::starting();

        health.note_serving(4);
        assert_eq!(
            health.snapshot(),
            Readiness::Starting,
            "a node still working through its first assignment list has not been told what it \
             should be running, so no count of it is an answer"
        );

        health.note_refusal(
            "demo.production was pushed and this node did not take it",
            4,
        );
        assert_eq!(
            health.snapshot(),
            Readiness::Starting,
            "a refusal must never be what promotes a node into rotation"
        );

        // The first pass is still what decides, and it still can.
        health.mark_ready(4);
        assert!(health.is_healthy());
    }

    #[test]
    fn recovering_contact_with_the_control_plane_clears_the_degraded_state() {
        let health = NodeHealth::starting();
        health.mark_degraded("control plane unreachable", 1);
        health.mark_ready(1);
        assert_eq!(health.snapshot(), Readiness::Ready { apps: 1 });
    }
}
