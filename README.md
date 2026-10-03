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
| `running` | Claude reports `busy` in its per-process status file (`~/.claude/sessions/<pid>.json`). Other agents in tmux show a spinner or interrupt hint, or their pane output changed in the last 5 s. Other agents in plain terminals need two consecutive polls with ≥ 80 ms CPU activity, so one spike from a repaint or focus change is not enough. |
| `idle` | Claude reports `idle`, or no activity signal for 10 s. The card shows how long. |
| finished | A session that went from `running` or `permission` to `idle` after at least 3 s of work. Its card is highlighted and sorts to the top of **Idle**, and the pill shows a **✓** count. The flag stays until you double-click the card to focus the session, click the card once, or the session starts working again. |
| `permission` | An approval hook event, or an approval prompt matched in the pane text (`Do you want…`, `(Y)es/(N)o`). Shown in **Needs Approval**. |

Hook events override scraping while fresh. Sessions whose CLI has no hooks fall back to scraping automatically, so every agent works with zero setup.

## Hooks

Scraping works everywhere but can only guess at "needs approval" — and on Windows there are no panes to scrape at all. Where a CLI exposes lifecycle hooks, the overlay installs them and gets the answer directly.

It listens on `127.0.0.1:8377`:

```
POST /event  {"status": "running|idle|permission", "pane": "%3", "cwd": "…", "pids": [123]}
```

An event is filed against whatever names **one** session — the tmux pane id, or the reporting hook's ancestor pids, which is how a session outside tmux is identified. `cwd` is a last resort, used only when neither is available: two agents working in one folder is normal, and a key they share cannot mean "this one". An event that names its `agent` only reaches that agent's sessions through the `cwd` key. Requests carrying `Origin` or `Referer` are ignored, so a web page cannot forge status.

Codex 0.160+ runs every session's hooks inside one shared `codex app-server`, and opencode 2.x runs server plugins in one shared background service. A hook there inherits the environment of whichever terminal started the server. So a hook running under `codex app-server` reports only its folder and agent, never the server's pane or pids, and opencode reports from a TUI plugin, which runs inside each tab. Neither server is listed as a session.

`running`/`idle` override scraping for 2 minutes; `permission` stays sticky for 30 minutes or until the session's next event, whichever comes first.

**Hooks install themselves.** No JSON to merge by hand:

- the Windows installer runs `agent-overlay.exe --install-hooks` after copying files;
- every other install does it on first run;
- **Settings → Status hooks** shows a chip per CLI (`✓` current, `⭯` outdated, dimmed = not installed) with a Reinstall button — use it after moving the binary or installing a new agent CLI;
- scripted setups: `agent-overlay --install-hooks`.

| CLI | Installed into | Approval signal |
|-----|----------------|-----------------|
| Claude Code | `~/.claude/settings.json` (merged) | `PermissionRequest` and `Notification`; `PreToolUse` and `Stop` clear it once answered |
| Codex CLI | `$CODEX_HOME/hooks.json`, default `~/.codex/hooks.json` (merged) | `PermissionRequest`; `PostToolUse` and `Stop` clear it once answered |
| opencode 2.x | `~/.config/opencode/agent-overlay/tui.ts`, listed in `cli.json` | `permission.asked`; `permission.replied` clears it |
| pi | `~/.pi/agent/extensions/agent-overlay.ts` | none — pi exposes no approval event, so approvals stay scraped |

Installation is idempotent and non-destructive. Only entries the overlay recognises as its own are ever replaced; everything else in those files is preserved verbatim; the original is copied to `<name>.agent-overlay.bak` before the first edit. A config it cannot parse, or a file already sitting at one of the plugin paths that it did not write, is reported and left alone. Uninstalling leaves the hooks in place — they are inert without the overlay running.

Command hooks invoke the overlay binary (`agent-overlay --hook-event running`) rather than `curl`, so one command works under both `sh` and `cmd.exe`. A hook never fails or blocks its agent: with no overlay running the connection times out silently.

