mod claude_status;
mod focus;
mod hooks;
mod parser;
mod procscan;
mod sound;
mod tmux;

/// Re-export for the `focus` example / CLI debugging.
pub fn focus_handle(handle: &str) -> Result<(), String> {
    focus::focus(handle)
}

/// All agent sessions: tmux panes plus agents in plain terminals.
/// Fresh hook-reported status (hooks.rs) overrides the scraped status —
/// hooks are exact where present, scraping covers everything else.
pub fn discover_sessions() -> Vec<tmux::AgentSession> {
    let (mut sessions, pane_pids) = tmux::discover_with_pane_pids();
    sessions.extend(procscan::discover(&pane_pids));
    for s in &mut sessions {
        if let Some(status) = hooks::override_for(&s.pane_id, &s.cwd) {
            // For claude, running/idle already comes from its authoritative
            // per-pid session file (procscan/tmux). A hook keyed only on cwd
            // (non-tmux sessions send no pane id) can't tell two claude
            // sessions in one folder apart, so honouring its running/idle would
            // flip an idle sibling whenever the other works. Take only the one
            // state the status file lacks — permission (awaiting approval).
            if s.agent == "claude" && status != "permission" {
                continue;
            }
            if status == "running" {
                s.idle_secs = None;
            } else if s.idle_secs.is_none() {
                s.idle_secs = Some(0);
            }
            s.status = status;
        }
    }
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

/// Play a status sound natively (bypasses WebView audio). "done" → click,
/// "approval" → beeps; anything else is ignored.
#[tauri::command]
fn play_sound(kind: String) {
    match kind.as_str() {
        "done" => sound::play(sound::Sound::Click),
        "approval" => sound::play(sound::Sound::Approval),
        _ => {}
    }
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
            match app.global_shortcut().on_shortcut(shortcut, |app, _s, event| {
                if event.state() == ShortcutState::Pressed {
                    toggle_main_window(app);
                }
            }) {
                Ok(_) => {}
                Err(e) => {
                    // Wayland: X11 XGrab doesn't intercept keys held by native
                    // Wayland windows. The webview keydown handler in main.ts
                    // acts as fallback when the overlay itself has focus.
                    eprintln!("global shortcut registration failed ({e}); \
                        on Wayland, bind Ctrl+Shift+Space in your compositor \
                        settings, or use the ─ button + app launcher to toggle.");
                    // Emit so the UI can show a warning badge.
                    if let Some(win) = app.get_webview_window("main") {
                        let _ = win.eval("window.__shortcutFailed = true; \
                            document.querySelector('.hint') && \
                            (document.querySelector('.hint').textContent = \
                            'shortcut unavailable on Wayland — use compositor binding')");
                    }
                }
            }

            // System-tray icon (bottom-right / status area): a persistent handle
            // to show/hide the overlay and to quit it without hunting the window.
            {
                use tauri::menu::{Menu, MenuItem};
                use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

                let show = MenuItem::with_id(app, "toggle", "Show / Hide overlay", true, None::<&str>)?;
                let quit = MenuItem::with_id(app, "quit", "Quit agent-overlay", true, None::<&str>)?;
                let menu = Menu::with_items(app, &[&show, &quit])?;

                let mut tray = TrayIconBuilder::with_id("main-tray")
                    .tooltip("agent-overlay — click to show/hide")
                    .menu(&menu)
                    .show_menu_on_left_click(false)
                    .on_menu_event(|app, event| match event.id.as_ref() {
                        "toggle" => toggle_main_window(app),
                        "quit" => app.exit(0),
                        _ => {}
                    })
                    .on_tray_icon_event(|tray, event| {
                        if let TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        } = event
                        {
                            toggle_main_window(tray.app_handle());
                        }
                    });
                if let Some(icon) = app.default_window_icon().cloned() {
                    tray = tray.icon(icon);
                }
                tray.build(app)?;
            }

            // Native audio thread for status sounds (bypasses WebView audio).
            sound::init();

            // Push-based status events from agent hooks (see hooks/ examples).
            hooks::serve();

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
            toggle_overlay,
            play_sound
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
