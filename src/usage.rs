//! Usage events — a firehose sibling to editorial events, routed to a
//! distinct sink.
//!
//! Where a [`Message`](crate::Message) is *editorial* (a producer decided
//! this side-effect was worth surfacing), a [`Usage`] is *firehose*: one
//! record per CLI invocation, one per Claude `Skill` load, no editorial
//! voice. Distinct on-disk log at [`default_usage_log_path`] so consumer
//! surfaces (e.g. `claude-overview`'s events widget) reading feed's
//! editorial log stay unpolluted.
//!
//! Written by two paths:
//! - **Rust CLIs** — [`log_cli_invocation`](crate::log_cli_invocation) at
//!   the top of `main()` emits a `tracing::trace!(target = "usage", …)`
//!   which the [`UsageLayer`](crate::UsageLayer) picks up.
//! - **The Claude `Skill` tool** — a `PreToolUse` hook shells out to
//!   `feed usage --kind skill …`, which writes the same on-disk shape.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The conventional usage log location: `~/.local/share/claude/tool-usage.log`.
///
/// Under `~/.local/share/` (not `~/.cache/`, where feed's editorial log
/// lives) because usage metrics are durable data whose whole point is
/// being queryable weeks later, not disposable cache. Location *policy*
/// lives here in one place; the read/write primitives take an explicit
/// path so they stay testable.
pub fn default_usage_log_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/share/claude/tool-usage.log")
}

/// One usage event. Serialized as a single JSON line in the usage log.
///
/// Kind-specific fields are absent (not `null`) via
/// `skip_serializing_if = "Option::is_none"` — a `cli` event has no
/// `args`/`session`; a `skill` event has no `argv`. Keeps the wire
/// shape tight and consumer parsing unambiguous.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub ts: DateTime<Utc>,
    pub kind: Kind,
    /// For `cli`: the binary name (e.g. `task`, `system-registry`).
    /// For `skill`: the skill name (e.g. `retro`, `system-registry`).
    pub name: String,
    /// Best-effort: cwd at invocation time. Present for both kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// CLI events only: argv without argv[0]. Each element is truncated
    /// at [`ARGV_ELEMENT_MAX`] to keep lines under POSIX `PIPE_BUF`
    /// (4 KiB) so cross-process appends stay atomic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    /// Skill events only: the free-form `args` string from the `Skill`
    /// tool payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    /// Skill events only: the Claude Code session UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// What flavor of tool this event describes. Closed vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Cli,
    Skill,
}

/// Per-argv-element byte cap. Each element is truncated to this many
/// bytes so a single [`Usage`] line stays comfortably under the
/// POSIX `PIPE_BUF` = 4 KiB threshold that `O_APPEND` guarantees
/// atomic writes below. A `task log -m "<long paragraph>"` invocation
/// otherwise would tear across concurrent writers.
pub const ARGV_ELEMENT_MAX: usize = 512;

impl Usage {
    /// Build a CLI event stamped at the current time, with cwd captured
    /// best-effort. Each argv element is truncated to [`ARGV_ELEMENT_MAX`]
    /// bytes; a trailing `…` marker signals truncation.
    pub fn cli(name: impl Into<String>, argv: Vec<String>) -> Self {
        Self {
            ts: Utc::now(),
            kind: Kind::Cli,
            name: name.into(),
            cwd: std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned()),
            argv: Some(argv.into_iter().map(truncate_argv_element).collect()),
            args: None,
            session: None,
        }
    }
}

fn truncate_argv_element(s: String) -> String {
    if s.len() <= ARGV_ELEMENT_MAX {
        return s;
    }
    // Slice at a UTF-8 boundary <= the byte cap, then append the marker.
    let mut end = ARGV_ELEMENT_MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = String::with_capacity(end + 3);
    truncated.push_str(&s[..end]);
    truncated.push('…');
    truncated
}

/// Append one usage event to the log at `path`, creating the file (and
/// its parent dir) if absent. Each event is one JSON line.
pub fn append_usage(path: &Path, event: &Usage) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating usage log dir {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening usage log {}", path.display()))?;
    let line = serde_json::to_string(event).context("serializing usage event")?;
    writeln!(file, "{line}").with_context(|| format!("writing to {}", path.display()))?;
    Ok(())
}

