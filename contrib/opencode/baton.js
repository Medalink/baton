// A baton plug-in for opencode 2 (`@opencode/plugin` 2.x), which has no command hooks. It sends
// `baton hook opencode` one payload each time a session's turn ends: the session id, the directory the
// session runs in, the model as `<provider>/<model>` and the turn's prompt.
//
// opencode 2 marks the end of a turn with `session.execution.succeeded`, `.failed` or `.interrupted`,
// not opencode 1's `session.idle`. The payload keeps the `session.idle` shape the built-in adapter reads
// (docs/protocol.md), so a failed or stopped turn is recorded too: its files changed all the same.
//
// Install: copy this file to the `plugin` directory of opencode's global config, which is
// `$XDG_CONFIG_HOME/opencode`, else `~/.config/opencode`, on every OS (Windows included; `opencode debug
// paths` prints it), or to a project's .opencode/plugin/. opencode loads each .js file there as a plug-in.
//
// Which baton to run, first found wins:
//   BATON_BIN  in opencode's environment
//   baton.json beside this file: {"bin": "/path/to/baton", "env": {"BATON_HOME": "..."}}; `env` is added
//              to the hook's environment, for a daemon whose data dir is not the default
//   `baton` on PATH
//
// One opencode server loads a plug-in once per directory it serves (`opencode serve --service`, which
// `opencode run` shares, and editors' servers), and every copy sees every session's events. Each copy
// therefore sends only the turns of sessions in its own directory, so a turn is recorded once.
//
// The hook is started and not waited for: opencode never blocks on it. A missing or failing baton is
// ignored without a word, and a hook still running after 5 s is killed. Plug-ins run in opencode's
// server, which outlives the turn, so the payload is delivered after `opencode run` exits too.

import { spawn } from "node:child_process"
import { readFileSync } from "node:fs"

const ENDED = ["session.execution.succeeded", "session.execution.failed", "session.execution.interrupted"]
const TYPES = ["session.inbox.enqueued", "session.step.started", ...ENDED]

function settings() {
  let local = {}
  try {
    local = JSON.parse(readFileSync(new URL("./baton.json", import.meta.url), "utf8")) ?? {}
  } catch {}
  const env = local.env && typeof local.env === "object" ? local.env : {}
  return { bin: process.env.BATON_BIN || (typeof local.bin === "string" && local.bin) || "baton", env }
}

function hook(baton, payload) {
  try {
    const child = spawn(baton.bin, ["hook", "opencode"], {
      env: { ...process.env, ...baton.env },
      stdio: ["pipe", "ignore", "ignore"],
      windowsHide: true,
    })
    child.on("error", () => {})
    child.stdin.on("error", () => {})
    child.stdin.end(JSON.stringify(payload))
    const limit = setTimeout(() => child.kill(), 5000)
    limit.unref?.()
    child.on("exit", () => clearTimeout(limit))
    child.unref()
  } catch {}
}

export default {
  id: "baton",
  setup(ctx) {
    const baton = settings()
    const sessions = new Map()
    const abort = new AbortController()
    const watch = async () => {
      for await (const event of ctx.event.subscribe({ types: TYPES, signal: abort.signal })) {
        const id = event.data?.sessionID
        if (typeof id !== "string") continue
        const session = sessions.get(id) ?? {}
        sessions.set(id, session)
        if (event.location?.directory) session.directory = event.location.directory
        if (event.type === "session.inbox.enqueued" && event.data.item?.type === "user") session.message = event.data.item.payload?.text
        const model = event.type === "session.step.started" ? event.data.model : null
        if (model?.id) session.model = model.providerID ? `${model.providerID}/${model.id}` : model.id
        if (!ENDED.includes(event.type)) continue
        sessions.delete(id)
        if (session.directory !== ctx.location.directory) continue
        hook(baton, {
          type: "session.idle",
          properties: { sessionID: id },
          directory: session.directory,
          model: session.model,
          message: typeof session.message === "string" ? session.message : undefined,
        })
      }
    }
    watch().catch(() => {})
    return () => abort.abort()
  },
}
