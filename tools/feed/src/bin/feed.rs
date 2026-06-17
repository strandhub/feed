use anyhow::Result;
use clap::Parser;
use feed::{append, default_log_path, Level, Message};

/// Append an event to the shared activity feed.
///
/// This is the bypass path: non-Rust callers (or a quick manual line)
/// construct the same on-disk schema a Rust producer would emit through
/// `feed`'s tracing layer. `claude-overview`'s events widget tails the log.
#[derive(Parser)]
#[command(name = "feed", about = "append an event to the shared activity feed")]
struct Cli {
    /// The message text.
    message: String,
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let level = if cli.error { Level::Error } else { cli.level };
    let path = default_log_path();
    let message = Message::new(level, &cli.target, &cli.message);
    append(&path, &message)?;
    // Mutation verb: one explicit success line naming what landed, so a
    // caller can trust the append happened without re-reading the log.
    println!("fed [{level}] {}: {}", cli.target, cli.message);
    Ok(())
}