/// Read every valid event from the usage log at `path`. Missing log →
/// empty vec. Malformed lines are skipped rather than aborting the read
/// (partial writes, hand-edits).
pub fn read_all_usage(path: &Path) -> Vec<Usage> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_then_read_roundtrips_cli() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.log");
        let ev = Usage::cli("task", vec!["log".into(), "-m".into(), "hi".into()]);
        append_usage(&path, &ev).unwrap();
        let got = read_all_usage(&path);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, Kind::Cli);
        assert_eq!(got[0].name, "task");
        assert_eq!(
            got[0].argv,
            Some(vec!["log".into(), "-m".into(), "hi".into()])
        );
        assert!(got[0].args.is_none());
        assert!(got[0].session.is_none());
    }

    #[test]
    fn cli_event_serializes_without_skill_fields() {
        let ev = Usage::cli("task", vec!["log".into()]);
        let line = serde_json::to_string(&ev).unwrap();
        assert!(!line.contains("\"args\""), "got: {line}");
        assert!(!line.contains("\"session\""), "got: {line}");
        assert!(line.contains("\"kind\":\"cli\""));
        assert!(line.contains("\"argv\""));
    }

    #[test]
    fn skill_event_serializes_without_cli_fields() {
        let ev = Usage {
            ts: Utc::now(),
            kind: Kind::Skill,
            name: "system-registry".into(),
            cwd: Some("/home/jst".into()),
            argv: None,
            args: Some("list".into()),
            session: Some("abc123".into()),
        };
        let line = serde_json::to_string(&ev).unwrap();
        assert!(!line.contains("\"argv\""), "got: {line}");
        assert!(line.contains("\"kind\":\"skill\""));
        assert!(line.contains("\"session\":\"abc123\""));
    }

    #[test]
    fn skill_event_deserializes_from_hook_shape() {
        // The `PreToolUse` shell hook (via `feed usage`) emits this
        // exact shape; guard the wire format against drift.
        let line = r#"{"ts":"2026-07-14T01:44:48Z","kind":"skill","name":"system-registry","args":"list","session":"3192e88c-22d4-40df-9614-9391bd9f26c9","cwd":"/home/jst/tasks/0153-tooling-usage-metrics"}"#;
        let ev: Usage = serde_json::from_str(line).unwrap();
        assert_eq!(ev.kind, Kind::Skill);
        assert_eq!(ev.name, "system-registry");
        assert_eq!(ev.args.as_deref(), Some("list"));
        assert_eq!(
            ev.session.as_deref(),
            Some("3192e88c-22d4-40df-9614-9391bd9f26c9")
        );
        assert!(ev.argv.is_none());
    }

    #[test]
    fn read_all_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.log");
        assert!(read_all_usage(&path).is_empty());
    }

    #[test]
    fn read_all_skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.log");
        std::fs::write(
            &path,
            format!(
                "{}\nnot json\n{}\n",
                serde_json::to_string(&Usage::cli("task", vec!["log".into()])).unwrap(),
                serde_json::to_string(&Usage::cli("hol", vec!["file".into()])).unwrap(),
            ),
        )
        .unwrap();
        let got = read_all_usage(&path);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "task");
        assert_eq!(got[1].name, "hol");
    }

    #[test]
    fn append_creates_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/sub/usage.log");
        append_usage(&path, &Usage::cli("task", vec![])).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn long_argv_element_is_truncated() {
        let huge = "x".repeat(ARGV_ELEMENT_MAX * 2);
        let ev = Usage::cli("task", vec!["log".into(), "-m".into(), huge]);
        let argv = ev.argv.unwrap();
        assert_eq!(argv[0], "log");
        assert_eq!(argv[1], "-m");
        // Truncated element ends with the ellipsis marker.
        assert!(argv[2].ends_with('…'), "got: {}", argv[2]);
        // Byte length stays within cap + a few UTF-8 bytes for `…`.
        assert!(argv[2].len() <= ARGV_ELEMENT_MAX + 3);
    }

    #[test]
    fn short_argv_element_is_untouched() {
        let ev = Usage::cli("task", vec!["log".into(), "-m".into(), "hi".into()]);
        let argv = ev.argv.unwrap();
        assert_eq!(argv[2], "hi"); // no truncation, no ellipsis
    }

    #[test]
    fn truncation_respects_utf8_boundary() {
        // A multi-byte char straddling the cap must not produce invalid UTF-8.
        // 'é' is 2 bytes. Fill up to cap-1 with ascii, then add 'é' so the
        // boundary lands mid-char.
        let mut s = "a".repeat(ARGV_ELEMENT_MAX - 1);
        s.push('é');
        s.push_str(&"b".repeat(50));
        let ev = Usage::cli("task", vec![s]);
        let out = &ev.argv.unwrap()[0];
        assert!(out.ends_with('…'));
        // Round-trips as valid JSON string (no invalid UTF-8).
        let json = serde_json::to_string(out).unwrap();
        let round: String = serde_json::from_str(&json).unwrap();
        assert_eq!(&round, out);
    }
}
