# baton design

baton records every coding-agent turn as a git commit and syncs it live between a developer's machines through a server they host. This file is the contract the code is built against. Where code and this file disagree, one of them is wrong, and the change that fixes it updates both.

## Goals and limits

- An agent is never blocked by baton. The hook always exits 0, prints nothing, and never waits on the network or the daemon.
- baton never writes a user's index, `HEAD`, refs, object store, or working tree, except when the user runs `rewind --apply`, `pull`, `follow --into` or `bisect`. `pull` registers one worktree with `git worktree add --detach` and writes only inside it. `follow --into` writes only inside a worktree `pull` made. `bisect` registers a scratch worktree the same way, in the system temp dir, writes only inside it, and removes it with `git worktree remove` on every way out.
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
baton enroll --repo [path]        # enroll the repository; any of its worktrees is recorded from its first turn
baton unenroll [path]             # stop watching; its history stays
baton unenroll --repo [path]
baton status                      # worktrees, enrolled repositories and their watched worktrees
baton log [thread] [--verdicts]
baton verdict <turn> [--thread T] [--json]   # the verdict stored for a turn
baton rewind <turn> [--thread T] [--apply]
baton report [--since 7d] [--project key] [--json]   # thrashing, rework, abandoned threads, hotspots
baton pull <thread> [--into dir]  # a new worktree at the thread's latest turn
baton follow <thread> [--into dir]  # print each new turn; with --into, apply it there too
baton blame <file>:<line> [--json]   # the recorded turn that wrote a line (also: <file> -L a,b)
baton bisect <thread> [--good N] [--bad N] [--timeout 300] -- <command>...   # the first turn where the command fails
baton ask <question> --option a --option b [--context text] [--thread id]   # exit 0 chose, 2 ask a human
baton serve [--data dir] [--listen addr] [--tls-cert pem --tls-key pem] [--trace]
baton admin [--data dir] device add <name>     # prints a token once
baton admin [--data dir] device list|revoke ...
baton admin [--data dir] reindex|threads
```

`baton admin` works on the server's data dir, with the server up or down.

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
- `ProjectKey` comes from `.baton/config.toml` `project`, else the normalized URL of `origin`, else of the repo's only remote. Only remotes named in the repo's own config (`.git/config`, `config.worktree`) count, so the user's global git config cannot change a key. With no such remote, or several and none named `origin`, enrollment is refused.
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
- **Secret names.** Redaction finds a secret by its shape, so a bare value in a file such as `keys/console-password` passes through it. A file that git does not track (not in the `HEAD` tree, so a staged but uncommitted one still counts as untracked) is therefore left out, and listed in `Baton-Skipped:`, when its path matches one of these built-in names. A tracked file is always stored, so a committed `.env` or `password_reset.rs` is unaffected.
  - Whatever the extension: `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.kdbx`, `*.keystore`, `*.jks`, `*.token`, `.env`, `.env.*`, `*.env`, `.netrc`, `_netrc`, `.pgpass`, `credentials`, `credentials.json`, and anything under a directory named `.ssh`.
  - Unless the file is source or a document: `*password*`, `*passwd*`, `*secret*`, `id_rsa*`, `id_dsa*`, `id_ecdsa*`, `id_ed25519*`, and anything under a directory named `keys` or `secrets`. The exemption is by extension (`rs`, `py`, `js`, `ts`, `tsx`, `go`, `java`, `php`, `rb`, `cs`, `c`, `cpp`, `sh`, `sql`, `html`, `css`, `md` and similar), because an agent that adds `password_reset.rs` or `src/keys/mod.rs` is doing ordinary work that a rewind must be able to undo, and redaction reads source files. A name with no extension, or a data extension such as `.txt`, `.json`, `.yaml` or `.csv`, is not exempt.
  - Never matched: `*.pub`, `.env.example`, `.env.sample`, `.env.dist`, `.env.template`.
  - The built-in names ignore ASCII case on every file system, so `.ENV`, `ID_RSA` and `Passwords.txt` match in a case-sensitive repo too: a file left out by mistake costs less than one stored by mistake.
  - `[snapshot] store_secret_named` (a list of globs, in the user config and in `.baton/config.toml`, matched like `exclude`: a pattern without `/` matches a name at any depth, and a match covers everything below a directory) stores matching untracked files anyway. Unlike `store_whole`, the repo config may use it, since a file it brings back still goes through every redaction rule; what it cannot catch is a bare value, which is the case the list exists to opt into. Its globs follow `core.ignorecase`, as `exclude` does.
  - The daemon logs one line per newly skipped path, naming the rule. The trailer carries the path only, as for a size skip; the reason is in the log. The list is part of the policy fingerprint, so adding or changing it re-reads every file at the next turn, and a file stored by an earlier version leaves the tree.

## Daemon

One daemon per user. Single owners, no shared locks:

| State | Owner |
|---|---|
| Dirty paths, stat cache, current snapshot tree of one worktree | that worktree's task |
| Writes to one side repo (objects, `refs/baton/*`) and its send state | that project's writer task |
| The server connection | the link task |

Turn path: the hook writes one frame to the local pipe and exits. The daemon's IPC task parses it through the agent adapter into a `TurnRequest`, routes it to the worktree task, which drains its dirty set and calls `snapshot_from_dirty` on the project writer. The writer writes blobs, edits the previous tree with gix's tree editor, writes the commit, moves `refs/baton/<thread>` by compare-and-swap, and hands the new objects to the link.

Worktree discovery of what changed is a file watcher feeding a dirty set, with a full rescan on start, on overflow or any watcher error, and when the store policy changes (see Decisions). The dirty set holds up to 50,000 paths. Past that it stops tracking them and the next turn does a full rescan, which is bounded by the worktree and never overflows again, so a 50,100-file checkout is captured, not retried forever. Only enrolled worktrees are watched, so enrollment is the allowlist; a repository is the allowlist for every worktree it has (see Enrolling a repository).

The walk and today's policy decide what is in the tree; the stat cache only saves reading a file again. A cached path that the policy now ignores or excludes leaves the tree at the next turn, and a nested `.gitignore` that appears in a later batch applies to files that came before it. Every event path is put to the policy again, with no memory of an earlier "ignored" answer for a directory, so a file that git starts tracking inside an ignored directory, or a directory a config edit stops excluding, is not dropped on the strength of a stale verdict. Events that arrive within 2 ms of each other (at most 25 ms) are classified as one batch, which sets up the ignore rules once: a 30,000-file write into an ignored directory costs the daemon 1.6 to 1.9 s of CPU against 1.5 to 1.65 s with the cache. The cache is trusted only while its policy fingerprint matches: a hash of the built-in rules, the merged settings, and the contents of `.baton/config.toml`, the user config, every value file, `.git/info/exclude`, `.git/config`, the root `.gitignore`, the global excludes file, and every git config file that could name it: the installation, system, XDG and `~/.gitconfig` files (whether or not they exist yet) and any file they include. A change reopens the policy, so a `core.excludesFile` added to the global gitconfig, or edited in the file it names, applies at the next turn without a daemon restart. The fingerprint is read at every turn, because the value files and the user's ignore files sit outside the worktree and no watcher sees them. A change marks every cached file racy and rescans, so a new redaction rule rewrites the stored tip the same turn it is added. Blobs in earlier turns keep their old bytes, as history does.

A path is stored under the spelling the file system lists, and what makes two paths the same file follows the repo's `core.ignorecase`, which git sets on a case-insensitive file system (Windows, default macOS); baton only reads it. With it set, the stat cache and the set of files HEAD tracks are matched ASCII-folded, each cache entry remembers the spelling its tree path uses, and a dirty path is spelled from the directory listings before it is looked at: two events for one file in two cases are one path. A case-only rename (`Readme` to `README`) removes the old tree path and adds the new one, reusing the stored blob when the file did not change, and it is found at daemon start as well, because the rescan compares spellings. This holds for a tracked file below an ignored directory too (HEAD's spelling is not the disk's), and for a deleted file, so `rewind` still refuses with "deleted since the last snapshot" when the deleted file was last stored under another case. The watcher recognises `.gitignore`, `.git/info/exclude` and `.baton/config.toml` in any case under the same setting, by the final name rather than a suffix. On Windows `notify` reports no event for a case-only rename of a directory, so the old spelling stays until the next full rescan (daemon start, overflow, policy change) or the next event below that directory, which spells the whole path from the disk; a file rename is seen at once. A saved mirror that holds two spellings of one file, or a config edit that flips `core.ignorecase`, starts from an empty tree. With it unset, paths are compared exactly and a path whose spelling is not the disk's is treated as absent. Only ASCII folds, as in git, and `exclude` and `store_whole` globs stay case-sensitive; `store_secret_named` follows the setting, and the built-in secret names ignore case everywhere (see Store policy).

Routing: the payload's `cwd` is resolved to its real path before it is matched against enrolled worktrees, so a hook that reports a junction or a symlink reaches the worktree it points into. A `cwd` inside a git repo that is nested in an enrolled worktree, but not itself enrolled, is not the project's: the turn is dropped with a log line naming the nested repo, and the worktree is untouched. `baton turn` from there fails with the same reason and says to run `baton enroll`.

Local IPC: a named pipe `\\.\pipe\baton-<user>` on Windows, a unix socket `daemon.sock` (mode 0600) in the canonical data dir elsewhere, the directory that holds the single-instance lock, which baton makes 0700. The socket path is a pure function of that resolved data dir (symlinks resolved, no trailing slash), so every process that agrees on the lock agrees on the socket whatever its `XDG_RUNTIME_DIR`, `TMPDIR` or spelling of `BATON_HOME`. A data dir so deep that the path would pass the 100-byte `sun_path` budget uses `/tmp/baton-<uid>/<first 32 hex digits of the SHA-256 of the canonical data dir>.sock` instead. `/tmp` is fixed, never read from the environment. The daemon creates `/tmp/baton-<uid>` with mode 0700 and refuses to start unless an `lstat` shows a real directory owned by the user with no group or other access; the error names the path and the problem, and a command that finds the daemon stopped reports that error from `daemon.log` instead of waiting out the 20 s start deadline. The hook computes the name without reading config. When `BATON_HOME` is set, data, config, and the endpoint all live under it, which is how the e2e run stays hermetic. On Windows the pipe name is unchanged. The same endpoint carries two frame types: `BTN1` hook frames, which get no reply, and `BTC1` control frames (a JSON request, a `u32` LE length and JSON reply) used by `enroll`, `enroll --repo`, `unenroll`, `unenroll --repo`, `turn`, and `status`. One endpoint keeps single-instance and permissions in one place.

### Enrolling a repository

Agent tools create a fresh linked worktree per task (`git worktree add`), so enrolling worktrees one by one captures almost nothing. `baton enroll --repo [path]` enrolls the repository the path is in, by its **common git dir** (`gix`'s `common_dir()`, the `.git` of the main worktree or the bare repo that all linked worktrees share), made real and with the Windows verbatim prefix removed. The dir is stored in `enrolled.json` under `repos`, beside the `roots` of worktrees enrolled by themselves. Enrollment is refused when the repository names no project (`ProjectKey`), as for a worktree. It reads the repository and nothing else: no baseline runs and no worktree is touched.

A hook (or `baton turn`) whose cwd no enrolled worktree owns is put to the repository check before it is ignored: `gix::discover(cwd)` gives the worktree root and the common git dir, and a common dir in the enrolled set enrolls that worktree on the spot. The baseline runs first, then the turn, exactly as for `baton enroll` followed by a hook. The check runs for a nested-repo miss too, so a linked worktree that sits inside an enrolled checkout (such as `.claude/worktrees/<id>`) is its own worktree and not a nested repo. A cwd in a repository that is not enrolled is ignored as before, whether it is the main worktree or a linked one, and the check only reads.

Two hooks for one new worktree both land: the second waits for the first's enrollment (a condition variable on the registry, not an error) and then sends its turn. A turn is queued under the registry lock, and so is the `Stop` of an eviction, so a turn is never queued behind a stop and lost.

Worktrees that came in through a repository are not written to `enrolled.json`. After a daemon restart only the repository list returns, and each worktree comes back with its next turn from its saved mirror, which already rescans at start. `baton enroll <worktree>` on one of them makes it permanent (it moves to `roots`), and `baton unenroll --repo` stops the ones the repository brought in and leaves those. `baton unenroll <worktree>` on one brought in by a repository stops it until its next turn, since the repository is still enrolled; `unenroll --repo` is what ends that.

**Bounded watch set.** At most `[watch] max_repo_worktrees` (default 64, read when the daemon starts) worktrees brought in by repositories are watched at once, since each costs a watcher handle (a kernel watch per directory on Linux). The default is twice the 30 active agents of the latency target, so the cap does not thrash in the case the design is sized for. When a new one would pass the cap, the least recently used (by the last turn sent to it) is unwatched: its task stops and its mirror file stays. Its next turn enrolls it again, from the mirror, with a full rescan, so what changed while it was unwatched is in that turn. A worktree enrolled by itself is never counted or evicted.

**Worktrees that disappear.** Agent tools delete their worktrees. A repository worktree is dropped when its root has no `.git` (`git worktree remove` takes it out with the rest). Its task ends by itself when its watcher reports an error or an event on the root or its `.git` and the check says it is gone, which closes the watcher handle promptly. `e2e/local.sh` removes a watched worktree with `git worktree remove --force` and checks that git succeeds and the directory is gone at once. A reaper thread runs the same check every 2 s as a backstop, removes the worktree from the registry, deletes its mirror file and logs `<root> is gone; no longer watched`. Nothing is an error: a turn that arrives later from the same path is an ordinary first turn. A worktree enrolled by itself is kept when its directory vanishes, since an unmounted drive looks the same, and that is the person's call.

`baton status` prints its first line as before, then one line per watched worktree (with `via <repo>` for one a repository brought in), then one `repository <dir>  <n> of at most <cap> worktrees watched` line per enrolled repository.

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
server.toml                    optional, the auth provider
server.lock                    one server per data dir
empty.git                      an empty bare repo, read only, that an advance for a new project is checked against
projects/<ProjectKey>.git      plain bare repo, refs/baton/<thread>, gc.auto = 0
```

A project's directory name is its key with `/` replaced by `+`, plus `.git` (`github.com+acme+app.git`). Each project has one writer thread. It takes the advance requests of every session in arrival order, so a compare-and-swap never races another one in the same process.

A project key costs a map entry while a session holds it and nothing after. The repo and the writer thread are created when the first `advance` for the project is accepted: until then the advance is judged against `empty.git`, so a `need`, a refusal, a `subscribe` or a `want` on a project the server has never stored leaves no directory and no thread. A writer with no work for 30 s ends, its project is forgotten once no session holds it, and the next request starts a writer again from the repo. A session may name 256 distinct projects in all (hello, subscribe, advance and want together), so one session cannot make the server remember more than that.

Invariants:

1. **Present means complete.** An object stored on the server has every object it references stored. Objects are written blobs first, then trees deepest first, then commits, and every received object is re-hashed before it is stored.
2. **Refs move last.** A ref moves only after the new commit's closure is complete, by compare-and-swap against the expected old value.
3. **One writer per thread.** The owner of a thread is the `Baton-Machine` trailer of its current tip, so the repo alone says who owns what and `threads.owner` is only an index of it. An advance is checked in this order. The authenticated machine must equal the current tip's owner (`not_owner`). The ref must be at `expected` (`stale`). Then every new turn must name the authenticated machine (`machine_mismatch`) and continue the thread by sequence number (`invalid`). The ref moves last.
4. **Received trees are validated as git validates them.** `names.rs` ports git v2.54.0's rules (`fsck_tree`, `verify_path_internal`, `is_ntfs_dotgit`, `is_hfs_dotgit`, `fsck_gitmodules_fn`, the submodule name and url checks, the null id and dotfile symlink checks), so a tree baton accepts passes `git fsck --strict` and cannot name a `.git` directory on NTFS, HFS+ or ext4. `docs/protocol.md` lists the rules. The same functions guard `RelPath::parse`, which every client write path goes through (apply, rewind, capture, diff and flatten), so a path is either safe to write or never written. Capture also asks `check_file` before it stores a file, so a turn never carries a name or a `.gitmodules` the server would refuse.
5. **Object kinds match, even for stored objects.** A tree entry or a commit field that names an id already stored as another kind is refused, because a stored id proves only that some object has that hash.
6. **Walks are iterative and capped.** Trees nest at most 512 levels. Every walk of a received tree (intake, closure, flatten, diff, apply) keeps its stack on the heap, because a stack overflow aborts the process and `catch_unwind` cannot stop it. A panic in a project writer is caught: it is logged, the advance is refused with `invalid`, and that project's writer restarts.

SQLite is authoritative only for devices. `projects` and `threads` are an index rebuilt from refs by `baton admin reindex`.

Fan-out: one broadcast channel per project. A session whose queue fills is closed and resyncs on reconnect, so a slow client never costs server memory or other clients' latency.

Limits per connection, each driven by a scenario in `e2e/sync.sh`:

| Limit | Value | When it is hit |
|---|---|---|
| Handshake (TLS and upgrade, auth included) | 10 s, at most 128 at once | the connection is closed without a reply |
| `hello` after the upgrade | 5 s | `error`, close |
| Frame | `max_frame` exactly, enforced by the WebSocket reader | `error`, close |
| Session pool, finished bytes plus announced `total` of unfinished objects | 512 MiB | `too_large`, close |
| Projects one session names | 256 | `too_large`, close |
| Ids in one `want` | 2,048, duplicates read once | `too_large`, close |
| A send that makes no progress | 30 s, then 5 s for the goodbye | the session is dropped |
| Silence from the client | 60 s | the session is closed |
| A revoked device | 2 s, and at the next request or forwarded event | `unauthorized`, close |

A revoked device is cut off by a timer in its own session, not by the operator's command, so `baton admin device revoke` never needs to reach the server process. The delay is the 2 s timer plus the time a database read takes. A `command` auth provider is asked at the upgrade only; revoking in its own system takes effect when the device next connects.

Auth providers: `token` (built in, per-device token, stored as SHA-256), or `command` (`baton.auth.v1` in, `{ok, machine}` out). The provider is named in `<data>/server.toml`:

```toml
[auth]
provider = "command"
command = ["/usr/local/bin/baton-auth"]
timeout_ms = 2000
```

Without `server.toml` the built-in token provider runs. A `server.toml` the server cannot read stops it at start. A command that fails or times out, a token that matches no active device, and a missing `Authorization` header all answer the WebSocket upgrade with `401`, so no `welcome` is sent. The machine id that auth returns is the machine for the whole session: it is the `machine` in `welcome`, the only value the server compares against the tip owner, and the client's `me` from then on. The id in the client's config only picks what the `hello` claims. `contrib/examples/tailscale-selfhost.toml` shows a server on a tailnet with a Tailscale certificate and device tokens; no mTLS example is shipped.

Operators: `baton admin device add <name>` creates a device and prints its token once, `device list` shows every device with its state, and `device revoke <name>` revokes all of a machine's active devices. A revoked device cannot connect again. Its open session gets an `error` with code `unauthorized` and is closed within 2 s. `admin threads` lists what the server holds and `admin reindex` rebuilds `projects` and `threads` from the refs.

## Wire

See `docs/protocol.md`. In short: one WebSocket per machine, subprotocol `baton.v1`. Control frames are JSON. Object frames are binary. A push sends the objects the turn created, then `Advance { thread, expected, new }`. The server answers `Advanced`, `Need { ids }` when the closure is incomplete, or `Refused { reason, current }`. The server tells other machines `Moved { thread, new, meta }`, sending small new objects ahead of it, and a follower asks `Want { ids }` for the rest.

The set of turns not yet on the server is derived from local refs and the server's last acknowledged heads, so there is no queue file. Turns made while offline go out on reconnect. Turns made while a push is in flight ride the next push.

### Daemon link

The link task holds the one WebSocket. It reads `[machine] id` and `[server] url`, `token` or `token_file`, and optional `ca_file` from the user config once, when the daemon starts, so a change to them needs a daemon restart. With no `[server]` the daemon runs local-only and says so in its log. It reconnects with backoff, sends a heartbeat ping every 15 s and treats 45 s of silence as a dead link.

After `welcome` the link compares each enrolled project's local refs with the server's `heads`, per thread:

- The server has no head and the local tip's `Baton-Machine` is this machine, or the server head's owner is this machine and differs from the local tip: push. The turns the server lacks go oldest first, in batches of at most 4 MiB, as objects and then `advance`. A `need` is answered with the objects it lists and the same `advance`, at most 3 times per advance. A `stale` takes the server's `current` as the new base and is retried at most twice. A `stale` whose `current` is the pushed tip is a success whose answer was lost. Any other refusal is logged with its `detail` and that tip is not sent again until a newer turn exists.
- The server head's owner is another machine and the local tip differs: follow. The objects of the new tip arrive ahead of `moved` when small, and the rest are asked for with `want`, one round per missing layer. The turns are stored and the local ref moves only to a tip that continues the local one. Anything else is logged as `sync: not following <thread>: <why>` and left alone.
- A new local turn on a thread whose server head belongs to another machine is not pushed. The daemon logs `sync: <thread> belongs to <owner>; its turns recorded here stay on this machine`, so an honest client is never refused with `not_owner`. The server hands a thread over only when an operator points its ref at another machine's turn.

Pushes and follows share the one connection and the daemon's own writer tasks, so the path from hook to a second machine is local IPC, commit, `advance`, server fan-out, and the follower's store, with no polling between the machines.

## Plug-ins

Every plug-in is a `CommandSpec { command: Vec<String>, timeout }`, called with one JSON document on stdin and read as one JSON document from stdout. Timeout, non-zero exit, or unreadable output means fail open for classify, decide, and notify, and fail closed for auth. `plugin.rs` in `baton-core` is the only code that runs one. The timeout covers the whole call, from spawn until the output is read, not only until the command exits. The command runs in its own process group (unix) or Job Object (Windows), and the whole tree is killed when the call ends, so a grandchild that keeps stdout open cannot hold the caller past the deadline or outlive the call. No plug-in runs on the hook's path: the hook writes a frame, and a turn is classified on a worker thread of the daemon after the turn is committed.

| Slot | Input | Output | Default |
|---|---|---|---|
| `classify` | `baton.turn.v1` | `baton.verdict.v1` | built-in rules (below) |
| `decide` | `baton.ask.v1` | `baton.answer.v1` | none, answer is "ask a human" |
| `notify` | `{"verdict": ..., "turn": ...}` | ignored | none |
| `lens` | a file path, or `baton.lens.v1` | text, or `{"lens": text}` | none |
| `auth` | `baton.auth.v1` | `{ok, machine}` | built-in token |
| `agents.<name>` | raw hook payload | `baton.turnreq.v1` or `null` | built-in adapters for Claude Code, Codex, opencode, Pi |

Verdicts never rewrite a turn commit. They are stored on `refs/baton-verdict/<turn commit>`, outside `refs/baton/`, so a verdict is never a thread tip and the sync link never sees it. Verdicts stay on the machine that made them.

### Config

Plug-ins are named in the user config (`config.toml` in baton's config directory) and nowhere else. A repo's `.baton/config.toml` that sets `[classify]`, `[decide]`, `[[notify]]`, `[lens]` or `[agents]` is ignored with a notice in the daemon log, because a cloned repo must not be able to run a command on the machine. The repo config type has no field that could carry one.

```toml
[classify]
mode = "shadow"            # off | shadow (default) | live
command = ["python3", "/path/to/classifier"]   # omit to use the built-in rules
timeout = "10s"            # default 10s
defaults = true            # built-in rules only: false drops the default rule set

[[classify.rule]]          # built-in rules only; added to the defaults
class = "react"
kind = "billing_changed"
paths = ["billing/**"]
diff = "(?i)refund"        # a regex over the added lines

[decide]
command = ["python3", "/path/to/decider"]
timeout = "30s"

[[notify]]
on = ["react", "check"]
command = ["python3", "/path/to/notifier"]
timeout = "5s"

[lens]
file = "docs/review-lens.md"   # inside the repo; or command = [...] with timeout = "5s"

[agents.my-agent]
command = ["/path/to/adapter"]
timeout = "2s"
```

`shadow` stores verdicts and runs no notifier, so a classifier can be watched before it can interrupt anyone. `live` also runs every `[[notify]]` whose `on` includes the verdict's class. `off` skips classification. Plug-in config is read by its own loader for each turn, so a typo in it logs a line and costs the turn its verdict, never its capture. A rule needs `paths`, `diff`, or both; one with neither would match every turn and is refused. The lens file is read through its real path and must stay inside the repo, since a clone can carry a symlink.

Any failure of a classifier, notifier or lens logs a line and leaves the turn recorded (`no verdict for <turn>: <why>`). A secret the redaction policy knows is redacted from a command's verdict before it is stored.

### Default rules

With no `[classify] command`, the built-in classifier runs these generic rules. The class of the verdict is the most urgent among the findings (`react`, `check`, `log`, `quiet`); a turn that matches nothing is `quiet`.

| Class | Kind | Matches |
|---|---|---|
| `react` | `dropped_table_or_column` | a file under `migrations/` or `migrate/`, or any `.sql` file, whose added lines contain `DROP TABLE`, `DROP COLUMN`, `DROP SCHEMA`, `DROP DATABASE`, `TRUNCATE TABLE`, `drop_table`, `drop_column` or `remove_column` |
| `check` | `lockfile_changed` | `Cargo.lock`, `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, `pnpm-lock.yaml`, `bun.lockb`, `composer.lock`, `Gemfile.lock`, `poetry.lock`, `uv.lock`, `go.sum` |
| `check` | `env_template_changed` | `.env.example`, `.env.sample`, `.env.dist` |
| `check` | `auth_code_changed` | any file under an `auth`, `authn`, `authz`, `authentication`, `authorization`, `permissions` or `policies` directory |

These read added lines and file names only. They do not parse SQL, so a drop written some other way, or in a file outside these paths, is missed. `[[classify.rule]]` and a command classifier exist for that. The defaults are a floor for a project with nothing configured, not a review.

### Agent adapters

`claude`, `codex`, `opencode` and `pi` are built in, and `docs/protocol.md` lists the payload each reads. Codex runs a command hook with the payload on stdin, so it needs only `baton hook codex` in its hook config. opencode and Pi have no command hooks, only in-process plug-ins, so each needs a few lines of glue that pipes its event to `baton hook <name>`. The glue is not shipped, because it could not be run against the real tools here. The field names come from the vendors' current documentation (linked in the pull request that added the adapters). The opencode `session.idle` shape is checked against the installed `@opencode-ai/sdk` types and the Pi context fields (`ctx.cwd`, `ctx.sessionManager.getSessionId()`) against its extension docs. `ctx.model.id` in Pi is not confirmed, so `model` is optional there. A `[agents.<name>]` command replaces the built-in adapter of that name. It runs on a thread of its own under its timeout, so a slow adapter never holds up the daemon.

### Windows

The daemon is a detached process with no console. A plug-in child started with the default flags, or with `CREATE_NO_WINDOW`, was given a console of its own, and about one launch in five died with a Ctrl+C status (`0xc000013a`). Children are started with `DETACHED_PROCESS`, which gave no failure in 80 launches of a repro. A plug-in that itself starts a console program may see a window flash.

## History

`baton blame`, `baton bisect` and `baton report` answer "which turn wrote this", "which turn broke this" and "where is work going wrong" from the side repo. All three read it directly and work with the daemon stopped.

### Blame

`baton blame <file>:<line>` (or `<file> -L a,b`), run inside an enrolled worktree, first asks `git blame --porcelain` for each line: the commit, the file name that commit held the line under, and the line's text. It then searches the project's side repo for the turn that first added that text to that file:

1. Every thread is walked oldest turn first. A turn *adds* a line when its copy of the file holds that line more often than the previous turn of the thread does (a multiset difference, so a moved line is not added, and a second identical line is). A thread's first turn is compared with the file at its `Baton-Base` commit, read from the user's repository; a file the base lacks counts as empty, and a thread that began in a repository with no commit starts empty.
2. A thread contributes at most one match per line: its first adding turn.
3. Matches rank in three steps. A match loses one point for a turn recorded after the git commit's committer time (second resolution, so the same second is not later), and one for a `Baton-Base` that is not an ancestor of the commit (`git merge-base --is-ancestor`; a turn with no base is not penalised). The lowest score wins, then the earliest time, then the thread id, then the turn number. The best match is printed and the others are counted ("N other turns also added this line").
4. With no match the line is reported as not written in a recorded turn: written by hand, written before enrollment, or out of baton's sight (below).

A line that is not committed yet has no commit, so no time or ancestry preference applies and the earliest adding turn wins. A blank line is never attributed.

Limits. Blame sees only what baton stored, and attributes by text, not by position:

- A line with a secret cut out by redaction no longer matches git's text. A file that baton did not store (ignored, over `max_file_bytes`, an untracked secret-named file) has no turns to match.
- A rename between the base and a turn is not followed: the file is looked up under the name `git blame` reports for the commit, in the base and in every turn.
- Identical lines are not told apart. `}` or an import that two turns both added matches both, and the earliest wins; a thread that adds a line, deletes it and adds it again is credited with the first time. Turns are whole-tree snapshots, so a hand edit saved between two turns is credited to the next turn.
- A thread whose first turn has a `Baton-Base` this repository does not hold cannot be compared with its base, so its first turn adds nothing. The command says how many threads were skipped and to run `git fetch`.
- A line the agent wrote and a person then rewrote by hand is attributed to the agent's turn only if the person's final text equals the agent's.

`--json` prints the `baton.blame.v1` document (`docs/protocol.md`).

### Bisect

`baton bisect <thread> -- <command>...` finds the first turn of a thread at which the command fails, as `git bisect run` does for commits. The exit code of one run is 0 for good, 125 for "cannot test this turn", anything else for bad. A run that outlives `--timeout` (default 300 s; a bare number is seconds, `500ms`, `5m` and `2h` also work, up to 24 h) is bad, and the command's whole process tree is killed, with the `Tree` that kills a plug-in's.

The turns are applied into a scratch worktree made the way `pull` makes one: `git worktree add --detach` at the thread's first `Baton-Base`, then `apply_tree`. It lives in a directory baton makes new under the system temp dir, `baton-bisect-<pid>-<random>/`, with `create_dir` (it fails if the name exists, so a directory that was there before is never reused), holding `owner.lock` and the worktree in `tree/`. Baton removes only that directory, never anything it did not make in this run: `git worktree remove --force` on `tree/`, then the directory. It does this when the command ends, whether it found a turn, failed, or was stopped by Ctrl+C (or SIGTERM). If removal fails, baton says which path is left and the `git worktree remove --force` command to run. Moving between turns applies only the difference. A path the check changed is not safe to move forward, so the worktree is then made again from the base.

A run holds an exclusive lock on `owner.lock` while it lives; the operating system drops it when the process dies, however it dies. A baton killed hard (SIGKILL, `Stop-Process -Force`, power loss) cannot clean up, so the start of every `baton bisect` looks at this repository's registered worktrees. One at `<temp>/baton-bisect-*/tree` whose `owner.lock` nobody holds any more is removed with `git worktree remove --force`, and baton says so. A lock still held means a running bisect, whose worktree is left alone. Worktrees of other repositories wait for a bisect run in those.

The search runs the first turn first, unless `--good <turn>` names a turn assumed good. If the first turn fails, it is the first bad turn and nothing else runs. A first turn that cannot be tested (exit 125) is assumed good and stays a candidate if no later turn passes. Then the latest turn runs, unless `--bad <turn>` names one; a latest turn that passes is an error, since no turn failed. With `--bad 1` and no `--good`, only turn 1 runs: bad (or skipped) reports it, and a pass is an error. A one-turn thread works the same way. With `--good` the named turn is trusted and not run, as `git bisect` trusts its good commit. Turns are named by number. The middle of the untested turns is run next. A skipped turn is not run again, and the next middle is taken from the rest. When skips leave several turns that could be the first bad one, all are listed. The first bad turn is printed in `baton log` form with the paths it changed. Interrupted, the command exits 130.

Limits. The check sees only what baton stored: files left out of a turn (size, ignore rules, secret names) are missing or at their git version in the scratch worktree, so a check that needs an untracked `.env` or `node_modules` fails on every turn. A turn is applied whole, so a failure caused by two threads' work cannot be told apart from one caused by this thread alone. `--timeout` bounds a run, not the search. The stale-worktree sweep is per repository: a killed run's leftovers in another repository stay until a bisect runs there. A check process that outlives a hard-killed baton can hold the scratch directory on Windows; the sweep then reports the path and the command to run.

### Report

`baton report [--since 7d] [--project key] [--json]`, run inside an enrolled worktree (or with `--project`, which finds a worktree of that project the daemon knows), reads the project's side repo and, read-only, the user's repository, and lists patterns in how agents worked. It never starts the daemon and writes nothing to either repository; the only thing it writes is a statistics cache under baton's data dir. `--since` takes `90m`, `48h`, `7d` or `2w`; the default is 7 days. Without a repository on this machine, rework and abandonment are skipped and `repo.available` is false.

Every rule works on **significant lines**. A line is compared by its trimmed text (a re-indent or a CRLF checkout is no change), and a line under three bytes or without a letter or digit (`}`, `);`, a blank) is dropped, since it appears everywhere. A line a change both adds and removes (a move, a re-indent) counts for neither. A turn's changes are measured against the previous turn of its thread, which is its parent turn even when the two were written on machines whose clocks disagree. A thread's first turn is measured against its `Baton-Base` commit in the user's repository the way the turn document is: only paths the store could hold are compared (a file the config excludes, one over the size limit and one named like a secret are not changes), and the commit's side goes through the store's redaction first, so a file the store keeps redacted is unchanged unless the thread changed it. The same comparison gives a thread's abandonment its final-tree changes. A binary file, a file over 1 MiB, or one with over 100,000 significant lines is counted as changed but has no lines, so no rule that needs lines counts it. A diff that takes over 250 ms stops there and keeps the coarser edit script it has, which may list more lines as changed than the minimum. Such a result is used for that run and not cached, since it depends on how busy the machine was.

**Thrashing** has two forms, both within one thread inside the window.
- *File:* a file changed in 3 or more turns of the thread, where some turn removed a line that an earlier turn of the same thread had added. `lines` counts those removals (each removal cancels one earlier addition of the same text, so a line is counted once per add and remove). The entry names the first turn at which both conditions held.
- *Check:* one check name that failed (non-zero), passed, then failed again across the thread's turns that report it. Turns without the check are skipped, and consecutive runs with the same result merge, so red, red, green, red counts and red, green, green does not. The entry names the turn of the second failure.

**Rework** counts a line a thread added in a turn that was later shipped, then changed by a different thread soon after. A line is *shipped* when a non-merge commit in the user's repository's history (first reached from `HEAD`, back to the start of the window, at most 20,000 commits) added the same line text to the same path, with a committer time no earlier than the turn's. It is *reworked* when a turn of a different thread removed that line from that path no earlier than the ship, and within 48 hours of it. A shipment of a line belongs to one thread: the thread whose turn added the line first (two in the same second go to the lower thread name). A thread that wrote the same text earlier or later than the owner, before it shipped, counts it neither as shipped nor as reworked, so two threads that duplicated a line are not both blamed when a third removes it. An entry is one (thread, turn, file) with the count of such lines, the earliest ship time, and the earliest reworking turn.

**Abandonment** applies to a thread whose last turn is more than 24 hours old. Its final tree is compared with its `Baton-Base` commit, giving the lines the thread added and removed. A line the thread added *reached* `HEAD` when `HEAD`'s copy of that file still holds it, and a line it removed reached `HEAD` when `HEAD`'s copy no longer holds it. A file without lines counts as one unit, reached when `HEAD` has the same blob (or, for a removal, no file). The thread is abandoned when fewer than half of its changed lines reached `HEAD`. The entry lists up to 20 files, most unreached lines first, where less than half of the file's changed lines reached `HEAD`.

**Hotspots** rank files by their file-thrash entries plus their rework entries, then by rework, then by path.

**By model and by agent** group every turn in the window by its `Baton-Model` or `Baton-Agent` (turns with no model form one group with a null name). `rework_rate` is reworked lines over shipped lines among the lines that group's turns added. `thrash_rate` is the share of the group's threads with a file or check thrash entry, counting a thread under the group of the turn the entry names.

Limits, stated so the numbers are not read as more than they are:
- Text is matched by line, not by position, so a line copied to another place is still the same line, and two different lines that are textually equal are the same.
- A thread is judged by the files it left in its tree, so a file it created and deleted again shows only as turns.
- Shipping is judged on the history reachable from `HEAD` only. Work merged on another branch, squashed (the lines usually survive, the path may not), or rewritten after a rebase reads as not shipped, and so as abandoned. A force-pushed `HEAD` loses what it dropped.
- A thread whose `Baton-Base` commit is not in this machine's repository is left out of the base-relative parts (its first turn and abandonment) and counted in `skipped.threads_without_base`. A `git fetch` fixes it, and the result is not cached.
- Each thread is walked to its first turn and the window filters its turns by time afterwards, because turns from several machines can be out of time order along the parent links. The cost is reading every thread's commits, not only the recent ones. A window that cuts a thread in the middle keeps the turns inside it, each compared with its parent turn.
- Times are commit times in seconds, so two events in the same second are not ordered.
- The text report shows the 10 strongest entries of each section; `--json` holds all of them.

The per-turn statistics are cached in `report-cache/<project>.bin` under baton's data dir, keyed by the commit id of the turn (and by the id of a user commit or of a thread's last turn), so an entry never goes stale. The key of a first turn or a last turn also names the store policy of the worktree (the settings and every file that feeds them), because those statistics are read through it: a changed `exclude` or redaction rule finds nothing cached. New entries are appended to the file; a run that finds a duplicate or a torn tail, or entries nothing reaches any more, rewrites the file whole instead. An entry is dropped when its turn is no longer reachable from any thread ref, when its last-turn key names a thread that has moved on, or when its commit is no longer in the user's repository (nothing is dropped on a run without the repository). A torn tail loses entries and nothing else, and a version byte changes whenever a statistic's definition does. Deleting the file only costs time. On a project of 1,000 threads of 20 turns (20,000 turns) over 5,000 files and 400 user commits (`e2e/report.sh` with `BATON_E2E_THREADS=1000 BATON_E2E_TURNS=20 BATON_E2E_FILES=5000`), a cold run takes 1.4 s and a cached run 0.45 s; on 300 threads of 10 turns over 2,000 files, the default, 0.29 s and 0.15 s. Most of a cached run is reading every turn commit of every thread (about 0.1 s for 20,000) and looking up each abandoned thread's files in `HEAD`, which is done once per path.

## Verification

`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, real runs of the binaries, measurement tools under `tools/`, and end-to-end scenarios under `e2e/` that drive the real binaries: `e2e/local.sh` for capture (including enrolling a repository, linked worktrees created afterwards, a removed worktree, the watch cap, and the user's `.git` and every worktree's admin dir staying byte-identical) and `e2e/sync.sh` for the server, the daemon link, `pull`, `follow` and the latency measurement, `e2e/history.sh` for `blame` (three threads, a commit that ships some lines, a hand-written line) and `bisect` (a planted failing turn, a skip, a timeout, a failing first turn, a one-turn thread, a planted directory that survives, a hard-killed run cleaned by the next bisect, Ctrl+C on Unix, no worktree left behind)., and `e2e/report.sh` for `report` (one planted case of each pattern next to clean threads, then a cold and a cached run over thousands of turns). No unit tests and no CI workflows.

