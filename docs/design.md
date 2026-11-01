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
baton unenroll [path]             # stop watching; its history stays
baton status
baton log [thread]
baton rewind <turn> [--thread T] [--apply]
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
- **Redact.** Every other file, the commit subject, and the model trailer pass through the redactor. Built-in patterns plus the project's `.baton/config.toml` patterns, which are shared through git and so identical on every machine. A match becomes `[baton:redacted:<rule>]`. A thread or agent id that redaction would change is refused rather than rewritten, because ids are keys.
- **Literal values.** User config alone may name `[redact] value_files`: files whose contents are secrets to cut out wherever they appear. A `.json` file contributes every string value, any other file contributes every line, and values under 8 characters are dropped so a short word cannot blank out source code. A listed file that cannot be read stops the turn, since silently skipping it would store the secret. This list is per machine, so a file that contains one of these values gets a different blob id on a machine without it, and that machine's snapshot trees differ from a peer's for that file. Nothing breaks (a turn is still complete and valid on its own), but it is the one place two machines can disagree about the same bytes. Project patterns do not have this problem, which is why the project config is the place for anything every machine should agree on.
- **Skip.** Files over `max_file_bytes` (default 64 MiB) are left out and listed in a `Baton-Skipped:` trailer.

## Daemon

One daemon per user. Single owners, no shared locks:

| State | Owner |
|---|---|
| Dirty paths, stat cache, current snapshot tree of one worktree | that worktree's task |
| Writes to one side repo (objects, `refs/baton/*`) and its send state | that project's writer task |
| The server connection | the link task |

Turn path: the hook writes one frame to the local pipe and exits. The daemon's IPC task parses it through the agent adapter into a `TurnRequest`, routes it to the worktree task, which drains its dirty set and calls `snapshot_from_dirty` on the project writer. The writer writes blobs, edits the previous tree with gix's tree editor, writes the commit, moves `refs/baton/<thread>` by compare-and-swap, and hands the new objects to the link.

Worktree discovery of what changed is a file watcher feeding a dirty set, with a full rescan on start, on overflow or any watcher error, and when `.gitignore`, `.git/info/exclude`, or `.baton/config.toml` changes (see Decisions). Only enrolled worktrees are watched, so enrollment is the allowlist.

Local IPC: a named pipe `\\.\pipe\baton-<user>` on Windows, a unix socket `$XDG_RUNTIME_DIR/baton/daemon.sock` (else `<tmp>/baton-<uid>/daemon.sock`) with mode 0600 in a 0700 directory elsewhere. The hook computes the name without reading config. When `BATON_HOME` is set, data, config, and the endpoint all live under it, which is how the e2e run stays hermetic. The same endpoint carries two frame types: `BTN1` hook frames, which get no reply, and `BTC1` control frames (a JSON request, a `u32` LE length and JSON reply) used by `enroll`, `unenroll`, `turn`, and `status`. One endpoint keeps single-instance and permissions in one place.

## Hook contract

- Dispatched in the first lines of `main`, before config, logging, or gix load.
- Reads stdin up to 1 MiB on a helper thread, writes one frame `BTN1 | u32 LE length | agent name | \n | payload`, and exits 0.
- A missing pipe or socket, a full pipe, or any error means exit 0 with nothing written.
- A 50 ms watchdog calls `exit(0)`. The body runs under `catch_unwind`, and the release profile keeps `panic = "unwind"`.
- Never prints to stdout or stderr. Never spawns the daemon.
- The argument is the adapter name (`claude`). The turn records the agent as `claude-code`.

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

## Decisions

`tools/bench` measured the questions this section used to list. The numbers below are from `tools/bench` on Windows unless a row says `e2e/local.sh`, which drives the release binary on the same machine against a copy of the IRIS tree (7,384 files, 7,359 of them stored).

