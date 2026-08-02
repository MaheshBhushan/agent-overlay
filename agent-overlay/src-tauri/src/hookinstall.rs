//! Installing the overlay's status hooks into each supported agent CLI.
//!
//! Scraping works everywhere with zero setup, but it can only *guess* at
//! "needs approval" from pane text — and on Windows there are no panes to
//! scrape at all. Hooks are exact, so we install them for the user instead of
//! asking them to hand-merge JSON:
//!
//! | CLI      | target                                       | approval signal        |
//! |----------|----------------------------------------------|------------------------|
//! | claude   | `~/.claude/settings.json` (merged)           | `Notification`         |
//! | codex    | `~/.codex/hooks/hooks.json` (merged)         | `permission_request`   |
//! | opencode | `~/.config/opencode/plugin/agent-overlay.ts` | `permission.ask`       |
//! | pi       | `~/.pi/agent/extensions/agent-overlay.ts`    | none (scraped)         |
//!
//! Two properties matter more than anything else here, because this code edits
//! files the user did not ask us to touch:
//!
//! * **Idempotent.** Installing twice is a no-op; installing after an upgrade
//!   rewrites our own entries (the exe path may have moved).
//! * **Never destructive.** We only ever remove hook entries we recognise as
//!   ours ([`is_ours`]). Everything else in those files is preserved verbatim.
//!
//! Command hooks (claude, codex) invoke *this binary* rather than `curl`:
//! `agent-overlay --hook-event running`. That sidesteps having to write a shell
//! pipeline that works in both `sh` and `cmd.exe` — the old curl payload used
//! `$TMUX_PANE`, `$PWD`, single-quoted headers and `payload=$(cat)`, none of
//! which `cmd.exe` honours, so it silently did nothing on Windows.

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// Bumped when a payload changes, so an upgrade can tell "installed" from
/// "installed, but an older version".
///
/// 2: claude installs `Notification` only. Existing installs carry the three
/// retired events, and first run short-circuits on the stamp — without a bump
/// the cleanup below would never run for anyone already set up.
pub const HOOKS_VERSION: &str = "2";

const OPENCODE_PLUGIN: &str = include_str!("../../hooks/opencode-plugin.ts");
const PI_EXTENSION: &str = include_str!("../../hooks/pi-extension.ts");

/// What we found (and possibly did) for one CLI.
#[derive(serde::Serialize, Clone)]
pub struct CliHooks {
    /// Stable id: "claude" | "codex" | "opencode" | "pi".
    pub id: &'static str,
    pub name: &'static str,
    /// Is this CLI present on the machine at all?
    pub present: bool,
    /// Our hooks are installed and current.
    pub installed: bool,
    /// Our hooks are installed but from an older payload version.
    pub outdated: bool,
    /// Where the hooks live (or would live).
    pub path: String,
    /// Does this CLI give us an exact approval signal?
    pub exact_approval: bool,
    /// Non-fatal detail for the UI: why not installed, what's approximated.
    pub note: String,
}

/// Result of an install attempt, one per CLI.
#[derive(serde::Serialize)]
pub struct InstallOutcome {
    pub id: &'static str,
    pub name: &'static str,
    /// "installed" | "updated" | "unchanged" | "skipped" | "failed"
    pub action: String,
    pub detail: String,
}

/// `$HOME` on unix, `%USERPROFILE%` on Windows.
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// The command a hook should run to report `status`. Quoted so a path with
/// spaces survives both `sh -c` and `cmd /c` — the Windows default install
/// location (`…\Program Files\…`, `…\AppData\Local\…`) always has one.
fn event_command(status: &str) -> String {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "agent-overlay".into());
    format!("\"{exe}\" --hook-event {status}")
}

/// Claude's `Notification` hook fires for more than approvals, so the decision
/// needs the event payload on stdin. `--hook-notify` reads it and posts only
/// when it is a permission request.
fn notify_command() -> String {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "agent-overlay".into());
    format!("\"{exe}\" --hook-notify")
}

/// Is this hook command one of ours (so it may be replaced)?
///
/// Matching on our own flags rather than on the exe path means a renamed or
/// relocated binary is still recognised. The `127.0.0.1:8377` clause retires
/// the older hand-merged `curl` payload from the README, which is what most
/// existing users have and is broken on Windows.
fn is_ours(cmd: &str) -> bool {
    cmd.contains("--hook-event") || cmd.contains("--hook-notify") || cmd.contains("127.0.0.1:8377")
}

