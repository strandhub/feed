//! `feed` — a tiny append-only activity log shared across workspace tools.
//!
//! A *feeder* (a skill, a CLI, a hook) appends a [`Message`] to a log file
//! as a side effect of doing something noteworthy ("task-archive ran on
//! task-xyz"). A *reader* — currently `claude-overview`'s bottom panel —
//! tails the log and renders the last N messages.
//!
//! This crate owns the data only: the on-disk format ([`Message`] as one
//! JSON object per line) and the read/write primitives. It deliberately
//! does NOT own rendering (the consumer styles messages by [`Status`]) or
//! a polling loop (the consumer drives its own redraws).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One feed entry. Serialized as a single JSON line in the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub timestamp: DateTime<Utc>,
    pub status: Status,
    pub message: String,
}

impl Message {
    /// Build a message stamped at the current time.
    pub fn new(status: Status, message: impl Into<String>) -> Self {
        Self {
            timestamp: Utc::now(),
            status,
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

/// Outcome a feeder is reporting. Drives the consumer's color choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Success,
    Error,
    /// In-progress / informational. Neither green nor red.
    Pending,
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

    fn msg(status: Status, text: &str, secs: i64) -> Message {
        Message {
            timestamp: Utc.timestamp_opt(secs, 0).unwrap(),
            status,
            message: text.into(),
        }
    }

    #[test]
    fn append_then_tail_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("feed.log");
        append(&path, &msg(Status::Success, "one", 1)).unwrap();
        append(&path, &msg(Status::Error, "two", 2)).unwrap();
        let got = tail(&path, 10);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].message, "one");
        assert_eq!(got[1].message, "two");
        assert_eq!(got[1].status, Status::Error);
    }

    #[test]
    fn tail_returns_last_n_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("feed.log");
        for i in 1..=5 {
            append(&path, &msg(Status::Pending, &format!("m{i}"), i)).unwrap();
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
        append(&path, &msg(Status::Success, "good", 1)).unwrap();
        // Simulate a partial write / hand-edit.
        std::fs::write(
            &path,
            format!(
                "{}\nnot json\n{}\n",
                serde_json::to_string(&msg(Status::Success, "good", 1)).unwrap(),
                serde_json::to_string(&msg(Status::Error, "also good", 2)).unwrap(),
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
        append(&path, &msg(Status::Success, "hi", 1)).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn status_serializes_lowercase() {
        let line = serde_json::to_string(&msg(Status::Success, "x", 1)).unwrap();
        assert!(line.contains("\"status\":\"success\""), "got: {line}");
    }
}
