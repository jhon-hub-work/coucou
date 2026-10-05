// Phone sync — a compact snapshot of the agents and the video board, published to
// a private ntfy topic that 27launch's island on Jhon's phone listens to.
//
// Only names, states and short whitelisted step labels leave the PC: never a
// prompt, a command, a file name or a path (an agent's name is at most its
// project folder name). Off unless switched on in Settings → General → Phone.
// The one exception is a pending permission request: the phone has to see what
// it would allow, so the island card's own line (≤ APPROVAL_TEXT chars) goes too,
// with a one-time code. Anyone who learns the topic name could read those while
// they are pending; the name is 64 random bits (accepted by Jhon, 2026-10-03).
// The phone answers on `<topic>-reply`, which Boo streams only while one is pending.
//
// ntfy.sh allows 250 messages a day per IP on the free tier, shared with anything
// else on this PC that uses ntfy, so a message goes out only when something starts
// or stops waiting on Jhon (see change_key), at most one every MIN_GAP, plus a
// HEARTBEAT that keeps the phone's copy fresh, and never more than DAILY_CAP a day
// (counted in phone-sent.json next to boo.log, so restarts don't reset it).

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;

use crate::log;

pub const MIN_GAP: Duration = Duration::from_secs(2);
/// Under 27launch's BOO_STALE_MS (25 min), so Boo never drops off the phone while
/// the PC is on: 72 a day at most, well inside DAILY_CAP.
pub const HEARTBEAT: Duration = Duration::from_secs(20 * 60);
/// ponytail: fixed share of ntfy's 250/day, so phone sync can never starve other ntfy alerts.
pub const DAILY_CAP: u32 = 150;
/// Snapshots that carry (or clear) something that needs Jhon may go a little past
/// DAILY_CAP: an approval that never reaches the phone is worse than a quiet board.
pub const NEEDS_YOU_CAP: u32 = 200;
/// The longest approval description that leaves the PC (the island card's text, cut).
pub const APPROVAL_TEXT: usize = 120;

/// One pill as the island has it; the step is the raw last step and is filtered here.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentIn {
    pub id: String,
    pub name: String,
    pub color: String,
    pub state: String,
    #[serde(default)]
    pub step: String,
}

enum Update {
    Agents(Vec<AgentIn>),
    Board(Value),
    /// Approvals or usage changed; they live in the statics below.
    Kick,
}

static TX: OnceLock<mpsc::UnboundedSender<Update>> = OnceLock::new();

/// A permission request the phone may answer. `code` is a one-time secret that
/// only ever leaves the PC inside the snapshot, so an answer must have seen it.
#[derive(Debug, Clone, PartialEq)]
pub struct Approval {
    pub id: String,
    pub agent: String,
    pub tool: String,
    pub text: String,
    pub code: String,
    /// Unix seconds; the relay hands the question to the terminal at this point.
    pub expires: i64,
    /// AskUserQuestion's questions, full length. Answered by picks, never by Allow.
    pub questions: Vec<Question>,
}

/// One AskUserQuestion question. The phone sees cut copies and answers by index,
/// so a long label can never be sent back wrong.
#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub question: String,
    pub header: String,
    pub multi: bool,
    pub options: Vec<String>,
}

/// AskUserQuestion's `questions` array → what Boo keeps. Empty when unusable.
pub fn parse_questions(v: &Value) -> Vec<Question> {
    let list: Vec<Question> = v
        .as_array()
        .map(|qs| {
            qs.iter()
                .filter_map(|q| {
                    let options: Vec<String> = q["options"].as_array()?.iter().filter_map(|o| o["label"].as_str().map(String::from)).collect();
                    Some(Question {
                        question: q["question"].as_str().filter(|s| !s.is_empty())?.to_string(),
                        header: q["header"].as_str().unwrap_or("").to_string(),
                        multi: q["multiSelect"].as_bool().unwrap_or(false),
                        options,
                    })
                    .filter(|q| !q.options.is_empty())
                })
                .collect()
        })
        .unwrap_or_default();
    // All or nothing: a question Boo could not show must not be answered around.
    if list.len() == v.as_array().map_or(0, Vec::len) { list } else { Vec::new() }
}

