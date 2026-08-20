<h1 align="center">Agent Overlay</h1>

<p align="center">
  An always-on-top HUD that shows every coding-agent session on your machine, and which ones are waiting on you.
</p>

<p align="center">
  <a href="https://github.com/MaheshBhushan/agent-overlay/actions/workflows/agent-overlay-windows.yml"><img alt="Windows build" src="https://img.shields.io/github/actions/workflow/status/MaheshBhushan/agent-overlay/agent-overlay-windows.yml?branch=main&label=windows%20build"></a>
  <a href="https://github.com/MaheshBhushan/agent-overlay/releases"><img alt="Latest release" src="https://img.shields.io/github/v/release/MaheshBhushan/agent-overlay"></a>
  <a href="https://github.com/MaheshBhushan/agent-overlay/releases"><img alt="Downloads" src="https://img.shields.io/github/downloads/MaheshBhushan/agent-overlay/total"></a>
  <a href="https://github.com/MaheshBhushan/agent-overlay/commits/main"><img alt="Last commit" src="https://img.shields.io/github/last-commit/MaheshBhushan/agent-overlay"></a>
  <a href="https://github.com/MaheshBhushan/agent-overlay/issues"><img alt="Open issues" src="https://img.shields.io/github/issues/MaheshBhushan/agent-overlay"></a>
</p>

<p align="center">
  <a href="#install">Install</a> ·
  <a href="#how-it-works">How it works</a> ·
  <a href="#hooks">Hooks</a> ·
  <a href="#controls">Controls</a> ·
  <a href="https://github.com/MaheshBhushan/agent-overlay/releases">Releases</a>
</p>

<!-- HERO: drop assets/demo.gif here — a screencast of the HUD with several sessions,
     one of them landing in Needs Approval. This is the single highest-value addition
     to this README; see the punch-list in the PR description. -->

## Overview

Run more than two or three coding agents at once and the bottleneck stops being the agents — it becomes finding the one that stopped. You end up cycling through tmux panes and terminal windows asking each one whether it is still working.

Agent Overlay watches them all from outside. It finds agent CLIs in tmux panes *and* in plain terminal windows, gives each detected session a short overlay ID (`AO-01`, `AO-02`, …), sorts them into **running**, **idle** (with duration), and **needs approval**, and floats the result above whatever you are doing. Where an agent's CLI supports lifecycle hooks it reports transitions directly, so status is exact rather than inferred.

Tauri v2 — Rust backend, vanilla-TypeScript frontend. Linux and Windows.

## Supported agents

| Badge | CLI | |
|-------|-----|--|
| CC | `claude` | Claude Code |
| CX | `codex` | OpenAI Codex CLI |
| GM | `gemini` | Gemini CLI |
| OC | `opencode` | opencode |
| AI | `aider` | Aider |
| GS | `goose` | Goose |
| PI | `pi` / `pycli` | PyCLI / pi |

## Install

