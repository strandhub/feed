use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use feed::spans::{self, Span};
use feed::{append, default_log_path, LogRecord, SeverityNumber};
use serde_json::Value;

/// Append a record to the shared activity feed, or manage an in-progress
/// span.
///
/// The bare form is the record bypass: non-Rust callers (or a quick
/// manual line) construct the same on-disk OTel `LogRecord` shape a Rust
/// producer would emit through `feed`'s tracing layer. `claude-overview`'s
/// events widget tails the log. The `span` subcommand manages the live
/// in-progress state the in-progress widget reads (one file per open
/// span). The `usage` subcommand emits an invocation Event — the same
/// shape `feed::log_cli_invocation()` produces in-process.
#[derive(Parser)]
#[command(
    name = "feed",
    about = "append an OTel-shaped record to the shared activity feed"
)]
struct Cli {
    /// The record body text. Omit when using a subcommand.
    body: Option<String>,

    /// Severity — closed vocabulary. Producers emit at the level that
    /// reflects the *event's* actual impact, not what any consumer wants
    /// filtered. See CLAUDE.md for the honest-severity principle.
    #[arg(long, default_value = "info", value_parser = parse_severity)]
    severity: SeverityNumber,

    /// Producer identity — lands in `attributes.source` (OTel's
    /// `service.name` shorthand). Shown and filterable in the consumer.
    #[arg(long, default_value = "manual")]
    source: String,

    /// OTel Event marker — when set, this row is an Event with a stable
    /// static name (`task.invoked`, `audit.run.failed`, `skill.loaded`).
    /// Use dotted `<subsystem>.<action>[.<state>]` form; dynamic data
    /// goes in `--attr`, never in the name.
    #[arg(long)]
    event_name: Option<String>,

    /// Structured attribute, repeatable: `--attr key=value`. Values are
    /// stored as strings; consumers can parse further.
    #[arg(long = "attr", value_parser = parse_attr)]
    attrs: Vec<(String, String)>,

    /// Sugar for `--severity error`.
    #[arg(long, conflicts_with = "severity")]
    error: bool,

    /// Sugar for `--severity warn`.
    #[arg(long, conflicts_with_all = ["severity", "error"])]
    warn: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

fn parse_severity(s: &str) -> std::result::Result<SeverityNumber, String> {
    s.parse()
}

fn parse_attr(s: &str) -> std::result::Result<(String, String), String> {
    let Some((k, v)) = s.split_once('=') else {
        return Err(format!("expected key=value, got `{s}`"));
    };
    if k.is_empty() {
        return Err("attribute key cannot be empty".into());
    }
    Ok((k.to_string(), v.to_string()))
}

#[derive(Subcommand)]
enum Command {
    /// Manage an in-progress span (the live state the in-progress widget
    /// reads). A span is enter → advance* → exit; exit also drops a
    /// settled record into the log so the row collapses into the feed.
    #[command(subcommand)]
    Span(SpanCommand),

