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
  session_id: string;
  pane_id: string;
  session_name: string;
  window_index: string;
  agent: string;
  cwd: string;
  status: string;
  idle_secs: number | null;
  tail: string[];
}

/// Mirrors hookinstall::CliHooks — status-hook state for one agent CLI.
interface CliHooks {
  id: string;
  name: string;
  present: boolean;
  installed: boolean;
  outdated: boolean;
  path: string;
  exact_approval: boolean;
  note: string;
}

/// Mirrors hookinstall::InstallOutcome.
interface InstallOutcome {
  id: string;
  name: string;
  action: "installed" | "updated" | "unchanged" | "skipped" | "failed";
  detail: string;
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

// Previous status per verified backend session, to detect transitions.
let prevStatus = new Map<string, string>();
let primed = false; // skip sounds on the very first snapshot

function updateSessions(next: AgentSession[]) {
  if (primed) {
    for (const s of next) {
      const before = prevStatus.get(s.session_id);
      if (before && before !== s.status) {
        if (s.status === "idle" && before === "running") playSound("done");
        else if (s.status === "permission") playSound("approval");
      }
    }
  }
  prevStatus = new Map(next.map(s => [s.session_id, s.status]));
  primed = true;
  sessions = next;
  render();
}

const ESCAPES: Record<string, string> = {
  "&": "&amp;",
  "<": "&lt;",
  ">": "&gt;",
  '"': "&quot;",
  "'": "&#39;",
};

/// Escape for both element and attribute context. The quotes matter: cardHtml
/// interpolates into `title="…"` and `data-session="…"`, and cwd/pane_id come
/// from scanned processes and tmux, so a directory named `x" onmouseover="…`
/// would otherwise close the attribute and run in the webview.
///
/// Done by hand rather than via textContent → innerHTML, which escapes &, <
/// and > but not quotes — correct for element context, silently wrong for
/// attributes, and not fixable in place: that escaping is fixed by the HTML
/// serialization spec.
function esc(s: string): string {
  return s.replace(/[&<>"']/g, (c) => ESCAPES[c]);
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

  return `<div class="card" data-session="${esc(s.session_id)}" title="Double-click to open terminal">
    <div class="card-head">
      <span class="agent-badge">${esc(badge)}</span>
      <span class="session-id" title="Overlay session ID · ${esc(s.pane_id)}">${esc(s.session_id)}</span>
      <span class="project" title="${esc(s.cwd)}">${esc(projectName(s.cwd))}</span>
      ${srcTag}
      <button class="kill" data-session="${esc(s.session_id)}" title="Close terminal tab">✕</button>
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
// On Wayland a client can't reposition itself, so we must never rely on
// setPosition to keep the pill centred. There the window is always the panel's
// width, the pill is centred horizontally at the top, and expanding just grows
// the height DOWNWARD (which the compositor allows). The pill never moves.
//
// Windows/WebView2 does not get that luxury: it paints the transparent host
// window as a visible glass slab, so a 900px-wide window around a ~200px pill
// shows a large translucent rectangle (issue #1). There the collapsed window is
// sized to the measured pill instead, and since Windows *does* honour
// setPosition we re-centre the window on every width change so the pill stays
// put.
const WIDTH = 900;
const COLLAPSED_H = 52;
const EXPANDED_H = 620;
const TOP_MARGIN = 6;
const HUD_PADDING = 12; // #hud's 6px padding on each side, see styles.css
const POS_KEY = "winpos"; // persisted window position (physical px)
const IS_WINDOWS = /Windows/i.test(navigator.userAgent);

const MIN_PILL_W = 80; // sanity floor; a smaller measurement means "not laid out yet"

let expanded = false;
// Window sizing is only allowed once the startup sequence below has placed the
// window. Before that, a stray resize would race restorePosition() and the
// resulting position would be persisted by the tauri://move listener.
let sizingReady = false;
// Serialises resizes. Every resize spans several awaits, and both the pill's
// width changing and the user expanding can trigger one; without this a stale
// continuation could shrink an already-expanded window back to pill size.
let sizingChain: Promise<void> = Promise.resolve();

// Logical width of the collapsed window. Everywhere but Windows this is the full
// panel width. On Windows it hugs the pill, which changes width as the counters
// grow and as the permission badge appears. Returns null if the pill has not
// been laid out yet, in which case the caller must leave the window alone.
function collapsedWidth(): number | null {
  if (!IS_WINDOWS) return WIDTH;
  const pill = Math.ceil($("#pill").getBoundingClientRect().width);
  if (!Number.isFinite(pill) || pill < MIN_PILL_W) return null;
  return pill + HUD_PADDING;
}

// Expand/collapse changes the window height; on Windows it changes the width
// too. The top edge stays put and the window keeps its centre, so the centred
// pill never moves and the panel grows downward.
async function setExpanded(on: boolean) {
  if (expanded === on) return;
  expanded = on;
  document.body.classList.toggle("expanded", on);
  await applySize();
}

// Resize the window to whatever the current state calls for, holding its
// horizontal centre fixed. Calls are serialised and each one re-reads `expanded`
// at the moment it runs, so the last state wins and nothing lands out of order.
function applySize(): Promise<void> {
  sizingChain = sizingChain.then(async () => {
    const w = expanded ? WIDTH : collapsedWidth();
    if (w === null) return; // pill not measurable yet; try again on the next change
    const h = expanded ? EXPANDED_H : COLLAPSED_H;
    const win = getCurrentWindow();

    // Off Windows the width never changes and setPosition is unreliable on
    // Wayland, so a bare resize is both sufficient and safer.
    if (!IS_WINDOWS) {
      await win.setSize(new LogicalSize(WIDTH, h))
        .catch((e) => console.error("resize failed:", e));
      return;
    }

    // innerSize matches what setSize sets; mixing it with outerSize would leave
    // a frame-sized residue that this arithmetic would re-apply forever.
    let before: { x: number; y: number; width: number; scale: number };
    try {
      const [pos, size, scale] = await Promise.all([
        win.outerPosition(), win.innerSize(), win.scaleFactor(),
      ]);
      before = { x: pos.x, y: pos.y, width: size.width, scale };
    } catch (e) {
      console.error("could not read window geometry:", e);
      await win.setSize(new LogicalSize(w, h))
        .catch((err) => console.error("resize failed:", err));
      return;
    }

    const wantW = Math.round(w * before.scale);
    if (wantW === before.width) return; // already the right width; don't touch position
    await win.setSize(new LogicalSize(w, h))
      .catch((e) => console.error("resize failed:", e));
    await win.setPosition(
      new PhysicalPosition(
        await clampToMonitor(before.x + Math.round((before.width - wantW) / 2), wantW),
        before.y,
      ),
    ).catch((e) => console.error("reposition failed:", e));
  }).catch((e) => console.error("sizing failed:", e));
  return sizingChain;
}

// Keep a re-centred window on screen: growing the panel around a pill parked
// near a screen edge would otherwise push half of it onto the next monitor.
async function clampToMonitor(x: number, widthPx: number): Promise<number> {
  try {
    const mon = (await currentMonitor()) ?? (await primaryMonitor());
    if (!mon) return x;
    const left = mon.position.x;
    const right = left + mon.size.width;
    if (widthPx >= mon.size.width) return left;
    return Math.min(Math.max(x, left), right - widthPx);
  } catch {
    return x;
  }
}

// The pill's width changes whenever the counters or the permission badge do, so
// on Windows the collapsed window has to follow it.
function syncCollapsedWidth() {
  if (!IS_WINDOWS || !sizingReady || expanded) return;
  void applySize();
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
  const x = Math.round(originX + (screenW - (collapsedWidth() ?? WIDTH)) / 2);
  await win.setPosition(new LogicalPosition(x, TOP_MARGIN));
}

window.addEventListener("DOMContentLoaded", async () => {
  // The window starts hidden (see tauri.conf.json) so Windows never flashes the
  // full-width glass slab before the first resize lands. Size and place it, then
  // show it — and show it even if that fails, or a broken frontend would leave
  // no window at all (skipTaskbar hides it from the taskbar too).
  const win = getCurrentWindow();
  try {
    render(); // lays the pill out, so its width can be measured below
    const w0 = collapsedWidth();
    if (w0 !== null) {
      await win.setSize(new LogicalSize(w0, COLLAPSED_H))
        .catch((e) => console.error("initial resize failed:", e));
    }
    // Restore where the user last dropped the pill (or top-centre on first run).
    await restorePosition().catch((e) =>
      console.error("initial positioning failed:", e));
  } finally {
    sizingReady = true;
    await win.show().catch((e) => console.error("initial show failed:", e));
  }

  // From here on the collapsed window tracks the pill's width, which changes as
  // the counters grow and as the permission badge appears.
  if (IS_WINDOWS) {
    new ResizeObserver(() => syncCollapsedWidth()).observe($("#pill"));
  }

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
      const sessionId = el.dataset.session;
      if (sessionId && confirm(`Close the terminal tab for ${sessionId}?`)) {
        // Tear down the verified backend target FIRST, then unmount the card.
        // Removing it from state before a successful close would orphan a
        // live session with no card. The `.exiting` class is a transient exit
        // animation that ends in a real unmount, never a resting width:0 state.
        const card = el.closest(".card") as HTMLElement | null;
        card?.classList.add("exiting");
        invoke("kill_session", { sessionId })
          .then(() => {
            // Process is confirmed gone (backend waits for /proc to clear).
            prevStatus.delete(sessionId);
            updateSessions(sessions.filter((s) => s.session_id !== sessionId));
          })
          .catch((err) => {
            card?.classList.remove("exiting");
            alert(`Could not kill session: ${err}`);
          });
      }
    }
  });

  document.body.addEventListener("dblclick", (e) => {
    const card = (e.target as HTMLElement).closest(".card") as HTMLElement | null;
    if (!card?.dataset.session || (e.target as HTMLElement).classList.contains("kill")) return;
    invoke("focus_session", { sessionId: card.dataset.session }).catch((err) =>
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

  // Settings: adjust the expanded panel's opacity (persisted). The value is a
  // percentage 40–100 mapped to the --panel-alpha CSS variable on :root.
  const settings = $("#settings");
  const opacity = $<HTMLInputElement>("#opacity");
  const opacityVal = $("#opacity-val");
  const applyOpacity = (pct: number) => {
    document.documentElement.style.setProperty("--panel-alpha", String(pct / 100));
    opacityVal.textContent = `${pct}%`;
  };
  const savedOpacity = parseInt(localStorage.getItem("panelOpacity") ?? "97", 10);
  opacity.value = String(savedOpacity);
  applyOpacity(savedOpacity);
  opacity.addEventListener("input", () => {
    const pct = parseInt(opacity.value, 10);
    applyOpacity(pct);
    localStorage.setItem("panelOpacity", String(pct));
  });
  // Settings: per-CLI status-hook state, with a one-click (re)install. Hooks
  // are normally written by the installer or on first run; this is the recovery
  // path — after upgrading the binary to a new path, or installing a new agent
  // CLI after the overlay.
  const hookList = $("#hook-list");
  const installBtn = $<HTMLButtonElement>("#btn-install-hooks");

  const renderHooks = (clis: CliHooks[]) => {
    hookList.innerHTML = "";
    for (const c of clis) {
      const chip = document.createElement("span");
      // absent: not on this machine. ok: exact. partial: installed but the CLI
      // can't report approvals exactly. missing/outdated: needs the button.
      const state = !c.present ? "absent"
        : !c.installed ? "missing"
        : c.exact_approval ? "ok" : "partial";
      chip.className = `hook-chip ${state}`;
      chip.textContent = c.id + (c.outdated ? " ⭯" : state === "ok" ? " ✓" : "");
      chip.title = !c.present
        ? `${c.name} isn't installed here`
        : [
            `${c.name} — ${c.outdated ? "outdated hooks" : c.installed ? "hooks installed" : "hooks not installed"}`,
            c.path,
            c.note,
          ].filter(Boolean).join("\n");
      hookList.appendChild(chip);
    }
    const needed = clis.some((c) => c.present && (!c.installed || c.outdated));
    installBtn.textContent = needed ? "Install" : "Reinstall";
  };

  const refreshHooks = async () =>
    renderHooks(await invoke<CliHooks[]>("hook_status"));

  installBtn.addEventListener("click", async () => {
    installBtn.disabled = true;
    installBtn.textContent = "…";
    try {
      const outcomes = await invoke<InstallOutcome[]>("install_hooks");
      const failed = outcomes.filter((o) => o.action === "failed");
      await refreshHooks();
      installBtn.title = failed.length
        ? failed.map((o) => `${o.name}: ${o.detail}`).join("\n")
        : "Hooks are up to date — restart your agent CLIs to pick them up";
    } finally {
      installBtn.disabled = false;
    }
  });

  $("#btn-settings").addEventListener("click", () => {
    settings.classList.toggle("hidden");
    // Cheap enough to re-read on open, and it keeps the chips honest when a CLI
    // is installed while the overlay is running.
    if (!settings.classList.contains("hidden")) void refreshHooks();
  });
  void refreshHooks();

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
