## feed

A tiny append-only activity log shared across workspace tools.

A *feeder* (a skill, a CLI, a hook) appends a message as a side effect
of doing something noteworthy. A *reader* tails the log and renders the
last few messages. Today the only reader is `claude-overview`'s bottom
feed panel (toggled with `f`).

This crate owns the **data** only — the on-disk format and the
read/write primitives. It does not own rendering (the reader styles
messages by status) or any polling loop (the reader drives its own
redraws). `claude-overview` is a *consumer* of `feed`, not its owner.

## Write a message

    feed "task-archive: task-xyz" --success
    feed "deploy to yggdrasil failed" --error
    feed "syncing knowledge base"          # no flag → pending/informational

Each call appends one JSON line and prints a confirmation:

    fed [Success] task-archive: task-xyz

## On disk

JSONL at `~/.cache/claude-status/feed.log` (the same cache dir
`claude-overview` already reads), one object per line:

    {"timestamp":"2026-06-17T14:34:04Z","status":"success","message":"…"}

`status` is one of `success` / `error` / `pending`. Malformed lines
(partial writes, hand-edits) are skipped on read rather than aborting it.

## Library

    use feed::{append, tail, default_log_path, Message, Status};

    append(&default_log_path(), &Message::new(Status::Success, "done"))?;
    let recent = tail(&default_log_path(), 8); // last 8, oldest first

Read/write primitives take an explicit path so they stay testable;
`default_log_path()` is the one place location policy lives.

## Build

    cargo install --path .

## Feeders

Feeders are wired in over time across skills and CLIs (e.g. a `task
archive` shelling out to `feed`). None ship with this crate yet — it's
the substrate they'll write into.
