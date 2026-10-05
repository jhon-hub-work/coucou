// Claude Code usage: NoTo's watcher (D:\Work\Claude\NotiClaude\backend\watcher.py)
// ported, so Boo is the only thing that tells Jhon about limits.
//
// Where the numbers come from, and how much they can be trusted:
//   exact     Claude Code hands `rate_limits` to its statusline command; NoTo's
//             statusline.py caches them in backend/usage.json. Only fresh while a
//             terminal session is open: the desktop app never runs a statusline.
//   estimate  input + output + cache_creation tokens from ~/.claude/projects/**/*.jsonl
//             against a self-calibrated cap. NO local source exposes the real quota,
//             so this is a guess and is always labelled one (no digits on screen).
//   limit     an API error line (`isApiErrorMessage`) saying "hit your … limit". Exact.
//   reset     the time that error names. Exact.
// (NoTo's /api/oauth/usage fetch is not ported: the setup-token it uses is refused,
// 403 "scope user:profile", so it never returned anything.)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::log;

const SESSION: i64 = 5 * 3600;
const WEEK: i64 = 7 * 86_400;
/// Limit messages older than this are history, not news (first-run backfill guard).
const STALE_LIMIT: i64 = 900;
const THRESHOLD: f64 = 75.0;
/// Hysteresis: a warning re-arms only once usage drops well below the threshold.
const REARM: f64 = 70.0;
/// Caps from NoTo's config.json, measured against a real Settings › Usage reading.
const DEFAULT_CAPS: (u64, u64) = (18_600_000, 497_000_000);
/// An exact statusline reading older than this no longer describes now.
const EXACT_FRESH: i64 = 15 * 60;
/// How long a warn / reset alert stays on the phone; a limit stays until it lifts.
const ALERT_SHOWN: i64 = 15 * 60;
const TICK: Duration = Duration::from_secs(60);
/// ponytail: NoTo's folder is fixed on this machine; make it a setting if Boo ever ships wider.
const NOTO_BACKEND: &str = "D:/Work/Claude/NotiClaude/backend";

/// The ledger, saved between runs. Field names match NoTo's state.json so the
/// first run can start from it (offsets, 7 days of events and the calibration).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Ledger {
    offsets: HashMap<String, u64>,
    events: Vec<(i64, u64)>,
    fired: HashMap<String, bool>,
    reset_at: Option<i64>,
    caps: HashMap<String, u64>,
    session_epoch: i64,
    /// Window starts Claude Code reported through the statusline (resets_at minus the window).
    block_hint: Option<i64>,
    week_hint: Option<i64>,
    /// Last alert, for the phone: (kind, text, when).
    alert: Option<(String, String, i64)>,
}

/// What a transcript line means to the ledger.
#[derive(Debug, PartialEq)]
pub enum Record {
    Tokens(i64, u64),
    Limit(i64, String),
}

/// One Claude Code API reading from the statusline cache.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Exact {
    pub five: f64,
    pub week: f64,
    pub five_resets: i64,
    pub week_resets: i64,
    pub updated: i64,
}

/// What Boo shows: percentages, and whether they are real or a guess.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub five: f64,
    pub week: f64,
    pub exact: bool,
    pub limited_until: Option<i64>,
}

/// `2026-10-03T01:58:48.123Z` → Unix seconds. Transcripts are UTC; reading them
/// as local time shifts every event by the UTC offset and empties the 5 h window.
fn utc_secs(ts: &str) -> Option<i64> {
    let b = ts.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| ts.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// The "session" in "You've hit your session limit", or None when the text is not a limit.
fn limit_kind(text: &str) -> Option<&'static str> {
    let low = text.to_lowercase();
    let mut from = 0;
    while let Some(i) = low[from..].find("hit your ") {
        let rest = &low[from + i + 9..];
        for kind in ["session", "weekly", "monthly", "usage"] {
            if let Some(after) = rest.strip_prefix(kind) {
                let window: String = after.chars().take(36).collect();
                if window.contains("limit") && !window[..window.find("limit").unwrap()].contains('\n') {
                    return Some(kind);
                }
            }
        }
        from += i + 9;
    }
    None
}