`e2e/sync.sh` starts real `baton serve` and `baton daemon` processes under a temp dir, each machine with its own `BATON_HOME` and its own clone, on loopback ports from `BATON_E2E_PORT` (default 47880). It never reads or writes the user's real baton dirs, and a final scenario checks that they are untouched.

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

Inside the daemon the median turn takes 3.5 ms to sync, 0.3 ms to commit, 0.2 ms to move the ref, and 3.9 ms in all.

### Sync latency

The target is under 50 ms from hook call on one machine to the ref being stored on a second. `e2e/sync.sh` measures it with a real server and two daemons on one Windows host, the large tree enrolled on both, 100 turns of three edited files, each timed from just before `baton hook claude` is started in Git Bash. Every daemon and the server stamp their log lines with unix microseconds, so one timeline spans the processes and the segments below are read from the logs. The ref in B's side repo is the finish line: B's log says when it was stored, and a poller in bash notices it later.

| Segment | p50 | p95 | max |
|---|---|---|---|
| Hook call to B's ref stored | 54.8 ms | 57.7 ms | 173.0 ms |
| of which the hook process, spawn to exit | 36.3 ms | 49.5 ms | 167.7 ms |
| of which hook exit to B's ref stored | 15.1 ms | 21.0 ms | 22.4 ms |
| A's daemon, hook exit to push sent | 10.5 ms | 16.3 ms | 17.7 ms |
| server, push sent to advance done | 2.4 ms | 3.0 ms | 3.4 ms |
| B's daemon, advance done to ref stored | 2.4 ms | 3.0 ms | 3.5 ms |

