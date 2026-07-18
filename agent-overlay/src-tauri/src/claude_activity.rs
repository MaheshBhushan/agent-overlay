//! Claude Code transcript freshness: Claude writes session events to
//! ~/.claude/projects/<slugified-cwd>/<session>.jsonl. Those files only
//! change while a prompt is being processed (possibly with pauses during
//! long tool runs), so a recent mtime distinguishes real work from UI
//! repaints caused by focusing or typing.

use std::path::PathBuf;
use std::time::SystemTime;

/// Claude Code's project-dir slug: every non-alphanumeric char becomes '-'.
fn slug(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// True if any transcript for this project dir changed within `secs`.
pub fn active_within(cwd: &str, secs: u64) -> bool {
    let Some(dir) = home().map(|h| h.join(".claude/projects").join(slug(cwd))) else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        // No transcript dir → no information; don't veto other signals.
        return true;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "jsonl") {
            if let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) {
                if now
                    .duration_since(mtime)
                    .map(|d| d.as_secs() < secs)
                    .unwrap_or(true)
                {
                    return true;
                }
            }
        }
    }
    false
}
