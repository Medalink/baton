mod adapter;
mod bisect;
mod blame;
mod commands;
mod control;
mod daemon;
mod endpoint;
mod hook;
mod pull;
mod report;
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
    /// Start recording a worktree, or with --repo every worktree of a repository, from its first turn.
    Enroll {
        path: Option<PathBuf>,
        /// Enroll the repository the path is in, so any of its worktrees, existing or created later, is
        /// recorded from its first turn.
        #[arg(long)]
        repo: bool,
    },
    /// Stop recording a worktree, or with --repo a repository. Its history stays.
    Unenroll {
        path: Option<PathBuf>,
        #[arg(long)]
        repo: bool,
    },
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
    Log {
        thread: Option<String>,
        /// One line per turn with its verdict class instead of its paths.
        #[arg(long)]
        verdicts: bool,
    },
    /// Show the verdict stored for a turn.
    Verdict {
        /// `thread@seq`, a number (with --thread), or at least 7 digits of a turn id.
        turn: String,
        #[arg(long)]
        thread: Option<String>,
        /// Print the stored `baton.verdict.v1` document.
        #[arg(long)]
        json: bool,
    },
    /// Ask the configured decider to choose. Exit 0 when it chose, 2 when a human must answer.
    Ask {
        question: String,
        #[arg(long = "option", required = true)]
        options: Vec<String>,
        #[arg(long)]
        context: Option<String>,
        /// The agent's session or thread id, passed to the decider. Defaults to $BATON_THREAD.
        #[arg(long)]
        thread: Option<String>,
    },
    /// Put a turn's files back on disk. Without --apply it only shows what would change.
    Rewind {
        /// `thread@seq`, a number (with --thread), or at least 7 digits of a turn id.
        turn: String,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        apply: bool,
    },
    /// Find the recorded turn that wrote a line git blames on a commit.
    Blame {
        /// `file:line`, or a file with -L.
        target: String,
        /// `first,last` (or one line) of the file.
        #[arg(short = 'L', value_name = "A,B")]
        lines: Option<String>,
        /// Print the `baton.blame.v1` document.
        #[arg(long)]
        json: bool,
    },
    /// Find the first turn of a thread at which a command fails. Exit codes as in `git bisect run`: 0 good, 125 skip, else bad.
    Bisect {
        thread: String,
        /// A turn number known to pass. Default: the thread's first turn.
        #[arg(long)]
        good: Option<u32>,
        /// A turn number known to fail. Default: the latest turn, which is run first.
        #[arg(long)]
        bad: Option<u32>,
        /// Longest one run may take, for example `90`, `500ms`, `5m` or `2h`; a bare number is seconds.
        #[arg(long, default_value = "300")]
        timeout: String,
        /// The command to run in the scratch worktree, after `--`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
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
    /// Flag thrashing, rework and abandoned threads in the recorded turns. Reads only.
    Report {
        /// How far back to look: `7d`, `48h`, `90m`, `2w`.
        #[arg(long, default_value = "7d")]
        since: String,
        /// A project key, to report outside a worktree.
        #[arg(long)]
        project: Option<String>,
        /// Print the `baton.report.v1` document.
        #[arg(long)]
        json: bool,
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

    // `ask` uses exit code 2 for "a human must answer", which is also clap's code for a usage error.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            std::process::exit(if e.use_stderr() { 1 } else { 0 });
        }
    };
    let result = match cli.command {
        Command::Hook { .. } => Ok(()),
        Command::Daemon => daemon::run(),
        Command::Enroll { path, repo: false } => commands::enroll(path.as_deref()),
        Command::Enroll { path, repo: true } => commands::enroll_repo(path.as_deref()),
        Command::Unenroll { path, repo: false } => commands::unenroll(path.as_deref()),
        Command::Unenroll { path, repo: true } => commands::unenroll_repo(path.as_deref()),
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
        Command::Log { thread, verdicts } => commands::log(thread.as_deref(), verdicts),
        Command::Verdict { turn, thread, json } => {
            commands::verdict(&turn, thread.as_deref(), json)
        }
        Command::Ask {
            question,
            options,
            context,
            thread,
        } => {
            commands::ask(&question, &options, context.as_deref(), thread.as_deref()).map(|code| {
                if code != 0 {
                    std::process::exit(code);
                }
            })
        }
        Command::Rewind {
            turn,
            thread,
            apply,
        } => commands::rewind(&turn, thread.as_deref(), apply),
        Command::Blame {
            target,
            lines,
            json,
        } => blame::run(&target, lines.as_deref(), json),
        Command::Bisect {
            thread,
            good,
            bad,
            timeout,
            command,
        } => bisect::run(bisect::Options {
            thread,
            good,
            bad,
            timeout,
            command,
        })
        .map(|code| {
            if code != 0 {
                std::process::exit(code);
            }
        }),
        Command::Status => commands::status(),
        Command::Pull { thread, into } => pull::pull(&thread, into.as_deref()),
        Command::Follow { thread, into } => pull::follow(&thread, into.as_deref()),
        Command::Report {
            since,
            project,
            json,
        } => report::report(report::ReportArgs {
            since,
            project,
            json,
        }),
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