The 50 ms target is not met when the hook is started from Git Bash (measured): p50 is 54.8 ms and p95 is 57.7 ms. A hook run with a payload that names no enrolled worktree, 30 times the same way, takes a median 36.9 ms and 49.6 ms at p95 (measured), so the process start accounts for about two thirds of the total. From hook exit to the ref stored on B the figure is 15.1 ms at p50 and 21.0 ms at p95 (measured). The `tools/bench` spawn-to-frame figure of 5.5 ms puts a hook that an agent starts directly near 21 ms at p50 (inferred). No agent started the hook in either run, so whether the target holds in use is not measured. The A-side 10.5 ms includes the 5 ms quiescence wait that the Decisions table explains.

A poller in bash saw the ref a median 13.5 ms after B stored it, so the table reads B's log and not the poller.

A burst of 30 threads, one turn each at 100 ms spacing, takes a median 11.6 ms and at most 15.5 ms from the turn being recorded on A to being stored on B (measured). That is the 30-agent case at a lower rate than 30 hooks at once. The first upload of the 7,384-file tree took 5.1 s (measured).

### Behavior fixed by this slice

- `rewind` never deletes a file. It refuses, in the plan, a file whose stored copy has a redaction marker, a file whose parent is a symlink, any symlink on Windows, a file whose content on disk is not what the last snapshot stored (take a turn first), a file deleted since the last snapshot, and a create over a directory. The refusals are decided before any write, so `--apply` does what the plan showed. A failed write removes its temp file. `--apply` writes the files it can, lists the ones it refused, and exits 1 if any were refused. It compares contents, not timestamps, since two writes inside one timestamp tick are indistinguishable by mtime.
- `log` shows each turn's changed paths as a diff against the thread's previous turn, and for a thread's first turn only the number of files in its tree.
- `Baton-Skipped` values are escaped so a path cannot add a trailer line.
- A turn is a sync of the worktree followed by one commit on the thread's tip. The tree belongs to the worktree and the parent chain to the thread, so a turn's diff against its parent can include changes another thread in the same worktree made in between.
- The writer removes stale ref `.lock` files left in a side repo by a killed daemon when it starts. `e2e/local.sh` kills the daemon during a 60 MB write five times and checks that `fsck` passes, no lock is left, and the restarted daemon captures the file correctly.
- A non-hook, non-daemon command starts the daemon when it is absent, with its standard handles closed, so `$(baton status)` in a shell returns.

