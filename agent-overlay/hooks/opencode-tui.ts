// agent-overlay hooks v2 — installed by `agent-overlay --install-hooks`.
// Reports this opencode tab to the agent-overlay HUD (listener on
// 127.0.0.1:8377) and lets the HUD answer the tab's approvals.
//
// This is an opencode 2.x *TUI* plugin, loaded from cli.json's `plugins`. It
// has to be: opencode 2.x runs server plugins in one background service that
// every tab shares, so a server plugin's TMUX_PANE and pid name the service,
// not a tab. Here in the tab's own process they are exactly the keys the
// overlay names a session by — the tmux pane, or this pid outside tmux.
//
// Fire-and-forget: when the overlay isn't running every request fails at
// once and opencode carries on as if this file weren't here.
//
// Deliberately untyped: importing `@opencode-ai/plugin` would make this file
// fail to load on installs that don't have the package present.

const proc = (globalThis as any).process
const HUD = "http://127.0.0.1:8377"
/** Re-report an unchanged status this often; the overlay drops it after 120s. */
const REFRESH_MS = 60_000
const POLL_MS = 500

type Status = "running" | "idle"

export default {
  id: "agent-overlay",

  async setup(ctx: any) {
    const who = {
      // No tmux on Windows: pane is empty there and the pid is the key.
      pane: proc?.env?.TMUX_PANE ?? "",
      cwd: ctx?.location?.directory ?? proc?.cwd?.() ?? "",
      pids: proc?.pid ? [proc.pid] : [],
    }
    const post = (path: string, body: object, signal?: AbortSignal) =>
      fetch(`${HUD}${path}`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ ...who, ...body }),
        signal,
        // Bun's fetch has a default timeout; an approval may wait minutes.
        timeout: false,
      } as any)

    // The session this tab is showing, and whether a request belongs to it.
    // Subagents ask in their own child sessions, so compare session trees.
    const shown = (): string | undefined => {
      const route = ctx.ui?.router?.current?.()
      return route?.type === "session" ? route.sessionID : undefined
    }
    const root = (id: string) => {
      try {
        return ctx.data?.session?.root?.(id) ?? id
      } catch {
        return id
      }
    }
    const ours = (sessionID: string) => {
      const s = shown()
      return !!s && root(sessionID) === root(s)
    }

    // ── status ─────────────────────────────────────────────────────────
    let last: Status | undefined
    let lastAt = 0
    const report = (status: Status) => {
      last = status
      lastAt = Date.now()
      post("/event", { status }, AbortSignal.timeout(2000)).catch(() => {})
    }

    // ── approvals ──────────────────────────────────────────────────────
    // requestID → the overlay request waiting on it. Aborting it is how the
    // overlay learns the request was answered here in the terminal.
    const open = new Map<string, AbortController>()

    const ask = (p: any) => {
      const waiting = new AbortController()
      open.set(p.id, waiting)
      const done = () => {
        if (open.get(p.id) !== waiting) return false
        open.delete(p.id)
        // Re-report after the dialog: the card is still in Needs Approval.
        last = undefined
        return true
      }
      post(
        "/permission",
        { tool: p.action ?? "permission", summary: (p.resources ?? []).join("\n") },
        waiting.signal,
      )
        .then((r) => (r.headers.get("x-agent-overlay") ? r.text() : "pass"))
        .then((decision) => {
          if (!done() || (decision !== "allow" && decision !== "deny")) return
          return ctx.client.permission.reply({
            sessionID: p.sessionID,
            requestID: p.id,
            decision: decision === "allow" ? "once" : "reject",
            ...(decision === "deny" ? { message: "The user denied this from Agent Overlay." } : {}),
          })
        })
        .catch(() => done())
    }

    const off = [
      ctx.data.on("permission.asked", (event: any) => {
        const p = event?.data
        if (p?.id && p?.sessionID && ours(p.sessionID)) ask(p)
      }),
      ctx.data.on("permission.replied", (event: any) => {
        const waiting = open.get(event?.data?.requestID)
        if (!waiting) return
        open.delete(event.data.requestID)
        last = undefined
        waiting.abort()
      }),
    ]

    const poll = setInterval(() => {
      // While an approval is open its own request speaks for the session:
      // a running/idle report would retire it on the overlay.
      if (open.size) return
      const s = shown()
      const raw = s ? ctx.data?.session?.status?.(s) : "idle"
      const kind = typeof raw === "string" ? raw : raw?.type
      const status: Status = !kind || kind === "idle" ? "idle" : "running"
      if (status !== last || Date.now() - lastAt > REFRESH_MS) report(status)
    }, POLL_MS)

    return () => {
      clearInterval(poll)
      for (const unsubscribe of off) unsubscribe?.()
      for (const waiting of open.values()) waiting.abort()
    }
  },
}
