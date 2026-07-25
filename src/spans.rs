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
//! - advance → [`write`] again with a new display `name`
//! - exit → [`remove`] the file (the caller separately appends a settled
//!   [`crate::LogRecord`] to the event log, so the row collapses into the
//!   feed below it)
//!
//! A reader lists the directory ([`list_open`]) to get every currently-
//! open span. This module owns the span data only — the on-disk format
//! and the read/write primitives — mirroring how the crate root owns the
//! event format. It does NOT own rendering or any polling loop.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{append, LogRecord, SeverityNumber};

/// Process-global current span id, set by whoever opens the span and
/// read by deep-in-the-stack code that wants to narrate progress
/// without threading the id through every function signature.
///
/// Modeled on `tracing::Span::current()`: an outer scope enters a span
/// and any code invoked within can annotate that span in flight without
/// knowing its identity. Because our spans live on disk (cross-process),
/// this is only useful within one process — a child process invoked via
/// `Command::new` starts with a clean slate and must be told the id
/// explicitly (e.g. via an env var or CLI flag) if it wants to advance
/// the same span.
static CURRENT_SPAN_ID: Mutex<Option<String>> = Mutex::new(None);

/// One open span. Serialized as a single JSON object in `spans/<id>.json`.
///
/// Modeled on `tracing`'s [`Span`](https://docs.rs/tracing/latest/tracing/struct.Span.html):
/// `name` is the mutable display label (equivalent to `otel.name` in the
/// tracing-opentelemetry bridge — a reserved field that overrides the
/// exporter-visible name). `advance` swaps the name in place; the reader
/// renders `name` verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    /// Stable identity for the span's whole life. Used as the file stem,
    /// so advancing rewrites the same file rather than creating a new one.
    pub id: String,
    /// Mutable one-line label the reader renders for this row. Callers
    /// embed whatever narration they want (e.g. `"cli-verb-error: judging
    /// session 3/17"`); no phase/total baked into the render.
    pub name: String,
    /// When the span was first entered. A reader can show elapsed time.
    pub started: DateTime<Utc>,
}

impl Span {
    /// A span entered now with the given display label.
    pub fn enter(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            started: Utc::now(),
        }
    }

    /// Rewrite the display label — the one operation `advance` performs.
    /// The auditor calls this to narrate progress in place (`"reading
    /// corpus"` → `"judging 3/17"` → `"writing report"`).
    pub fn advance(&mut self, name: impl Into<String>) {
        self.name = name.into();
    }

    /// The one-line label a reader renders for this row — `name` verbatim.
    pub fn label(&self) -> String {
        self.name.clone()
    }
}

/// Set the process-global current span id. Called by whoever opens the
/// outer span; deep-in-the-stack code then calls [`advance_current`]
/// without needing to know the id. Pair with [`clear_current`] before
/// the span exits.
pub fn set_current(id: impl Into<String>) {
    if let Ok(mut cur) = CURRENT_SPAN_ID.lock() {
        *cur = Some(id.into());
    }
}

/// Clear the process-global current span id. Called by whoever set it,
/// after the span exits (so a stale id doesn't leak into a later span
/// opened by the same process).
pub fn clear_current() {
    if let Ok(mut cur) = CURRENT_SPAN_ID.lock() {
        *cur = None;
    }
}

