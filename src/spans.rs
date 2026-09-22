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
//!
//! # Stale-span cleanup
//!
//! Two layers, because neither alone is sufficient:
//!
//! 1. **In-process, RAII.** [`SpanGuard`] removes the file on drop, so a
//!    panic between enter and exit doesn't leak. Callers that want the
//!    explicit exit path (append a settled record alongside removal) call
//!    [`SpanGuard::disarm`] first — otherwise the guard would double-remove
//!    a file whose settled record has already landed.
//! 2. **Cross-process, reaper.** [`Span`] records the owning `pid` at
//!    enter. [`list_open`] filters out (and unlinks) files whose owner is
//!    no longer alive — the only defense against `SIGKILL`, OOM, and host
//!    reboot mid-run. When the writer isn't the owner (a shell script
//!    invokes `feed span enter` and does the work itself), the caller
//!    passes the real owner's PID via [`Span::enter_owned_by`] (Rust) or
//!    `feed span enter <id> --pid "$$"` (shell) — modeled on systemd's
//!    `PIDFile=`, where the invocation that writes the file and the
//!    process that owns the work are deliberately separable. Legacy files
//!    without a `pid` get an age fallback so this migration doesn't
//!    strand them forever.

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
    /// PID of the process that entered the span. Used by [`list_open`]'s
    /// reaper to detect and unlink spans whose owner is no longer alive
    /// (crashes, SIGKILL, host reboot mid-run). `None` on files written
    /// before this field existed — those fall through to the age-based
    /// reaper rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

impl Span {
    /// A span entered now with the given display label. Records the
    /// current process's PID as the owner — the reaper unlinks the span
    /// once that PID is gone. For a span whose real owner outlives the
    /// process that calls this (e.g. `feed span enter` invoked from a
    /// shell script that then does the work), use [`Span::enter_owned_by`]
    /// with the owner's PID.
    pub fn enter(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self::enter_owned_by(id, name, std::process::id())
    }

    /// A span entered now, ownership attributed to `pid` instead of the
    /// current process. Modeled on systemd's `PIDFile=` — the invocation
    /// that writes the file is separate from the process that *owns* the
    /// work, and only the latter's liveness gates cleanup.
    ///
    /// Typical caller: a shell script that opens a span for work it will
    /// then run, invoked as `feed span enter <id> --pid "$$"`. The shell's
    /// PID (`$$`) is the real owner; the `feed` process exits immediately
    /// after writing the file.
    pub fn enter_owned_by(id: impl Into<String>, name: impl Into<String>, pid: u32) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            started: Utc::now(),
            pid: Some(pid),
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
///
/// **Reaps stale files.** A span whose recorded PID is no longer alive
/// (or a legacy pid-less file older than [`LEGACY_REAP_AGE`]) is
/// unlinked and excluded from the result, so a crashed / killed owner
/// doesn't leave permanent ghost rows in the overview. Removal is
/// best-effort — a file we can't unlink (permissions, race with another
/// reaper) is just skipped from the render, not surfaced.
pub fn list_open(dir: &Path) -> Vec<Span> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let now = Utc::now();
    let mut spans: Vec<Span> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter(|p| {
            !p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(".tmp."))
        })
        .filter_map(|p| {
            let contents = fs::read_to_string(&p).ok()?;
            let span: Span = serde_json::from_str(&contents).ok()?;
            if is_stale(&span, now) {
                let _ = fs::remove_file(&p);
                return None;
            }
            Some(span)
        })
        .collect();
    spans.sort_by(|a, b| a.id.cmp(&b.id));
    spans
}

/// Legacy files (written before `Span::pid` existed) are reaped once
/// they've been open longer than this. Long enough that a genuinely
/// long-running task isn't reaped mid-flight, short enough that a
/// stranded file goes away within a day.
const LEGACY_REAP_AGE: chrono::Duration = chrono::Duration::hours(24);

fn is_stale(span: &Span, now: DateTime<Utc>) -> bool {
    match span.pid {
        Some(pid) => !pid_alive(pid),
        None => now.signed_duration_since(span.started) > LEGACY_REAP_AGE,
    }
}

