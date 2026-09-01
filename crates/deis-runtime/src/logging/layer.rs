//! Turns `target: "app"` tracing events into buffered [`LogEntry`] values.
//!
//! # Why the fields matter
//!
//! deis-telemetry stores one application dimension, `app_id`, and every query
//! (`deis tail`, the log panel) filters on it. An entry that arrives with an
//! empty `app_id` is still stored, but nothing can ever find it again: it is
//! invisible to every per-application query while looking, from the shipper's
//! side, like a successful delivery. So the layer reads `app_id` off the event
//! rather than leaving the column blank, and keeps the remaining structured
//! fields instead of discarding them.
//!
//! # Where the environment goes
//!
//! Inside this node an application is the pair (application, environment),
//! and every isolate logs that pair as its `app_id` field - `demo.production`.
//! deis-telemetry is not partitioned by tenant the way deis-store is; it has
//! no isolation to protect, and asking it for "this application's logs" has
//! to answer with every environment's. So the pair is split here, on the way
//! out: the application goes in `app_id` and the environment becomes an
//! ordinary structured field. Shipping the joined string instead would make
//! a query for `demo` return nothing at all, which reads exactly like an app
//! that never logged.
//!
//! # Which invocation a line belongs to
//!
//! `trace_id` is lifted the same way and for the same kind of reason. It is
//! the id of the one request, queue batch or cron tick that wrote the line
//! (see [`crate::runtime::ops::CurrentTrace`]), deis-telemetry keeps it in a
//! column beside `app_id`, and the whole of what it buys is the join: `deis
//! trace` names an invocation and `deis logs --trace-id` answers with the
//! lines that invocation wrote, out of everything the application logged that
//! minute. Left in the generic field map the value would still arrive and the
//! query would still find nothing.

use super::buffer::LogBuffer;
use super::types::{LogEntry, LogLevel};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

/// Event field naming the application an entry belongs to. It is lifted out of
/// the field map into [`LogEntry::app_id`], because that is the only dimension
/// deis-telemetry can filter on.
///
/// An empty value is treated as absent: the wire type spells "no application"
/// as the empty string, so `Some("")` and `None` are the same entry once
/// shipped, and collapsing them here keeps the in-memory form from suggesting
/// a distinction the protocol cannot carry.
const APP_ID_FIELD: &str = "app_id";

/// Field the environment half of a tenant is shipped under, for the events
/// whose `app_id` is a tenant. Named to match the control plane's REST field
/// and the `x-deis-environment` header, so one word means one thing.
const ENVIRONMENT_FIELD: &str = "environment";

/// Event field naming the invocation an entry came from, lifted out of the
/// field map into [`LogEntry::trace_id`].
///
/// A column of its own in deis-telemetry, next to `app_id`, and the only one
/// that can answer "what else did *this* request write". Left in the generic
/// field map it would still be shipped, but `deis logs --trace-id` filters on
/// the column and would match nothing, which reads exactly like an invocation
/// that logged nothing at all.
///
/// An empty value is absent, the same way an empty `app_id` is: the wire type
/// spells "no invocation" as the empty string, and every line a node writes
/// about itself rather than about an invocation carries one.
const TRACE_ID_FIELD: &str = "trace_id";

/// Event field holding the human-readable text, which every `tracing` macro
/// records under this name.
const MESSAGE_FIELD: &str = "message";

pub struct RuntimeLayer {
    buffer: Arc<LogBuffer>,
    node_id: String,
}

impl RuntimeLayer {
    pub fn new(buffer: Arc<LogBuffer>, node_id: String) -> Self {
        Self { buffer, node_id }
    }
}

impl<S> Layer<S> for RuntimeLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        if metadata.target() != "app" {
            return;
        }

        let level = LogLevel::from(*metadata.level());

        let mut visitor = EntryVisitor::default();
        event.record(&mut visitor);

        let message = visitor.message.unwrap_or_default();
        let mut entry = LogEntry::new(self.node_id.clone(), level, message);
        entry.fields = visitor.fields;

        entry.app_id = match visitor.app_id.filter(|id| !id.is_empty()) {
            Some(app_id) => Some(split_tenant_field(&app_id, &mut entry.fields)),
            None => None,
        };
        entry.trace_id = visitor.trace_id.filter(|id| !id.is_empty());

        self.buffer.push(entry);
    }
}

/// Reduce a tenant to the application it names, recording the environment as a
/// field. A value that is not a tenant - a bare application id from an older
/// call site - is kept as it is and adds no field.
///
/// An `environment` the event set itself is left alone: it was written
/// deliberately, and overwriting it would hide what the caller meant to say.
fn split_tenant_field(app_id: &str, fields: &mut HashMap<String, String>) -> String {
    match deis_wire::conventions::split_tenant(app_id) {
        Some((app, environment)) => {
            fields
                .entry(ENVIRONMENT_FIELD.to_string())
                .or_insert_with(|| environment.to_string());
            app.to_string()
        }
        None => app_id.to_string(),
    }
}

/// Splits an event's fields into the four places they belong: the message, the
/// application id, the invocation id, and everything else.
#[derive(Default)]
struct EntryVisitor {
    message: Option<String>,
    app_id: Option<String>,
    trace_id: Option<String>,
    fields: HashMap<String, String>,
}

impl EntryVisitor {
    /// Route one already-stringified field value to its destination.
    fn store(&mut self, name: &str, value: String) {
        match name {
            MESSAGE_FIELD => self.message = Some(value),
            APP_ID_FIELD => self.app_id = Some(value),
            TRACE_ID_FIELD => self.trace_id = Some(value),
            _ => {
                self.fields.insert(name.to_string(), value);
            }
        }
    }
}

impl tracing::field::Visit for EntryVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.store(field.name(), format!("{:?}", value));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.store(field.name(), value.to_string());
    }

    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        self.store(field.name(), value.to_string());
    }
}

#[cfg(test)]
#[path = "layer_test.rs"]
mod layer_test;