### Behavior fixed by the sync slice

- `pull` makes a worktree with `git worktree add --detach <dir> <Baton-Base of the tip>` and fills it with the tip's tree. It fails and says to run `git fetch` when the base commit is not in the repository, and when the thread began in a repository with no commit. The default directory is next to the repository, named `<repo>-<first 8 characters of the thread>`. Files over the size limit (`Baton-Skipped`) keep the git version. A path that `apply_tree` refuses to overwrite is kept and listed, and the command exits 1.
- `follow <thread>` prints each new turn of a thread as it arrives. With `--into <worktree>` it also applies the turn there, but only into a worktree `baton pull` created on this machine (recorded in `pulled.json` in the data dir, by resolved path); any other directory, a path under `.git` included, is refused before anything is read or written. A path is changed or removed only when it still equals the previous turn's content, so an edit made in that worktree is kept and reported, never overwritten. A path that left the tree because the sender stopped storing it (it is in the new turn's `Baton-Skipped`, for example an untracked file whose name marks it as a secret) is left as it is: the file still exists on the sender, and removing it here would lose the copy an earlier turn delivered. A history that was replaced (the new tip does not descend from the last one seen) is announced and applied as one step from what the directory holds.
- Both read the local side repo. The daemon's link keeps it current, so `pull` and `follow` need no connection of their own. `follow` polls the thread's ref every 50 ms.
- The server derives a thread's owner from its tip, and a daemon never pushes a thread another machine owns (see Server and Daemon link).
- A server `refused` carries an optional `detail`, a short sentence that the daemon puts in its log.
- The daemon and the server stamp each log line with Unix seconds and microseconds, which is what makes the latency table above readable from the logs of several processes.