/// Best-effort liveness probe via POSIX `kill(pid, 0)` — the standard
/// way to ask "does this process exist" on any Unix (Linux, macOS, BSDs).
/// Sending signal 0 performs the permission check but doesn't deliver a
/// signal:
///
/// - `Ok(())` → process exists and we could have signalled it → alive.
/// - `Err(EPERM)` → process exists but we lack permission (different
///   uid / capability set). Still alive — reported as such so we don't
///   unlink a live process's span.
/// - `Err(ESRCH)` → no such process → dead → reap.
///
/// PID reuse is a theoretical false-negative (reaper skips a span whose
/// owner died and its PID was reassigned to something else) — tolerated
/// because the alternative is unlinking a live span, which is strictly
/// worse. Non-Unix platforms conservatively report alive; stale spans on
/// those hosts fall through to the legacy age rule.
fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // `pid_t` is `i32` on all Unix targets we run on. A `u32` that
        // doesn't fit — or that fits but as a non-positive pid_t after
        // sign reinterpretation — is not a legitimate process id, and
        // handing it to `kill(2)` invokes the broadcast/process-group
        // forms (0 = "every process in our pgrp", -1 = "every process
        // we can signal", other negatives = pgid targets). Any of those
        // would spuriously return success and hold a ghost span alive.
        // Reject them up front as dead.
        let Ok(pid_t) = libc::pid_t::try_from(pid) else {
            return false;
        };
        if pid_t <= 0 {
            return false;
        }
        // SAFETY: `kill(pid, 0)` with signal 0 is defined by POSIX to be
        // a permission/existence probe with no side effects. No memory
        // is read or written; the only observable is the return value
        // and `errno`. `pid_t` is now guaranteed positive.
        let ret = unsafe { libc::kill(pid_t, 0) };
        if ret == 0 {
            return true;
        }
        let err = std::io::Error::last_os_error();
        // ESRCH is the only "definitely dead" errno; anything else
        // (EPERM = alive but not signal-able by us, EINVAL = shouldn't
        // happen with sig=0) reports alive to stay safe.
        !matches!(err.raw_os_error(), Some(libc::ESRCH))
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// RAII guard that removes the span file on drop, so an in-process panic
/// between enter and exit doesn't leak a ghost span. Owns two resources:
///
/// 1. The span file at `spans/<id>.json` — removed on drop.
/// 2. The process-global [`CURRENT_SPAN_ID`] — set to this guard's id at
///    enter, cleared on drop. Callers no longer set/clear `CURRENT_SPAN_ID`
///    manually; the guard owns that pairing so a panic between set and
///    clear can't leak a stale id into a later span in the same process.
///
/// Drop is best-effort (a failed unlink is silently swallowed — the
/// reaper on the read side is the backstop).
///
/// Callers that want the explicit exit path — [`exit`] appends a settled
/// [`LogRecord`] alongside removal — call [`SpanGuard::disarm`] first so
/// the guard doesn't double-remove after the settled record has already
/// landed. `disarm` only suppresses the file removal; the
/// `CURRENT_SPAN_ID` clear still happens on drop, so the current-id
/// invariant holds regardless of which exit path the caller took.
///
/// Does NOT help with `SIGKILL` / OOM / host reboot / cross-process spans
/// — Rust can't run destructors on aborts. That's the reaper's job.
#[must_use = "SpanGuard removes the span file on drop; hold it for the span's lifetime"]
pub struct SpanGuard {
    dir: PathBuf,
    /// `Some` while the guard is armed for file removal. `disarm` takes
    /// this without also releasing the current-id ownership below.
    id: Option<String>,
    /// True iff this guard successfully set `CURRENT_SPAN_ID` at enter
    /// — i.e., is responsible for clearing it on drop. False when the
    /// mutex was poisoned at enter (rare); drop then leaves the slot
    /// alone rather than clearing someone else's id.
    owns_current: bool,
}

impl SpanGuard {
    /// Enter a span and take a guard that will remove the file on drop.
    /// Writes the span file eagerly, and sets [`CURRENT_SPAN_ID`] so
    /// deep-in-the-stack code can call [`advance_current`] without
    /// threading the id through every function signature. If either
    /// side-effect fails (write error, mutex poisoning) the guard is
    /// still returned so drop can attempt best-effort cleanup — no
    /// worse than the no-guard status quo.
    pub fn enter(dir: impl Into<PathBuf>, id: impl Into<String>, name: impl Into<String>) -> Self {
        Self::enter_owned_by(dir, id, name, std::process::id())
    }

