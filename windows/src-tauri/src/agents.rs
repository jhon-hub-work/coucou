// Hook installation for the agents that are not Claude Code: OpenCode, Codex CLI
// and jcode. Every one of them can tell an outside program what a session is
// doing, each in its own way:
//
//   Codex CLI  ~/.codex/hooks.json         the Claude Code hook format, same events,
//                                          and PermissionRequest can be answered.
//   OpenCode   ~/.config/opencode/plugins/boo.js
//                                          a JS plugin; watch-only (its
//                                          `permission.ask` hook is not called by
//                                          the 1.18 builds, checked on 1.18.33).
//   jcode      ~/.jcode/config.toml [hooks] one command per lifecycle slot, the
//                                          event arrives in JCODE_HOOK_* variables.
//                                          Watch-only: pre_tool can only block.
//
// The safety rules are the same as hooks.rs, to the letter: show the diff, take a
// dated backup, merge without touching anything that is not ours, write only when
// the file still matches what the user was shown, and uninstall removes Boo's
// entries and nothing else. A plugin file somebody else wrote is never replaced.
//
// Everything below `plan` takes explicit bytes and paths so it can be tested
// without touching a home directory.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::hooks::{entry_is_ours, fingerprint, parse_settings, pretty, stamp, unified_diff, write_like, MARKER};
use crate::{platform, settings};

/// Rust-side marker that tells our plugin file from anybody else's.
const PLUGIN_MARKER: &str = "BOO-MANAGED-PLUGIN";

/// Codex hook events and the timeouts (seconds) written to hooks.json.
/// PermissionRequest waits for a human; SessionEnd is capped at 3 s by Codex.
const CODEX_EVENTS: &[(&str, u64)] = &[
    ("SessionStart", 10),
    ("UserPromptSubmit", 10),
    ("PreToolUse", 10),
    ("PostToolUse", 10),
    ("PermissionRequest", 120),
    ("Stop", 10),
    ("SessionEnd", 2),
];

/// jcode's `[hooks]` slots and the island event each one is relayed as.
const JCODE_SLOTS: &[(&str, &str)] = &[
    ("session_start", "SessionStart"),
    ("session_end", "SessionEnd"),
    ("turn_start", "UserPromptSubmit"),
    ("turn_end", "Stop"),
    ("pre_tool", "PreToolUse"),
    ("post_tool", "PostToolUse"),
];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Codex,
    Jcode,
    Opencode,
}

fn kind_of(id: &str) -> Option<Kind> {
    match id {
        "codex" => Some(Kind::Codex),
        "jcode" => Some(Kind::Jcode),
        "opencode" => Some(Kind::Opencode),
        _ => None,
    }
}

// ── Where things live ─────────────────────────────────────────────────────────

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn codex_home() -> PathBuf {
    env_dir("CODEX_HOME").unwrap_or_else(|| platform::home_dir().join(".codex"))
}

fn jcode_home() -> PathBuf {
    env_dir("JCODE_HOME").unwrap_or_else(|| platform::home_dir().join(".jcode"))
}

fn opencode_home() -> PathBuf {
    env_dir("OPENCODE_CONFIG_DIR")
        .or_else(|| env_dir("XDG_CONFIG_HOME").map(|d| d.join("opencode")))
        .unwrap_or_else(|| platform::home_dir().join(".config").join("opencode"))
}

/// The one file Boo edits for this agent.
fn config_path(kind: Kind) -> PathBuf {
    match kind {
        Kind::Codex => codex_home().join("hooks.json"),
        Kind::Jcode => jcode_home().join("config.toml"),
        Kind::Opencode => opencode_home().join("plugins").join("boo.js"),
    }
}

fn agent_home(kind: Kind) -> PathBuf {
    match kind {
        Kind::Codex => codex_home(),
        Kind::Jcode => jcode_home(),
        Kind::Opencode => opencode_home(),
    }
}

/// `"C:/path/boo-hook.exe" --agent <agent> <Event>` — forward slashes, because
/// Codex runs it through cmd.exe and jcode parses it shell-style, where a
/// backslash is an escape.
fn relay_command(exe: &Path, agent: &str, event: &str) -> String {
    let exe = exe.to_string_lossy().replace('\\', "/");
    format!("\"{exe}\" --agent {agent} {event}")
}

