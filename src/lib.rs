//! `feed` — a tiny append-only **event** log shared across workspace tools.
//!
//! A *feeder* (a Rust producer via [`FeedLayer`], or the `feed` CLI as a
//! bypass) appends a [`LogRecord`] to a log file as a side effect of
//! doing something noteworthy ("archived task-xyz"). A *reader* — for
//! example `claude-overview`'s events widget — tails the log and renders
//! the last N records.
//!
//! Records are OTel [LogRecords][spec], serialized one JSON object per
//! line (the flat `opentelemetry-stdout` convention, not the batched
//! OTLP/JSON `resourceLogs`/`scopeLogs` shape). Every row is a valid
//! OTel LogRecord — we populate a subset (`timestamp`, `severity_number`,
//! `severity_text`, `body`, `event_name`, `attributes`, `trace_id`,
//! `span_id`) and skip the distributed-systems bits (`resource`,
//! `instrumentation_scope`, `trace_flags`) since we run on one machine
//! and write to a file, not a collector.
//!
//! [spec]: https://opentelemetry.io/docs/specs/otel/logs/data-model/
//!
//! **Severity is producer-honest.** A producer picks the severity that
//! reflects the *event's* actual impact, not what any downstream consumer
//! wants filtered. Consumers apply their own filter recipes over
//! [`SeverityNumber`], `event_name`, and attributes. See CLAUDE.md for
//! the full framing.
//!
//! *In-progress* state is a separate, mutable, cross-process concern — a
//! live [`Span`] with an enter → advance → exit lifecycle, not an
//! immutable event. It lives in the [`spans`] module: a distinct on-disk
//! shape (one file per open span) for a distinct lifecycle. The two share
//! only this crate's directory policy.
//!
//! This crate owns the *data* for both: the on-disk formats
//! ([`LogRecord`] as one JSON line in the event log; [`Span`] as one
//! JSON file per span) and their read/write primitives. It does NOT own
//! rendering (the consumer styles by [`SeverityNumber`]) or any polling
//! loop. The `tracing`-bridge ([`FeedLayer`]) is behind the `tracing`
//! feature so the core crate and CLI stay dependency-light.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(feature = "tracing")]
mod layer;
#[cfg(feature = "tracing")]
pub use layer::{init, log_cli_invocation, FeedLayer};

pub mod spans;
pub use spans::Span;

/// One feed record. Serialized as a single JSON line in the log — a
/// valid OTel LogRecord subset (see crate docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecord {
    pub timestamp: DateTime<Utc>,
    /// OTel numeric severity: TRACE=1, DEBUG=5, INFO=9, WARN=13, ERROR=17.
    pub severity_number: SeverityNumber,
    /// OTel severity text — one of `TRACE`/`DEBUG`/`INFO`/`WARN`/`ERROR`.
    pub severity_text: SeverityText,
    /// The human-readable message (OTel `body`).
    pub body: String,
    /// OTel Event marker: when present, this LogRecord is also an Event
    /// with a stable, static name in dotted form (`task.invoked`,
    /// `audit.run.failed`, `skill.loaded`). Dynamic data goes in
    /// [`attributes`](Self::attributes), never in the name. Consumers
    /// filter on `event_name` to separate routine invocations from
    /// noteworthy state changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_name: Option<String>,
    /// Structured data. OTel semantic-convention keys (`source`,
    /// `operation`, `exception.type`, …) plus producer-domain keys as
    /// needed. Empty maps are omitted from the wire.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, Value>,
    /// OTel trace correlation: when this record was emitted while a
    /// [`spans::Span`] was open, this is the span's id (also serves as
    /// the trace id for our single-machine setup). Absent for records
    /// with no span context. Set automatically by [`spans::exit`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// OTel span correlation: the id of the specific span this record
    /// belongs to. In our single-span-per-trace model this equals
    /// `trace_id`; kept as a separate field so future multi-span traces
    /// slot in without a schema change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
}

