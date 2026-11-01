// A baton plug-in for opencode 2 (`@opencode/plugin` 2.x), which has no command hooks. It sends
// `baton hook opencode` one payload each time a session's turn ends: the session id, the directory the
// session runs in, the model as `<provider>/<model>` and the turn's prompt.
//
// opencode 2 marks the end of a turn with `session.execution.succeeded`, `.failed` or `.interrupted`,
// not opencode 1's `session.idle`. The payload keeps the `session.idle` shape the built-in adapter reads
// (docs/protocol.md), so a failed or stopped turn is recorded too: its files changed all the same.
//
// Install: copy this file to ~/.config/opencode/plugin/baton.js (every project) or to a project's
// .opencode/plugin/. opencode loads each .js file there as a plug-in.
//
// Environment:
//   BATON_BIN  the baton binary (default `baton` on PATH)
//
// One opencode server loads a plug-in once per directory it serves (`opencode serve --service`, which
// `opencode run` shares, and editors' servers), and every copy sees every session's events. Each copy
// therefore sends only the turns of sessions in its own directory, so a turn is recorded once.
//
// The hook runs synchronously, as Claude Code's Stop hook does: `opencode run` exits right after the turn,
// and a child started without waiting could lose its payload. The hook only hands the payload to the
// daemon (about 20 ms), and a 5 s limit bounds a stuck one. Nothing it does can fail the turn.

import { spawnSync } from "node:child_process"

const BATON = process.env.BATON_BIN || "baton"
const ENDED = ["session.execution.succeeded", "session.execution.failed", "session.execution.interrupted"]
const TYPES = ["session.inbox.enqueued", "session.step.started", ...ENDED]

export default {
  id: "baton",
  setup(ctx) {
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
        const payload = {
          type: "session.idle",
          properties: { sessionID: id },
          directory: session.directory,
          model: session.model,
          message: typeof session.message === "string" ? session.message : undefined,
        }
        spawnSync(BATON, ["hook", "opencode"], { input: JSON.stringify(payload), stdio: ["pipe", "ignore", "ignore"], timeout: 5000 })
      }
    }
    watch().catch(() => {})
    return () => abort.abort()
  },
}