// ── Codex: hooks.json ─────────────────────────────────────────────────────────

fn codex_merged(existing: &Value, exe: &Path) -> Value {
    let mut root = existing.as_object().cloned().unwrap_or_default();
    let mut hooks = root.get("hooks").and_then(Value::as_object).cloned().unwrap_or_else(Map::new);
    for (event, timeout) in CODEX_EVENTS {
        let mut list = hooks.get(*event).and_then(Value::as_array).cloned().unwrap_or_default();
        list.retain(|entry| !entry_is_ours(entry));
        list.push(json!({
            "hooks": [{
                "type": "command",
                "command": relay_command(exe, "codex", event),
                "timeout": timeout,
            }]
        }));
        hooks.insert((*event).to_string(), Value::Array(list));
    }
    root.insert("hooks".into(), Value::Object(hooks));
    Value::Object(root)
}

fn codex_without_ours(existing: &Value) -> Value {
    let mut root = existing.as_object().cloned().unwrap_or_default();
    let Some(hooks) = root.get("hooks").and_then(Value::as_object).cloned() else {
        return Value::Object(root);
    };
    let mut out = Map::new();
    for (event, value) in hooks {
        match value.as_array() {
            Some(list) => {
                let kept: Vec<Value> = list.iter().filter(|e| !entry_is_ours(e)).cloned().collect();
                if !kept.is_empty() {
                    out.insert(event, Value::Array(kept));
                }
            }
            None => {
                out.insert(event, value);
            }
        }
    }
    if out.is_empty() {
        root.remove("hooks");
    } else {
        root.insert("hooks".into(), Value::Object(out));
    }
    Value::Object(root)
}

fn codex_installed(existing: &Value) -> bool {
    existing
        .get("hooks")
        .and_then(Value::as_object)
        .map(|h| h.values().filter_map(Value::as_array).flatten().any(entry_is_ours))
        .unwrap_or(false)
}

