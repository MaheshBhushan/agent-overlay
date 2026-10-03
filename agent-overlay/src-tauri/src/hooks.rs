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

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

pub const PORT: u16 = 8377;

/// running/idle events override scraping for this long; after that the
/// scraper takes back over (covers missed hooks / killed agents).
const EVENT_TTL_SECS: u64 = 120;
/// A permission request stays sticky longer — the user may take a while to
/// answer and no further hook fires until they do. Any newer event
/// (e.g. PreToolUse after approval) replaces it immediately.
const PERMISSION_TTL_SECS: u64 = 1800;
/// How long the overlay holds a `--hook-permission` request open. The hook
/// entry's own timeout (hookinstall.rs) is longer, so the overlay always
/// answers "no decision" before Claude kills the hook.
const APPROVAL_WAIT_SECS: u64 = 570;

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
    /// The reporting agent ("codex", …), when the hook could tell. Scopes the
    /// `cwd` fallback key to sessions of that agent.
    #[serde(default)]
    agent: Option<String>,
}

struct Entry {
    status: String,
    at: Instant,
    /// Process creation value for pid-keyed events; prevents a stale hook from
    /// attaching to a replacement process that reused the same pid.
    start_time: Option<u64>,
}

static STATE: Mutex<Option<HashMap<String, Entry>>> = Mutex::new(None);

/// A state key plus, for pid keys, the process creation value it was taken at.
type Key = (String, Option<u64>);

/// The keys an event from a hook is filed under.
///
/// A pane id or a pid names one session. `cwd` names a folder, and two agents
/// working in one folder are a normal thing to do — so it is the last resort,
/// used only when nothing precise is available. Writing it alongside a precise
/// key is what used to put an idle sibling in the Needs Approval column next
/// to the session actually waiting.
fn event_keys(pane: Option<&str>, cwd: Option<&str>, pids: &[u32], agent: Option<&str>) -> Vec<Key> {
    let pane = pane.filter(|p| !p.is_empty()).map(str::to_string);
    let live_pids: Vec<(u32, u64)> = pids
        .iter()
        .filter_map(|pid| crate::procscan::process_start_time(*pid).map(|start| (*pid, start)))
        .collect();
    let precise = pane.is_some() || !live_pids.is_empty();
    let mut keys: Vec<Key> = pane.into_iter().map(|key| (key, None)).collect();
    keys.extend(
        live_pids
            .into_iter()
            .map(|(pid, start)| (format!("pid:{pid}"), Some(start))),
    );
    if let Some(cwd) = cwd.filter(|c| !c.is_empty() && !precise) {
        keys.push((cwd_key(cwd, agent), None));
    }
    keys
}

/// The folder key. With the agent named, only that agent's sessions in the
/// folder match it: a codex approval must not land on a claude session that
/// happens to work in the same directory.
fn cwd_key(cwd: &str, agent: Option<&str>) -> String {
    match agent.filter(|a| !a.is_empty()) {
        Some(agent) => format!("cwd:{agent}:{cwd}"),
        None => format!("cwd:{cwd}"),
    }
}

/// The keys a session is looked up by, most precise first.
fn lookup_keys(pane_id: &str, cwd: &str, agent: &str) -> [String; 3] {
    [pane_id.to_string(), cwd_key(cwd, Some(agent)), cwd_key(cwd, None)]
}

/// The keys that name the reporting session itself, for matching one hook
/// request against another.
///
/// [`event_keys`] files a status under every ancestor pid, which is harmless
/// for lookups: a session is only ever looked up by its own key. Comparing two
/// events by those keys is not harmless. Two agents in sibling tabs share the
/// terminal emulator's pid, so the tab that keeps working would cancel the
/// approval pending in the other one. Only the pane and the agent's own pid
/// identify a session.
fn session_keys(pane: Option<&str>, cwd: Option<&str>, pids: &[u32], agent: Option<&str>) -> Vec<Key> {
    only_agent_pid(event_keys(pane, cwd, pids, agent), agent_pid(pids))
}