> [!NOTE]
> Codex asks you to review new or changed hooks once, at startup. Choose **Trust all and continue**; until then Codex doesn't run them and the overlay scrapes Codex panes as before. Versions before 7 wrote Codex hooks to `~/.codex/hooks/hooks.json`, which Codex never read; installing removes the overlay's entries from that file.

### Answering approvals from the overlay

Sessions in Needs Approval show the tool and what it will touch (the command for `Bash`, the path for file tools), with **Approve** and **Deny** buttons:

| CLI | Where the buttons work | How the answer arrives |
|-----|------------------------|------------------------|
| Claude Code | tmux and plain terminals | the `PermissionRequest` hook's decision |
| opencode 2.x | tmux and plain terminals | opencode's permission API, called by the TUI plugin |
| Codex CLI | tmux only | typed into Codex's own prompt |
| pi | — | pi has no approvals of its own |


Claude's `PermissionRequest` hook runs `agent-overlay --hook-permission`. The hook posts the request to `POST /permission` and holds the connection open until you click. Claude shows its own approval dialog at the same time, and whichever answers first wins. Answering in the terminal stays possible. The session's next hook event then removes the buttons from the card. With no overlay running, the hook exits at once and prints nothing. The overlay gives up on an unanswered request after 9½ minutes, and the hook's own timeout is 10 minutes. Requests carrying `Origin` or `Referer` are answered without a decision, so a web page cannot approve anything.

opencode's TUI plugin does the same from inside each tab. On `permission.asked` for the session the tab shows (or one of its subagents), it posts the request and waits. An answer from the overlay goes to opencode's permission API, which closes opencode's own dialog. Answering in opencode instead cancels the overlay's request. The plugin runs in the tab's process, so its pane and pid name the right card.

Codex is different: it runs `PermissionRequest` hooks *before* it shows its prompt, so a hook that waited for the overlay would freeze the terminal. `agent-overlay --hook-codex-permission` reports the request and returns at once. **Approve** then types `y` into the Codex prompt in the clicked card's tmux pane, and **Deny** types `Esc`. Before typing, the overlay checks that the pane shows Codex's approval prompt for this command. Otherwise it refuses and asks you to answer in the terminal, so a stray `y` never lands in the composer or approves a different command. Codex in a plain terminal shows the approval without buttons. Codex asks only when its `approval_policy` allows it to; with `never` there is nothing to answer.

Other agents still show approvals without buttons; answer those in their terminal.

## Controls

| Action | How |
|--------|-----|
| Toggle overlay | `Ctrl+Shift+Space` |
| Expand / collapse the panel | Click the pill; **─** collapses it |
| Move window | Drag the pill's **⠿** grip, or the panel's titlebar |
| Refresh sessions | Click **⟳** |
| Focus a session | Double-click its card |
| Answer an approval (Claude Code, opencode, Codex in tmux) | Click **Approve** or **Deny** on its card |
| Mark a finished session as seen | Click its card |
| Close a session's terminal tab | Click **✕** on the card |
| Close every session | Click **✕ ALL** |
| Mute status sounds | Click the speaker |
| Settings | Click the gear |
| Hide overlay | `Ctrl+Shift+Space`, or click the tray icon |

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
- [x] Answer Claude Code, opencode and Codex approvals from the overlay
- [x] Windows support
- [x] Hooks that install themselves
- [x] Highlight sessions that finished and haven't been looked at
- [ ] Desktop notification when a session goes idle
- [x] Per-session output tail in the HUD (tmux sessions)
- [ ] Configurable agent list and shortcuts

## Contributing

Issues and pull requests are welcome. `cargo test --lib` in `agent-overlay/src-tauri/` runs the suite; the parser, process scanning, hook listener and hook installer are all covered.

## License

[MIT](LICENSE).