/// Parses one transcript line. Most lines are skipped without a JSON parse.
pub fn read_line(line: &str, now: i64) -> Option<Record> {
    let tokens = line.contains("\"usage\"");
    let error = line.contains("isApiErrorMessage");
    if !tokens && !error {
        return None;
    }
    let d: Value = serde_json::from_str(line).ok()?;
    let when = d["timestamp"].as_str().and_then(utc_secs).unwrap_or(now);
    let usage = &d["message"]["usage"];
    if usage.is_object() {
        // cache_read is left out on purpose: ~97% of the raw count here, metered at a
        // fraction of a real token, and it drowns the signal (NoTo, measured).
        let total: u64 = ["input_tokens", "output_tokens", "cache_creation_input_tokens"]
            .iter()
            .map(|k| usage[*k].as_u64().unwrap_or(0))
            .sum();
        if total > 0 {
            return Some(Record::Tokens(when, total));
        }
    }
    // The flag is load-bearing: text alone also matches any conversation ABOUT limits.
    if d["isApiErrorMessage"].as_bool() == Some(true) {
        let content = &d["message"]["content"];
        let text = match content.as_array() {
            Some(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(" "),
            None => content.as_str().unwrap_or("").to_string(),
        };
        if limit_kind(&text).is_some() {
            return Some(Record::Limit(when, text));
        }
    }
    None
}

/// When the limit lifts. `local_sod` is the local seconds-of-day at `now`.
/// "limit reached|1791009600" → that epoch; "resets 5:20pm" → next 17:20 local;
/// nothing parseable → a full session from now.
pub fn parse_reset(text: &str, now: i64, local_sod: i64) -> i64 {
    let low = text.to_lowercase();
    if let Some(i) = low.find("limit reached|") {
        let digits: String = low[i + 14..].chars().take_while(char::is_ascii_digit).collect();
        if (9..=11).contains(&digits.len()) {
            if let Ok(t) = digits.parse() {
                return t;
            }
        }
    }
    if let Some(i) = low.find("reset") {
        let tail: Vec<char> = low[i + 5..].chars().collect();
        // First digit within 20 characters, not across a full stop.
        if let Some(start) = tail.iter().take(21).take_while(|c| **c != '.').position(char::is_ascii_digit) {
            let mut j = start;
            let mut hour = 0i64;
            while j < tail.len() && j < start + 2 && tail[j].is_ascii_digit() {
                hour = hour * 10 + tail[j].to_digit(10).unwrap() as i64;
                j += 1;
            }
            let mut minute = 0i64;
            if tail.get(j) == Some(&':') && tail.get(j + 1).is_some_and(char::is_ascii_digit) && tail.get(j + 2).is_some_and(char::is_ascii_digit) {
                minute = tail[j + 1].to_digit(10).unwrap() as i64 * 10 + tail[j + 2].to_digit(10).unwrap() as i64;
                j += 3;
            }
            while tail.get(j) == Some(&' ') {
                j += 1;
            }
            let ampm: String = tail.iter().skip(j).take(2).collect();
            if ampm == "pm" && hour < 12 {
                hour += 12;
            } else if ampm == "am" && hour == 12 {
                hour = 0;
            }
            let mut target = now - local_sod + hour * 3600 + minute * 60;
            if target <= now {
                target += 86_400; // the stated time already passed today: it means tomorrow
            }
            return target;
        }
    }
    now + SESSION
}

/// "5:20 PM" for an epoch, in local time.
fn clock(at: i64, now: i64, local_sod: i64) -> String {
    let sod = (local_sod + at - now).rem_euclid(86_400);
    let (h, m) = (sod / 3600, sod % 3600 / 60);
    let h12 = if h % 12 == 0 { 12 } else { h % 12 };
    format!("{h12}:{m:02} {}", if h < 12 { "AM" } else { "PM" })
}

impl Ledger {
    fn used(&self, from: i64, to: i64) -> u64 {
        self.events.iter().filter(|e| e.0 >= from && e.0 <= to).map(|e| e.1).sum()
    }

    /// Start of the 5-hour block running at `now`, if one is. Claude's session is a
    /// FIXED block that opens on the first message after the previous one closed,
    /// not a rolling window (NoTo's session_block_end). A block start Claude Code
    /// itself reported (statusline resets_at - 5 h) anchors the walk.
    fn block_start(&self, now: i64) -> Option<i64> {
        let mut times: Vec<i64> = self.events.iter().map(|e| e.0).filter(|t| *t <= now).collect();
        times.sort_unstable();
        let mut start = self.block_hint;
        for t in times {
            if start.is_none_or(|s| t >= s + SESSION) {
                start = Some(t);
            }
        }
        start.filter(|s| s + SESSION > now)
    }

    /// Start of the current week: Claude's weekly window resets on a fixed schedule,
    /// known once the statusline has reported it; a rolling 7 days until then.
    fn week_start(&self, now: i64) -> i64 {
        match self.week_hint {
            Some(mut w) => {
                while w + WEEK <= now {
                    w += WEEK;
                }
                w
            }
            None => now - WEEK,
        }
    }

    /// Prunes to a week; returns (session tokens, weekly tokens).
    fn roll(&mut self, now: i64) -> (u64, u64) {
        self.events.retain(|e| e.0 > now - WEEK);
        // A reset rolls the short window over: spend before it no longer counts.
        let session = self.block_start(now).map(|s| self.used(s.max(self.session_epoch), now)).unwrap_or(0);
        (session, self.used(self.week_start(now), now))
    }

    fn cap(&self, key: &str) -> u64 {
        let default = if key == "session" { DEFAULT_CAPS.0 } else { DEFAULT_CAPS.1 };
        self.caps.get(key).copied().filter(|c| *c > 0).unwrap_or(default)
    }

    /// A limit hit: arm the reset and calibrate the session cap to what was spent.
    fn on_limit(&mut self, text: &str, now: i64, local_sod: i64) -> Option<(String, String)> {
        let reset_at = parse_reset(text, now, local_sod);
        if self.reset_at.is_some_and(|r| (r - reset_at).abs() < 300) {
            return None; // the same hit, seen again
        }
        self.reset_at = Some(reset_at);
        let (session, _) = self.roll(now);
        if session > 0 {
            self.caps.insert("session".into(), session);
        }
        // Weekly is left alone: a hit cannot say which window ran out.
        self.fired.insert("warn_session".into(), true);
        self.fired.insert("warn_weekly".into(), true);
        let which = limit_kind(text).unwrap_or("usage");
        let which = format!("{}{}", which[..1].to_uppercase(), &which[1..]);
        Some(("limit".into(), format!("Claude Code: {which} limit hit, out until {}", clock(reset_at, now, local_sod))))
    }

    /// Folds new records in and returns the reading plus any alerts to raise.
    pub fn tick(&mut self, records: Vec<Record>, exact: Option<Exact>, now: i64, local_sod: i64) -> (Reading, Vec<(String, String)>) {
        let mut alerts = Vec::new();
        for r in records {
            match r {
                Record::Tokens(t, n) => self.events.push((t, n)),
                // First runs backfill months of history; only a fresh hit is news.
                Record::Limit(t, text) if t > now - STALE_LIMIT => alerts.extend(self.on_limit(&text, now, local_sod)),
                Record::Limit(..) => {}
            }
        }
        // Window boundaries from a real reading stay true however old it is.
        if let Some(e) = exact {
            if e.five_resets > 0 {
                self.block_hint = Some(e.five_resets - SESSION);
            }
            if e.week_resets > 0 {
                self.week_hint = Some(e.week_resets - WEEK);
            }
        }
        let (session, week) = self.roll(now);

        if self.reset_at.is_none() {
            // A real reading is the division NoTo's README says to redo by hand whenever
            // the estimate drifts: ledger tokens in that window / real percentage.
            // Readings older than a day are skipped (the model mix behind them moves).
            if let Some(e) = exact.filter(|e| now - e.updated <= 86_400) {
                let windows = [("session", e.five_resets - SESSION, e.five), ("weekly", e.week_resets - WEEK, e.week)];
                for (key, from, pct) in windows {
                    let used = self.used(from, e.updated);
                    if pct >= 5.0 && used > 0 && from > 0 {
                        self.caps.insert(key.into(), (used as f64 * 100.0 / pct) as u64);
                    }
                }
            }
            // Over the cap without Claude Code saying so proves the cap is too low.
            for (key, used) in [("session", session), ("weekly", week)] {
                if used > self.cap(key) {
                    self.caps.insert(key.into(), (used as f64 * 1.15) as u64);
                    self.fired.remove(&format!("warn_{key}"));
                }
            }
        }
        let exact = exact.filter(|e| now - e.updated <= EXACT_FRESH && (e.five_resets == 0 || e.five_resets > now));

        let pct = |used: u64, cap: u64| (used as f64 / cap as f64 * 100.0).min(100.0);
        let reading = match exact {
            Some(e) => Reading { five: e.five, week: e.week, exact: true, limited_until: None },
            None => Reading { five: pct(session, self.cap("session")), week: pct(week, self.cap("weekly")), exact: false, limited_until: None },
        };

        for (key, used, label) in [("session", reading.five, "5-hour"), ("weekly", reading.week, "weekly")] {
            let flag = format!("warn_{key}");
            if used >= THRESHOLD && !self.fired.get(&flag).copied().unwrap_or(false) {
                self.fired.insert(flag, true);
                let text = if reading.exact {
                    format!("Claude Code: {}% of your {label} limit used", used.round())
                } else {
                    format!("Claude Code: about 75% of your {label} limit used (estimate)")
                };
                alerts.push(("warn".into(), text));
            } else if used < REARM {
                self.fired.remove(&flag);
            }
        }

        if let Some(at) = self.reset_at.filter(|r| now >= *r) {
            self.reset_at = None;
            self.session_epoch = now;
            self.fired.clear();
            // A reset that passed long ago (Boo was off) is not news.
            if now - at < STALE_LIMIT {
                alerts.push(("reset".into(), "Claude Code: your limit just reset, good to go again".into()));
            }
        }
        if let Some((kind, text)) = alerts.last() {
            self.alert = Some((kind.clone(), text.clone(), now));
        }
        (Reading { limited_until: self.reset_at, ..reading }, alerts)
    }

    /// The alert the phone should still show: a limit until it lifts, others briefly.
    pub fn live_alert(&self, now: i64) -> Option<(String, String)> {
        let (kind, text, at) = self.alert.clone()?;
        let live = match kind.as_str() {
            "limit" => self.reset_at.is_some_and(|r| r > now),
            _ => now - at < ALERT_SHOWN,
        };
        live.then_some((kind, text))
    }
}

// ── IO ───────────────────────────────────────────────────────────────────────

fn projects_dir() -> PathBuf {
    crate::platform::home_dir().join(".claude").join("projects")
}

fn state_path() -> PathBuf {
    crate::settings::local_dir().join("usage-state.json")
}

fn jsonl_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => jsonl_files(&p, out),
            Ok(_) if p.extension().is_some_and(|x| x == "jsonl") => out.push(p),
            _ => {}
        }
    }
}