/// Drop every pid key but the agent's. With no agent among the pids (an agent
/// CLI we don't recognise), keep them all: an approval that can collide with a
/// sibling tab beats one that never shows up.
fn only_agent_pid(keys: Vec<Key>, agent: Option<u32>) -> Vec<Key> {
    let Some(agent) = agent else {
        return keys;
    };
    let agent = format!("pid:{agent}");
    keys.into_iter()
        .filter(|(key, _)| !key.starts_with("pid:") || *key == agent)
        .collect()
}

/// The first of `pids` whose command line is an agent CLI, matched the way
/// procscan matches a session's own process.
#[cfg(target_os = "linux")]
fn agent_pid(pids: &[u32]) -> Option<u32> {
    pids.iter()
        .copied()
        .find(|pid| cmdline(*pid).is_some_and(|args| crate::tmux::agent_from_args(&args).is_some()))
}

#[cfg(not(target_os = "linux"))]
fn agent_pid(_pids: &[u32]) -> Option<u32> {
    None
}

#[cfg(test)]
fn record(status: &str, pane: Option<&str>, cwd: Option<&str>, pids: &[u32]) {
    record_as(status, pane, cwd, pids, None);
}

fn record_as(status: &str, pane: Option<&str>, cwd: Option<&str>, pids: &[u32], agent: Option<&str>) {
    if !matches!(status, "running" | "idle" | "permission") {
        return;
    }
    // A running or idle event means the session moved on, so an approval it
    // was waiting on was answered in the terminal (or the turn ended). Drop
    // the overlay's copy so the card loses its buttons. A permission event is
    // the same request announced again by `Notification`, so it keeps it.
    if status != "permission" {
        cancel_pending(&session_keys(pane, cwd, pids, agent));
    }
    record_keys(status, event_keys(pane, cwd, pids, agent));
}

fn record_keys(status: &str, keys: Vec<Key>) {
    let mut guard = STATE.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    let now = Instant::now();
    for (key, start_time) in keys {
        map.insert(
            key,
            Entry {
                status: status.to_string(),
                at: now,
                start_time,
            },
        );
    }
}

