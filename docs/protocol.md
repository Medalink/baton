# baton protocol, version 1

This file defines everything a third party needs to talk to baton: the WebSocket protocol between a machine's daemon and `baton serve`, the local hook frame, and the JSON documents passed to plug-in commands. Field names are exact. Unknown fields must be ignored. A change that breaks a reader gets a new version number.

## Conventions

- Object ids are lowercase hex SHA-1 strings in JSON and 20 raw bytes in binary frames.
- `thread` is a thread id matching `^[A-Za-z0-9][A-Za-z0-9._-]{0,95}$`.
- `project` is a project key: lowercase, `[a-z0-9._-]` and `/`, no leading or trailing `/`, no `..`, at most 200 bytes. It is derived from the origin URL as `host/path` without a `.git` suffix, or set in `.baton/config.toml`.
- Times are Unix milliseconds.

## Transport

One WebSocket per machine. The client sends the subprotocol `baton.v1` and an `Authorization: Bearer <token>` header. The server refuses any other subprotocol.

Every WebSocket message is binary. Byte 0 is the frame kind:

| Kind | Layout |
|---|---|
| `0x00` control | bytes 1.. are one UTF-8 JSON object with a `"t"` tag |
| `0x01` object | `u32 LE xfer`, `u8 type` (1 commit, 2 tree, 3 blob), 20-byte id, `u64 LE total`, `u64 LE offset`, payload |

An object larger than 1 MiB is split into frames of at most 1 MiB of payload with increasing `offset`. The receiver stores an object only when all `total` bytes have arrived and the git hash of `type` plus bytes equals the id. A disconnect discards partial objects.

`xfer` groups the object frames that precede one control message. The client numbers its transfers with odd numbers and the server with even numbers.

## Control messages

### Client to server

```json
{"t":"hello","proto":1,"machine":"laptop","projects":["github.com/acme/app"],"client":"baton 0.1.0"}
```

```json
{"t":"advance","xfer":7,"project":"github.com/acme/app","thread":"3f2a9c","expected":null,"new":"<commit id>"}
```

`advance` asks the server to move `refs/baton/<thread>` from `expected` (null for a new thread) to `new`. The object frames carrying `xfer` 7 precede it. The client sends the objects its turn created without asking first.

```json
{"t":"want","xfer":9,"project":"github.com/acme/app","ids":["<id>","<id>"]}
```

`want` asks the server for objects, after a `moved` whose objects did not all arrive.

```json
{"t":"subscribe","project":"github.com/acme/app"}
```

### Server to client

```json
{"t":"welcome","proto":1,"machine":"laptop","limits":{"max_frame":1049600,"max_object":67108864},"heads":[{"project":"github.com/acme/app","thread":"3f2a9c","tip":"<commit id>","owner":"laptop"}]}
```

`machine` is the authoritative machine id from auth. `heads` lists every thread tip in the projects the client named, so a reconnecting client knows which local turns the server lacks.

```json
{"t":"advanced","xfer":7,"thread":"3f2a9c","new":"<commit id>"}
```

```json
{"t":"need","xfer":7,"thread":"3f2a9c","ids":["<id>"]}
```

`need` means the closure of `new` is incomplete. The client sends the listed objects in a new transfer, then repeats the same `advance`.

```json
{"t":"refused","xfer":7,"thread":"3f2a9c","reason":"stale","current":"<commit id>"}
```

