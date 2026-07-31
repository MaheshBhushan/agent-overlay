// agent-overlay hooks v1 — installed by `agent-overlay --install-hooks`.
// Reports session status to the agent-overlay HUD (listener on 127.0.0.1:8377).
// Fire-and-forget: when the overlay isn't running the fetch fails silently and
// the overlay falls back to process scanning.
//
// Deliberately untyped: importing `@opencode-ai/plugin` would make this file
// fail to load on installs that don't have the package present.

const proc = (globalThis as any).process

const post = (status: "running" | "idle" | "permission") => {
  fetch("http://127.0.0.1:8377/event", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      status,
      // No tmux on Windows: pane is empty there and cwd is the only key.
      pane: proc?.env?.TMUX_PANE ?? "",
      cwd: proc?.cwd?.() ?? "",
    }),
    signal: AbortSignal.timeout(2000),
  }).catch(() => {})
}

export const AgentOverlay = async () => ({
  "tool.execute.before": async () => post("running"),
  "permission.ask": async () => post("permission"),
  event: async ({ event }: { event: { type: string } }) => {
    if (event.type === "session.idle") post("idle")
    if (event.type === "message.updated") post("running")
  },
})
