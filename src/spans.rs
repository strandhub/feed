//! `spans` — live cross-process state for **in-progress** items.
//!
//! An event (the rest of this crate) is an immutable point in time. An
//! in-progress item is the opposite: a *live, mutable* span with an
//! enter → advance* → exit lifecycle. A phasal task is the motivating
//! case — it spans many separate short-lived CLI invocations, so no
//! single process holds the span open across its life, and the viewer
//! (`claude-overview`) is yet another process. The only thing all those
//! processes share is the filesystem, so an open span is represented as
//! **one small file per span** that survives across processes:
//!
//! - enter → [`write`] `spans/<id>.json`
//! - advance → [`write`] again with a bumped `phase`
//! - exit → [`remove`] the file (the caller separately appends a settled
//!   [`crate::Message`] to the event log, so the row collapses into the
//!   feed below it)
//!
//! A reader lists the directory ([`list_open`]) to get every currently-
//! open span. This module owns the span data only — the on-disk format
//! and the read/write primitives — mirroring how the crate root owns the
//! event format. It does NOT own rendering or any polling loop.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{append, Level, Message};

/// One open span. Serialized as a single JSON object in `spans/<id>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    /// Stable identity for the span's whole life. Used as the file stem,
    /// so advancing rewrites the same file rather than creating a new one.
    pub id: String,
    /// Human label for the row, e.g. a task slug. Distinct from `id` so
    /// the identity can be opaque while the display stays readable.
    pub name: String,
    /// Current phase (1-based) and the total phase count, rendered as
    /// `phase/total`. `total` is `None` when the count isn't known.
    pub phase: u32,
    #[serde(default)]
    pub total: Option<u32>,
    /// When the span was first entered. A reader can show elapsed time.
    pub started: DateTime<Utc>,
}

impl Span {
    /// A span entered now at `phase` of `total`.
    pub fn enter(
        id: impl Into<String>,
        name: impl Into<String>,
        phase: u32,
        total: Option<u32>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            phase,
            total,
            started: Utc::now(),
        }
    }

    /// Advance to `phase`, optionally rewriting the human label. `None`
    /// preserves the existing name (a phase-only bump); `Some(new)` swaps
    /// it (e.g. each issue of a batch triage).
    pub fn advance(&mut self, phase: u32, name: Option<String>) {
        self.phase = phase;
        if let Some(n) = name {
            self.name = n;
        }
    }

    /// `name  phase N/M` (or `name  phase N` when the total is unknown) —
    /// the one-line label a reader renders for this row.
    pub fn label(&self) -> String {
        match self.total {
            Some(t) => format!("{}  phase {}/{}", self.name, self.phase, t),
            None => format!("{}  phase {}", self.name, self.phase),
        }
    }
}

/// The conventional spans directory: `~/.cache/claude-status/spans/`.
///
/// A sibling of the event log's [`crate::default_log_path`], under the
/// same cache dir `claude-overview` already reads from. Location *policy*
/// lives here in one place; the primitives take an explicit dir so they
/// stay testable.
pub fn spans_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache/claude-status/spans")
}

/// Map a span `id` to its file inside `dir`. The id is sanitized to a
/// flat filename so a stray path separator can't escape `dir`; in
/// practice ids are task slugs, which are already safe.
fn span_path(dir: &Path, id: &str) -> PathBuf {
    let safe: String = id
        .chars()
        .map(|c| if c == '/' || c == '\\' || c == '.' { '-' } else { c })
        .collect();
    dir.join(format!("{safe}.json"))
}

