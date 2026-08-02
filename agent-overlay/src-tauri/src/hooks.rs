//! Push-based status via agent lifecycle hooks.
//!
//! A tiny HTTP listener on 127.0.0.1:8377 accepts POST /event with a JSON
//! body like `{"status":"running","pane":"%3","cwd":"/home/me/proj"}`.
//! Agent CLIs that support hooks (e.g. Claude Code's Notification hook, or
//! opencode's tool/permission events) report an event on each transition,
//! giving exact, instant status. An event is filed against whatever names one
//! session — the tmux pane id, or the reporting hook's ancestor pids, which is
//! how a session outside tmux is named. `cwd` is the last resort, used only
//! when neither is available, because two agents working in one folder is
//! normal and a shared key would put them all in the same state. Events
//! override the scraped status while fresh; scraping remains the source of
//! truth for agents without hooks.
//!
//! The same socket doubles as the single-instance lock. Exactly one overlay can
//! hold 127.0.0.1:8377, so a failed bind means either another overlay is already
//! running — in which case `POST /show` hands the launch over to it — or an
//! unrelated program has the port, in which case we carry on without hooks as
//! before.

use serde::Deserialize;
use serde_json::Value;
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
    /// The reporting hook's ancestor pids. The agent is among them, and
    /// `pid:{p}` is exactly how procscan keys a session outside tmux, so this
    /// lands the event on one session instead of every session in a folder.
    #[serde(default)]
    pids: Vec<u32>,
}

struct Entry {
    status: String,
    at: Instant,
}

static STATE: Mutex<Option<HashMap<String, Entry>>> = Mutex::new(None);

fn record(status: &str, pane: Option<&str>, cwd: Option<&str>, pids: &[u32]) {
    if !matches!(status, "running" | "idle" | "permission") {
        return;
    }
    let pane = pane.filter(|p| !p.is_empty()).map(str::to_string);
    // A pane id or a pid names one session. `cwd` names a folder, and two
    // agents working in one folder are a normal thing to do — so it is the
    // last resort, used only when nothing precise is available. Writing it
    // alongside a precise key is what used to put an idle sibling in the
    // Needs Approval column next to the session actually waiting.
    let precise = pane.is_some() || !pids.is_empty();
    let mut guard = STATE.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    let now = Instant::now();
    for key in pane
        .into_iter()
        .chain(pids.iter().map(|p| format!("pid:{p}")))
        .chain(
            cwd.filter(|c| !c.is_empty() && !precise)
                .map(|c| format!("cwd:{c}")),
        )
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

/// Identifies our own responses, so a second launch can tell an overlay from
/// whatever else might be sitting on the port.
const MARKER: &str = "X-Agent-Overlay";

/// Report a status transition to a running overlay, then exit. This is the
/// `--hook-event` path: agent CLIs invoke the overlay binary itself instead of
/// shelling out to `curl`, which keeps the hook command identical on `sh` and
/// `cmd.exe` (see hookinstall.rs).
///
/// Deliberately silent and always successful from the CLI's point of view — a
/// hook that fails or blocks would disrupt the agent session it is reporting
/// on, and the overlay simply falls back to scanning when no event arrives.
pub fn post_event(status: &str) {
    post_event_to(PORT, status);
}

/// The pids between us and the agent that ran this hook. A command hook runs as
/// `agent -> sh -> agent-overlay`, so the agent's own pid is a couple of steps
/// up, and procscan keys a non-tmux session by exactly that pid.
///
/// Linux only: it is a few small reads under /proc, which a hook can afford.
/// Elsewhere this is empty and the cwd fallback still applies, so nothing
/// breaks — it just stays as imprecise as it is today.
#[cfg(target_os = "linux")]
fn ancestor_pids() -> Vec<u32> {
    let mut out = Vec::new();
    let mut pid = std::process::id();
    // Deep enough to clear the shell and any wrapper, bounded so a cycle or a
    // pid-reuse oddity can never spin a hook.
    for _ in 0..8 {
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            break;
        };
        let Some(ppid) = status
            .lines()
            .find_map(|l| l.strip_prefix("PPid:"))
            .and_then(|v| v.trim().parse::<u32>().ok())
        else {
            break;
        };
        if ppid <= 1 {
            break;
        }
        out.push(ppid);
        pid = ppid;
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn ancestor_pids() -> Vec<u32> {
    Vec::new()
}

fn post_event_to(port: u16, status: &str) {
    let pane = std::env::var("TMUX_PANE").unwrap_or_default();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let body = serde_json::json!({
        "status": status,
        "pane": pane,
        "cwd": cwd,
        "pids": ancestor_pids(),
    })
    .to_string();

    let timeout = Duration::from_secs(2);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut c) = TcpStream::connect_timeout(&addr, timeout) else {
        return; // overlay isn't running
    };
    let _ = c.set_write_timeout(Some(timeout));
    let _ = c.write_all(
        format!(
            "POST /event HTTP/1.1\r\nHost: localhost\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .as_bytes(),
    );
    let _ = c.flush();
}

/// The `--hook-notify` path. Claude Code's `Notification` hook fires for more
/// than approvals (idle nudges too), and the distinguishing detail is only in
/// the JSON payload on stdin — so read it and report a permission request only
/// when that is what it is.
pub fn post_notification(payload: &str) {
    if is_permission_notification(payload) {
        post_event("permission");
    }
}

fn is_permission_notification(payload: &str) -> bool {
    // Match on the message text rather than a fixed schema: the notification
    // wording ("Claude needs your permission to use Bash") is stabler across
    // versions than the envelope around it.
    //
    // The message only, though — the envelope carries a cwd and a transcript
    // path, and a session merely working in a directory called `permissions`
    // must not read as one waiting for approval. Falling back to the whole
    // payload when there's no message keeps an unrecognised schema working:
    // missing a real approval is worse than an occasional false positive.
    let message = serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string));
    message
        .as_deref()
        .unwrap_or(payload)
        .to_ascii_lowercase()
        .contains("permission")
}

