# baton design

baton records every coding-agent turn as a git commit and syncs it live between a developer's machines through a server they host. This file is the contract the code is built against. Where code and this file disagree, one of them is wrong, and the change that fixes it updates both.

## Goals and limits

- An agent is never blocked by baton. The hook always exits 0, prints nothing, and never waits on the network or the daemon.
- baton never writes a user's index, `HEAD`, refs, object store, or working tree, except when the user runs `rewind --apply`, `pull`, or `follow`.
- Git is the record. Every server repo is a plain bare repo that `git fetch` reads completely.
- Secrets are redacted before an object is written anywhere.
- Every outside dependency (auth, classifier, decider, notifier, agent adapter) is a command named in config. A repo's own config can never name a command.
- Target latency: hook call to the ref being visible on a second machine in under 50 ms on a LAN, with 30 agents active.

## Usage

```text
baton hook claude                 # agent end-of-turn hook; raw payload on stdin
baton turn --thread T --message M [--agent A] [--model X] [--check name=code]...
baton daemon                      # per-user process; other commands start it when absent
baton enroll [path]               # start watching a worktree
baton log [thread]
baton rewind <turn> [--apply]
baton pull <thread> [--into dir]
baton follow <thread>
baton ask <question> --option a --option b
baton serve                       # the server
baton admin device add <name>     # prints a token once
```

## Crates

| Crate | Owns |
|---|---|
| `baton-core` | ids, turn records and trailers, store policy (ignore, redact, whole-file rules), snapshot, side-repo store, wire messages and codec, config types, plug-in call |
| `baton-server` | accept, auth, per-project bare repos, ref compare-and-swap, fan-out, SQLite |
| `baton-cli` | the `baton` binary: hook fast path, daemon, commands. No library target |

`baton-core` modules are `pub(crate)`. `lib.rs` re-exports the small public surface.

## Identity

`ThreadId`, `MachineId`, `ProjectKey`, `TurnId`, `TreeId`, `BlobId` are newtypes with private fields and a `parse` constructor. A value that exists is valid. Ref names are derived from them in one function, `turn_ref(thread) -> refs/baton/<thread>`, and the wire carries ids, never ref names, so a peer cannot name an arbitrary ref.

- `ThreadId` is the agent's session id, matching `^[A-Za-z0-9][A-Za-z0-9._-]{0,95}$`.
- `ProjectKey` comes from `.baton/config.toml` `project`, else the normalized `origin` URL. With neither, enrollment is refused.
- `MachineId` comes from user config, defaulting to the hostname. The server takes the authoritative value from auth.

## Where turns live

Each project gets a side bare repo owned only by the daemon, at `<data>/projects/<ProjectKey>.git`. Turn commits and their objects are written there, never into the user's repo. This keeps baton off the user's packed-refs, object store, and `gc`, at the cost of a second copy of file contents.

A thread's first turn commit has no parent and carries `Baton-Base: <user HEAD>`. Later turns of the thread chain to the previous turn. The server therefore never needs the project's real history, and every object it holds is complete.

Commit layout:

```text
tree <snapshot tree>
parent <previous turn>            (absent on the thread's first turn)
author baton <baton@localhost> <time>
committer baton <baton@localhost> <time>

<message subject, one line>

Baton-Thread: <thread>
Baton-Turn: <seq>
Baton-Agent: <agent>
Baton-Model: <model>
Baton-Machine: <machine>
Baton-Base: <user HEAD sha>
Baton-Check: <name>=<exit code>   (zero or more)
```

The trailer grammar lives in one module (`turn.rs`).

## Store policy

One `StorePolicy` decides, per path, whether to store it and how:

- **Ignore.** Nested `.gitignore`, `.git/info/exclude`, and global excludes, plus config `exclude` globs. `.git` is never stored.
- **Whole.** Lockfiles, files matching `store_whole` globs, and files with a NUL in the first 8 KiB are stored byte for byte, without redaction.
- **Redact.** Every other file, and the commit message and trailers, pass through the redactor. Built-in patterns plus the project's `.baton/config.toml` patterns. User config cannot add patterns, because two machines with different lists would produce different blob ids for the same file. A match becomes `[baton:redacted:<rule>]`.
- **Skip.** Files over `max_file_bytes` (default 64 MiB) are left out and listed in a `Baton-Skipped:` trailer.

## Daemon

One daemon per user. Single owners, no shared locks:

| State | Owner |
|---|---|
| Dirty paths, stat cache, current snapshot tree of one worktree | that worktree's task |
| Writes to one side repo (objects, `refs/baton/*`) and its send state | that project's writer task |
| The server connection | the link task |