    /// Emit an invocation Event — one row with `event_name:
    /// "<name>.invoked"` (cli) or `event_name: "skill.loaded"` (skill).
    ///
    /// Severity is INFO because a CLI actually running is a real event;
    /// consumers that don't want invocation records filter by
    /// `event_name` matching `*.invoked`, not by severity.
    ///
    /// The bypass path: non-Rust callers (the `PreToolUse` Skill hook,
    /// a shell wrapper) construct the same on-disk shape a Rust
    /// producer would emit via `feed::log_cli_invocation()`.
    Usage {
        /// Invocation kind.
        #[arg(long, value_enum)]
        kind: UsageKind,
        /// Binary name (`--kind cli`) or skill name (`--kind skill`).
        #[arg(long)]
        name: String,
        /// Skill-mode: the raw `args` string from the Skill tool
        /// payload — lands in `attributes.skill_args`.
        #[arg(long)]
        args: Option<String>,
        /// Skill-mode: the Claude Code session UUID — lands in
        /// `attributes.session`.
        #[arg(long)]
        session: Option<String>,
        /// CLI-mode: one argv element (excluding argv[0]). Repeat to
        /// pass multiple: `--argv log --argv -m --argv "hi"`. Each
        /// element is truncated to 512 bytes so the line stays under
        /// POSIX PIPE_BUF (4 KiB) for cross-process append atomicity.
        /// Lands in `attributes.argv` as a JSON array.
        #[arg(long)]
        argv: Vec<String>,
        /// Cwd at invocation time — lands in `attributes.cwd`. Defaults
        /// to the process cwd when omitted; the hook path overrides with
        /// the caller's cwd from its payload.
        #[arg(long)]
        cwd: Option<String>,
    },
}

#[derive(Copy, Clone, Debug, clap::ValueEnum)]
#[value(rename_all = "lowercase")]
enum UsageKind {
    Cli,
    Skill,
}

#[derive(Subcommand)]
enum SpanCommand {
    /// Open a span: write `spans/<id>.json`. `id` is the stable identity
    /// for the span's whole life (a task slug is the conventional
    /// choice); it also becomes `trace_id`/`span_id` on the settled row
    /// [`exit`](feed::spans::exit) emits.
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
    /// Close a span: delete `spans/<id>.json` AND append one settled
    /// record to the feed log (with `trace_id`/`span_id` = the span id),
    /// so the in-progress row collapses into a log line. Idempotent on
    /// the file removal (a never-entered id just logs).
    Exit {
        /// Identity passed to `enter`.
        id: String,
        /// Settled body text. Defaults to `<id> done`.
        #[arg(long)]
        body: Option<String>,
        /// Producer identity for the settled record.
        #[arg(long, default_value = "span")]
        source: String,
        /// Severity for the settled record. Use `warn` when a required
        /// side-effect was blocked (recovered), `error` to mark the run
        /// as failed; default `info` is a settled success.
        #[arg(long, default_value = "info", value_parser = parse_severity)]
        severity: SeverityNumber,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Span(span_cmd)) => {
            if cli.body.is_some() {
                bail!("provide either a record body or a `span` subcommand, not both");
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
            if cli.body.is_some() {
                bail!("provide either a record body or a `usage` subcommand, not both");
            }
            // Kind-specific field validation: reject the wrong-shape
            // fields at parse-time so a hook misconfigured with e.g.
            // --argv on a skill event bails loudly instead of writing a
            // garbled row.
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
            let (event_name, body, source) = match kind {
                UsageKind::Cli => (format!("{name}.invoked"), format!("{name} invoked"), name.clone()),
                UsageKind::Skill => ("skill.loaded".to_string(), format!("skill {name} loaded"), "skill".to_string()),
            };
            let mut record = LogRecord::event(SeverityNumber::Info, source, event_name.clone(), body);
            match kind {
                UsageKind::Cli => {
                    record
                        .attributes
                        .insert("cli_name".to_string(), Value::String(name.clone()));
                    let argv: Vec<String> = argv.into_iter().map(truncate_argv).collect();
                    record.attributes.insert(
                        "argv".to_string(),
                        serde_json::to_value(argv).unwrap_or(Value::Null),
                    );
                }
                UsageKind::Skill => {
                    record
                        .attributes
                        .insert("skill_name".to_string(), Value::String(name.clone()));
                    if let Some(a) = args {
                        record
                            .attributes
                            .insert("skill_args".to_string(), Value::String(a));
                    }
                    if let Some(s) = session {
                        record
                            .attributes
                            .insert("session".to_string(), Value::String(s));
                    }
                }
            }
            if let Some(cwd) = cwd {
                record
                    .attributes
                    .insert("cwd".to_string(), Value::String(cwd));
            }
            append(&default_log_path(), &record)?;
            // Mutation verb: one explicit success line naming what
            // landed. The callsite (a hook, a shell wrapper) reads this
            // to confirm the append happened.
            println!("logged usage: {event_name} ({name})");
            Ok(())
        }
        None => {
            let Some(body) = cli.body else {
                bail!("a body is required (or use a subcommand)");
            };
            let severity = if cli.error {
                SeverityNumber::Error
            } else if cli.warn {
                SeverityNumber::Warn
            } else {
                cli.severity
            };
            let mut record = LogRecord::new(severity, &cli.source, &body);
            if let Some(name) = cli.event_name {
                record.event_name = Some(name);
            }
            for (k, v) in cli.attrs {
                record.attributes.insert(k, Value::String(v));
            }
            append(&default_log_path(), &record)?;
            // Mutation verb: one explicit success line naming what landed.
            println!("fed [{severity}] {}: {}", cli.source, body);
            Ok(())
        }
    }
}

/// Cap each `--argv` element at 512 bytes so a single usage line stays
/// under POSIX PIPE_BUF (4 KiB) — the atomic-append threshold for
/// concurrent writers.
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
        SpanCommand::Exit {
            id,
            body,
            source,
            severity,
        } => {
            let text = body.unwrap_or_else(|| format!("{id} done"));
            spans::exit(&dir, &default_log_path(), &id, severity, &source, &text)?;
            println!("span exit {id}: {text}");
            Ok(())
        }
    }
}
