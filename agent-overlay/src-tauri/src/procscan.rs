//! Process-table discovery: find coding agents running outside tmux
//! (plain terminal windows, IDE terminals, headless invocations) and
//! classify them as running/idle from CPU-time deltas between polls.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Instant;

use crate::claude_activity;
use crate::tmux::{agent_from_args, process_table, AgentSession};

/// CPU jiffies per poll that count as an "active" sample. Measured: idle
/// agent TUIs burn 0–1 jiffies/s (cursor blink), actively-executing claude
/// ~38 jiffies/s. A focus/repaint burst can exceed this for one poll but
/// not for RUNNING_STREAK consecutive polls.
const ACTIVE_JIFFIES: u64 = 8;
/// Consecutive active polls required before a session counts as running.
const RUNNING_STREAK: u32 = 2;
/// Seconds without an active sample before a running session goes idle
/// (bridges brief CPU dips during long tool executions).
const IDLE_AFTER_SECS: u64 = 10;
/// Transcript freshness window for claude sessions (writes can lag well
/// behind the actual work during long tool runs).
const TRANSCRIPT_WINDOW_SECS: u64 = 120;

struct ProcActivity {
    cpu: u64,
    streak: u32,
    last_active: Option<Instant>,
}

static ACTIVITY: Mutex<Option<HashMap<u32, ProcActivity>>> = Mutex::new(None);

// --- Windows process/CPU backend (sysinfo) --------------------------------
//
// On Linux we read /proc directly (see the cfg(not(windows)) helpers below);
// the numbers there are CPU "jiffies" where one jiffy is 10ms of CPU time, so
// ACTIVE_JIFFIES (8) is ~80ms of CPU per ~1s poll. On Windows sysinfo reports
// accumulated CPU time in milliseconds, which we convert to the same
// jiffy unit (ms / 10) so all the streak/threshold semantics carry over
// unchanged.
#[cfg(windows)]
mod win {
    use std::sync::Mutex;
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

    /// Shared System kept across polls so CPU accumulation is meaningful and
    /// process CPU deltas line up with our per-poll cadence.
    static SYSTEM: Mutex<Option<System>> = Mutex::new(None);

    /// Refresh the shared System's process list (pid, parent, cmd, cwd, cpu).
    fn with_system<R>(f: impl FnOnce(&System) -> R) -> R {
        let mut guard = SYSTEM.lock().unwrap();
        let sys = guard.get_or_insert_with(System::new);
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_cmd(sysinfo::UpdateKind::Always)
                .with_cwd(sysinfo::UpdateKind::Always),
        );
        f(sys)
    }

    /// pid -> (ppid, joined command line), matching the Linux `ps` table shape.
    pub fn process_table() -> std::collections::HashMap<u32, (u32, String)> {
        with_system(|sys| {
            let mut table = std::collections::HashMap::new();
            for (pid, proc_) in sys.processes() {
                let ppid = proc_.parent().map(|p| p.as_u32()).unwrap_or(0);
                let cmd: String = if proc_.cmd().is_empty() {
                    proc_
                        .exe()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default()
                } else {
                    proc_
                        .cmd()
                        .iter()
                        .map(|s| s.to_string_lossy())
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                table.insert(pid.as_u32(), (ppid, cmd));
            }
            table
        })
    }

    /// Accumulated CPU time for a pid, expressed in 10ms "jiffies" so the
    /// same ACTIVE_JIFFIES threshold used on Linux applies unchanged.
    pub fn cpu_jiffies(pid: u32) -> Option<u64> {
        with_system(|sys| {
            sys.process(sysinfo::Pid::from_u32(pid))
                .map(|p| p.accumulated_cpu_time() / 10)
        })
    }

    pub fn cwd(pid: u32) -> String {
        with_system(|sys| {
            sys.process(sysinfo::Pid::from_u32(pid))
                .and_then(|p| p.cwd())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "?".to_string())
        })
    }

    pub fn kill(pid: u32) -> bool {
        with_system(|sys| {
            sys.process(sysinfo::Pid::from_u32(pid))
                .map(|p| p.kill())
                .unwrap_or(false)
        })
    }
}

/// Windows: expose the sysinfo process table to tmux.rs's process_table().
#[cfg(windows)]
pub fn process_table_sysinfo() -> HashMap<u32, (u32, String)> {
    win::process_table()
}

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
        let cwd = read_cwd(*pid);

        let cpu = read_cpu_jiffies(*pid).unwrap_or(0);
        let entry = activity.entry(*pid).or_insert(ProcActivity {
            cpu,
            streak: 0,
            last_active: None, // start as idle until activity is proven
        });
        let mut active_sample = cpu.saturating_sub(entry.cpu) >= ACTIVE_JIFFIES;
        // A repaint burst (focus, typing) can spike CPU; real work also keeps
        // the session transcript fresh. Require both for claude.
        if active_sample && agent == "claude" {
            active_sample = claude_activity::active_within(&cwd, TRANSCRIPT_WINDOW_SECS);
        }
        entry.cpu = cpu;
        entry.streak = if active_sample { entry.streak + 1 } else { 0 };
        if entry.streak >= RUNNING_STREAK {
            entry.last_active = Some(now);
        }

        let since_active = entry
            .last_active
            .map(|t| now.duration_since(t).as_secs());
        let (status, idle_secs) = match since_active {
            Some(s) if s < IDLE_AFTER_SECS => ("running".to_string(), None),
            Some(s) => ("idle".to_string(), Some(s)),
            None => ("idle".to_string(), None),
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

/// Working directory of a process, or "?" if unavailable.
#[cfg(not(windows))]
fn read_cwd(pid: u32) -> String {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "?".to_string())
}

#[cfg(windows)]
fn read_cwd(pid: u32) -> String {
    win::cwd(pid)
}

/// utime + stime from /proc/<pid>/stat, in CPU jiffies (10ms each). The comm
/// field can contain spaces and parens, so split on the *last* ')' before
/// indexing fields.
#[cfg(not(windows))]
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

#[cfg(windows)]
fn read_cpu_jiffies(pid: u32) -> Option<u64> {
    win::cpu_jiffies(pid)
}

/// Terminate a non-tmux agent session by pid.
pub fn kill(pid_handle: &str) -> Result<(), String> {
    let pid: u32 = pid_handle
        .strip_prefix("pid:")
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| format!("bad pid handle {pid_handle}"))?;
    if kill_pid(pid) {
        Ok(())
    } else {
        Err(format!("failed to signal pid {pid}"))
    }
}

/// SIGTERM the pid on Unix; TerminateProcess (via sysinfo) on Windows.
#[cfg(not(windows))]
fn kill_pid(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn kill_pid(pid: u32) -> bool {
    win::kill(pid)
}
