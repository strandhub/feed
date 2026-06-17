use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use feed::spans::{self, Span};
use feed::{append, default_log_path, Level, Message};

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
    /// bumped phase, preserving its name/total/start time. Errors if the
    /// span isn't open (nothing to advance).
    Advance {
        /// Identity passed to `enter`.
        id: String,
        /// New phase number.
        #[arg(long)]
        phase: u32,
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
        SpanCommand::Advance { id, phase } => {
            let Some(mut span) = spans::read(&dir, &id) else {
                bail!("no open span `{id}` to advance (call `feed span enter` first)");
            };
            span.phase = phase;
            spans::write(&dir, &span)?;
            println!("span advance {}: {}", id, span.label());
            Ok(())
        }
        SpanCommand::Exit { id, message, target, level } => {
            spans::remove(&dir, &id)?;
            let text = message.unwrap_or_else(|| format!("{id} done"));
            append(&default_log_path(), &Message::new(level, &target, &text))?;
            // Names both side effects: the span closed and what settled.
            println!("span exit {id}: {text}");
            Ok(())
        }
    }
}
