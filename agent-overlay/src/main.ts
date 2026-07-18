import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

interface AgentSession {
  pane_id: string;
  session_name: string;
  window_index: string;
  agent: string;
  cwd: string;
  status: string;
  idle_secs: number | null;
  tail: string[];
}

const AGENT_BADGE: Record<string, string> = {
  claude:    "CC",
  codex:     "CX",
  gemini:    "GM",
  opencode:  "OC",
  aider:     "AI",
  goose:     "GS",
  pi:        "PI",
};

let sessions: AgentSession[] = [];

const $ = <T extends HTMLElement>(sel: string) =>
  document.querySelector(sel) as T;

function esc(s: string): string {
  const div = document.createElement("div");
  div.textContent = s;
  return div.innerHTML;
}

function projectName(cwd: string): string {
  return cwd.split("/").filter(Boolean).pop() ?? cwd;
}

function fmtDuration(secs: number): string {
  if (secs < 60)   return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ${secs % 60}s`;
  return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
}

function cardHtml(s: AgentSession): string {
  const inTmux = !s.pane_id.startsWith("pid:");
  const srcTag = inTmux
    ? ""
    : `<span class="term-tag" title="Plain terminal (not tmux)">term</span>`;
  const badge = AGENT_BADGE[s.agent] ?? s.agent.slice(0, 2).toUpperCase();
  const idleTag = s.idle_secs != null
    ? `<span class="card-idle">${fmtDuration(s.idle_secs)}</span>`
    : "";
  const tailText = s.tail.slice(-2).join("\n").trim();

  return `<div class="card" data-pane="${esc(s.pane_id)}" title="Double-click to open terminal">
    <div class="card-head">
      <span class="agent-badge">${esc(badge)}</span>
      <span class="project" title="${esc(s.cwd)}">${esc(projectName(s.cwd))}</span>
      ${srcTag}
      <button class="kill" data-pane="${esc(s.pane_id)}" title="Kill session">✕</button>
    </div>
    <div class="card-meta">
      <span class="card-path" title="${esc(s.cwd)}">${esc(s.cwd)}</span>
      ${idleTag}
    </div>
    ${tailText ? `<div class="card-tail">${esc(tailText)}</div>` : ""}
  </div>`;
}

function render() {
  const badge      = $("#status-badge");
  const empty      = $("#empty");
  const board      = $("#board");

  const running = sessions.filter(s => s.status === "running");
  const idle    = sessions.filter(s => s.status === "idle");
  const errors  = sessions.filter(s => s.status === "error");

  empty.classList.toggle("hidden", sessions.length > 0);
  board.classList.toggle("hidden", sessions.length === 0);

  $("#cards-running").innerHTML = running.map(cardHtml).join("");
  $("#cards-idle").innerHTML    = idle.map(cardHtml).join("");
  $("#cards-error").innerHTML   = errors.map(cardHtml).join("");

  $("#count-running").textContent = String(running.length);
  $("#count-idle").textContent    = String(idle.length);
  $("#count-error").textContent   = String(errors.length);

  badge.textContent = `${running.length} running · ${idle.length} idle`;
  badge.classList.toggle("hidden", sessions.length === 0);
  badge.classList.toggle("all-idle", running.length === 0 && idle.length > 0);
}

window.addEventListener("DOMContentLoaded", async () => {
  render();

  await listen<AgentSession[]>("sessions-update", (event) => {
    sessions = event.payload;
    render();
  });

  sessions = await invoke<AgentSession[]>("get_sessions");
  render();

  document.body.addEventListener("click", (e) => {
    const el = e.target as HTMLElement;
    if (el.classList.contains("kill")) {
      if (confirm("Kill this agent session?")) {
        invoke("kill_session", { paneId: el.dataset.pane });
      }
    }
  });

  document.body.addEventListener("dblclick", (e) => {
    const card = (e.target as HTMLElement).closest(".card") as HTMLElement | null;
    if (!card?.dataset.pane || (e.target as HTMLElement).classList.contains("kill")) return;
    invoke("focus_session", { paneId: card.dataset.pane }).catch((err) =>
      alert(`Could not open terminal: ${err}`)
    );
  });

  $("#btn-hide").addEventListener("click", () => getCurrentWindow().hide());

  $("#btn-refresh").addEventListener("click", async () => {
    sessions = await invoke<AgentSession[]>("get_sessions");
    render();
  });

  const dialog = $("#launch-dialog") as HTMLDialogElement;
  $("#btn-launch").addEventListener("click", () => dialog.showModal());
  $("#launch-form").addEventListener("submit", (e) => {
    const submitter = (e as SubmitEvent).submitter as HTMLButtonElement | null;
    if (submitter?.value !== "ok") return;
    const agent = ($("#launch-agent") as HTMLSelectElement).value;
    const cwd   = ($("#launch-cwd") as HTMLInputElement).value || "~";
    invoke("launch_session", { agent, cwd }).catch((err) =>
      alert(`Launch failed: ${err}`)
    );
  });
});
