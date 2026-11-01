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

- **Ignore.** Nested `.gitignore`, `.git/info/exclude`, and global excludes, plus config `exclude` globs. `.git` is never stored. A file that git tracks (it is in the `HEAD` tree) is stored even when an ignore rule matches it, because git itself ignores the rule for a tracked file. The tracked set comes from the `HEAD` tree in the object database. baton never opens `.git/index`, so a file that is staged but not yet committed and matches an ignore rule is treated as ignored until it is committed. A config `exclude` glob still wins over tracking, since the user wrote it for baton.
- **Whole.** Lockfiles, files matching `store_whole` globs, and files with a NUL in the first 8 KiB are stored byte for byte, without pattern redaction. Only the user's `[snapshot] store_whole` is read. A `store_whole` in `.baton/config.toml` is ignored with a log line, since that file arrives with `git clone` and must not be able to put a file in unredacted. Literal values (below) are still cut out of a `store_whole` match.
- **Redact.** Every other file, the commit subject, and the model trailer pass through the redactor. Built-in patterns plus the project's `.baton/config.toml` patterns, which are shared through git and so identical on every machine. A match becomes `[baton:redacted:<rule>]`. A thread or agent id that redaction would change is refused rather than rewritten, because ids are keys. A project rule whose regex matches the empty string would blank every byte, so it is dropped at load with a log line and capture continues.
- **Built-in patterns.** Private-key blocks; AWS access keys and secret keys (a value of 40 or more characters after `aws_secret_access_key`, with no word boundary after it, since the value can end in `/`, `+` or `=`); GitHub, Anthropic, OpenAI, Slack, Stripe, Google and JWT tokens; a password in a URL (`://user:secret@` or `://:secret@` with no user, where the secret may hold `/`, `+`, `'` or `<` and ends at whitespace, `@` or `"`); and any name containing `password`, `passwd`, `secret`, `api_key`, `access_token`, `auth_token` or `private_key` assigned with `=` or `:`, quoted (8 or more characters up to the closing quote on the same line, spaces included) or unquoted (12 or more characters with no whitespace, quote, bracket, parenthesis, `$` or backslash). So `DB_PASSWORD=...`, `API_KEY='abc def ghi'` and `stripe_secret_key: ...` match. The unquoted form also matches ordinary code such as `password = request.password_confirmation`, the quoted form matches prose such as `password: "Enter your password"`, and a stored blob that holds a marker makes `rewind` refuse that file. That is the price of not missing `.env` files, and a project can name a file in `exclude` to keep it out.
- **Literal values.** User config alone may name `[redact] value_files`: files whose contents are secrets to cut out wherever they appear, in every text file including a `store_whole` match. A `.json` file contributes every string value, any other file contributes every line, and values under 8 characters are dropped so a short word cannot blank out source code. A listed file that cannot be read stops the turn, since silently skipping it would store the secret. This list is per machine, so a file that contains one of these values gets a different blob id on a machine without it, and that machine's snapshot trees differ from a peer's for that file. Nothing breaks (a turn is still complete and valid on its own), but it is the one place two machines can disagree about the same bytes. Project patterns do not have this problem, which is why the project config is the place for anything every machine should agree on.
- **Names.** A path is stored as an object name and in the `Baton-Skipped` trailer, so it is checked like content. A path that redaction would change (`ghp_...txt`) is not stored; it is listed in `Baton-Skipped` under its redacted spelling (`[baton:redacted:github-token].txt`) for as long as it exists. The trailer reader accepts such a label, since it is not a valid path. `Baton-Machine` falls back to the host name, with a log line, when the configured id holds a secret. A `--check` name that holds a secret is dropped with a log line. A thread or agent id that holds one is refused, as above.
- **Skip.** Files over `max_file_bytes` (default 64 MiB) are left out and listed in a `Baton-Skipped:` trailer.

## Daemon

One daemon per user. Single owners, no shared locks:

| State | Owner |
|---|---|
| Dirty paths, stat cache, current snapshot tree of one worktree | that worktree's task |
| Writes to one side repo (objects, `refs/baton/*`) and its send state | that project's writer task |
| The server connection | the link task |

