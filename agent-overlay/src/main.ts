import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";

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

// ── Sound effects ─────────────────────────────────────────────────────────
// Played natively from the Rust side (see src-tauri/src/sound.rs) via rodio,
// which talks straight to the system audio backend. This bypasses WebView
// audio entirely — on Linux, WebKitGTK needs GStreamer's autoaudiosink, which
// isn't always present, so an in-page <audio>/Web-Audio would stay silent.
// A click when a session finishes (running → idle); two beeps when one starts
// waiting on you (→ permission). Mute state persisted in localStorage.
let soundOn = localStorage.getItem("sound") !== "off";

function playSound(kind: "done" | "approval", force = false) {
  if (!soundOn && !force) return;
  invoke("play_sound", { kind }).catch(() => { /* audio unavailable — ignore */ });
}

// Previous status per pane, to detect transitions between updates.
let prevStatus = new Map<string, string>();
let primed = false; // skip sounds on the very first snapshot

function updateSessions(next: AgentSession[]) {
  if (primed) {
    for (const s of next) {
      const before = prevStatus.get(s.pane_id);
      if (before && before !== s.status) {
        if (s.status === "idle" && before === "running") playSound("done");
        else if (s.status === "permission") playSound("approval");
      }
    }
  }
  prevStatus = new Map(next.map(s => [s.pane_id, s.status]));
  primed = true;
  sessions = next;
  render();
}

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
  const idleTag = s.status === "permission"
    ? `<span class="card-perm">⚠ approval needed${s.idle_secs != null ? " · " + fmtDuration(s.idle_secs) : ""}</span>`
    : s.idle_secs != null
    ? `<span class="card-idle">idle ${fmtDuration(s.idle_secs)}</span>`
    : s.status === "idle"
    ? `<span class="card-idle">waiting</span>`
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
  const perms   = sessions.filter(s => s.status === "permission");

  empty.classList.toggle("hidden", sessions.length > 0);
  board.classList.toggle("hidden", sessions.length === 0);

  $("#cards-running").innerHTML    = running.map(cardHtml).join("");
  $("#cards-idle").innerHTML       = idle.map(cardHtml).join("");
  $("#cards-permission").innerHTML = perms.map(cardHtml).join("");

  $("#count-running").textContent    = String(running.length);
  $("#count-idle").textContent       = String(idle.length);
  $("#count-permission").textContent = String(perms.length);

  badge.textContent = perms.length > 0
    ? `${perms.length} need approval · ${running.length} running`
    : `${running.length} running · ${idle.length} idle`;
  badge.classList.toggle("hidden", sessions.length === 0);
  badge.classList.toggle("all-idle",
    perms.length === 0 && running.length === 0 && idle.length > 0);
}

window.addEventListener("DOMContentLoaded", async () => {
  render();

  await listen<AgentSession[]>("sessions-update", (event) => {
    updateSessions(event.payload);
  });

  updateSessions(await invoke<AgentSession[]>("get_sessions"));

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

  const win = getCurrentWindow();
  let maximized = false;

  // ─ hides the overlay; ＋ toggles between default size and maximized.
  $("#btn-minimize").addEventListener("click", () => win.hide());
  $("#btn-maximize").addEventListener("click", async () => {
    if (maximized) {
      await win.setSize(new LogicalSize(960, 640));
      maximized = false;
    } else {
      await win.maximize();
      maximized = true;
    }
  });

  $("#btn-refresh").addEventListener("click", async () => {
    updateSessions(await invoke<AgentSession[]>("get_sessions"));
  });

  // Mute / unmute the completion + approval sounds.
  const soundBtn = $("#btn-sound");
  const renderSoundBtn = () => {
    soundBtn.textContent = soundOn ? "🔊" : "🔇";
    soundBtn.title = soundOn ? "Mute sounds" : "Unmute sounds";
  };
  renderSoundBtn();
  soundBtn.addEventListener("click", () => {
    soundOn = !soundOn;
    localStorage.setItem("sound", soundOn ? "on" : "off");
    renderSoundBtn();
    // Always play a test click on the button gesture (even when muting, so you
    // can verify audio works regardless of state) — also unlocks AudioContext.
    playSound("done", true);
  });

  // Auto-refresh when the overlay gains focus (i.e. after toggle shows it).
  await listen("tauri://focus", async () => {
    updateSessions(await invoke<AgentSession[]>("get_sessions"));
  });

  // Fallback: handle Ctrl+Shift+Space inside the webview when it has focus.
  window.addEventListener("keydown", (e) => {
    if (e.ctrlKey && e.shiftKey && e.code === "Space") {
      e.preventDefault();
      invoke("toggle_overlay");
    }
  });
});