/// What the phone said.
#[derive(Debug, PartialEq)]
pub enum Answer {
    Decision(&'static str),
    /// Question text → chosen label(s), comma-joined for multi-select (Claude Code's own format).
    Answers(serde_json::Map<String, Value>),
}

/// Picks → AskUserQuestion answers. Every question exactly once, each either a list
/// of option indexes (single-select one, multi-select at least one, all existing) or
/// the text Jhon typed for "Other" (non-empty, at most 1000 characters).
pub fn answers_from_picks(questions: &[Question], picks: &Value) -> Option<serde_json::Map<String, Value>> {
    let picks = picks.as_array()?;
    if questions.is_empty() || picks.len() != questions.len() {
        return None;
    }
    let mut out = serde_json::Map::new();
    for (q, p) in questions.iter().zip(picks) {
        if let Some(text) = p.as_str() {
            let text = text.trim();
            if text.is_empty() || text.chars().count() > 1000 {
                return None;
            }
            out.insert(q.question.clone(), json!(text));
            continue;
        }
        let idx: Vec<usize> = p.as_array()?.iter().map(|i| i.as_u64().map(|i| i as usize)).collect::<Option<_>>()?;
        if idx.is_empty() || (!q.multi && idx.len() != 1) {
            return None;
        }
        let labels: Vec<&str> = idx.iter().map(|i| q.options.get(*i).map(String::as_str)).collect::<Option<_>>()?;
        out.insert(q.question.clone(), json!(labels.join(",")));
    }
    Some(out)
}

static APPROVALS: Mutex<Vec<Approval>> = Mutex::new(Vec::new());
/// The reply-topic stream; open only while an approval is pending.
static LISTENER: Mutex<Option<tauri::async_runtime::JoinHandle<()>>> = Mutex::new(None);
/// The latest usage view from usage.rs.
static USAGE: Mutex<Value> = Mutex::new(Value::Null);

fn kick() {
    if let Some(tx) = TX.get() {
        let _ = tx.send(Update::Kick);
    }
}

/// Claude Code usage changed (usage.rs, once a minute).
pub fn usage(view: Value) {
    *USAGE.lock().unwrap() = view;
    kick();
}

fn cut(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n - 1).collect::<String>()) }
}

/// The island has a permission card up: offer it to the phone as well.
pub fn approval_open(app: &AppHandle, id: &str, agent: &str, tool: &str, text: &str, questions: Vec<Question>) {
    let topic = app.try_state::<crate::Shared>().and_then(|s| {
        let s = s.settings.lock().unwrap();
        (s.phone_sync && !s.phone_topic.is_empty()).then(|| s.phone_topic.clone())
    });
    if topic.is_none() {
        return;
    }
    let a = Approval {
        id: id.to_string(),
        agent: cut(agent, 32),
        tool: cut(tool, 32),
        text: cut(text, APPROVAL_TEXT),
        code: random_hex(),
        expires: now_secs() + crate::pipe::DECISION_TIMEOUT.as_secs() as i64,
        questions,
    };
    APPROVALS.lock().unwrap().push(a);
    refresh(app);
}

fn phone_topic(app: &AppHandle) -> Option<String> {
    app.try_state::<crate::Shared>().and_then(|s| {
        let s = s.settings.lock().unwrap();
        (s.phone_sync && !s.phone_topic.is_empty()).then(|| s.phone_topic.clone())
    })
}

/// Something the phone shows changed: republish, and keep the reply topic open
/// exactly while the phone has something to answer (approvals, notify cards).
pub fn refresh(app: &AppHandle) {
    let need = !APPROVALS.lock().unwrap().is_empty() || crate::notify::phone_pending(now_secs());
    let mut listener = LISTENER.lock().unwrap();
    match (need, listener.is_some(), phone_topic(app)) {
        (true, false, Some(topic)) => {
            *listener = Some(tauri::async_runtime::spawn(listen_replies(app.clone(), format!("{topic}-reply"))));
        }
        (false, true, _) => {
            if let Some(h) = listener.take() {
                h.abort();
            }
        }
        _ => {}
    }
    drop(listener);
    kick();
}

