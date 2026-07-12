---
name: feed
description: |
  The `feed` substrate — a tiny append-only **event** log + **span**
  ledger shared across workspace tools, built on `tracing`. Carries
  the two producer paths (Rust via the `tracing` bridge,
  everything-else via the `feed` CLI), the on-disk schema,
  the events-vs-spans surface split, and the producer-consumer
  ownership rule (the crate owns the data plane; rendering and
  polling belong to consumers like `claude-overview`).

  TRIGGER when editing the `feed` crate at `~/.dotfiles/tools/feed/`
  (auto-fires via paths:), or when about to add a new **producer**
  into the feed — installing the `tracing` bridge in another Rust
  crate's `main()`, shelling to `feed "..." --target X` from a
  script, opening/advancing/closing a span via `feed span`. Also
  TRIGGER on "what is feed / where does it log / how do I emit an
  event / what target should I use," and when inspecting
  `~/.cache/claude-status/feed.log` or `spans/` to understand the
  schema. SKIP for consumer-side work (rendering, polling, widget
  shape — that's the consumer's surface, e.g. `claude-overview`'s
  events widget). SKIP for producer-internal decisions about what
  to emit or at what level (that's the producer's call; feed
  carries the substrate, not the editorial policy). SKIP when
  merely reading a row a feed reader rendered (no schema or
  producer question in play).
paths: "tools/feed/**"
---

# feed

Two-surface activity substrate: append-only **events** (`feed.log`)
and live mutable **spans** (`spans/<id>.json`). Crate at
`~/.dotfiles/tools/feed/`, JSONL log at
`~/.cache/claude-status/feed.log`, spans at
`~/.cache/claude-status/spans/<id>.json`. Read the crate's
`README.md` for the on-disk schema in detail; this skill carries
when-to-reach-for-what and the ownership boundary.

`feed --help` and `feed span --help` are the **authoritative
references** for verbs and flags. Defer to them for syntax. The
system's `scope.md` (`~/.claude/systems/feed/scope.md`) is the
**authoritative reference** for boundary and routing. This skill
encodes what neither carries: which producer path to pick, the
target taxonomy in use, and the rule that keeps feed from
swallowing concerns it doesn't own.

## Two rules carry most of the value

- **Pick the producer path by language, not by convenience.** A Rust
  producer installs the `tracing` bridge once and uses `tracing!`
  macros — that's the native path; the CLI is a *bypass* for
  non-Rust callers, not a shortcut. Shelling to `feed` from inside
  a Rust process is a smell (it forks a subprocess to do what
  `tracing::info!` would do inline).
- **The crate owns the data plane; nothing else.** Friction about
  *what* to emit, *at what level*, or *with what target* is the
  producer's call. Friction about *how a row renders* is the
  consumer's call. Feed owns the JSONL format, the spans file
  format, and the read/write primitives — that's the whole
  contract. When a question crosses the boundary, route per the
  scope's "Filing friction" section.

## Recipes

### Emit a settled event from Rust

```rust
// in main()
feed::init();                               // installs the FeedLayer

// anywhere
tracing::info!(target: "task", "archived xyz");
tracing::error!(target: "deploy", "boom");
```

Add the feature to `Cargo.toml`:

```toml
feed = { path = "../feed", features = ["tracing"] }
```

`feed::init()` honors a `FEED_LOG` env filter (default `info`) and
is best-effort — it never panics or disturbs the producer. Level
encodes outcome: `error` for errors, anything else (info/warn) for
settled/successful events.

### Emit a settled event from a shell / non-Rust caller

```bash
feed "created my-task" --target task
feed "deploy failed" --error              # sugar for --level error
feed "low detail" --level debug --target deploy
```

Each call appends one JSONL line and prints a confirmation:

```
fed [info] task: created my-task
```

### Manage an in-progress span

A span is `enter` → `advance*` → `exit`. One file lives at
`spans/<id>.json` while the span is open; `exit` removes it AND
appends one settled event to the log so the in-progress row
collapses into a log line.

```bash
feed span enter <slug> --phase 1 --of 3 --name "<short label>"
feed span advance <slug> --phase 2
feed span exit <slug> --message "settled line"
```

`<id>` is the stable identity for the span's whole life — by
convention a task slug, but any stable string works. See
`feed span --help` for the full flag set. `phasal-task` is the
canonical caller; consult [[phasal-task]] for the
enter/advance/exit-at-phase-boundary discipline.

## On-disk schema (one place)

Events — JSONL at `~/.cache/claude-status/feed.log`:

```json
{"timestamp":"2026-06-17T15:00:02Z","level":"info","target":"task","message":"…"}
```

`level` ∈ `{trace, debug, info, warn, error}`. Malformed lines
(partial writes, hand-edits) are skipped on read.

Spans — one JSON object per open span at
`~/.cache/claude-status/spans/<id>.json`. Schema lives in the
crate's `src/spans.rs`; the file is rewritten on `advance` and
removed on `exit`.

## Target taxonomy

Targets are free-form strings, but a small set is conventional —
match what producers already use so consumers can group / filter
consistently:

- `task` — the workspace `task` CLI and its skills
- `deploy` — deploy verbs (yggdrasil push, etc.)
- `manual` — default for unscoped CLI calls (`feed` without
  `--target`)

When adding a new producer with a fresh subsystem, pick a short
lowercase noun naming the subsystem. There's no closed enum — file
against `feed` if a taxonomy convention earns codification.

## Ownership boundary (the rule that pays for itself)

The crate owns the **data plane**. Rendering, polling, widget
toggles, and "what does this row mean" are NOT feed's concerns:

- `claude-overview` is a *consumer*. It styles by level and toggles
  with `f`. Friction about its rendering routes to that tool.
- A producer at the wrong level / wrong target / wrong message is
  the **producer's** call. File against the producer.
- Cross-tool aggregations (this week's events, error rates) build
  on the JSONL — feed owns the data, not the report.

This boundary is the load-bearing piece — without it, friction
accretes silently into the wrong inboxes. The system's `scope.md`
"What it does not own" stanza is the contract surface; this skill's
job is to keep the boundary readable from the producer side.

## Known shape questions

- The crate's `README.md` declares spans "deliberately not modelled
  here" — that's currently stale (spans ship). The README catches
  up on the next feed-shaped change. See `feed/scope.md` known
  tensions for the full note.
- Whether the target taxonomy should harden (a closed enum, a
  registered list) is an open question — file against `feed` if
  drift across producers earns codification.
