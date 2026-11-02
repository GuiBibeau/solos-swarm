//! The one pause gate over a directory: `control.json`, a `PAUSE` file and,
//! with `SWARM_REFUSE_PEAK=1`, the weekday peak windows.

use serde_json::{Value, json};
use std::{fs, path::Path};

/// Seconds until the weekday peak window ends when `now_secs` (Unix, UTC) falls inside one: Mon-Fri 01:00-04:00 and
/// 06:00-10:00. No model call may start inside it when `SWARM_REFUSE_PEAK=1`, so no tokens are bought at peak price.
pub fn peak_retry_after(now_secs: u64) -> Option<u64> {
    let weekday = (now_secs / 86_400 + 4) % 7; // 1970-01-01 was a Thursday; 0 = Sunday
    if !(1..=5).contains(&weekday) {
        return None;
    }
    let second_of_day = now_secs % 86_400;
    [(3_600, 14_400), (21_600, 36_000)]
        .into_iter()
        .find(|(start, end)| (*start..*end).contains(&second_of_day))
        .map(|(_, end)| end - second_of_day)
}

/// The one pause test. `control.json` in the run directory is the source of truth: an operator or the broker
/// itself (provider outage, drain) writes `{"mode":"pause"|"drain"}` there. Two inputs feed the same answer: a
/// `PAUSE` file (the older operator switch) and, with `SWARM_REFUSE_PEAK=1`, a peak window. Returns the polling
/// interval the client should wait (at most 60 s). A paused request is refused BEFORE any reservation, so it
/// costs no call, no hold and no ledger entry.
pub fn effective_control(dir: &Path, now_secs: u64) -> Option<u64> {
    let mode = fs::read_to_string(dir.join("control.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|control| control["mode"].as_str().map(str::to_owned));
    if matches!(mode.as_deref(), Some("pause" | "drain")) {
        return Some(30);
    }
    if dir.join("PAUSE").exists() {
        return Some(60);
    }
    if std::env::var("SWARM_REFUSE_PEAK").as_deref() == Ok("1") {
        return peak_retry_after(now_secs).map(|s| s.clamp(1, 60));
    }
    None
}

/// The body of the 503 a paused broker answers `/model` with. The Pi
/// extension waits `retry_after_s` and asks again.
pub fn paused_reply(wait: u64) -> Value {
    json!({"paused":true,"retry_after_s":wait,"reason":"model calls are paused (operator pause or peak window)"})
}

pub fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