/// The request is over, whoever answered it (or nobody): the phone card goes.
pub fn approval_close(app: &AppHandle, id: &str) {
    let removed = {
        let mut list = APPROVALS.lock().unwrap();
        let before = list.len();
        list.retain(|a| a.id != id);
        list.len() != before
    };
    if removed {
        refresh(app);
    }
}

/// The phone's answer, if it names a pending request with its code, unused and in
/// time. A match is removed, so a replay finds nothing; a wrong code leaves the
/// request for the real phone.
///
/// A question takes `picks` (one list of option indexes per question) and nothing
/// else: an Allow for a question is refused, never passed on blind.
pub fn take_answer(list: &mut Vec<Approval>, msg: &str, now: i64) -> Option<(String, Answer)> {
    let v: Value = serde_json::from_str(msg).ok()?;
    let (id, code) = (v["id"].as_str()?, v["code"].as_str()?);
    let i = list.iter().position(|a| a.id == id && same(&a.code, code) && now <= a.expires)?;
    let answer = if list[i].questions.is_empty() {
        match v["decision"].as_str()? {
            "allow" => Answer::Decision("allow"),
            "deny" => Answer::Decision("deny"),
            _ => return None,
        }
    } else {
        Answer::Answers(answers_from_picks(&list[i].questions, &v["picks"])?)
    };
    Some((list.remove(i).id, answer))
}

/// Compares without stopping at the first difference.
pub fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// One line of ntfy's JSON stream on the reply topic.
fn on_reply_line(app: &AppHandle, line: &[u8]) {
    let Ok(v) = serde_json::from_slice::<Value>(line) else { return };
    if v["event"] != "message" {
        return;
    }
    let msg = v["message"].as_str().unwrap_or("");
    // A notify card: {id, code, action: i} presses a button, {id, code, dismiss: true} clears it.
    if let Ok(m) = serde_json::from_str::<Value>(msg) {
        if let (Some(id), Some(code)) = (m["id"].as_str().filter(|i| i.starts_with("n-")), m["code"].as_str()) {
            let index = m["action"].as_u64().map(|i| i as usize);
            if index.is_some() || m["dismiss"] == true {
                crate::notify::act(app, id, index, Some(code));
            }
            return;
        }
    }
    let taken = take_answer(&mut APPROVALS.lock().unwrap(), msg, now_secs());
    match taken {
        Some((id, answer)) => {
            let decision = match &answer {
                Answer::Decision(d) => {
                    crate::pipe::answer(app, &id, d);
                    *d
                }
                Answer::Answers(map) => {
                    crate::pipe::answer_questions(app, &id, map);
                    "answers"
                }
            };
            log::line(format!("phone answered id={id} {decision}"));
            let _ = app.emit_to(crate::island::WINDOW_LABEL, "approval-resolved", json!({ "requestId": id, "decision": decision }));
            kick();
        }
        None => log::line("phone reply ignored (unknown id, wrong code, used or expired)"),
    }
}

/// Streams the reply topic until approval_close aborts it. Replies older than this
/// listener are never fetched; anything re-delivered after a reconnect has already
/// been used and is ignored.
async fn listen_replies(app: AppHandle, topic: String) {
    let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).build().unwrap_or_default();
    // A little slack for clock skew between this PC and ntfy: older answers are
    // either for a request that is gone or already used, and both are ignored.
    let since = now_secs() - 30;
    loop {
        if let Ok(mut resp) = client.get(format!("https://ntfy.sh/{topic}/json?since={since}")).send().await {
            let mut buf: Vec<u8> = Vec::new();
            while let Ok(Some(chunk)) = resp.chunk().await {
                buf.extend_from_slice(&chunk);
                while let Some(i) = buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=i).collect();
                    on_reply_line(&app, &line);
                }
                if buf.len() > 64 * 1024 {
                    buf.clear();
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// The island's agents changed (called from the `phone_agents` command).
pub fn agents(list: Vec<AgentIn>) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(Update::Agents(list));
    }
}

/// The board poller read the board again.
pub fn board(data: &Value) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(Update::Board(data.clone()));
    }
}

const STATES: [&str; 10] = [
    "idle", "working", "thinking", "searching", "approval", "question", "error", "finished", "ratelimit", "sleeping",
];