Turn path: the hook writes one frame to the local pipe and exits. The daemon's IPC task parses it through the agent adapter into a `TurnRequest`, routes it to the worktree task, which drains its dirty set and calls `snapshot_from_dirty` on the project writer. The writer writes blobs, edits the previous tree with gix's tree editor, writes the commit, moves `refs/baton/<thread>` by compare-and-swap, and hands the new objects to the link.

Worktree discovery of what changed is a file watcher feeding a dirty set, with a full rescan on start, on overflow or any watcher error, and when the store policy changes (see Decisions). The dirty set holds up to 50,000 paths. Past that it stops tracking them and the next turn does a full rescan, which is bounded by the worktree and never overflows again, so a 50,100-file checkout is captured, not retried forever. Only enrolled worktrees are watched, so enrollment is the allowlist.

The walk and today's policy decide what is in the tree; the stat cache only saves reading a file again. A cached path that the policy now ignores or excludes leaves the tree at the next turn, and a nested `.gitignore` that appears in a later batch applies to files that came before it. Every event path is put to the policy again, with no memory of an earlier "ignored" answer for a directory, so a file that git starts tracking inside an ignored directory, or a directory a config edit stops excluding, is not dropped on the strength of a stale verdict. Events that arrive within 2 ms of each other (at most 25 ms) are classified as one batch, which sets up the ignore rules once: a 30,000-file write into an ignored directory costs the daemon 1.6 to 1.9 s of CPU against 1.5 to 1.65 s with the cache. The cache is trusted only while its policy fingerprint matches: a hash of the built-in rules, the merged settings, and the contents of `.baton/config.toml`, the user config, every value file, `.git/info/exclude`, `.git/config`, the root `.gitignore`, the global excludes file, and every git config file that could name it: the installation, system, XDG and `~/.gitconfig` files (whether or not they exist yet) and any file they include. A change reopens the policy, so a `core.excludesFile` added to the global gitconfig, or edited in the file it names, applies at the next turn without a daemon restart. The fingerprint is read at every turn, because the value files and the user's ignore files sit outside the worktree and no watcher sees them. A change marks every cached file racy and rescans, so a new redaction rule rewrites the stored tip the same turn it is added. Blobs in earlier turns keep their old bytes, as history does.

A path is stored under the spelling the file system lists, and what makes two paths the same file follows the repo's `core.ignorecase`, which git sets on a case-insensitive file system (Windows, default macOS); baton only reads it. With it set, the stat cache and the set of files HEAD tracks are matched ASCII-folded, each cache entry remembers the spelling its tree path uses, and a dirty path is spelled from the directory listings before it is looked at: two events for one file in two cases are one path. A case-only rename (`Readme` to `README`) removes the old tree path and adds the new one, reusing the stored blob when the file did not change, and it is found at daemon start as well, because the rescan compares spellings. This holds for a tracked file below an ignored directory too (HEAD's spelling is not the disk's), and for a deleted file, so `rewind` still refuses with "deleted since the last snapshot" when the deleted file was last stored under another case. The watcher recognises `.gitignore`, `.git/info/exclude` and `.baton/config.toml` in any case under the same setting, by the final name rather than a suffix. On Windows `notify` reports no event for a case-only rename of a directory, so the old spelling stays until the next full rescan (daemon start, overflow, policy change) or the next event below that directory, which spells the whole path from the disk; a file rename is seen at once. A saved mirror that holds two spellings of one file, or a config edit that flips `core.ignorecase`, starts from an empty tree. With it unset, paths are compared exactly and a path whose spelling is not the disk's is treated as absent. Only ASCII folds, as in git, and `exclude` and `store_whole` globs stay case-sensitive.

