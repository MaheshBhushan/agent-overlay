//! Bring the terminal that hosts a session to the foreground.
//!
//! Window activation is compositor-specific. On KDE (X11 or Wayland) we
//! inject a one-shot KWin script over DBus that activates the first window
//! owned by any pid in the session's ancestor chain — the same mechanism
//! kdotool uses, without the extra dependency. Elsewhere we fall back to
//! xdotool / wmctrl when present.

#[cfg(windows)]
pub fn focus(_handle: &str) -> Result<(), String> {
    // Window activation is compositor-specific and not yet implemented on
    // Windows. The UI surfaces this error to the user.
    Err("focus is not supported yet on Windows".into())
}

#[cfg(not(windows))]
pub use unix::focus;

#[cfg(not(windows))]
mod unix {
use std::collections::HashMap;
use std::process::Command;

use crate::tmux::process_table;

pub fn focus(handle: &str) -> Result<(), String> {
    if let Some(pid) = handle.strip_prefix("pid:") {
        let pid: u32 = pid.parse().map_err(|_| format!("bad pid handle {handle}"))?;
        let table = process_table();
        activate_window(&ancestors(pid, &table))
    } else {
        focus_tmux_pane(handle)
    }
}

/// pid and its ancestors, nearest first, stopping before init.
fn ancestors(pid: u32, table: &HashMap<u32, (u32, String)>) -> Vec<u32> {
    let mut chain = vec![pid];
    let mut cur = pid;
    while let Some((ppid, _)) = table.get(&cur) {
        if *ppid <= 1 || chain.len() > 32 {
            break;
        }
        chain.push(*ppid);
        cur = *ppid;
    }
    chain
}

fn tmux_out(args: &[&str]) -> Option<String> {
    let out = Command::new("tmux").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Reveal the pane inside tmux, then raise the terminal showing that tmux
/// client — or open a new terminal attached to the session if none is.
fn focus_tmux_pane(pane_id: &str) -> Result<(), String> {
    let info = tmux_out(&[
        "display-message",
        "-p",
        "-t",
        pane_id,
        "#{session_name}\t#{window_index}",
    ])
    .ok_or_else(|| format!("pane {pane_id} not found"))?;
    let (session, window) = info
        .trim_end()
        .split_once('\t')
        .ok_or_else(|| "unexpected tmux output".to_string())?;

    // Make the pane the visible one within its session.
    let target = format!("{session}:{window}");
    tmux_out(&["select-window", "-t", &target]);
    tmux_out(&["select-pane", "-t", pane_id]);

    let clients = tmux_out(&["list-clients", "-t", session, "-F", "#{client_pid}"])
        .unwrap_or_default();
    let client_pids: Vec<u32> = clients
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();

    if client_pids.is_empty() {
        return attach_in_new_terminal(session);
    }

    // Any attached client will now show the selected pane; raise its terminal.
    let table = process_table();
    let mut errs = Vec::new();
    for cpid in &client_pids {
        match activate_window(&ancestors(*cpid, &table)) {
            Ok(()) => return Ok(()),
            Err(e) => errs.push(e),
        }
    }
    Err(errs.join("; "))
}

/// Open a fresh terminal window attached to the tmux session.
fn attach_in_new_terminal(session: &str) -> Result<(), String> {
    let attach = ["tmux", "attach", "-t", session];
    let term = std::env::var("TERMINAL").ok();
    let mut candidates: Vec<Vec<String>> = Vec::new();
    if let Some(t) = term {
        let mut c = vec![t, "-e".into()];
        c.extend(attach.iter().map(|s| s.to_string()));
        candidates.push(c);
    }
    for base in [
        vec!["konsole", "-e"],
        vec!["kitty"],
        vec!["alacritty", "-e"],
        vec!["wezterm", "start"],
        vec!["foot"],
        vec!["gnome-terminal", "--"],
        vec!["xterm", "-e"],
    ] {
        let mut c: Vec<String> = base.iter().map(|s| s.to_string()).collect();
        c.extend(attach.iter().map(|s| s.to_string()));
        candidates.push(c);
    }
    for cmd in candidates {
        if Command::new(&cmd[0]).args(&cmd[1..]).spawn().is_ok() {
            return Ok(());
        }
    }
    Err("no terminal emulator found to attach the session".into())
}

/// Activate the first window owned by any of `pids` (nearest-ancestor first).
fn activate_window(pids: &[u32]) -> Result<(), String> {
    if pids.is_empty() {
        return Err("no candidate pids".into());
    }
    let mut errs = Vec::new();
    match activate_kwin(pids) {
        Ok(()) => return Ok(()),
        Err(e) => errs.push(format!("kwin: {e}")),
    }
    match activate_xdotool(pids) {
        Ok(()) => return Ok(()),
        Err(e) => errs.push(format!("xdotool: {e}")),
    }
    match activate_wmctrl(pids) {
        Ok(()) => return Ok(()),
        Err(e) => errs.push(format!("wmctrl: {e}")),
    }
    Err(errs.join("; "))
}

/// KDE: load a one-shot KWin script that activates by pid. Works on Wayland.
fn activate_kwin(pids: &[u32]) -> Result<(), String> {
    let qdbus = ["qdbus6", "qdbus"]
        .iter()
        .find(|b| Command::new(b).arg("--version").output().is_ok())
        .ok_or("qdbus not available")?;

    let pid_list = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    // Plasma 6 uses windowList/activeWindow, Plasma 5 clientList/activeClient.
    let script = format!(
        r#"const pids = [{pid_list}];
const list = (typeof workspace.windowList === 'function') ? workspace.windowList() : workspace.clientList();
outer:
for (const p of pids) {{
    for (const w of list) {{
        if (w.pid === p) {{
            if (workspace.activeWindow !== undefined) {{ workspace.activeWindow = w; }}
            else {{ workspace.activeClient = w; }}
            break outer;
        }}
    }}
}}"#
    );
    let path = std::env::temp_dir().join(format!("agent-overlay-focus-{}.js", std::process::id()));
    std::fs::write(&path, script).map_err(|e| e.to_string())?;

    let name = format!("agent-overlay-focus-{}", std::process::id());
    // Stale copy from a previous call is fine to unload blindly.
    let _ = Command::new(qdbus)
        .args(["org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.unloadScript", &name])
        .output();
    let out = Command::new(qdbus)
        .args([
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting.loadScript",
            path.to_str().unwrap(),
            &name,
        ])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();

    // Object path differs between Plasma versions; try both.
    let mut ran = false;
    for obj in [format!("/Scripting/Script{id}"), format!("/{id}")] {
        let ok = Command::new(qdbus)
            .args(["org.kde.KWin", &obj, "org.kde.kwin.Script.run"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            ran = true;
            break;
        }
    }
    let _ = Command::new(qdbus)
        .args(["org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.unloadScript", &name])
        .output();
    let _ = std::fs::remove_file(&path);

    if ran {
        Ok(())
    } else {
        Err("could not run KWin script".into())
    }
}

fn activate_xdotool(pids: &[u32]) -> Result<(), String> {
    for pid in pids {
        let ok = Command::new("xdotool")
            .args(["search", "--pid", &pid.to_string(), "windowactivate"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Ok(());
        }
    }
    Err("no window found".into())
}

fn activate_wmctrl(pids: &[u32]) -> Result<(), String> {
    let out = Command::new("wmctrl")
        .args(["-lp"])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err("wmctrl failed".into());
    }
    let listing = String::from_utf8_lossy(&out.stdout).into_owned();
    for pid in pids {
        for line in listing.lines() {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() >= 3 && cols[2] == pid.to_string() {
                let ok = Command::new("wmctrl")
                    .args(["-ia", cols[0]])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if ok {
                    return Ok(());
                }
            }
        }
    }
    Err("no window found".into())
}
} // mod unix
