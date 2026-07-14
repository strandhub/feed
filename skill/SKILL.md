---
name: feed
description: |
  The `feed` substrate — a tiny append-only **event** log + **span**
  ledger shared across workspace tools, built on `tracing` and shaped
  as an OTel `LogRecord` subset on-disk. Carries the two producer
  paths (Rust via the `tracing` bridge, everything-else via the
  `feed` CLI), the on-disk schema, the events-vs-spans surface split,
  the producer-consumer ownership rule (the crate owns the data
  plane; rendering and polling belong to consumers), the honest-
  severity principle (producers emit at the level reflecting the
  event's actual impact; consumers filter), and the `event_name`
  discipline that turns a log into an OTel Event.

  TRIGGER when editing the `feed` crate at `~/projects/feed/`
  (auto-fires via paths:), or when about to add a new **producer**
  into the feed — installing the `tracing` bridge in another Rust
  crate's `main()`, shelling to `feed "..." --source X` from a
  script, opening/advancing/closing a span via `feed span`. Also
  TRIGGER on "what is feed / where does it log / how do I emit an
  event / what severity should I use / what event_name should I
  set," and when inspecting `~/.local/share/claude-status/feed.log`
  or `spans/` to understand the schema. Especially fires when the
  question is **at what severity** — WARN vs INFO vs ERROR is a
  contract now, not a personal call. SKIP for consumer-side work
  (rendering, polling, widget shape — that's the consumer's
  surface, e.g. `claude-overview`'s events widget). SKIP for
  producer-internal decisions about **what to say** (message text,
  business logic — that's the producer's call; feed carries the
  substrate + severity semantics, not the copywriting). SKIP when
  merely reading a row a feed reader rendered (no schema or
  producer question in play).
paths: "projects/feed/**"
---

# feed

Two-surface activity substrate: append-only **events** (`feed.log`,
one OTel `LogRecord` per JSON line) and live mutable **spans**
(`spans/<id>.json`). Crate at `~/projects/feed/`, JSONL log at
`~/.local/share/claude-status/feed.log`, spans at
`~/.cache/claude-status/spans/<id>.json`. Read the crate's
`README.md` for the on-disk schema in detail; this skill carries
when-to-reach-for-what, the ownership boundary, and the discipline
that keeps producers emitting honestly.

`feed --help` and `feed span --help` are the **authoritative
references** for verbs and flags. Defer to them for syntax. The
system's `scope.md` (`~/projects/feed/SCOPE.md`) is the
**authoritative reference** for boundary and routing. This skill
encodes what neither carries: which producer path to pick, the
severity semantics, the event_name discipline, and the rule that
keeps feed from swallowing concerns it doesn't own.

## The rules that carry the value

- **Pick the producer path by language, not by convenience.** A Rust
  producer installs the `tracing` bridge once and uses `tracing!`
  macros — that's the native path; the CLI is a *bypass* for
  non-Rust callers, not a shortcut. Shelling to `feed` from inside
  a Rust process is a smell (it forks a subprocess to do what
  `tracing::info!` would do inline).
- **The crate owns the data plane; nothing else.** Friction about
  *how a row renders* is the consumer's call. Feed owns the JSONL
  format (an OTel `LogRecord` subset), the spans file format, and
  the read/write primitives — that's the whole contract. When a
  question crosses the boundary, route per the scope's "Filing
  friction" section.
- **Producers emit at HONEST severity; consumers filter.** This is
  a contract, grounded in OTel spec + community consensus. Pick the
  severity that reflects the *event's actual impact*, not what any
  downstream consumer wants filtered. `task new` succeeding is INFO
  from `task`'s point of view — a real event happened. Don't
  downgrade to DEBUG "so claude-overview filters it" — that's the
  anti-pattern the community rejects; consumers apply their own
  filter recipes. See "Severity semantics" below for the vocabulary.
- **Named occurrences use `event_name`.** OTel's Event marker: a
  stable static dotted name (`task.invoked`, `audit.run.failed`,
  `skill.loaded`) turns a plain log into an Event. Dynamic data
  goes in attributes, NEVER in the name. Consumers filter on
  `event_name` to separate routine invocations from noteworthy
  state changes.

## Severity semantics (the contract)

Load-bearing definitions. Pick the level that matches the *event*,
not the audience:

- **`error`** — the producer itself fell over. Non-zero exit, crash,
  hard failure. Redundancy layer on top of systemd/cron. Carries
  `exception.type` / `exception.message` / `exception.stacktrace`
  attributes when a real error is in hand.
- **`warn`** — a required side-effect got blocked, but the producer
  recovered enough to keep going. The auditor-emits-"done"-but-
  trailer-is-malformed case. If a reader downstream depends on
  something you couldn't produce, that's WARN. This is the signal
  that fixes flying-blind on background work — the one 0157 exists
  to make cheap.
- **`info`** — a real event happened. Task lifecycle transitions,
  span closes, invocation records, deploy success. Producer-honest:
  `feed::log_cli_invocation()` emits at INFO because a CLI actually
  running is a real event, not diagnostic noise.
- **`debug` / `trace`** — the producer's own diagnostic output.
  Filtered out in prod by the default `FEED_LOG=info` filter.
  Reach for these when the emit is genuinely for debugging feed
  itself, not for a downstream reader.

**Producers do not downgrade for consumer convenience** and **do not
inflate for alarm value**. Both are anti-patterns. If a consumer's
tail feels noisy, the consumer widens its filter; if a consumer
misses a signal, the consumer tightens its filter or the *event*
gets promoted to WARN honestly.

## `event_name` discipline

An OTel Event is a `LogRecord` with `event_name` set — a stable
static name identifying the kind of occurrence. Naming rules:

- **Static and dotted:** `<subsystem>.<action>[.<state>]`.
  `task.invoked`, `audit.run.completed`, `audit.run.degraded`,
  `audit.run.failed`, `skill.loaded`, `deploy.pushed`.
- **Never dynamic:** the name is a *type* of event, not an instance.
  Task-slug, error-message, cli-argv all go in `attributes`, never
  in the name. `task.new.foo-bar-baz` is wrong; `task.new.completed`
  with `attributes.slug: "foo-bar-baz"` is right.
- **`*.invoked` for invocation records** — every CLI's routine
  invocation. `feed::log_cli_invocation("task")` produces
  `task.invoked` automatically.
- **`*.loaded` for load records** — skill loads via the PreToolUse
  hook emit `skill.loaded`.
- **`*.degraded` for the WARN case** — a run finished but a required
  side-effect was blocked. Pair with attribute `degraded_reason`
  naming what got blocked.
- **`*.failed` for the ERROR case** — the producer fell over. Pair
  with `exception.*` attributes.
- **Absent for plain editorial logs** — `tracing::info!(target:
  "task", "archived xyz")` doesn't need an event_name. Only set
  one when the row categorizes as an OTel Event.

Consumers filter on prefix / suffix (`*.invoked`, `*.degraded`) to
scope their read; keeping names in a dotted namespace makes those
filters cheap.

## Attributes we standardize on

The `attributes` bag is open, but a small vocabulary is universal —
match it so consumers can group / filter consistently:

- **`source`** — which producer emitted this. Set automatically
  from `tracing`'s `target:` (or overridable with `--source` on the
  CLI). Free-form short lowercase noun.
- **`operation`** — which verb/step inside the producer.
  `audit-run`, `task-archive`, `deploy-push`. Pair with a source.
- **`event_name` (top-level, not an attribute)** — see above.
- **`exception.type` / `exception.message` / `exception.stacktrace`**
  — OTel exception convention, populated on ERROR rows carrying a
  real failure.
- **`degraded_reason`** — on WARN rows, name what side-effect got
  blocked in one sentence.

Producer-domain attrs (`cli_name`, `argv`, `cwd`, `session`,
`slug`, `duration_s`) are fine — the schema is open. Just don't
overwrite the standard keys with different meanings.

## Recipes

### Emit a settled event from Rust

```rust
// in main()
feed::init();                               // installs the FeedLayer

// anywhere — plain log
tracing::info!(target: "task", "archived xyz");

// an OTel Event (a log with event_name)
tracing::info!(
    target: "task",
    event_name = "task.archive.completed",
    slug = "0142-foo",
    "archived xyz",
);

// a WARN with a degraded_reason
tracing::warn!(
    target: "audit",
    event_name = "audit.run.degraded",
    degraded_reason = "trailer written without a TL;DR",
    auditor = "cli-verb-error",
    "audit run cli-verb-error degraded",
);

// an ERROR with OTel exception attributes
tracing::error!(
    target: "deploy",
    event_name = "deploy.pushed.failed",
    "exception.type" = "std::io::Error",
    "exception.message" = %e,
    "deploy failed",
);
```

Add the feature to `Cargo.toml`:

```toml
feed = { git = "https://github.com/strandhub/feed", features = ["tracing"] }
```

`feed::init()` honors a `FEED_LOG` env filter (default `info`) and
is best-effort — it never panics or disturbs the producer.

### Emit from a shell / non-Rust caller

```bash
feed "created my-task" --source task
feed --warn --source audit --attr degraded_reason="trailer missing tldr" \
     --event-name audit.run.degraded "audit run cli-verb-error degraded"
feed --error --source deploy --attr exception.type=IoError "deploy failed"
```

Each call appends one JSONL line and prints a confirmation:

```
fed [info] task: created my-task
```

### Log a CLI invocation (auto-shape)

```rust
// at top of main(), after feed::init()
feed::log_cli_invocation("task");
```

Produces an INFO row with `event_name: "task.invoked"`,
`attributes.cli_name`, `attributes.argv`, `attributes.cwd`. Standard
shape; use this rather than hand-rolling.

### Manage an in-progress span

A span is `enter` → `advance*` → `exit`. One file lives at
`spans/<id>.json` while the span is open; `exit` removes it AND
appends one settled record to the log (with `trace_id`/`span_id`
set to the span id, per OTel correlation) so the in-progress row
collapses into a log line.

```bash
feed span enter <slug> --phase 1 --total 3 --name "<short label>"
feed span advance <slug> --phase 2
feed span exit <slug> --body "settled line" --severity warn
```

`<id>` is the stable identity for the span's whole life — by
convention a task slug, but any stable string works. See
`feed span --help` for the full flag set. `phasal-task` is the
canonical caller; consult [[phasal-task]] for the
enter/advance/exit-at-phase-boundary discipline.

## On-disk schema (one place)

Events — JSONL at `~/.local/share/claude-status/feed.log`, one OTel
`LogRecord` per line:

```json
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
```

Fields:
- `severity_number` — OTel numeric: 1/5/9/13/17 for TRACE/DEBUG/
  INFO/WARN/ERROR. Ordered for `>=` filters.
- `severity_text` — uppercase name, same info as the number.
- `body` — the human message.
- `event_name` — optional; when present, this row is an OTel Event.
- `attributes` — open bag; empty maps omitted from the wire.
- `trace_id` / `span_id` — optional; set when a span was open.

**Dropped from OTel** (we're one machine, one file, not a
distributed collector): `resource`, `instrumentation_scope`,
`trace_flags`.

Spans — one JSON object per open span at
`~/.cache/claude-status/spans/<id>.json`. Schema lives in the
crate's `src/spans.rs`; the file is rewritten on `advance` and
removed on `exit`.

Malformed lines (partial writes, hand-edits, pre-OTel-shape rows
from before 2026-07-14) are skipped on read.

## Consumer filter recipes

Consumers apply their own severity + event_name filters — that's
the whole point of the honest-severity contract. Common recipes:

- **`claude-overview` editorial tail** — hide `event_name` matching
  `*.invoked` and `skill.loaded` below WARN; always show WARN+
  regardless of event_name.
- **0153-shape usage aggregator** — filter *in* on `event_name`
  matching `*.invoked` or `skill.loaded`; ignore severity.
- **Health reader** — `severity_number >= 13` (WARN and above),
  group by `attributes.source` + `attributes.operation`.
- **`jq` one-liner** — `jq 'select(.severity_number >= 13)'` on the
  log file.

The OTel Collector's `filterprocessor` treats severity, attribute,
and event_name as peer filter primitives; multi-consumer is
expressed as multiple pipelines over one source. Our shape matches.

## Ownership boundary (the rule that pays for itself)

The crate owns the **data plane and the severity semantics**.
Rendering, polling, widget toggles, and "what does this row mean
for my dashboard" are NOT feed's concerns:

- `claude-overview` is a *consumer*. It styles by severity and
  toggles with `f`. Friction about its rendering routes to that
  tool.
- A producer at the wrong severity is the **producer's** call *if*
  the severity is genuinely honest for their event. If the severity
  is being chosen to game a downstream consumer's filter, that's a
  contract violation and belongs against the producer — the fix is
  emit-honestly + fix-the-consumer-filter.
- Cross-tool aggregations (this week's WARN rate, source X's
  degraded runs) build on the JSONL — feed owns the data, not the
  report.

This boundary is load-bearing — without it, friction accretes
silently into the wrong inboxes. The system's `SCOPE.md` "What it
does not own" stanza is the contract surface; this skill's job is
to keep the boundary and the discipline readable from the producer
side.

## Known shape questions

- Whether `event_name` should be a closed enum (per-subsystem
  namespaces registered somewhere) or stay free-form as it is —
  file against `feed` if drift across producers earns
  codification.
- Whether the two-file split (feed.log at `~/.local/share/` +
  spans at `~/.cache/`) should collapse into one home. Not
  biting; spans have distinct lifecycle semantics.
