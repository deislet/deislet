use super::*;
use crate::logging::buffer::LogBuffer;
use crate::logging::types::{LogEntry, LogLevel};
use std::sync::Arc;
use tracing_subscriber::prelude::*;

/// Run `emit` with a subscriber that only holds a [`RuntimeLayer`], and return
/// whatever reached the buffer.
fn capture(emit: impl FnOnce()) -> Vec<LogEntry> {
    let buffer = Arc::new(LogBuffer::new(16));
    let subscriber = tracing_subscriber::registry()
        .with(RuntimeLayer::new(buffer.clone(), "node-a".to_string()));
    tracing::subscriber::with_default(subscriber, emit);
    buffer.drain(16)
}

#[test]
fn an_app_event_keeps_the_application_it_came_from() {
    let entries = capture(|| {
        tracing::info!(target: "app", app_id = "demo", "hello");
    });

    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].app_id.as_deref(),
        Some("demo"),
        "without the app id deis-telemetry cannot attribute the line to an application, \
         so a query by app_id returns nothing"
    );
    assert_eq!(entries[0].message, "hello");
    assert_eq!(entries[0].level, LogLevel::Info);
    assert_eq!(entries[0].source, "node-a");
}

#[test]
fn an_app_id_recorded_by_display_is_kept_too() {
    let app = "demo".to_string();
    let entries = capture(|| {
        tracing::warn!(target: "app", app_id = %app, "careful");
    });

    assert_eq!(entries[0].app_id.as_deref(), Some("demo"));
}

#[test]
fn other_structured_fields_survive_as_fields() {
    let entries = capture(|| {
        tracing::info!(target: "app", app_id = "demo", op = "op_kv_get", key = "k1", "did a thing");
    });

    assert_eq!(entries[0].message, "did a thing");
    assert_eq!(
        entries[0].fields.get("op").map(String::as_str),
        Some("op_kv_get")
    );
    assert_eq!(entries[0].fields.get("key").map(String::as_str), Some("k1"));
    assert!(
        !entries[0].fields.contains_key("app_id"),
        "app_id has its own column on the wire; duplicating it into fields would ship it twice"
    );
    assert!(!entries[0].fields.contains_key("message"));
}

#[test]
fn an_event_without_an_app_id_still_ships() {
    let entries = capture(|| {
        tracing::error!(target: "app", "node level trouble");
    });

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].app_id, None);
    assert_eq!(entries[0].message, "node level trouble");
}

#[test]
fn events_from_other_targets_are_ignored() {
    let entries = capture(|| {
        tracing::info!("platform chatter");
    });

    assert!(entries.is_empty());
}

#[test]
fn an_empty_app_id_counts_as_no_application() {
    let entries = capture(|| {
        tracing::info!(target: "app", app_id = "", "no tenant here");
    });

    assert_eq!(
        entries[0].app_id, None,
        "the wire type spells \"no application\" as the empty string, so an empty \
         field must not look like a real tenant in memory"
    );
}

/// The isolates log the tenant - application and environment joined - because
/// that is what identifies a running application on this node. deis-telemetry
/// has one application dimension and no environment dimension, so a query for
/// "demo" has to find these lines; shipping "demo.production" in that column
/// would make the application look silent.
#[test]
fn a_tenant_is_shipped_as_an_application_and_an_environment_field() {
    let entries = capture(|| {
        tracing::info!(target: "app", app_id = "demo.preview", "greeted world");
    });

    assert_eq!(entries[0].app_id.as_deref(), Some("demo"));
    assert_eq!(
        entries[0].fields.get("environment").map(String::as_str),
        Some("preview")
    );
}

#[test]
fn a_bare_application_id_is_left_alone() {
    let entries = capture(|| {
        tracing::info!(target: "app", app_id = "demo", "greeted world");
    });

    assert_eq!(entries[0].app_id.as_deref(), Some("demo"));
    assert!(!entries[0].fields.contains_key("environment"));
}

#[test]
fn an_environment_the_event_wrote_itself_wins() {
    let entries = capture(|| {
        tracing::info!(
            target: "app",
            app_id = "demo.production",
            environment = "staging",
            "greeted world"
        );
    });

    assert_eq!(entries[0].app_id.as_deref(), Some("demo"));
    assert_eq!(
        entries[0].fields.get("environment").map(String::as_str),
        Some("staging"),
        "the value the caller recorded must not be overwritten"
    );
}