### Differences from the sections above

- Control commands share the hook's endpoint through a second frame type, `BTC1`, rather than listening on another.
- The state of each worktree's stat cache and current tree is saved to `<data>/worktrees/<hash>.json` after each turn and after the baseline. It is a mirror of the side repo, so a lost or unreadable file costs one rescan, not data.

### Differences from the sections above, sync slice

- The owner of a thread is the `Baton-Machine` trailer of its tip. This file and `protocol.md` said `threads.owner` is set by the first authenticated push. That would make SQLite authoritative for something a bare repo already records, against "SQLite is authoritative only for devices". The code follows the repo, and both documents now say so.
- `apply_tree` in `baton-core` did not exist. `rewind --apply`, `pull` and `follow --into` need one routine that writes a tree into a directory without overwriting a file that differs from what baton last put there, so it was added, with the two guards `Fresh` (a new worktree) and `Holds` (a worktree that already has content).
- `objects_sent` carries the server's own transfer number, not the client's `want` number. A client pairs replies with `want`s by order. `protocol.md` says so.
- `refused` has an optional `detail`.
- The `[server]` section of the user config is read when the daemon starts. A change needs a restart.
- Tailscale ships as a config example. No mTLS example is shipped, and the server does not ask for client certificates.

### Differences from the sections above, plug-ins slice

