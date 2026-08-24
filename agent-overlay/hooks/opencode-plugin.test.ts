import { afterEach, describe, expect, mock, test } from "bun:test"

const fetchMock = mock(() => Promise.resolve(new Response(null, { status: 204 })))
globalThis.fetch = fetchMock as typeof fetch
AbortSignal.timeout = () => new AbortController().signal

const { AgentOverlay } = await import("./opencode-plugin")
const hooks = await AgentOverlay()

const statuses = () => fetchMock.mock.calls.map(([_, init]) =>
  JSON.parse(String(init?.body)).status,
)

const payloads = () => fetchMock.mock.calls.map(([_, init]) =>
  JSON.parse(String(init?.body)),
)

afterEach(() => fetchMock.mockClear())

describe("OpenCode lifecycle events", () => {
  test("tracks authoritative busy and idle status", async () => {
    await hooks.event({ event: { type: "session.status", properties: { status: { type: "busy" } } } })
    await hooks.event({ event: { type: "session.status", properties: { status: { type: "idle" } } } })
    expect(statuses()).toEqual(["running", "idle"])
  })

  test("does not mistake message updates for activity", async () => {
    await hooks.event({ event: { type: "message.updated" } })
    expect(statuses()).toEqual([])
  })

  test("reports approval requests", async () => {
    await hooks.event({ event: { type: "permission.asked" } })
    expect(statuses()).toEqual(["permission"])
  })

  test("identifies this terminal tab by process outside tmux", async () => {
    await hooks.event({ event: { type: "session.idle" } })
    expect(payloads()[0].pids).toEqual([process.pid])
  })
})