/// Fresh hook-reported status for a session, if any. Pane id wins over cwd.
pub fn override_for(pane_id: &str, cwd: &str, agent: &str, start_time: u64) -> Option<String> {
    let guard = STATE.lock().unwrap();
    let map = guard.as_ref()?;
    let now = Instant::now();
    for key in lookup_keys(pane_id, cwd, agent) {
        if let Some(e) = map.get(&key) {
            if e.start_time.is_some_and(|recorded| recorded != start_time) {
                continue;
            }
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

// ── approvals answered from the overlay ────────────────────────────────
//
// Claude Code's `PermissionRequest` hook runs alongside its own approval
// dialog, and whichever answers first wins. The hook (`--hook-permission`)
// posts the request here and holds the connection open. The card shows
// Approve / Deny, and the user's click travels back down that connection as
// the hook's decision. Answering in the terminal instead is always possible.
// The session's next running/idle event then retires the request here.
//
// opencode's TUI plugin (hooks/opencode-tui.ts) holds a request open the same
// way and passes the answer to opencode's own permission API.
//
// Codex is the exception. It shows its prompt only once the hook has
// returned, so a hook that waited for the overlay would freeze the terminal.
// `--hook-codex-permission` reports the request and returns at once, and the
// answer is typed into Codex's prompt in the session's tmux pane — see
// [`Reply::Keys`].

/// An approval the overlay can answer, as shown on the session's card.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Approval {
    pub request_id: String,
    pub tool: String,
    pub summary: String,
}

struct Pending {
    approval: Approval,
    keys: Vec<Key>,
    reply: Reply,
}

/// How the user's answer reaches the agent.
enum Reply {
    /// A hook holding its connection open for the decision.
    Hook(mpsc::Sender<Decision>),
    /// Codex's prompt, on screen once its hook has returned. The answer is
    /// typed into the pane of the card the user clicked, so only sessions in
    /// tmux get buttons. Nothing holds a connection, so the request also
    /// expires on its own.
    Keys { since: Instant },
}

impl Pending {
    fn expired(&self, now: Instant) -> bool {
        matches!(&self.reply, Reply::Keys { since, .. }
            if now.duration_since(*since).as_secs() >= PERMISSION_TTL_SECS)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decision {
    Allow,
    Deny,
    /// Nothing from the overlay; Claude's own dialog decides.
    Pass,
}

static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());
static NEXT_APPROVAL: AtomicU64 = AtomicU64::new(1);

fn keys_overlap(a: &[Key], b: &[Key]) -> bool {
    a.iter().any(|k| b.contains(k))
}

/// Retire every pending request filed under one of `keys`, answering each
/// hook with [`Decision::Pass`].
fn cancel_pending(keys: &[Key]) {
    cancel_in(&mut PENDING.lock().unwrap(), keys);
}

fn cancel_in(pending: &mut Vec<Pending>, keys: &[Key]) {
    if keys.is_empty() {
        return;
    }
    pending.retain(|p| {
        let stale = keys_overlap(&p.keys, keys);
        if let (true, Reply::Hook(tx)) = (stale, &p.reply) {
            let _ = tx.send(Decision::Pass);
        }
        !stale
    });
}

fn add_pending(tool: String, summary: String, keys: Vec<Key>, reply: Reply) -> String {
    let request_id = format!("AP-{}", NEXT_APPROVAL.fetch_add(1, Ordering::Relaxed));
    // A new request from a session supersedes any older one: Claude has moved
    // past the dialog that one belonged to. One lock for both steps, so no
    // refresh in between sees the session with no request at all.
    let mut pending = PENDING.lock().unwrap();
    cancel_in(&mut pending, &keys);
    pending.push(Pending {
        approval: Approval {
            request_id: request_id.clone(),
            tool,
            summary,
        },
        keys,
        reply,
    });
    request_id
}

fn remove_pending(request_id: &str) {
    PENDING
        .lock()
        .unwrap()
        .retain(|p| p.approval.request_id != request_id);
}

/// The approval waiting on this session, if the overlay can answer it.
/// Looked up the same way as [`override_for`]. A Codex request can only be
/// answered in tmux.
pub fn approval_for(
    pane_id: &str,
    cwd: &str,
    agent: &str,
    start_time: u64,
    in_tmux: bool,
) -> Option<Approval> {
    let mut pending = PENDING.lock().unwrap();
    let now = Instant::now();
    pending.retain(|p| !p.expired(now));
    let wanted = lookup_keys(pane_id, cwd, agent);
    pending
        .iter()
        .rev()
        .filter(|p| in_tmux || matches!(p.reply, Reply::Hook(_)))
        .find(|p| {
            p.keys.iter().any(|(key, recorded)| {
                wanted.contains(key) && recorded.is_none_or(|r| r == start_time)
            })
        })
        .map(|p| p.approval.clone())
}

/// Send the user's answer to the hook holding `request_id`. `pane` is the
/// tmux pane of the card that was clicked, where a Codex answer is typed.
pub fn answer(request_id: &str, pane: Option<&str>, allow: bool) -> Result<(), String> {
    let pending = {
        let mut all = PENDING.lock().unwrap();
        let i = all
            .iter()
            .position(|p| p.approval.request_id == request_id)
            .ok_or_else(|| format!("approval {request_id} was already answered"))?;
        all.remove(i)
    };
    match &pending.reply {
        Reply::Hook(tx) => tx
            .send(if allow { Decision::Allow } else { Decision::Deny })
            .map_err(|_| format!("approval {request_id} was already answered"))?,
        Reply::Keys { .. } => {
            let pane = pane.ok_or("this session isn't in tmux; answer it in its terminal")?;
            answer_codex_prompt(pane, &pending.approval.summary, allow)?
        }
    }
    // Nothing else reports that the dialog closed: the next hook is the
    // following tool's PreToolUse or the turn's Stop. Leave the card in Needs
    // Approval until then and it looks as if the click did nothing.
    record_keys("running", pending.keys);
    Ok(())
}

#[derive(Deserialize)]
struct PermissionRequest {
    pane: Option<String>,
    cwd: Option<String>,
    #[serde(default)]
    pids: Vec<u32>,
    #[serde(default)]
    tool: String,
    #[serde(default)]
    summary: String,
    /// `"keys"`: the hook has already returned, and the answer is typed into
    /// the agent's prompt (Codex). Absent: the hook waits for the decision.
    #[serde(default)]
    reply: Option<String>,
    #[serde(default)]
    agent: Option<String>,
}

/// Has the hook on the other end hung up? Claude may kill it once the
/// terminal dialog is answered, and a request nobody is waiting on must not
/// keep its buttons.
fn peer_closed(stream: &TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return true;
    }
    let closed = match stream.peek(&mut [0u8; 1]) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => e.kind() != std::io::ErrorKind::WouldBlock,
    };
    let _ = stream.set_nonblocking(false);
    closed
}

/// Hold a `POST /permission` open until the user decides, the session moves
/// on, the hook hangs up, or [`APPROVAL_WAIT_SECS`] pass.
fn await_decision(stream: &TcpStream, req: PermissionRequest) -> Decision {
    let pane = req.pane.as_deref();
    let cwd = req.cwd.as_deref();
    let agent = req.agent.as_deref();
    record_as("permission", pane, cwd, &req.pids, agent);
    let (tx, rx) = mpsc::channel();
    let keys = session_keys(pane, cwd, &req.pids, agent);
    let request_id = add_pending(req.tool, req.summary, keys, Reply::Hook(tx));
    let deadline = Instant::now() + Duration::from_secs(APPROVAL_WAIT_SECS);
    let decision = loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(decision) => break decision,
            Err(mpsc::RecvTimeoutError::Disconnected) => break Decision::Pass,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline || peer_closed(stream) {
                    break Decision::Pass;
                }
            }
        }
    };
    remove_pending(&request_id);
    decision
}

