//! Installing the overlay's status hooks into each supported agent CLI.
//!
//! Scraping works everywhere with zero setup, but it can only *guess* at
//! "needs approval" from pane text — and on Windows there are no panes to
//! scrape at all. Hooks are exact, so we install them for the user instead of
//! asking them to hand-merge JSON:
//!
//! | CLI      | target                                          | approval signal     |
//! |----------|-------------------------------------------------|---------------------|
//! | claude   | `~/.claude/settings.json` (merged)              | `PermissionRequest` |
//! | codex    | `~/.codex/hooks.json` (merged)                  | `PermissionRequest` |
//! | opencode | `~/.config/opencode/agent-overlay/tui.ts`, listed in `cli.json` | `permission.asked` |
//! | pi       | `~/.pi/agent/extensions/agent-overlay.ts`       | none (scraped)      |
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
/// 2: the exe path in command hooks is single-quoted on unix, and claude
/// stopped installing `UserPromptSubmit`/`PreToolUse`/`Stop`.
/// 3: `PreToolUse` and `Stop` are back — they are what end a sticky approval,
/// which 2 broke.
/// 4: opencode/pi report their process id, and command hooks repair executable
/// paths accidentally installed with Linux's ` (deleted)` marker.
/// 5: codex hook names use kebab-case, and `stop` reports idle immediately.
/// 6: claude gets a `PermissionRequest` hook, so approvals can be answered
/// from the overlay.
/// 7: codex hooks move to `~/.codex/hooks.json` with PascalCase names, which
/// is where codex reads them, and gain approvals; opencode gets a 2.x TUI
/// plugin in place of the 1.x server plugin, which 2.x no longer loads.
///
/// First run short-circuits on the stamp, so each of those needed a bump to
/// reach anyone already set up.
pub const HOOKS_VERSION: &str = "7";

const OPENCODE_TUI: &str = include_str!("../../hooks/opencode-tui.ts");
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

/// Quote the exe path for the shell the hooks system runs the command in.
///
/// Under `sh -c`, double quotes still expand `$(…)`, `` ` `` and `\`, so a
/// binary under a path someone else chose — an extracted archive, a checkout
/// directory — would run their text on every prompt and tool call. The command
/// is persisted into the user's settings.json, so it outlives the overlay.
/// Single quotes are exact: nothing inside them is special, and an embedded
/// quote is closed, escaped and reopened.
#[cfg(not(windows))]
fn quote_exe(path: &str) -> Result<String, String> {
    Ok(format!("'{}'", path.replace('\'', r"'\''")))
}

/// `cmd.exe` has no equivalent of sh's single quotes, and worse, it expands
/// `%VAR%` *before* it parses quotes and then re-parses the substituted text.
/// The filename can't contain a `"`, but the variable's value can — so
/// `C:\…\%FOO%\agent-overlay.exe` with `FOO` set to `" & calc.exe & "` closes
/// our quoted string and runs a second command, on every prompt and tool call.
/// That is the parse order behind CVE-2024-24576, and there is no escape for
/// `%` on a command line.
///
/// So: keep the double quotes, which do cover the spaces every default install
/// location has and do neutralise `& | < > ^` in the literal path, and refuse
/// outright when the path contains a `%` we cannot make safe.
/// Split out from the `#[cfg(windows)]` path so the decision is testable on
/// any host — the quoting itself can only be exercised on Windows.
#[cfg_attr(not(windows), allow(dead_code))]
fn cmd_would_expand(path: &str) -> bool {
    path.contains('%')
}

#[cfg(windows)]
fn quote_exe(path: &str) -> Result<String, String> {
    if cmd_would_expand(path) {
        return Err(format!(
            "cmd.exe would expand the '%' in {path}; move the binary somewhere \
             without one and install hooks again"
        ));
    }
    Ok(format!("\"{path}\""))
}

/// The command a hook should run to report `status`.
fn event_command(status: &str) -> Result<String, String> {
    Ok(format!("{} --hook-event {status}", quoted_exe()?))
}

fn quoted_exe() -> Result<String, String> {
    let exe = std::env::current_exe()
        .map(|p| executable_path(&p))
        .unwrap_or_else(|_| "agent-overlay".into());
    quote_exe(&exe)
}

/// Linux exposes an unlinked running executable as `/path/app (deleted)`.
/// An upgrade can replace the binary before the old overlay refreshes hooks;
/// persisting that kernel-only suffix produces a command that never exists.
fn executable_path(path: &Path) -> String {
    let displayed = path.display().to_string();
    displayed
        .strip_suffix(" (deleted)")
        .filter(|live| Path::new(live).exists())
        .unwrap_or(&displayed)
        .to_string()
}

/// Claude's `Notification` hook fires for more than approvals, so the decision
/// needs the event payload on stdin. `--hook-notify` reads it and posts only
/// when it is a permission request.
fn notify_command() -> Result<String, String> {
    Ok(format!("{} --hook-notify", quoted_exe()?))
}

/// Is this hook command one of ours (so it may be replaced)?
///
/// Matching on our own flags rather than on the exe path means a renamed or
/// relocated binary is still recognised. The `127.0.0.1:8377` clause retires
/// the older hand-merged `curl` payload from the README, which is what most
/// existing users have and is broken on Windows.
fn is_ours(cmd: &str) -> bool {
    cmd.contains("--hook-event")
        || cmd.contains("--hook-notify")
        || cmd.contains("--hook-permission")
        || cmd.contains("--hook-codex-permission")
        || cmd.contains("127.0.0.1:8377")
}