impl LogRecord {
    /// Build a plain record stamped now — severity, `attributes.source`,
    /// and body. No `event_name` (this is a plain log, not an OTel
    /// Event); use [`event`](Self::event) for that.
    pub fn new(severity: SeverityNumber, source: impl Into<String>, body: impl Into<String>) -> Self {
        let mut attributes = BTreeMap::new();
        attributes.insert("source".to_string(), Value::String(source.into()));
        Self {
            timestamp: Utc::now(),
            severity_number: severity,
            severity_text: severity.into(),
            body: body.into(),
            event_name: None,
            attributes,
            trace_id: None,
            span_id: None,
        }
    }

    /// Build a record that is also an OTel Event — carries a stable
    /// static `event_name` in addition to the fields [`new`](Self::new)
    /// sets. Use for invocation records and other named occurrences.
    pub fn event(
        severity: SeverityNumber,
        source: impl Into<String>,
        event_name: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        let mut r = Self::new(severity, source, body);
        r.event_name = Some(event_name.into());
        r
    }

    /// `HH:MM:SS` in local time, for a reader to prefix a row with.
    pub fn local_hms(&self) -> String {
        self.timestamp
            .with_timezone(&chrono::Local)
            .format("%H:%M:%S")
            .to_string()
    }

    /// True when this record landed within the last `within_ms` of *now* —
    /// the "live" highlight (rule A): a record arriving while you watch
    /// glows briefly and self-expires as it ages, driven by the reader's
    /// tick loop. Uses wall-clock now, so it's inherently time-relative.
    pub fn is_fresh(&self, within_ms: i64) -> bool {
        let age = Utc::now().signed_duration_since(self.timestamp);
        age >= chrono::Duration::zero() && age < chrono::Duration::milliseconds(within_ms)
    }

    /// True when this record is strictly newer than `watermark` — the
    /// "unseen" highlight (rule B): everything that arrived after the last
    /// time the reader was opened. A `None` watermark (never opened
    /// before) means nothing has been seen yet, so every record counts.
    pub fn is_after(&self, watermark: Option<DateTime<Utc>>) -> bool {
        match watermark {
            Some(w) => self.timestamp > w,
            None => true,
        }
    }

    /// The producer identity (`attributes.source`), or `""` if absent.
    /// Convenience for consumers grouping/filtering by producer.
    pub fn source(&self) -> &str {
        self.attributes
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    }
}

/// OTel `SeverityNumber` — the numeric severity attached to every
/// record. We use the five main levels (TRACE=1, DEBUG=5, INFO=9,
/// WARN=13, ERROR=17); the sub-levels the spec defines (2/6/10/14/18,
/// etc.) are legal on the wire but we don't emit them.
///
/// Serialized as a raw integer, matching the OTel data model on-wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(into = "u8", try_from = "u8")]
pub enum SeverityNumber {
    Trace = 1,
    Debug = 5,
    Info = 9,
    Warn = 13,
    Error = 17,
}

impl SeverityNumber {
    /// Best-effort parse from a numeric string (`"9"` → `Info`).
    pub fn from_u8(n: u8) -> Option<Self> {
        match n {
            1 => Some(Self::Trace),
            5 => Some(Self::Debug),
            9 => Some(Self::Info),
            13 => Some(Self::Warn),
            17 => Some(Self::Error),
            _ => None,
        }
    }
}

impl From<SeverityNumber> for u8 {
    fn from(s: SeverityNumber) -> u8 {
        s as u8
    }
}

impl TryFrom<u8> for SeverityNumber {
    type Error = String;
    fn try_from(n: u8) -> std::result::Result<Self, String> {
        Self::from_u8(n).ok_or_else(|| format!("unknown severity_number: {n}"))
    }
}

impl fmt::Display for SeverityNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        })
    }
}

impl std::str::FromStr for SeverityNumber {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "trace" => Ok(Self::Trace),
            "debug" => Ok(Self::Debug),
            "info" => Ok(Self::Info),
            "warn" | "warning" => Ok(Self::Warn),
            "error" | "err" => Ok(Self::Error),
            other => Err(format!("unknown severity: {other}")),
        }
    }
}

