//! Heuristic parsing of agent CLI pane output into a coarse status.
//!
//! This is intentionally tolerant: agent TUIs differ and change between
//! versions, so we look for broad shapes (spinner/interrupt markers, error
//! text) rather than exact strings. Marker detection is combined with
//! output-activity tracking in `tmux.rs` — a pane whose output is still
//! changing is running even if we don't recognize its spinner.

pub struct Parsed {
    /// "running" | "idle" | "error" — activity tracking may upgrade idle→running.
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
        if line.contains("error:") || line.contains("panic") || line.contains("traceback") {
            return "error".to_string();
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
    fn error_output_is_error() {
        let text = "error: failed to compile\n> ";
        assert_eq!(parse(text).status, "error");
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