/// File a request whose hook has already returned (Codex). The card gets
/// buttons only in tmux, the one place the overlay can type the answer.
fn register_keys_request(req: PermissionRequest) {
    let pane = req.pane.as_deref();
    let cwd = req.cwd.as_deref();
    let agent = req.agent.as_deref();
    record_as("permission", pane, cwd, &req.pids, agent);
    let keys = session_keys(pane, cwd, &req.pids, agent);
    if !keys.is_empty() {
        add_pending(req.tool, req.summary, keys, Reply::Keys { since: Instant::now() });
    }
}

/// Type the user's answer into Codex's approval prompt.
///
/// Refuses unless the prompt for this very request is on screen. Keys sent
/// anywhere else would land in the composer or in another prompt, and a
/// stray `y` must never approve a different command.
fn answer_codex_prompt(pane: &str, summary: &str, allow: bool) -> Result<(), String> {
    let screen = crate::tmux::capture_pane(pane, 0);
    if !codex_prompt_shows(&screen, summary) {
        return Err("Codex isn't showing this approval any more; answer it in the terminal".into());
    }
    if allow {
        crate::tmux::send_keys(pane, "y", false)
    } else {
        crate::tmux::send_key(pane, "Escape")
    }
}

/// Is Codex's approval prompt on `screen`, and is it about `summary`?
///
/// Codex wraps the command to the pane, so compare with whitespace folded,
/// and only a prefix: a long command is cut short on screen.
fn codex_prompt_shows(screen: &str, summary: &str) -> bool {
    let fold = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let screen = fold(screen);
    let summary = fold(summary);
    let prefix: String = summary.chars().take(24).collect();
    screen.contains("Yes, proceed") && screen.contains(&prefix)
}

/// The `--hook-permission` path: Claude Code's `PermissionRequest` hook.
/// Returns what the hook should print on stdout, or `None` to print nothing
/// and leave the decision to Claude's own dialog. With no overlay running it
/// returns `None` at once.
pub fn request_permission(payload: &str) -> Option<String> {
    let wait = Duration::from_secs(APPROVAL_WAIT_SECS + 10);
    send_permission(PORT, &permission_body(payload, &reporter()), wait)
}

/// The `--hook-codex-permission` path. Codex waits for its hooks before it
/// shows the prompt, so this reports the request and returns without waiting
/// for an answer.
pub fn report_codex_permission(payload: &str) {
    let body = keys_body(&permission_body(payload, &reporter()));
    send_permission(PORT, &body, Duration::from_secs(2));
}

/// Who a hook is reporting for: the session's tmux pane and pids, and its
/// agent when one of the pids is an agent CLI.
#[derive(Default)]
struct Reporter {
    pane: String,
    pids: Vec<u32>,
    agent: Option<String>,
}