/// Claude's `PermissionRequest` hook. It blocks until the user answers on the
/// overlay, so it needs a timeout longer than the overlay's own wait
/// (hooks::APPROVAL_WAIT_SECS). Claude's dialog stays usable meanwhile.
fn permission_entry() -> Result<Value, String> {
    Ok(json!({ "hooks": [{
        "type": "command",
        "command": format!("{} --hook-permission", quoted_exe()?),
        "timeout": 600,
    }] }))
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

/// Take our hooks out of an event's array. A group of ours alone goes whole;
/// a group we share with the user's hooks keeps theirs. Returns the index of
/// the first group that held one of ours.
fn strip_ours(arr: &mut Vec<Value>) -> Option<usize> {
    let first = arr.iter().position(entry_is_ours);
    arr.retain_mut(|e| {
        let Some(hs) = e.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        hs.retain(|h| !h.get("command").and_then(Value::as_str).is_some_and(is_ours));
        !hs.is_empty()
    });
    first
}

/// Merge `wanted` (event name → our entry) into the `hooks` object of a
/// settings document, dropping any previous entries of ours. Returns whether
/// the document changed, or an error naming the shape we refused to touch.
fn merge_hooks(
    root: &mut Map<String, Value>,
    wanted: &[(&str, Value)],
    retire: &[&str],
) -> Result<bool, String> {
    let before = root.get("hooks").cloned();
    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        // Something unexpected lives there; refuse rather than clobber it.
        return Err("`hooks` in this file is not an object; left untouched".into());
    };
    // Check every event before touching any of them: refusing halfway through
    // would leave the document merged for the events we already passed.
    for (event, _) in wanted {
        if hooks.get(*event).is_some_and(|v| !v.is_array()) {
            return Err(format!(
                "`hooks.{event}` in this file is not an array; left untouched"
            ));
        }
    }
    for (event, ours) in wanted {
        let arr = hooks
            .entry((*event).to_string())
            .or_insert_with(|| json!([]));
        let arr = arr.as_array_mut().expect("checked above");
        // Replace ours where it stands rather than moving it to the end.
        // Codex trusts each hook by its position in the array, so moving ours
        // would shift the user's own hooks and send them back for review.
        let at = strip_ours(arr).unwrap_or(arr.len());
        arr.insert(at.min(arr.len()), ours.clone());
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
        strip_ours(arr);
        if arr.is_empty() {
            hooks.remove(*event);
        }
    }
    Ok(Some(&*hooks) != before.as_ref().and_then(Value::as_object))
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
            std::fs::copy(path, &bak)
                .map_err(|e| format!("cannot back up {}: {e}", path.display()))?;
        }
    }
    let body = serde_json::to_string_pretty(doc).map_err(|e| e.to_string())?;
    // Write a sibling scratch file and rename it over the target. `fs::write`
    // truncates first, so an interrupted write would leave the user's settings
    // — everything in the file that isn't ours — truncated or half-written,
    // and the one-time .bak above is no help on the second install. rename is
    // atomic within a directory and replaces an existing target.
    //
    // Follow symlinks first: settings.json is often a link into a dotfiles
    // repo, and renaming over the link would replace it with a regular file,
    // leaving the repo copy stale. The pid keeps concurrent installs — first
    // run racing the Windows installer's `--install-hooks` — off each other's
    // scratch file.
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let tmp = target.with_extension(format!("json.agent-overlay.{}.tmp", std::process::id()));
    std::fs::write(&tmp, body + "\n").map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot write {}: {e}", tmp.display())
    })?;
    // rename takes the scratch file's permissions with it, so carry the
    // original's across: a settings.json the user chmod 600'd holds `env` and
    // `apiKeyHelper` and must not come back world-readable.
    if let Ok(meta) = std::fs::metadata(&target) {
        std::fs::set_permissions(&tmp, meta.permissions()).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("cannot set permissions on {}: {e}", tmp.display())
        })?;
    }
    std::fs::rename(&tmp, &target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot replace {}: {e}", target.display())
    })
}

// ── per-CLI definitions ─────────────────────────────────────────────

fn claude_path() -> Option<PathBuf> {
    Some(home()?.join(".claude").join("settings.json"))
}

/// UserPromptSubmit/PreToolUse → running, Notification → permission,
/// Stop → idle.
/// `Notification` reports the approval; `PreToolUse` and `Stop` end it.
/// `PermissionRequest` also reports it, and carries the user's answer back
/// when they decide on the overlay.
///
/// Claude's running/idle come from its own per-pid session file, which is
/// authoritative, so `discover_sessions` throws away hook-reported running/idle
/// for claude — which made these two look like pure overhead. They aren't. A
/// permission entry is sticky for half an hour, and recording *any* newer event
/// for that session is what replaces it, whatever `discover_sessions` then does
/// with the value. So these are not status hooks here, they are the signal that
/// an approval has been answered:
///
/// * `PreToolUse` — the approved tool is running. Clears immediately, which is
///   the common case.
/// * `Stop` — the turn ended. Covers the approval being *denied*, or answered
///   by a turn that runs no further tool, where `PreToolUse` never fires and
///   the card would otherwise sit in Needs Approval for the full 30 minutes.
///
/// `UserPromptSubmit` stays retired: every turn it starts ends in a `Stop`, so
/// it can only clear an entry one of the two above already would.
fn claude_wanted() -> Result<Vec<(&'static str, Value)>, String> {
    Ok(vec![
        ("PreToolUse", entry(event_command("running")?)),
        ("Notification", entry(notify_command()?)),
        ("PermissionRequest", permission_entry()?),
        ("Stop", entry(event_command("idle")?)),
    ])
}