Download from [Releases](https://github.com/MaheshBhushan/agent-overlay/releases):

| File | Platform |
|------|----------|
| `agent-overlay-linux-x86_64` | Linux, run directly |
| `agent-overlay_0.5.0_amd64.deb` | Debian / Ubuntu |
| `agent-overlay-0.5.0-1.x86_64.rpm` | Fedora / RHEL |
| `agent-overlay_0.5.0_x64-setup.exe` | Windows installer (NSIS) |
| `agent-overlay_0.5.0_x64_en-US.msi` | Windows MSI |
| `agent-overlay-windows-x86_64.exe` | Windows, bare exe |

Linux, add it to your app menu:

```sh
chmod +x agent-overlay-linux-x86_64
cat > ~/.local/share/applications/agent-overlay.desktop <<EOF
[Desktop Entry]
Type=Application
Name=Agent Overlay
Exec=/full/path/to/agent-overlay-linux-x86_64
Icon=utilities-system-monitor
Terminal=false
Categories=Development;Utility;
EOF
```

### Build from source

```sh
# Prerequisites: Rust, Node 20+, tmux (Linux), webkit2gtk-4.1 (Linux)
git clone https://github.com/MaheshBhushan/agent-overlay.git
cd agent-overlay/agent-overlay   # the app lives in a subdirectory
npm install
npm run tauri dev      # hot-reload dev build
npm run tauri build    # release bundle
```

> [!NOTE]
> On first launch the overlay installs its status hooks into any agent CLI it finds. It edits files you did not ask it to touch, so it backs each one up first — see [Hooks](#hooks).

## How it works

Every second, the overlay:

- queries `tmux list-panes -a` and walks each pane's process tree, which catches agents running as child `node` processes;
- scans the system process table for agent CLIs outside tmux — IDE terminals, standalone windows — keeping only the top-most match per session.

Each card's `AO-*` label is assigned by the Rust backend, not the webview. Native sessions are keyed by **PID plus process creation time**, so a PID reused by the OS becomes a new overlay session instead of inheriting a stale card or action target. Focus and close commands carry only the opaque `AO-*` ID; the backend resolves it and rechecks the creation time immediately before acting. Tmux sessions remain keyed to their pane plus the agent process identity and close through `tmux kill-pane`.

For plain Linux terminals, closing targets only the selected controlling-terminal session (shell, agent and its tools), first with `SIGTERM` and then `SIGKILL` if necessary. It never signals the terminal-emulator process that may own sibling tabs. On Windows, discovered sessions are creation-time verified before the existing shell-subtree close path runs.

Status comes from three sources, most trusted first:

| Status | How it is decided |
|--------|-------------------|
| `running` | Two consecutive polls with ≥ 80 ms CPU activity — one spike from a repaint or focus change is not enough. For Claude, also a session transcript touched within 120 s. |
| `idle` | No active CPU sample for 10 s. The card shows how long. |
| `permission` | An approval hook event, or an approval prompt matched in the pane text (`Do you want…`, `(Y)es/(N)o`). Shown in **Needs Approval**. |

Hook events override scraping while fresh. Sessions whose CLI has no hooks fall back to scraping automatically, so every agent works with zero setup.

## Hooks

Scraping works everywhere but can only guess at "needs approval" — and on Windows there are no panes to scrape at all. Where a CLI exposes lifecycle hooks, the overlay installs them and gets the answer directly.

It listens on `127.0.0.1:8377`:

```
POST /event  {"status": "running|idle|permission", "pane": "%3", "cwd": "…", "pids": [123]}
```

An event is filed against whatever names **one** session — the tmux pane id, or the reporting hook's ancestor pids, which is how a session outside tmux is identified. `cwd` is a last resort, used only when neither is available: two agents working in one folder is normal, and a key they share cannot mean "this one". Requests carrying `Origin` or `Referer` are ignored, so a web page cannot forge status.

`running`/`idle` override scraping for 2 minutes; `permission` stays sticky for 30 minutes or until the session's next event, whichever comes first.

**Hooks install themselves.** No JSON to merge by hand:

- the Windows installer runs `agent-overlay.exe --install-hooks` after copying files;
- every other install does it on first run;
- **Settings → Status hooks** shows a chip per CLI (`✓` current, `⭯` outdated, dimmed = not installed) with a Reinstall button — use it after moving the binary or installing a new agent CLI;
- scripted setups: `agent-overlay --install-hooks`.

| CLI | Installed into | Approval signal |
|-----|----------------|-----------------|
| Claude Code | `~/.claude/settings.json` (merged) | `Notification`; `PreToolUse` and `Stop` clear it once answered |
| Codex CLI | `~/.codex/hooks/hooks.json` (merged) | `permission_request` — exact |
| opencode | `~/.config/opencode/plugin/agent-overlay.ts` | `permission.ask` — exact |
| pi | `~/.pi/agent/extensions/agent-overlay.ts` | none — pi exposes no approval event, so approvals stay scraped |

Installation is idempotent and non-destructive. Only entries the overlay recognises as its own are ever replaced; everything else in those files is preserved verbatim; the original is copied to `<name>.agent-overlay.bak` before the first edit. A config it cannot parse, or a file already sitting at one of the plugin paths that it did not write, is reported and left alone. Uninstalling leaves the hooks in place — they are inert without the overlay running.

Command hooks invoke the overlay binary (`agent-overlay --hook-event running`) rather than `curl`, so one command works under both `sh` and `cmd.exe`. A hook never fails or blocks its agent: with no overlay running the connection times out silently.

> [!NOTE]
> Codex's hook event names and file layout were read off the shipped binary (0.144), not published docs. If a future codex renames them the hooks stop firing and that CLI degrades to scraping.

## Controls

| Action | How |
|--------|-----|
| Toggle overlay | `Ctrl+Shift+Space` |
| Move window | Drag the titlebar |
| Refresh sessions | Click **⟳** |
| Focus a session | Double-click its card |
| Kill a session | Click **✕** on the card |
| Mute status sounds | Click the speaker |
| Settings | Click the gear |
| Hide overlay | Click **─** |

### Wayland

- The global shortcut uses X11 APIs. It works through XWayland on most setups (KDE Plasma, Hyprland with XWayland); under pure Wayland it may not fire — bind `Ctrl+Shift+Space` to relaunch the app in your compositor instead.
- `alwaysOnTop` is honoured by most compositors. On KDE you may need a Window Rule → *Keep above*.

## Repository structure

```
agent-overlay/
  src-tauri/src/
    lib.rs           Tauri commands, 1s poll loop, global shortcut, first-run hook install
    hooks.rs         local listener for push status, and the --hook-event/--hook-notify client
    hookinstall.rs   idempotent hook installation for claude / codex / opencode / pi
    tmux.rs          tmux pane discovery, capture, activity tracking, kill
    procscan.rs      plain-terminal process scanning, CPU streak detection
    claude_status.rs Claude session-file status (~/.claude/sessions/<pid>.json)
    parser.rs        running/idle/permission heuristics from pane text
    focus.rs         raising the terminal that hosts a session
    sound.rs         native status sounds (bypasses WebView audio)
  hooks/             payloads installed into each agent CLI, plus the NSIS install step
  src/
    main.ts          HUD frontend — session cards, status, settings panel
    styles.css       dark overlay theme
```

## Roadmap

- [x] tmux and plain-terminal session discovery
- [x] Needs Approval column, with push-based hook events and a scraping fallback
- [x] Windows support
- [x] Hooks that install themselves
- [ ] Desktop notification when a session goes idle
- [ ] Per-session output tail in the HUD
- [ ] Configurable agent list and shortcuts

## Contributing

Issues and pull requests are welcome. `cargo test --lib` in `agent-overlay/src-tauri/` runs the suite; the parser, process scanning, hook listener and hook installer are all covered.

## License

Not yet licensed — until a licence file is added, default copyright applies and the code is not free to reuse. Opening an issue is the fastest way to get that fixed.
