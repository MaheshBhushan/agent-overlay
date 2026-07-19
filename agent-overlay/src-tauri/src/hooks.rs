//! Push-based status via agent lifecycle hooks.
//!
//! A tiny HTTP listener on 127.0.0.1:8377 accepts POST /event with a JSON
//! body like `{"status":"running","pane":"%3","cwd":"/home/me/proj"}`.
//! Agent CLIs that support hooks (e.g. Claude Code's UserPromptSubmit /
//! PreToolUse / Stop / Notification hooks) curl an event on each transition,
//! giving exact, instant status. Events are stored per pane (and per cwd as
//! a fallback for non-tmux sessions) and override the scraped status while
//! fresh; scraping remains the source of truth for agents without hooks.

use serde::Deserialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const PORT: u16 = 8377;

/// running/idle events override scraping for this long; after that the
/// scraper takes back over (covers missed hooks / killed agents).
const EVENT_TTL_SECS: u64 = 120;
/// A permission request stays sticky longer — the user may take a while to
/// answer and no further hook fires until they do. Any newer event
/// (e.g. PreToolUse after approval) replaces it immediately.
const PERMISSION_TTL_SECS: u64 = 1800;

#[derive(Deserialize)]
struct HookEvent {
    /// "running" | "idle" | "permission"
    status: String,
    pane: Option<String>,
    cwd: Option<String>,
}

struct Entry {
    status: String,
    at: Instant,
}

static STATE: Mutex<Option<HashMap<String, Entry>>> = Mutex::new(None);

fn record(status: &str, pane: Option<&str>, cwd: Option<&str>) {
    if !matches!(status, "running" | "idle" | "permission") {
        return;
    }
    let mut guard = STATE.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    let now = Instant::now();
    for key in [
        pane.filter(|p| !p.is_empty()).map(str::to_string),
        cwd.filter(|c| !c.is_empty()).map(|c| format!("cwd:{c}")),
    ]
    .into_iter()
    .flatten()
    {
        map.insert(
            key,
            Entry {
                status: status.to_string(),
                at: now,
            },
        );
    }
}

/// Fresh hook-reported status for a session, if any. Pane id wins over cwd.
pub fn override_for(pane_id: &str, cwd: &str) -> Option<String> {
    let guard = STATE.lock().unwrap();
    let map = guard.as_ref()?;
    let now = Instant::now();
    for key in [pane_id.to_string(), format!("cwd:{cwd}")] {
        if let Some(e) = map.get(&key) {
            let ttl = if e.status == "permission" {
                PERMISSION_TTL_SECS
            } else {
                EVENT_TTL_SECS
            };
            if now.duration_since(e.at).as_secs() < ttl {
                return Some(e.status.clone());
            }
        }
    }
    None
}

/// Minimal HTTP request handling: enough for `curl -X POST -d '{...}'`.
fn handle(stream: TcpStream) -> Option<()> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .ok()?;
    let mut reader = BufReader::new(stream);
    let mut content_length = 0usize;
    let mut line = String::new();
    reader.read_line(&mut line).ok()?; // request line
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim();
        if header.is_empty() {
            break;
        }
        if let Some((k, v)) = header.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; content_length.min(64 * 1024)];
    reader.read_exact(&mut body).ok()?;
    if let Ok(ev) = serde_json::from_slice::<HookEvent>(&body) {
        record(&ev.status, ev.pane.as_deref(), ev.cwd.as_deref());
    }
    let mut stream = reader.into_inner();
    let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_override_by_pane_and_cwd() {
        record("permission", Some("%9"), Some("/tmp/proj"));
        assert_eq!(override_for("%9", "/other").as_deref(), Some("permission"));
        assert_eq!(
            override_for("pid:123", "/tmp/proj").as_deref(),
            Some("permission")
        );
        assert_eq!(override_for("%404", "/nowhere"), None);

        // A newer event replaces the sticky permission state.
        record("running", Some("%9"), Some("/tmp/proj"));
        assert_eq!(override_for("%9", "/tmp/proj").as_deref(), Some("running"));
    }

    #[test]
    fn unknown_status_ignored() {
        record("exploded", Some("%8"), None);
        assert_eq!(override_for("%8", ""), None);
    }

    #[test]
    fn http_post_reaches_state() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle(stream);
        });
        let body = r#"{"status":"idle","pane":"%77","cwd":"/tmp/x"}"#;
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            c,
            "POST /event HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
        let mut resp = String::new();
        c.read_to_string(&mut resp).unwrap();
        t.join().unwrap();
        assert!(resp.starts_with("HTTP/1.1 204"));
        assert_eq!(override_for("%77", "/tmp/x").as_deref(), Some("idle"));
    }
}

/// Start the listener thread. Errors are logged, never fatal: without the
/// listener the overlay simply falls back to tmux scraping alone.
pub fn serve() {
    std::thread::spawn(|| {
        let listener = match TcpListener::bind(("127.0.0.1", PORT)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("hook listener failed to bind 127.0.0.1:{PORT}: {e}");
                return;
            }
        };
        for stream in listener.incoming().flatten() {
            let _ = handle(stream);
        }
    });
}