/// Events earlier versions installed into. Our entries there are removed on
/// the next install rather than left as litter in the user's settings.
const CLAUDE_RETIRED: &[&str] = &["UserPromptSubmit"];

/// `$CODEX_HOME`, which codex defaults to `~/.codex`.
fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(home()?.join(".codex")))
}

fn codex_path() -> Option<PathBuf> {
    Some(codex_home()?.join("hooks.json"))
}

/// Where versions before 7 wrote codex hooks. Codex never read this file.
fn codex_legacy_path() -> Option<PathBuf> {
    Some(codex_home()?.join("hooks").join("hooks.json"))
}

/// codex ships a Claude-shaped hooks system, read from `$CODEX_HOME/hooks.json`
/// with Claude's PascalCase event names.
///
/// Codex runs `PermissionRequest` hooks *before* it shows its prompt, so ours
/// must return at once; see `--hook-codex-permission`. `PostToolUse` is what
/// ends an approval answered in the terminal: nothing else fires between the
/// approved tool and the next one.
///
/// Codex asks the user to review each new or changed hook before running it.
/// That is codex's decision to make, so the review is left to them.
fn codex_wanted() -> Result<Vec<(&'static str, Value)>, String> {
    Ok(vec![
        ("UserPromptSubmit", entry(event_command("running")?)),
        ("PreToolUse", entry(event_command("running")?)),
        (
            "PermissionRequest",
            entry(format!("{} --hook-codex-permission", quoted_exe()?)),
        ),
        ("PostToolUse", entry(event_command("running")?)),
        ("Stop", entry(event_command("idle")?)),
    ])
}

/// Names earlier versions wrote. Codex reads none of them.
const CODEX_RETIRED: &[&str] = &[
    "user_prompt_submit",
    "pre_tool_use",
    "permission_request",
    "user-prompt-submit",
    "pre-tool-use",
    "permission-request",
    "stop",
];

/// Take our entries out of the file earlier versions wrote by mistake.
/// Returns whether anything changed. The file goes too once nothing is left.
fn retire_codex_legacy(path: &Path) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let mut doc = read_json_object(path)?;
    let Some(hooks) = doc.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(false);
    };
    let before = hooks.clone();
    for arr in hooks.values_mut().filter_map(Value::as_array_mut) {
        strip_ours(arr);
    }
    hooks.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
    if *hooks == before {
        return Ok(false);
    }
    if hooks.is_empty() && doc.len() == 1 {
        std::fs::remove_file(path).map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
    } else {
        write_json(path, &doc)?;
    }
    Ok(true)
}

fn codex_legacy_present(path: &Path) -> bool {
    read_json_object(path).is_ok_and(|doc| {
        doc.get("hooks")
            .and_then(Value::as_object)
            .is_some_and(|h| h.values().filter_map(Value::as_array).flatten().any(entry_is_ours))
    })
}

/// opencode's config directory: `$XDG_CONFIG_HOME/opencode`, on every OS.
fn opencode_dir() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|c| !c.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(home()?.join(".config")))?;
    Some(config.join("opencode"))
}

/// The plugin directory, and how `cli.json` names it. opencode 2.x loads a
/// TUI plugin from a directory's `tui` module, resolved relative to cli.json.
const OPENCODE_TUI_DIR: &str = "agent-overlay";
const OPENCODE_CLI_ENTRY: &str = "./agent-overlay";

fn opencode_path() -> Option<PathBuf> {
    Some(opencode_dir()?.join(OPENCODE_TUI_DIR).join("tui.ts"))
}

/// Does cli.json load our plugin?
fn opencode_listed(cli: &Path) -> bool {
    read_json_object(cli).is_ok_and(|doc| {
        doc.get("plugins")
            .and_then(Value::as_array)
            .is_some_and(|a| a.iter().any(|p| p.as_str() == Some(OPENCODE_CLI_ENTRY)))
    })
}

/// Add our plugin to cli.json's `plugins`, keeping everything else.
fn list_opencode_plugin(cli: &Path) -> Result<bool, String> {
    if opencode_listed(cli) {
        return Ok(false);
    }
    let mut doc = read_json_object(cli)?;
    if doc.is_empty() {
        doc.insert("$schema".into(), json!("https://opencode.ai/v2/cli.json"));
    }
    let plugins = doc.entry("plugins".to_string()).or_insert_with(|| json!([]));
    let Some(plugins) = plugins.as_array_mut() else {
        return Err(format!("`plugins` in {} is not an array; left untouched", cli.display()));
    };
    plugins.push(json!(OPENCODE_CLI_ENTRY));
    write_json(cli, &doc)?;
    Ok(true)
}

