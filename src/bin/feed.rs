use anyhow::{bail, Result};
use clap::{Parser, Subcommand, ValueEnum};
use feed::spans::{self, Span};
use feed::{
    append, append_usage, default_log_path, default_usage_log_path, Level, Message, Usage,
    UsageKind,
};

/// Append an event to the shared activity feed, or manage an in-progress
/// span.
///
/// The bare form is the event bypass: non-Rust callers (or a quick manual
/// line) construct the same on-disk schema a Rust producer would emit
/// through `feed`'s tracing layer. `claude-overview`'s events widget tails
/// the log. The `span` subcommand manages the live in-progress state the
/// in-progress widget reads (one file per open span).
#[derive(Parser)]
#[command(name = "feed", about = "append an event to the shared activity feed")]
struct Cli {
    /// The event message text. Omit when using a subcommand.
    message: Option<String>,
    /// Severity. Outcome is encoded here: an error is `--level error`;
    /// anything else is a settled/successful event.
    #[arg(long, default_value = "info")]
    level: Level,
    /// Subsystem that emitted this (e.g. `task`, `deploy`). Shown and
    /// filterable in the consumer.
    #[arg(long, default_value = "manual")]
    target: String,
    /// Sugar for `--level error`.
    #[arg(long)]
    error: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Manage an in-progress span (the live state the in-progress widget
    /// reads). A span is enter → advance* → exit; exit also drops a
    /// settled event into the log so the row collapses into the feed.
    #[command(subcommand)]
    Span(SpanCommand),

    /// Log one tool-invocation event to the usage log.
    ///
    /// Skill-mode events (--kind skill) record that the router pulled a
    /// skill's guidance into context — NOT that the guidance shaped
    /// what followed. For staleness queries this is fine; for "which
    /// skills earn their mental cost" it's a directional but noisy
    /// proxy. See tasks/0153-tooling-usage-metrics for the design
    /// story.
    ///
    /// The bypass path: non-Rust callers (the `PreToolUse` Skill hook,
    /// a shell wrapper) construct the same on-disk shape a Rust
    /// producer would emit via `feed::log_cli_invocation()`.
    Usage {
        /// Event kind — closed vocabulary; the whole valid set is:
        #[arg(long, value_enum)]
        kind: UsageKindArg,
        /// Binary name (for `--kind cli`) or skill name (for
        /// `--kind skill`).
        #[arg(long)]
        name: String,
        /// Skill-mode: the raw `args` string from the Skill tool
        /// payload (whatever the caller typed after the skill name).
        #[arg(long)]
        args: Option<String>,
        /// Skill-mode: the Claude Code session UUID from the hook
        /// payload — lets a consumer group skill loads by session.
        #[arg(long)]
        session: Option<String>,
        /// CLI-mode: one argv element (excluding argv[0]). Repeat to
        /// pass multiple: `--argv log --argv -m --argv "hi"`. Each
        /// element is truncated to 512 bytes so the line stays under
        /// POSIX PIPE_BUF (4 KiB) for cross-process append atomicity.
        #[arg(long)]
        argv: Vec<String>,
        /// Cwd at invocation time. Defaults to the process cwd when
        /// omitted; the hook path overrides with the caller's cwd from
        /// its payload.
        #[arg(long)]
        cwd: Option<String>,
    },
}

/// CLI-facing enum for `--kind`, kept separate from the library's
/// `UsageKind` so clap can derive `ValueEnum`. The `possible values`
/// list rendered in `--help` is the whole valid set — no upstream doc
/// lookup needed.
#[derive(Copy, Clone, Debug, ValueEnum)]
#[value(rename_all = "lowercase")]
enum UsageKindArg {
    Cli,
    Skill,
}

impl From<UsageKindArg> for UsageKind {
    fn from(k: UsageKindArg) -> Self {
        match k {
            UsageKindArg::Cli => UsageKind::Cli,
            UsageKindArg::Skill => UsageKind::Skill,
        }
    }
}

#[derive(Subcommand)]
enum SpanCommand {
    /// Open a span: write `spans/<id>.json`. `id` is the stable identity
    /// for the span's whole life (a task slug is the conventional choice).
    Enter {
        /// Stable identity; reused by `advance`/`exit`.
        id: String,
        /// Human label for the row (e.g. the task slug). Defaults to `id`.
        #[arg(long)]
        name: Option<String>,
        /// Current phase, 1-based.
        #[arg(long, default_value = "1")]
        phase: u32,
        /// Total phase count, rendered as `phase/total`. Omit if unknown.
        #[arg(long)]
        total: Option<u32>,
    },
    /// Advance an open span to a new phase: rewrite the same file with a
    /// bumped phase, preserving its total/start time. The name is
    /// preserved unless `--name` is given (useful when each phase is a
    /// distinct unit — e.g. the issue being triaged).
    Advance {
        /// Identity passed to `enter`.
        id: String,
        /// New phase number.
        #[arg(long)]
        phase: u32,
        /// Replacement human label for the row. Omit to keep the existing
        /// name (the phase-bump-only behavior).
        #[arg(long)]
        name: Option<String>,
    },
    /// Close a span: delete `spans/<id>.json` AND append one settled event
    /// to the feed log, so the in-progress row collapses into a log line.
    /// Idempotent on the file removal (a never-entered id just logs).
    Exit {
        /// Identity passed to `enter`.
        id: String,
        /// Settled event text. Defaults to `<id> done`.
        #[arg(long)]
        message: Option<String>,
        /// Subsystem for the settled event.
        #[arg(long, default_value = "span")]
        target: String,
        /// Severity for the settled event. Use `error` to mark the run
        /// as failed; default `info` is a settled success.
        #[arg(long, default_value = "info")]
        level: Level,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Span(span_cmd)) => {
            if cli.message.is_some() {
                bail!("provide either an event message or a `span` subcommand, not both");
            }
            run_span(span_cmd)
        }
        Some(Command::Usage {
            kind,
            name,
            args,
            session,
            argv,
            cwd,
        }) => {
            if cli.message.is_some() {
                bail!("provide either an event message or a `usage` subcommand, not both");
            }
            let kind: UsageKind = kind.into();
            // Kind-specific field validation: reject the wrong-shape
            // fields at parse-time so a hook misconfigured with e.g.
            // --argv on a skill event bails loudly instead of writing
            // a garbled row.
            match kind {
                UsageKind::Cli => {
                    if args.is_some() || session.is_some() {
                        bail!("--args and --session are skill-mode fields; unused with --kind cli");
                    }
                }
                UsageKind::Skill => {
                    if !argv.is_empty() {
                        bail!("--argv is a cli-mode field; unused with --kind skill");
                    }
                }
            }
            let cwd = cwd.or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            });
            let ev = Usage {
                ts: chrono::Utc::now(),
                kind,
                name: name.clone(),
                cwd,
                argv: if matches!(kind, UsageKind::Cli) {
                    // Truncate here to match the Rust-side Usage::cli
                    // behavior (cap at 512 bytes/element).
                    Some(argv.into_iter().map(truncate_argv).collect())
                } else {
                    None
                },
                args,
                session,
            };
            append_usage(&default_usage_log_path(), &ev)?;
            // Mutation verb: one explicit success line naming what
            // landed. The callsite (a hook, a shell wrapper) reads this
            // to confirm the append happened.
            let kind_str = match kind {
                UsageKind::Cli => "cli",
                UsageKind::Skill => "skill",
            };
            println!("logged usage: {kind_str} {name}");
            Ok(())
        }
        None => {
            let Some(message) = cli.message else {
                bail!("a message is required (or use the `span` subcommand)");
            };
            let level = if cli.error { Level::Error } else { cli.level };
            let path = default_log_path();
            let msg = Message::new(level, &cli.target, &message);
            append(&path, &msg)?;
            // Mutation verb: one explicit success line naming what landed.
            println!("fed [{level}] {}: {}", cli.target, message);
            Ok(())
        }
    }
}

