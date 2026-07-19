//! Heuristic parsing of agent CLI pane output into a coarse status.
//!
//! This is intentionally tolerant: agent TUIs differ and change between
//! versions, so we look for broad shapes (spinner/interrupt markers, error
//! text) rather than exact strings. Marker detection is combined with
//! output-activity tracking in `tmux.rs` — a pane whose output is still
//! changing is running even if we don't recognize its spinner.

pub struct Parsed {
    /// "running" | "idle" | "permission" — activity tracking may upgrade
    /// idle→running, and hook events (hooks.rs) may override entirely.
    pub status: String,
    pub tail: Vec<String>,
}

const RUNNING_MARKERS: &[&str] = &[
    "esc to interrupt",
    "ctrl+c to interrupt",
    "thinking",
    "pondering",
    "working",
    "running…",
    "✻",
    "✳",
    "✽",
];

/// Approval-prompt shapes across agent TUIs: Claude Code's "Do you want to
/// …? ❯ 1. Yes", aider's "(Y)es/(N)o", gemini's "Apply this change?", plain
/// "[y/n]" prompts.
const PERMISSION_MARKERS: &[&str] = &[
    "do you want",
    "would you like to",
    "1. yes",
    "(y)es",
    "y/n)",
    "[y/n]",
    "(y/n",
    "apply this change",
    "allow this",
    "approve this",
    "grant permission",
    "waiting for approval",
];

pub fn parse(text: &str) -> Parsed {
    let lines: Vec<&str> = text.lines().collect();
    let trimmed_end = lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map(|i| i + 1)
        .unwrap_or(0);
    let lines = &lines[..trimmed_end];
    let tail: Vec<String> = lines
        .iter()
        .rev()
        .take(8)
        .rev()
        .map(|l| l.to_string())
        .collect();

    Parsed {
        status: detect_status(lines),
        tail,
    }
}

fn detect_status(lines: &[&str]) -> String {
    // Approval dialogs span several lines (question + options), so scan a
    // wider tail for them and check first: while a permission dialog is up
    // the agent is blocked even if spinner remnants are still visible above.
    let permission_zone: Vec<String> = lines
        .iter()
        .rev()
        .take(12)
        .map(|l| l.to_lowercase())
        .collect();
    for line in &permission_zone {
        if PERMISSION_MARKERS.iter().any(|m| line.contains(m)) {
            return "permission".to_string();
        }
    }
    let recent: Vec<String> = lines
        .iter()
        .rev()
        .take(6)
        .map(|l| l.to_lowercase())
        .collect();
    for line in &recent {
        if RUNNING_MARKERS.iter().any(|m| line.contains(m)) {
            return "running".to_string();
        }
    }
    "idle".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_means_running() {
        let text = "✻ Thinking… (12s · esc to interrupt)";
        assert_eq!(parse(text).status, "running");
    }

    #[test]
    fn interrupt_hint_means_running() {
        let text = "Bashing… (34s · ctrl+c to interrupt)";
        assert_eq!(parse(text).status, "running");
    }

    #[test]
    fn prompt_box_is_idle() {
        let text = "\
● Done! All tests pass.

╭──────────────────────────╮
│ ❯                        │
╰──────────────────────────╯
";
        assert_eq!(parse(text).status, "idle");
    }

    #[test]
    fn claude_permission_dialog_is_permission() {
        let text = "\
Do you want to make this edit to main.rs?
 ❯ 1. Yes
   2. Yes, allow all edits during this session
   3. No, and tell Claude what to do differently
";
        assert_eq!(parse(text).status, "permission");
    }

    #[test]
    fn yn_prompt_is_permission() {
        let text = "Allow edits to config.py? (Y)es/(N)o [Yes]:";
        assert_eq!(parse(text).status, "permission");
    }

    #[test]
    fn error_output_is_idle() {
        let text = "error: failed to compile\n> ";
        assert_eq!(parse(text).status, "idle");
    }

    #[test]
    fn old_spinner_scrolled_away_is_idle() {
        // Marker lines far above the tail must not count.
        let mut text = String::from("✻ Thinking…\n");
        for _ in 0..10 {
            text.push_str("output line\n");
        }
        assert_eq!(parse(&text).status, "idle");
    }
}
