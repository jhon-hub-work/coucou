//! boo-hook — the relay Claude Code runs on every hook event.
//!
//! Reads the hook JSON on stdin, adds a little terminal context, and hands it to
//! Boo over the named pipe `\\.\pipe\boo-<sid>` (Windows) or the Unix
//! socket `$XDG_RUNTIME_DIR/boo.sock` (Linux).
//!
//! Hard rule (docs/CLAUDE.md): **never block Claude Code.**
//! * If the pipe does not exist — Boo is closed — we exit 0 immediately with
//!   nothing on stdout, and the session carries on untouched.
//! * Every step runs under a deadline enforced by the main thread, so a pipe that
//!   accepts the connection and then stops reading cannot wedge the session
//!   either: we abandon the worker and exit.
//! * Only `PermissionRequest` waits for an answer, because approving from the
//!   island is the whole point. No answer means empty stdout, and Claude Code
//!   asks in the terminal exactly as if Boo were not installed.
//!
//! Usage: `boo-hook [--agent <name>] <EventName>` (the name is also read from the JSON).

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::Duration;

/// Budget for getting a pipe connection. Beyond this Claude Code wins, always.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
/// Whole-run budget for an event nobody waits on: connect and write, no more.
const FIRE_AND_FORGET_BUDGET: Duration = Duration::from_secs(2);
/// How long a permission prompt may stay on screen before the terminal takes over.
const DECISION_BUDGET: Duration = Duration::from_secs(110);

/// Fields that are pointless to forward and can be enormous (a whole file read,
/// a full command output). The island never shows them.
const DROPPED_FIELDS: &[&str] = &["tool_response", "transcript_path"];
/// Longest string forwarded for any single field; the island truncates to far
/// less than this anyway.
const MAX_FIELD_LEN: usize = 2_000;

#[cfg(windows)]
mod win;
#[cfg(windows)]
use win::connect;

#[cfg(target_os = "linux")]
mod unix;
#[cfg(target_os = "linux")]
use unix::connect;

fn main() {
    let Some((payload, event)) = read_event() else { std::process::exit(0) };

    let waits_for_answer = event == "PermissionRequest";
    let budget = if waits_for_answer { DECISION_BUDGET } else { FIRE_AND_FORGET_BUDGET };

    // The worker owns every blocking call. If it overruns the budget we simply
    // stop listening and exit: the process dying takes the pipe handle with it.
    // (No catch_unwind here — the release profile is panic = "abort", so it would
    // be dead code. `talk` is written to have nothing to panic on instead.)
    let (tx, rx) = mpsc::channel::<Option<String>>();
    std::thread::spawn(move || {
        let _ = tx.send(talk(&payload, waits_for_answer));
    });

    if let Ok(Some(decision)) = rx.recv_timeout(budget) {
        if let Some(json) = decision_json(&decision) {
            let mut out = std::io::stdout();
            let _ = writeln!(out, "{json}");
            let _ = out.flush();
        }
    }
    // Nothing printed: Claude Code asks in the terminal, as if we were not here.
    std::process::exit(0);
}

/// The documented PermissionRequest output. Anything we do not recognise prints
/// nothing at all rather than guessing — silence is the safe answer.
/// See https://code.claude.com/docs/en/hooks
fn decision_json(decision: &str) -> Option<String> {
    let behavior = match decision.trim() {
        // "always" still answers a plain allow; remembering it is the island's
        // business, not Claude Code's.
        "allow" | "always" => r#"{"behavior":"allow"}"#.to_string(),
        "deny" => r#"{"behavior":"deny","message":"Denied from Boo"}"#.to_string(),
        _ => return None,
    };
    Some(format!(
        r#"{{"hookSpecificOutput":{{"hookEventName":"PermissionRequest","decision":{behavior}}}}}"#
    ))
}

/// `boo-hook.exe [--agent <name>] [<EventName>]`
fn parse_args() -> (String, String) {
    let mut agent = String::new();
    let mut arg_event = String::new();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        if arg == "--agent" {
            agent = it.next().unwrap_or_default();
        } else if arg_event.is_empty() {
            arg_event = arg;
        }
    }
    (agent, arg_event)
}

