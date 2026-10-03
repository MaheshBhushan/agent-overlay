import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test"

type Call = { path: string; body: any; signal?: AbortSignal }

let calls: Call[] = []
// What the overlay answers a /permission request with, or a promise to wait on.
let answer: () => Promise<string>
globalThis.fetch = mock((url: string, init: any) => {
  const path = new URL(url).pathname
  calls.push({ path, body: JSON.parse(init.body), signal: init.signal })
  const reply = path === "/permission" ? answer() : Promise.resolve("")
  return reply.then(
    (text) => new Response(text, { headers: { "x-agent-overlay": "1" } }),
  )
}) as any

const { default: plugin } = await import("./opencode-tui")

const tick = (ms = 650) => new Promise((r) => setTimeout(r, ms))

function fakeOpencode() {
  const handlers = new Map<string, (event: any) => void>()
  const state = {
    route: { type: "session", sessionID: "ses_main" } as any,
    status: { ses_main: "idle" } as Record<string, any>,
    parents: { ses_child: "ses_main" } as Record<string, string>,
  }
  const reply = mock(() => Promise.resolve())
  const ctx = {
    location: { directory: "/work" },
    ui: { router: { current: () => state.route } },
    data: {
      on: (type: string, fn: (event: any) => void) => {
        handlers.set(type, fn)
        return () => handlers.delete(type)
      },
      session: {
        status: (id: string) => state.status[id] ?? "idle",
        root: (id: string) => state.parents[id] ?? id,
      },
    },
    client: { permission: { reply } },
  }
  const emit = (type: string, data: any) => handlers.get(type)?.({ type, data })
  return { ctx, state, reply, emit }
}

let oc: ReturnType<typeof fakeOpencode>
let dispose: () => void

beforeEach(async () => {
  calls = []
  answer = () => Promise.resolve("pass")
  oc = fakeOpencode()
  dispose = await plugin.setup(oc.ctx)
})
afterEach(() => dispose())

const events = () => calls.filter((c) => c.path === "/event").map((c) => c.body.status)
const asks = () => calls.filter((c) => c.path === "/permission")
const shell = (id: string, sessionID = "ses_main") => ({
  id,
  sessionID,
  action: "shell",
  resources: ["touch done.txt"],
})

describe("opencode tab status", () => {
  test("reports the shown session, keyed by this tab", async () => {
    await tick()
    oc.state.status.ses_main = { type: "busy" }
    await tick()
    expect(events()).toEqual(["idle", "running"])
    expect(calls[0].body.pids).toEqual([process.pid])
    expect(calls[0].body.cwd).toBe("/work")
  })

  test("an unchanged status is not reposted every poll", async () => {
    await tick(1600)
    expect(events()).toEqual(["idle"])
  })
})

describe("opencode approvals", () => {
  test("the overlay's approval answers the request once", async () => {
    answer = () => Promise.resolve("allow")
    oc.emit("permission.asked", shell("per_1"))
    await tick(50)
    expect(asks()[0].body).toMatchObject({ tool: "shell", summary: "touch done.txt" })
    expect(oc.reply).toHaveBeenCalledWith({
      sessionID: "ses_main",
      requestID: "per_1",
      decision: "once",
    })
  })

  test("the overlay's denial rejects the request", async () => {
    answer = () => Promise.resolve("deny")
    oc.emit("permission.asked", shell("per_2"))
    await tick(50)
    expect(oc.reply.mock.calls[0][0]).toMatchObject({ requestID: "per_2", decision: "reject" })
  })

  test("no decision from the overlay leaves opencode's dialog to decide", async () => {
    oc.emit("permission.asked", shell("per_3"))
    await tick(50)
    expect(asks()).toHaveLength(1)
    expect(oc.reply).not.toHaveBeenCalled()
  })

  test("a subagent's request belongs to the tab showing its parent", async () => {
    oc.emit("permission.asked", shell("per_4", "ses_child"))
    await tick(50)
    expect(asks()).toHaveLength(1)
  })

  test("another tab's session is not ours to raise", async () => {
    oc.emit("permission.asked", shell("per_5", "ses_elsewhere"))
    await tick(50)
    expect(asks()).toHaveLength(0)
  })

  test("answering in the terminal hangs up on the overlay", async () => {
    answer = () => new Promise(() => {})
    oc.emit("permission.asked", shell("per_6"))
    await tick(50)
    const { signal } = asks()[0]
    expect(signal?.aborted).toBe(false)
    oc.emit("permission.replied", { sessionID: "ses_main", requestID: "per_6", reply: "once" })
    expect(signal?.aborted).toBe(true)
  })

  test("status stays quiet while an approval is open", async () => {
    answer = () => new Promise(() => {})
    oc.emit("permission.asked", shell("per_7"))
    oc.state.status.ses_main = { type: "busy" }
    await tick()
    expect(events()).toEqual([])
  })
})