/// Minimal HTTP request handling: enough for `curl -X POST -d '{...}'`.
/// `POST /show` is the single-instance handover and invokes `on_show`;
/// anything else is treated as a hook event.
fn handle(stream: TcpStream, on_show: &dyn Fn()) -> Option<()> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .ok()?;
    let mut reader = BufReader::new(stream);
    let mut content_length = 0usize;
    let mut line = String::new();
    reader.read_line(&mut line).ok()?; // request line
    let mut words = line.split_whitespace();
    let method = words.next().unwrap_or("").to_string();
    let path = words.next().unwrap_or("").to_string();
    // Neither endpoint may be driven by a web page: /show manipulates the
    // window, and /event writes the status the user acts on — a forged "idle"
    // hides a waiting agent. A browser cannot suppress Origin/Referer on a
    // cross-origin fetch, and a simple POST is the only shape that gets
    // through without a preflight.
    let mut from_browser = false;
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
            if k.eq_ignore_ascii_case("origin") || k.eq_ignore_ascii_case("referer") {
                from_browser = true;
            }
        }
    }
    let mut body = vec![0u8; content_length.min(64 * 1024)];
    // A client that declares more than it sends must not cost us the response:
    // treat a short read as an empty body and still answer below, rather than
    // dropping the connection after having waited out the read timeout.
    if reader.read_exact(&mut body).is_err() {
        body.clear();
    }

    let show = path == "/show" && method.eq_ignore_ascii_case("POST") && !from_browser;
    if path != "/show" && !from_browser {
        if let Ok(ev) = serde_json::from_slice::<HookEvent>(&body) {
            record(&ev.status, ev.pane.as_deref(), ev.cwd.as_deref(), &ev.pids);
        }
    }
    // Answer before showing the window: the caller is waiting on this response
    // with a short timeout, and raising a window can be slow.
    let mut stream = reader.into_inner();
    let _ = stream.write_all(
        format!("HTTP/1.1 204 No Content\r\n{MARKER}: 1\r\nContent-Length: 0\r\n\r\n").as_bytes(),
    );
    let _ = stream.flush();
    if show {
        on_show();
    }
    Some(())
}

/// Outcome of trying to become the one live overlay.
pub enum Claim {
    /// We own the port: this is the only overlay running.
    Primary(TcpListener),
    /// Another overlay owns it and has been asked to show itself. This launch
    /// should exit without opening a second window.
    Secondary,
    /// Something that isn't an overlay owns the port. Start anyway, without the
    /// hook listener — the same degraded mode as before.
    Foreign(std::io::Error),
}

/// Try to claim the singleton port, handing the launch to a running overlay if
/// there is one.
pub fn claim() -> Claim {
    claim_on(PORT)
}