| Question | Decision | Evidence |
|---|---|---|
| Loose objects or a pack per turn | Loose objects, written serially by the project's one writer thread | 12 objects take about 1.2 ms serially. A pack per turn is no faster. Threaded writes stalled once near 150 ms. |
| Tree edit | Plumbing `gix::objs::tree::Editor` on the previous tree. Never `Repository::edit_tree` | About 5 ms for a 3-file turn on 9,700 files, against 53 ms, because `edit_tree` probes the object database per entry. In the daemon, sync plus commit plus ref update for the 3-file turns of the latency run take a median 3.9 ms. |
| Empty blob | Written explicitly, and the empty tree only when a commit needs it | `write_blob(b"")` does not store the empty blob. `e2e/local.sh` checks it with `git cat-file -e` and runs `fsck --strict --no-dangling` over the side repo. |
| Ignore rules | gix excludes over an empty in-memory index, for every decision | It matches `git check-ignore` exactly. The `ignore` crate disagrees on repo-local `core.excludesFile` and on nested repos. |
| Baseline | gix dirwalk (paths only) with the excludes stack, then chunked writes of 4,096 files or 64 MiB with no commit and no ref | The dirwalk costs 58 ms on 9,700 files. Enrolling the IRIS copy stores 7,359 files in 1.6 s (`e2e/local.sh`), almost all of it reading, hashing, and writing the objects. |
| Finding what changed | A file watcher feeds a per-worktree dirty set. A full rescan runs on start, on overflow or any watcher error, and when `.gitignore`, `.git/info/exclude`, or `.baton/config.toml` changes. No periodic stat sweep | The dirwalk is cheap enough to be the backstop. A create, remove, or rename re-reads that subtree, because the watcher cannot say what a directory move carried. |
| Quiescence | A turn waits for 5 ms without events, and at most 30 ms | The hook can fire before the last write reaches the watcher. 0 of 50 writes made immediately before the hook were missed (`e2e/local.sh`). |
| Racy files | A file whose mtime is within 2 s of the time it was read is read again at the next turn | Same rule git uses for its index: equal size and mtime do not prove equal content inside the timestamp's resolution. |
| Hook cost | A 50 ms watchdog, one frame to the endpoint, exit 0 | Spawn to frame received is about 5.5 ms in the bench. Timed from Git Bash, which adds its own fork and exec cost, a hook process takes a median 33 ms and 44 ms at p95 against a running daemon (`e2e/local.sh`), so the bench figure is the better measure of the hook itself. |
| First ref compare-and-swap stall of 2 to 3 s | Did not reproduce | Slowest first ref update per new side repo and thread was 1.4 ms, 9.5 ms, and 1.3 ms in three full runs, and no update of any kind took over a second. The stage times are logged per turn in `daemon.log` (`sync`, `commit`, `ref`, `save`, `total`), so a recurrence shows which stage it was. |

Latency from `e2e/local.sh`, 100 turns of three edited files on the 7,384-file IRIS copy, daemon already running, measured in bash from just before the hook starts until `refs/baton/<thread>` changes:

| Span | p50 | p95 |
|---|---|---|
| Hook call to ref moved | 58.4 ms | 68.9 ms |
| of which the hook process, spawn to exit | 33.2 ms | 47.9 ms |
| of which hook exit to ref moved | 21.9 ms | 31.2 ms |

Inside the daemon the median turn takes 3.5 ms to sync, 0.3 ms to commit, 0.2 ms to move the ref, and 3.9 ms in all. The rest of hook exit to ref moved is the 5 ms quiescence wait and the script's own polling of the ref file. A target of 50 ms from hook call to the ref visible on a second machine cannot be judged from this run: p50 on one machine is already 58 ms, and most of that is process spawn under Git Bash and the polling, which a native agent hook does not pay. This slice has no network, so the second-machine figure belongs to the sync slice.

### Behavior fixed by this slice

- The first line of a hook run decides the agent from its argument (`claude`). The turn records the adapter's own name (`claude-code`).
- `rewind` never deletes a file. It refuses a file whose stored copy has a redaction marker, a file whose parent is a symlink, any symlink on Windows, and a file whose content on disk is not what the last snapshot stored (take a turn first). `--apply` writes the files it can, lists the ones it refused, and exits 1 if any were refused. It compares contents, not timestamps, since two writes inside one timestamp tick are indistinguishable by mtime.
- `log` shows each turn's changed paths as a diff against the thread's previous turn, and for a thread's first turn only the number of files in its tree.
- `Baton-Skipped` values are escaped so a path cannot add a trailer line.
- A turn is a sync of the worktree followed by one commit on the thread's tip. The tree belongs to the worktree and the parent chain to the thread, so a turn's diff against its parent can include changes another thread in the same worktree made in between.
- The writer removes stale ref `.lock` files left in a side repo by a killed daemon when it starts. `e2e/local.sh` kills the daemon during a 60 MB write five times and checks that `fsck` passes, no lock is left, and the restarted daemon captures the file correctly.
- A non-hook, non-daemon command starts the daemon when it is absent, with its standard handles closed, so `$(baton status)` in a shell returns.

### Differences from the sections above

- Control commands share the hook's endpoint through a second frame type, `BTC1`, rather than listening on another.
- `pull`, `follow`, `ask`, `serve`, and `admin` are not in this slice, nor are the other agent adapters. Only `claude` is.
- The state of each worktree's stat cache and current tree is saved to `<data>/worktrees/<hash>.json` after each turn and after the baseline. It is a mirror of the side repo, so a lost or unreadable file costs one rescan, not data.