// ── jcode: [hooks] in config.toml ─────────────────────────────────────────────
//
// A line-level edit rather than a TOML round trip: the user's file is full of
// comments and ordering that a re-serialiser would flatten. Only the flat
// `[hooks]` table jcode documents is supported; anything fancier is refused
// instead of guessed at.

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn eol_of(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

/// The table name a `[header]` line opens, or None for any other line.
fn header_name(line: &str) -> Option<&str> {
    let t = line.trim();
    let inner = t.strip_prefix('[')?;
    let inner = inner.strip_prefix('[').unwrap_or(inner);
    let end = inner.find(']')?;
    Some(inner[..end].trim())
}

fn key_of(line: &str) -> Option<&str> {
    let t = line.trim_start();
    if t.starts_with('#') || t.starts_with('[') {
        return None;
    }
    t.split_once('=').map(|(k, _)| k.trim())
}

fn value_of(line: &str) -> &str {
    line.split_once('=').map(|(_, v)| v.trim()).unwrap_or("")
}

/// `(header line, one past the last line of the table)` of `[hooks]`.
fn hooks_region(lines: &[&str]) -> Result<Option<(usize, usize)>, String> {
    let mut found: Option<usize> = None;
    let mut seen_header = false;
    for (i, line) in lines.iter().enumerate() {
        match header_name(line) {
            Some(name) => {
                seen_header = true;
                if name == "hooks" {
                    if found.is_some() {
                        return Err("config.toml has two [hooks] tables — Boo won't guess which one to edit.".into());
                    }
                    found = Some(i);
                } else if name.starts_with("hooks.") {
                    return Err("config.toml defines [hooks.…] sub-tables, which Boo doesn't edit.".into());
                }
            }
            None if !seen_header => {
                if let Some(k) = key_of(line) {
                    if k == "hooks" || k.starts_with("hooks.") {
                        return Err("config.toml sets hooks as an inline or dotted key, which Boo doesn't edit.".into());
                    }
                }
            }
            None => {}
        }
    }
    Ok(found.map(|start| {
        let end = lines[start + 1..]
            .iter()
            .position(|l| header_name(l).is_some())
            .map(|p| start + 1 + p)
            .unwrap_or(lines.len());
        (start, end)
    }))
}

fn is_empty_value(v: &str) -> bool {
    let v = v.split('#').next().unwrap_or("").trim();
    v == "\"\"" || v == "''"
}

/// The config with Boo's slots filled in, and the slots left alone because
/// something else already uses them (jcode takes one command per slot).
fn jcode_merged(text: &str, exe: &Path) -> Result<(String, Vec<String>), String> {
    let eol = eol_of(text);
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let region = hooks_region(&lines)?;
    let mut skipped = Vec::new();

    let Some((start, end)) = region else {
        let mut out = text.to_string();
        if !out.is_empty() {
            if !out.ends_with('\n') {
                out.push_str(eol);
            }
            out.push_str(eol);
        }
        out.push_str(&format!("[hooks]{eol}"));
        for (key, event) in JCODE_SLOTS {
            out.push_str(&format!("{key} = {}{eol}", toml_string(&relay_command(exe, "jcode", event))));
        }
        return Ok((out, skipped));
    };

    let mut owned: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    let mut inserts: Vec<String> = Vec::new();
    for (key, event) in JCODE_SLOTS {
        let new_line = format!("{key} = {}{eol}", toml_string(&relay_command(exe, "jcode", event)));
        let at = (start + 1..end).find(|&i| key_of(lines[i]) == Some(*key));
        match at {
            Some(i) => {
                let v = value_of(lines[i]);
                if v.contains(MARKER) || is_empty_value(v) {
                    owned[i] = new_line;
                } else {
                    skipped.push((*key).to_string());
                }
            }
            None => inserts.push(new_line),
        }
    }
    if !inserts.is_empty() {
        let last = (start + 1..end).rev().find(|&i| !lines[i].trim().is_empty()).unwrap_or(start);
        if !owned[last].ends_with('\n') {
            owned[last].push_str(eol);
        }
        for (n, line) in inserts.into_iter().enumerate() {
            owned.insert(last + 1 + n, line);
        }
    }
    Ok((owned.concat(), skipped))
}

fn jcode_without_ours(text: &str) -> Result<String, String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let Some((start, end)) = hooks_region(&lines)? else { return Ok(text.to_string()) };
    let ours = |i: usize| {
        key_of(lines[i]).is_some_and(|k| JCODE_SLOTS.iter().any(|(s, _)| *s == k))
            && value_of(lines[i]).contains(MARKER)
    };
    let mut keep: Vec<bool> = (0..lines.len()).map(|i| !(i > start && i < end && ours(i))).collect();
    // A table we emptied is a table we created: take the header, and the blank
    // line we put in front of it, away too.
    if (start + 1..end).all(|i| !keep[i] || lines[i].trim().is_empty()) {
        keep[start] = false;
        for i in start + 1..end {
            keep[i] = false;
        }
        if start > 0 && lines[start - 1].trim().is_empty() {
            keep[start - 1] = false;
        }
    }
    Ok(lines.iter().zip(keep).filter(|(_, k)| *k).map(|(l, _)| *l).collect())
}

fn jcode_installed(text: &str) -> bool {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    match hooks_region(&lines) {
        Ok(Some((start, end))) => (start + 1..end).any(|i| {
            key_of(lines[i]).is_some_and(|k| JCODE_SLOTS.iter().any(|(s, _)| *s == k))
                && value_of(lines[i]).contains(MARKER)
        }),
        _ => false,
    }
}

// ── OpenCode: plugins/boo.js ──────────────────────────────────────────────────

/// The plugin source with the relay's path filled in. It only ever spawns that
/// one program and never waits on it, so a closed Boo costs OpenCode nothing.
fn plugin_source(exe: &Path) -> String {
    let exe = exe.to_string_lossy().replace('\\', "/");
    include_str!("opencode_plugin.js").replace("__BOO_HOOK_EXE__", &serde_json::to_string(&exe).unwrap_or_default())
}

// ── Plans ─────────────────────────────────────────────────────────────────────

struct Plan {
    /// What the user is shown on the left of the diff.
    before: String,
    /// The new bytes; None means the file is removed.
    next: Option<Vec<u8>>,
    after: String,
    notes: Vec<String>,
}

fn utf8(bytes: &[u8], path: &Path) -> Result<String, String> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8(bytes.to_vec()).map_err(|_| format!("{} isn't UTF-8 text — Boo won't touch it.", path.display()))
}

