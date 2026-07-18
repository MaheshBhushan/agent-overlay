# Agent Overlay

Always-on-top desktop HUD for all your coding-agent sessions. One dashboard shows every agent running in **tmux panes or plain terminal windows** and whether each one is **running** or **idle** — with idle duration — so you never have to hunt through terminals to find which agents need attention.

Designed for agents in auto-accept mode: no approval plumbing, just status at a glance.

Built with **Tauri v2** (Rust backend, vanilla-TS frontend). Runs on Linux and Windows.

![screenshot placeholder — drag the HUD to any corner of your screen]

## Supported agents

| Badge | CLI | Notes |
|-------|-----|-------|
| CC | `claude` | Claude Code |
| CX | `codex` | OpenAI Codex CLI |
| GM | `gemini` | Gemini CLI |
| OC | `opencode` | opencode |
| AI | `aider` | Aider |
| GS | `goose` | Goose |
| PI | `pi` / `pycli` | PyCLI / pi |

## How it works

**Discovery (every 1 second):**
- Queries `tmux list-panes -a` and walks the process tree of each pane to find agent CLIs — catches agents that run as child `node` processes.
- Scans the full system process table for agent CLIs running outside tmux (IDE terminals, standalone terminal windows). Only the top-most matching process per session is kept.

**Status accuracy:**
- `running` = 2+ consecutive polls with ≥ 80 ms of CPU activity, and (for Claude) a session transcript updated within the last 120 s. This filters out UI repaints and focus bursts that spike CPU for only one poll.
- `idle` = no active CPU sample for 10 s. Idle cards show how long they've been idle.
- `error` = recognized error marker in tmux pane output.

**Windows:** uses the `sysinfo` crate for process scanning and CPU tracking; `ps` and `/proc` are absent so the Windows path is fully self-contained.

## Install

### Pre-built binaries (Linux & Windows)

Download from [Releases](https://github.com/MaheshBhushan/agent-overlay/releases):

| File | Platform |
|------|----------|
| `agent-overlay-linux-x86_64` | Linux (run directly) |
| `agent-overlay_x64-setup.exe` | Windows NSIS installer |
| `agent-overlay_x64_en-US.msi` | Windows MSI installer |
| `agent-overlay-windows-x86_64.exe` | Windows bare exe |

**Linux — add to app menu:**
```sh
chmod +x agent-overlay-linux-x86_64
# Add a .desktop entry so it shows up in your launcher:
cat > ~/.local/share/applications/agent-overlay.desktop <<EOF
[Desktop Entry]
Type=Application
Name=Agent Overlay
Exec=/path/to/agent-overlay-linux-x86_64
Icon=utilities-system-monitor
Terminal=false
Categories=Development;Utility;
EOF
```

### Build from source

```sh
# Prerequisites: Rust, Node 20+, tmux (Linux), webkit2gtk-4.1 (Linux)
npm install
npm run tauri dev     # dev mode with hot reload
npm run tauri build   # release bundle
```

## Controls

| Action | How |
|--------|-----|
| Toggle overlay | `Ctrl+Shift+Space` (global shortcut) |
| Move window | Drag the titlebar |
| Refresh sessions | Click **⟳** |
| Launch new agent | Click **＋**, pick agent + directory |
| Focus session | Double-click a card |
| Kill session | Click **✕** on a card |
| Hide overlay | Click **─** |

## Source layout

```
src-tauri/src/
  lib.rs              Tauri commands, 1s poll loop, global shortcut
  tmux.rs             tmux pane discovery, capture, activity tracking, launch/kill
  procscan.rs         plain-terminal process scanning, CPU streak detection
  claude_activity.rs  Claude transcript freshness check (~/.claude/projects/)
  parser.rs           running/idle/error heuristics from pane output (unit-tested)
src/
  main.ts             HUD frontend — session cards, status badges, actions
  styles.css          dark overlay theme
```

## Wayland notes

- The global shortcut uses X11 APIs; under pure Wayland it may not fire. It works via XWayland on most setups (KDE Plasma, Hyprland with XWayland enabled).
- If the shortcut doesn't work, bind `Ctrl+Shift+Space` to re-launch the app in your compositor settings, or use the **─** button to hide and re-open from your app launcher.
- `alwaysOnTop` is honored by most compositors. On KDE you may need a Window Rule → *Keep above*.

## Roadmap

- [x] tmux session detection with running/idle/error status
- [x] Plain terminal detection (kitty, wezterm, any terminal outside tmux)
- [x] Accurate running status via CPU streak + Claude transcript freshness
- [x] Global shortcut toggle
- [x] Session launch / kill from overlay
- [x] Windows support
- [x] GitHub Releases with Linux + Windows binaries
- [ ] Desktop notification when a session goes idle
- [ ] Per-session output tail in the HUD
- [ ] Configurable agent list and shortcuts
