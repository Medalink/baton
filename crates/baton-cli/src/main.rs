mod adapter;
mod commands;
mod control;
mod daemon;
mod endpoint;
mod hook;
mod timefmt;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "baton",
    version,
    about = "Record every coding-agent turn as a git commit"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Agent end-of-turn hook. Reads the payload on stdin, always exits 0, prints nothing.
    Hook { agent: Option<String> },
    /// Run the per-user daemon. Other commands start it when it is absent.
    Daemon,
    /// Start recording a worktree.
    Enroll { path: Option<PathBuf> },
    /// Stop recording a worktree. Its history stays.
    Unenroll { path: Option<PathBuf> },
    /// Record a turn now.
    Turn {
        #[arg(long)]
        thread: String,
        #[arg(long)]
        message: String,
        #[arg(long, default_value = "manual")]
        agent: String,
        #[arg(long)]
        model: Option<String>,
        /// `name=exit code`, repeatable.
        #[arg(long = "check")]
        checks: Vec<String>,
    },
    /// Show recorded turns and the paths each one changed.
    Log { thread: Option<String> },
    /// Put a turn's files back on disk. Without --apply it only shows what would change.
    Rewind {
        /// `thread@seq`, a number (with --thread), or at least 7 digits of a turn id.
        turn: String,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        apply: bool,
    },
    /// Show the daemon and the enrolled worktrees.
    Status,
}

fn main() {
    // Nothing runs before this: the hook must not pay for argument parsing, config, logging or gix.
    let mut args = std::env::args_os().skip(1);
    if args.next().is_some_and(|first| first == "hook") {
        hook::run(args.next().and_then(|a| a.into_string().ok()));
    }

    let cli = Cli::parse();
    let result = match cli.command {
        Command::Hook { .. } => Ok(()),
        Command::Daemon => daemon::run(),
        Command::Enroll { path } => commands::enroll(path.as_deref()),
        Command::Unenroll { path } => commands::unenroll(path.as_deref()),
        Command::Turn {
            thread,
            message,
            agent,
            model,
            checks,
        } => commands::turn(commands::TurnOptions {
            thread,
            message,
            agent,
            model,
            checks,
        }),
        Command::Log { thread } => commands::log(thread.as_deref()),
        Command::Rewind {
            turn,
            thread,
            apply,
        } => commands::rewind(&turn, thread.as_deref(), apply),
        Command::Status => commands::status(),
    };
    if let Err(e) = result {
        eprintln!("baton: {e:#}");
        std::process::exit(1);
    }
}