/// Work out the reporter from this hook process's environment and ancestry.
///
/// Codex 0.160+ runs every session's hooks inside one shared `codex
/// app-server`. A hook there inherits the server's environment, whose
/// TMUX_PANE is whichever terminal happened to start the server, possibly a
/// pane long since closed and reused, and its pids are the server's. Both
/// would file the event on the wrong card, so a hook under a shared server
/// reports only its folder and agent.
fn reporter() -> Reporter {
    let pids = ancestor_pids();
    let pane = std::env::var("TMUX_PANE").unwrap_or_default();
    for pid in &pids {
        let Some(args) = cmdline(*pid) else { continue };
        if let Some(agent) = crate::tmux::shared_server(&args) {
            return Reporter {
                agent: Some(agent.to_string()),
                ..Reporter::default()
            };
        }
        if let Some(agent) = crate::tmux::agent_from_args(&args) {
            return Reporter {
                pane,
                pids,
                agent: Some(agent),
            };
        }
    }
    Reporter {
        pane,
        pids,
        agent: None,
    }
}

#[cfg(target_os = "linux")]
fn cmdline(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(String::from_utf8_lossy(&raw).replace('\0', " "))
}

#[cfg(not(target_os = "linux"))]
fn cmdline(_pid: u32) -> Option<String> {
    None
}

/// Mark a `POST /permission` body as answered by typing into the prompt.
fn keys_body(body: &str) -> String {
    let mut body: Value = serde_json::from_str(body).unwrap_or_default();
    body["reply"] = Value::from("keys");
    body.to_string()
}

/// The `POST /permission` body for the hook payload Claude wrote on stdin.
fn permission_body(payload: &str, who: &Reporter) -> String {
    let input: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    let tool = input
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string();
    let summary = permission_summary(&tool, input.get("tool_input").unwrap_or(&Value::Null));
    let cwd = input
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| std::env::current_dir().ok().map(|p| p.display().to_string()))
        .unwrap_or_default();
    serde_json::json!({
        "pane": who.pane,
        "cwd": cwd,
        "pids": who.pids,
        "agent": who.agent,
        "tool": tool,
        "summary": summary,
    })
    .to_string()
}

/// Post a permission request and wait for the overlay's answer. Returns the
/// hook's stdout for a decision, or `None` for none.
fn send_permission(port: u16, body: &str, wait: Duration) -> Option<String> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut c = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()?;
    let _ = c.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = c.set_read_timeout(Some(wait));
    c.write_all(
        format!(
            "POST /permission HTTP/1.1\r\nHost: localhost\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .as_bytes(),
    )
    .ok()?;
    let mut resp = String::new();
    c.read_to_string(&mut resp).ok()?;
    let (head, decision) = resp.split_once("\r\n\r\n")?;
    if !head.to_ascii_lowercase().contains(&MARKER.to_ascii_lowercase()) {
        return None;
    }
    let decision = match decision.trim() {
        "allow" => serde_json::json!({ "behavior": "allow" }),
        "deny" => serde_json::json!({
            "behavior": "deny",
            "message": "The user denied this from Agent Overlay.",
        }),
        _ => return None,
    };
    Some(
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": decision,
            }
        })
        .to_string(),
    )
}

