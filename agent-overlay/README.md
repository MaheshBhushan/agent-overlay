# Agent Overlay

Always-on-top desktop HUD for all your coding-agent sessions (Claude Code,
Codex, Gemini CLI, opencode, aider, goose). One dashboard shows every agent
running in tmux and whether each one is still **running** or has gone **idle**
(finished, waiting for you) — with idle duration — so you never have to hunt
through terminals to see which agents need attention. Designed for agents in
auto-accept mode: no approval plumbing, just status at a glance.

Built with Tauri v2 (Rust backend, vanilla-TS frontend).

## How it works

- A background thread polls tmux every second: `list-panes -a` + a process-tree
  walk finds panes whose command (or any descendant process) is a known agent
  CLI — this catches agents that show up as `node`.
- Status is `running` / `idle` / `error`, decided two ways and OR-ed:
  spinner markers in the pane output ("esc to interrupt", "✻ Thinking…" —
  `src-tauri/src/parser.rs`), plus output-activity tracking — if the pane's
  content changed within the last 5 s it counts as running even when the
  spinner isn't recognized. Idle cards show how long they've been idle.
- Sessions can be launched (`tmux new-session -d`) and killed from the HUD.

## Run

```sh
npm install
npm run tauri dev     # dev mode
npm run tauri build   # release bundle
```

Requires: tmux, Rust, Node, webkit2gtk-4.1.

## Keys

| Key | Action |
|---|---|
| `Ctrl+Shift+Space` | toggle overlay (global) |
| drag titlebar | move the frameless window |

## Wayland notes

- The global shortcut uses X11 global-hotkey APIs; under pure Wayland it may
  not fire (it works via XWayland). On KDE/Hyprland, bind a compositor shortcut
  to re-launching/raising the app, or run the window toggle from the HUD.
- `alwaysOnTop` is honored by most compositors for the frameless window; on
  some you may need a window rule (e.g. KDE: Window Rules → Keep above).

## Layout

```
src-tauri/src/tmux.rs    discovery, capture, activity tracking, launch/kill
src-tauri/src/parser.rs  running/idle/error heuristics (unit-tested)
src-tauri/src/lib.rs     Tauri commands, 1s poll loop, global shortcut
src/main.ts              status dashboard UI
```

## Roadmap

- [x] MVP: detect tmux sessions, show running/idle status + idle duration
- [x] Global shortcut toggle
- [x] Session launch/kill from overlay
- [ ] Desktop notification when a session goes idle
- [ ] Non-tmux detection (kitty/wezterm APIs, WebSocket bridge for agents)
- [ ] Configurable shortcuts, session history search
- [ ] Package for GitHub Releases