- Verdicts are local only. They are not pushed to the server and not followed from it. A verdict is a judgment one machine made with its own classifier and config, and `refs/baton-verdict/` sits outside the refs the sync link tracks.
- `mode = off` was added beside `shadow` and `live`, so classification can be turned off without removing config.
- `notify` receives the turn document and the verdict together as `{"verdict": ..., "turn": ...}`.
- `[lens]` may name a command as well as a file, with `baton.lens.v1` in and `{"lens": text}` out.
- `files_truncated` was added to `baton.turn.v1`, `lines` in verdict evidence is optional, and a command adapter may print `null` for a payload that is not the end of a turn.
- Plug-in children are started detached on Windows (see Plug-ins, Windows).

### Differences from the sections above, enrollment slice

- A repository can be enrolled (`enroll --repo`), so the allowlist is no longer only a list of worktrees. Its worktrees are enrolled by their first turn, are not persisted, and are bounded and reaped as described in Enrolling a repository.
- The user config gained `[watch] max_repo_worktrees` (default 64). Only the user config may set it, and the daemon reads it at start.
- `find_root` and the new `repo_of` in `baton-core` are the only code that asks git where a path sits. `repo_of` returns the worktree root and the common git dir, both real paths. Neither writes.
- A second caller for a worktree that is being enrolled now waits for the first. It used to get "is already being enrolled", which would have dropped the second hook's turn.
- `baton turn` and a hook route through one delivery function. It resolves the worktree, enrolls it through its repository when needed, and queues the turn under the registry lock.

### Known limits

These are chosen limits, not open bugs. Each says what is not covered and what bounds it.

- The 128-connection handshake pool is global. A host that opens 128 silent sockets holds every place for up to 10 s, and a new connection in that window is dropped at accept. There is no per-address limit, so an operator who needs one puts a firewall or a proxy in front.
- A session authenticated by a command provider is not asked again. Revoking such an identity is the provider's job and takes effect at the identity's next connect. Token auth is checked every 2 s.
- The send timeout cuts a client that reads nothing for 30 s. A client that reads a few bytes a second keeps its session.
- The command provider's tree is killed through the process group (unix) or the Job Object (windows). A descendant that leaves the group with `setsid`, or breaks away from the job, outlives the kill. It no longer delays or changes the verdict.
- A directory named `pwn/` that holds files named `HEAD`, `config` and `hooks` is an ordinary tree to git and to baton. It is stored and written as files. It acts like a repository only if the user enters it and runs git there, as with any repository's contents.
