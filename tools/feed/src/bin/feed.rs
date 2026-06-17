use anyhow::Result;
use clap::Parser;
use feed::{append, default_log_path, Message, Status};

/// Append a message to the shared activity feed.
///
/// Feeders (skills, CLIs, hooks) call this as a side effect of doing
/// something noteworthy. `claude-overview`'s bottom panel tails the log.
#[derive(Parser)]
#[command(name = "feed", about = "append a message to the shared activity feed")]
struct Cli {
    /// The message text.
    message: String,
    /// Mark the entry as an error (red).
    #[arg(long, conflicts_with = "success")]
    error: bool,
    /// Mark the entry as a success (green). This is the default.
    #[arg(long)]
    success: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let status = if cli.error {
        Status::Error
    } else if cli.success {
        Status::Success
    } else {
        // Default: a bare `feed "..."` is informational.
        Status::Pending
    };
    let path = default_log_path();
    let message = Message::new(status, &cli.message);
    append(&path, &message)?;
    // Mutation verb: one explicit success line naming what landed, so a
    // feeder can trust the append happened without re-reading the log.
    println!("fed [{status:?}] {}", cli.message);
    Ok(())
}
