---
id: 0001
title: feed system scope.md doesn't carry an integrator map — callers re-derive log path + event shape
filed: 2026-06-18
author: audit-sessions
kind: bug-fix
triage_state: accept
triaged_on: 2026-06-18
flags: {impact: 3, effort: 2, urgency: 3}
---

## Problem

**Crux:** Across 4 sessions this window, agents re-derived feed integration details: log path (`~/.cache/claude-status/feed.log`), event JSON shape, which CLIs emit feed events, and how the `audit:` prefix flows. Each re-derivation is a ~10-call streak of `grep`/`ls`/`Read` across `~/.dotfiles/tools/feed/src/`, `~/.dotfiles/tools/audit/src/`, and `~/.cache/claude-status/`.

**Evidence (R3 streaks, integrator-side):**
- session 7a458955, 2026-06-18 (streak#12, 7 calls): looking for how system-registry would emit feed events
- session 8ee07af8, 2026-06-18 (streak#16, 5 calls): "Locate feed system" → "Check whether system-registry emits feed events" → "Sample recent feed events to see actual filing-event shape"
- session a681ae8b, 2026-06-17 (streak#23, 6 calls): "Find where feed log is written"
- session fa048316, 2026-06-17 (streak#32, 12 calls): "How audit emits ERROR + audit: prefix"

The repeated questions are integrator-shaped: "where does feed get written," "what does an emitted event look like," "how do I emit one from my CLI." Each session's agent had to walk the tools/ source to answer.

**Why this matters:** Feed is the right system to own this map because it's the producer-side contract. The current `~/.claude/systems/feed/scope.md` defines what feed *is*; it doesn't carry a "## Integrators" section answering (a) log path, (b) JSON schema with example event, (c) emit recipe for Rust integrators (`feed::emit` or whatever), (d) which CLIs already emit. Every integrator-shaped session this window would have resolved at the first read with that section present.

**Discussion notes:**
- Distinct framing from a SKILL.md gap: feed has no skill, only a system. The fix lives in scope.md.
- The repeated `grep -n "tracing::info\|tracing::warn"` pattern (sessions 7a458955, c6f0f53a) suggests the integrators were also trying to map their own tracing emissions onto feed events — a worked example would short-circuit that.