/// The 1.x server plugin earlier versions installed. opencode 2.x refuses to
/// load it and reports a failed plugin on every start.
fn opencode_legacy_path() -> Option<PathBuf> {
    Some(opencode_dir()?.join("plugin").join("agent-overlay.ts"))
}

fn install_opencode(dir: &Path) -> Result<&'static str, String> {
    let plugin = install_file(&dir.join(OPENCODE_TUI_DIR).join("tui.ts"), OPENCODE_TUI)?;
    let listed = list_opencode_plugin(&dir.join("cli.json"))?;
    let retired = retire_file(&dir.join("plugin").join("agent-overlay.ts"))?;
    Ok(match plugin {
        "unchanged" if listed || retired => "updated",
        other => other,
    })
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

fn is_our_file(path: &Path) -> bool {
    file_state(path, "\0").1
}

/// Remove a payload of ours we no longer ship, keeping it as the `.bak` if
/// there isn't one yet. Someone else's file at that path is left alone.
fn retire_file(path: &Path) -> Result<bool, String> {
    if !is_our_file(path) {
        return Ok(false);
    }
    let bak = path.with_extension("ts.agent-overlay.bak");
    let gone = if bak.exists() {
        std::fs::remove_file(path)
    } else {
        std::fs::rename(path, &bak)
    };
    gone.map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
    Ok(true)
}