/// Step labels that may leave the PC, in English. Anything else (prompts, Stop
/// messages, questions) is dropped; the part after " · " (a command or a file
/// name) never goes.
fn safe_step(raw: &str) -> &'static str {
    let label = raw.split(" · ").next().unwrap_or("").trim();
    match label {
        "Exécute" => "Running a command",
        "Lit" => "Reading",
        "Écrit" => "Writing",
        "Modifie" => "Editing",
        "Cherche" | "Recherche" => "Searching",
        "Recherche web" => "Web search",
        "Récupère" => "Fetching a page",
        "Tâches" => "Updating tasks",
        "Agent" => "Subagent",
        "Liste" => "Listing",
        "Notebook" => "Notebook",
        "⚠ failed" => "Tool failed",
        "+ subagent" => "Subagent started",
        "• subagent done" => "Subagent done",
        _ => "",
    }
}

fn safe_color(c: &str) -> String {
    let ok = c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|ch| ch.is_ascii_hexdigit());
    if ok { c.to_uppercase() } else { "#FFFFFF".into() }
}

/// The snapshot the phone gets. `at` is Unix seconds.
pub fn snapshot(agents: &[AgentIn], board: &Value, at: i64) -> Value {
    let agents: Vec<Value> = agents
        .iter()
        .take(12) // ntfy turns bodies over 4 KB into attachments
        .map(|a| {
            let state = if STATES.contains(&a.state.as_str()) { a.state.as_str() } else { "idle" };
            json!({
                "id": a.id.chars().take(40).collect::<String>(),
                "name": a.name.chars().take(32).collect::<String>(),
                "color": safe_color(&a.color),
                "state": state,
                "step": safe_step(&a.step),
            })
        })
        .collect();
    let needs_you = agents.iter().any(|a| matches!(a["state"].as_str(), Some("approval" | "question")));
    let board = board.get("counts").map(|_| {
        let next = &board["next"];
        let waiting: i64 = ["thumb", "drl", "check", "approval", "issues"]
            .iter()
            .map(|k| board["counts"][k].as_i64().unwrap_or(0))
            .sum();
        json!({
            "next_title": next["title"].as_str().unwrap_or("").chars().take(48).collect::<String>(),
            "next_time": format!("{} {}", next["date"].as_str().unwrap_or(""), next["time"].as_str().unwrap_or("")).trim().to_string(),
            "waiting": waiting,
        })
    });
    json!({ "v": 1, "at": at, "agents": agents, "board": board, "needs_you": needs_you })
}

/// Adds the notify cards meant for the phone (notify.rs). Oldest go first if the
/// snapshot would pass ntfy's 4 KB, which turns bigger bodies into attachments.
pub fn with_alerts(mut snap: Value, mut cards: Vec<Value>) -> Value {
    if !cards.is_empty() {
        snap["needs_you"] = json!(true);
    }
    loop {
        snap["alerts"] = json!(cards);
        if snap.to_string().len() < 3900 || cards.is_empty() {
            return snap;
        }
        cards.pop();
    }
}

/// Adds pending approvals and Claude Code usage to a snapshot. Expired approvals
/// are left out; either one waiting on Jhon makes `needs_you` true.
pub fn with_extras(mut snap: Value, approvals: &[Approval], usage: &Value, at: i64) -> Value {
    let list: Vec<Value> = approvals
        .iter()
        .filter(|a| a.expires > at)
        .take(2)
        .map(|a| {
            let questions: Vec<Value> = a.questions.iter().take(4).map(|q| json!({
                "q": cut(&q.question, 100),
                "h": cut(&q.header, 16),
                "multi": q.multi,
                "options": q.options.iter().take(4).map(|o| cut(o, 32)).collect::<Vec<_>>(),
            })).collect();
            json!({ "id": a.id, "agent": a.agent, "tool": a.tool, "text": a.text, "code": a.code, "expires": a.expires, "questions": questions })
        })
        .collect();
    let alert = usage["alert"].as_str().map(|s| cut(s, APPROVAL_TEXT));
    if !list.is_empty() || alert.is_some() {
        snap["needs_you"] = json!(true);
    }
    snap["approvals"] = json!(list);
    snap["usage"] = if usage.is_object() {
        json!({ "five": usage["five"], "week": usage["week"], "exact": usage["exact"], "alert": alert })
    } else {
        Value::Null
    };
    snap
}

