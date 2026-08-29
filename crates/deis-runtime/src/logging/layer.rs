//! Turns `target: "app"` tracing events into buffered [`LogEntry`] values.
//!
//! # Why the fields matter
//!
//! deis-telemetry stores one tenant dimension, `app_id`, and every query
//! (`deis tail`, the log panel) filters on it. An entry that arrives with an
//! empty `app_id` is still stored, but nothing can ever find it again: it is
//! invisible to every per-application query while looking, from the shipper's
//! side, like a successful delivery. So the layer reads `app_id` off the event
//! rather than leaving the column blank, and keeps the remaining structured
//! fields instead of discarding them.

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
        entry.app_id = visitor.app_id.filter(|id| !id.is_empty());
        entry.fields = visitor.fields;

        self.buffer.push(entry);
    }
}

/// Splits an event's fields into the three places they belong: the message, the
/// application id, and everything else.
#[derive(Default)]
struct EntryVisitor {
    message: Option<String>,
    app_id: Option<String>,
    fields: HashMap<String, String>,
}

impl EntryVisitor {
    /// Route one already-stringified field value to its destination.
    fn store(&mut self, name: &str, value: String) {
        match name {
            MESSAGE_FIELD => self.message = Some(value),
            APP_ID_FIELD => self.app_id = Some(value),
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