/// Advance the process-global current span in flight, if one is set.
/// A no-op if [`set_current`] was never called (e.g. called from a
/// test, or outside a runner-managed context) — narration is
/// best-effort and never blocks the caller.
pub fn advance_current(name: impl Into<String>) {
    let Ok(cur) = CURRENT_SPAN_ID.lock() else { return };
    let Some(id) = cur.as_ref() else { return };
    let dir = spans_dir();
    let Some(mut span) = read(&dir, id) else { return };
    span.advance(name);
    let _ = write(&dir, &span);
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

/// Close a span and append its settled record in one call: [`remove`]
/// the span file, then [`append`] a [`LogRecord`] whose `trace_id` and
/// `span_id` are set to `id`. Setting the correlation ids is the whole
/// reason to prefer this over the bare `remove` + `append` pair — it
/// lets a reader that already reflects span state (the in-progress
/// panel in `claude-overview`) dedupe the settled record out of its
/// log-line panel.
pub fn exit(
    spans_dir: &Path,
    log_path: &Path,
    id: &str,
    severity: SeverityNumber,
    source: impl Into<String>,
    body: impl Into<String>,
) -> Result<()> {
    remove(spans_dir, id)?;
    let mut record = LogRecord::new(severity, source, body);
    record.trace_id = Some(id.to_string());
    record.span_id = Some(id.to_string());
    append(log_path, &record)
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
    fn label_is_name_verbatim() {
        let s = Span::enter("feed-widget", "feed-widget");
        assert_eq!(s.label(), "feed-widget");
        let s = Span::enter("build", "build: streaming layers 3/7");
        assert_eq!(s.label(), "build: streaming layers 3/7");
    }

    #[test]
    fn write_then_read_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let span = Span::enter("t1", "task-one");
        write(dir.path(), &span).unwrap();
        let got = read(dir.path(), "t1").unwrap();
        assert_eq!(got, span);
    }

    #[test]
    fn advance_rewrites_same_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &Span::enter("t1", "task-one")).unwrap();
        let mut s = read(dir.path(), "t1").unwrap();
        s.advance("task-one: step two");
        write(dir.path(), &s).unwrap();
        // Still exactly one open span, now with the new name.
        let open = list_open(dir.path());
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].name, "task-one: step two");
    }

    #[test]
    fn remove_deletes_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &Span::enter("t1", "task-one")).unwrap();
        remove(dir.path(), "t1").unwrap();
        assert!(read(dir.path(), "t1").is_none());
        // Second remove is a no-op, not an error.
        remove(dir.path(), "t1").unwrap();
    }

    #[test]
    fn exit_removes_span_and_stamps_settled_record_with_span_and_trace_ids() {
        use crate::tail;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("feed.log");
        write(dir.path(), &Span::enter("t1", "task-one")).unwrap();
        exit(dir.path(), &log, "t1", SeverityNumber::Info, "task", "task-one done").unwrap();
        // Span file gone; settled record landed with span_id and
        // trace_id populated (both = the span id in our single-span
        // model, per LogRecord docs).
        assert!(read(dir.path(), "t1").is_none());
        let records = tail(&log, usize::MAX);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].span_id.as_deref(), Some("t1"));
        assert_eq!(records[0].trace_id.as_deref(), Some("t1"));
        assert_eq!(records[0].body, "task-one done");
        assert_eq!(records[0].source(), "task");
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
        write(dir.path(), &Span::enter("zebra", "z")).unwrap();
        write(dir.path(), &Span::enter("alpha", "a")).unwrap();
        // A malformed json file must be skipped, not abort the read.
        fs::write(dir.path().join("garbage.json"), "not json").unwrap();
        let open = list_open(dir.path());
        let ids: Vec<&str> = open.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "zebra"]);
    }

    #[test]
    fn advance_swaps_name() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &Span::enter("triage", "batch")).unwrap();
        let mut s = read(dir.path(), "triage").unwrap();
        s.advance("fix bug in foo");
        write(dir.path(), &s).unwrap();
        let got = read(dir.path(), "triage").unwrap();
        assert_eq!(got.name, "fix bug in foo");
    }

    #[test]
    fn advance_current_no_op_when_unset() {
        // Preconditions: no current span set. Should not panic, should
        // not write anything anywhere. Best-effort narration from a
        // caller invoked outside a runner-managed context.
        clear_current();
        advance_current("orphan narration");
    }

    #[test]
    fn set_and_clear_current_are_idempotent() {
        // Double-set overwrites; double-clear is a no-op. This shape
        // matches how a nested caller might set-then-clear inside an
        // outer set-then-clear without corrupting the outer id (well,
        // it does — the outer would find a cleared slot on its own
        // clear — which is acceptable for the current single-runner
        // usage. If nested spans ever land, this would need a stack.)
        set_current("first");
        set_current("second");
        clear_current();
        clear_current();
    }

    #[test]
    fn id_with_path_separators_is_sanitized() {
        let dir = tempfile::tempdir().unwrap();
        // A malicious / accidental id must not escape the spans dir.
        write(dir.path(), &Span::enter("../escape", "x")).unwrap();
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