/// jcode does not pipe a JSON document: its hooks describe the event in
/// `JCODE_HOOK_*` environment variables (docs/HOOKS.md). Translate them into the
/// Claude-Code-shaped object the island already understands. stdin is never read
/// here: an observer hook may inherit a console, and waiting on it would stall
/// jcode's own `pre_tool` gate for its whole timeout.
fn jcode_payload(event: &str, env: &dyn Fn(&str) -> String) -> serde_json::Value {
    use serde_json::{json, Value};
    let failed = env("JCODE_HOOK_STATUS") == "error";
    let event = match (event, failed) {
        ("Stop", true) => "StopFailure",
        ("PostToolUse", true) => "PostToolUseFailure",
        (e, _) => e,
    };
    let mut map = serde_json::Map::new();
    map.insert("session_id".into(), json!(env("JCODE_HOOK_SESSION_ID")));
    map.insert("cwd".into(), json!(env("JCODE_HOOK_CWD")));
    map.insert("hook_event_name".into(), json!(event));
    let tool = env("JCODE_HOOK_TOOL_NAME");
    if !tool.is_empty() {
        // jcode names tools in lower case (`bash`, `edit`); the island's labels
        // are keyed the Claude Code way.
        let mut chars = tool.chars();
        let name: String = chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default();
        map.insert("tool_name".into(), json!(name));
        if let Ok(input @ Value::Object(_)) = serde_json::from_str::<Value>(&env("JCODE_HOOK_TOOL_INPUT")) {
            map.insert("tool_input".into(), input);
        }
    }
    let text = if failed { env("JCODE_HOOK_ERROR") } else { env("JCODE_HOOK_LAST_ASSISTANT_TEXT") };
    if !text.is_empty() {
        map.insert("message".into(), json!(text));
    }
    Value::Object(map)
}

/// Reads stdin and returns the payload to forward plus the event name.
fn read_event() -> Option<(String, String)> {
    // --agent tags the payload with boo_agent so the app routes to the right pill.
    // Absent or invalid names are validated and discarded by the app, not here.
    let (agent, arg_event) = parse_args();

    let mut payload = if agent == "jcode" {
        jcode_payload(&arg_event, &|k| std::env::var(k).unwrap_or_default())
    } else {
        let mut raw = Vec::new();
        if std::io::stdin().read_to_end(&mut raw).is_err() || raw.is_empty() {
            return None;
        }
        // Some shells hand us a UTF-8 BOM; serde_json would choke on it.
        if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
            raw.drain(..3);
        }
        serde_json::from_slice::<serde_json::Value>(&raw).ok()?
    };
    let map = payload.as_object_mut()?;

    // Which agent this hook was installed for. Absent means Claude Code,
    // so existing hook commands keep working unchanged.
    if !agent.is_empty() {
        map.insert("boo_agent".into(), serde_json::Value::String(agent));
    }
    let event = map
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or(arg_event);
    map.insert("hook_event_name".into(), serde_json::Value::String(event.clone()));

    for field in DROPPED_FIELDS {
        map.remove(*field);
    }

    let cwd_missing = map
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(str::is_empty)
        .unwrap_or(true);
    if cwd_missing {
        if let Ok(cwd) = std::env::current_dir() {
            map.insert(
                "cwd".into(),
                serde_json::Value::String(cwd.to_string_lossy().to_string()),
            );
        }
    }

    // Which terminal the session runs in. Unlike macOS, Boo here accepts
    // events from every terminal, so this is context only — never a filter.
    for (key, var) in [
        ("term_program", "TERM_PROGRAM"),
        ("wt_session", "WT_SESSION"),
        ("term_session_id", "TERM_SESSION_ID"),
        ("vscode_pid", "VSCODE_PID"),
        ("session_pid", "CLAUDE_CODE_SSE_PORT"),
    ] {
        if !map.contains_key(key) {
            let value = std::env::var(var).unwrap_or_default();
            map.insert(key.into(), serde_json::Value::String(value));
        }
    }

    // Where the agent's window lives — read by Boo's ↗ "Show window" button.
    #[cfg(windows)]
    map.insert("ancestor_pids".into(), serde_json::json!(win::ancestor_pids()));

    truncate_strings(&mut payload);

    let mut line = payload.to_string();
    line.push('\n');
    Some((line, event))
}