fn plan(kind: Kind, path: &Path, current: Option<&[u8]>, install: bool, exe: &Path) -> Result<Plan, String> {
    let bytes = current.unwrap_or(b"");
    let mut notes = Vec::new();
    match kind {
        Kind::Codex => {
            let value = parse_settings(bytes, &path.display().to_string())?;
            let next = if install { codex_merged(&value, exe) } else { codex_without_ours(&value) };
            if install {
                notes.push("Codex asks you to review new hooks once: open Codex and run /hooks, then trust Boo's entries.".into());
                if value.get("hooks").and_then(|h| h.get("PermissionRequest")).and_then(Value::as_array)
                    .is_some_and(|l| l.iter().any(|e| !entry_is_ours(e)))
                {
                    notes.push("Another PermissionRequest hook is already installed; both will run, and Codex decides which answer wins.".into());
                }
            }
            let empty = next.as_object().is_some_and(Map::is_empty);
            let mut text = pretty(&next);
            text.push('\n');
            Ok(Plan {
                before: pretty(&value),
                after: if empty && !install { String::new() } else { pretty(&next) },
                next: if empty && !install { None } else { Some(text.into_bytes()) },
                notes,
            })
        }
        Kind::Jcode => {
            let text = utf8(bytes, path)?;
            let (next, skipped) = if install {
                jcode_merged(&text, exe)?
            } else {
                (jcode_without_ours(&text)?, Vec::new())
            };
            for slot in skipped {
                notes.push(format!("[hooks] {slot} already runs something else, so Boo leaves it alone (jcode takes one command per slot)."));
            }
            let remove = !install && next.trim().is_empty();
            Ok(Plan {
                before: text,
                after: next.clone(),
                next: if remove { None } else { Some(next.into_bytes()) },
                notes,
            })
        }
        Kind::Opencode => {
            let text = utf8(bytes, path)?;
            let ours = text.contains(PLUGIN_MARKER);
            if current.is_some() && !ours {
                return Err(format!(
                    "{} exists and wasn't written by Boo. Boo won't overwrite or remove it.",
                    path.display()
                ));
            }
            if install {
                let next = plugin_source(exe);
                notes.push("OpenCode loads plugins when it starts: restart running sessions to pick it up.".into());
                Ok(Plan { before: text, after: next.clone(), next: Some(next.into_bytes()), notes })
            } else {
                Ok(Plan { before: text, after: String::new(), next: None, notes })
            }
        }
    }
}

fn is_installed(kind: Kind, bytes: Option<&[u8]>) -> bool {
    let Some(bytes) = bytes else { return false };
    let text = String::from_utf8_lossy(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)).to_string();
    match kind {
        Kind::Codex => serde_json::from_str::<Value>(&text).map(|v| codex_installed(&v)).unwrap_or(false),
        Kind::Jcode => jcode_installed(&text),
        Kind::Opencode => text.contains(PLUGIN_MARKER),
    }
}

/// The only error that means "not there" is the file not being there.
fn load(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Can't read {}: {e}", path.display())),
    }
}

fn fp(current: Option<&[u8]>) -> String {
    fingerprint(current.unwrap_or(b""))
}

fn backup_of(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    path.with_file_name(format!("{name}.bak-{}", stamp()))
}

/// Backs up, then writes beside the target and renames over it (or removes it).
/// Refuses when the file is no longer the one the user was shown.
fn commit(path: &Path, install: bool, kind: Kind, expected: &str, exe: &Path) -> Result<String, String> {
    let current = load(path)?;
    if fp(current.as_deref()) != expected {
        return Err(format!("{} changed since the preview. Nothing was written — review the new diff.", path.display()));
    }
    let plan = plan(kind, path, current.as_deref(), install, exe)?;

    let backup = backup_of(path);
    if current.is_some() {
        std::fs::copy(path, &backup).map_err(|e| format!("backup failed: {e}"))?;
    }
    match plan.next {
        None => {
            if current.is_some() {
                std::fs::remove_file(path).map_err(|e| format!("write failed: {e}"))?;
            }
        }
        Some(bytes) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            let temp = path.with_extension(format!("boo-{}", std::process::id()));
            if let Err(err) = write_like(&temp, path, &bytes) {
                let _ = std::fs::remove_file(&temp);
                return Err(format!("write failed: {err}"));
            }
            if let Err(err) = std::fs::rename(&temp, path) {
                let _ = std::fs::remove_file(&temp);
                return Err(format!("write failed: {err}"));
            }
        }
    }
    Ok(if current.is_some() { backup.to_string_lossy().to_string() } else { String::new() })
}