fn claim_on(port: u16) -> Claim {
    match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => Claim::Primary(l),
        Err(_) if request_show(port) => Claim::Secondary,
        // The holder isn't an overlay, or it quit while we were asking. Retry
        // once before giving up: if it has since exited the port is ours, and
        // starting without the listener when it is free would silently disable
        // push-based status for the whole session.
        Err(_) => match TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => Claim::Primary(l),
            Err(e) => Claim::Foreign(e),
        },
    }
}

/// Ask whoever holds the port to show themselves. True only if something
/// answered with our marker header. An overlay older than this change replies
/// without the marker and is treated as foreign, so during an upgrade the new
/// build will start alongside the old one — once.
fn request_show(port: u16) -> bool {
    let timeout = Duration::from_secs(2);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut c) = TcpStream::connect_timeout(&addr, timeout) else {
        return false;
    };
    let _ = c.set_read_timeout(Some(timeout));
    let _ = c.set_write_timeout(Some(timeout));
    if c.write_all(b"POST /show HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n")
        .is_err()
    {
        return false;
    }
    // Read until the end of the response head; a single read is not guaranteed
    // to return all of it, and a short read would mean a duplicate window.
    let mut resp = Vec::new();
    let mut chunk = [0u8; 128];
    while !resp.windows(4).any(|w| w == b"\r\n\r\n") && resp.len() < 1024 {
        match c.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => resp.extend_from_slice(&chunk[..n]),
            Err(_) => return false,
        }
    }
    let resp = String::from_utf8_lossy(&resp).to_ascii_lowercase();
    resp.starts_with("http/1.1 204") && resp.contains(&MARKER.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_override_by_pane_and_cwd() {
        // No pane and no pids: cwd is all we have, so it is used.
        record("permission", None, Some("/tmp/proj"), &[]);
        assert_eq!(
            override_for("pid:123", "/tmp/proj").as_deref(),
            Some("permission")
        );

        record("permission", Some("%9"), Some("/tmp/other"), &[]);
        assert_eq!(
            override_for("%9", "/nowhere").as_deref(),
            Some("permission")
        );
        assert_eq!(override_for("%404", "/nowhere"), None);

        // A newer event replaces the sticky permission state.
        record("running", Some("%9"), Some("/tmp/other"), &[]);
        assert_eq!(override_for("%9", "/tmp/other").as_deref(), Some("running"));
    }

    /// The reported bug: two agents in one folder, one waiting on approval,
    /// and both cards landing in Needs Approval. An event that names a session
    /// must not also be filed under the folder every sibling shares.
    #[test]
    fn a_sibling_in_the_same_folder_is_not_flagged() {
        let cwd = "/tmp/shared-project";

        // The tmux session that actually needs approval.
        record("permission", Some("%21"), Some(cwd), &[]);
        assert_eq!(override_for("%21", cwd).as_deref(), Some("permission"));
        assert_eq!(override_for("%22", cwd), None, "sibling pane flagged");
        assert_eq!(override_for("pid:9001", cwd), None, "sibling pid flagged");

        // And the same for a session outside tmux, named by pid.
        let cwd2 = "/tmp/shared-two";
        record("permission", None, Some(cwd2), &[4242]);
        assert_eq!(
            override_for("pid:4242", cwd2).as_deref(),
            Some("permission")
        );
        assert_eq!(override_for("pid:4243", cwd2), None, "sibling pid flagged");
    }

    /// Approving is not itself an event, so the next thing the session does is
    /// what has to clear the sticky permission — otherwise the card sits in
    /// Needs Approval for the full 30 minutes after the user has answered.
    #[test]
    fn the_next_event_clears_a_sticky_permission() {
        record("permission", Some("%31"), None, &[]);
        assert_eq!(override_for("%31", "").as_deref(), Some("permission"));
        // PreToolUse after the approval.
        record("running", Some("%31"), None, &[]);
        assert_eq!(override_for("%31", "").as_deref(), Some("running"));
    }

    /// The full `--hook-event` round trip: what an agent CLI's hook actually
    /// runs must land in the state the overlay reads. Covers the wire format
    /// the hand-written curl payload used to get wrong.
    #[test]
    fn hook_event_cli_reaches_the_listener() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        serve(listener, || {});

        // The hook reports its ancestors, and procscan names a non-tmux
        // session `pid:{p}` — so on linux the event lands on this test process
        // by pid. Elsewhere there are no pids and cwd is still the fallback.
        let cwd = std::env::current_dir().unwrap().display().to_string();
        let key = if cfg!(target_os = "linux") {
            let parent = ancestor_pids();
            assert!(!parent.is_empty(), "no ancestors resolved");
            format!("pid:{}", parent[0])
        } else {
            "no-such-pane".to_string()
        };
        post_event_to(port, "permission");
        wait_until(
            || override_for(&key, &cwd).as_deref() == Some("permission"),
            "hook event never reached the listener",
        );
    }

    /// A hook must never fail or hang its agent, even with no overlay running.
    #[test]
    fn posting_with_no_listener_is_harmless() {
        let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let started = Instant::now();
        post_event_to(port, "running");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// Claude's Notification hook also fires for idle nudges; only approval
    /// notifications may reach the Needs Approval column.
    #[test]
    fn only_permission_notifications_count() {
        assert!(is_permission_notification(
            r#"{"message":"Claude needs your permission to use Bash"}"#
        ));
        assert!(is_permission_notification(
            r#"{"message":"Permission required"}"#
        ));
        assert!(!is_permission_notification(
            r#"{"message":"Claude is waiting for your input"}"#
        ));
        assert!(!is_permission_notification(""));
    }

    /// The envelope carries paths we don't control — a cwd or transcript path
    /// under a directory called `permissions` would otherwise pin the session
    /// in Needs Approval for the full 30-minute sticky window.
    #[test]
    fn only_the_message_decides_not_the_envelope() {
        assert!(!is_permission_notification(
            r#"{"cwd":"/home/u/src/permissions","message":"Claude is waiting for your input"}"#
        ));
        assert!(!is_permission_notification(
            r#"{"transcript_path":"/home/u/.claude/permission-notes.jsonl","message":"Claude is waiting for your input"}"#
        ));
        // The message still decides, whatever else the envelope holds.
        assert!(is_permission_notification(
            r#"{"cwd":"/home/u/proj","message":"Claude needs your permission to use Bash"}"#
        ));
    }

    /// We don't own this schema. If the payload isn't JSON, or carries no
    /// message, fall back to the whole document rather than going silent —
    /// missing an approval is worse than an occasional false positive.
    #[test]
    fn an_unrecognised_payload_still_falls_back() {
        assert!(is_permission_notification("Claude needs your permission"));
        assert!(is_permission_notification(
            r#"{"detail":"permission required"}"#
        ));
        assert!(!is_permission_notification(r#"{"detail":"all done"}"#));
    }

    #[test]
    fn unknown_status_ignored() {
        record("exploded", Some("%8"), None, &[]);
        assert_eq!(override_for("%8", ""), None);
    }

    #[test]
    fn http_post_reaches_state() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle(stream, &|| {});
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

    /// A page the user happens to visit can POST here without a preflight, so
    /// an event carrying Origin/Referer must not be allowed to write status —
    /// forging "idle" would hide a waiting agent, "permission" would fill the
    /// Needs Approval column with sessions that want nothing.
    #[test]
    fn browser_events_are_ignored() {
        for header in [
            "Origin: https://evil.example",
            "Referer: https://evil.example/x",
        ] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            let t = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                handle(stream, &|| {});
            });
            let body = r#"{"status":"permission","pane":"%78","cwd":"/tmp/y"}"#;
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(
                c,
                "POST /event HTTP/1.1\r\nHost: x\r\n{}\r\nContent-Length: {}\r\n\r\n{}",
                header,
                body.len(),
                body
            )
            .unwrap();
            let mut resp = String::new();
            c.read_to_string(&mut resp).unwrap();
            t.join().unwrap();
            // Answered like any other request: the page learns nothing from it.
            assert!(resp.starts_with("HTTP/1.1 204"));
            assert_eq!(override_for("%78", "/tmp/y"), None, "recorded via {header}");
        }
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A second launch finds the port taken, is recognised as a handover, and
    /// the running instance is told to show itself.
    #[test]
    fn second_claim_hands_over_to_the_first() {
        let shown = Arc::new(AtomicUsize::new(0));
        let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        // First launch owns the port and starts serving.
        let Claim::Primary(listener) = claim_on(port) else {
            panic!("first claim on a free port must be primary");
        };
        let counter = shown.clone();
        serve(listener, move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });

        // Second launch: port taken, and an overlay answers.
        assert!(matches!(claim_on(port), Claim::Secondary));
        wait_until(
            || shown.load(Ordering::SeqCst) == 1,
            "primary was never shown",
        );
    }

    /// The reply must not wait on the window actually coming up. A primary
    /// still starting its UI would otherwise blow the requester's timeout and
    /// the second launch would open a window of its own — the original bug.
    #[test]
    fn handover_is_answered_before_the_window_is_raised() {
        let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let Claim::Primary(listener) = claim_on(port) else {
            panic!("first claim on a free port must be primary");
        };
        // Far longer than request_show's 2s budget.
        serve(listener, || std::thread::sleep(Duration::from_secs(6)));

        let started = Instant::now();
        assert!(matches!(claim_on(port), Claim::Secondary));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "handover blocked on the show callback"
        );
    }

    /// A stranger on the port must not be mistaken for an overlay, or the app
    /// would refuse to start whenever 8377 is occupied. This is also the
    /// upgrade case: an overlay predating the marker header answers like this,
    /// so a new build starts alongside an old one exactly once.
    #[test]
    fn foreign_listener_is_not_mistaken_for_an_overlay() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut s = stream;
                let _ = s.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
            }
        });
        // Right status code, no marker header — still not us.
        assert!(matches!(claim_on(port), Claim::Foreign(_)));
    }

    /// A browser can reach the port, so /show must ignore anything carrying an
    /// Origin — otherwise a web page could raise and focus the overlay at will.
    #[test]
    fn browser_originated_show_is_ignored() {
        let shown = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let counter = shown.clone();
        serve(listener, move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });

        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            c,
            "POST /show HTTP/1.1\r\nHost: x\r\nOrigin: http://evil.example\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .unwrap();
        let mut resp = [0u8; 128];
        let n = c.read(&mut resp).unwrap();
        assert!(String::from_utf8_lossy(&resp[..n]).starts_with("HTTP/1.1 204"));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            shown.load(Ordering::SeqCst),
            0,
            "browser request raised the window"
        );
    }

    /// A client that declares more body than it sends holds its connection for
    /// the full 2s read timeout. The next hook event must not queue behind it,
    /// and the stalled request must still be answered rather than dropped.
    #[test]
    fn a_stalled_client_does_not_block_the_next_event() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        serve(listener, || {});

        // Declares 500 bytes, sends none, and keeps the socket open.
        let mut stalled = TcpStream::connect(("127.0.0.1", port)).unwrap();
        // Timeouts so a regression that leaves the connection open without
        // answering fails the assertion instead of hanging the suite.
        stalled
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            stalled,
            "POST /event HTTP/1.1\r\nHost: x\r\nContent-Length: 500\r\n\r\n"
        )
        .unwrap();

        let started = Instant::now();
        let body = r#"{"status":"running","pane":"%81","cwd":"/tmp/z"}"#;
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(
            c,
            "POST /event HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
        let mut resp = String::new();
        c.read_to_string(&mut resp).unwrap();

        assert!(resp.starts_with("HTTP/1.1 204"));
        assert_eq!(override_for("%81", "/tmp/z").as_deref(), Some("running"));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "queued behind the stalled client: {:?}",
            started.elapsed()
        );

        // And the stalled one is still answered once its read times out.
        let mut stalled_resp = String::new();
        stalled.read_to_string(&mut stalled_resp).unwrap();
        assert!(
            stalled_resp.starts_with("HTTP/1.1 204"),
            "stalled request got no response: {stalled_resp:?}"
        );
    }

    fn wait_until(cond: impl Fn() -> bool, msg: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{msg}");
    }
}

