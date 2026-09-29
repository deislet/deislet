//! The catalog's routing table, and how it gets here.
//!
//! # The problem this closes
//!
//! Two things used to answer "which application does this request belong
//! to". The control plane's catalog held routes an operator created through
//! the API or the dashboard, and deis-proxy matched hosts out of its own
//! application list. Nothing joined them: the routes an operator edited were
//! read by nobody, and the table that decided live traffic could only be
//! changed by editing the proxy's own configuration. This module makes the
//! catalog the source of truth for that question.
//!
//! The local application list is still here and still matters, but for a
//! different question: it carries certificates, the cache policy and the
//! enabled flag - what to *do* with a request once the application is known.
//! When both can answer "which application", the catalog wins, because it is
//! the one an operator can change without touching a machine. See
//! `proxy::resolve_app` for the exact order.
//!
//! # Poll, not push
//!
//! deis-proxy asks the control plane for the whole table every
//! `routing.control_poll_interval_secs` seconds, over the control plane's
//! machine channel, as `deis.machine.MachineChannel/FetchRoutingTable`,
//! proving a certificate that names this proxy alone. There is one way and it
//! is that one: the HTTP route this used to be fetched from, and the
//! fleet-wide value that opened it, are both gone - see [`client`].
//!
//! The alternatives were a push down the gRPC channel that is already open,
//! and reading the control plane's SQLite file directly. Both were rejected:
//!
//! * **Push** delivers a change quickly and delivers nothing else. A proxy
//!   that starts while the control plane is down has no table and no way to
//!   ask for one; a proxy that misses one message because its channel
//!   dropped mid-push stays wrong until the next unrelated change happens to
//!   arrive. Every push design ends up needing a periodic full resync to be
//!   trustworthy, at which point the resync is the mechanism and the push is
//!   an optimisation. The catalog changes a few times a day on a private
//!   platform; a few seconds of delay is not worth a second delivery path
//!   that can silently be wrong.
//! * **Sharing the store** would tie the data plane to the control plane's
//!   SQLite file and therefore to its filesystem, which forbids the ordinary
//!   deployment where the edge and the control plane are different machines.
//!
//! Polling is also self-repairing in the direction that matters: whatever
//! went wrong - a dropped connection, a control plane restart, a table that
//! failed validation once - the next poll fixes it without anyone noticing.
//!
//! # When the control plane is down
//!
//! **The last table this process accepted stays in force, indefinitely.** A
//! failed poll changes nothing: it logs at `error`, says which table is
//! still routing and how old it is, and the next poll tries again. Traffic
//! keeps being served exactly as it was a second earlier.
//!
//! This is not a detail, it is the point. A data plane that starts refusing
//! traffic because its control plane is unreachable turns a management
//! outage into a customer-visible one, and the control plane is the process
//! most likely to be restarted for an upgrade. The only thing an operator
//! loses while it is down is the ability to *change* routing.
//!
//! One case is genuinely different: a proxy that has never once fetched the
//! table has nothing to keep. It says so at `error` on every failed attempt
//! and falls back to the locally configured applications, which is exactly
//! what it did before this module existed.
//!
//! A restart used to be that case. Every table this instance publishes is
//! now written to the file described in [`persist`], and a starting process
//! reads it back before its first poll, so an edge restarted while the
//! control plane is unreachable routes exactly as it did before it stopped.
//! The file never outranks a live answer: a successful poll replaces the
//! table in memory and the copy on disk together.
//!
//! # All or nothing
//!
//! A table that arrives half-written must never be applied - half a routing
//! table sends live requests to the wrong application, which is worse than
//! an old table. So a fetched document is parsed whole, validated whole
//! ([`wire::RoutingTableDocument::validate`]), compiled whole, and only then
//! published with a single pointer swap ([`arc_swap`]). A request either
//! sees the table before or the table after; there is no moment at which it
//! can see part of each. A document with one unusable entry is refused
//! entirely rather than applied with that entry skipped.
//!
//! # Reloads
//!
//! Nothing here needs a restart and nothing accumulates across one. The
//! poller is a single pingora background service, started once and owned by
//! the server, so a zero-downtime upgrade starts exactly one in the new
//! process and the old one goes away with it. Each poll opens one connection
//! and drops it; nothing is pooled and no task is spawned per table.

pub mod client;
#[cfg(test)]
mod mock;
pub mod persist;
pub mod service;
pub mod table;
pub mod wire;

pub use client::{CatalogClient, FetchError, routing_table_client};
pub use persist::{CacheError, RoutingTableCache, StoredRoutingTable};
pub use service::CatalogPoller;
pub use table::{CatalogRoutes, Match, MatchSource, PreviewZone, Target};
pub use wire::RoutingTableDocument;
