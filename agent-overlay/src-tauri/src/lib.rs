mod claude_status;
mod focus;
mod hookinstall;
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
            // per-pid session file (procscan/tmux). We no longer install hooks
            // that report those (see hookinstall::claude_wanted), but the guard
            // stays: the cwd fallback key is not namespaced by agent, so an
            // opencode or pi session working in the same folder would otherwise
            // hand its running/idle to a claude session sitting next to it.
            // Take only the one state the status file lacks — permission.
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

/// Unconditionally bring the overlay to the front. Used when a second launch
/// hands over to this instance — the user asked for the overlay, so showing it
/// is right even if it was already visible.
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.set_focus();
    }
}

/// The listener must start accepting the instant the port is bound, well before
/// the webview exists — otherwise a second launch racing our start-up connects
/// into the backlog, times out waiting for a reply, and opens its own window.
/// So handovers that arrive early are parked here and replayed once the app is
/// up.
static APP: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();
static SHOW_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn handover_show() {
    match APP.get() {
        Some(app) => show_main_window(app),
        None => SHOW_PENDING.store(true, std::sync::atomic::Ordering::SeqCst),
    }
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

/// Hook state for every supported agent CLI, for the settings panel.
#[tauri::command]
fn hook_status() -> Vec<hookinstall::CliHooks> {
    hookinstall::status()
}

/// Install (or refresh) our hooks in every agent CLI present on this machine.
#[tauri::command]
fn install_hooks() -> Vec<hookinstall::InstallOutcome> {
    hookinstall::install_all()
}

/// Non-GUI invocations. Agent CLIs call this binary to report status, and the
/// Windows installer calls it to install those hooks — neither should start a
/// window, claim the singleton port, or spin up a webview.
///
/// Returns true if the process handled a command and should exit.
fn run_cli() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--hook-event") => {
            // An unknown status is dropped by the listener anyway; nothing to
            // report back to a hook, which must never fail its agent.
            if let Some(status) = args.get(1) {
                hooks::post_event(status);
            }
            true
        }
        Some("--hook-notify") => {
            use std::io::Read;
            let mut payload = String::new();
            let _ = std::io::stdin().read_to_string(&mut payload);
            hooks::post_notification(&payload);
            true
        }
        Some("--install-hooks") => {
            hookinstall::report(&hookinstall::install_all());
            true
        }
        _ => false,
    }
}

/// Whether this machine has had hooks installed by any build of the overlay.
/// Bare-exe and MSI installs have no post-install step to call
/// `--install-hooks`, so first run does it — otherwise those users would keep
/// the scraping-only behaviour with no indication anything was missing.
fn install_hooks_on_first_run(app: &tauri::AppHandle) {
    let Ok(dir) = app.path().app_config_dir() else {
        return;
    };
    install_hooks_once(&dir, hookinstall::install_all);
}

/// Run `install` unless `dir` already records this `HOOKS_VERSION` as done,
/// and record it only if nothing failed.
///
/// `install` is a parameter so this can be tested without writing hooks into
/// the machine running the tests.
fn install_hooks_once(
    dir: &std::path::Path,
    install: impl FnOnce() -> Vec<hookinstall::InstallOutcome>,
) {
    let stamp = dir.join("hooks-installed");
    if std::fs::read_to_string(&stamp).is_ok_and(|s| s.trim() == hookinstall::HOOKS_VERSION) {
        return;
    }
    // Before the install rather than after: the stamp can only land if the
    // directory exists, and without a stamp the install runs again on every
    // launch for the life of the install. Failing to make our own bookkeeping
    // directory is no reason to skip hooks that would have installed fine —
    // those live under $HOME/.claude and friends, a different tree.
    let dir_ready = std::fs::create_dir_all(dir).is_ok();
    let outcomes = install();
    hookinstall::report(&outcomes);
    // Stamping a failed install retires it permanently: nothing retries at the
    // same HOOKS_VERSION, and `report` only reaches stderr, which a GUI launch
    // throws away. Leave it unstamped so the next launch tries again — a repeat
    // is cheap, since install_json/install_file return "unchanged" without
    // touching the disk once a CLI is already set up.
    if dir_ready && !outcomes.iter().any(|o| o.action == "failed") {
        let _ = std::fs::write(&stamp, hookinstall::HOOKS_VERSION);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Hook reporting and hook installation are one-shot CLI commands; they must
    // not fall through into starting the overlay.
    if run_cli() {
        return;
    }

    // Claim the singleton port before building anything. If another overlay is
    // already up it has been asked to show itself, and this launch is done.
    match hooks::claim() {
        // Serve straight away, so a launch racing ours gets a real answer
        // instead of timing out against a bound-but-silent socket.
        hooks::Claim::Primary(l) => hooks::serve(l, handover_show),
        hooks::Claim::Secondary => {
            eprintln!("agent-overlay is already running; showed the existing overlay.");
            return;
        }
        hooks::Claim::Foreign(e) => eprintln!(
            "hook listener failed to bind 127.0.0.1:{}: {e}; \
             starting without push-based status.",
            hooks::PORT
        ),
    }

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

            // Covers installs with no post-install step (bare exe, MSI).
            install_hooks_on_first_run(app.handle());

            // The listener is already running (see run()); wire it to the app
            // now and replay a handover that arrived during start-up.
            let _ = APP.set(app.handle().clone());
            if SHOW_PENDING.swap(false, std::sync::atomic::Ordering::SeqCst) {
                show_main_window(app.handle());
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
            toggle_overlay,
            play_sound,
            hook_status,
            install_hooks
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use hookinstall::InstallOutcome;

    fn outcome(action: &str) -> InstallOutcome {
        InstallOutcome {
            id: "claude",
            name: "Claude Code",
            action: action.into(),
            detail: String::new(),
        }
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("agent-overlay-stamptest-{name}"));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    /// A stamped run is never retried at the same HOOKS_VERSION, so a failure
    /// must not be recorded as done — the user would be left on scraping-only
    /// status forever, with the reason only ever printed to a discarded stderr.
    #[test]
    fn a_failed_install_is_retried_next_launch() {
        let dir = tmp("failed");
        install_hooks_once(&dir, || vec![outcome("installed"), outcome("failed")]);
        assert!(!dir.join("hooks-installed").exists(), "stamped a failure");

        // So the next launch runs it again.
        let mut ran = false;
        install_hooks_once(&dir, || {
            ran = true;
            vec![outcome("installed")]
        });
        assert!(ran, "a failed install was not retried");
        assert!(dir.join("hooks-installed").exists());
    }

    /// "skipped" is a CLI that isn't on this machine — nothing to retry. Once
    /// stamped, a later launch must not run the install again.
    #[test]
    fn a_clean_install_is_stamped_and_not_repeated() {
        let dir = tmp("clean");
        install_hooks_once(&dir, || {
            vec![
                outcome("installed"),
                outcome("unchanged"),
                outcome("skipped"),
            ]
        });
        assert_eq!(
            std::fs::read_to_string(dir.join("hooks-installed")).unwrap(),
            hookinstall::HOOKS_VERSION
        );

        let mut ran = false;
        install_hooks_once(&dir, || {
            ran = true;
            vec![]
        });
        assert!(!ran, "re-ran an install already stamped as done");
    }
}