    /// Enter a span attributed to `pid` (not the current process). See
    /// [`Span::enter_owned_by`] — same escape hatch, guard version.
    pub fn enter_owned_by(
        dir: impl Into<PathBuf>,
        id: impl Into<String>,
        name: impl Into<String>,
        pid: u32,
    ) -> Self {
        let dir = dir.into();
        let id = id.into();
        let span = Span::enter_owned_by(id.clone(), name, pid);
        let _ = write(&dir, &span);
        let owns_current = if let Ok(mut cur) = CURRENT_SPAN_ID.lock() {
            *cur = Some(id.clone());
            true
        } else {
            false
        };
        Self { dir, id: Some(id), owns_current }
    }

    /// The span id this guard is holding open.
    pub fn id(&self) -> &str {
        self.id.as_deref().unwrap_or("")
    }

    /// Suppress the drop-time file removal — the caller is taking over
    /// that side (typically via [`exit`], which removes the file and
    /// appends a settled record atomically). Idempotent. Does NOT
    /// release `CURRENT_SPAN_ID` ownership; the guard still clears it
    /// on drop so a subsequent span in this process starts clean.
    pub fn disarm(&mut self) {
        self.id = None;
    }
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let _ = remove(&self.dir, &id);
        }
        if self.owns_current {
            if let Ok(mut cur) = CURRENT_SPAN_ID.lock() {
                *cur = None;
            }
        }
    }
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
    fn enter_records_current_pid() {
        let s = Span::enter("t", "task");
        assert_eq!(s.pid, Some(std::process::id()));
    }

    #[test]
    fn enter_owned_by_records_given_pid_not_current() {
        // The escape hatch: caller records a foreign PID so the reaper's
        // liveness check tracks the *owner* of the work, not the process
        // that wrote the file.
        let s = Span::enter_owned_by("t", "task", 12345);
        assert_eq!(s.pid, Some(12345));
        assert_ne!(s.pid, Some(std::process::id()));
    }

    #[test]
    fn span_guard_enter_owned_by_records_given_pid() {
        let dir = tempfile::tempdir().unwrap();
        // Use PID 1 — always alive on any Unix, so the reaper won't
        // unlink it out from under us mid-test.
        let g = SpanGuard::enter_owned_by(dir.path(), "delegated", "delegated work", 1);
        assert_eq!(g.id(), "delegated");
        let span = read(dir.path(), "delegated").unwrap();
        assert_eq!(span.pid, Some(1));
        drop(g);
    }

    #[test]
    fn list_open_reaps_span_whose_pid_is_dead() {
        // A span recorded against a PID that's virtually certain not to
        // exist. u32::MAX exceeds every platform's practical pid range,
        // so `kill(pid, 0)` returns ESRCH on both Linux and macOS.
        // We can't spawn-and-kill a process portably in a unit test —
        // the fake-pid approach is what the reaper is actually built to
        // catch.
        let dir = tempfile::tempdir().unwrap();
        let ghost = Span {
            id: "ghost".to_string(),
            name: "ghost span".to_string(),
            started: Utc::now(),
            pid: Some(u32::MAX),
        };
        write(dir.path(), &ghost).unwrap();
        // Unix (Linux, macOS, BSDs): list_open reaps and returns empty;
        // the file is gone. Non-Unix conservatively reports alive so the
        // span survives — asserted separately.
        #[cfg(unix)]
        {
            let open = list_open(dir.path());
            assert!(open.is_empty(), "ghost span should be reaped");
            assert!(read(dir.path(), "ghost").is_none(), "file should be unlinked");
        }
    }

    #[test]
    #[cfg(unix)]
    fn pid_alive_reports_true_for_own_process() {
        // Self-liveness is the tightest sanity check for the kill(0)
        // probe: our own PID must always report alive, on every Unix.
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    #[cfg(unix)]
    fn pid_alive_reports_false_for_ghost_pid() {
        // A value comfortably above Linux's default pid_max (4194304)
        // and macOS's ceiling (99998), but still within pid_t's positive
        // range. ESRCH → reported dead on both platforms.
        assert!(!pid_alive(9_999_999));
    }

    #[test]
    #[cfg(unix)]
    fn pid_alive_rejects_zero_and_negative_pid_t_values() {
        // `kill(0, sig)` broadcasts to the current process group and
        // `kill(-1, sig)` broadcasts to every signal-able process — both
        // return success and would spuriously hold a ghost span alive if
        // pid_alive naively cast them through. Guard against both edges
        // of the pid_t sign trap:
        //
        // - pid = 0 → pid_t = 0 → broadcast to pgrp.
        // - pid = u32::MAX → pid_t = -1 → broadcast to all.
        // - Any u32 that doesn't fit pid_t → tryfrom fails → dead.
        assert!(!pid_alive(0), "pid 0 must not be treated as a live process");
        assert!(!pid_alive(u32::MAX), "u32::MAX casts to pid_t=-1 (broadcast); must be dead");
    }

    #[test]
    fn list_open_keeps_span_whose_pid_is_alive() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &Span::enter("live", "live span")).unwrap();
        let open = list_open(dir.path());
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, "live");
    }

    #[test]
    fn list_open_reaps_legacy_pidless_span_past_age_cap() {
        let dir = tempfile::tempdir().unwrap();
        let old = Span {
            id: "legacy".to_string(),
            name: "legacy".to_string(),
            started: Utc::now() - chrono::Duration::hours(48),
            pid: None,
        };
        write(dir.path(), &old).unwrap();
        let open = list_open(dir.path());
        assert!(open.is_empty(), "stale legacy span should be reaped");
    }

    #[test]
    fn list_open_keeps_legacy_pidless_span_within_age_cap() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = Span {
            id: "legacy-fresh".to_string(),
            name: "legacy fresh".to_string(),
            started: Utc::now() - chrono::Duration::hours(1),
            pid: None,
        };
        write(dir.path(), &fresh).unwrap();
        let open = list_open(dir.path());
        assert_eq!(open.len(), 1);
    }

    #[test]
    fn span_guard_removes_file_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        {
            let _g = SpanGuard::enter(dir.path(), "guarded", "guarded work");
            assert!(read(dir.path(), "guarded").is_some());
        }
        assert!(
            read(dir.path(), "guarded").is_none(),
            "drop should have removed the span file"
        );
    }

    #[test]
    fn span_guard_disarm_suppresses_drop_removal() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut g = SpanGuard::enter(dir.path(), "handed-off", "handed off");
            g.disarm();
        }
        assert!(
            read(dir.path(), "handed-off").is_some(),
            "disarmed guard must not remove the file"
        );
    }

    #[test]
    fn span_guard_sets_and_clears_current_span_id() {
        // Baseline: nothing set.
        clear_current();
        let dir = tempfile::tempdir().unwrap();
        {
            let _g = SpanGuard::enter(dir.path(), "current-owning", "current-owning work");
            let cur = CURRENT_SPAN_ID.lock().unwrap().clone();
            assert_eq!(cur.as_deref(), Some("current-owning"));
        }
        let cur = CURRENT_SPAN_ID.lock().unwrap().clone();
        assert_eq!(cur, None, "drop should have cleared CURRENT_SPAN_ID");
    }

    #[test]
    fn span_guard_disarm_still_clears_current_span_id() {
        // Disarm suppresses file removal but keeps current-id ownership;
        // drop still clears CURRENT_SPAN_ID so a subsequent span in this
        // process starts from a clean slot.
        clear_current();
        let dir = tempfile::tempdir().unwrap();
        {
            let mut g = SpanGuard::enter(dir.path(), "disarm-current", "disarm test");
            g.disarm();
            let cur = CURRENT_SPAN_ID.lock().unwrap().clone();
            assert_eq!(cur.as_deref(), Some("disarm-current"), "current still set pre-drop");
        }
        let cur = CURRENT_SPAN_ID.lock().unwrap().clone();
        assert_eq!(cur, None, "drop must clear current even after disarm");
        // And disarm preserved the file itself.
        assert!(read(dir.path(), "disarm-current").is_some());
    }

    #[test]
    fn span_guard_clears_current_span_id_on_panic() {
        // The whole point of moving CURRENT_SPAN_ID ownership into the
        // guard: a panic between enter and manual clear no longer leaks
        // a stale id into a subsequent span in the same process.
        clear_current();
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();
        let result = std::panic::catch_unwind(|| {
            let _g = SpanGuard::enter(&dir_path, "panic-current", "panic-current work");
            panic!("simulated failure with CURRENT_SPAN_ID set");
        });
        assert!(result.is_err());
        let cur = CURRENT_SPAN_ID.lock().unwrap().clone();
        assert_eq!(cur, None, "panic-unwound drop should have cleared CURRENT_SPAN_ID");
    }

    #[test]
    fn span_guard_removes_file_on_panic() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();
        let result = std::panic::catch_unwind(|| {
            let _g = SpanGuard::enter(&dir_path, "panicky", "panicky work");
            assert!(read(&dir_path, "panicky").is_some());
            panic!("simulated failure between enter and exit");
        });
        assert!(result.is_err(), "inner block should have panicked");
        assert!(
            read(dir.path(), "panicky").is_none(),
            "panic-unwound drop should have removed the file"
        );
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