/// What counts as news: something waiting on Jhon (an approval, an agent asking, a
/// card) or the usage meter moving a tenth. Agents starting and stopping and the
/// board ride along on the HEARTBEAT instead: on 2026-10-04 they used the whole
/// day's share by evening and the phone went quiet (Jhon, 2026-10-05).
pub fn change_key(snap: &Value) -> String {
    let mut s = snap.clone();
    s["at"] = json!(0);
    s["board"] = Value::Null;
    for k in ["five", "week"] {
        if let Some(p) = s["usage"][k].as_f64() {
            s["usage"][k] = json!((p / 10.0).floor());
        }
    }
    if let Some(list) = s["agents"].as_array_mut() {
        list.retain(|a| matches!(a["state"].as_str(), Some("approval" | "question")));
        for a in list {
            a["step"] = json!("");
            a["name"] = json!("");
        }
    }
    s.to_string()
}

/// How long to hold a publish so two are never closer than MIN_GAP.
pub fn wait_before(last: Option<Instant>, now: Instant) -> Duration {
    last.map(|l| MIN_GAP.saturating_sub(now.saturating_duration_since(l))).unwrap_or(Duration::ZERO)
}

/// Random 16 hex digits from the OS-seeded std hasher keys (no rand crate needed).
pub fn random_hex() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    format!("{:016x}", h.finish())
}