/// One Claude/codex-shaped hook entry: `{"hooks":[{"type":"command",…}]}`.
fn entry(command: String) -> Value {
    json!({ "hooks": [{ "type": "command", "command": command }] })
}

/// Does `entry` (an element of an event's array) contain a command of ours?
fn entry_is_ours(v: &Value) -> bool {
    v.get("hooks")
        .and_then(Value::as_array)
        .map(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(is_ours)
            })
        })
        .unwrap_or(false)
}

/// Merge `wanted` (event name → our entry) into the `hooks` object of a
/// settings document, dropping any previous entries of ours. Returns true if
/// the document changed.
fn merge_hooks(root: &mut Map<String, Value>, wanted: &[(&str, Value)], retire: &[&str]) -> bool {
    let before = root.get("hooks").cloned();
    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        // Something unexpected lives there; refuse rather than clobber it.
        return false;
    }
    let hooks = hooks.as_object_mut().expect("checked is_object");
    for (event, ours) in wanted {
        let arr = hooks
            .entry((*event).to_string())
            .or_insert_with(|| json!([]));
        if !arr.is_array() {
            *arr = json!([]);
        }
        let arr = arr.as_array_mut().expect("checked is_array");
        arr.retain(|e| !entry_is_ours(e));
        arr.push(ours.clone());
    }
    // Events we used to install into: drop our entries so an upgrade doesn't
    // leave them behind, and take the key with them if nothing else is there.
    // This runs after the merge above, so an event in both lists would have the
    // entry we just wrote deleted again and rewrite the file on every install.
    debug_assert!(
        !retire.iter().any(|r| wanted.iter().any(|(w, _)| w == r)),
        "an event cannot be both wanted and retired"
    );
    for event in retire {
        let Some(arr) = hooks.get_mut(*event).and_then(Value::as_array_mut) else {
            continue;
        };
        arr.retain(|e| !entry_is_ours(e));
        if arr.is_empty() {
            hooks.remove(*event);
        }
    }
    Some(&*hooks) != before.as_ref().and_then(Value::as_object)
}

/// Are all of `wanted`'s events already served by an entry of ours whose
/// command matches exactly? (Exact match is what makes an upgrade with a new
/// exe path register as "outdated" rather than "installed".)
fn hooks_current(root: &Map<String, Value>, wanted: &[(&str, Value)], retire: &[&str]) -> bool {
    let Some(hooks) = root.get("hooks").and_then(Value::as_object) else {
        return false;
    };
    let leftovers = retire.iter().any(|event| {
        hooks
            .get(*event)
            .and_then(Value::as_array)
            .is_some_and(|arr| arr.iter().any(entry_is_ours))
    });
    !leftovers
        && wanted.iter().all(|(event, ours)| {
            hooks
                .get(*event)
                .and_then(Value::as_array)
                .is_some_and(|arr| arr.contains(ours))
        })
}

/// Any of ours present at all, current or not — including under events we have
/// since retired, so removing those still reports as an update rather than a
/// fresh install.
fn hooks_present(root: &Map<String, Value>, wanted: &[(&str, Value)], retire: &[&str]) -> bool {
    root.get("hooks")
        .and_then(Value::as_object)
        .is_some_and(|hooks| {
            let events = wanted.iter().map(|(e, _)| *e).chain(retire.iter().copied());
            events.into_iter().any(|event| {
                hooks
                    .get(event)
                    .and_then(Value::as_array)
                    .is_some_and(|arr| arr.iter().any(entry_is_ours))
            })
        })
}

fn read_json_object(path: &Path) -> Result<Map<String, Value>, String> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        Ok(s) if s.trim().is_empty() => Ok(Map::new()),
        Ok(s) => match serde_json::from_str::<Value>(&s) {
            Ok(Value::Object(m)) => Ok(m),
            Ok(_) => Err(format!("{} is not a JSON object", path.display())),
            Err(e) => Err(format!("{} is not valid JSON ({e})", path.display())),
        },
    }
}

