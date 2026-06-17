//! `feed` — a tiny append-only **event** log shared across workspace tools.
//!
//! A *feeder* (a Rust producer via [`FeedLayer`], or the `feed` CLI as a
//! bypass) appends a [`Message`] to a log file as a side effect of doing
//! something noteworthy ("archived task-xyz"). A *reader* — currently
//! `claude-overview`'s events widget — tails the log and renders the last
//! N messages.
//!
//! Events are immutable points in time, modelled on a `tracing::Event`:
//! a [`Level`] (severity), a `target` (which subsystem), and a message.
//! Outcome is encoded in the level — an error is logged at [`Level::Error`];
//! anything else is a settled/successful event. *In-progress* state is a
//! separate concern (a live span, not an event) and is NOT modelled here.
//!
//! This crate owns the event data only: the on-disk format ([`Message`]
//! as one JSON object per line) and the read/write primitives. It does
//! NOT own rendering (the consumer styles by [`Level`]) or any polling
//! loop. The `tracing`-bridge ([`FeedLayer`]) is behind the `tracing`
//! feature so the core crate and CLI stay dependency-light.

use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[cfg(feature = "tracing")]
mod layer;
#[cfg(feature = "tracing")]
pub use layer::{init, FeedLayer};

/// One feed event. Serialized as a single JSON line in the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub timestamp: DateTime<Utc>,
    pub level: Level,
    /// Which subsystem emitted this (e.g. `task`, `deploy`). Mirrors a
    /// `tracing` event's target; the consumer can color and filter on it.
    #[serde(default)]
    pub target: String,
    pub message: String,
}

impl Message {
    /// Build an event stamped at the current time.
    pub fn new(level: Level, target: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            timestamp: Utc::now(),
            level,
            target: target.into(),
            message: message.into(),
        }
    }

    /// `HH:MM:SS` in local time, for a reader to prefix a feed line with.
    pub fn local_hms(&self) -> String {
        self.timestamp
            .with_timezone(&chrono::Local)
            .format("%H:%M:%S")
            .to_string()
    }
}

/// Severity of an event, mirroring `tracing::Level`. Outcome is encoded
/// here: errors are [`Level::Error`]; a settled success is [`Level::Info`].
///
/// Defined locally (rather than re-exporting `tracing::Level`) so the
/// core crate and the `feed` CLI need nothing from `tracing`. Under the
/// `tracing` feature, `tracing::Level` converts in via `From`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Level::Trace => "trace",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for Level {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "trace" => Ok(Level::Trace),
            "debug" => Ok(Level::Debug),
            "info" => Ok(Level::Info),
            "warn" | "warning" => Ok(Level::Warn),
            "error" | "err" => Ok(Level::Error),
            other => Err(format!("unknown level: {other}")),
        }
    }
}

/// The conventional log location: `~/.cache/claude-status/feed.log`.
///
/// This shares the cache dir `claude-overview` already reads from, so the
/// consumer needs no extra configuration. Location *policy* lives here in
/// one place; the read/write primitives take an explicit path so they
/// stay testable.
pub fn default_log_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache/claude-status/feed.log")
}

/// Append one message to the log at `path`, creating the file (and its
/// parent dir) if absent. Each message is one JSON line.
pub fn append(path: &Path, message: &Message) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating feed dir {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening feed log {}", path.display()))?;
    let line = serde_json::to_string(message).context("serializing feed message")?;
    writeln!(file, "{line}").with_context(|| format!("writing to {}", path.display()))?;
    Ok(())
}

/// Read the last `n` valid messages from the log at `path`, oldest first
/// (so a reader renders them top-to-bottom in chronological order).
///
/// A missing log is not an error — it just means nothing has been fed yet,
/// so this returns an empty vec. Malformed lines (partial writes, hand-
/// edits) are skipped rather than aborting the read.
pub fn tail(path: &Path, n: usize) -> Vec<Message> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut msgs: Vec<Message> = contents
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    if msgs.len() > n {
        msgs.drain(0..msgs.len() - n);
    }
    msgs
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn msg(level: Level, text: &str, secs: i64) -> Message {
        Message {
            timestamp: Utc.timestamp_opt(secs, 0).unwrap(),
            level,
            target: "test".into(),
            message: text.into(),
        }
    }

    #[test]
    fn append_then_tail_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("feed.log");
        append(&path, &msg(Level::Info, "one", 1)).unwrap();
        append(&path, &msg(Level::Error, "two", 2)).unwrap();
        let got = tail(&path, 10);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].message, "one");
        assert_eq!(got[1].message, "two");
        assert_eq!(got[1].level, Level::Error);
        assert_eq!(got[1].target, "test");
    }

    #[test]
    fn tail_returns_last_n_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("feed.log");
        for i in 1..=5 {
            append(&path, &msg(Level::Info, &format!("m{i}"), i)).unwrap();
        }
        let got = tail(&path, 3);
        let texts: Vec<&str> = got.iter().map(|m| m.message.as_str()).collect();
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
        // Simulate a partial write / hand-edit between two good lines.
        std::fs::write(
            &path,
            format!(
                "{}\nnot json\n{}\n",
                serde_json::to_string(&msg(Level::Info, "good", 1)).unwrap(),
                serde_json::to_string(&msg(Level::Error, "also good", 2)).unwrap(),
            ),
        )
        .unwrap();
        let got = tail(&path, 10);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].message, "good");
        assert_eq!(got[1].message, "also good");
    }

    #[test]
    fn append_creates_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/sub/feed.log");
        append(&path, &msg(Level::Info, "hi", 1)).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn level_serializes_lowercase_with_target() {
        let line = serde_json::to_string(&msg(Level::Warn, "x", 1)).unwrap();
        assert!(line.contains("\"level\":\"warn\""), "got: {line}");
        assert!(line.contains("\"target\":\"test\""), "got: {line}");
    }

    #[test]
    fn level_parses_aliases() {
        use std::str::FromStr;
        assert_eq!(Level::from_str("WARN").unwrap(), Level::Warn);
        assert_eq!(Level::from_str("warning").unwrap(), Level::Warn);
        assert_eq!(Level::from_str("err").unwrap(), Level::Error);
        assert!(Level::from_str("nope").is_err());
    }
}
