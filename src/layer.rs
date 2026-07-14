//! `tracing` bridge: a [`Layer`] that turns each `tracing::Event` into a
//! [`Message`] and appends it to the feed log. This is the native path
//! for Rust producers; the `feed` CLI is the bypass for everything else.
//!
//! Only events are bridged here. Spans (the in-progress / "pending" view)
//! are a separate, mutable, cross-process concern and are deliberately
//! not handled by this layer.

use std::fmt;
use std::path::PathBuf;

use chrono::Utc;
use tracing::field::{Field, Visit};
use tracing::{Event, Level as TLevel, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

use crate::usage::{append_usage, default_usage_log_path, Kind, Usage};
use crate::{append, default_log_path, Level, Message};

/// The reserved target for usage (firehose) events. [`FeedLayer`] skips
/// events with this target so they don't double-write into the editorial
/// log; [`UsageLayer`] only handles events with this target.
pub const USAGE_TARGET: &str = "usage";

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
        // Usage events are firehose; they belong in the usage log
        // (handled by `UsageLayer`), not the editorial log.
        if meta.target() == USAGE_TARGET {
            return;
        }
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let msg = Message::new(meta.level().into(), meta.target(), visitor.finish());
        // Best-effort: a feed write failing must never disturb the
        // producer's real work.
        let _ = append(&self.path, &msg);
    }
}

/// A [`Layer`] that peels off `tracing` events with
/// `target = "usage"` (i.e. [`USAGE_TARGET`]) and writes them to the
/// usage log at `path` as [`Usage`] records.
///
/// Sibling to [`FeedLayer`]: the two share one subscriber, split by
/// event target. Any event with a non-`usage` target is ignored by this
/// layer — the routing is exclusive so no event is double-written.
pub struct UsageLayer {
    path: PathBuf,
}

impl UsageLayer {
    /// Route usage events to the given log path.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Route usage events to the conventional [`default_usage_log_path`].
    pub fn at_default_path() -> Self {
        Self::new(default_usage_log_path())
    }
}

impl<S: Subscriber> Layer<S> for UsageLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() != USAGE_TARGET {
            return;
        }
        let mut v = UsageVisitor::default();
        event.record(&mut v);
        let Some(kind) = v.kind else { return };
        let Some(name) = v.name else { return };
        let ev = Usage {
            ts: Utc::now(),
            kind,
            name,
            cwd: v.cwd,
            argv: v.argv,
            args: v.args,
            session: v.session,
        };
        // Best-effort: never disturb the producer.
        let _ = append_usage(&self.path, &ev);
    }
}

/// Extracts usage-event fields from a `tracing::Event`. Fields we care
/// about: `kind` ("cli"/"skill"), `name`, `argv` (formatted list),
/// `args`, `session`, `cwd`.
#[derive(Default)]
struct UsageVisitor {
    kind: Option<Kind>,
    name: Option<String>,
    cwd: Option<String>,
    argv: Option<Vec<String>>,
    args: Option<String>,
    session: Option<String>,
}

impl Visit for UsageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        // `%value` (Display) and `?value` (Debug) both land here rather
        // than in `record_str` — the visitor only sees `record_str` for
        // string-literal field values, not for owned Strings passed via
        // `%`/`?`. Format each and route by field name; strip a single
        // surrounding pair of `"` that Debug-formatting a &str adds.
        let raw = format!("{value:?}");
        let s = strip_debug_quotes(&raw);
        match field.name() {
            "kind" => self.kind = parse_kind(s),
            "name" => self.name = Some(s.to_string()),
            "cwd" => self.cwd = Some(s.to_string()),
            "args" => self.args = Some(s.to_string()),
            "session" => self.session = Some(s.to_string()),
            "argv" => self.argv = parse_debug_vec(s),
            _ => {}
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "kind" => self.kind = parse_kind(value),
            "name" => self.name = Some(value.to_string()),
            "cwd" => self.cwd = Some(value.to_string()),
            "args" => self.args = Some(value.to_string()),
            "session" => self.session = Some(value.to_string()),
            _ => {}
        }
    }
}

/// Debug-formatting a `&str` wraps it in `"…"`. Strip a single leading
/// and trailing quote if both are present so field values round-trip.
fn strip_debug_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn parse_kind(s: &str) -> Option<Kind> {
    match s {
        "cli" => Some(Kind::Cli),
        "skill" => Some(Kind::Skill),
        _ => None,
    }
}

/// Parse the Debug-formatted `Vec<String>` representation `tracing` emits
/// for a `?vec` field. Best-effort — anything unparseable returns None.
///
/// Uses `serde_json` since a `Vec<String>` Debug-formatted looks like
/// JSON when the strings contain no unusual characters: `["a", "b"]`.
/// For argv this holds in the common case.
fn parse_debug_vec(s: &str) -> Option<Vec<String>> {
    serde_json::from_str::<Vec<String>>(s).ok()
}