/// OTel `SeverityText` — the human-readable severity label. Always
/// derivable from [`SeverityNumber`]; kept on the wire so a jq/grep
/// reader can filter without knowing the numeric mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SeverityText {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl From<SeverityNumber> for SeverityText {
    fn from(n: SeverityNumber) -> Self {
        match n {
            SeverityNumber::Trace => Self::Trace,
            SeverityNumber::Debug => Self::Debug,
            SeverityNumber::Info => Self::Info,
            SeverityNumber::Warn => Self::Warn,
            SeverityNumber::Error => Self::Error,
        }
    }
}

/// The conventional log location: `~/.local/share/claude-status/feed.log`.
///
/// Under `~/.local/share/` (durable data) rather than `~/.cache/`
/// (disposable) — the fleet reads history for regression checks and
/// aggregations, so this isn't cache-shaped. Location *policy* lives
/// here in one place; the read/write primitives take an explicit path
/// so they stay testable.
pub fn default_log_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/share/claude-status/feed.log")
}

/// Append one record to the log at `path`, creating the file (and its
/// parent dir) if absent. Each record is one JSON line.
pub fn append(path: &Path, record: &LogRecord) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating feed dir {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening feed log {}", path.display()))?;
    let line = serde_json::to_string(record).context("serializing feed record")?;
    writeln!(file, "{line}").with_context(|| format!("writing to {}", path.display()))?;
    Ok(())
}