/// Cap each `--argv` element at 512 bytes so a single usage line stays
/// under POSIX PIPE_BUF (4 KiB) — the atomic-append threshold for
/// concurrent writers. Mirrors `feed::usage::truncate_argv_element`;
/// duplicated (not re-exported) to keep the library's public surface
/// small.
fn truncate_argv(s: String) -> String {
    const CAP: usize = 512;
    if s.len() <= CAP {
        return s;
    }
    let mut end = CAP;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + 3);
    out.push_str(&s[..end]);
    out.push('…');
    out
}

fn run_span(cmd: SpanCommand) -> Result<()> {
    let dir = spans::spans_dir();
    match cmd {
        SpanCommand::Enter { id, name, phase, total } => {
            let name = name.unwrap_or_else(|| id.clone());
            let span = Span::enter(&id, &name, phase, total);
            spans::write(&dir, &span)?;
            // Mutation verb success line: name what's now live.
            println!("span enter {}: {}", id, span.label());
            Ok(())
        }
        SpanCommand::Advance { id, phase, name } => {
            let Some(mut span) = spans::read(&dir, &id) else {
                bail!("no open span `{id}` to advance (call `feed span enter` first)");
            };
            span.advance(phase, name);
            spans::write(&dir, &span)?;
            println!("span advance {}: {}", id, span.label());
            Ok(())
        }
        SpanCommand::Exit { id, message, target, level } => {
            let text = message.unwrap_or_else(|| format!("{id} done"));
            spans::exit(&dir, &default_log_path(), &id, level, &target, &text)?;
            // Names both side effects: the span closed and what settled.
            println!("span exit {id}: {text}");
            Ok(())
        }
    }
}