`reason` is one of `stale` (the ref is not at `expected`), `not_owner` (another machine owns the thread), `machine_mismatch` (the tip's `Baton-Machine` trailer is not the authenticated machine), `invalid` (a malformed object or tree). A `stale` refusal whose `current` equals the client's `new` is a success whose answer was lost.

```json
{"t":"moved","xfer":8,"project":"github.com/acme/app","thread":"3f2a9c","old":"<commit id>","new":"<commit id>","meta":{"turn":12,"subject":"Rename the trap receiver","agent":"claude-code","model":"claude-sonnet-5-5","machine":"laptop","at":1793508000000}}
```

The server sends `moved` to every other subscribed machine after a ref moves. Object frames carrying the same `xfer` precede it when the new objects total at most 256 KiB. Otherwise the follower sends `want` for what it lacks.

```json
{"t":"objects_sent","xfer":10}
```

The end of a server reply to `want`.

```json
{"t":"error","code":"unsupported_version","message":"..."}
```

## Server invariants

1. An object stored on the server has every object it references stored.
2. A ref moves only after the new commit's closure is complete, by compare-and-swap.
3. A thread has one owner machine, set by its first accepted `advance`.
4. Tree entry names are non-empty, contain no `/` or NUL, are not `.`, `..`, or `.git` in any case, and appear once per tree.

These make every server repo a plain bare repo that `git fetch` reads completely.

## Turn commits

```text
<subject>

Baton-Thread: <thread>
Baton-Turn: <sequence number, starting at 1>
Baton-Agent: <agent name>
Baton-Model: <model, may be absent>
Baton-Machine: <machine id>
Baton-Base: <the worktree's HEAD sha when the turn was taken>
Baton-Check: <name>=<exit code>      zero or more
Baton-Skipped: <path>                zero or more, files over the size limit
```

A thread's first turn has no parent. Each later turn's parent is the thread's previous turn. Author and committer are `baton <baton@localhost>`.

## Local hook frame

The hook writes one frame to `\\.\pipe\baton-<user>` on Windows or `$XDG_RUNTIME_DIR/baton/daemon.sock` (falling back to `$TMPDIR/baton-<uid>/daemon.sock`) elsewhere:

```text
"BTN1"  u32 LE length  <agent name> "\n" <raw hook payload>
```

`length` counts the bytes after itself. The payload is at most 1 MiB. The daemon sends nothing back.

## Plug-in commands

Each plug-in is a command in user config. baton writes one JSON document to its stdin, closes stdin, and reads one JSON document from stdout within the configured timeout. A timeout, a non-zero exit, or unparseable output is a failure: classify, decide and notify fail open, auth fails closed.

### `baton.turn.v1`, the input to classify and notify

```json
{
  "schema": "baton.turn.v1",
  "project": "github.com/acme/app",
  "thread": "3f2a9c",
  "turn": 12,
  "commit": "<commit id>",
  "parent": "<commit id or null>",
  "base": "<worktree HEAD sha>",
  "subject": "Rename the trap receiver",
  "agent": "claude-code",
  "model": "claude-sonnet-5-5",
  "machine": "laptop",
  "at": 1793508000000,
  "checks": [{"name": "clippy", "exit": 0}],
  "files": [{"path": "src/trap.rs", "status": "modified", "added": 12, "removed": 4}],
  "diff": "<unified diff against the parent turn, or against base for the first turn, redacted, at most 256 KiB>",
  "diff_truncated": false,
  "lens": "<contents of the lens file, or null>"
}
```

### `baton.verdict.v1`, the output of classify

```json
{
  "schema": "baton.verdict.v1",
  "class": "check",
  "findings": [
    {"kind": "one_way", "score": 0.91, "evidence": [{"path": "db/migrate/0042.sql", "lines": [3, 7]}], "note": "drops the users.legacy_id column"}
  ]
}
```

`class` is one of `react`, `check`, `log`, `quiet`. `score` is between 0 and 1. `kind` is free text. baton stores the verdict on `refs/baton-verdict/<commit>` and never rewrites the turn.

### `baton.ask.v1` and `baton.answer.v1`, for decide

```json
{"schema": "baton.ask.v1", "project": "github.com/acme/app", "thread": "3f2a9c", "question": "Keep the old endpoint?", "options": ["keep", "remove"], "context": "<free text>"}
```

```json
{"schema": "baton.answer.v1", "choice": "remove", "confidence": 0.82, "reason": "matches 4 earlier decisions", "ask_human": false}
```

`ask_human: true`, a failure, or no configured decider means `baton ask` reports that a human must answer.

### `baton.auth.v1`, for a command auth provider

```json
{"schema": "baton.auth.v1", "remote_addr": "100.64.0.7:51234", "headers": {"authorization": "Bearer ..."}, "client_cert_sha256": null}
```

```json
{"ok": true, "machine": "laptop"}
```

### `baton.turnreq.v1`, the output of an agent adapter

```json
{"schema": "baton.turnreq.v1", "cwd": "/home/me/app", "thread": "3f2a9c", "subject": "Rename the trap receiver", "agent": "my-agent", "model": null, "checks": []}
```

A command adapter receives the raw hook payload on stdin. Built-in adapters exist for `claude-code`, `codex`, `opencode` and `pi`.
