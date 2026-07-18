//! tmux discovery: find panes running coding agents, capture their output,
//! and classify each as running / idle / error.

use serde::Serialize;
use std::collections::HashMap;
use std::process::Command;
use std::sync::Mutex;
use std::time::Instant;

use crate::parser;

#[derive(Debug, Clone, Serialize)]
pub struct AgentSession {
    /// tmux pane id, e.g. "%3" — the handle used for send-keys/capture-pane.
    pub pane_id: String,
    pub session_name: String,
    pub window_index: String,
    pub agent: String,
    pub cwd: String,
    /// "running" | "idle" | "error"
    pub status: String,
    /// Seconds since the pane's output last changed; None while running.
    pub idle_secs: Option<u64>,
    pub tail: Vec<String>,
}

/// Known agent CLIs, matched against the pane command and its descendants.
const AGENTS: &[(&str, &str)] = &[
    ("claude", "claude"),
    ("codex", "codex"),
    ("gemini", "gemini"),
    ("opencode", "opencode"),
    ("aider", "aider"),
    ("goose", "goose"),
];

/// Output stability window: if a pane's content changed within this many
/// seconds it counts as running even without a recognized spinner marker.
const ACTIVITY_WINDOW_SECS: u64 = 5;

struct PaneActivity {
    content_hash: u64,
    last_change: Instant,
}

/// Per-pane activity state, shared between the poll loop and commands.
static ACTIVITY: Mutex<Option<HashMap<String, PaneActivity>>> = Mutex::new(None);

