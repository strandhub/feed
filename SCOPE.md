# feed

A tiny append-only **event** log + **span** ledger shared across
workspace tools, built on `tracing`. Crate at `~/projects/feed/`
(lib + `feed` CLI + optional `tracing` bridge feature). JSONL log at
`~/.cache/claude-status/feed.log`; live spans at
`~/.cache/claude-status/spans/<id>.json`.

For surface (subcommands, flags): `feed --help`.
For producer wiring + on-disk schema: the crate's
`~/projects/feed/README.md`.
For invocation prose (when to emit, which producer path, what
target to use): the `feed` skill's SKILL.md.

## What it is

Two-surface activity substrate for short-lived workspace tools:

- **Events** — immutable points in time, modelled on
  `tracing::Event`: a **level**, a **target**, a **message**. Outcome
  is encoded in the level; an error is `error`, anything else is a
  settled/successful event. Appended one JSONL line at a time.
- **Spans** — live, mutable cross-process state for **in-progress**
  things (a phasal task mid-phase, a build streaming layers). One
  file per open span: `enter` writes it, `advance` rewrites it,
  `exit` removes it AND drops one settled event into the log.

Two producer paths into the event surface: the `tracing` bridge
(native, Rust producers install `feed::init()` and use `tracing!`
macros) and the `feed` CLI (bypass, for everything else — shell
callers, manual one-liners, non-Rust producers). The crate owns the
**data plane** for both surfaces — the on-disk format and the
read/write primitives.

## What it does not own

- **Rendering.** Consumers style by level / surface. `claude-overview`
  is the canonical reader (events widget, `f` to toggle); how it
  styles a row is its concern, not feed's.
- **Polling loops, refresh cadence, widget toggles.** Consumers
  choose their own read rhythm. Feed exposes tail / read primitives
  and nothing more.
- **What a producer chooses to emit, or at what level.** Feed
  carries the schema and the bridge; the producer owns its message
  text, target string, and severity choice. Friction about "the
  `task` system emits at the wrong level" or "the target taxonomy is
  too coarse" routes to *that producer*, not here.
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

- **The crate's own README is currently stale on the span surface.**
  The README (as of 2026-06-17) declares spans "deliberately not
  modelled here," but `feed span enter|advance|exit` ship and the
  in-progress widget reads `spans/<id>.json`. The README catches up
  in the same PR as the next feed-shaped change. Tracked here when
  filed.
- **Producer-consumer ownership boundary is non-obvious from the
  file tree.** Feed owns the data; the consumer owns rendering;
  producers own their own emission choices. This is documented here
  and in the feed skill's body, but a fresh reader looking at the
  crate alone won't see the boundary — it lives in the conventions
  the crate enforces, not the code itself. The skill is the load-
  bearing carrier; if the skill drifts, the boundary blurs.