fn install_file(path: &Path, payload: &str) -> Result<&'static str, String> {
    let (current, ours) = file_state(path, payload);
    if current {
        return Ok("unchanged");
    }
    let existed = path.exists();
    if existed && !ours {
        // Same name, someone else's file. We don't own it, so we don't replace
        // it — this runs unattended on first launch.
        return Err(format!(
            "{} exists and was not written by agent-overlay; left untouched",
            path.display()
        ));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    if existed {
        let bak = path.with_extension("ts.agent-overlay.bak");
        if !bak.exists() {
            std::fs::copy(path, &bak)
                .map_err(|e| format!("cannot back up {}: {e}", path.display()))?;
        }
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
    if !merge_hooks(&mut doc, wanted, retire)? {
        return Ok("unchanged");
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
        let note = match &wanted {
            Ok(_) => "approvals only fire when permission mode isn't auto-accept".to_string(),
            Err(e) => e.clone(),
        };
        // On a refusal there is nothing we could have installed — and an empty
        // `wanted` would make hooks_current vacuously true.
        let refused = wanted.is_err();
        let wanted = wanted.unwrap_or_default();
        let doc = path
            .as_deref()
            .and_then(|p| read_json_object(p).ok())
            .unwrap_or_default();
        let installed = !refused && hooks_current(&doc, &wanted, CLAUDE_RETIRED);
        out.push(CliHooks {
            id: "claude",
            name: "Claude Code",
            present: cli_present(path.clone().map(|p| p.with_file_name("")), "claude"),
            installed,
            outdated: !refused && !installed && hooks_present(&doc, &wanted, CLAUDE_RETIRED),
            path: path.map(|p| p.display().to_string()).unwrap_or_default(),
            exact_approval: true,
            note,
        });
    }

    // codex
    {
        let path = codex_path();
        let wanted = codex_wanted();
        let note = match &wanted {
            Ok(_) => "approvals can be answered from the overlay for codex in tmux; \
                      codex asks you to review changed hooks once"
                .to_string(),
            Err(e) => e.clone(),
        };
        // On a refusal there is nothing we could have installed — and an empty
        // `wanted` would make hooks_current vacuously true.
        let refused = wanted.is_err();
        let wanted = wanted.unwrap_or_default();
        let doc = path
            .as_deref()
            .and_then(|p| read_json_object(p).ok())
            .unwrap_or_default();
        let legacy = codex_legacy_path().is_some_and(|p| codex_legacy_present(&p));
        let installed = !refused && !legacy && hooks_current(&doc, &wanted, CODEX_RETIRED);
        out.push(CliHooks {
            id: "codex",
            name: "OpenAI Codex CLI",
            present: cli_present(codex_home(), "codex"),
            installed,
            outdated: !refused
                && !installed
                && (legacy || hooks_present(&doc, &wanted, CODEX_RETIRED)),
            path: path.map(|p| p.display().to_string()).unwrap_or_default(),
            exact_approval: true,
            note,
        });
    }

    // opencode
    {
        let path = opencode_path();
        let (current, ours) = path
            .as_deref()
            .map(|p| file_state(p, OPENCODE_TUI))
            .unwrap_or((false, false));
        let listed = opencode_dir().is_some_and(|d| opencode_listed(&d.join("cli.json")));
        let legacy = opencode_legacy_path().is_some_and(|p| is_our_file(&p));
        let installed = current && listed && !legacy;
        out.push(CliHooks {
            id: "opencode",
            name: "opencode",
            present: cli_present(opencode_dir(), "opencode"),
            installed,
            outdated: !installed && (ours || current || legacy),
            path: path.map(|p| p.display().to_string()).unwrap_or_default(),
            exact_approval: true,
            note: "needs opencode 2.x".into(),
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
                    Some(p) => claude_wanted().and_then(|w| install_json(&p, &w, CLAUDE_RETIRED)),
                    None => Err("no home directory".into()),
                },
                "codex" => match (codex_path(), codex_legacy_path()) {
                    (Some(p), Some(legacy)) => codex_wanted()
                        .and_then(|w| install_json(&p, &w, CODEX_RETIRED))
                        .and_then(|action| {
                            let retired = retire_codex_legacy(&legacy)?;
                            Ok(if action == "unchanged" && retired { "updated" } else { action })
                        }),
                    _ => Err("no home directory".into()),
                },
                "opencode" => match opencode_dir() {
                    Some(dir) => install_opencode(&dir),
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
            install_json(&path, &claude_wanted().unwrap(), CLAUDE_RETIRED).unwrap(),
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
            install_json(&path, &claude_wanted().unwrap(), CLAUDE_RETIRED).unwrap(),
            "installed"
        );
        assert_eq!(
            install_json(&path, &claude_wanted().unwrap(), CLAUDE_RETIRED).unwrap(),
            "unchanged"
        );
        let doc = read_json_object(&path).unwrap();
        assert_eq!(doc["hooks"]["Notification"].as_array().unwrap().len(), 1);
        assert!(hooks_current(
            &doc,
            &claude_wanted().unwrap(),
            CLAUDE_RETIRED
        ));
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
        assert!(hooks_present(
            &doc,
            &claude_wanted().unwrap(),
            CLAUDE_RETIRED
        ));
        assert!(!hooks_current(
            &doc,
            &claude_wanted().unwrap(),
            CLAUDE_RETIRED
        ));

        assert_eq!(
            install_json(&path, &claude_wanted().unwrap(), CLAUDE_RETIRED).unwrap(),
            "updated"
        );
        let doc = read_json_object(&path).unwrap();
        let arr = doc["hooks"]["Notification"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "stale entry replaced");
        assert!(hooks_current(
            &doc,
            &claude_wanted().unwrap(),
            CLAUDE_RETIRED
        ));
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
            install_json(&path, &claude_wanted().unwrap(), CLAUDE_RETIRED).unwrap(),
            "updated"
        );
        let doc = read_json_object(&path).unwrap();
        let stop = doc["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "legacy entry doubled instead of replaced");
        assert!(is_ours(stop[0]["hooks"][0]["command"].as_str().unwrap()));
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
                 "UserPromptSubmit":[
                   {"hooks":[{"type":"command","command":"\"/old/agent-overlay\" --hook-event running"}]},
                   {"hooks":[{"type":"command","command":"python3 audit.py"}]}
                 ],
                 "SessionEnd":[
                   {"hooks":[{"type":"command","command":"python3 bye.py"}]}
                 ]
               }}"#,
        )
        .unwrap();

        assert_eq!(
            install_json(&path, &claude_wanted().unwrap(), CLAUDE_RETIRED).unwrap(),
            "updated"
        );
        let doc = read_json_object(&path).unwrap();
        // Ours is gone from the retired event; theirs stays, so the key stays.
        let ups = doc["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0]["hooks"][0]["command"], json!("python3 audit.py"));
        // An event we never touch is untouched.
        assert_eq!(
            doc["hooks"]["SessionEnd"][0]["hooks"][0]["command"],
            json!("python3 bye.py")
        );
        // And a second run is a no-op.
        assert_eq!(
            install_json(&path, &claude_wanted().unwrap(), CLAUDE_RETIRED).unwrap(),
            "unchanged"
        );

        // When ours was the only entry there, the event goes with it rather
        // than being left behind empty.
        std::fs::write(
            &path,
            r#"{"hooks":{"UserPromptSubmit":[
                 {"hooks":[{"type":"command","command":"\"/old/agent-overlay\" --hook-event running"}]}
               ]}}"#,
        )
        .unwrap();
        install_json(&path, &claude_wanted().unwrap(), CLAUDE_RETIRED).unwrap();
        let doc = read_json_object(&path).unwrap();
        assert!(doc["hooks"].get("UserPromptSubmit").is_none());
    }

    /// A settings.json we can't parse must be reported, never overwritten.
    #[test]
    fn unparseable_settings_is_left_alone() {
        let dir = tmp("garbage");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(install_json(&path, &claude_wanted().unwrap(), CLAUDE_RETIRED).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
    }

    /// An event key holding something other than an array is a shape we don't
    /// understand — a hand-written entry, or a schema we haven't seen. Refuse
    /// it the way a non-object `hooks` is refused, rather than dropping it.
    #[test]
    fn non_array_event_is_refused_not_clobbered() {
        let dir = tmp("nonarray");
        let path = dir.join("settings.json");
        let original = r#"{"hooks":{"Notification":{"matcher":"Bash","hooks":[{"type":"command","command":"audit.sh"}]}}}"#;
        std::fs::write(&path, original).unwrap();

        assert!(install_json(&path, &claude_wanted().unwrap(), CLAUDE_RETIRED).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    /// A bad shape under a *later* event still refuses the whole file — the
    /// events before it must not be written on their own. (The pre-validation
    /// pass in `merge_hooks` is what guarantees this; today `install_json`
    /// would also drop the half-merged document on the `?`, so this pins the
    /// behaviour against a future refactor that writes incrementally.)
    ///
    /// Uses the codex set: claude installs a single event now, so it can't
    /// express "a later one".
    #[test]
    fn a_bad_later_event_refuses_the_whole_file() {
        let dir = tmp("partial");
        let path = dir.join("hooks.json");
        // UserPromptSubmit is merged before PermissionRequest is reached.
        let original = r#"{"hooks":{"UserPromptSubmit":[],"PermissionRequest":"nope"}}"#;
        std::fs::write(&path, original).unwrap();

        assert!(install_json(&path, &codex_wanted().unwrap(), CODEX_RETIRED).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn codex_hooks_use_pascal_case_names_and_retire_old_ones() {
        let dir = tmp("codex-events");
        let path = dir.join("hooks.json");
        std::fs::write(
            &path,
            r#"{"hooks":{"user_prompt_submit":[{"hooks":[{"type":"command","command":"agent-overlay --hook-event running"}]}],
                         "permission-request":[{"hooks":[{"type":"command","command":"agent-overlay --hook-event permission"}]}]}}"#,
        )
        .unwrap();

        install_json(&path, &codex_wanted().unwrap(), CODEX_RETIRED).unwrap();
        let doc = read_json_object(&path).unwrap();
        let hooks = doc["hooks"].as_object().unwrap();
        let mut events: Vec<_> = hooks.keys().map(String::as_str).collect();
        events.sort();
        assert_eq!(
            events,
            ["PermissionRequest", "PostToolUse", "PreToolUse", "Stop", "UserPromptSubmit"]
        );
        let command = |event: &str| hooks[event][0]["hooks"][0]["command"].as_str().unwrap().to_string();
        assert!(command("Stop").ends_with("--hook-event idle"));
        // Codex waits on this one before showing its prompt: it must be the
        // variant that returns at once, not Claude's blocking one.
        assert!(command("PermissionRequest").ends_with("--hook-codex-permission"));
    }

    /// Codex trusts a hook by its position in the event's array. Updating ours
    /// must leave the user's hooks where they were.
    #[test]
    fn our_entry_is_replaced_in_place() {
        let dir = tmp("in-place");
        let path = dir.join("hooks.json");
        std::fs::write(
            &path,
            r#"{"hooks":{"UserPromptSubmit":[
                 {"hooks":[{"type":"command","command":"'/old/agent-overlay' --hook-event running"}]},
                 {"hooks":[{"type":"command","command":"python3 recall.py"}]}
               ]}}"#,
        )
        .unwrap();
        install_json(&path, &codex_wanted().unwrap(), CODEX_RETIRED).unwrap();
        let doc = read_json_object(&path).unwrap();
        let ups = doc["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(ups.len(), 2);
        assert!(entry_is_ours(&ups[0]));
        assert_eq!(ups[1]["hooks"][0]["command"], json!("python3 recall.py"));
    }

    /// The user may list their own hook in the same group as ours. Replacing
    /// ours must keep theirs.
    #[test]
    fn a_hook_sharing_our_group_survives() {
        let dir = tmp("shared-group");
        let path = dir.join("hooks.json");
        std::fs::write(
            &path,
            r#"{"hooks":{"UserPromptSubmit":[{"hooks":[
                 {"type":"command","command":"'/old/agent-overlay' --hook-event running"},
                 {"type":"command","command":"python3 recall.py"}
               ]}],
               "user-prompt-submit":[{"hooks":[
                 {"type":"command","command":"'/old/agent-overlay' --hook-event running"},
                 {"type":"command","command":"python3 other.py"}
               ]}]}}"#,
        )
        .unwrap();
        install_json(&path, &codex_wanted().unwrap(), CODEX_RETIRED).unwrap();
        let doc = read_json_object(&path).unwrap();
        let ups = doc["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(ups.len(), 2);
        assert!(entry_is_ours(&ups[0]));
        assert_eq!(ups[1]["hooks"], json!([{"type":"command","command":"python3 recall.py"}]));
        let retired = doc["hooks"]["user-prompt-submit"].as_array().unwrap();
        assert_eq!(retired[0]["hooks"], json!([{"type":"command","command":"python3 other.py"}]));
        // A second run finds everything current and leaves the file alone.
        assert!(hooks_current(&doc, &codex_wanted().unwrap(), CODEX_RETIRED));
    }

    #[test]
    fn the_misplaced_codex_file_loses_our_hooks() {
        let dir = tmp("codex-legacy");
        let ours_only = dir.join("ours.json");
        std::fs::write(
            &ours_only,
            r#"{"hooks":{"stop":[{"hooks":[{"type":"command","command":"x --hook-event idle"}]}]}}"#,
        )
        .unwrap();
        assert!(codex_legacy_present(&ours_only));
        assert!(retire_codex_legacy(&ours_only).unwrap());
        assert!(!ours_only.exists(), "a file left with nothing in it");

        let shared = dir.join("shared.json");
        std::fs::write(
            &shared,
            r#"{"hooks":{"stop":[{"hooks":[{"type":"command","command":"x --hook-event idle"}]},
                                 {"hooks":[{"type":"command","command":"notify.sh"}]}]}}"#,
        )
        .unwrap();
        assert!(retire_codex_legacy(&shared).unwrap());
        let doc = read_json_object(&shared).unwrap();
        assert_eq!(doc["hooks"]["stop"].as_array().unwrap().len(), 1);
        assert!(!retire_codex_legacy(&shared).unwrap(), "second run changed it");
        assert!(!retire_codex_legacy(&dir.join("missing.json")).unwrap());
    }

    #[test]
    fn opencode_gets_a_tui_plugin_listed_in_cli_json() {
        let dir = tmp("opencode");
        let cli = dir.join("cli.json");
        std::fs::write(&cli, r#"{"$schema":"x","plugins":["./herdr-tui-session.js"]}"#).unwrap();
        let legacy = dir.join("plugin").join("agent-overlay.ts");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "// agent-overlay hooks v1\n").unwrap();

        assert_eq!(install_opencode(&dir).unwrap(), "installed");
        assert_eq!(
            std::fs::read_to_string(dir.join("agent-overlay").join("tui.ts")).unwrap(),
            OPENCODE_TUI
        );
        let doc = read_json_object(&cli).unwrap();
        assert_eq!(doc["plugins"], json!(["./herdr-tui-session.js", "./agent-overlay"]));
        // The 1.x plugin that 2.x refuses to load is gone, kept as the backup.
        assert!(!legacy.exists());
        assert!(legacy.with_extension("ts.agent-overlay.bak").exists());

        assert_eq!(install_opencode(&dir).unwrap(), "unchanged");
        assert_eq!(read_json_object(&cli).unwrap()["plugins"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn opencode_without_cli_json_gets_one() {
        let dir = tmp("opencode-fresh");
        install_opencode(&dir).unwrap();
        let doc = read_json_object(&dir.join("cli.json")).unwrap();
        assert_eq!(doc["plugins"], json!(["./agent-overlay"]));
    }

    #[test]
    fn a_cli_json_we_do_not_understand_is_left_alone() {
        let dir = tmp("opencode-odd");
        let original = r#"{"plugins":{"herdr":true}}"#;
        std::fs::write(dir.join("cli.json"), original).unwrap();
        assert!(install_opencode(&dir).is_err());
        assert_eq!(std::fs::read_to_string(dir.join("cli.json")).unwrap(), original);
    }

    /// Someone else's file at the old plugin path is not ours to retire.
    #[test]
    fn a_foreign_legacy_plugin_is_kept() {
        let dir = tmp("opencode-foreign-legacy");
        let legacy = dir.join("plugin").join("agent-overlay.ts");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "export default {}\n").unwrap();
        install_opencode(&dir).unwrap();
        assert!(legacy.exists());
    }

    #[test]
    fn original_is_backed_up_once() {
        let dir = tmp("backup");
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"model":"opus"}"#).unwrap();
        install_json(&path, &claude_wanted().unwrap(), CLAUDE_RETIRED).unwrap();
        let bak = path.with_extension("json.agent-overlay.bak");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), r#"{"model":"opus"}"#);

        // A later install must not overwrite the pristine backup.
        std::fs::write(&path, r#"{"model":"changed"}"#).unwrap();
        install_json(&path, &claude_wanted().unwrap(), CLAUDE_RETIRED).unwrap();
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), r#"{"model":"opus"}"#);
    }

    /// The backup is the whole reason we dare touch these files. If it can't be
    /// made, the edit must not happen either.
    ///
    /// unix-only: a read-only directory is the portable-enough way to make
    /// creating the `.bak` fail while leaving the existing file itself
    /// readable and writable, which is what isolates the backup step.
    #[cfg(unix)]
    #[test]
    fn a_failed_backup_stops_the_write() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tmp("bakfail");
        let path = dir.join("settings.json");
        let original = r#"{"model":"opus"}"#;
        std::fs::write(&path, original).unwrap();

        let writable = std::fs::metadata(&dir).unwrap().permissions();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        // Mode 500 doesn't stop root, and CI often runs as root. Check the
        // premise rather than reporting a failure the mode never caused.
        if std::fs::File::create(dir.join("probe")).is_ok() {
            std::fs::set_permissions(&dir, writable).unwrap();
            return;
        }
        let result = install_json(&path, &claude_wanted().unwrap(), CLAUDE_RETIRED);
        let after = std::fs::read_to_string(&path).unwrap();
        // Restore before asserting, so a failure doesn't leave an undeletable dir.
        std::fs::set_permissions(&dir, writable).unwrap();

        assert!(result.is_err(), "wrote without a backup: {result:?}");
        assert_eq!(after, original);
    }

    /// The scratch file we rename through is an implementation detail and must
    /// not be left lying next to the user's config.
    #[test]
    fn writing_leaves_no_scratch_file_behind() {
        let dir = tmp("notemp");
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"model":"opus"}"#).unwrap();
        install_json(&path, &claude_wanted().unwrap(), CLAUDE_RETIRED).unwrap();

        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .filter(|n| n.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "left scratch files: {strays:?}");
    }

    /// A settings.json the user chmod 600'd holds `env` and `apiKeyHelper`.
    /// Replacing it must not widen it to the default 644.
    #[cfg(unix)]
    #[test]
    fn the_original_permissions_survive() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tmp("perms");
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"model":"opus"}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        install_json(&path, &claude_wanted().unwrap(), CLAUDE_RETIRED).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "permissions widened to {mode:o}");
    }

    /// settings.json is commonly a symlink into a dotfiles repo. Writing must
    /// go through the link, not replace it with a regular file.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_is_written_through() {
        let dir = tmp("symlink");
        let real = dir.join("dotfiles-settings.json");
        let link = dir.join("settings.json");
        std::fs::write(&real, r#"{"model":"opus"}"#).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        install_json(&link, &claude_wanted().unwrap(), CLAUDE_RETIRED).unwrap();

        assert!(
            std::fs::symlink_metadata(&link).unwrap().is_symlink(),
            "the symlink was replaced by a regular file"
        );
        let doc = read_json_object(&real).unwrap();
        assert_eq!(doc["model"], json!("opus"));
        assert!(
            hooks_current(&doc, &claude_wanted().unwrap(), CLAUDE_RETIRED),
            "repo copy went stale"
        );
    }

    #[test]
    fn file_payload_install_and_state() {
        let dir = tmp("plugin");
        let path = dir.join("plugin").join("agent-overlay.ts");
        assert_eq!(file_state(&path, OPENCODE_TUI), (false, false));
        assert_eq!(install_file(&path, OPENCODE_TUI).unwrap(), "installed");
        assert_eq!(file_state(&path, OPENCODE_TUI), (true, false));
        assert_eq!(install_file(&path, OPENCODE_TUI).unwrap(), "unchanged");

        // An older version of ours reads as outdated, and gets refreshed.
        std::fs::write(&path, "// agent-overlay hooks v0\n").unwrap();
        assert_eq!(file_state(&path, OPENCODE_TUI), (false, true));
        assert_eq!(install_file(&path, OPENCODE_TUI).unwrap(), "updated");
    }

    /// Same filename, someone else's file. We don't own it, so we don't get to
    /// replace it — this one runs unattended on first launch.
    #[test]
    fn a_foreign_file_is_never_overwritten() {
        let dir = tmp("foreign");
        let path = dir.join("plugin").join("agent-overlay.ts");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let theirs = "export const mine = () => console.log('not ours')\n";
        std::fs::write(&path, theirs).unwrap();

        assert!(install_file(&path, OPENCODE_TUI).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), theirs);
    }

    /// README promises a `.bak` before the first edit for every target, not
    /// just the merged JSON ones.
    #[test]
    fn replacing_our_own_file_backs_it_up_once() {
        let dir = tmp("filebak");
        let path = dir.join("plugin").join("agent-overlay.ts");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let old = "// agent-overlay hooks v0\n";
        std::fs::write(&path, old).unwrap();

        assert_eq!(install_file(&path, OPENCODE_TUI).unwrap(), "updated");
        let bak = path.with_extension("ts.agent-overlay.bak");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), old);

        // A later upgrade must not overwrite the pristine backup.
        std::fs::write(&path, "// agent-overlay hooks v0.5\n").unwrap();
        assert_eq!(install_file(&path, OPENCODE_TUI).unwrap(), "updated");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), old);
    }

    /// Quoting is the whole reason command hooks broke on Windows.
    #[test]
    fn event_command_quotes_the_exe_path() {
        let cmd = event_command("permission").unwrap();
        let quote = if cfg!(windows) { '"' } else { '\'' };
        assert!(cmd.starts_with(quote), "exe path must be quoted: {cmd}");
        assert!(cmd.ends_with("--hook-event permission"));
        assert!(is_ours(&cmd));
    }

    #[test]
    fn linux_deleted_marker_is_not_persisted_as_an_executable_name() {
        let live = std::env::current_exe().unwrap();
        let marked = PathBuf::from(format!("{} (deleted)", live.display()));
        assert_eq!(executable_path(&marked), live.display().to_string());

        // A real filename with that suffix is left alone.
        let real = tmp("deleted-name").join("tool (deleted)");
        std::fs::write(&real, "").unwrap();
        assert_eq!(executable_path(&real), real.display().to_string());
    }

    /// The Windows refusal can only be exercised on Windows, but the decision
    /// behind it is plain string logic and worth pinning anywhere. `%` is legal
    /// in a filename, and cmd.exe substitutes before it parses quotes, so a
    /// variable whose *value* holds a quote escapes the command entirely.
    #[test]
    fn a_percent_in_the_path_is_refused_on_windows() {
        assert!(cmd_would_expand(r"C:\Users\x\%FOO%\agent-overlay.exe"));
        assert!(cmd_would_expand("C:/100%/agent-overlay.exe"));
        assert!(!cmd_would_expand(
            r"C:\Program Files\agent-overlay\agent-overlay.exe"
        ));
        assert!(!cmd_would_expand(
            r"C:\Users\Ann O'Hara\AppData\Local\agent-overlay.exe"
        ));
    }

    /// The quoting has to hold against a real shell, not just look right:
    /// these hook commands are run by `sh -c` and persisted into the user's
    /// settings.json, so a path that expands is a path that executes.
    #[cfg(unix)]
    #[test]
    fn the_exe_path_reaches_sh_verbatim() {
        for path in [
            "/tmp/plain/agent-overlay",
            "/tmp/with space/agent-overlay",
            "/tmp/$(id -u)/agent-overlay",
            "/tmp/`id -u`/agent-overlay",
            "/tmp/$HOME/agent-overlay",
            "/tmp/it's/agent-overlay",
            r"/tmp/back\slash/agent-overlay",
            "/tmp/semi;colon&amp/agent-overlay",
        ] {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {}", quote_exe(path).unwrap()))
                .output()
                .expect("sh");
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                path,
                "sh mangled the path"
            );
        }
    }

    /// Every CLI must be reported, present or not, so the UI can list them.
    #[test]
    fn status_covers_all_supported_clis() {
        let ids: Vec<_> = status().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["claude", "codex", "opencode", "pi"]);
    }
}