Turn path: the hook writes one frame to the local pipe and exits. The daemon's IPC task parses it through the agent adapter into a `TurnRequest`, routes it to the worktree task, which drains its dirty set and calls `snapshot_from_dirty` on the project writer. The writer writes blobs, edits the previous tree with gix's tree editor, writes the commit, moves `refs/baton/<thread>` by compare-and-swap, and hands the new objects to the link.

Worktree discovery of what changed is settled by measurement in `tools/bench` (see Open questions): a file watcher with rescan on overflow, a stat sweep against a private stat cache, or both.

Local IPC: a named pipe `\\.\pipe\baton-<user>` on Windows, a unix socket `<runtime dir>/baton/daemon.sock` with mode 0600 elsewhere. The hook computes the path without reading config.

## Hook contract

- Dispatched in the first lines of `main`, before config, logging, or gix load.
- Reads stdin up to 1 MiB on a helper thread, writes one frame `BTN1 | u32 LE length | agent name | \n | payload`, and exits 0.
- A missing pipe or socket, a full pipe, or any error means exit 0 with nothing written.
- A 50 ms watchdog calls `exit(0)`. The body runs under `catch_unwind`, and the release profile keeps `panic = "unwind"`.
- Never prints to stdout or stderr. Never spawns the daemon.

## Server

`baton serve` holds, under its data dir:

```text
server.sqlite
projects/<ProjectKey>.git      plain bare repo, refs/baton/<thread>, gc.auto = 0
```

Invariants:

1. **Present means complete.** An object stored on the server has every object it references stored. Objects are written blobs first, then trees deepest first, then commits, and every received object is re-hashed before it is stored.
2. **Refs move last.** A ref moves only after the new commit's closure is complete, by compare-and-swap against the expected old value.
3. **One writer per thread.** `threads.owner` is set by the first authenticated push. A push from another machine, or a tip whose `Baton-Machine` trailer differs from the authenticated machine, is refused.
4. **Received trees are validated.** Entry names are non-empty, contain no `/` or NUL, are not `.`, `..`, or `.git` in any case, and appear once.

SQLite is authoritative only for devices. `projects` and `threads` are an index rebuilt from refs by `baton admin reindex`.

Fan-out: one broadcast channel per project. A session whose queue fills is closed and resyncs on reconnect, so a slow client never costs server memory or other clients' latency.

Auth providers: `token` (built in, per-device token, stored as SHA-256), or `command` (JSON in, `{ok, machine}` out). Tailscale and mTLS ship as example commands under `contrib/`.

## Wire

See `docs/protocol.md`. In short: one WebSocket per machine, subprotocol `baton.v1`. Control frames are JSON. Object frames are binary. A push sends the objects the turn created, then `Advance { thread, expected, new }`. The server answers `Advanced`, `Need { ids }` when the closure is incomplete, or `Refused { reason, current }`. The server tells other machines `Moved { thread, new, meta }`, sending small new objects ahead of it, and a follower asks `Want { ids }` for the rest.

The set of turns not yet on the server is derived from local refs and the server's last acknowledged heads, so there is no queue file. Turns made while offline go out on reconnect. Turns made while a push is in flight ride the next push.

## Plug-ins

Every plug-in is a `CommandSpec { command: Vec<String>, timeout }`, called with one JSON document on stdin and read as one JSON document from stdout. Timeout, non-zero exit, or unreadable output means fail open for classify, decide, and notify, and fail closed for auth.

| Slot | Input | Output | Default |
|---|---|---|---|
| `classify` | `baton.turn.v1` | `baton.verdict.v1` | built-in rules from config globs and diff patterns |
| `decide` | `baton.ask.v1` | `baton.answer.v1` | none, answer is "ask a human" |
| `notify` | `baton.verdict.v1` plus turn | ignored | none |
| `auth` | `baton.auth.v1` | `{ok, machine}` | built-in token |
| `agents.<name>` | raw hook payload | `baton.turnreq.v1` | built-in adapters for Claude Code, Codex, opencode, Pi |

Verdicts never rewrite a turn commit. They are stored on `refs/baton-verdict/<turn>`.

## Verification

`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, real runs of the binaries, measurement tools under `tools/`, and end-to-end scenarios under `e2e/` that drive the real binaries. No unit tests and no CI workflows.

## Open questions settled by `tools/bench`

1. Cost of a gix loose-object write on Windows, serial and parallel, which decides whether turns persist as loose objects or one pack per turn.
2. Cost of a full stat sweep of a large worktree with ignore rules, which decides watcher, sweep, or both.
3. Cost of a tree edit and commit for a three-file turn on a 9,700-file baseline.
4. Wall time of starting the `baton` binary and writing one pipe frame on Windows.
5. Whether gix stores the empty blob, and whether its excludes stack honors nested `.gitignore` the same way `git check-ignore` does.
