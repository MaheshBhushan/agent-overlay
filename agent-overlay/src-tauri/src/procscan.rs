//! Process-table discovery: find coding agents running outside tmux
//! (plain terminal windows, IDE terminals, headless invocations) and
//! classify them as running/idle from CPU-time deltas between polls.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Instant;

use crate::tmux::{agent_from_args, process_table, AgentSession};

/// CPU jiffies a process must burn between two polls to count as running
/// (10 jiffies ≈ 0.1 s of CPU per ~1 s poll; an idle TUI stays well below).
const RUNNING_JIFFIES: u64 = 10;

struct ProcActivity {
    cpu: u64,
    last_active: Instant,
}

static ACTIVITY: Mutex<Option<HashMap<u32, ProcActivity>>> = Mutex::new(None);

/// All descendants (inclusive) of the given root pids.
fn descendants(roots: &[u32], table: &HashMap<u32, (u32, String)>) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, (ppid, _)) in table {
        children.entry(*ppid).or_default().push(*pid);
    }
    let mut out: HashSet<u32> = HashSet::new();
    let mut stack: Vec<u32> = roots.to_vec();
    while let Some(pid) = stack.pop() {
        if !out.insert(pid) {
            continue;
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids);
        }
    }
    out
}

/// Discover agent processes not rooted in any tmux pane.
pub fn discover(tmux_pane_pids: &[u32]) -> Vec<AgentSession> {
    let table = process_table();
    let excluded = descendants(tmux_pane_pids, &table);

    // Candidate pids whose argv names an agent CLI.
    let mut matched: HashMap<u32, String> = HashMap::new();
    for (pid, (_, args)) in &table {
        if excluded.contains(pid) {
            continue;
        }
        if let Some(agent) = agent_from_args(args) {
            matched.insert(*pid, agent);
        }
    }

    // Drop candidates that have a matched ancestor (helper/child processes of
    // the same session), keeping only the top-most process per session.
    let tops: Vec<u32> = matched
        .keys()
        .copied()
        .filter(|pid| {
            let mut cur = *pid;
            let mut depth = 0;
            while let Some((ppid, _)) = table.get(&cur) {
                if depth > 64 || *ppid <= 1 {
                    break;
                }
                if matched.contains_key(ppid) {
                    return false;
                }
                cur = *ppid;
                depth += 1;
            }
            true
        })
        .collect();

    let mut guard = ACTIVITY.lock().unwrap();
    let activity = guard.get_or_insert_with(HashMap::new);
    let now = Instant::now();
    let mut sessions = Vec::new();

    for pid in &tops {
        let agent = matched[pid].clone();
        let cwd = std::fs::read_link(format!("/proc/{pid}/cwd"))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "?".to_string());

        let cpu = read_cpu_jiffies(*pid).unwrap_or(0);
        let entry = activity.entry(*pid).or_insert(ProcActivity {
            cpu,
            last_active: now,
        });
        if cpu.saturating_sub(entry.cpu) >= RUNNING_JIFFIES {
            entry.last_active = now;
        }
        entry.cpu = cpu;
        let since_active = now.duration_since(entry.last_active).as_secs();

        let (status, idle_secs) = if since_active < 5 {
            ("running".to_string(), None)
        } else {
            ("idle".to_string(), Some(since_active))
        };

        sessions.push(AgentSession {
            pane_id: format!("pid:{pid}"),
            session_name: "terminal".to_string(),
            window_index: "-".to_string(),
            agent,
            cwd,
            status,
            idle_secs,
            tail: Vec::new(),
        });
    }

    activity.retain(|pid, _| tops.contains(pid));
    sessions.sort_by(|a, b| a.cwd.cmp(&b.cwd));
    sessions
}

/// utime + stime from /proc/<pid>/stat. The comm field can contain spaces and
/// parens, so split on the *last* ')' before indexing fields.
fn read_cpu_jiffies(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // rest starts at the state field: utime and stime are fields 12 and 13
    // (0-based) counting from state.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

/// Terminate a non-tmux agent session by pid (SIGTERM).
pub fn kill(pid_handle: &str) -> Result<(), String> {
    let pid: u32 = pid_handle
        .strip_prefix("pid:")
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| format!("bad pid handle {pid_handle}"))?;
    let ok = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err(format!("failed to signal pid {pid}"))
    }
}
