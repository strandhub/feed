//! `tracing` bridge: a [`Layer`] that turns each `tracing::Event` into a
//! [`Message`] and appends it to the feed log. This is the native path
//! for Rust producers; the `feed` CLI is the bypass for everything else.
//!
//! Only events are bridged here. Spans (the in-progress / "pending" view)
//! are a separate, mutable, cross-process concern and are deliberately
//! not handled by this layer.

use std::fmt;
use std::path::PathBuf;

use tracing::field::{Field, Visit};
use tracing::{Event, Level as TLevel, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

use crate::{append, default_log_path, Level, Message};

impl From<&TLevel> for Level {
    fn from(l: &TLevel) -> Self {
        match *l {
            TLevel::TRACE => Level::Trace,
            TLevel::DEBUG => Level::Debug,
            TLevel::INFO => Level::Info,
            TLevel::WARN => Level::Warn,
            TLevel::ERROR => Level::Error,
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
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let msg = Message::new(meta.level().into(), meta.target(), visitor.finish());
        // Best-effort: a feed write failing must never disturb the
        // producer's real work.
        let _ = append(&self.path, &msg);
    }
}

/// Pulls the human-readable text out of an event. `tracing`'s conventional
/// message lives in the `message` field; other fields are appended as
/// `key=value` so structured context isn't silently dropped.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    extras: Vec<String>,
}

impl MessageVisitor {
    fn finish(mut self) -> String {
        if !self.extras.is_empty() {
            if !self.message.is_empty() {
                self.message.push(' ');
            }
            self.message.push('(');
            self.message.push_str(&self.extras.join(", "));
            self.message.push(')');
        }
        self.message
    }
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
            // Debug-formatting a &str wraps it in quotes; strip them.
            if self.message.starts_with('"') && self.message.ends_with('"') {
                self.message = self.message[1..self.message.len() - 1].to_string();
            }
        } else {
            self.extras.push(format!("{}={:?}", field.name(), value));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.extras.push(format!("{}={}", field.name(), value));
        }
    }
}

/// Install a global subscriber that bridges `tracing` events to the feed
/// log (at [`default_log_path`]), honoring a `FEED_LOG` env filter
/// (defaults to `info`). Call once from a producer's `main()`.
///
/// Best-effort and idempotent-ish: a second call (or a pre-existing global
/// subscriber) is ignored rather than panicking, so wiring this into a
/// short-lived CLI never takes the process down.
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
    fn event_becomes_feed_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("feed.log");
        let subscriber =
            tracing_subscriber::registry().with(FeedLayer::new(path.clone()));
        with_default(subscriber, || {
            tracing::info!(target: "task", "archived xyz");
            tracing::error!(target: "deploy", "boom");
        });
        let got = tail(&path, 10);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].level, Level::Info);
        assert_eq!(got[0].target, "task");
        assert_eq!(got[0].message, "archived xyz");
        assert_eq!(got[1].level, Level::Error);
        assert_eq!(got[1].target, "deploy");
    }

    #[test]
    fn extra_fields_are_appended() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("feed.log");
        let subscriber =
            tracing_subscriber::registry().with(FeedLayer::new(path.clone()));
        with_default(subscriber, || {
            tracing::info!(target: "task", slug = "feed-panel", "archived");
        });
        let got = tail(&path, 10);
        assert_eq!(got.len(), 1);
        assert!(
            got[0].message.contains("archived") && got[0].message.contains("slug=feed-panel"),
            "got: {}",
            got[0].message
        );
    }
}