fn hash_text(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

fn tmux(args: &[&str]) -> Option<String> {
    let out = Command::new("tmux").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Map of pid -> (ppid, argv) for the whole system, one `ps` call.
pub fn process_table() -> HashMap<u32, (u32, String)> {
    let mut table = HashMap::new();
    let Some(out) = Command::new("ps")
        .args(["-e", "-o", "pid=,ppid=,args="])
        .output()
        .ok()
    else {
        return table;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (it.next(), it.next()) else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse(), ppid.parse()) else {
            continue;
        };
        let args: String = it.collect::<Vec<_>>().join(" ");
        table.insert(pid, (ppid, args));
    }
    table
}

/// Match an argv string against the known agent CLIs
/// ("claude", "/usr/bin/claude", "node .../claude" etc.).
pub fn agent_from_args(args: &str) -> Option<String> {
    let lower = args.to_lowercase();
    for (name, label) in AGENTS {
        if lower
            .split_whitespace()
            .any(|tok| tok == *name || tok.ends_with(&format!("/{name}")))
        {
            return Some(label.to_string());
        }
    }
    None
}

/// Find which agent (if any) runs inside a pane by walking the process tree
/// rooted at the pane's shell pid. Handles agents that show up as `node`.
fn detect_agent(pane_pid: u32, pane_cmd: &str, table: &HashMap<u32, (u32, String)>) -> Option<String> {
    for (name, label) in AGENTS {
        if pane_cmd == *name {
            return Some(label.to_string());
        }
    }
    let mut stack = vec![pane_pid];
    let mut depth = 0;
    while let Some(pid) = stack.pop() {
        depth += 1;
        if depth > 64 {
            break;
        }
        if let Some((_, args)) = table.get(&pid) {
            if let Some(label) = agent_from_args(args) {
                return Some(label);
            }
        }
        for (child, (ppid, _)) in table.iter() {
            if *ppid == pid {
                stack.push(*child);
            }
        }
    }
    None
}

/// Capture the last `lines` of a pane's visible output.
pub fn capture_pane(pane_id: &str, lines: u32) -> String {
    tmux(&[
        "capture-pane",
        "-p",
        "-t",
        pane_id,
        "-S",
        &format!("-{lines}"),
    ])
    .unwrap_or_default()
}

/// Discover all agent sessions across every tmux session, classifying each
/// as running/idle/error using output markers plus change tracking.
pub fn discover() -> Vec<AgentSession> {
    discover_with_pane_pids().0
}

/// Like `discover`, but also returns the shell pid of *every* tmux pane
/// (agent or not) so the process scanner can exclude tmux-rooted processes.
pub fn discover_with_pane_pids() -> (Vec<AgentSession>, Vec<u32>) {
    let Some(out) = tmux(&[
        "list-panes",
        "-a",
        "-F",
        "#{pane_id}\t#{session_name}\t#{window_index}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_pid}",
    ]) else {
        return (Vec::new(), Vec::new()); // no tmux server running
    };

    let table = process_table();
    let mut sessions = Vec::new();
    let mut pane_pids = Vec::new();
    let mut guard = ACTIVITY.lock().unwrap();
    let activity = guard.get_or_insert_with(HashMap::new);
    let now = Instant::now();
    let mut seen: Vec<String> = Vec::new();

    for line in out.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() != 6 {
            continue;
        }
        let (pane_id, session_name, window_index, cmd, cwd, pid) =
            (parts[0], parts[1], parts[2], parts[3], parts[4], parts[5]);
        let Ok(pid) = pid.parse::<u32>() else { continue };
        pane_pids.push(pid);
        let Some(agent) = detect_agent(pid, cmd, &table) else {
            continue;
        };
        seen.push(pane_id.to_string());

        let text = capture_pane(pane_id, 60);
        let parsed = parser::parse(&text);

        let hash = hash_text(&text);
        let entry = activity.entry(pane_id.to_string()).or_insert(PaneActivity {
            content_hash: hash,
            last_change: now,
        });
        if entry.content_hash != hash {
            entry.content_hash = hash;
            entry.last_change = now;
        }
        let since_change = now.duration_since(entry.last_change).as_secs();

        let (status, idle_secs) = match parsed.status.as_str() {
            "running" => ("running".to_string(), None),
            // Output still moving → running even without a spinner marker.
            // For claude, pane content also moves on focus repaints/typing,
            // so additionally require a fresh session transcript.
            _ if since_change < ACTIVITY_WINDOW_SECS
                && (agent != "claude" || crate::claude_activity::active_within(cwd, 120)) =>
            {
                ("running".to_string(), None)
            }
            "error" => ("error".to_string(), Some(since_change)),
            _ => ("idle".to_string(), Some(since_change)),
        };

        sessions.push(AgentSession {
            pane_id: pane_id.to_string(),
            session_name: session_name.to_string(),
            window_index: window_index.to_string(),
            agent,
            cwd: cwd.to_string(),
            status,
            idle_secs,
            tail: parsed.tail,
        });
    }

    activity.retain(|k, _| seen.contains(k));
    (sessions, pane_pids)
}

/// Send literal keys to a pane, optionally followed by Enter.
pub fn send_keys(pane_id: &str, text: &str, enter: bool) -> Result<(), String> {
    let mut args = vec!["send-keys", "-t", pane_id, "-l", text];
    if text.is_empty() {
        args = vec!["send-keys", "-t", pane_id];
    }
    tmux(&args).ok_or_else(|| format!("send-keys to {pane_id} failed"))?;
    if enter {
        tmux(&["send-keys", "-t", pane_id, "Enter"])
            .ok_or_else(|| format!("send Enter to {pane_id} failed"))?;
    }
    Ok(())
}

pub fn kill_pane(pane_id: &str) -> Result<(), String> {
    tmux(&["kill-pane", "-t", pane_id]).ok_or_else(|| format!("kill-pane {pane_id} failed"))?;
    Ok(())
}

/// Launch an agent in a fresh detached tmux session.
pub fn launch(agent: &str, cwd: &str) -> Result<String, String> {
    let name = format!(
        "overlay-{}-{}",
        agent,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() % 100_000)
            .unwrap_or(0)
    );
    tmux(&["new-session", "-d", "-s", &name, "-c", cwd, agent])
        .ok_or_else(|| format!("failed to launch {agent} in {cwd}"))?;
    Ok(name)
}