/// Reads every transcript from its stored offset, whole lines only.
fn scan(ledger: &mut Ledger, now: i64) -> Vec<Record> {
    use std::io::{Read, Seek, SeekFrom};
    let mut files = Vec::new();
    jsonl_files(&projects_dir(), &mut files);
    let mut out = Vec::new();
    let mut offsets = HashMap::with_capacity(files.len());
    for path in files {
        let key = path.to_string_lossy().to_string();
        let Ok(size) = std::fs::metadata(&path).map(|m| m.len()) else { continue };
        let mut start = ledger.offsets.get(&key).copied().unwrap_or(0);
        if start > size {
            start = 0; // rotated or truncated
        }
        if start < size {
            let mut chunk = Vec::new();
            if let Ok(mut f) = std::fs::File::open(&path) {
                if f.seek(SeekFrom::Start(start)).is_ok() && f.read_to_end(&mut chunk).is_ok() {
                    if let Some(last) = chunk.iter().rposition(|b| *b == b'\n') {
                        for raw in chunk[..last].split(|b| *b == b'\n') {
                            if let Some(r) = read_line(&String::from_utf8_lossy(raw), now) {
                                out.push(r);
                            }
                        }
                        start += last as u64 + 1;
                    }
                }
            }
        }
        offsets.insert(key, start);
    }
    ledger.offsets = offsets; // files that are gone drop out
    out
}