// ── Public API ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatus {
    pub id: &'static str,
    pub label: &'static str,
    pub installed: bool,
    /// The agent's own config folder exists, i.e. it looks installed on this PC.
    pub available: bool,
    pub config_path: String,
    pub hook_ready: bool,
    /// True when Allow / Deny from the island can answer this agent.
    pub approvals: bool,
    pub summary: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPreview {
    pub diff: String,
    /// Empty when there is nothing to back up (the file does not exist yet).
    pub backup: String,
    pub config_path: String,
    pub fingerprint: String,
    pub notes: Vec<String>,
}

fn describe(id: &'static str) -> Option<AgentStatus> {
    let kind = kind_of(id)?;
    let path = config_path(kind);
    let (label, approvals, summary) = match kind {
        Kind::Codex => ("Codex CLI", true, "Tool calls, questions and permission requests show up in the island, and you can answer permissions there."),
        Kind::Opencode => ("OpenCode", false, "Sessions, tool calls and permission prompts show up in the island. Watch only: answer permissions in OpenCode."),
        Kind::Jcode => ("jcode", false, "Sessions, turns and tool calls show up in the island. Watch only."),
    };
    let installed = is_installed(kind, load(&path).ok().flatten().as_deref());
    Some(AgentStatus {
        id,
        label,
        installed,
        available: agent_home(kind).exists(),
        config_path: path.to_string_lossy().to_string(),
        hook_ready: settings::hook_exe_path().exists(),
        approvals,
        summary,
    })
}

pub fn status() -> Vec<AgentStatus> {
    ["opencode", "codex", "jcode"].into_iter().filter_map(describe).collect()
}

pub fn preview(id: &str, install: bool) -> Result<AgentPreview, String> {
    let kind = kind_of(id).ok_or_else(|| format!("unknown agent {id}"))?;
    let path = config_path(kind);
    let current = load(&path)?;
    let plan = plan(kind, &path, current.as_deref(), install, &settings::hook_exe_path())?;
    Ok(AgentPreview {
        diff: unified_diff(&plan.before, &plan.after),
        backup: if current.is_some() { backup_of(&path).to_string_lossy().to_string() } else { String::new() },
        config_path: path.to_string_lossy().to_string(),
        fingerprint: fp(current.as_deref()),
        notes: plan.notes,
    })
}