/// Write JSON, keeping a one-time `.bak` of the original. These are files the
/// user owns and did not ask us to edit, so a rollback must always exist.
fn write_json(path: &Path, doc: &Map<String, Value>) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    if path.exists() {
        let bak = path.with_extension("json.agent-overlay.bak");
        if !bak.exists() {
            let _ = std::fs::copy(path, &bak);
        }
    }
    let body = serde_json::to_string_pretty(doc).map_err(|e| e.to_string())?;
    std::fs::write(path, body + "\n").map_err(|e| format!("cannot write {}: {e}", path.display()))
}

// ── per-CLI definitions ─────────────────────────────────────────────

fn claude_path() -> Option<PathBuf> {
    Some(home()?.join(".claude").join("settings.json"))
}

/// Only `Notification`. Claude's running/idle come from its own per-pid session
/// file, which is authoritative and can tell two sessions in one folder apart —
/// so `discover_sessions` discards hook-reported running/idle for claude, and
/// installing `UserPromptSubmit`/`PreToolUse`/`Stop` meant spawning a process
/// on every prompt and every tool call to produce a value that was always
/// dropped. Approval is the one state that file can't report.
fn claude_wanted() -> Vec<(&'static str, Value)> {
    vec![("Notification", entry(notify_command()))]
}

/// Events earlier versions installed into. Our entries there are removed on
/// the next install rather than left as litter in the user's settings.
const CLAUDE_RETIRED: &[&str] = &["UserPromptSubmit", "PreToolUse", "Stop"];

fn codex_path() -> Option<PathBuf> {
    Some(home()?.join(".codex").join("hooks").join("hooks.json"))
}

/// codex 0.144+ ships a Claude-shaped hooks system. `permission_request` is
/// the exact approval signal; there is no turn-end event in its set, so idle
/// comes from the running override expiring and the scraper taking back over.
///
/// The event names and file layout are read off the shipped binary rather than
/// public docs, so treat the schema as provisional: if a future codex renames
/// these, the hooks simply never fire and the overlay degrades to scraping.
fn codex_wanted() -> Vec<(&'static str, Value)> {
    vec![
        ("user_prompt_submit", entry(event_command("running"))),
        ("pre_tool_use", entry(event_command("running"))),
        ("permission_request", entry(event_command("permission"))),
    ]
}

fn opencode_path() -> Option<PathBuf> {
    Some(
        home()?
            .join(".config")
            .join("opencode")
            .join("plugin")
            .join("agent-overlay.ts"),
    )
}

fn pi_path() -> Option<PathBuf> {
    Some(
        home()?
            .join(".pi")
            .join("agent")
            .join("extensions")
            .join("agent-overlay.ts"),
    )
}

/// A CLI counts as present if it left a config directory behind or is on PATH.
/// Config dir first: it is the thing we are about to write into.
fn cli_present(config_dir: Option<PathBuf>, bin: &str) -> bool {
    config_dir.is_some_and(|d| d.exists()) || on_path(bin)
}

fn on_path(bin: &str) -> bool {
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p).any(|dir| {
                exts.iter()
                    .any(|ext| dir.join(format!("{bin}{ext}")).exists())
            })
        })
        .unwrap_or(false)
}

/// A file payload (opencode/pi) is "installed" when its content is byte-equal
/// to what we ship, and "outdated" when it is ours but differs.
fn file_state(path: &Path, payload: &str) -> (bool, bool) {
    match std::fs::read_to_string(path) {
        Ok(s) if s == payload => (true, false),
        Ok(s) if s.contains("agent-overlay hooks v") || s.contains("127.0.0.1:8377") => {
            (false, true)
        }
        _ => (false, false),
    }
}

fn install_file(path: &Path, payload: &str) -> Result<&'static str, String> {
    let (current, _) = file_state(path, payload);
    if current {
        return Ok("unchanged");
    }
    let existed = path.exists();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::write(path, payload).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(if existed { "updated" } else { "installed" })
}

fn install_json(
    path: &Path,
    wanted: &[(&str, Value)],
    retire: &[&str],
) -> Result<&'static str, String> {
    let mut doc = read_json_object(path)?;
    if hooks_current(&doc, wanted, retire) {
        return Ok("unchanged");
    }
    let existed = hooks_present(&doc, wanted, retire);
    if !merge_hooks(&mut doc, wanted, retire) && !existed {
        return Err("`hooks` in this file is not an object; left untouched".into());
    }
    write_json(path, &doc)?;
    Ok(if existed { "updated" } else { "installed" })
}

