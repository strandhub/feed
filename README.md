# feed

> TL;DR: Tiny append-only event log + span ledger shared across workspace tools.

A tiny append-only **event** log shared across workspace tools, built on
`tracing`.

- **Scope, boundary, inbox routing** → [SCOPE.md](SCOPE.md).
- **Skill body** (LLM-facing conventions, when-to-fire) → [skill/SKILL.md](skill/SKILL.md).
- **Error-lookup skill body** → [skill-error-lookup/SKILL.md](skill-error-lookup/SKILL.md).
- **Issues** — file friction against this system on this repo's GitHub Issues tab. Legacy pre-migration issues archived read-only in [issues/](issues/).

File substrate-shaped friction files against this repo; producer-side
editorial choices route to the producer.

A *feeder* appends an event as a side effect of doing something
noteworthy. A *reader* tails the log and renders the last few events.
Today the reader is `claude-overview`'s events widget (toggled with `f`).

Events are immutable points in time, modelled on a `tracing::Event`: a
**level** (severity), a **target** (which subsystem), and a message.
Outcome is encoded in the level — an error is logged at `error`; anything
else is a settled/successful event. **In-progress** state (a phasal task
mid-phase, a build streaming layers) is a *different* concern — a live,
mutable span, not an event — and is deliberately **not** modelled here.

This crate owns the event **data** only — the on-disk format and the
read/write primitives. It does not own rendering (the reader styles by
level) or any polling loop. `claude-overview` is a *consumer*, not the
owner.

## Two producer paths

**1. `tracing` (native, for Rust producers).** A producer installs the
bridge once in `main()` and then just uses `tracing`:

    feed::init();                              // installs the FeedLayer
    tracing::info!(target: "task", "archived xyz");
    tracing::error!(target: "deploy", "boom");

Each event becomes a line in the log, with level + target captured
automatically. `feed::init()` honors a `FEED_LOG` env filter (default
`info`) and is best-effort — it never panics or disturbs the producer.
The bridge lives behind the `tracing` feature:

    feed = { path = "../feed", features = ["tracing"] }

`task` is the first adopter — see its `main.rs`.

**2. The `feed` CLI (bypass, for everything else).** A non-Rust caller
(or a quick manual line) constructs the same on-disk schema by hand:

    feed "created my-task" --target task
    feed "deploy failed" --error            # sugar for --level error
    feed "low detail" --level debug --target deploy

Each call appends one line and prints a confirmation:

    fed [info] task: created my-task

## On disk

JSONL at `~/.cache/claude-status/feed.log` (the same cache dir
`claude-overview` already reads), one object per line:

    {"timestamp":"2026-06-17T15:00:02Z","level":"info","target":"task","message":"…"}

`level` is one of `trace` / `debug` / `info` / `warn` / `error`.
Malformed lines (partial writes, hand-edits) are skipped on read.

## Library

    use feed::{append, tail, default_log_path, Message, Level};

    append(&default_log_path(), &Message::new(Level::Info, "task", "done"))?;
    let recent = tail(&default_log_path(), 8); // last 8, oldest first

Read/write primitives take an explicit path so they stay testable;
`default_log_path()` is the one place location policy lives.

## Build / test

    cargo install --path . --features tracing   # CLI + the tracing bridge
    cargo test --features tracing               # includes FeedLayer tests