pub fn write(id: &str, install: bool, fingerprint: &str) -> Result<String, String> {
    let kind = kind_of(id).ok_or_else(|| format!("unknown agent {id}"))?;
    commit(&config_path(kind), install, kind, fingerprint, &settings::hook_exe_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe() -> PathBuf {
        PathBuf::from(r"C:\Users\x\AppData\Local\Boo\bin\boo-hook.exe")
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from("D:/Temp").join(format!("boo-agents-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ── Codex ────────────────────────────────────────────────────────────────

    #[test]
    fn codex_merge_keeps_foreign_hooks_and_uninstall_restores_the_file() {
        // The shape of a real hooks.json, other tools' hooks included.
        let existing = json!({
            "hooks": {
                "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "gate.exe", "timeout": 20 }] }],
                "PermissionRequest": [{ "hooks": [{ "type": "command", "command": "approve.exe", "timeout": 60 }] }],
                "SessionEnd": [{ "hooks": [{ "type": "command", "command": "stats.exe" }] }]
            }
        });
        let after = codex_merged(&existing, &exe());
        let pre = after["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2, "foreign hook kept, ours added");
        assert!(pre[0]["hooks"][0]["command"] == "gate.exe");
        let ours = pre[1]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(ours, "\"C:/Users/x/AppData/Local/Boo/bin/boo-hook.exe\" --agent codex PreToolUse");
        assert_eq!(after["hooks"]["PermissionRequest"][1]["hooks"][0]["timeout"], 120);
        assert!(codex_installed(&after));

        // Installing twice does not stack entries.
        assert_eq!(codex_merged(&after, &exe()), after);
        // Uninstall puts the original back exactly.
        assert_eq!(codex_without_ours(&after), existing);
    }

    #[test]
    fn codex_plan_warns_about_review_and_a_competing_permission_hook() {
        let existing = br#"{"hooks":{"PermissionRequest":[{"hooks":[{"type":"command","command":"approve.exe"}]}]}}"#;
        let p = plan(Kind::Codex, Path::new("hooks.json"), Some(existing), true, &exe()).unwrap();
        assert!(p.notes.iter().any(|n| n.contains("/hooks")));
        assert!(p.notes.iter().any(|n| n.contains("PermissionRequest")));
        // A hooks.json that Boo created is removed again, not left as `{}`.
        let installed = plan(Kind::Codex, Path::new("hooks.json"), None, true, &exe()).unwrap().next.unwrap();
        let back = plan(Kind::Codex, Path::new("hooks.json"), Some(&installed), false, &exe()).unwrap();
        assert!(back.next.is_none());
    }

    #[test]
    fn unreadable_codex_hooks_are_refused_not_replaced() {
        assert!(plan(Kind::Codex, Path::new("hooks.json"), Some(b"{ nope"), true, &exe()).is_err());
    }

    // ── jcode ────────────────────────────────────────────────────────────────

    const JCODE_CFG: &str = "[server]\nwake_mode = \"internal\"\n\n[terminal]\nspawn_hook = \"cmd.exe /c C:/x.cmd\"\n\n[hooks]\npre_tool_timeout_ms = 5000\n\n[ambient]\nenabled = false\n";

    #[test]
    fn jcode_slots_are_added_inside_hooks_and_removed_again() {
        let (after, skipped) = jcode_merged(JCODE_CFG, &exe()).unwrap();
        assert!(skipped.is_empty());
        assert!(jcode_installed(&after));
        // Still one [hooks] table, still followed by [ambient], user keys untouched.
        assert_eq!(after.matches("[hooks]").count(), 1);
        assert!(after.contains("pre_tool_timeout_ms = 5000\n"));
        assert!(after.contains("turn_end = \"\\\"C:/Users/x/AppData/Local/Boo/bin/boo-hook.exe\\\" --agent jcode Stop\"\n"));
        let hooks_at = after.find("[hooks]").unwrap();
        assert!(after.find("session_start").unwrap() > hooks_at);
        assert!(after.find("session_start").unwrap() < after.find("[ambient]").unwrap());
        // Reinstall is idempotent; uninstall gives the original back byte for byte.
        assert_eq!(jcode_merged(&after, &exe()).unwrap().0, after);
        assert_eq!(jcode_without_ours(&after).unwrap(), JCODE_CFG);
    }

    #[test]
    fn jcode_never_replaces_a_slot_somebody_else_uses() {
        let cfg = "[hooks]\npre_tool = \"~/bin/jcode-tool-policy\"\nturn_end = \"\"\n";
        let (after, skipped) = jcode_merged(cfg, &exe()).unwrap();
        assert_eq!(skipped, vec!["pre_tool".to_string()]);
        assert!(after.contains("pre_tool = \"~/bin/jcode-tool-policy\"\n"));
        assert!(after.contains("--agent jcode Stop"), "an empty slot is free");
        let back = jcode_without_ours(&after).unwrap();
        assert!(back.contains("pre_tool = \"~/bin/jcode-tool-policy\""));
        assert!(!back.contains("boo-hook"));
    }

    #[test]
    fn jcode_without_a_hooks_table_gets_one_appended_and_removed() {
        for original in ["", "[server]\nwake_mode = \"internal\"\n", "[server]\nx = 1"] {
            let (after, _) = jcode_merged(original, &exe()).unwrap();
            assert!(jcode_installed(&after));
            let back = jcode_without_ours(&after).unwrap();
            // A missing final newline is the one thing allowed to change.
            assert_eq!(back.trim_end(), original.trim_end());
        }
        let p = plan(Kind::Jcode, Path::new("config.toml"), None, true, &exe()).unwrap();
        let installed = p.next.unwrap();
        let back = plan(Kind::Jcode, Path::new("config.toml"), Some(&installed), false, &exe()).unwrap();
        assert!(back.next.is_none(), "a config Boo created disappears again");
    }

    #[test]
    fn jcode_refuses_hook_layouts_it_cannot_edit_safely() {
        assert!(jcode_merged("hooks = { turn_end = \"x\" }\n", &exe()).is_err());
        assert!(jcode_merged("[hooks.extra]\na = 1\n", &exe()).is_err());
        assert!(jcode_merged("[hooks]\n[hooks]\n", &exe()).is_err());
    }

    #[test]
    fn jcode_keeps_windows_line_endings() {
        let cfg = "[hooks]\r\npre_tool_timeout_ms = 5000\r\n";
        let (after, _) = jcode_merged(cfg, &exe()).unwrap();
        assert!(!after.replace("\r\n", "").contains('\n'), "no bare LF introduced");
        assert_eq!(jcode_without_ours(&after).unwrap(), cfg);
    }

    // ── OpenCode ─────────────────────────────────────────────────────────────

    #[test]
    fn opencode_plugin_is_ours_to_install_and_remove_but_nobody_elses() {
        let src = plugin_source(&exe());
        assert!(src.contains(PLUGIN_MARKER));
        assert!(src.contains("\"C:/Users/x/AppData/Local/Boo/bin/boo-hook.exe\""));
        assert!(!src.contains("__BOO_HOOK_EXE__"));

        let p = plan(Kind::Opencode, Path::new("boo.js"), None, true, &exe()).unwrap();
        let bytes = p.next.unwrap();
        assert!(is_installed(Kind::Opencode, Some(&bytes)));
        let gone = plan(Kind::Opencode, Path::new("boo.js"), Some(&bytes), false, &exe()).unwrap();
        assert!(gone.next.is_none());

        // A boo.js written by somebody else is neither overwritten nor deleted.
        let theirs = b"export const Mine = async () => ({})";
        assert!(plan(Kind::Opencode, Path::new("boo.js"), Some(theirs), true, &exe()).is_err());
        assert!(plan(Kind::Opencode, Path::new("boo.js"), Some(theirs), false, &exe()).is_err());
    }

    // ── The write path, on real files ────────────────────────────────────────

    #[test]
    fn writing_backs_up_refuses_a_changed_file_and_leaves_foreign_files_alone() {
        let dir = scratch("write");
        let path = dir.join("config.toml");
        std::fs::write(&path, JCODE_CFG).unwrap();

        let current = std::fs::read(&path).unwrap();
        let p = plan(Kind::Jcode, &path, Some(&current), true, &exe()).unwrap();
        assert!(unified_diff(&p.before, &p.after).contains("+ turn_end"));

        // The file moves after the preview: refused, untouched.
        let shown = fp(Some(&current));
        std::fs::write(&path, "[hooks]\n# edited meanwhile\n").unwrap();
        let err = commit(&path, true, Kind::Jcode, &shown, &exe()).unwrap_err();
        assert!(err.contains("changed since the preview"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[hooks]\n# edited meanwhile\n");

        // Fresh preview, install: backup holds the pre-install bytes.
        let before = std::fs::read(&path).unwrap();
        let backup = commit(&path, true, Kind::Jcode, &fp(Some(&before)), &exe()).unwrap();
        assert_eq!(std::fs::read(&backup).unwrap(), before);
        let installed = std::fs::read(&path).unwrap();
        assert!(is_installed(Kind::Jcode, Some(&installed)));
        assert!(String::from_utf8_lossy(&installed).contains("# edited meanwhile"));

        // Uninstall restores the text.
        commit(&path, false, Kind::Jcode, &fp(Some(&installed)), &exe()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[hooks]\n# edited meanwhile\n");

        // OpenCode: new file under a folder that does not exist yet, no backup,
        // then removal (with a backup, because there is something to lose).
        let plugin = dir.join("plugins").join("boo.js");
        assert_eq!(commit(&plugin, true, Kind::Opencode, &fp(None), &exe()).unwrap(), "");
        let written = std::fs::read(&plugin).unwrap();
        let bak = commit(&plugin, false, Kind::Opencode, &fp(Some(&written)), &exe()).unwrap();
        assert!(!plugin.exists());
        assert_eq!(std::fs::read(bak).unwrap(), written);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Prints what installing would change in the real configs on this PC, and
    /// writes nothing: `cargo test real_previews -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_previews() {
        for id in ["codex", "jcode", "opencode"] {
            let p = preview(id, true).unwrap();
            println!("=== {id}: {}
{}
notes: {:?}", p.config_path, p.diff, p.notes);
        }
    }
}