/// Caps every string in the payload. A single Write can carry a whole file.
fn truncate_strings(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            if s.len() > MAX_FIELD_LEN {
                // Cut on a char boundary; a lone byte index can split UTF-8.
                let mut end = MAX_FIELD_LEN;
                while end > 0 && !s.is_char_boundary(end) {
                    end -= 1;
                }
                s.truncate(end);
                s.push('…');
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(truncate_strings),
        serde_json::Value::Object(map) => map.values_mut().for_each(truncate_strings),
        _ => {}
    }
}

/// Connect, send, and — for a permission request — wait for the island's word.
fn talk(payload: &str, waits_for_answer: bool) -> Option<String> {
    let mut pipe = connect()?;

    if pipe.write_all(payload.as_bytes()).is_err() {
        return None;
    }
    let _ = pipe.flush();

    if !waits_for_answer {
        return None;
    }

    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.contains(&b'\n') {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let answer = String::from_utf8_lossy(&buf).trim().to_string();
    (!answer.is_empty()).then_some(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_json_matches_the_documented_shape() {
        assert_eq!(
            decision_json("allow").unwrap(),
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#
        );
        assert_eq!(
            decision_json("deny").unwrap(),
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"Denied from Boo"}}}"#
        );
        // "always" is an island concept; Claude Code just gets an allow.
        assert!(decision_json("always").unwrap().contains(r#""behavior":"allow""#));
    }

    #[test]
    fn anything_unrecognised_prints_nothing() {
        assert!(decision_json("").is_none());
        assert!(decision_json("maybe").is_none());
        // The shape the app used to send must not be mistaken for a decision.
        assert!(decision_json(r#"{"permissionDecision":"allow"}"#).is_none());
    }

    #[test]
    fn jcode_env_becomes_a_claude_shaped_payload() {
        let env = |k: &str| match k {
            "JCODE_HOOK_SESSION_ID" => "ses_1".to_string(),
            "JCODE_HOOK_CWD" => "D:/proj".to_string(),
            "JCODE_HOOK_TOOL_NAME" => "bash".to_string(),
            "JCODE_HOOK_TOOL_INPUT" => r#"{"command":"ls"}"#.to_string(),
            "JCODE_HOOK_STATUS" => "ok".to_string(),
            "JCODE_HOOK_LAST_ASSISTANT_TEXT" => "done".to_string(),
            _ => String::new(),
        };
        let p = jcode_payload("PreToolUse", &env);
        assert_eq!(p["hook_event_name"], "PreToolUse");
        assert_eq!(p["session_id"], "ses_1");
        assert_eq!(p["tool_name"], "Bash");
        assert_eq!(p["tool_input"]["command"], "ls");
        assert_eq!(jcode_payload("Stop", &env)["message"], "done");
    }

    #[test]
    fn a_failed_jcode_turn_or_tool_is_reported_as_a_failure() {
        let env = |k: &str| match k {
            "JCODE_HOOK_STATUS" => "error".to_string(),
            "JCODE_HOOK_ERROR" => "boom".to_string(),
            _ => String::new(),
        };
        assert_eq!(jcode_payload("Stop", &env)["hook_event_name"], "StopFailure");
        assert_eq!(jcode_payload("PostToolUse", &env)["hook_event_name"], "PostToolUseFailure");
        assert_eq!(jcode_payload("SessionEnd", &env)["hook_event_name"], "SessionEnd");
    }

    #[test]
    fn long_strings_are_cut_on_a_char_boundary() {
        let mut v = serde_json::json!({ "tool_input": { "content": "é".repeat(4000) } });
        truncate_strings(&mut v);
        let s = v["tool_input"]["content"].as_str().unwrap();
        assert!(s.len() <= MAX_FIELD_LEN + 4);
        assert!(s.ends_with('…'));
    }
}
