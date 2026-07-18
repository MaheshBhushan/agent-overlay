mod claude_activity;
mod focus;
mod parser;
mod procscan;
mod tmux;

/// Re-export for the `focus` example / CLI debugging.
pub fn focus_handle(handle: &str) -> Result<(), String> {
    focus::focus(handle)
}

/// All agent sessions: tmux panes plus agents in plain terminals.
pub fn discover_sessions() -> Vec<tmux::AgentSession> {
    let (mut sessions, pane_pids) = tmux::discover_with_pane_pids();
    sessions.extend(procscan::discover(&pane_pids));
    sessions
}

use tauri::{Emitter, Manager};

#[tauri::command]
fn get_sessions() -> Vec<tmux::AgentSession> {
    discover_sessions()
}

#[tauri::command]
fn send_text(pane_id: String, text: String) -> Result<(), String> {
    if pane_id.starts_with("pid:") {
        return Err("session is not in tmux — type in its own terminal".into());
    }
    tmux::send_keys(&pane_id, &text, true)
}

#[tauri::command]
fn kill_session(pane_id: String) -> Result<(), String> {
    if pane_id.starts_with("pid:") {
        return procscan::kill(&pane_id);
    }
    tmux::kill_pane(&pane_id)
}

#[tauri::command]
fn launch_session(agent: String, cwd: String) -> Result<String, String> {
    tmux::launch(&agent, &cwd)
}

/// Bring the terminal hosting this session to the foreground.
#[tauri::command]
fn focus_session(pane_id: String) -> Result<(), String> {
    focus::focus(&pane_id)
}

#[tauri::command]
fn capture_output(pane_id: String) -> String {
    tmux::capture_pane(&pane_id, 200)
}

fn toggle_main_window(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let visible = win.is_visible().unwrap_or(true);
        if visible {
            let _ = win.hide();
        } else {
            let _ = win.show();
            let _ = win.set_focus();
        }
    }
}

#[tauri::command]
fn toggle_overlay(app: tauri::AppHandle) {
    toggle_main_window(&app);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            // Global toggle shortcut. On Wayland this depends on compositor
            // support (works under X11/XWayland); the tray/UI toggle still works.
            use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
            let shortcut = "ctrl+shift+space";
            if let Err(e) = app.global_shortcut().on_shortcut(shortcut, |app, _s, event| {
                if event.state() == ShortcutState::Pressed {
                    toggle_main_window(app);
                }
            }) {
                eprintln!("global shortcut unavailable ({e}); use the window toggle instead");
            }

            // Poll tmux every second and push state to the UI.
            let handle = app.handle().clone();
            std::thread::spawn(move || loop {
                let sessions = discover_sessions();
                let _ = handle.emit("sessions-update", &sessions);
                std::thread::sleep(std::time::Duration::from_secs(1));
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_sessions,
            focus_session,
            send_text,
            kill_session,
            launch_session,
            capture_output,
            toggle_overlay
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
