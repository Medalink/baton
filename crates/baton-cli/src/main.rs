mod adapter;
mod commands;
mod control;
mod daemon;
mod endpoint;
mod hook;
mod pull;
mod serve;
mod timefmt;

use clap::{Parser, Subcommand};
use std::net::SocketAddr;
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
    /// Make a worktree here holding a thread's latest turn, for example one another machine recorded.
    Pull {
        thread: String,
        /// Where to create it. Default: next to this repository, named after it and the thread.
        #[arg(long)]
        into: Option<PathBuf>,
    },
    /// Print each new turn of a thread as it arrives. With --into, apply it to that worktree too.
    Follow {
        thread: String,
        /// A worktree made by `baton pull`. Paths with local edits are kept as they are.
        #[arg(long)]
        into: Option<PathBuf>,
    },
    /// Run the sync server.
    Serve {
        /// Where projects and the device list live. Default: `server` under baton's data dir.
        #[arg(long)]
        data: Option<PathBuf>,
        #[arg(long, default_value = "127.0.0.1:7878")]
        listen: SocketAddr,
        /// Serve `wss://` with this certificate chain (PEM). Needs --tls-key.
        #[arg(long)]
        tls_cert: Option<PathBuf>,
        #[arg(long)]
        tls_key: Option<PathBuf>,
        /// Log every advance and fan-out with its timing.
        #[arg(long)]
        trace: bool,
    },
    /// Manage the sync server's devices and index. Works on the data dir, with the server up or down.
    Admin {
        #[arg(long, global = true)]
        data: Option<PathBuf>,
        #[command(subcommand)]
        command: AdminCommand,
    },
}

#[derive(Subcommand)]
enum AdminCommand {
    /// Add, list or revoke the machines allowed to connect.
    Device {
        #[command(subcommand)]
        command: DeviceCommand,
    },
    /// Rebuild the project and thread index from the refs in the repos.
    Reindex,
    /// List every thread the server holds.
    Threads,
}

#[derive(Subcommand)]
enum DeviceCommand {
    /// Create a device for a machine and print its token once.
    Add {
        name: String,
    },
    List,
    /// Revoke every active device of a machine.
    Revoke {
        name: String,
    },
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
        Command::Pull { thread, into } => pull::pull(&thread, into.as_deref()),
        Command::Follow { thread, into } => pull::follow(&thread, into.as_deref()),
        Command::Serve {
            data,
            listen,
            tls_cert,
            tls_key,
            trace,
        } => serve::serve(serve::ServeArgs {
            data,
            listen,
            tls_cert,
            tls_key,
            trace,
        }),
        Command::Admin { data, command } => match command {
            AdminCommand::Device { command } => match command {
                DeviceCommand::Add { name } => serve::device_add(data, &name),
                DeviceCommand::List => serve::device_list(data),
                DeviceCommand::Revoke { name } => serve::device_revoke(data, &name),
            },
            AdminCommand::Reindex => serve::reindex(data),
            AdminCommand::Threads => serve::threads(data),
        },
    };
    if let Err(e) = result {
        eprintln!("baton: {e:#}");
        std::process::exit(1);
    }
}
