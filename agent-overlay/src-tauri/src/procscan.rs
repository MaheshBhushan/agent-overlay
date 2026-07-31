//! Process-table discovery: find coding agents running outside tmux
//! (plain terminal windows, IDE terminals, headless invocations) and
//! classify them as running/idle from CPU-time deltas between polls.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

/// The string a process is matched against. Reading a Windows process's command
/// line needs a PEB read, which fails for anything we can't open with more than
/// PROCESS_QUERY_LIMITED_INFORMATION — that's how a second opencode.exe in its
/// own terminal ends up with an empty command line and gets dropped. The
/// process name comes from the system process table instead, needs no handle,
/// and is always populated, so it is the fallback. `agent_from_args` normalizes
/// away the `.exe`, so a bare "opencode.exe" still resolves to opencode.
///
/// There is deliberately no executable-path rung: `exe()` is only populated
/// when the refresh asks for it, which `with_system` below does not, and
/// requesting it would not help here — it needs the same process handle the
/// command-line read already failed to get.
///
/// Lives outside the `win` module so it is compiled and tested on every
/// platform; only its caller is Windows-specific.
#[cfg_attr(not(windows), allow(dead_code))]
fn match_string(cmd: &str, name: &str) -> String {
    for candidate in [cmd, name] {
        if !candidate.trim().is_empty() {
            return candidate.trim().to_string();
        }
    }
    String::new()
}

/// Shortest gap between two process-table refreshes. Comfortably under the 1s
/// poll cadence, so every poll still gets fresh data, but well above sysinfo's
/// 200ms minimum CPU update interval, and long enough that the several reads
/// within one poll share a single snapshot.
#[cfg_attr(not(windows), allow(dead_code))]
const MIN_REFRESH: Duration = Duration::from_millis(500);
/// cpu_usage() percentage above which a sample counts as activity.
#[cfg_attr(not(windows), allow(dead_code))]
const ACTIVE_USAGE_PCT: f32 = 5.0;

/// Decides when the shared process snapshot is rebuilt, and labels each
/// snapshot with a generation number.
///
/// sysinfo derives cpu_usage from the CPU time accumulated since the *previous*
/// refresh, so refreshing immediately before every read measures over a window
/// microseconds wide and reports ~0%. One poll reads the table, then a cwd and
/// a CPU sample per session, so refreshing on each of those is what made busy
/// agents look idle. Gating on elapsed time keeps the measurement window the
/// full poll interval and gives every reader in a poll the same snapshot.
///
/// The generation exists because sharing a snapshot makes reads no longer
/// equivalent to measurements: `discover` runs on the 1s poll *and* on window
/// focus, refresh and startup, so two runs can land inside one refresh window
/// and read the same cpu_usage figure twice. Counting that as two samples would
/// let a single busy poll satisfy RUNNING_STREAK.
///
/// Lives outside the `win` module so it is compiled and tested on every
/// platform; only its callers are Windows-specific.
#[cfg_attr(not(windows), allow(dead_code))]
struct RefreshGate {
    last: Option<Instant>,
    generation: u64,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl RefreshGate {
    const fn new() -> Self {
        Self {
            last: None,
            generation: 0,
        }
    }

    /// Returns whether the caller must refresh before reading, and the
    /// generation of the snapshot it will then be reading.
    fn begin(&mut self, now: Instant) -> (bool, u64) {
        let stale = match self.last {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= MIN_REFRESH,
        };
        if stale {
            self.last = Some(now);
            self.generation += 1;
        }
        (stale, self.generation)
    }
}

/// Fold one CPU reading into a session's activity counter, at most once per
/// snapshot. `counted` is the generation this pid was last credited for; a
/// snapshot reused by a second reader is the same measurement, not new
/// evidence, so it must not move the counter again.
#[cfg_attr(not(windows), allow(dead_code))]
fn accumulate_sample(counter: u64, counted: u64, generation: u64, usage: f32) -> (u64, u64) {
    if counted == generation {
        return (counter, counted);
    }
    let counter = if usage > ACTIVE_USAGE_PCT {
        counter + ACTIVE_JIFFIES
    } else {
        counter
    };
    (counter, generation)
}

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
    /// process CPU deltas line up with our per-poll cadence, alongside the gate
    /// deciding when to rebuild it. See `RefreshGate`.
    static SYSTEM: Mutex<(Option<System>, super::RefreshGate)> =
        Mutex::new((None, super::RefreshGate::new()));