fn read_exact() -> Option<Exact> {
    let v: Value = serde_json::from_slice(&std::fs::read(Path::new(NOTO_BACKEND).join("usage.json")).ok()?).ok()?;
    let w = |k: &str| (v["usage"][k]["used_percentage"].as_f64(), v["usage"][k]["resets_at"].as_i64().unwrap_or(0));
    let ((five, fr), (week, wr)) = (w("five_hour"), w("seven_day"));
    Some(Exact { five: five?, week: week?, five_resets: fr, week_resets: wr, updated: v["updated"].as_i64()? })
}

fn load() -> Ledger {
    let own = std::fs::read(state_path()).ok();
    // First run: carry on from NoTo's ledger so nothing is re-read and the calibration survives.
    let bytes = own.or_else(|| std::fs::read(Path::new(NOTO_BACKEND).join("state.json")).ok());
    bytes.and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(ledger: &Ledger) {
    let Ok(text) = serde_json::to_vec(ledger) else { return };
    let path = state_path();
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn local_sod() -> i64 {
    let t = crate::platform::local_time();
    (t.hour * 3600 + t.minute * 60 + t.second) as i64
}

/// Ticks once a minute on its own thread: the scan is plain file IO.
pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        let mut ledger = load();
        // The island's listeners attach during boot.
        std::thread::sleep(Duration::from_secs(5));
        loop {
            let now = now_secs();
            let records = scan(&mut ledger, now);
            let (r, alerts) = ledger.tick(records, read_exact(), now, local_sod());
            save(&ledger);
            let alert = ledger.live_alert(now);
            let view = json!({
                "five": (r.five * 10.0).round() / 10.0,
                "week": (r.week * 10.0).round() / 10.0,
                "exact": r.exact,
                "limitedUntil": r.limited_until,
                "alert": alert.as_ref().map(|a| a.1.clone()),
            });
            let _ = app.emit_to(crate::island::WINDOW_LABEL, "usage", view.clone());
            for (kind, text) in &alerts {
                log::line(format!("usage alert {kind}: {text}"));
                let _ = app.emit_to(crate::island::WINDOW_LABEL, "usage-alert", json!({ "kind": kind, "text": text }));
            }
            crate::phone::usage(view);
            std::thread::sleep(TICK);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_791_000_000;

    #[test]
    fn transcript_lines_count_tokens_and_only_flagged_limits() {
        let tok = r#"{"timestamp":"2026-10-03T01:00:00.000Z","message":{"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":100,"cache_read_input_tokens":99999}}}"#;
        assert_eq!(read_line(tok, NOW), Some(Record::Tokens(utc_secs("2026-10-03T01:00:00").unwrap(), 115)));
        let hit = r#"{"isApiErrorMessage":true,"message":{"content":[{"type":"text","text":"You've hit your session limit · resets 5:20pm (Asia/Manila)"}]}}"#;
        assert!(matches!(read_line(hit, NOW), Some(Record::Limit(_, t)) if t.contains("5:20pm")));
        // The same words in ordinary prose are not a limit.
        let talk = r#"{"message":{"content":"what if you've hit your session limit?"}}"#;
        assert_eq!(read_line(talk, NOW), None);
        assert_eq!(utc_secs("1970-01-02T00:00:01"), Some(86_401));
        assert_eq!(utc_secs("2000-03-01T00:00:00"), Some(951_868_800));
    }

    #[test]
    fn reset_times_parse_like_noto() {
        // 10:00 local: 5:20pm is later today, 9am is tomorrow.
        let sod = 10 * 3600;
        assert_eq!(parse_reset("hit your session limit - resets 5:20pm", NOW, sod), NOW + 7 * 3600 + 20 * 60);
        assert_eq!(parse_reset("resets 9am", NOW, sod), NOW + 23 * 3600);
        assert_eq!(parse_reset("Claude AI usage limit reached|1791009600", NOW, sod), 1_791_009_600);
        assert_eq!(parse_reset("no time here", NOW, sod), NOW + SESSION);
        assert_eq!(clock(NOW + 7 * 3600 + 20 * 60, NOW, sod), "5:20 PM");
    }

    #[test]
    fn warnings_fire_once_and_limits_and_resets_are_exact() {
        let mut l = Ledger::default();
        l.caps.insert("session".into(), 1000);
        l.caps.insert("weekly".into(), 1_000_000);
        let sod = 10 * 3600;
        let (r, a) = l.tick(vec![Record::Tokens(NOW - 60, 500)], None, NOW, sod);
        assert!(!r.exact && (r.five - 50.0).abs() < 0.01 && a.is_empty());
        let (r, a) = l.tick(vec![Record::Tokens(NOW - 30, 300)], None, NOW, sod);
        assert!((r.five - 80.0).abs() < 0.01);
        assert_eq!(a.len(), 1);
        assert!(a[0].1.contains("estimate") && !a[0].1.contains("80"), "an estimate never shows a digit");
        assert!(l.tick(vec![], None, NOW + 1, sod).1.is_empty(), "fires once");

        let hit = "You've hit your session limit - resets 5:20pm".to_string();
        let (r, a) = l.tick(vec![Record::Limit(NOW, hit.clone())], None, NOW + 2, sod);
        assert_eq!(a[0].0, "limit");
        assert!(a[0].1.contains("5:20 PM"));
        assert_eq!(l.caps["session"], 800, "a real hit calibrates the cap");
        assert!(r.limited_until.is_some());
        assert!(l.live_alert(NOW + 3).is_some());
        assert!(l.tick(vec![Record::Limit(NOW, hit)], None, NOW + 3, sod).1.is_empty(), "same hit twice is one alert");

        let reset = l.reset_at.unwrap();
        let (r, a) = l.tick(vec![], None, reset + 5, sod);
        assert_eq!(a, vec![("reset".to_string(), "Claude Code: your limit just reset, good to go again".to_string())]);
        assert_eq!(r.limited_until, None);
        // A reset long past (imported from an old ledger) stays quiet.
        let mut old = Ledger { reset_at: Some(NOW - 86_400), ..Default::default() };
        assert!(old.tick(vec![], None, NOW, sod).1.is_empty());
        // Old limit lines from a backfill are not news.
        let mut fresh = Ledger::default();
        assert!(fresh.tick(vec![Record::Limit(NOW - 3600, "hit your weekly limit".into())], None, NOW, sod).1.is_empty());
    }

    #[test]
    fn a_fresh_exact_reading_wins_and_recalibrates() {
        let mut l = Ledger::default();
        let e = Exact { five: 20.0, week: 50.0, five_resets: NOW + 3600, week_resets: NOW + 86_400, updated: NOW - 60 };
        let (r, a) = l.tick(vec![Record::Tokens(NOW - 60, 2_000_000)], Some(e), NOW, 0);
        assert!(r.exact && r.five == 20.0 && r.week == 50.0 && a.is_empty());
        assert_eq!(l.caps["session"], 10_000_000);
        // Stale: back to the estimate against the new cap.
        let (r, _) = l.tick(vec![], Some(Exact { updated: NOW - 3600, ..e }), NOW, 0);
        assert!(!r.exact && (r.five - 20.0).abs() < 0.01);
        // An exact 80% says so with its number.
        let (_, a) = l.tick(vec![], Some(Exact { five: 80.0, updated: NOW, ..e }), NOW, 0);
        assert_eq!(a[0].1, "Claude Code: 80% of your 5-hour limit used");
    }

    #[test]
    fn the_session_is_a_fixed_block_not_a_rolling_window() {
        let t = NOW - 10 * 3600;
        let mut l = Ledger { events: vec![(t, 1), (t + 3600, 2), (t + 4 * 3600, 4), (t + 6 * 3600, 8), (t + 9 * 3600, 16)], ..Default::default() };
        // Blocks: t..t+5h (1+2+4), then the first message after it, t+6h..t+11h (8+16).
        assert_eq!(l.block_start(NOW), Some(t + 6 * 3600));
        assert_eq!(l.roll(NOW).0, 24);
        // Nothing since the block closed: no block, nothing used.
        assert_eq!(l.block_start(t + 12 * 3600), None);
        // A start reported by Claude Code anchors it.
        l.block_hint = Some(t + 5 * 3600 + 1800);
        assert_eq!(l.block_start(NOW), Some(t + 5 * 3600 + 1800));
        // Weekly follows the reported reset schedule once known.
        l.week_hint = Some(NOW - 3 * WEEK - 100);
        assert_eq!(l.week_start(NOW), NOW - 100);
    }

    #[test]
    fn noto_state_json_loads() {
        let noto = r#"{"offsets":{"C:\\a.jsonl":12},"events":[[1790000000,5]],"fired":{"warn_session":true},
            "reset_at":null,"caps":{"session":6971901},"usage_published":1,"session_epoch":1787130637}"#;
        let l: Ledger = serde_json::from_str(noto).unwrap();
        assert_eq!(l.offsets["C:\\a.jsonl"], 12);
        assert_eq!(l.events, vec![(1_790_000_000, 5)]);
        assert_eq!(l.cap("session"), 6_971_901);
        assert_eq!(l.cap("weekly"), DEFAULT_CAPS.1);
    }
}
