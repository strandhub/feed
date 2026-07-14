# feed

> TL;DR: Tiny append-only OTel-shaped log + span ledger shared across workspace tools.

A tiny append-only **event** log + **span** ledger shared across
workspace tools, built on `tracing` and shaped as an OTel `LogRecord`
subset on-disk.

- **Scope, boundary, inbox routing** → [SCOPE.md](SCOPE.md).
- **Skill body** (LLM-facing conventions, when-to-fire, severity
  discipline, `event_name` naming) → [skill/SKILL.md](skill/SKILL.md).
- **Error-lookup skill body** → [skill-error-lookup/SKILL.md](skill-error-lookup/SKILL.md).
- **Issues** — file friction against this system on this repo's GitHub Issues tab. Legacy pre-migration issues archived read-only in [issues/](issues/).

File substrate-shaped friction against this repo; producer-side
editorial choices (message text, `source`, `event_name`) route to
the producer.

A *feeder* appends a `LogRecord` as a side effect of doing something
noteworthy. A *reader* tails the log and renders the last few
records. Today the primary reader is `claude-overview`'s events
widget (toggled with `f`).

Records follow the OTel `LogRecord` data model — subset, no
distributed-systems bits: `severity_number` + `severity_text`,
`body`, optional `event_name` (the OTel Event marker), open
`attributes` bag, optional `trace_id`/`span_id`. Every row is a
valid OTel LogRecord; consumers apply their own filter recipes over
severity, event_name, and attributes.

**In-progress** state (a phasal task mid-phase, a build streaming
layers) is a *different* concern — a live, mutable span, not an
event. Spans live in `src/spans.rs`: one file per open span with an
enter → advance → exit lifecycle. `exit` removes the span file AND
appends a settled `LogRecord` with `trace_id`/`span_id` set to the
span id (OTel correlation).

This crate owns the event **data** and the **severity semantics** —
the on-disk format, the read/write primitives, and the honest-
severity contract. It does not own rendering (the reader styles by
`severity_number`) or any polling loop. `claude-overview` is a
*consumer*, not the owner. See the [`feed` skill](skill/SKILL.md)
for the honest-severity + event_name discipline.

## Two producer paths

**1. `tracing` (native, for Rust producers).** A producer installs
the bridge once in `main()` and then just uses `tracing`:

    feed::init();                              // installs the FeedLayer
    tracing::info!(target: "task", "archived xyz");
    tracing::error!(target: "deploy", "boom");

    // an OTel Event (log with event_name)
    tracing::info!(
        target: "task",
        event_name = "task.archive.completed",
        slug = "0142-foo",
        "archived xyz",
    );

Each event becomes one JSONL line. The `tracing` `target:` lands as
`attributes.source`; extra fields land as attributes; a `message`
field (or the format-string body) lands as `body`; an `event_name`
field lifts to the top-level OTel Event marker. `feed::init()`
honors a `FEED_LOG` env filter (default `info`) and is best-effort
— it never panics or disturbs the producer.

Add the bridge feature:

    feed = { git = "https://github.com/strandhub/feed", features = ["tracing"] }

`task`, `system-registry`, and `audit` are adopters — see each
crate's `main.rs` / `runner.rs`.

**2. The `feed` CLI (bypass, for everything else).** A non-Rust
caller (or a quick manual line) constructs the same OTel-shaped
row:

    feed "created my-task" --source task
    feed --error --source deploy "deploy failed"
    feed --warn --source audit --event-name audit.run.degraded \
         --attr degraded_reason="trailer missing tldr" \
         "audit run cli-verb-error degraded"

Each call appends one line and prints a confirmation:

    fed [info] task: created my-task

See `feed --help` for the full flag set, including `--attr
key=value` for structured attributes.

**Auto-invocation records.** A CLI can emit an invocation Event
with one line:

    feed::init();
    feed::log_cli_invocation("task");   // event_name: "task.invoked"

Standard shape; use this rather than hand-rolling.

## On disk

JSONL at `~/.local/share/claude-status/feed.log` — one OTel
`LogRecord` per line:

    {
      "timestamp": "2026-07-14T22:18:19.721498487Z",
      "severity_number": 13,
      "severity_text": "WARN",
      "body": "audit run cli-verb-error degraded",
      "event_name": "audit.run.degraded",
      "attributes": {
        "source": "audit",
        "operation": "audit-run",
        "auditor": "cli-verb-error",
        "degraded_reason": "trailer written without a TL;DR — …"
      },
      "trace_id": "audit-run-cli-verb-error",
      "span_id": "audit-run-cli-verb-error"
    }

Under `~/.local/share/` (durable data) rather than `~/.cache/`
(disposable) — the fleet reads history for regression checks, so
this isn't cache-shaped.

`severity_number` follows OTel: 1/5/9/13/17 for TRACE/DEBUG/INFO/
WARN/ERROR. `severity_text` mirrors it uppercase. `event_name` and
`attributes` are optional; `trace_id`/`span_id` are set when a span
was open. Malformed lines (partial writes, hand-edits, pre-OTel-
shape rows from before 2026-07-14) are skipped on read.

Spans live at `~/.cache/claude-status/spans/<id>.json` (one JSON
object per open span). See `src/spans.rs`.

## Library

    use feed::{append, tail, default_log_path, LogRecord, SeverityNumber};

    let mut r = LogRecord::new(SeverityNumber::Info, "task", "done");
    r.attributes.insert("slug".into(), "0142-foo".into());
    append(&default_log_path(), &r)?;

    let recent = tail(&default_log_path(), 8); // last 8, oldest first

`LogRecord::event(severity, source, event_name, body)` constructs
an OTel Event (a record with `event_name` set). Read/write
primitives take an explicit path so they stay testable;
`default_log_path()` is the one place location policy lives.

## Build / test

    cargo install --path . --features tracing   # CLI + the tracing bridge
    cargo test --features tracing               # includes FeedLayer tests