    /// Read from the shared process snapshot, rebuilding it first if it has
    /// aged out. The closure also receives the snapshot's generation, so a
    /// caller that must not count one measurement twice can tell reads apart.
    fn with_system<R>(f: impl FnOnce(&System, u64) -> R) -> R {
        let mut guard = SYSTEM.lock().unwrap();
        let (stale, generation) = guard.1.begin(std::time::Instant::now());
        if stale || guard.0.is_none() {
            let sys = guard.0.get_or_insert_with(System::new);
            sys.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing()
                    .with_cpu()
                    .with_cmd(sysinfo::UpdateKind::Always)
                    .with_cwd(sysinfo::UpdateKind::Always),
            );
        }
        let sys = guard.0.as_ref().expect("snapshot initialized above");
        f(sys, generation)
    }

    /// A private, freshly-built process table for the kill path.
    ///
    /// Deliberately not the shared snapshot. Killing a terminal's subtree has
    /// to see processes spawned since the last poll, or a tool child started a
    /// moment ago is never enumerated and outlives its terminal. Refreshing the
    /// shared snapshot instead would reset the CPU measurement window that
    /// `discover` depends on, reintroducing the very bug this change fixes —
    /// so the kill path keeps its own. No CPU data is requested: it is not
    /// needed here, and asking for it is what makes a refresh expensive.
    fn kill_path_system() -> System {
        let mut sys = System::new();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::Always),
        );
        sys
    }

    /// pid -> (ppid, joined command line), matching the Linux `ps` table shape.
    pub fn process_table() -> std::collections::HashMap<u32, (u32, String)> {
        with_system(|sys, _| {
            let mut table = std::collections::HashMap::new();
            for (pid, proc_) in sys.processes() {
                let ppid = proc_.parent().map(|p| p.as_u32()).unwrap_or(0);
                let cmd = proc_
                    .cmd()
                    .iter()
                    .map(|s| s.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ");
                let name = proc_.name().to_string_lossy().into_owned();
                table.insert(pid.as_u32(), (ppid, super::match_string(&cmd, &name)));
            }
            table
        })
    }

    /// Synthetic jiffies for Windows: sysinfo 0.33 has no accumulated_cpu_time,
    /// so we use cpu_usage() (0–100 %) as an activity signal. Each poll where
    /// usage > 5 % we add ACTIVE_JIFFIES to a per-pid counter so the outer
    /// delta-based streak logic behaves identically to Linux.
    pub fn cpu_jiffies(pid: u32) -> Option<u64> {
        let (usage, generation) = with_system(|sys, generation| {
            (
                sys.process(sysinfo::Pid::from_u32(pid))
                    .map(|p| p.cpu_usage()),
                generation,
            )
        });
        let usage = usage?;
        // counter, plus the snapshot generation it was last credited for.
        static ACCUM: std::sync::Mutex<Option<std::collections::HashMap<u32, (u64, u64)>>> =
            std::sync::Mutex::new(None);
        let mut guard = ACCUM.lock().unwrap();
        let map = guard.get_or_insert_with(std::collections::HashMap::new);
        let entry = map.entry(pid).or_insert((0, 0));
        *entry = super::accumulate_sample(entry.0, entry.1, generation, usage);
        Some(entry.0)
    }

    pub fn cwd(pid: u32) -> String {
        with_system(|sys, _| {
            sys.process(sysinfo::Pid::from_u32(pid))
                .and_then(|p| p.cwd())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "?".to_string())
        })
    }

    /// Is the process still present? Called immediately after a kill to decide
    /// whether it worked, so it must not answer from a snapshot taken before
    /// the kill — that would report a false failure for a successful kill.
    pub fn alive(pid: u32) -> bool {
        kill_path_system()
            .process(sysinfo::Pid::from_u32(pid))
            .is_some()
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
        let sys = kill_path_system();
        let mut parent: HashMap<u32, u32> = HashMap::new();
        let mut name: HashMap<u32, String> = HashMap::new();
        for (pid, proc_) in sys.processes() {
            let id = pid.as_u32();
            parent.insert(id, proc_.parent().map(|x| x.as_u32()).unwrap_or(0));
            name.insert(id, proc_.name().to_string_lossy().to_ascii_lowercase());
        }

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
            if let Some(p) = sys.process(sysinfo::Pid::from_u32(pid)) {
                p.kill();
            }
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
        // TerminateProcess is asynchronous: the process stays in the table
        // until its threads reap, so a single immediate check reports a false
        // failure for a kill that did work. Same settle loop as the Linux
        // branch above.
        for _ in 0..20 {
            if !win::alive(pid) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_wins_over_the_name() {
        assert_eq!(
            match_string("node C:\\opencode\\bin.js", "node.exe"),
            "node C:\\opencode\\bin.js"
        );
    }

    /// The reported case: Windows hands back a process whose command line
    /// cannot be read, and dropping it loses a real session.
    #[test]
    fn process_name_is_used_when_the_command_line_is_unreadable() {
        assert_eq!(match_string("", "opencode.exe"), "opencode.exe");
    }

    /// sysinfo reports an unreadable command line as an empty argv, which joins
    /// to "" — but a single empty argument joins to "" as well, so the check has
    /// to be on the joined string, not on the slice.
    #[test]
    fn blank_command_line_falls_through() {
        assert_eq!(match_string("   ", "opencode.exe"), "opencode.exe");
        assert_eq!(match_string("", ""), "");
        assert_eq!(agent_from_args(&match_string("", "")), None);
    }

    /// The point of the fallback: a name-only process must still resolve to its
    /// agent through the normal matching path.
    #[test]
    fn name_only_process_still_resolves_to_its_agent() {
        for (name, expected) in [
            ("opencode.exe", "opencode"),
            ("claude.exe", "claude"),
            ("opencode", "opencode"),
        ] {
            assert_eq!(
                agent_from_args(&match_string("", name)).as_deref(),
                Some(expected),
                "name {name} should resolve to {expected}"
            );
        }
    }

    /// The risk this change introduces: every process whose command line cannot
    /// be read now contributes its name instead of an empty string, so the whole
    /// protected/elevated slice of the Windows process table is newly evaluated
    /// against the agent list. None of it may match — a false positive would put
    /// a junk card on the overlay whose ✕ kills a system subtree.
    #[test]
    fn common_windows_process_names_are_not_agents() {
        for name in [
            "System",
            "Registry",
            "smss.exe",
            "csrss.exe",
            "wininit.exe",
            "services.exe",
            "lsass.exe",
            "svchost.exe",
            "winlogon.exe",
            "explorer.exe",
            "dwm.exe",
            "MsMpEng.exe",
            "audiodg.exe",
            "fontdrvhost.exe",
            "spoolsv.exe",
            "RuntimeBroker.exe",
            "SearchIndexer.exe",
            "conhost.exe",
            "WindowsTerminal.exe",
            "powershell.exe",
            "pwsh.exe",
            "cmd.exe",
            "node.exe",
            "python.exe",
            "python3.exe",
            "pip.exe",
            "git.exe",
            "code.exe",
            "chrome.exe",
            "msedge.exe",
        ] {
            assert_eq!(
                agent_from_args(&match_string("", name)),
                None,
                "{name} must not be reported as an agent session"
            );
        }
    }

    /// Two independent agent processes stay two sessions: `descendants` only
    /// excludes tmux-rooted trees, and neither is an ancestor of the other.
    #[test]
    fn two_name_only_agents_remain_two_sessions() {
        let table: HashMap<u32, (u32, String)> = HashMap::from([
            (4, (0, "System".to_string())),
            (100, (4, match_string("", "WindowsTerminal.exe"))),
            (101, (100, match_string("", "opencode.exe"))),
            (200, (4, match_string("", "WindowsTerminal.exe"))),
            (201, (200, match_string("", "opencode.exe"))),
        ]);

        let matched: HashMap<u32, String> = table
            .iter()
            .filter_map(|(pid, (_, args))| agent_from_args(args).map(|a| (*pid, a)))
            .collect();
        assert_eq!(matched.len(), 2, "both opencode processes must match");

        // Mirror discover()'s top-most filter: neither agent may be shadowed by
        // a matched ancestor, or one of the two sessions disappears.
        let tops: Vec<u32> = matched
            .keys()
            .copied()
            .filter(|pid| {
                let mut cur = *pid;
                while let Some((ppid, _)) = table.get(&cur) {
                    if *ppid <= 1 {
                        break;
                    }
                    if matched.contains_key(ppid) {
                        return false;
                    }
                    cur = *ppid;
                }
                true
            })
            .collect();
        assert_eq!(tops.len(), 2, "neither session may shadow the other");
    }

    /// The terminal hosting an agent must not itself match, or it would shadow
    /// the agent and re-key the session to a pid with no cwd and no status file.
    #[test]
    fn host_terminal_names_do_not_shadow_their_agent() {
        for host in ["WindowsTerminal.exe", "conhost.exe", "cmd.exe", "pwsh.exe"] {
            assert_eq!(
                agent_from_args(&match_string("", host)),
                None,
                "{host} must not match, or it would shadow the agent beneath it"
            );
        }
    }

    /// The poll cadence in lib.rs. Kept here so the gate is checked against the
    /// interval it actually has to fit inside.
    const POLL_INTERVAL: Duration = Duration::from_secs(1);

    /// The bug: one `discover` reads the table, then a cwd and a CPU sample per
    /// session. Under the old code each of those refreshed, so every CPU read
    /// measured over a window microseconds wide and reported ~0%. All the reads
    /// in one poll must now come from a single refresh.
    #[test]
    fn one_poll_refreshes_once_however_many_reads() {
        let mut gate = RefreshGate::new();
        let t0 = Instant::now();
        let reads = [0, 0, 1, 2, 7, 30, 120, 499];
        let refreshes = reads
            .iter()
            .filter(|ms| gate.begin(t0 + Duration::from_millis(**ms)).0)
            .count();
        assert_eq!(refreshes, 1, "a single poll must refresh exactly once");
    }

    /// ...and every read in that poll must see the same snapshot, or the
    /// double-count guard cannot tell them apart.
    #[test]
    fn reads_within_one_poll_share_a_generation() {
        let mut gate = RefreshGate::new();
        let t0 = Instant::now();
        let gens: Vec<u64> = [0, 3, 60, 499]
            .iter()
            .map(|ms| gate.begin(t0 + Duration::from_millis(*ms)).1)
            .collect();
        assert_eq!(gens, vec![1, 1, 1, 1]);
    }

    /// The next poll must still get fresh data and a new generation — the gate
    /// cannot be so wide that it starves the cadence.
    #[test]
    fn each_poll_gets_a_new_snapshot() {
        let mut gate = RefreshGate::new();
        let t0 = Instant::now();
        for poll in 0..5u32 {
            let at = t0 + POLL_INTERVAL * poll;
            let (stale, generation) = gate.begin(at);
            assert!(stale, "poll {poll} must refresh");
            assert_eq!(generation, u64::from(poll) + 1);
        }
    }

    #[test]
    fn refresh_gap_fits_inside_the_poll_interval() {
        assert!(
            MIN_REFRESH < POLL_INTERVAL,
            "MIN_REFRESH must stay below the poll cadence or polls get stale data"
        );
    }

    /// A snapshot reused by a second reader is the same measurement, not new
    /// evidence. `discover` also runs on focus/refresh/startup, so without this
    /// one busy poll could be counted twice and satisfy RUNNING_STREAK alone.
    #[test]
    fn a_reused_snapshot_does_not_count_twice() {
        let (counter, counted) = accumulate_sample(0, 0, 1, 90.0);
        assert_eq!(counter, ACTIVE_JIFFIES, "first read of a snapshot counts");
        let (again, _) = accumulate_sample(counter, counted, 1, 90.0);
        assert_eq!(again, counter, "the same snapshot must not count again");
    }

    /// Consecutive busy snapshots still accumulate, or nothing ever reaches
    /// RUNNING_STREAK.
    #[test]
    fn successive_snapshots_accumulate() {
        let (mut counter, mut counted) = (0, 0);
        for generation in 1..=3 {
            (counter, counted) = accumulate_sample(counter, counted, generation, 90.0);
        }
        assert_eq!(counter, ACTIVE_JIFFIES * 3);
    }

    /// An idle process must not accumulate, but must still be marked as having
    /// seen the snapshot — otherwise a later reader of the same snapshot could
    /// credit it.
    #[test]
    fn idle_samples_do_not_accumulate() {
        let (counter, counted) = accumulate_sample(0, 0, 1, 0.0);
        assert_eq!(counter, 0);
        assert_eq!(counted, 1, "the snapshot is still consumed");
        assert_eq!(accumulate_sample(counter, counted, 1, 90.0).0, 0);
    }

    /// The threshold is a strict greater-than, matching the original sampler.
    #[test]
    fn usage_at_the_threshold_is_not_activity() {
        assert_eq!(accumulate_sample(0, 0, 1, ACTIVE_USAGE_PCT).0, 0);
        assert_eq!(
            accumulate_sample(0, 0, 1, ACTIVE_USAGE_PCT + 0.1).0,
            ACTIVE_JIFFIES
        );
    }
}
