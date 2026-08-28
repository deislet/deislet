use super::buffer::LogBuffer;
use super::types::{LogEntry, LogLevel};
use std::sync::Arc;
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

pub struct RuntimeLayer {
    buffer: Arc<LogBuffer>,
    worker_id: String,
}

impl RuntimeLayer {
    pub fn new(buffer: Arc<LogBuffer>, worker_id: String) -> Self {
        Self { buffer, worker_id }
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

        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);

        let message = visitor.message.unwrap_or_else(|| "".to_string());

        let entry = LogEntry::new(self.worker_id.clone(), level, message);

        self.buffer.push(entry);
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{:?}", value));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        }
    }

    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        }
    }
}