/// Read the last `n` valid records from the log at `path`, oldest first
/// (so a reader renders them top-to-bottom in chronological order).
///
/// A missing log is not an error — it just means nothing has been fed
/// yet, so this returns an empty vec. Malformed lines (partial writes,
/// hand-edits, pre-schema-migration rows) are skipped rather than
/// aborting the read.
pub fn tail(path: &Path, n: usize) -> Vec<LogRecord> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut records: Vec<LogRecord> = contents
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    if records.len() > n {
        records.drain(0..records.len() - n);
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn rec(sev: SeverityNumber, body: &str, secs: i64) -> LogRecord {
        let mut r = LogRecord::new(sev, "test", body);
        r.timestamp = Utc.timestamp_opt(secs, 0).unwrap();
        r
    }

    #[test]
    fn append_then_tail_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("feed.log");
        append(&path, &rec(SeverityNumber::Info, "one", 1)).unwrap();
        append(&path, &rec(SeverityNumber::Error, "two", 2)).unwrap();
        let got = tail(&path, 10);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].body, "one");
        assert_eq!(got[1].body, "two");
        assert_eq!(got[1].severity_number, SeverityNumber::Error);
        assert_eq!(got[1].source(), "test");
    }

    #[test]
    fn tail_returns_last_n_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("feed.log");
        for i in 1..=5 {
            append(&path, &rec(SeverityNumber::Info, &format!("m{i}"), i)).unwrap();
        }
        let got = tail(&path, 3);
        let texts: Vec<&str> = got.iter().map(|r| r.body.as_str()).collect();
        assert_eq!(texts, vec!["m3", "m4", "m5"]);
    }

    #[test]
    fn tail_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.log");
        assert!(tail(&path, 10).is_empty());
    }

    #[test]
    fn tail_skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("feed.log");
        std::fs::write(
            &path,
            format!(
                "{}\nnot json\n{}\n",
                serde_json::to_string(&rec(SeverityNumber::Info, "good", 1)).unwrap(),
                serde_json::to_string(&rec(SeverityNumber::Error, "also good", 2)).unwrap(),
            ),
        )
        .unwrap();
        let got = tail(&path, 10);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].body, "good");
        assert_eq!(got[1].body, "also good");
    }

    #[test]
    fn append_creates_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/sub/feed.log");
        append(&path, &rec(SeverityNumber::Info, "hi", 1)).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn severity_serializes_as_number_and_text() {
        let line = serde_json::to_string(&rec(SeverityNumber::Warn, "x", 1)).unwrap();
        assert!(line.contains("\"severity_number\":13"), "got: {line}");
        assert!(line.contains("\"severity_text\":\"WARN\""), "got: {line}");
    }

    #[test]
    fn source_lands_in_attributes() {
        let line = serde_json::to_string(&rec(SeverityNumber::Info, "x", 1)).unwrap();
        assert!(line.contains("\"attributes\":{\"source\":\"test\"}"), "got: {line}");
    }

    #[test]
    fn event_carries_top_level_event_name() {
        let r = LogRecord::event(SeverityNumber::Info, "task", "task.invoked", "task new foo");
        let line = serde_json::to_string(&r).unwrap();
        assert!(line.contains("\"event_name\":\"task.invoked\""), "got: {line}");
    }

    #[test]
    fn plain_record_omits_event_name() {
        let line = serde_json::to_string(&rec(SeverityNumber::Info, "x", 1)).unwrap();
        assert!(!line.contains("event_name"), "got: {line}");
    }

    #[test]
    fn omits_span_and_trace_ids_when_unset() {
        let line = serde_json::to_string(&rec(SeverityNumber::Info, "x", 1)).unwrap();
        assert!(!line.contains("span_id"), "got: {line}");
        assert!(!line.contains("trace_id"), "got: {line}");
    }

    #[test]
    fn empty_attributes_omitted() {
        let mut r = rec(SeverityNumber::Info, "x", 1);
        r.attributes.clear();
        let line = serde_json::to_string(&r).unwrap();
        assert!(!line.contains("attributes"), "got: {line}");
    }

    #[test]
    fn severity_parses_aliases() {
        use std::str::FromStr;
        assert_eq!(SeverityNumber::from_str("WARN").unwrap(), SeverityNumber::Warn);
        assert_eq!(SeverityNumber::from_str("warning").unwrap(), SeverityNumber::Warn);
        assert_eq!(SeverityNumber::from_str("err").unwrap(), SeverityNumber::Error);
        assert!(SeverityNumber::from_str("nope").is_err());
    }

    #[test]
    fn severity_number_deserializes_from_wire_int() {
        // `severity_number: 13` on the wire round-trips as Warn.
        let line = r#"{"timestamp":"2026-07-14T00:00:00Z","severity_number":13,"severity_text":"WARN","body":"x"}"#;
        let r: LogRecord = serde_json::from_str(line).unwrap();
        assert_eq!(r.severity_number, SeverityNumber::Warn);
        assert_eq!(r.severity_text, SeverityText::Warn);
    }

    #[test]
    fn is_fresh_only_for_recent_records() {
        let now = LogRecord::new(SeverityNumber::Info, "t", "now");
        assert!(now.is_fresh(1500));
        let mut old = LogRecord::new(SeverityNumber::Info, "t", "old");
        old.timestamp = Utc::now() - chrono::Duration::seconds(60);
        assert!(!old.is_fresh(1500));
    }

    #[test]
    fn is_after_compares_against_watermark() {
        let base = Utc.timestamp_opt(1_000, 0).unwrap();
        let mut r = LogRecord::new(SeverityNumber::Info, "t", "x");
        r.timestamp = base;
        assert!(r.is_after(Some(base - chrono::Duration::seconds(1))));
        assert!(!r.is_after(Some(base + chrono::Duration::seconds(1))));
        assert!(!r.is_after(Some(base))); // equal is not strictly after
        assert!(r.is_after(None)); // no watermark → everything is unseen
    }

    #[test]
    fn severity_ordering_matches_otel() {
        // The Ord derive gives us the natural OTel ordering:
        // Trace < Debug < Info < Warn < Error. Consumers filter with
        // `>= Warn`, so this ordering has to hold.
        assert!(SeverityNumber::Trace < SeverityNumber::Debug);
        assert!(SeverityNumber::Debug < SeverityNumber::Info);
        assert!(SeverityNumber::Info < SeverityNumber::Warn);
        assert!(SeverityNumber::Warn < SeverityNumber::Error);
    }
}
