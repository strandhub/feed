# feed

A tiny append-only **event** log + **span** ledger shared across
workspace tools, built on `tracing` and shaped as an OTel
`LogRecord` subset on-disk. Crate at `~/projects/feed/` (lib +
`feed` CLI + optional `tracing` bridge feature). JSONL log at
`~/.local/share/claude-status/feed.log`; live spans at
`~/.cache/claude-status/spans/<id>.json`.

For surface (subcommands, flags): `feed --help`.
For producer wiring + on-disk schema: the crate's
`~/projects/feed/README.md`.
For invocation prose (when to emit, which producer path, what
target to use): the `feed` skill's SKILL.md.

## What it is

Two-surface activity substrate for short-lived workspace tools:

- **Events** — immutable points in time, modelled as OTel
  `LogRecord`s: `severity_number`/`severity_text`, `body`,
  optional `event_name` (the OTel Event marker), open
  `attributes` bag (`source`, `operation`, `exception.*`,
  producer-domain fields), optional `trace_id`/`span_id` for
  span correlation. Appended one JSONL line at a time.
- **Spans** — live, mutable cross-process state for **in-progress**
  things (a phasal task mid-phase, a build streaming layers). One
  file per open span: `enter` writes it, `advance` rewrites it,
  `exit` removes it AND drops one settled record into the log
  (with `trace_id`/`span_id` set to the span id).

Two producer paths into the event surface: the `tracing` bridge
(native, Rust producers install `feed::init()` and use `tracing!`
macros) and the `feed` CLI (bypass, for everything else — shell
callers, manual one-liners, non-Rust producers). The crate owns the
**data plane** for both surfaces — the on-disk format and the
read/write primitives — AND the **severity semantics** (WARN =
side-effect blocked but producer recovered; ERROR = producer fell
over; INFO = a real event happened). See the `feed` skill for the
honest-severity contract and the `event_name` naming discipline.

## What it does not own

- **Rendering.** Consumers style by level / surface. `claude-overview`
  is the canonical reader (events widget, `f` to toggle); how it
  styles a row is its concern, not feed's.
- **Polling loops, refresh cadence, widget toggles.** Consumers
  choose their own read rhythm. Feed exposes tail / read primitives
  and nothing more.
- **What a producer chooses to emit as the message text.** Feed
  carries the schema, the bridge, and the severity semantics; the
  producer owns its body text, its `source`, its `operation`, and
  its `event_name` choice. Friction about "the `task` system's
  message text is confusing" routes to *that producer*, not here.
  BUT — if a producer is emitting at a dishonest severity (INFO on
  a run that actually blocked a required side-effect; DEBUG chosen
  to game a consumer's tail filter), that's a contract violation
  against the feed severity semantics and belongs against the
  producer as such.
- **Cross-tool aggregations / dashboards / "what happened this
  week" reports.** Consumers can build them on top of the JSONL;
  feed owns the data plane only.
- **The harness `TaskCreate` / `TaskUpdate` task list.** Separate
  concept — those live in-session in the agent harness; feed lives
  on disk and is multi-process.

## Roots

Single-root. Crate, skill, scope.md, and inbox all live in dotfiles
— feed carries no work-domain specifics, and the substrate is
generic infra usable on any machine the dotfiles target.

## Filing friction

File here when the friction is about the **data plane** — the
JSONL schema, the spans file format, the `feed` CLI verbs, the
`tracing` bridge, the cache path. File against the *producer* when
it's about that producer's emission choices (level, target,
message). File against the *consumer* when it's about rendering,
polling, or widget shape.

When unclear (e.g. "the events widget shows the wrong target for
deploy lines" — is that feed's schema, the producer's target
choice, or the consumer's rendering?), file here and let triage
forward. `issue forward` exists for exactly this.

## Known structural tensions

- **Producer-consumer ownership boundary is non-obvious from the
  file tree.** Feed owns the data plane and the severity semantics;
  the consumer owns rendering and its own filter recipes; producers
  own their body text, `source`/`operation`/`event_name` choices,
  and honesty about severity. This is documented here and in the
  feed skill's body, but a fresh reader looking at the crate alone
  won't see the boundary — it lives in the conventions the crate
  enforces, not the code itself. The skill is the load-bearing
  carrier; if the skill drifts, the boundary blurs.
