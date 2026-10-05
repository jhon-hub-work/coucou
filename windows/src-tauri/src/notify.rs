// Notification cards from Jhon's local scripts (the DYE board, the Dr L revision
// agent, Hoot), so Boo is the only thing that buzzes him — the ntfy app retires.
//
// A script runs `boo-hook --notify` with a JSON card on stdin; the relay hands it
// over the private pipe as a `BooNotify` event. A button is exactly what the old
// ntfy button did: POST `body` to `url` (the scripts already listen on their own
// reply topics, so nothing changes on their side). Pressed on the PC island, Boo
// POSTs it; pressed on the phone, the phone sends {id, code, action} to the reply
// topic and Boo POSTs it, with the same one-time-code rules as approvals.
//
// The last HISTORY cards are kept in notify-history.json next to boo.log.

use std::sync::Mutex;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::log;

/// Cards still waiting on Jhon; the oldest go first past this.
const MAX_PENDING: usize = 20;
/// A phone card older than this has stopped being news (still in the history).
pub const PHONE_TTL: i64 = 12 * 3600;
const HISTORY: usize = 50;
pub const BODY_PHONE: usize = 300;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Action {
    pub label: String,
    #[serde(skip)]
    pub url: String,
    #[serde(skip)]
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Card {
    pub id: String,
    pub source: String,
    pub title: String,
    pub body: String,
    pub urgent: bool,
    pub phone: bool,
    pub link: Option<String>,
    pub actions: Vec<Action>,
    #[serde(skip)]
    pub code: String,
    pub at: i64,
}

static PENDING: Mutex<Vec<Card>> = Mutex::new(Vec::new());

fn text(v: &Value, n: usize) -> String {
    v.as_str().unwrap_or("").trim().chars().take(n).collect()
}

/// A card from a script, or why not. Only https buttons and links: a button is a
/// POST Boo makes on Jhon's say-so, so it goes where the script said and nowhere odd.
pub fn parse(v: &Value, id: String, code: String, at: i64) -> Result<Card, String> {
    let source = text(&v["source"], 16).to_lowercase();
    if source.is_empty() || !source.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("source must be a short lowercase name".into());
    }
    let title = text(&v["title"], 120);
    if title.is_empty() {
        return Err("a card needs a title".into());
    }
    let https = |s: &str| s.starts_with("https://") && !s.chars().any(char::is_whitespace);
    let link = v["link"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    if link.as_deref().is_some_and(|l| !https(l)) {
        return Err("link must be https".into());
    }
    let mut actions = Vec::new();
    for a in v["actions"].as_array().map(Vec::as_slice).unwrap_or_default().iter().take(3) {
        let (label, url, body) = (text(&a["label"], 24), text(&a["url"], 300), a["body"].as_str().unwrap_or("").to_string());
        if label.is_empty() || !https(&url) || body.len() > 2000 {
            return Err(format!("bad action {label:?}: needs a label, an https url and a short body"));
        }
        actions.push(Action { label, url, body });
    }
    Ok(Card {
        id,
        source,
        title,
        body: text(&v["body"], 2000),
        urgent: v["urgent"].as_bool().unwrap_or(false),
        phone: v["phone"].as_bool().unwrap_or(false),
        link,
        actions,
        code,
        at,
    })
}

/// The phone's share of the pending cards: short, newest first, with their codes.
pub fn phone_view(cards: &[Card], now: i64) -> Vec<Value> {
    cards
        .iter()
        .rev()
        .filter(|c| c.phone && now - c.at < PHONE_TTL)
        .take(3)
        .map(|c| {
            let body: String = c.body.chars().take(BODY_PHONE).collect();
            json!({ "id": c.id, "source": c.source, "title": c.title, "body": body, "urgent": c.urgent,
                    "actions": c.actions.iter().map(|a| a.label.clone()).collect::<Vec<_>>(), "code": c.code, "at": c.at })
        })
        .collect()
}

pub fn phone_cards(now: i64) -> Vec<Value> {
    phone_view(&PENDING.lock().unwrap(), now)
}

/// Any phone card still pending: the reply topic must stay open for it.
pub fn phone_pending(now: i64) -> bool {
    PENDING.lock().unwrap().iter().any(|c| c.phone && now - c.at < PHONE_TTL)
}

fn emit(app: &AppHandle) {
    let list = PENDING.lock().unwrap().clone();
    let _ = app.emit_to(crate::island::WINDOW_LABEL, "notify-cards", list);
    crate::phone::refresh(app);
}

fn remember(card: &Card) {
    let path = crate::settings::local_dir().join("notify-history.json");
    let mut list: Vec<Value> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    list.push(json!({ "at": card.at, "source": card.source, "title": card.title, "body": card.body, "phone": card.phone,
                      "actions": card.actions.iter().map(|a| a.label.clone()).collect::<Vec<_>>() }));
    let skip = list.len().saturating_sub(HISTORY);
    let _ = std::fs::write(&path, serde_json::to_vec_pretty(&list[skip..]).unwrap_or_default());
}

/// A card arrived from boo-hook --notify.
pub fn receive(app: &AppHandle, v: &Value, now: i64) -> Result<(), String> {
    let card = parse(v, format!("n-{}", crate::phone::random_hex()), crate::phone::random_hex(), now)?;
    log::line(format!("notify {} {:?} phone={}", card.source, card.title, card.phone));
    remember(&card);
    {
        let mut list = PENDING.lock().unwrap();
        list.push(card);
        let extra = list.len().saturating_sub(MAX_PENDING);
        list.drain(..extra);
    }
    emit(app);
    Ok(())
}

/// Takes a card off the list; with an action, makes its POST. `code` is checked
/// for answers from the phone only (the PC island is trusted through the webview).
pub fn take(id: &str, code: Option<&str>) -> Option<Card> {
    let mut list = PENDING.lock().unwrap();
    let i = list.iter().position(|c| c.id == id && code.is_none_or(|k| crate::phone::same(&c.code, k)))?;
    Some(list.remove(i))
}

/// Pressed (index) or dismissed (None), from the island or the phone.
pub fn act(app: &AppHandle, id: &str, index: Option<usize>, code: Option<&str>) -> bool {
    let Some(card) = take(id, code) else {
        log::line(format!("notify {id}: not pending (already handled, or wrong code)"));
        return false;
    };
    emit(app);
    let Some(action) = index.and_then(|i| card.actions.get(i)).cloned() else {
        log::line(format!("notify {id} dismissed"));
        return true;
    };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build().unwrap_or_default();
        let ok = matches!(client.post(&action.url).body(action.body.clone()).send().await, Ok(r) if r.status().is_success());
        log::line(format!("notify {} {:?} → {}", card.source, action.label, if ok { "sent" } else { "FAILED" }));
        if !ok {
            // Put it back so the button can be pressed again.
            PENDING.lock().unwrap().push(card);
            emit(&app);
        }
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(v: Value) -> Result<Card, String> {
        parse(&v, "n-1".into(), "c0de".into(), 100)
    }

    #[test]
    fn cards_are_checked_and_buttons_only_post_to_https() {
        let ok = card(json!({ "source": "hoot", "title": "Acme · Designer", "body": "Fit 82", "phone": true,
            "link": "https://acme.example/job", "actions": [
                { "label": "Apply", "url": "https://ntfy.sh/x-hoot-reply", "body": "apply:ab12cd34" },
                { "label": "Skip", "url": "https://ntfy.sh/x-hoot-reply", "body": "skip:ab12cd34" } ] })).unwrap();
        assert_eq!(ok.actions[1].body, "skip:ab12cd34");
        assert!(ok.phone && !ok.urgent);
        assert!(card(json!({ "source": "hoot", "title": "" })).is_err());
        assert!(card(json!({ "source": "Ho ot", "title": "x" })).is_err());
        assert!(card(json!({ "source": "board", "title": "x", "link": "file:///c:/x" })).is_err());
        assert!(card(json!({ "source": "board", "title": "x", "actions": [{ "label": "Go", "url": "http://ntfy.sh/a", "body": "" }] })).is_err());
        assert!(card(json!({ "source": "board", "title": "x", "actions": [{ "label": "", "url": "https://ntfy.sh/a" }] })).is_err());
        // The secret parts (where a button posts, the code) never serialise to the island.
        let shown = serde_json::to_string(&ok).unwrap();
        assert!(!shown.contains("apply:ab12cd34") && !shown.contains("c0de") && shown.contains("Apply"));
    }

    #[test]
    fn the_phone_gets_short_recent_phone_cards_only() {
        let mut long = card(json!({ "source": "revisions", "title": "Dr L", "body": "é".repeat(900), "phone": true })).unwrap();
        long.at = 100;
        let pc_only = card(json!({ "source": "hoot", "title": "Applied", "phone": false })).unwrap();
        let old = Card { at: 100 - PHONE_TTL, ..long.clone() };
        let v = phone_view(&[old, pc_only, long], 200);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["body"].as_str().unwrap().chars().count(), BODY_PHONE);
        assert_eq!(v[0]["code"], "c0de");
    }
}