pub fn new_topic() -> String {
    format!("boo-phone-{}", random_hex())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Today's (UTC day, publishes) from disk. Kept there because the count used to start at
/// zero on every restart, and Boo restarted six times on 2026-10-03 while ntfy ran dry.
fn read_sent(path: &std::path::Path) -> (i64, u32) {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or((0, 0))
}

/// Spawns the publisher. It sleeps on the channel while everything is idle, so it
/// costs nothing until something changes.
pub fn start(app: AppHandle) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    if TX.set(tx).is_err() {
        return;
    }
    tauri::async_runtime::spawn(async move {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap_or_default();
        let mut agents: Vec<AgentIn> = Vec::new();
        let mut board = Value::Null;
        let mut last: Option<(Instant, String)> = None;
        let sent_path = crate::settings::local_dir().join("phone-sent.json");
        let mut sent_today = read_sent(&sent_path); // (day, count); ntfy's day is UTC too
        // Due a fixed time after the last publish, however many updates that are not
        // news arrive in between (each one would otherwise restart a plain timeout).
        let mut next_beat = Instant::now();
        let mut last_needs = false;
        loop {
            let update = match tokio::time::timeout(next_beat.saturating_duration_since(Instant::now()), rx.recv()).await {
                Ok(None) => return,
                Ok(Some(u)) => Some(u),
                Err(_) => None,
            };
            let heartbeat = Instant::now() >= next_beat;
            if heartbeat {
                next_beat = Instant::now() + HEARTBEAT; // also when this one is skipped, so a skip never spins
            }
            match update {
                Some(Update::Agents(a)) => agents = a,
                Some(Update::Board(b)) => board = b,
                Some(Update::Kick) | None => {}
            }
            tokio::time::sleep(wait_before(last.as_ref().map(|l| l.0), Instant::now())).await;
            while let Ok(u) = rx.try_recv() {
                match u {
                    Update::Agents(a) => agents = a,
                    Update::Board(b) => board = b,
                    Update::Kick => {}
                }
            }
            let at = now_secs();
            let approvals = APPROVALS.lock().unwrap().clone();
            let usage = USAGE.lock().unwrap().clone();
            let snap = with_alerts(with_extras(snapshot(&agents, &board, at), &approvals, &usage, at), crate::notify::phone_cards(at));
            let key = change_key(&snap);
            if !heartbeat && last.as_ref().is_some_and(|l| l.1 == key) {
                continue;
            }
            let Some(topic) = app.try_state::<crate::Shared>().and_then(|s| {
                let s = s.settings.lock().unwrap();
                (s.phone_sync && !s.phone_topic.is_empty()).then(|| s.phone_topic.clone())
            }) else {
                continue;
            };
            if crate::integrations::PAUSED.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }
            let day = now_secs() / 86_400;
            if sent_today.0 != day {
                sent_today = (day, 0);
            }
            let needs = snap["needs_you"] == true;
            if sent_today.1 >= if needs || last_needs { NEEDS_YOU_CAP } else { DAILY_CAP } {
                continue;
            }
            sent_today.1 += 1;
            let _ = std::fs::write(&sent_path, serde_json::to_vec(&sent_today).unwrap_or_default());
            last_needs = needs;
            last = Some((Instant::now(), key));
            next_beat = Instant::now() + HEARTBEAT;
            match client.post(format!("https://ntfy.sh/{topic}")).body(snap.to_string()).send().await {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => log::line(format!("phone sync HTTP {}", r.status())),
                Err(e) => log::line(format!("phone sync failed: {e}")),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(state: &str, step: &str) -> AgentIn {
        AgentIn { id: "agent_codex".into(), name: "Codex".into(), color: "#10a37f".into(), state: state.into(), step: step.into() }
    }

    #[test]
    fn snapshot_carries_states_but_never_prompts_commands_or_paths() {
        let board = json!({ "counts": { "thumb": 1, "drl": 2, "check": 0, "approval": 1, "issues": 0 },
            "next": { "title": "Friday PM", "date": "2026-10-03", "time": "15:00" } });
        let list = vec![
            agent("working", "Exécute · rm -rf C:\\secret"),
            AgentIn { id: "integration_claude".into(), name: "coucou".into(), color: "bad".into(), state: "question".into(), step: "Should I delete prod?".into() },
            agent("dizzy", "Lit · D:\\Work\\notes.txt"),
        ];
        let s = snapshot(&list, &board, 42);
        let text = s.to_string();
        assert!(!text.contains("rm -rf") && !text.contains("secret") && !text.contains("prod") && !text.contains("notes"));
        assert_eq!(s["v"], 1);
        assert_eq!(s["at"], 42);
        assert_eq!(s["agents"][0]["step"], "Running a command");
        assert_eq!(s["agents"][0]["color"], "#10A37F");
        assert_eq!(s["agents"][1]["step"], "");
        assert_eq!(s["agents"][1]["color"], "#FFFFFF");
        assert_eq!(s["agents"][2]["state"], "idle");
        assert_eq!(s["agents"][2]["step"], "Reading");
        assert_eq!(s["needs_you"], true);
        assert_eq!(s["board"]["waiting"], 4);
        assert_eq!(s["board"]["next_title"], "Friday PM");
        assert_eq!(s["board"]["next_time"], "2026-10-03 15:00");
        assert!(snapshot(&[], &Value::Null, 0)["board"].is_null());
        assert!(text.len() < 4096);
    }

    #[test]
    fn only_things_waiting_on_jhon_count_and_publishes_keep_their_distance() {
        let a = snapshot(&[agent("working", "Lit · a.rs")], &Value::Null, 1);
        let b = snapshot(&[agent("working", "Écrit · b.rs")], &Value::Null, 2);
        let c = snapshot(&[agent("finished", "")], &Value::Null, 3);
        let d = snapshot(&[agent("thinking", "")], &Value::Null, 4);
        // agents starting, working and stopping are not news: the heartbeat carries them
        assert_eq!(change_key(&a), change_key(&b));
        assert_eq!(change_key(&a), change_key(&c));
        assert_eq!(change_key(&a), change_key(&d));
        assert_eq!(change_key(&a), change_key(&snapshot(&[agent("working", "")], &json!({"next": "x"}), 5)));
        // an agent waiting on Jhon is, and so is it stopping waiting
        let q = snapshot(&[agent("question", "Pick one?")], &Value::Null, 6);
        let p = snapshot(&[agent("approval", "")], &Value::Null, 7);
        assert_ne!(change_key(&a), change_key(&q));
        assert_ne!(change_key(&q), change_key(&p));

        let t = Instant::now();
        assert_eq!(wait_before(None, t), Duration::ZERO);
        assert_eq!(wait_before(Some(t), t), MIN_GAP);
        assert_eq!(wait_before(Some(t), t + Duration::from_millis(500)), Duration::from_millis(1500));
        assert_eq!(wait_before(Some(t), t + Duration::from_secs(5)), Duration::ZERO);
    }

    #[test]
    fn topics_are_private_and_distinct() {
        let (a, b) = (new_topic(), new_topic());
        assert_ne!(a, b);
        assert!(a.starts_with("boo-phone-") && a.len() == 26);
        assert!(a[10..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    fn pending(id: &str, code: &str, expires: i64) -> Approval {
        Approval { id: id.into(), agent: "Claude Code".into(), tool: "Bash".into(), text: "Bash · ls".into(), code: code.into(), expires, questions: vec![] }
    }

    #[test]
    fn a_phone_answer_needs_the_right_code_in_time_and_works_once() {
        let mut list = vec![pending("1-1", "c0ffee", 100), pending("1-2", "beef", 50)];
        let msg = |id: &str, code: &str, d: &str| json!({ "id": id, "code": code, "decision": d }).to_string();
        // Wrong code, unknown id, a made-up decision, garbage: nothing, and the request stays.
        assert_eq!(take_answer(&mut list, &msg("1-1", "c0ffef", "allow"), 10), None);
        assert_eq!(take_answer(&mut list, &msg("9-9", "c0ffee", "allow"), 10), None);
        assert_eq!(take_answer(&mut list, &msg("1-1", "c0ffee", "always"), 10), None);
        assert_eq!(take_answer(&mut list, "allow 1-1", 10), None);
        assert_eq!(list.len(), 2);
        // Too late: the terminal has the question now.
        assert_eq!(take_answer(&mut list, &msg("1-2", "beef", "allow"), 51), None);
        // Right code, in time: answered, and gone.
        assert_eq!(take_answer(&mut list, &msg("1-1", "c0ffee", "allow"), 100), Some(("1-1".to_string(), Answer::Decision("allow"))));
        assert_eq!(list.len(), 1);
        // A replay of the same message does nothing.
        assert_eq!(take_answer(&mut list, &msg("1-1", "c0ffee", "allow"), 10), None);
        assert_eq!(take_answer(&mut list, &msg("1-2", "beef", "deny"), 50), Some(("1-2".to_string(), Answer::Decision("deny"))));
        assert_ne!(random_hex(), random_hex());
        assert_eq!(random_hex().len(), 16);
    }

    #[test]
    fn a_question_is_answered_by_picks_never_by_allow() {
        let raw = json!([
            { "question": "Cua driver: remove too?", "header": "Cua", "multiSelect": false,
              "options": [{ "label": "Remove" }, { "label": "Keep" }] },
            { "question": "Which?", "header": "", "multiSelect": true,
              "options": [{ "label": "a" }, { "label": "b" }, { "label": "c" }] } ]);
        let qs = parse_questions(&raw);
        assert_eq!(qs.len(), 2);
        assert!(parse_questions(&json!([{ "question": "no options", "options": [] }])).is_empty(), "all or nothing");
        let mut list = vec![Approval { questions: qs.clone(), ..pending("2-1", "c0de", 100) }];
        let msg = |v: Value| json!({ "id": "2-1", "code": "c0de" }).as_object().unwrap().clone().into_iter().chain(v.as_object().unwrap().clone()).collect::<serde_json::Map<_, _>>();
        let send = |list: &mut Vec<Approval>, v: Value| take_answer(list, &Value::Object(msg(v)).to_string(), 10);
        // Allow for a question is refused, and so are incomplete or impossible picks.
        assert_eq!(send(&mut list, json!({ "decision": "allow" })), None);
        assert_eq!(send(&mut list, json!({ "picks": [[0]] })), None);
        assert_eq!(send(&mut list, json!({ "picks": [[0, 1], [0]] })), None, "single-select takes one");
        assert_eq!(send(&mut list, json!({ "picks": [[5], [0]] })), None);
        assert_eq!(send(&mut list, json!({ "picks": [[0], []] })), None);
        assert_eq!(list.len(), 1);
        let (id, answer) = send(&mut list, json!({ "picks": [[0], [0, 2]] })).unwrap();
        assert_eq!(id, "2-1");
        let Answer::Answers(map) = answer else { panic!("expected answers") };
        assert_eq!(map["Cua driver: remove too?"], "Remove");
        assert_eq!(map["Which?"], "a,c");
        // "Other": typed text answers that question; blank text answers nothing.
        let mut typed = vec![Approval { questions: parse_questions(&raw), ..pending("2-1", "c0de", 100) }];
        assert_eq!(send(&mut typed, json!({ "picks": ["  ", [0]] })), None);
        let (_, Answer::Answers(map)) = send(&mut typed, json!({ "picks": [" Remove it, and the logs ", [1]] })).unwrap() else { panic!() };
        assert_eq!(map["Cua driver: remove too?"], "Remove it, and the logs");
        // The phone sees the question and its options, cut, plus the code to answer with.
        let snap = with_extras(snapshot(&[], &Value::Null, 1), &[Approval { questions: qs, ..pending("2-2", "x", 100) }], &Value::Null, 1);
        assert_eq!(snap["approvals"][0]["questions"][1]["options"], json!(["a", "b", "c"]));
        assert_eq!(snap["approvals"][0]["questions"][1]["multi"], true);
        // Worst case still fits ntfy's 4 KB body.
        let big_q: Vec<Question> = (0..4).map(|i| Question { question: format!("{i}{}", "q".repeat(300)), header: "h".repeat(30), multi: true,
            options: (0..6).map(|j| format!("{j}{}", "o".repeat(80))).collect() }).collect();
        let two: Vec<Approval> = (0..3).map(|i| Approval { questions: big_q.clone(), text: "t".repeat(200), ..pending(&format!("9-{i}"), &"f".repeat(16), 100) }).collect();
        let s = with_extras(snapshot(&[agent("working", "")], &Value::Null, 1), &two, &json!({ "five": 1.0, "week": 1.0, "exact": true, "alert": "a".repeat(200) }), 1);
        assert!(s.to_string().len() < 4096, "{}", s.to_string().len());
    }

    #[test]
    fn approvals_and_usage_ride_the_snapshot() {
        let base = snapshot(&[agent("idle", "")], &Value::Null, 10);
        let usage = json!({ "five": 8.0, "week": 57.0, "exact": true, "alert": null });
        let s = with_extras(base.clone(), &[pending("1-1", "c0ffee", 100), pending("1-2", "x", 5)], &usage, 10);
        assert_eq!(s["needs_you"], true);
        assert_eq!(s["approvals"].as_array().unwrap().len(), 1, "expired ones stay home");
        assert_eq!(s["approvals"][0]["code"], "c0ffee");
        assert_eq!(s["usage"]["week"], 57.0);
        let quiet = with_extras(base.clone(), &[], &usage, 10);
        assert_eq!(quiet["needs_you"], false);
        let alert = with_extras(base, &[], &json!({ "five": 100.0, "week": 57.0, "exact": false, "alert": "Claude Code: Session limit hit" }), 10);
        assert_eq!(alert["needs_you"], true);
        // A small usage drift is not worth a message; a new tenth is.
        let a = with_extras(snapshot(&[], &Value::Null, 1), &[], &json!({ "five": 41.0, "week": 1.0, "exact": true }), 1);
        let b = with_extras(snapshot(&[], &Value::Null, 2), &[], &json!({ "five": 44.0, "week": 1.0, "exact": true }), 2);
        let c = with_extras(snapshot(&[], &Value::Null, 3), &[], &json!({ "five": 51.0, "week": 1.0, "exact": true }), 3);
        assert_eq!(change_key(&a), change_key(&b));
        assert_ne!(change_key(&a), change_key(&c));
        let long: Vec<Approval> = vec![pending("1-1", &"f".repeat(16), 100); 3].into_iter().map(|mut p| { p.text = "é".repeat(120); p }).collect();
        let big = with_extras(snapshot(&[], &Value::Null, 1), &long, &usage, 1);
        assert!(big.to_string().len() < 4096);
    }

    #[test]
    fn the_daily_count_survives_a_restart() {
        let path = std::env::temp_dir().join(format!("boo-phone-sent-{}.json", random_hex()));
        assert_eq!(read_sent(&path), (0, 0)); // first run: no file yet
        std::fs::write(&path, serde_json::to_vec(&(20_364i64, 149u32)).unwrap()).unwrap();
        assert_eq!(read_sent(&path), (20_364, 149));
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(read_sent(&path), (0, 0));
        let _ = std::fs::remove_file(&path);
    }
}