// ── public API ──────────────────────────────────────────────────────

/// Current hook state for every supported CLI, for the settings UI.
pub fn status() -> Vec<CliHooks> {
    let mut out = Vec::new();

    // claude
    {
        let path = claude_path();
        let wanted = claude_wanted();
        let doc = path
            .as_deref()
            .and_then(|p| read_json_object(p).ok())
            .unwrap_or_default();
        let installed = hooks_current(&doc, &wanted, CLAUDE_RETIRED);
        out.push(CliHooks {
            id: "claude",
            name: "Claude Code",
            present: cli_present(path.clone().map(|p| p.with_file_name("")), "claude"),
            installed,
            outdated: !installed && hooks_present(&doc, &wanted, CLAUDE_RETIRED),
            path: path.map(|p| p.display().to_string()).unwrap_or_default(),
            exact_approval: true,
            note: "approvals only fire when permission mode isn't auto-accept".into(),
        });
    }

    // codex
    {
        let path = codex_path();
        let wanted = codex_wanted();
        let doc = path
            .as_deref()
            .and_then(|p| read_json_object(p).ok())
            .unwrap_or_default();
        let installed = hooks_current(&doc, &wanted, &[]);
        out.push(CliHooks {
            id: "codex",
            name: "OpenAI Codex CLI",
            present: cli_present(home().map(|h| h.join(".codex")), "codex"),
            installed,
            outdated: !installed && hooks_present(&doc, &wanted, &[]),
            path: path.map(|p| p.display().to_string()).unwrap_or_default(),
            exact_approval: true,
            note: "no turn-end event: idle falls back to scraping".into(),
        });
    }

    // opencode
    {
        let path = opencode_path();
        let (installed, outdated) = path
            .as_deref()
            .map(|p| file_state(p, OPENCODE_PLUGIN))
            .unwrap_or((false, false));
        out.push(CliHooks {
            id: "opencode",
            name: "opencode",
            present: cli_present(
                home().map(|h| h.join(".config").join("opencode")),
                "opencode",
            ),
            installed,
            outdated,
            path: path.map(|p| p.display().to_string()).unwrap_or_default(),
            exact_approval: true,
            note: String::new(),
        });
    }

    // pi
    {
        let path = pi_path();
        let (installed, outdated) = path
            .as_deref()
            .map(|p| file_state(p, PI_EXTENSION))
            .unwrap_or((false, false));
        out.push(CliHooks {
            id: "pi",
            name: "pi",
            present: cli_present(home().map(|h| h.join(".pi")), "pi"),
            installed,
            outdated,
            path: path.map(|p| p.display().to_string()).unwrap_or_default(),
            exact_approval: false,
            note: "pi exposes no approval event: approvals stay scraped".into(),
        });
    }

    out
}

/// Install (or refresh) hooks for every CLI present on this machine.
/// Absent CLIs are skipped, not created — writing a config dir for a CLI the
/// user doesn't have would make it look installed to everything that probes.
pub fn install_all() -> Vec<InstallOutcome> {
    let mut out = Vec::new();
    for cli in status() {
        let done = if !cli.present {
            Err("not installed on this machine".to_string())
        } else {
            match cli.id {
                "claude" => match claude_path() {
                    Some(p) => install_json(&p, &claude_wanted(), CLAUDE_RETIRED),
                    None => Err("no home directory".into()),
                },
                "codex" => match codex_path() {
                    Some(p) => install_json(&p, &codex_wanted(), &[]),
                    None => Err("no home directory".into()),
                },
                "opencode" => match opencode_path() {
                    Some(p) => install_file(&p, OPENCODE_PLUGIN),
                    None => Err("no home directory".into()),
                },
                "pi" => match pi_path() {
                    Some(p) => install_file(&p, PI_EXTENSION),
                    None => Err("no home directory".into()),
                },
                _ => Err("unknown CLI".into()),
            }
        };
        out.push(match done {
            Ok(action) => InstallOutcome {
                id: cli.id,
                name: cli.name,
                action: action.to_string(),
                detail: cli.path,
            },
            Err(e) if !cli.present => InstallOutcome {
                id: cli.id,
                name: cli.name,
                action: "skipped".into(),
                detail: e,
            },
            Err(e) => InstallOutcome {
                id: cli.id,
                name: cli.name,
                action: "failed".into(),
                detail: e,
            },
        });
    }
    out
}

