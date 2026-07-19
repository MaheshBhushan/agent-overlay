# Agent Overlay

Always-on-top desktop HUD for all your coding-agent sessions (Claude Code,
Codex, Gemini CLI, opencode, aider, goose). One dashboard shows every agent
running in tmux and whether each one is still **running** or has gone **idle**
(finished, waiting for you) — with idle duration — so you never have to hunt
through terminals to see which agents need attention.

Built with Tauri v2 (Rust backend, vanilla-TS frontend).

📁 **The project lives in [`agent-overlay/`](agent-overlay/)** — see its
[README](agent-overlay/README.md) for how it works, setup, keybindings, and
the roadmap.

## Quick start

```sh
cd agent-overlay
npm install
npm run tauri dev     # dev mode
npm run tauri build   # release bundle
```

Requires: tmux, Rust, Node, webkit2gtk-4.1.
