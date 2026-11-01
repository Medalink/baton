# baton design

baton records every coding-agent turn as a git commit and syncs it live between a developer's machines through a server they host. This file is the contract the code is built against. Where code and this file disagree, one of them is wrong, and the change that fixes it updates both.

## Goals and limits

- An agent is never blocked by baton. The hook always exits 0, prints nothing, and never waits on the network or the daemon.
- baton never writes a user's index, `HEAD`, refs, object store, or working tree, except when the user runs `rewind --apply`, `pull`, or `follow --into`. `pull` registers one worktree with `git worktree add --detach` and writes only inside it. `follow --into` writes only inside a worktree `pull` made.
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
baton log [thread] [--verdicts]
baton verdict <turn> [--thread T] [--json]   # the verdict stored for a turn
baton rewind <turn> [--thread T] [--apply]
baton pull <thread> [--into dir]  # a new worktree at the thread's latest turn
baton follow <thread> [--into dir]  # print each new turn; with --into, apply it there too
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

## Verification

`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, real runs of the binaries, measurement tools under `tools/`, and end-to-end scenarios under `e2e/` that drive the real binaries: `e2e/local.sh` for capture and `e2e/sync.sh` for the server, the daemon link, `pull`, `follow` and the latency measurement. No unit tests and no CI workflows.

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
- `follow <thread>` prints each new turn of a thread as it arrives. With `--into <worktree>` it also applies the turn there, but only into a worktree `baton pull` created on this machine (recorded in `pulled.json` in the data dir, by resolved path); any other directory, a path under `.git` included, is refused before anything is read or written. A path is changed or removed only when it still equals the previous turn's content, so an edit made in that worktree is kept and reported, never overwritten. A history that was replaced (the new tip does not descend from the last one seen) is announced and applied as one step from what the directory holds.
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

### Known limits

These are chosen limits, not open bugs. Each says what is not covered and what bounds it.

- The 128-connection handshake pool is global. A host that opens 128 silent sockets holds every place for up to 10 s, and a new connection in that window is dropped at accept. There is no per-address limit, so an operator who needs one puts a firewall or a proxy in front.
- A session authenticated by a command provider is not asked again. Revoking such an identity is the provider's job and takes effect at the identity's next connect. Token auth is checked every 2 s.
- The send timeout cuts a client that reads nothing for 30 s. A client that reads a few bytes a second keeps its session.
- The command provider's tree is killed through the process group (unix) or the Job Object (windows). A descendant that leaves the group with `setsid`, or breaks away from the job, outlives the kill. It no longer delays or changes the verdict.
- A directory named `pwn/` that holds files named `HEAD`, `config` and `hooks` is an ordinary tree to git and to baton. It is stored and written as files. It acts like a repository only if the user enters it and runs git there, as with any repository's contents.
