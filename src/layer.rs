//! `tracing` bridge: a [`Layer`] that turns each `tracing::Event` into a
//! [`LogRecord`] and appends it to the feed log. This is the native path
//! for Rust producers; the `feed` CLI is the bypass for everything else.
//!
//! Only events are bridged here. Spans (the in-progress / "pending" view)
//! are a separate, mutable, cross-process concern and are deliberately
//! not handled by this layer.
//!
//! ## Field routing
//!
//! - The event's `target` (from `tracing::info!(target: "task", …)`)
//!   lands in `attributes.source` — OTel's `service.name` shorthand.
//! - A `message` field (the conventional format-string content) becomes
//!   the record's `body`.
//! - An `event_name` field lifts to the top-level [`LogRecord::event_name`]
//!   OTel Event marker; use for invocation records and other named
//!   occurrences.
//! - Every other field is captured into `attributes` — strings and
//!   numbers land as their native JSON type, everything else stringifies.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde_json::Value;
use tracing::field::{Field, Visit};
use tracing::{Event, Level as TLevel, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

use crate::{append, default_log_path, LogRecord, SeverityNumber};

impl From<&TLevel> for SeverityNumber {
    fn from(l: &TLevel) -> Self {
        match *l {
            TLevel::TRACE => SeverityNumber::Trace,
            TLevel::DEBUG => SeverityNumber::Debug,
            TLevel::INFO => SeverityNumber::Info,
            TLevel::WARN => SeverityNumber::Warn,
            TLevel::ERROR => SeverityNumber::Error,
        }
    }
}

/// A [`Layer`] that appends every event to the feed log at `path`.
pub struct FeedLayer {
    path: PathBuf,
}

impl FeedLayer {
    /// Bridge to the given log path.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Bridge to the conventional [`default_log_path`].
    pub fn at_default_path() -> Self {
        Self::new(default_log_path())
    }
}

impl<S: Subscriber> Layer<S> for FeedLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = RecordVisitor::default();
        event.record(&mut visitor);
        let mut record = LogRecord::new(meta.level().into(), meta.target(), visitor.body);
        if let Some(name) = visitor.event_name {
            record.event_name = Some(name);
        }
        // Merge any extra fields into attributes (`source` already set by
        // LogRecord::new; extras won't overwrite it unless the producer
        // explicitly emits a `source` field, which is legal).
        for (k, v) in visitor.attributes {
            record.attributes.insert(k, v);
        }
        // Best-effort: a feed write failing must never disturb the
        // producer's real work.
        let _ = append(&self.path, &record);
    }
}

/// Pulls fields off a `tracing::Event`. `message` → body; `event_name` →
/// the OTel Event marker; everything else → attributes.
#[derive(Default)]
struct RecordVisitor {
    body: String,
    event_name: Option<String>,
    attributes: BTreeMap<String, Value>,
}

