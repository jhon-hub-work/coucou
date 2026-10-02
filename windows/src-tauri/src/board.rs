// The video publishing board, read from its own files.
//
// The board (videos/_publish: tracker.py, schedule.py, instagram.py) keeps its
// state in plain JSON next to the scripts. Boo only reads it — never writes, never
// spawns Python, never touches the network — and shows what is waiting, what is
// rendering and what posts next.
//
//   status.json    every sign-off: approved / checked / thumb_ok / hold / posted…
//   schedule.json  start date, how often, paused, skipped days, hand order
//   uploads.json   upload jobs (waiting / editing) and their revision rounds
//
// The queue below is the same one `schedule.py` builds (colour runs, focus films,
// hand order, start date, skipped days), written out again in Rust; the unit tests
// pin it to a real-shaped fixture. Two gaps, both reported in `notes`:
//   * `tracker.py` also finds films that only exist inside a project folder under
//     videos/. Those are not packaged yet, so they are not counted here.
//   * It checks that the video, thumbnail and caption files exist. Boo trusts
//     status.json for that, which matches the real queue today.
//
// instagram.json (the access token) and alerts.json (the ntfy topic) are secrets
// and are deliberately never opened.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// Where the board lives unless the user says otherwise.
pub const DEFAULT_DIR: &str = r"D:\Work\Claude\videos\_publish";
/// `tracker.py board` listens here, on this PC only.
pub const BOARD_PORT: u16 = 8765;
/// The daily "DYE Instagram post" task, Manila time (schedule.py POST_TIME).
const POST_TIME: &str = "15:00";
/// Manila is UTC+8 all year; the board stamps dates in it.
const MANILA: i64 = 8 * 3600;
/// A render nobody has touched for this long is a leftover, not a render.
const RENDER_STALE_SECS: i64 = 6 * 3600;
const EDIT_TMP_STALE_SECS: u64 = 15 * 60;