/// Print the outcome of an install to stderr. Used by `--install-hooks`,
/// which is how the Windows installer and scripted setups drive this.
pub fn report(outcomes: &[InstallOutcome]) {
    for o in outcomes {
        eprintln!("{:9} {:18} {}", o.action, o.name, o.detail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("agent-overlay-hooktest-{name}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// The whole point: installing must not disturb hooks the user already has.
    #[test]
    fn merge_preserves_foreign_hooks() {
        let dir = tmp("preserve");
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{
              "model": "opus",
              "hooks": {
                "UserPromptSubmit": [
                  {"hooks":[{"type":"command","command":"python3 recall.py"}]}
                ],
                "SessionEnd": [
                  {"hooks":[{"type":"command","command":"python3 bye.py"}]}
                ],
                "Notification": [
                  {"hooks":[{"type":"command","command":"python3 notify.py"}]}
                ]
              }
            }"#,
        )
        .unwrap();

        assert_eq!(
            install_json(&path, &claude_wanted(), CLAUDE_RETIRED).unwrap(),
            "installed"
        );

        let doc = read_json_object(&path).unwrap();
        assert_eq!(doc["model"], json!("opus"));
        let hooks = &doc["hooks"];
        // Foreign entries survive, in place.
        assert_eq!(
            hooks["SessionEnd"][0]["hooks"][0]["command"],
            json!("python3 bye.py")
        );
        // A foreign entry under an event we have retired is still theirs.
        assert_eq!(
            hooks["UserPromptSubmit"][0]["hooks"][0]["command"],
            json!("python3 recall.py")
        );
        let notif = hooks["Notification"].as_array().unwrap();
        assert_eq!(notif.len(), 2, "ours appended, theirs kept");
        assert_eq!(notif[0]["hooks"][0]["command"], json!("python3 notify.py"));
        assert!(is_ours(notif[1]["hooks"][0]["command"].as_str().unwrap()));
    }

    #[test]
    fn install_is_idempotent() {
        let dir = tmp("idempotent");
        let path = dir.join("settings.json");
        assert_eq!(
            install_json(&path, &claude_wanted(), CLAUDE_RETIRED).unwrap(),
            "installed"
        );
        assert_eq!(
            install_json(&path, &claude_wanted(), CLAUDE_RETIRED).unwrap(),
            "unchanged"
        );
        let doc = read_json_object(&path).unwrap();
        assert_eq!(doc["hooks"]["Notification"].as_array().unwrap().len(), 1);
        assert!(hooks_current(&doc, &claude_wanted(), CLAUDE_RETIRED));
    }

    /// An upgrade moves the exe; the stale entry must be replaced, not doubled.
    #[test]
    fn stale_entries_are_replaced_not_duplicated() {
        let dir = tmp("stale");
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{"hooks":{"Notification":[
                 {"hooks":[{"type":"command","command":"\"/old/path/agent-overlay\" --hook-notify"}]}
               ]}}"#,
        )
        .unwrap();
        let doc = read_json_object(&path).unwrap();
        assert!(hooks_present(&doc, &claude_wanted(), CLAUDE_RETIRED));
        assert!(!hooks_current(&doc, &claude_wanted(), CLAUDE_RETIRED));

        assert_eq!(
            install_json(&path, &claude_wanted(), CLAUDE_RETIRED).unwrap(),
            "updated"
        );
        let doc = read_json_object(&path).unwrap();
        let arr = doc["hooks"]["Notification"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "stale entry replaced");
        assert!(hooks_current(&doc, &claude_wanted(), CLAUDE_RETIRED));
    }

    /// The hand-merged curl payload from the README is ours to retire — it is
    /// broken on Windows and would otherwise double up with the new entry.
    #[test]
    fn legacy_curl_payload_is_recognised_as_ours() {
        let curl = "curl -s -m 2 -X POST http://127.0.0.1:8377/event -d '{}' || true";
        assert!(is_ours(curl));
        let dir = tmp("legacy");
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            format!(r#"{{"hooks":{{"Stop":[{{"hooks":[{{"type":"command","command":"{curl}"}}]}}]}}}}"#),
        )
        .unwrap();
        assert_eq!(
            install_json(&path, &claude_wanted(), CLAUDE_RETIRED).unwrap(),
            "updated"
        );
        let doc = read_json_object(&path).unwrap();
        // Stop is retired, so the legacy payload is removed and the now-empty
        // event goes with it rather than being replaced by one of ours.
        assert!(doc["hooks"].get("Stop").is_none(), "legacy entry left behind");
    }

    /// Earlier versions installed running/idle hooks for claude that
    /// discover_sessions always discarded. Upgrading must take our own entries
    /// back out rather than leave them running a process per tool call.
    #[test]
    fn retired_claude_hooks_are_removed_on_upgrade() {
        let dir = tmp("retire");
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{"hooks":{
                 "PreToolUse":[
                   {"hooks":[{"type":"command","command":"\"/old/agent-overlay\" --hook-event running"}]},
                   {"hooks":[{"type":"command","command":"python3 audit.py"}]}
                 ],
                 "Stop":[
                   {"hooks":[{"type":"command","command":"\"/old/agent-overlay\" --hook-event idle"}]}
                 ]
               }}"#,
        )
        .unwrap();

        assert_eq!(
            install_json(&path, &claude_wanted(), CLAUDE_RETIRED).unwrap(),
            "updated"
        );
        let doc = read_json_object(&path).unwrap();
        // Ours are gone; Stop held nothing else, so the key goes too.
        assert!(doc["hooks"].get("Stop").is_none());
        // Theirs is untouched, and keeps its event.
        let pre = doc["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0]["hooks"][0]["command"], json!("python3 audit.py"));
        // And a second run is a no-op.
        assert_eq!(
            install_json(&path, &claude_wanted(), CLAUDE_RETIRED).unwrap(),
            "unchanged"
        );
    }

    /// A settings.json we can't parse must be reported, never overwritten.
    #[test]
    fn unparseable_settings_is_left_alone() {
        let dir = tmp("garbage");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(install_json(&path, &claude_wanted(), CLAUDE_RETIRED).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
    }

    #[test]
    fn original_is_backed_up_once() {
        let dir = tmp("backup");
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"model":"opus"}"#).unwrap();
        install_json(&path, &claude_wanted(), CLAUDE_RETIRED).unwrap();
        let bak = path.with_extension("json.agent-overlay.bak");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), r#"{"model":"opus"}"#);

        // A later install must not overwrite the pristine backup.
        std::fs::write(&path, r#"{"model":"changed"}"#).unwrap();
        install_json(&path, &claude_wanted(), CLAUDE_RETIRED).unwrap();
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), r#"{"model":"opus"}"#);
    }

    #[test]
    fn file_payload_install_and_state() {
        let dir = tmp("plugin");
        let path = dir.join("plugin").join("agent-overlay.ts");
        assert_eq!(file_state(&path, OPENCODE_PLUGIN), (false, false));
        assert_eq!(install_file(&path, OPENCODE_PLUGIN).unwrap(), "installed");
        assert_eq!(file_state(&path, OPENCODE_PLUGIN), (true, false));
        assert_eq!(install_file(&path, OPENCODE_PLUGIN).unwrap(), "unchanged");

        // An older version of ours reads as outdated, and gets refreshed.
        std::fs::write(&path, "// agent-overlay hooks v0\n").unwrap();
        assert_eq!(file_state(&path, OPENCODE_PLUGIN), (false, true));
        assert_eq!(install_file(&path, OPENCODE_PLUGIN).unwrap(), "updated");
    }

    /// Quoting is the whole reason command hooks broke on Windows.
    #[test]
    fn event_command_quotes_the_exe_path() {
        let cmd = event_command("permission");
        assert!(cmd.starts_with('"'), "exe path must be quoted: {cmd}");
        assert!(cmd.ends_with("--hook-event permission"));
        assert!(is_ours(&cmd));
    }

    /// Every CLI must be reported, present or not, so the UI can list them.
    #[test]
    fn status_covers_all_supported_clis() {
        let ids: Vec<_> = status().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["claude", "codex", "opencode", "pi"]);
    }
}
