// agent-overlay hooks v1 — installed by `agent-overlay --install-hooks`.
// Reports session status to the agent-overlay HUD (listener on 127.0.0.1:8377).
// Fire-and-forget: when the overlay isn't running the fetch fails silently and
// the overlay falls back to process scanning.
//
// Note: pi's extension API (pi.on) exposes no approval/permission event — its
// own tool-approval prompt is not observable from an extension. So this covers
// running/idle exactly and the overlay keeps scraping pi's pane text to spot
// "needs approval".

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
      // pi loads extensions in the session process, so this is also the
      // overlay's per-tab process key outside tmux.
      pids: proc?.pid ? [proc.pid] : [],
    }),
    signal: AbortSignal.timeout(2000),
  }).catch(() => {})
}

export default function (pi: any) {
  pi.on("turn_start", () => post("running"))
  pi.on("tool_execution_start", () => post("running"))
  pi.on("agent_settled", () => post("idle"))
  pi.on("session_shutdown", () => post("idle"))
}
