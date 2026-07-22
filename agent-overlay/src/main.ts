import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  getCurrentWindow,
  currentMonitor,
  primaryMonitor,
  LogicalSize,
  LogicalPosition,
  PhysicalPosition,
} from "@tauri-apps/api/window";

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

  // Collapsed-pill summary.
  $("#pill-running").textContent    = String(running.length);
  $("#pill-idle").textContent       = String(idle.length);
  $("#pill-permission").textContent = String(perms.length);
  $("#pill-perm-wrap").classList.toggle("hidden", perms.length === 0);
  $("#pill").classList.toggle("alert", perms.length > 0);
}

// ── Collapsed ↔ expanded window sizing ─────────────────────────────────────
// The window itself shrinks to just the pill when collapsed, so the large
// transparent area never eats clicks meant for windows underneath. Expanding
// grows the window and re-centres it at the top of the current monitor.
// Width is identical in both states — only the height changes. On Wayland a
// client can't reposition itself, so we must never rely on setPosition to keep
// the pill centred. Instead the window is always the panel's width, the pill is
// centred horizontally at the top, and expanding just grows the height DOWNWARD
// (which the compositor allows). The pill therefore never moves.
const WIDTH = 900;
const COLLAPSED = { w: WIDTH, h: 52 };
const EXPANDED  = { w: WIDTH, h: 620 };
const TOP_MARGIN = 6;
const POS_KEY = "winpos"; // persisted window position (physical px)

let expanded = false;

// Expand/collapse only changes the window HEIGHT (width is constant). The top
// edge stays put, so the centred pill never moves and the panel grows downward.
async function setExpanded(on: boolean) {
  if (expanded === on) return;
  expanded = on;
  document.body.classList.toggle("expanded", on);
  const s = on ? EXPANDED : COLLAPSED;
  await getCurrentWindow()
    .setSize(new LogicalSize(s.w, s.h))
    .catch((e) => console.error("resize failed:", e));
}

// Restore the user's last dropped position; on the very first launch (nothing
// saved yet) park it at the top-centre of the current monitor.
async function restorePosition() {
  const win = getCurrentWindow();
  const saved = localStorage.getItem(POS_KEY);
  if (saved) {
    try {
      const { x, y } = JSON.parse(saved);
      await win.setPosition(new PhysicalPosition(x, y));
      return;
    } catch { /* fall through to default */ }
  }
  let originX = 0;
  let screenW = window.screen.width;
  try {
    const mon = (await currentMonitor()) ?? (await primaryMonitor());
    if (mon) {
      const sf = mon.scaleFactor;
      originX = mon.position.x / sf;
      screenW = mon.size.width / sf;
    }
  } catch { /* fall back to window.screen */ }
  const x = Math.round(originX + (screenW - COLLAPSED.w) / 2);
  await win.setPosition(new LogicalPosition(x, TOP_MARGIN));
}

window.addEventListener("DOMContentLoaded", async () => {
  render();

  // Restore where the user last dropped the pill (or top-centre on first run).
  const win = getCurrentWindow();
  await restorePosition().catch((e) =>
    console.error("initial positioning failed:", e));

  // Float above other windows and stay visible on every workspace. Some
  // compositors (KDE/KWin, GNOME on Wayland) drop the "keep above" hint on
  // focus loss, so we re-assert it whenever the overlay is blurred.
  const assertOverlay = () => {
    win.setAlwaysOnTop(true).catch(() => {});
    win.setVisibleOnAllWorkspaces(true).catch(() => {});
  };
  assertOverlay();
  await listen("tauri://blur", assertOverlay);
  // KWin/Wayland silently drops the "keep above" + "all desktops" hints when
  // focus or the active virtual desktop changes (and no blur event fires on a
  // desktop switch), so re-assert them on a steady tick as a safety net.
  window.setInterval(assertOverlay, 1500);

  // Remember where the user drags it. Only persist while collapsed, so an
  // on-screen clamp of the *expanded* window can't overwrite the resting spot.
  let moveTimer: number | undefined;
  await listen("tauri://move", () => {
    clearTimeout(moveTimer);
    moveTimer = window.setTimeout(async () => {
      if (expanded) return;
      const p = await win.outerPosition();
      localStorage.setItem(POS_KEY, JSON.stringify({ x: p.x, y: p.y }));
    }, 300);
  });

  // Click the pill to drop the panel down; click again to collapse. Dragging is
  // done from the grip (its own drag-region), so a plain click never drags.
  const pill = $("#pill");
  pill.addEventListener("click", (e) => {
    if ((e.target as HTMLElement).closest(".pill-grip")) return;
    setExpanded(!expanded);
  });

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

  // ─ collapses the panel back to the pill.
  $("#btn-minimize").addEventListener("click", () => setExpanded(false));

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
