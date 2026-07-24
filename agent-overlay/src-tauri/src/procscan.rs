//! Process-table discovery: find coding agents running outside tmux
//! (plain terminal windows, IDE terminals, headless invocations) and
//! classify them as running/idle from CPU-time deltas between polls.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Instant;

use crate::claude_status;
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

    /// Synthetic jiffies for Windows: sysinfo 0.33 has no accumulated_cpu_time,
    /// so we use cpu_usage() (0–100 %) as an activity signal. Each poll where
    /// usage > 5 % we add ACTIVE_JIFFIES to a per-pid counter so the outer
    /// delta-based streak logic behaves identically to Linux.
    pub fn cpu_jiffies(pid: u32) -> Option<u64> {
        let usage = with_system(|sys| {
            sys.process(sysinfo::Pid::from_u32(pid))
                .map(|p| p.cpu_usage())
        })?;
        static ACCUM: std::sync::Mutex<Option<std::collections::HashMap<u32, u64>>> =
            std::sync::Mutex::new(None);
        let mut guard = ACCUM.lock().unwrap();
        let map = guard.get_or_insert_with(std::collections::HashMap::new);
        let counter = map.entry(pid).or_insert(0);
        if usage > 5.0 {
            *counter += 8; // ACTIVE_JIFFIES equivalent
        }
        Some(*counter)
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

    /// Is the process still present?
    pub fn alive(pid: u32) -> bool {
        with_system(|sys| sys.process(sysinfo::Pid::from_u32(pid)).is_some())
    }

    /// Close the terminal *tab* hosting an agent, the way `exit` would: walk UP
    /// from the agent to the shell (the child of the console host), then kill
    /// that whole subtree. Killing the shell is what makes the console host
    /// (Windows Terminal / conhost) close the tab — killing only the agent's
    /// descendants leaves the shell alive and the window open.
    ///
    /// The console-host names below are the *stable* Windows session roots (not
    /// an open-ended list of terminals like the Linux side avoids); we stop the
    /// upward walk when the parent is one of them, so the shell is the last node
    /// before the host.
    pub fn close_agent(agent: u32) {
        use std::collections::HashMap;
        let (parent, name): (HashMap<u32, u32>, HashMap<u32, String>) = with_system(|sys| {
            let mut p = HashMap::new();
            let mut n = HashMap::new();
            for (pid, proc_) in sys.processes() {
                let id = pid.as_u32();
                p.insert(id, proc_.parent().map(|x| x.as_u32()).unwrap_or(0));
                n.insert(id, proc_.name().to_string_lossy().to_ascii_lowercase());
            }
            (p, n)
        });

        let is_host = |nm: &str| {
            matches!(
                nm,
                "windowsterminal.exe" | "conhost.exe" | "openconsole.exe" | "explorer.exe"
                    | "services.exe" | "svchost.exe" | "wininit.exe" | "userinit.exe"
                    | "winlogon.exe"
            )
        };

        // Walk up to the shell = the highest ancestor still below a console host.
        let mut node = agent;
        for _ in 0..64 {
            let par = *parent.get(&node).unwrap_or(&0);
            if par == 0 {
                break;
            }
            let pname = name.get(&par).map(String::as_str).unwrap_or("");
            if is_host(pname) {
                break;
            }
            node = par;
        }

        // Kill node's whole subtree (shell + agent + everything under it).
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (pid, ppid) in &parent {
            children.entry(*ppid).or_default().push(*pid);
        }
        let mut order = Vec::new();
        let mut stack = vec![node];
        while let Some(p) = stack.pop() {
            order.push(p);
            if let Some(kids) = children.get(&p) {
                stack.extend(kids);
            }
        }
        for pid in order.into_iter().rev() {
            kill(pid);
        }
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
        let active_sample = cpu.saturating_sub(entry.cpu) >= ACTIVE_JIFFIES;
        entry.cpu = cpu;
        entry.streak = if active_sample { entry.streak + 1 } else { 0 };
        if entry.streak >= RUNNING_STREAK {
            entry.last_active = Some(now);
        }

        let since_active = entry
            .last_active
            .map(|t| now.duration_since(t).as_secs());
        // Prefer claude's own per-pid status: it is exact and, unlike the CPU
        // heuristic, never confuses two claude sessions that share one cwd
        // (a focus/typing repaint in the idle one no longer reads as running
        // just because its sibling is working). Fall back to the CPU streak
        // for non-claude agents and pre-status claude builds.
        let (status, idle_secs) = match (agent == "claude")
            .then(|| claude_status::status_for_pid(*pid))
            .flatten()
        {
            Some(cs) if cs.busy => ("running".to_string(), None),
            Some(cs) => ("idle".to_string(), Some(cs.since_status_secs)),
            None => match since_active {
                Some(s) if s < IDLE_AFTER_SECS => ("running".to_string(), None),
                Some(s) => ("idle".to_string(), Some(s)),
                None => ("idle".to_string(), None),
            },
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
#[cfg(not(windows))]
fn proc_comm(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "?".into())
}

/// Fields we care about from /proc/<pid>/stat.
#[cfg(not(windows))]
struct Stat {
    ppid: u32,
    session: u32,
    tty_nr: i32,
    comm: String,
}

#[cfg(not(windows))]
fn read_stat(pid: u32) -> Option<Stat> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm sits in parens and may itself contain spaces/parens, so span from the
    // first '(' to the last ')'. Remaining fields are whitespace-separated:
    //   [0]=state [1]=ppid [2]=pgrp [3]=session [4]=tty_nr ...
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let comm = s.get(open + 1..close)?.to_string();
    let f: Vec<&str> = s.get(close + 2..)?.split_whitespace().collect();
    Some(Stat {
        ppid: f.get(1)?.parse().ok()?,
        session: f.get(3)?.parse().ok()?,
        tty_nr: f.get(4)?.parse().ok()?,
        comm,
    })
}

/// Find the terminal emulator hosting an agent — structurally, with no list of
/// terminal names. A terminal opens a pty, then forks a child that calls
/// setsid(): that child becomes the *session leader* and gains the pty as its
/// controlling terminal, while the terminal emulator stays in its own, different
/// session. So the emulator is precisely "the parent of the agent's session
/// leader, living in a different session." This holds for konsole, alacritty,
/// kitty, foot, xterm, gnome-terminal, sshd, … — anything that spawns a pty.
///
/// Returns None when there's no controlling terminal (headless / piped) or the
/// candidate is a system process (pid 1, systemd, login), so we never nuke init.
#[cfg(not(windows))]
fn hosting_terminal(agent: u32) -> Option<u32> {
    let a = read_stat(agent)?;
    if a.tty_nr == 0 {
        return None; // no controlling terminal to speak of
    }
    let leader = read_stat(a.session)?; // the session leader (usually the shell)
    let cand = leader.ppid; // its parent = the pty master side = the emulator
    if cand <= 1 {
        return None;
    }
    let c = read_stat(cand)?;
    // The real emulator is in a *different* session than the agent, and isn't a
    // core system process.
    if c.session == a.session {
        return None;
    }
    if cand == std::process::id() || c.comm.starts_with("systemd") || c.comm == "init"
        || c.comm == "login"
    {
        return None;
    }
    Some(cand)
}

pub fn kill(pid_handle: &str) -> Result<(), String> {
    let pid: u32 = pid_handle
        .strip_prefix("pid:")
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| format!("bad pid handle {pid_handle}"))?;

    #[cfg(not(windows))]
    {
        let alive = |p: u32| std::path::Path::new(&format!("/proc/{p}")).exists();
        match hosting_terminal(pid) {
            Some(term) => {
                eprintln!(
                    "[kill] agent pid={pid} -> closing hosting terminal pid={term} comm={}",
                    proc_comm(term)
                );
                kill_group_and_pid(term);
            }
            None => {
                eprintln!("[kill] agent pid={pid}: no distinct terminal, killing agent group only");
            }
        }
        // Always also take down the agent's own group, in case it ignores the
        // pty hangup or was launched without a terminal.
        kill_group_and_pid(pid);

        for _ in 0..20 {
            if !alive(pid) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        return if alive(pid) {
            Err(format!("failed to kill pid {pid}"))
        } else {
            Ok(())
        };
    }

    #[cfg(windows)]
    {
        // Windows has no pty/session-leader structure, so walk UP to the shell
        // and kill its subtree — that's what makes the terminal close the tab,
        // exactly like typing `exit` (killing only the agent leaves the shell
        // alive and the window open).
        win::close_agent(pid);
        if win::alive(pid) {
            Err(format!("failed to kill pid {pid}"))
        } else {
            Ok(())
        }
    }
}

/// SIGKILL a process group and the pid itself, via the raw syscall (no PATH
/// dependency). Safe: kill/getpgid with a pid + signal have no memory effects.
#[cfg(not(windows))]
fn kill_group_and_pid(pid: u32) {
    let p = pid as libc::pid_t;
    unsafe {
        let pgid = libc::getpgid(p);
        if pgid > 0 {
            libc::kill(-pgid, libc::SIGKILL);
        }
        libc::kill(p, libc::SIGKILL);
    }
}