/// Emit a CLI-invocation usage event. Best-effort; never panics or
/// fails.
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
/// The event carries the binary `name`, the current `argv` (minus
/// argv[0]), and the current cwd. Uses a `tracing::trace!` under the
/// `usage` target, so [`UsageLayer`] picks it up and [`FeedLayer`]
/// skips it.
pub fn log_cli_invocation(name: &str) {
    let argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    // Serialize argv here so the visitor can re-parse it — sidesteps
    // the pain of routing a Vec through tracing's field-value system.
    let argv_debug = serde_json::to_string(&argv).unwrap_or_else(|_| "[]".to_string());
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    tracing::trace!(
        target: USAGE_TARGET,
        kind = "cli",
        name = name,
        argv = %argv_debug,
        cwd = %cwd,
    );
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

/// Install a global subscriber with two layers:
/// - [`FeedLayer`] — editorial events at [`default_log_path`]
/// - [`UsageLayer`] — firehose usage events at [`default_usage_log_path`]
///
/// Honors a `FEED_LOG` env filter. Default filter admits `info` and above
/// for every target *plus* `trace` events on the `usage` target — so
/// editorial reads stay at `info` (unchanged) while usage-firehose events
/// are captured without needing `RUST_LOG=trace` gymnastics from the
/// caller. Call once from a producer's `main()`.
///
/// Best-effort and idempotent-ish: a second call (or a pre-existing
/// global subscriber) is ignored rather than panicking, so wiring this
/// into a short-lived CLI never takes the process down.
pub fn init() {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    // `info,usage=trace`: default level `info`, but always keep `trace`
    // on the `usage` target so `log_cli_invocation` (which emits at
    // trace) is captured without env-var ceremony.
    let filter = EnvFilter::try_from_env("FEED_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,usage=trace"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(FeedLayer::at_default_path())
        .with(UsageLayer::at_default_path())
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
    fn feedlayer_skips_usage_events() {
        // Usage events must NOT double-write into the editorial log,
        // even though FeedLayer sees every event the subscriber routes.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("feed.log");
        let subscriber =
            tracing_subscriber::registry().with(FeedLayer::new(path.clone()));
        with_default(subscriber, || {
            tracing::info!(target: "task", "archived xyz");
            tracing::trace!(target: USAGE_TARGET, kind = "cli", name = "task", "usage");
        });
        let got = tail(&path, 10);
        assert_eq!(got.len(), 1, "usage event leaked into feed.log");
        assert_eq!(got[0].target, "task");
    }

    #[test]
    fn usagelayer_writes_cli_event() {
        use crate::usage::{read_all_usage, Kind};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.log");
        let filter = tracing_subscriber::EnvFilter::new("usage=trace");
        let subscriber = tracing_subscriber::registry()
            .with(filter)
            .with(UsageLayer::new(path.clone()));
        with_default(subscriber, || {
            tracing::trace!(
                target: USAGE_TARGET,
                kind = "cli",
                name = "task",
                argv = %r#"["log","-m","hi"]"#,
                cwd = %"/tmp/wd",
            );
        });
        let got = read_all_usage(&path);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, Kind::Cli);
        assert_eq!(got[0].name, "task");
        assert_eq!(
            got[0].argv,
            Some(vec!["log".into(), "-m".into(), "hi".into()])
        );
        assert_eq!(got[0].cwd.as_deref(), Some("/tmp/wd"));
    }

    #[test]
    fn usagelayer_writes_skill_event() {
        use crate::usage::{read_all_usage, Kind};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.log");
        let filter = tracing_subscriber::EnvFilter::new("usage=trace");
        let subscriber = tracing_subscriber::registry()
            .with(filter)
            .with(UsageLayer::new(path.clone()));
        with_default(subscriber, || {
            tracing::trace!(
                target: USAGE_TARGET,
                kind = "skill",
                name = "system-registry",
                args = %"list",
                session = %"abc-123",
            );
        });
        let got = read_all_usage(&path);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, Kind::Skill);
        assert_eq!(got[0].name, "system-registry");
        assert_eq!(got[0].args.as_deref(), Some("list"));
        assert_eq!(got[0].session.as_deref(), Some("abc-123"));
        assert!(got[0].argv.is_none());
    }

    #[test]
    fn usagelayer_ignores_non_usage_events() {
        // Symmetric to `feedlayer_skips_usage_events`: an event on some
        // other target must not accidentally land in the usage log.
        use crate::usage::read_all_usage;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.log");
        let subscriber =
            tracing_subscriber::registry().with(UsageLayer::new(path.clone()));
        with_default(subscriber, || {
            tracing::info!(target: "task", "archived xyz");
        });
        assert!(read_all_usage(&path).is_empty());
    }

    #[test]
    fn usagelayer_drops_events_missing_required_fields() {
        // A usage event with no `kind` or no `name` is malformed —
        // silently dropped rather than written as a partial record.
        use crate::usage::read_all_usage;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.log");
        let filter = tracing_subscriber::EnvFilter::new("usage=trace");
        let subscriber = tracing_subscriber::registry()
            .with(filter)
            .with(UsageLayer::new(path.clone()));
        with_default(subscriber, || {
            tracing::trace!(target: USAGE_TARGET, name = "task", "no kind");
            tracing::trace!(target: USAGE_TARGET, kind = "cli", "no name");
        });
        assert!(read_all_usage(&path).is_empty());
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
