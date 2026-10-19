import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import { emit } from "@tauri-apps/api/event";
import snapshots from "./fixtures/sessions-update.json";

type Sessions = typeof snapshots.initial;
type Call = { command: string; args: Record<string, unknown> };

declare global {
  interface Window {
    overlayTest: {
      calls: Call[];
      replay: (sessions: Sessions) => Promise<void>;
      fail: string | null;
      hold: string | null;
      release: () => void;
    };
  }
}

let sessions = snapshots.initial;
let release = () => {};
window.overlayTest = {
  calls: [],
  fail: null,
  hold: null,
  release: () => release(),
  replay: async (next) => {
    sessions = next;
    await emit("sessions-update", next);
  },
};
mockWindows("main");
mockIPC(async (command, payload) => {
  const args = (payload ?? {}) as Record<string, unknown>;
  const state = window.overlayTest;
  state.calls.push({ command, args });
  if (state.fail === command) throw new Error("Fixture command failed");
  if (state.hold === command) await new Promise<void>(resolve => { release = resolve; });
  switch (command) {
    case "get_sessions": return sessions;
    case "hook_status": return [];
    case "plugin:window|current_monitor":
    case "plugin:window|primary_monitor": return null;
    case "plugin:window|outer_position": return { x: 0, y: 0 };
    case "plugin:window|inner_size": return { width: 900, height: 620 };
    case "plugin:window|scale_factor": return 1;
    default:
      if (command.startsWith("plugin:window|") ||
          ["focus_session", "mark_seen", "answer_approval", "kill_session", "play_sound", "toggle_overlay"].includes(command)) return;
      throw new Error(`Unexpected IPC command: ${command}`);
  }
}, { shouldMockEvents: true });