Routing: the payload's `cwd` is resolved to its real path before it is matched against enrolled worktrees, so a hook that reports a junction or a symlink reaches the worktree it points into. A `cwd` inside a git repo that is nested in an enrolled worktree, but not itself enrolled, is not the project's: the turn is dropped with a log line naming the nested repo, and the worktree is untouched. `baton turn` from there fails with the same reason and says to run `baton enroll`.

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
| Finding what changed | A file watcher feeds a per-worktree dirty set. A full rescan runs on start, on overflow or any watcher error, and when the policy fingerprint changes. A nested `.gitignore` is not in the fingerprint (that would read every one at every turn); its change event marks its directory structural, so the subtree is re-read. No periodic stat sweep | The dirwalk is cheap enough to be the backstop. A create, remove, or rename re-reads that subtree, because the watcher cannot say what a directory move carried. |
| Quiescence | A turn waits for 5 ms without events, and at most 30 ms | The hook can fire before the last write reaches the watcher. 0 of 50 writes made immediately before the hook were missed (`e2e/local.sh`). |
| Racy files | A file whose mtime is within 2 s of the time it was read is read again at the next turn | Same rule git uses for its index: equal size and mtime do not prove equal content inside the timestamp's resolution. |
| Hook cost | A 50 ms watchdog, one frame to the endpoint, exit 0 | Spawn to frame received is about 5.5 ms in the bench. Timed from Git Bash, a hook process against a running daemon takes a median 33 ms and 44 ms at p95 (`e2e/local.sh`). The gap to the bench is not isolated: Git Bash's fork and exec is the likely share, and a hook started by an agent directly was not timed. The 50 ms watchdog sits close to that p95, so a slow machine could drop frames, which the hook allows by design. |
| First ref compare-and-swap stall of 2 to 3 s | Did not reproduce | Slowest first ref update per new side repo and thread was 1.4 ms, 9.5 ms, and 1.3 ms in three full runs, and no update of any kind took over a second. In the review-fix round, one of six full runs counted one ref update over a second that was not a first update (that run's slowest first update was 1.2 ms). Its `daemon.log` was not kept, and the cause is unknown; `e2e/local.sh` now prints the offending log lines when it happens. The stage times are logged per turn in `daemon.log` (`sync`, `commit`, `ref`, `save`, `total`), so a recurrence shows which stage it was. |

Latency from `e2e/local.sh`, 100 turns of three edited files on the 7,384-file IRIS copy, daemon already running, measured in bash from just before the hook starts until `refs/baton/<thread>` changes:

| Span | p50 | p95 |
|---|---|---|
| Hook call to ref moved | 58.4 ms | 68.9 ms |
| of which the hook process, spawn to exit | 33.2 ms | 47.9 ms |
| of which hook exit to ref moved | 21.9 ms | 31.2 ms |

Inside the daemon the median turn takes 3.5 ms to sync, 0.3 ms to commit, 0.2 ms to move the ref, and 3.9 ms in all. A target of 50 ms from hook call to the ref visible on a second machine cannot be judged from this run: p50 on one machine is already 58 ms, and about 33 ms of it is the hook process as Git Bash starts it, and the rest is the 5 ms quiescence wait, the daemon's 4 ms, and the script's polling of the ref file. This slice has no network, so the second-machine figure belongs to the sync slice.

### Behavior fixed by this slice

- `rewind` never deletes a file. It refuses, in the plan, a file whose stored copy has a redaction marker, a file whose parent is a symlink, any symlink on Windows, a file whose content on disk is not what the last snapshot stored (take a turn first), a file deleted since the last snapshot, and a create over a directory. The refusals are decided before any write, so `--apply` does what the plan showed. A failed write removes its temp file. `--apply` writes the files it can, lists the ones it refused, and exits 1 if any were refused. It compares contents, not timestamps, since two writes inside one timestamp tick are indistinguishable by mtime.
- `log` shows each turn's changed paths as a diff against the thread's previous turn, and for a thread's first turn only the number of files in its tree.
- `Baton-Skipped` values are escaped so a path cannot add a trailer line.
- A turn is a sync of the worktree followed by one commit on the thread's tip. The tree belongs to the worktree and the parent chain to the thread, so a turn's diff against its parent can include changes another thread in the same worktree made in between.
- The writer removes stale ref `.lock` files left in a side repo by a killed daemon when it starts. `e2e/local.sh` kills the daemon during a 60 MB write five times and checks that `fsck` passes, no lock is left, and the restarted daemon captures the file correctly.
- A non-hook, non-daemon command starts the daemon when it is absent, with its standard handles closed, so `$(baton status)` in a shell returns.

### Differences from the sections above

- Control commands share the hook's endpoint through a second frame type, `BTC1`, rather than listening on another.
- `pull`, `follow`, `ask`, `serve`, and `admin` are not in this slice, nor are the other agent adapters. Only `claude` is.
- The state of each worktree's stat cache and current tree is saved to `<data>/worktrees/<hash>.json` after each turn and after the baseline. It is a mirror of the side repo, so a lost or unreadable file costs one rescan, not data.