/// One line saying what the tool wants to do: the command for Bash, the path
/// for file tools, and the input itself for anything unrecognised.
fn permission_summary(tool: &str, input: &Value) -> String {
    // Codex passes a command as argv rather than one string.
    let field = |name: &str| match input.get(name)? {
        Value::String(s) => Some(s.clone()),
        Value::Array(argv) => Some(
            argv.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    };
    let text = match tool {
        "Bash" => field("command"),
        "Edit" | "MultiEdit" | "Write" | "Read" => field("file_path"),
        "NotebookEdit" => field("notebook_path"),
        "WebFetch" => field("url"),
        "WebSearch" => field("query"),
        "Glob" | "Grep" => field("pattern"),
        _ => None,
    }
    .unwrap_or_else(|| match input {
        Value::Null => String::new(),
        other => other.to_string(),
    });
    const MAX: usize = 400;
    match text.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
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
    let who = reporter();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let body = serde_json::json!({
        "status": status,
        "pane": who.pane,
        "cwd": cwd,
        "pids": who.pids,
        "agent": who.agent,
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
    if path == "/permission" {
        let mut stream = reader.into_inner();
        // A web page must never get to approve anything; answer it "no
        // decision" without registering the request.
        let decision = match serde_json::from_slice::<PermissionRequest>(&body) {
            Ok(req) if !from_browser && req.reply.as_deref() == Some("keys") => {
                register_keys_request(req);
                Decision::Pass
            }
            Ok(req) if !from_browser => await_decision(&stream, req),
            _ => Decision::Pass,
        };
        let body = match decision {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
            Decision::Pass => "pass",
        };
        let _ = stream.write_all(
            format!(
                "HTTP/1.1 200 OK\r\n{MARKER}: 1\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
        let _ = stream.flush();
        return Some(());
    }
    if path != "/show" && !from_browser {
        if let Ok(ev) = serde_json::from_slice::<HookEvent>(&body) {
            record_as(
                &ev.status,
                ev.pane.as_deref(),
                ev.cwd.as_deref(),
                &ev.pids,
                ev.agent.as_deref(),
            );
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

    fn override_for(handle: &str, cwd: &str) -> Option<String> {
        let start_time = handle
            .strip_prefix("pid:")
            .and_then(|pid| pid.parse().ok())
            .and_then(crate::procscan::process_start_time)
            .unwrap_or(0);
        super::override_for(handle, cwd, "claude", start_time)
    }

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
        let pid = std::process::id();
        record("permission", None, Some(cwd2), &[pid]);
        assert_eq!(
            override_for(&format!("pid:{pid}"), cwd2).as_deref(),
            Some("permission")
        );
        assert_eq!(
            override_for(&format!("pid:{}", pid + 1), cwd2),
            None,
            "sibling pid flagged"
        );
    }

    #[test]
    fn pid_reuse_does_not_inherit_a_hook_status() {
        let pid = std::process::id();
        let start = crate::procscan::process_start_time(pid).unwrap();
        record("permission", None, None, &[pid]);

        assert_eq!(
            super::override_for(&format!("pid:{pid}"), "", "claude", start).as_deref(),
            Some("permission")
        );
        assert_eq!(
            super::override_for(&format!("pid:{pid}"), "", "claude", start + 1),
            None
        );
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

    // ── approvals ──────────────────────────────────────────────────

    /// Start a listener and raise a permission request from `pane` on a
    /// thread, the way `--hook-permission` would. Returns the hook's eventual
    /// stdout. Each test uses its own pane: the pending list is global.
    fn raise_request(
        pane: &'static str,
        payload: &'static str,
    ) -> std::thread::JoinHandle<Option<String>> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        serve(listener, || {});
        let hook = std::thread::spawn(move || {
            send_permission(port, &permission_body(payload, &in_pane(pane)), Duration::from_secs(5))
        });
        wait_until(
            || approval_for(pane, "", "claude", 0, true).is_some(),
            "request never reached the overlay",
        );
        hook
    }

    /// A hook reporting from `pane`, agent unknown.
    fn in_pane(pane: &str) -> Reporter {
        Reporter {
            pane: pane.to_string(),
            ..Reporter::default()
        }
    }

    const BASH_PAYLOAD: &str =
        r#"{"tool_name":"Bash","tool_input":{"command":"npm test"},"cwd":"/tmp/approval-test"}"#;

    #[test]
    fn an_approval_from_the_overlay_reaches_the_hook() {
        let hook = raise_request("%7101", BASH_PAYLOAD);
        let approval = approval_for("%7101", "", "codex", 0, true).unwrap();
        assert_eq!(approval.tool, "Bash");
        assert_eq!(approval.summary, "npm test");
        assert_eq!(override_for("%7101", "").as_deref(), Some("permission"));

        answer(&approval.request_id, None, true).unwrap();
        let out: Value = serde_json::from_str(&hook.join().unwrap().unwrap()).unwrap();
        assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PermissionRequest");
        assert_eq!(out["hookSpecificOutput"]["decision"]["behavior"], "allow");

        // The card leaves Needs Approval at once instead of waiting for the
        // session's next hook.
        assert_eq!(approval_for("%7101", "", "codex", 0, true), None);
        assert_eq!(override_for("%7101", "").as_deref(), Some("running"));
        assert!(answer(&approval.request_id, None, true).is_err(), "answered twice");
    }

    #[test]
    fn a_denial_from_the_overlay_reaches_the_hook() {
        let hook = raise_request("%7102", BASH_PAYLOAD);
        let approval = approval_for("%7102", "", "codex", 0, true).unwrap();
        answer(&approval.request_id, None, false).unwrap();
        let out: Value = serde_json::from_str(&hook.join().unwrap().unwrap()).unwrap();
        let decision = &out["hookSpecificOutput"]["decision"];
        assert_eq!(decision["behavior"], "deny");
        assert!(decision["message"].as_str().unwrap().contains("Agent Overlay"));
    }

    /// Answered in the terminal instead: the session's next event retires the
    /// request, and the hook exits printing nothing.
    #[test]
    fn the_next_event_retires_an_open_request() {
        let hook = raise_request("%7103", BASH_PAYLOAD);
        // Notification announces the same request again; that keeps it.
        record("permission", Some("%7103"), None, &[]);
        assert!(approval_for("%7103", "", "codex", 0, true).is_some());

        record("running", Some("%7103"), None, &[]);
        assert_eq!(hook.join().unwrap(), None);
        assert_eq!(approval_for("%7103", "", "codex", 0, true), None);
    }

    #[test]
    fn a_new_request_supersedes_the_old_one() {
        let first = raise_request("%7104", BASH_PAYLOAD);
        let old = approval_for("%7104", "", "codex", 0, true).unwrap();
        let second = raise_request("%7104", BASH_PAYLOAD);
        assert_eq!(first.join().unwrap(), None, "the stale hook kept waiting");
        let new = approval_for("%7104", "", "codex", 0, true).unwrap();
        assert_ne!(old.request_id, new.request_id);
        answer(&new.request_id, None, true).unwrap();
        assert!(second.join().unwrap().is_some());
    }

    /// An unrelated session must not get, or clear, this one's buttons.
    #[test]
    fn a_request_belongs_to_its_own_session() {
        let hook = raise_request("%7105", BASH_PAYLOAD);
        assert_eq!(approval_for("%7106", "/tmp/approval-test", "codex", 0, true), None);
        record("running", Some("%7106"), None, &[]);
        let approval = approval_for("%7105", "", "codex", 0, true).expect("a sibling cleared it");
        answer(&approval.request_id, None, true).unwrap();
        hook.join().unwrap();
    }

    #[test]
    fn requesting_with_no_overlay_is_immediate_and_silent() {
        let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let started = Instant::now();
        assert_eq!(
            send_permission(port, &permission_body(BASH_PAYLOAD, &in_pane("%7107")), Duration::from_secs(5)),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_browser_cannot_raise_a_request() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        serve(listener, || {});
        let body = permission_body(BASH_PAYLOAD, &in_pane("%7108"));
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.write_all(
            format!(
                "POST /permission HTTP/1.1\r\nOrigin: https://evil.example\r\n\
                 Content-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .unwrap();
        let mut resp = String::new();
        c.read_to_string(&mut resp).unwrap();
        assert!(resp.ends_with("pass"), "{resp}");
        assert_eq!(approval_for("%7108", "", "codex", 0, true), None);
    }

    /// Sibling tabs share the terminal emulator's pid. Only the agent's own
    /// pid may identify the session an approval belongs to.
    #[test]
    fn only_the_agent_pid_names_a_session() {
        let keys = vec![
            ("%1".to_string(), None),
            ("pid:10".to_string(), Some(1)),
            ("pid:20".to_string(), Some(2)),
        ];
        assert_eq!(
            only_agent_pid(keys.clone(), Some(20)),
            vec![("%1".to_string(), None), ("pid:20".to_string(), Some(2))]
        );
        assert_eq!(only_agent_pid(keys.clone(), None), keys);
    }

    #[test]
    fn summaries_name_what_the_tool_will_touch() {
        let input = |v: &str| serde_json::from_str::<Value>(v).unwrap();
        assert_eq!(permission_summary("Bash", &input(r#"{"command":"ls -la"}"#)), "ls -la");
        assert_eq!(
            permission_summary("Edit", &input(r#"{"file_path":"/a/b.rs","old_string":"x"}"#)),
            "/a/b.rs"
        );
        assert_eq!(
            permission_summary("mcp__x__y", &input(r#"{"q":1}"#)),
            r#"{"q":1}"#
        );
        let long = format!(r#"{{"command":"{}"}}"#, "é".repeat(500));
        let cut = permission_summary("Bash", &input(&long));
        assert_eq!(cut.chars().count(), 401);
        assert!(cut.ends_with('…'));
    }

    // ── codex: answered by typing into its prompt ──────────────────

    /// Post a codex-style request the way `--hook-codex-permission` would.
    fn raise_codex_request(pane: &str, payload: &str) -> Duration {
        raise_codex_request_as(&in_pane(pane), payload)
    }

    fn raise_codex_request_as(who: &Reporter, payload: &str) -> Duration {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        serve(listener, || {});
        let started = Instant::now();
        let body = keys_body(&permission_body(payload, who));
        assert_eq!(send_permission(port, &body, Duration::from_secs(2)), None);
        started.elapsed()
    }

    /// Codex shows its prompt only after the hook exits, so the hook must not
    /// wait for the user.
    #[test]
    fn a_codex_request_returns_at_once_with_buttons_in_tmux() {
        let took = raise_codex_request("%7111", BASH_PAYLOAD);
        assert!(took < Duration::from_secs(1), "the hook waited {took:?}");
        let approval = approval_for("%7111", "", "codex", 0, true).expect("no buttons");
        assert_eq!(approval.summary, "npm test");
        assert_eq!(override_for("%7111", "").as_deref(), Some("permission"));

        // Approving when that pane doesn't show this prompt is refused, not
        // typed blind. (No tmux pane %7111 exists, so nothing shows it.)
        let err = answer(&approval.request_id, Some("%7111"), true).unwrap_err();
        assert!(err.contains("terminal"), "{err}");
    }

    /// Codex 0.160 runs hooks in a shared app-server, so the request names a
    /// folder and an agent, not a pane. Every codex session in tmux in that
    /// folder may answer it — the prompt check picks the right pane — but no
    /// other agent's session there sees it, and outside tmux there is nowhere
    /// to type the answer.
    #[test]
    fn a_request_from_a_shared_codex_server_reaches_codex_cards_only() {
        let payload = r#"{"tool_name":"Bash","tool_input":{"command":"ls"},"cwd":"/tmp/codex-shared"}"#;
        let server = Reporter {
            agent: Some("codex".into()),
            ..Reporter::default()
        };
        raise_codex_request_as(&server, payload);
        let dir = "/tmp/codex-shared";
        assert!(approval_for("%7113", dir, "codex", 0, true).is_some());
        assert_eq!(approval_for("pid:1", dir, "codex", 0, false), None, "not in tmux");
        assert_eq!(approval_for("%7114", dir, "claude", 0, true), None, "another agent");
        assert_eq!(super::override_for("%7114", dir, "claude", 0), None);
        assert_eq!(
            super::override_for("%7113", dir, "codex", 0).as_deref(),
            Some("permission")
        );
        let approval = approval_for("%7113", dir, "codex", 0, true).unwrap();
        let err = answer(&approval.request_id, None, true).unwrap_err();
        assert!(err.contains("tmux"), "{err}");
    }

    #[test]
    fn the_next_event_retires_a_codex_request() {
        raise_codex_request("%7112", BASH_PAYLOAD);
        assert!(approval_for("%7112", "", "codex", 0, true).is_some());
        record("running", Some("%7112"), None, &[]);
        assert_eq!(approval_for("%7112", "", "codex", 0, true), None);
    }

    #[test]
    fn keys_go_only_to_the_prompt_for_this_request() {
        let screen = "\
  Would you like to run the following command?

  $ npm run build -- --filter
    web

› 1. Yes, proceed (y)
  2. Yes, and don't ask again for this command (p)
  3. No, and tell Codex what to do differently (esc)
";
        assert!(codex_prompt_shows(screen, "npm run build -- --filter web"));
        assert!(!codex_prompt_shows(screen, "rm -rf target"), "another command");
        assert!(!codex_prompt_shows("› npm run build", "npm run build"), "no prompt");
    }

    #[test]
    fn an_argv_command_reads_as_one_line() {
        let input: Value = serde_json::from_str(r#"{"command":["bash","-lc","ls"]}"#).unwrap();
        assert_eq!(permission_summary("Bash", &input), "bash -lc ls");
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