impl Visit for RecordVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => self.body = value.to_string(),
            "event_name" => self.event_name = Some(value.to_string()),
            name => {
                self.attributes
                    .insert(name.to_string(), Value::String(value.to_string()));
            }
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.attributes
            .insert(field.name().to_string(), Value::Bool(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.attributes
            .insert(field.name().to_string(), Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.attributes
            .insert(field.name().to_string(), Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.attributes.insert(
            field.name().to_string(),
            serde_json::Number::from_f64(value)
                .map(Value::Number)
                .unwrap_or(Value::Null),
        );
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        // Debug-formatting a `&str` wraps it in `"…"`. `tracing` routes
        // `%value` (Display) and `?value` (Debug) here rather than to
        // `record_str` for owned Strings, so strip the outer quotes to
        // keep field values readable.
        let raw = format!("{value:?}");
        let s = strip_debug_quotes(&raw).to_string();
        match field.name() {
            "message" => self.body = s,
            "event_name" => self.event_name = Some(s),
            name => {
                self.attributes.insert(name.to_string(), Value::String(s));
            }
        }
    }
}

fn strip_debug_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Emit a CLI-invocation Event at INFO — a valid OTel LogRecord+Event
/// with `event_name: "<cli>.invoked"`, carrying `cli_name`, `argv`, and
/// `cwd` in attributes. Best-effort; never panics or fails.
///
/// Call once at the top of a Rust CLI's `main()`, after [`init`]:
///
/// ```no_run
/// fn main() {
///     feed::init();
///     feed::log_cli_invocation("task");
///     // ... rest of main
/// }
/// ```
///
/// Severity is INFO because a CLI actually running is a real event —
/// producer-honest. Consumers that don't care about invocation records
/// filter by `event_name` matching `*.invoked`, not by severity.
pub fn log_cli_invocation(name: &str) {
    let argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    let argv_str = serde_json::to_string(&argv).unwrap_or_else(|_| "[]".to_string());
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let event_name = format!("{name}.invoked");
    // `target` on `tracing::info!` must be a `&'static str` (baked into
    // the callsite), so we use a fixed target and pass the runtime
    // producer identity as a `source` field — the visitor merges it
    // into attributes, overriding the target-derived default.
    tracing::info!(
        target: "cli",
        source = %name,
        event_name = %event_name,
        cli_name = %name,
        argv = %argv_str,
        cwd = %cwd,
        "{name} invoked",
    );
}

/// Install a global subscriber with [`FeedLayer`] at [`default_log_path`].
///
/// Honors a `FEED_LOG` env filter, defaulting to `info` — producers
/// emit at their honest severity; consumers apply their own filter
/// recipes over the resulting records. Call once from a producer's
/// `main()`.
///
/// Best-effort and idempotent-ish: a second call (or a pre-existing
/// global subscriber) is ignored rather than panicking, so wiring this
/// into a short-lived CLI never takes the process down.
pub fn init() {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_env("FEED_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(FeedLayer::at_default_path())
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tail;
    use tracing::subscriber::with_default;
    use tracing_subscriber::prelude::*;

    #[test]
    fn event_becomes_feed_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("feed.log");
        let subscriber = tracing_subscriber::registry().with(FeedLayer::new(path.clone()));
        with_default(subscriber, || {
            tracing::info!(target: "task", "archived xyz");
            tracing::error!(target: "deploy", "boom");
        });
        let got = tail(&path, 10);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].severity_number, SeverityNumber::Info);
        assert_eq!(got[0].source(), "task");
        assert_eq!(got[0].body, "archived xyz");
        assert_eq!(got[1].severity_number, SeverityNumber::Error);
        assert_eq!(got[1].source(), "deploy");
    }

    #[test]
    fn event_name_field_lifts_to_top_level() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("feed.log");
        let subscriber = tracing_subscriber::registry().with(FeedLayer::new(path.clone()));
        with_default(subscriber, || {
            tracing::info!(
                target: "task",
                event_name = "task.invoked",
                cli_name = "task",
                "task invoked",
            );
        });
        let got = tail(&path, 10);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].event_name.as_deref(), Some("task.invoked"));
        // `event_name` does NOT leak into attributes; only source and the
        // remaining fields do.
        assert!(!got[0].attributes.contains_key("event_name"));
        assert_eq!(
            got[0].attributes.get("cli_name").and_then(|v| v.as_str()),
            Some("task")
        );
    }

    #[test]
    fn extra_fields_land_in_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("feed.log");
        let subscriber = tracing_subscriber::registry().with(FeedLayer::new(path.clone()));
        with_default(subscriber, || {
            tracing::info!(target: "task", slug = "feed-panel", count = 42_i64, "archived");
        });
        let got = tail(&path, 10);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "archived");
        assert_eq!(
            got[0].attributes.get("slug").and_then(|v| v.as_str()),
            Some("feed-panel")
        );
        assert_eq!(
            got[0].attributes.get("count").and_then(|v| v.as_i64()),
            Some(42)
        );
    }

    #[test]
    fn log_cli_invocation_produces_event_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("feed.log");
        let subscriber = tracing_subscriber::registry().with(FeedLayer::new(path.clone()));
        with_default(subscriber, || {
            log_cli_invocation("task");
        });
        let got = tail(&path, 10);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].severity_number, SeverityNumber::Info);
        assert_eq!(got[0].event_name.as_deref(), Some("task.invoked"));
        assert_eq!(got[0].source(), "task");
        assert_eq!(
            got[0].attributes.get("cli_name").and_then(|v| v.as_str()),
            Some("task")
        );
        assert!(got[0].attributes.contains_key("argv"));
    }
}
