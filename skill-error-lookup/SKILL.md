---
name: feed-error-lookup
description: |
  Read-side workflow for the `feed` substrate: when the user wants to
  see today's (or a recent slice of) errors from the activity log and
  drill into the originating Claude thread for the actual failure
  detail. Two hops: filter `~/.cache/claude-status/feed.log` for
  ERROR rows, then locate the corresponding JSONL thread under
  `~/.claude/projects/-/` (or other project dirs) by mtime and read
  the assistant text to see what actually broke.

  TRIGGER on "look at today's feed errors", "what feed errors fired
  this morning", "check the audit failures", "what did the cron
  errors say", or any read-shaped question about recent activity-log
  errors where the user expects a drill-in beyond the one-line feed
  message. SKIP for *producing* events into feed (use the [[feed]]
  skill — write surface). SKIP for non-error feed reads (spans,
  info-level rows, "what happened today" — general consumer
  questions, not error triage). SKIP when the user wants
  live-streamed feed activity (that's `claude-overview`'s job).
  SKIP when the user wants to search the JSONL thread corpus
  generally, with no feed-log entrypoint ("find the last time I
  did X", "grep all threads for Y") — that's
  [[prior-thread-search]]. This skill is specifically the
  feed-log → originating-thread join.

  ⚠ FIRST-DRAFT skill — formalize or fold into [[feed]] by
  2026-06-27. See the "Formalization deadline" section at the
  bottom. Don't let this skill rot at v0.
---

# feed-error-lookup

Read errors out of the feed log, then drill into the originating
Claude thread for the actual failure detail. The feed message is
intentionally terse ("claude exited with exit status: 1"); the real
error text lives in the JSONL thread that the failed `claude -p`
call produced. For schema, level vocabulary, and the on-disk path,
see `~/.dotfiles/tools/feed/README.md` and the [[feed]] skill —
this skill is the read-join workflow, not the data-plane spec.

## Recipe

**1. Pull today's error rows from feed.log:**

```sh
jq -c 'select(.level == "ERROR" or .level == "error")' \
   ~/.cache/claude-status/feed.log \
| jq -r 'select((.timestamp // "") | startswith("'"$(date -u +%Y-%m-%d)"'"))'
```

(Swap the `startswith` date for any UTC-day prefix to look at other
days. The `level` field is `"error"` from the feed CLI but `"ERROR"`
from the tracing bridge — match both.)

**2. Find candidate JSONL threads by mtime.**

Each failed `claude -p` invocation writes a thread under
`~/.claude/projects/<cwd-encoded>/`. The cwd encoding swaps `/` for
`-`, so a service with cwd=`/` lands under `~/.claude/projects/-/`.
List today's threads across all project dirs and match by mtime to
the feed error timestamps:

```sh
for d in ~/.claude/projects/*/; do
  find "$d" -maxdepth 1 -name '*.jsonl' \
       -newermt "$(date +%Y-%m-%d)" \
       -not -newermt "$(date -d tomorrow +%Y-%m-%d)" 2>/dev/null
done
```

Then `stat -c '%y %n' <file>` each match to read off the mtime
(local time) and align with the feed error timestamps (UTC — apply
your TZ offset).

**3. Pull the assistant text from the thread** — that's where the
error message lives:

```sh
jq -r 'select(.type == "assistant")
       | .message.content
       | if type == "string" then .
         elif type == "array" then (map(.text // "") | join(" "))
         else "" end' \
   /home/jst/.claude/projects/-/<thread-id>.jsonl \
| head -5
```

Typical errors you'll see this way: `API Error: 401 Invalid
authentication credentials`, `rate_limit_exceeded`, prompt-injection
guard refusals, model-side tool-call schema failures.

## Why claude-thread isn't the entrypoint (yet)

`claude-thread` is the registered read-CLI for the JSONL thread
corpus, but it's hardwired to
`~/.claude/projects/-home-jst-workspace/` and silently fails on
threads under other project dirs (including `-/`, where
yggdrasil systemd services land because they run with cwd=/).

Once `claude-thread list` returns threads across all project dirs
(or accepts a `--project` flag), step 2 above collapses to:

```sh
claude-thread list | jq -c 'select(.mtime_unix >= '"$(date -d 'today 00:00' +%s)"')'
```

and step 3 to:

```sh
claude-thread events <id> | jq -r 'select(.type == "assistant") | ...'
```

That's the formalization target.

## Formalization deadline

**This skill is a first-draft holding pattern.** It exists because
"look at feed errors" is recurring friction but the proper
solution requires shape changes in two other systems (`feed` —
needs a query verb; `claude-thread` — needs cross-project
listing). When either lands, fold this skill into the relevant
authoritative skill ([[feed]] or [[prior-thread-search]]) and
delete this file.

**Hard deadline: 2026-06-27.** If this file still exists on
2026-06-28 in its current first-draft shape, the skill needs
either (a) promotion to a real skill with proper rubric pass,
or (b) deletion because the underlying CLI fix made it
redundant. Either way, do not let it persist at v0.

Check on 2026-06-27:
- Has `feed` grown a read/query verb? → fold the step-1 jq
  recipe into the [[feed]] skill, delete this.
- Has `claude-thread` grown cross-project support? → fold the
  step-2/3 drill-in into [[prior-thread-search]], delete this.
- Neither? → run this skill through [[skill-authoring]]'s
  rubric and ship a real v1, OR file an issue against `feed`
  / `system-registry` to force one of the CLI fixes.
