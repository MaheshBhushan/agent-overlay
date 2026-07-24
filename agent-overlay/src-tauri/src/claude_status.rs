//! Authoritative Claude Code session status.
//!
//! Claude Code writes ~/.claude/sessions/<pid>.json for every live process,
//! e.g. {"pid":71052,"sessionId":"cbcf17fa-…","cwd":"…","procStart":"6455330",
//!       "status":"idle","statusUpdatedAt":1784830153413,…}. The `status` field
//! ("busy" | "idle") is claude's own view of whether it is currently working.
//!
//! Reading it per-pid is exact, and — unlike a transcript-freshness check keyed
//! on the working directory — it distinguishes two sessions that share one cwd.
//! Two `claude` processes in the same folder write to the same
//! ~/.claude/projects/<slug>/ transcript dir, so a cwd-keyed freshness check
//! reports *both* as active whenever *either* is working; the per-pid status
//! file does not have that blind spot.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct ClaudeStatus {
    /// True when claude reports it is actively processing a turn.
    pub busy: bool,
    /// Seconds since claude last changed this session's status.
    pub since_status_secs: u64,
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Claude's own status for the session owned by `pid`, or None if there is no
/// session file (older claude, non-claude agent) or it belongs to a dead
/// namesake after pid reuse.
pub fn status_for_pid(pid: u32) -> Option<ClaudeStatus> {
    let path = home()?.join(".claude/sessions").join(format!("{pid}.json"));
    let text = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;

    // Guard against pid reuse: the recorded process start-time must match the
    // live process, or this file is a stale leftover from a dead namesake.
    if let (Some(rec), Some(live)) = (
        v.get("procStart").and_then(|s| s.as_str()),
        proc_start(pid),
    ) {
        if rec != live {
            return None;
        }
    }

    let busy = v.get("status").and_then(|s| s.as_str()) == Some("busy");
    let since_status_secs = v
        .get("statusUpdatedAt")
        .and_then(|t| t.as_u64())
        .map(|t| (now_ms().saturating_sub(t as u128) / 1000) as u64)
        .unwrap_or(0);
    Some(ClaudeStatus {
        busy,
        since_status_secs,
    })
}

/// The process start-time in clock jiffies (field 22 of /proc/<pid>/stat),
/// matching the `procStart` string claude records. Used only as a pid-reuse
/// guard, so returning None (no guard) on platforms without /proc is fine.
#[cfg(not(windows))]
fn proc_start(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm may contain ')' and spaces, so start after the last ')': the first
    // remaining field is `state`, and starttime is field 22 (1-based) overall,
    // i.e. index 19 counting from state.
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(19).map(|s| s.to_string())
}

#[cfg(windows)]
fn proc_start(_pid: u32) -> Option<String> {
    None
}