/// Start the listener thread on a port already claimed by [`claim`].
/// `on_show` runs when a second launch hands its start-up over to us.
pub fn serve(listener: TcpListener, on_show: impl Fn() + Send + Sync + 'static) {
    let on_show = std::sync::Arc::new(on_show);
    std::thread::spawn(move || loop {
        match listener.accept() {
            // One connection per thread: `handle` can sit on its 2s read
            // timeout when a client declares more body than it sends, and a
            // hook event arriving meanwhile must not wait behind it.
            Ok((stream, _)) => {
                let on_show = on_show.clone();
                // Builder rather than thread::spawn: that one panics when the
                // OS refuses a thread, which would unwind this loop and kill
                // the listener for good — under exactly the exhaustion the arm
                // below exists to ride out. On failure the closure drops with
                // the connection: a dropped event is survivable (the scraper
                // covers it), a dead listener is not.
                if let Err(e) = std::thread::Builder::new().spawn(move || {
                    let _ = handle(stream, on_show.as_ref());
                }) {
                    eprintln!("hook listener: cannot spawn handler: {e}");
                }
            }
            // Never spin: a persistent failure here is fd exhaustion, which
            // returns immediately and forever. Without the pause this thread
            // burns a core with nothing logged and hooks silently dead.
            Err(e) => {
                eprintln!("hook listener: accept failed: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    });
}