/// Write (create or overwrite) the span's file under `dir`. Used for both
/// enter and advance — advancing is just a rewrite with a bumped phase,
/// keyed on the same `id`. Atomic: writes a temp file then renames, so a
/// reader never observes a half-written span.
pub fn write(dir: &Path, span: &Span) -> Result<()> {
    fs::create_dir_all(dir)
        .with_context(|| format!("creating spans dir {}", dir.display()))?;
    let path = span_path(dir, &span.id);
    let json = serde_json::to_string(span).context("serializing span")?;
    // Temp in the same dir so the rename is atomic (same filesystem).
    let tmp = dir.join(format!(".tmp.{}.json", std::process::id()));
    fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Remove the span's file (the exit step). A missing file is not an error
/// — exit is idempotent, so a double-exit or a never-entered id is a
/// no-op rather than a failure.
pub fn remove(dir: &Path, id: &str) -> Result<()> {
    let path = span_path(dir, id);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Close a span and append its settled event in one call: [`remove`] the
/// span file, then [`append`] a [`Message`] whose `span_id` is set to
/// `id`. Setting `span_id` is the whole reason to prefer this over the
/// bare `remove` + `append` pair — it lets a reader that already reflects
/// span state (the in-progress panel in `claude-overview`) dedupe the
/// settled event out of its log-line panel. See the `span_id` field docs.
pub fn exit(
    spans_dir: &Path,
    log_path: &Path,
    id: &str,
    level: Level,
    target: impl Into<String>,
    message: impl Into<String>,
) -> Result<()> {
    remove(spans_dir, id)?;
    let mut msg = Message::new(level, target, message);
    msg.span_id = Some(id.to_string());
    append(log_path, &msg)
}

/// Read one span by id, or `None` if it isn't open (or the file is
/// malformed — a half-written or hand-edited file is treated as absent
/// rather than aborting).
pub fn read(dir: &Path, id: &str) -> Option<Span> {
    let path = span_path(dir, id);
    let contents = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Every currently-open span under `dir`, sorted by `id` for a stable
/// render order. A missing dir means nothing is open → empty vec.
/// Malformed and temp (`.tmp.*`) files are skipped, not fatal.
pub fn list_open(dir: &Path) -> Vec<Span> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut spans: Vec<Span> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter(|p| {
            // Skip the atomic-write temp files (`.tmp.<pid>.json`).
            !p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(".tmp."))
        })
        .filter_map(|p| fs::read_to_string(&p).ok())
        .filter_map(|c| serde_json::from_str::<Span>(&c).ok())
        .collect();
    spans.sort_by(|a, b| a.id.cmp(&b.id));
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_with_and_without_total() {
        let s = Span::enter("feed-widget", "feed-widget", 2, Some(4));
        assert_eq!(s.label(), "feed-widget  phase 2/4");
        let s = Span::enter("build", "build", 7, None);
        assert_eq!(s.label(), "build  phase 7");
    }

    #[test]
    fn write_then_read_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let span = Span::enter("t1", "task-one", 1, Some(3));
        write(dir.path(), &span).unwrap();
        let got = read(dir.path(), "t1").unwrap();
        assert_eq!(got, span);
    }

    #[test]
    fn advance_rewrites_same_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &Span::enter("t1", "task-one", 1, Some(3))).unwrap();
        let mut s = read(dir.path(), "t1").unwrap();
        s.phase = 2;
        write(dir.path(), &s).unwrap();
        // Still exactly one open span, now at phase 2.
        let open = list_open(dir.path());
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].phase, 2);
    }

    #[test]
    fn remove_deletes_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &Span::enter("t1", "task-one", 1, None)).unwrap();
        remove(dir.path(), "t1").unwrap();
        assert!(read(dir.path(), "t1").is_none());
        // Second remove is a no-op, not an error.
        remove(dir.path(), "t1").unwrap();
    }

    #[test]
    fn exit_removes_span_and_stamps_settled_event_with_span_id() {
        use crate::tail;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("feed.log");
        write(dir.path(), &Span::enter("t1", "task-one", 1, None)).unwrap();
        exit(dir.path(), &log, "t1", Level::Info, "task", "task-one done").unwrap();
        // Span file gone; settled event landed with span_id populated.
        assert!(read(dir.path(), "t1").is_none());
        let events = tail(&log, usize::MAX);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].span_id.as_deref(), Some("t1"));
        assert_eq!(events[0].message, "task-one done");
        assert_eq!(events[0].target, "task");
    }

    #[test]
    fn list_open_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(list_open(&missing).is_empty());
    }

    #[test]
    fn list_open_sorted_by_id_and_skips_malformed() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &Span::enter("zebra", "z", 1, Some(2))).unwrap();
        write(dir.path(), &Span::enter("alpha", "a", 1, Some(2))).unwrap();
        // A malformed json file must be skipped, not abort the read.
        fs::write(dir.path().join("garbage.json"), "not json").unwrap();
        let open = list_open(dir.path());
        let ids: Vec<&str> = open.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "zebra"]);
    }

    #[test]
    fn advance_can_override_name() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &Span::enter("triage", "batch", 1, Some(9))).unwrap();
        let mut s = read(dir.path(), "triage").unwrap();
        s.advance(2, Some("fix bug in foo".to_string()));
        write(dir.path(), &s).unwrap();
        let got = read(dir.path(), "triage").unwrap();
        assert_eq!(got.phase, 2);
        assert_eq!(got.name, "fix bug in foo");
        assert_eq!(got.total, Some(9));
    }

    #[test]
    fn advance_without_name_preserves_existing_name() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &Span::enter("triage", "batch", 1, Some(9))).unwrap();
        let mut s = read(dir.path(), "triage").unwrap();
        s.advance(2, None);
        write(dir.path(), &s).unwrap();
        let got = read(dir.path(), "triage").unwrap();
        assert_eq!(got.phase, 2);
        assert_eq!(got.name, "batch");
    }

    #[test]
    fn id_with_path_separators_is_sanitized() {
        let dir = tempfile::tempdir().unwrap();
        // A malicious / accidental id must not escape the spans dir.
        write(dir.path(), &Span::enter("../escape", "x", 1, None)).unwrap();
        // The file lands inside dir (flattened), and reads back by the
        // same id.
        assert!(read(dir.path(), "../escape").is_some());
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().into_string().unwrap())
            .filter(|n| !n.starts_with(".tmp."))
            .collect();
        // `.`, `.`, `/` each map to `-` → `---escape`.
        assert_eq!(entries, vec!["---escape.json"]);
    }
}