// ── Dates (Howard Hinnant's civil-days algorithms) ───────────────────────────

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DD` as a day number.
fn parse_day(s: &str) -> Option<i64> {
    let s = s.get(..10)?;
    let mut p = s.split('-');
    let (y, m, d) = (p.next()?.parse().ok()?, p.next()?.parse().ok()?, p.next()?.parse().ok()?);
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then(|| days_from_civil(y, m, d))
}

fn day_string(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `2026-10-01T13:53:09+08:00` (seconds, fraction and offset optional) as Unix
/// seconds. A stamp with no offset is read as Manila time, which is what the
/// board writes.
fn parse_epoch(s: &str) -> Option<i64> {
    let day = parse_day(s)?;
    let rest = s.get(11..).unwrap_or("");
    let (clock, offset) = match rest.find(['+', '-', 'Z']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let mut c = clock.split(':');
    let h: i64 = c.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let m: i64 = c.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let sec: i64 = c.next().and_then(|x| x.split('.').next()?.parse().ok()).unwrap_or(0);
    let off = if offset.is_empty() {
        MANILA
    } else if offset == "Z" {
        0
    } else {
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let mut o = offset[1..].split(':');
        let oh: i64 = o.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let om: i64 = o.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        sign * (oh * 3600 + om * 60)
    };
    Some(day * 86_400 + h * 3600 + m * 60 + sec - off)
}

// ── What the board knows ─────────────────────────────────────────────────────

fn series_of(name: &str) -> &'static str {
    let low = name.to_lowercase();
    if low.starts_with("worth a try") || low.starts_with("not convinced") || low.starts_with("aon-hero") || low.contains("active-or-not") {
        "AoN"
    } else if low.starts_with("gh-") {
        "GH"
    } else if low.starts_with("qderm") {
        "Qderm"
    } else if low.starts_with("clear-margins") || low.starts_with("cm-") {
        "Clear Margins"
    } else {
        "DYE"
    }
}

fn posts(series: &str) -> bool {
    series == "DYE" || series == "Clear Margins"
}

#[derive(PartialEq, Debug, Clone, Copy)]
enum Stage {
    Skip,
    Issues,
    NeedsApproval,
    WaitingDrL,
    NeedsCheck,
    Ready,
}

/// Where a film stands, in the order tracker.outstanding() asks its questions.
fn stage(name: &str, s: &Value, now: i64, uploaded: &HashSet<String>) -> Stage {
    if !posts(series_of(name)) || s["posted"].as_bool() == Some(true) || uploaded.contains(name) {
        return Stage::Skip;
    }
    if let Some(h) = s["hold_until"].as_str() {
        if parse_epoch(h).is_some_and(|t| t > now) {
            return Stage::Skip;
        }
    }
    if s["issues"].as_array().is_some_and(|i| !i.is_empty()) {
        return Stage::Issues;
    }
    if s["approved"].as_bool() != Some(true) {
        return Stage::NeedsApproval;
    }
    let dr = s["dr_l"].as_str().unwrap_or("");
    let by = s["approved_by"].as_str().unwrap_or("");
    if dr == "needed" || dr == "pending" || (dr != "approved" && dr != "grandfathered" && by != "Dr L") {
        return Stage::WaitingDrL;
    }
    if s["checked"].as_bool() != Some(true) {
        return Stage::NeedsCheck;
    }
    Stage::Ready
}

fn colour(s: &Value) -> String {
    s["colour"].as_str().unwrap_or("").to_string()
}

/// schedule.colour_runs: one run per shirt colour, biggest first, `lead` first.
fn colour_runs(names: Vec<String>, status: &Map<String, Value>, lead: Option<&str>) -> Vec<String> {
    let col = |n: &str| status.get(n).map(colour).unwrap_or_default();
    let count = |c: &str| names.iter().filter(|n| col(n) == c).count() as i64;
    let mut out = names.clone();
    out.sort_by_key(|n| {
        let c = col(n);
        (Some(c.as_str()) != lead, c.is_empty(), -count(&c), c.clone(), n.clone())
    });
    out
}

struct Row {
    name: String,
    date: String,
    time: String,
    thumb_ok: bool,
    colour: String,
}

/// schedule.queue(): the films that will post, in order, with their days.
fn queue(ready: &BTreeSet<String>, status: &Map<String, Value>, sched: &Value, today: i64) -> Vec<Row> {
    let get = |n: &str| status.get(n).cloned().unwrap_or(Value::Null);
    let focus: Vec<String> = ready.iter().filter(|n| get(n)["focus"].as_bool() == Some(true)).cloned().collect();
    let focus = colour_runs(focus, status, None);
    let lead = focus.last().map(|n| colour(&get(n))).filter(|c| !c.is_empty());
    let rest: Vec<String> = ready.iter().filter(|n| !focus.contains(n)).cloned().collect();
    let rest = colour_runs(rest, status, lead.as_deref());
    let mut names: Vec<String> = focus.into_iter().chain(rest).collect();

    // Films Jhon dragged keep his order; anything newer follows.
    let order: Vec<String> = sched["order"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let placed: Vec<String> = order.into_iter().filter(|n| names.contains(n)).collect();
    names.retain(|n| !placed.contains(n));
    names = placed.into_iter().chain(names).collect();

    // A one-off post (`instagram.py now`) keeps its own day and time.
    let extra: Vec<String> = names.iter().filter(|n| get(n)["post_at"].as_str().is_some()).cloned().collect();
    names.retain(|n| !extra.contains(n));

    let every = sched["every_days"].as_i64().unwrap_or(1).max(1);
    let skip: HashSet<i64> = sched["skip_dates"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().and_then(parse_day)).collect()).unwrap_or_default();
    let start = sched["start"].as_str().and_then(parse_day);

    let mut rows = Vec::new();
    let mut days = Vec::new();
    if let Some(start) = start {
        // first_slot: not before the start date, not today if something went out
        // today, and a full gap after the last post.
        let mut first = today.max(start);
        if let Some(last) = status.values().filter_map(|s| s["posted_on"].as_str()).filter_map(parse_day).max() {
            first = first.max(last + every);
        }
        let mut day = first;
        while days.len() < names.len() {
            if !skip.contains(&day) {
                days.push(day);
            }
            day += every;
        }
    }
    for (i, name) in names.iter().enumerate() {
        let s = get(name);
        rows.push(Row {
            name: name.clone(),
            date: days.get(i).map(|d| day_string(*d)).unwrap_or_default(),
            time: POST_TIME.into(),
            thumb_ok: s["thumb_ok"].as_bool() == Some(true),
            colour: colour(&s),
        });
    }
    for name in extra {
        let s = get(&name);
        let at = s["post_at"].as_str().unwrap_or("");
        rows.push(Row {
            date: at.get(..10).unwrap_or("").into(),
            time: at.get(11..16).unwrap_or(POST_TIME).into(),
            thumb_ok: s["thumb_ok"].as_bool() == Some(true),
            colour: colour(&s),
            name,
        });
    }
    rows.sort_by(|a, b| {
        let key = |r: &Row| (if r.date.is_empty() { "9999".to_string() } else { r.date.clone() }, r.time.clone());
        key(a).cmp(&key(b))
    });
    rows
}

// ── Files ────────────────────────────────────────────────────────────────────

/// `Ok(None)` when the file is not there; an error when it is but cannot be used
/// — the board's status.json was once found zero-filled after a power cut, and
/// "nothing is waiting" would be a lie then.
fn read_json(path: &Path) -> Result<Option<Value>, String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Can't read {}: {e}", path.file_name().unwrap_or_default().to_string_lossy())),
    };
    let text = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    serde_json::from_slice(text)
        .map(Some)
        .map_err(|_| format!("{} isn't valid JSON right now", path.file_name().unwrap_or_default().to_string_lossy()))
}

fn stems(dir: &Path, ext: &str, deep: bool) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if deep {
                    todo.push(p);
                }
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case(ext)) {
                if let Some(s) = p.file_stem() {
                    out.insert(s.to_string_lossy().to_string());
                }
            }
        }
    }
    out
}

fn film_title(name: &str) -> String {
    // The board stores a few captions' `?` as an HTML entity in file names.
    name.replace("&#x3f;", "?")
}

/// What the island shows. `now` is Unix seconds; `board_up` is whether
/// `tracker.py board` answers on this PC (a render only lives while it does).
pub fn read(dir: &Path, now: i64, board_up: bool) -> Result<Value, String> {
    if !dir.is_dir() {
        return Err(format!("Board folder not found: {}", dir.display()));
    }
    let status_v = read_json(&dir.join("status.json"))?.ok_or("status.json is missing in the board folder")?;
    let status = status_v.as_object().ok_or("status.json isn't a JSON object")?.clone();
    let sched = read_json(&dir.join("schedule.json"))?.unwrap_or(Value::Null);
    let uploads = read_json(&dir.join("uploads.json")).unwrap_or(None).unwrap_or(Value::Null);

    // Films are packaged under videos/_FINAL as a video, thumbnail and caption.
    let final_dir = dir.parent().map(|p| p.join("_FINAL")).unwrap_or_default();
    let vids = stems(&final_dir.join("1 - Videos"), "mp4", true);
    let thumbs = stems(&final_dir.join("2 - Thumbnails"), "jpg", false);
    let caps = stems(&final_dir.join("3 - Captions"), "md", false);
    let uploaded = stems(&final_dir.join("Uploaded"), "mp4", false);

    let mut names: BTreeSet<String> = status.keys().cloned().collect();
    names.extend(vids.iter().filter(|n| thumbs.contains(*n) && caps.contains(*n)).cloned());

    let mut counts = [0usize; 6];
    let mut ready = BTreeSet::new();
    for name in &names {
        let s = status.get(name).cloned().unwrap_or(Value::Null);
        match stage(name, &s, now, &uploaded) {
            Stage::Skip => {}
            Stage::Issues => counts[0] += 1,
            Stage::NeedsApproval => counts[1] += 1,
            Stage::WaitingDrL => counts[2] += 1,
            Stage::NeedsCheck => counts[3] += 1,
            Stage::Ready => {
                ready.insert(name.clone());
            }
        }
    }

    let today = (now + MANILA).div_euclid(86_400);
    let rows = queue(&ready, &status, &sched, today);
    let thumb_pending = rows.iter().filter(|r| !r.thumb_ok).count();

    let paused = sched["paused"].as_bool() == Some(true);
    let has_start = sched["start"].as_str().is_some_and(|s| !s.is_empty());
    let next = rows.first().map(|r| json!({
        "title": film_title(&r.name), "date": r.date, "time": r.time,
        "thumbOk": r.thumb_ok, "colour": r.colour,
    }));
    let next_note = if paused {
        "Posting is paused"
    } else if !has_start {
        "Start date not set"
    } else if rows.first().is_some_and(|r| !r.thumb_ok) {
        "Thumbnail not OK'd, so posting waits"
    } else {
        ""
    };

    let last_posted = status
        .iter()
        .filter_map(|(n, s)| Some((s["posted_on"].as_str()?.to_string(), n.clone())))
        .max()
        .map(|(date, n)| json!({ "title": film_title(&n), "date": date }));

    // ── Rendering ──
    let mut rendering: Vec<Value> = Vec::new();
    let mut waiting_jobs = 0;
    if let Some(jobs) = uploads["jobs"].as_array() {
        for j in jobs.iter().filter(|j| j["hidden"].as_bool() != Some(true)) {
            let title = film_title(j["name"].as_str().unwrap_or("?"));
            let fresh = |key: &str| j[key].as_str().and_then(parse_epoch).is_some_and(|t| now - t < RENDER_STALE_SECS);
            match j["state"].as_str() {
                Some("editing") if fresh("started") => rendering.push(json!({ "title": title, "kind": "film", "detail": "being edited" })),
                Some("uploading") if fresh("created") => rendering.push(json!({ "title": title, "kind": "film", "detail": "uploading" })),
                Some("waiting") => waiting_jobs += 1,
                _ => {}
            }
            if let Some(r) = j["revisions"].as_array().and_then(|a| a.last()) {
                if r["state"].as_str() == Some("working") && r["started"].as_str().and_then(parse_epoch).is_some_and(|t| now - t < RENDER_STALE_SECS) {
                    rendering.push(json!({ "title": title, "kind": "revision", "detail": "revising" }));
                }
            }
        }
    }
    // Board edits (noise, pace, colour) render inside the board process. The state
    // is only believable while the board runs and an ffmpeg temp file is moving.
    let tmp_moving = std::fs::read_dir(dir.join(".edits")).ok().is_some_and(|d| {
        d.flatten().any(|e| {
            e.file_name().to_string_lossy().ends_with(".tmp.mp4")
                && e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_secs() < EDIT_TMP_STALE_SECS)
        })
    });
    if board_up && tmp_moving {
        for (n, s) in &status {
            if s["edit"]["state"].as_str() == Some("rendering") {
                rendering.push(json!({ "title": film_title(n), "kind": "edit", "detail": "rendering edit" }));
            }
        }
    }

    Ok(json!({
        "dir": dir.to_string_lossy(),
        "boardUp": board_up,
        "counts": {
            "issues": counts[0], "approval": counts[1], "drl": counts[2], "check": counts[3],
            "thumb": thumb_pending, "ready": rows.len() - thumb_pending,
        },
        "queueLen": rows.len(),
        "next": next,
        "nextNote": next_note,
        "upcoming": rows.iter().skip(1).take(3).map(|r| json!({ "title": film_title(&r.name), "date": r.date })).collect::<Vec<_>>(),
        "paused": paused,
        "rendering": rendering,
        "waitingJobs": waiting_jobs,
        "lastPosted": last_posted,
        "notes": [
            "Films that only exist in a project folder (not packaged yet) are not counted.",
            "Rendering comes from uploads.json and status.json, which only the board's own run keeps true.",
        ],
    }))
}

/// Does `tracker.py board` answer on this PC?
pub async fn board_up() -> bool {
    use std::time::Duration;
    matches!(
        tokio::time::timeout(Duration::from_millis(400), tokio::net::TcpStream::connect(("127.0.0.1", BOARD_PORT))).await,
        Ok(Ok(_))
    )
}

pub fn dir_from(setting: &str) -> PathBuf {
    let s = setting.trim();
    PathBuf::from(if s.is_empty() { DEFAULT_DIR } else { s })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-02 22:40 Manila.
    fn now() -> i64 {
        parse_epoch("2026-10-02T22:40:00+08:00").unwrap()
    }

    #[test]
    fn dates_round_trip_and_stamps_read_with_their_offset() {
        assert_eq!(day_string(parse_day("2026-10-02").unwrap()), "2026-10-02");
        assert_eq!(day_string(parse_day("2026-10-02").unwrap() + 30), "2026-11-01");
        assert_eq!(parse_epoch("1970-01-01T08:00:00+08:00"), Some(0));
        assert_eq!(parse_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_epoch("1970-01-01"), Some(-8 * 3600), "no offset means Manila");
        assert!(parse_epoch("2099-12-31T00:00:00+08:00").unwrap() > now());
        assert_eq!(parse_epoch("nonsense"), None);
    }

    #[test]
    fn series_follow_the_boards_naming_rules() {
        assert_eq!(series_of("Worth a try - copper peptide"), "AoN");
        assert_eq!(series_of("gh-sibo"), "GH");
        assert_eq!(series_of("qderm-pdt"), "Qderm");
        assert_eq!(series_of("clear-margins-ep3"), "Clear Margins");
        assert_eq!(series_of("cm-pimple-bcc"), "Clear Margins");
        assert_eq!(series_of("Riding shotgun"), "DYE");
    }

    fn fixture() -> (PathBuf, Value) {
        let status = json!({
            // Ready, thumbnail OK'd, olive.
            "Friday PM": { "approved": true, "checked": true, "dr_l": "approved", "thumb_ok": true, "colour": "olive", "issues": [], "hold_until": null },
            // Ready, thumbnail not OK'd yet, black.
            "Stress": { "approved": true, "checked": true, "dr_l": "grandfathered", "colour": "black" },
            "Scabies": { "approved": true, "checked": true, "dr_l": "grandfathered", "colour": "olive", "thumb_ok": true },
            // Waiting on Dr L, on a check, on an approval, with an open issue.
            "Quote": { "approved": true, "dr_l": "pending", "checked": true },
            "Keloid ear": { "approved": true, "dr_l": "approved" },
            "Vitiligo": { "approved": false },
            "Derms are fussy": { "approved": true, "dr_l": "approved", "checked": true, "issues": ["thumbnail"] },
            // Not in the queue at all: posted, held, test, other series.
            "Nose BCC": { "approved": true, "checked": true, "dr_l": "approved", "posted": true, "posted_on": "2026-10-02" },
            "Held one": { "approved": true, "checked": true, "dr_l": "approved", "hold_until": "2099-12-31T00:00:00+08:00" },
            "qderm-pdt": { "approved": true, "checked": true, "dr_l": "approved" },
            "aon-hero-ahas": { "approved": false }
        });
        let sched = json!({ "start": "2026-09-17", "every_days": 1, "paused": false, "skip_dates": ["2026-10-04"], "order": [] });
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = PathBuf::from("D:/Temp").join(format!("boo-board-{}-{n}", std::process::id())).join("_publish");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("status.json"), status.to_string()).unwrap();
        std::fs::write(dir.join("schedule.json"), sched.to_string()).unwrap();
        (dir, status)
    }

    #[test]
    fn the_queue_matches_what_schedule_py_would_build() {
        let (dir, _) = fixture();
        let v = read(&dir, now(), false).unwrap();
        let c = &v["counts"];
        assert_eq!((c["approval"].as_u64(), c["drl"].as_u64(), c["check"].as_u64(), c["issues"].as_u64()),
                   (Some(1), Some(1), Some(1), Some(1)));
        assert_eq!(v["queueLen"], 3);
        assert_eq!(c["thumb"], 1);
        assert_eq!(c["ready"], 2);
        // Olive is the bigger colour run, so it leads; the last post was today, so
        // the first free day is tomorrow, 3 Oct. 4 Oct is skipped.
        assert_eq!(v["next"]["title"], "Friday PM");
        assert_eq!(v["next"]["date"], "2026-10-03");
        assert_eq!(v["next"]["time"], "15:00");
        assert_eq!(v["next"]["thumbOk"], true);
        assert_eq!(v["nextNote"], "");
        assert_eq!(v["lastPosted"]["title"], "Nose BCC");
        let rows = queue(&["Friday PM", "Scabies", "Stress"].map(String::from).into_iter().collect(), &json!({
            "Friday PM": {"colour":"olive"}, "Scabies": {"colour":"olive"}, "Stress": {"colour":"black"}
        }).as_object().unwrap().clone(), &json!({"start":"2026-09-17","every_days":1,"skip_dates":["2026-10-04"]}), parse_day("2026-10-03").unwrap());
        let days: Vec<_> = rows.iter().map(|r| (r.name.as_str(), r.date.as_str())).collect();
        assert_eq!(days, vec![("Friday PM", "2026-10-03"), ("Scabies", "2026-10-05"), ("Stress", "2026-10-06")]);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn a_paused_board_or_a_missing_start_date_is_said_out_loud() {
        let (dir, _) = fixture();
        std::fs::write(dir.join("schedule.json"), r#"{"start":"","paused":false}"#).unwrap();
        let v = read(&dir, now(), false).unwrap();
        assert_eq!(v["nextNote"], "Start date not set");
        assert_eq!(v["next"]["date"], "");
        std::fs::write(dir.join("schedule.json"), r#"{"start":"2026-09-17","paused":true}"#).unwrap();
        assert_eq!(read(&dir, now(), false).unwrap()["nextNote"], "Posting is paused");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn renders_come_from_the_upload_jobs_and_ignore_leftovers() {
        let (dir, _) = fixture();
        let jobs = json!({ "jobs": [
            { "name": "Wart treatment", "state": "editing", "started": "2026-10-02T21:00:00+08:00" },
            { "name": "Old crash", "state": "editing", "started": "2026-09-23T18:10:00+08:00" },
            { "name": "Queued film", "state": "waiting" },
            { "name": "Hidden", "state": "editing", "started": "2026-10-02T21:30:00+08:00", "hidden": true },
            { "name": "Revised", "state": "done", "revisions": [{ "state": "working", "started": "2026-10-02T22:00:00+08:00" }] }
        ]});
        std::fs::write(dir.join("uploads.json"), jobs.to_string()).unwrap();
        let v = read(&dir, now(), true).unwrap();
        let titles: Vec<_> = v["rendering"].as_array().unwrap().iter().map(|r| r["title"].as_str().unwrap().to_string()).collect();
        assert_eq!(titles, vec!["Wart treatment", "Revised"]);
        assert_eq!(v["waitingJobs"], 1);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn a_damaged_status_file_is_an_error_not_an_empty_board() {
        let (dir, _) = fixture();
        std::fs::write(dir.join("status.json"), vec![0u8; 64]).unwrap();
        assert!(read(&dir, now(), false).unwrap_err().contains("status.json"));
        std::fs::remove_file(dir.join("status.json")).unwrap();
        assert!(read(&dir, now(), false).is_err());
        assert!(read(Path::new("D:/Temp/definitely-not-here"), now(), false).is_err());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// Reads the real board on this PC: `cargo test real_board -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_board() {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        let v = read(Path::new(DEFAULT_DIR), now, false).unwrap();
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    }
}
