//! Public Codex / ChatGPT Work reset radar. This is an announcement feed, not an account reading:
//! probabilities are historical estimates and only confirmed, non-preview resets produce alerts.

use crate::AppState;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Read;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

pub const SOURCE_NAME: &str = "codex-reset.com";
pub const SOURCE_URL: &str = "https://codex-reset.com/";
const TIMELINE_URL: &str = "https://codex-reset.com/api/timeline";
const FORECAST_URL: &str = "https://codex-reset.com/api/forecast";
const POLL_MS: u64 = 60_000;
const STALE_MS: u64 = 10 * 60_000;
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const EVENT: &str = "global_reset_state";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ConfirmedReset {
    pub id: String,
    pub announced_at: u64,
    pub summary: String,
    pub url: Option<String>,
    pub audience: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Forecast {
    pub probability_24h: Option<u8>,
    pub probability_48h: Option<u8>,
    pub confidence: String,
    pub confidence_note: String,
    pub last_reset_at: Option<u64>,
    pub age_days: Option<f64>,
    pub official_signal: Option<String>,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    /// loading = no successful read yet; stale/offline retain the last successful data.
    pub status: String,
    pub refreshing: bool,
    pub cached: bool,
    pub fetched_at: u64,
    pub updated_at: u64,
    pub backoff_until: u64,
    pub next_check_at: u64,
    pub error: String,
    pub source_name: String,
    pub source_url: String,
    pub notifications: bool,
    pub last_reset: Option<ConfirmedReset>,
    pub history: Vec<ConfirmedReset>,
    pub forecast: Option<Forecast>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            status: "loading".into(),
            refreshing: false,
            cached: false,
            fetched_at: 0,
            updated_at: 0,
            backoff_until: 0,
            next_check_at: 0,
            error: String::new(),
            source_name: SOURCE_NAME.into(),
            source_url: SOURCE_URL.into(),
            notifications: true,
            last_reset: None,
            history: Vec::new(),
            forecast: None,
        }
    }
}

/// The high-watermark is persisted before a card is queued. Starting/restarting Codenotch cannot
/// turn a historical announcement into a new alert. A reclassified/re-IDed row at the same
/// timestamp is the same reset instant and stays quiet.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    pub snapshot: Snapshot,
    initialized: bool,
    high_watermark_at: u64,
    high_watermark_ids: Vec<String>,
}

impl Store {
    fn observe(&mut self, resets: &[ConfirmedReset], now: u64) -> Option<ConfirmedReset> {
        let newest = resets.first();
        if !self.initialized {
            self.initialized = true;
            self.high_watermark_at = newest.map(|e| e.announced_at).unwrap_or(now);
            self.high_watermark_ids = resets
                .iter()
                .filter(|e| e.announced_at == self.high_watermark_at)
                .map(|e| e.id.clone())
                .collect();
            return None;
        }
        let new = resets
            .iter()
            .find(|e| e.announced_at > self.high_watermark_at)
            .cloned();
        if let Some(latest) = newest {
            if latest.announced_at > self.high_watermark_at {
                self.high_watermark_at = latest.announced_at;
                self.high_watermark_ids.clear();
            }
            for event in resets
                .iter()
                .filter(|e| e.announced_at == self.high_watermark_at)
            {
                if !self.high_watermark_ids.contains(&event.id) {
                    self.high_watermark_ids.push(event.id.clone());
                }
            }
        }
        new
    }

    fn view(&self, notifications: bool, now: u64) -> Snapshot {
        let mut snapshot = self.snapshot.clone();
        snapshot.notifications = notifications;
        if snapshot.status == "ok" && now.saturating_sub(snapshot.fetched_at) > STALE_MS {
            snapshot.status = "stale".into();
            snapshot.cached = true;
        }
        snapshot
    }
}

fn store_path() -> std::path::PathBuf {
    crate::config::config_path().with_file_name("global-resets.json")
}

pub fn load_persisted() -> Store {
    let mut store: Store = std::fs::read_to_string(store_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    store.snapshot.refreshing = false;
    store.snapshot.source_name = SOURCE_NAME.into();
    store.snapshot.source_url = SOURCE_URL.into();
    if store.snapshot.fetched_at > 0 {
        store.snapshot.status = "stale".into();
        store.snapshot.cached = true;
    }
    store
}

fn persist(store: &Store) -> Result<(), String> {
    let path = store_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let temporary = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(store).map_err(|e| e.to_string())?;
    std::fs::write(&temporary, json).map_err(|e| e.to_string())?;
    std::fs::rename(temporary, path).map_err(|e| e.to_string())
}

fn timestamp(value: Option<&Value>) -> Option<u64> {
    let at = chrono::DateTime::parse_from_rfc3339(value?.as_str()?)
        .ok()?
        .timestamp_millis();
    u64::try_from(at).ok().filter(|at| *at > 0)
}

fn text(value: Option<&Value>, limit: usize) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .take(limit)
        .collect()
}

fn source_link(value: Option<&Value>) -> Option<String> {
    let url = value?.as_str()?;
    // Feed strings are never opened as shell arguments; only known HTTPS announcement hosts are
    // retained for display. The UI opens SOURCE_URL through a fixed native command.
    [
        "https://x.com/",
        "https://twitter.com/",
        "https://codex-reset.com/",
    ]
    .iter()
    .any(|prefix| url.starts_with(prefix))
    .then(|| url.chars().take(512).collect())
}

fn parse_timeline(value: &Value, now: u64) -> Result<(u64, Vec<ConfirmedReset>), String> {
    // Timeline update metadata is useful but not part of the documented stable event contract.
    // Publication freshness comes from the response headers; an omitted date stays unknown.
    let updated_at = timestamp(value.get("updated_at")).unwrap_or(0);
    let events = value
        .get("events")
        .and_then(Value::as_array)
        .ok_or("Timeline has no events array")?;
    let mut resets = Vec::new();
    for event in events.iter().take(2000) {
        if event.get("group").and_then(Value::as_str) != Some("reset")
            || event.get("announcement_state").and_then(Value::as_str) != Some("announced")
            || event
                .get("preview")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            continue;
        }
        let Some(announced_at) = timestamp(event.get("announced_at")).filter(|at| *at <= now)
        else {
            continue;
        };
        let id = text(event.get("id"), 128);
        if id.is_empty() || resets.iter().any(|e: &ConfirmedReset| e.id == id) {
            continue;
        }
        let audience = event
            .get("audience")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .take(20)
                    .map(|s| s.chars().take(64).collect())
                    .collect()
            })
            .unwrap_or_default();
        resets.push(ConfirmedReset {
            id,
            announced_at,
            summary: text(event.get("summary"), 1000),
            url: source_link(event.get("url")),
            audience,
        });
    }
    resets.sort_by(|a, b| {
        b.announced_at
            .cmp(&a.announced_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    Ok((updated_at, resets))
}

fn probability(value: Option<&Value>) -> Option<u8> {
    value?
        .as_f64()
        .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
        .map(|v| v.round() as u8)
}

fn parse_forecast(value: &Value) -> Result<Forecast, String> {
    let updated_at =
        timestamp(value.get("updated_at")).ok_or("Forecast has no valid update time")?;
    let probabilities = value
        .get("probabilities")
        .and_then(Value::as_object)
        .ok_or("Forecast has no probabilities object")?;
    let signal = value
        .get("official_signal")
        .and_then(|v| {
            if v.is_null() {
                None
            } else if v.is_string() {
                Some(text(Some(v), 250))
            } else {
                ["label", "summary", "text"].iter().find_map(|key| {
                    v.get(*key)
                        .and_then(Value::as_str)
                        .map(|s| s.chars().take(250).collect())
                })
            }
        })
        .filter(|s: &String| !s.is_empty());
    Ok(Forecast {
        probability_24h: probability(probabilities.get("rounded_24h")),
        probability_48h: probability(probabilities.get("rounded_48h")),
        confidence: text(value.get("confidence"), 40),
        confidence_note: text(value.get("confidence_note"), 500),
        last_reset_at: timestamp(value.get("last_reset_at")),
        age_days: value
            .get("age_days")
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && *v >= 0.0),
        official_signal: signal,
        updated_at,
    })
}

struct FetchError {
    message: String,
    backoff_until: u64,
}

fn retry_after_ms(header: Option<&str>, now: u64) -> u64 {
    let requested = header
        .and_then(|s| {
            s.trim()
                .parse::<u64>()
                .ok()
                .map(|seconds| now.saturating_add(seconds.saturating_mul(1000)))
                .or_else(|| {
                    chrono::DateTime::parse_from_rfc2822(s)
                        .ok()
                        .and_then(|at| u64::try_from(at.timestamp_millis()).ok())
                })
        })
        .unwrap_or(0);
    now.saturating_add(POLL_MS).max(requested)
}

fn published_time(header: Option<&str>) -> Option<u64> {
    let raw = header?.trim();
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .and_then(|at| u64::try_from(at.timestamp_millis()).ok())
        .or_else(|| {
            raw.parse::<u64>().ok().map(|at| {
                if at < 10_000_000_000 {
                    at.saturating_mul(1000)
                } else {
                    at
                }
            })
        })
        .filter(|at| *at > 0)
}

fn fetch(url: &str, now: u64) -> Result<Value, FetchError> {
    let user_agent = format!(
        "Codenotch/{} (+https://github.com/ThePunisherai/codenotch)",
        env!("CARGO_PKG_VERSION")
    );
    let response = ureq::get(url)
        .set("User-Agent", &user_agent)
        .set("Accept", "application/json")
        .timeout(Duration::from_secs(15))
        .call();
    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::Status(429, response)) => {
            return Err(FetchError {
                message: "Reset source is rate limited; waiting for Retry-After".into(),
                backoff_until: retry_after_ms(response.header("Retry-After"), crate::now_ms()),
            })
        }
        Err(ureq::Error::Status(status, _)) => {
            return Err(FetchError {
                message: format!("Reset source returned HTTP {status}"),
                backoff_until: 0,
            })
        }
        Err(_) => {
            return Err(FetchError {
                message: "Cannot reach reset source. Last saved data is shown.".into(),
                backoff_until: 0,
            })
        }
    };
    let response_at = crate::now_ms().max(now);
    let published_at = published_time(response.header("x-published-checked-at"));
    let expires_at = published_time(response.header("x-published-expires-at"));
    if expires_at.is_some_and(|at| at < response_at)
        || published_at.is_some_and(|at| response_at.saturating_sub(at) > STALE_MS)
    {
        return Err(FetchError {
            message: "Reset source's published copy is stale; waiting for a fresh update".into(),
            backoff_until: 0,
        });
    }
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| FetchError {
            message: "Could not read reset source response".into(),
            backoff_until: 0,
        })?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(FetchError {
            message: "Reset source response is too large".into(),
            backoff_until: 0,
        });
    }
    serde_json::from_slice(&bytes).map_err(|_| FetchError {
        message: "Reset source returned invalid JSON".into(),
        backoff_until: 0,
    })
}

fn broadcast(app: &AppHandle) {
    let _ = app.emit(EVENT, get_global_reset_state(app.clone()));
}

fn poll(app: &AppHandle) {
    let now = crate::now_ms();
    let mut store = {
        let state = app.state::<AppState>();
        let mut store = state.global_resets.lock().unwrap();
        if store.snapshot.refreshing
            || now
                < store
                    .snapshot
                    .next_check_at
                    .max(store.snapshot.backoff_until)
        {
            return;
        }
        store.snapshot.next_check_at = now.saturating_add(POLL_MS);
        store.snapshot.refreshing = true;
        // Record the attempt too: repeatedly restarting must not evade the public API cadence.
        if let Err(error) = persist(&store) {
            crate::applog(&format!("global reset cache: {error}"));
        }
        store.clone()
    };
    broadcast(app);
    let mut errors = Vec::new();
    let mut new_reset = None;
    match fetch(TIMELINE_URL, now) {
        Ok(value) => match parse_timeline(&value, now) {
            Ok((updated_at, resets)) => {
                new_reset = store.observe(&resets, now);
                store.snapshot.updated_at = updated_at;
                store.snapshot.last_reset = resets.first().cloned();
                store.snapshot.history = resets.into_iter().take(6).collect();
            }
            Err(error) => errors.push(error),
        },
        Err(error) => {
            store.snapshot.backoff_until = store.snapshot.backoff_until.max(error.backoff_until);
            errors.push(error.message);
        }
    }
    // A 429 applies to the service: do not send a second request during its backoff.
    if store.snapshot.backoff_until <= now {
        match fetch(FORECAST_URL, now) {
            Ok(value) => match parse_forecast(&value) {
                Ok(forecast) => store.snapshot.forecast = Some(forecast),
                Err(error) => errors.push(error),
            },
            Err(error) => {
                store.snapshot.backoff_until =
                    store.snapshot.backoff_until.max(error.backoff_until);
                errors.push(error.message);
            }
        }
    }
    store.snapshot.refreshing = false;
    store.snapshot.next_check_at = crate::now_ms()
        .saturating_add(POLL_MS)
        .max(store.snapshot.backoff_until);
    if errors.is_empty() {
        store.snapshot.status = "ok".into();
        store.snapshot.cached = false;
        store.snapshot.fetched_at = crate::now_ms();
        store.snapshot.backoff_until = 0;
        store.snapshot.error.clear();
    } else {
        store.snapshot.status =
            if store.snapshot.last_reset.is_some() || store.snapshot.forecast.is_some() {
                "stale"
            } else {
                "offline"
            }
            .into();
        store.snapshot.cached = true;
        store.snapshot.error = errors.join(" · ");
    }
    // Persist before presenting so a crash/restart cannot replay a delivered announcement.
    let persisted = match persist(&store) {
        Ok(()) => true,
        Err(error) => {
            crate::applog(&format!("global reset cache: {error}"));
            store.snapshot.error =
                "Could not save reset history; notifications paused to prevent duplicates".into();
            false
        }
    };
    *app.state::<AppState>().global_resets.lock().unwrap() = store;
    broadcast(app);
    if persisted {
        if let Some(event) = new_reset {
            crate::reset_alert::enqueue_global(app, event);
        }
    }
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        crate::activity::lower_thread_priority();
        loop {
            poll(&app);
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

#[tauri::command]
pub fn get_global_reset_state(app: AppHandle) -> Snapshot {
    let state = app.state::<AppState>();
    let notifications = state.cfg.lock().unwrap().global_reset_notifications;
    let snapshot = state
        .global_resets
        .lock()
        .unwrap()
        .view(notifications, crate::now_ms());
    snapshot
}

#[tauri::command]
pub fn refresh_global_resets(app: AppHandle) -> Snapshot {
    // The worker checks once per second; a manual refresh shares its persisted 60-second deadline.
    // Returning immediately keeps Settings responsive while the network request runs.
    let worker = app.clone();
    std::thread::spawn(move || poll(&worker));
    get_global_reset_state(app)
}

#[tauri::command]
pub fn set_global_reset_notifications(app: AppHandle, on: bool) -> bool {
    {
        let state = app.state::<AppState>();
        let mut cfg = state.cfg.lock().unwrap();
        cfg.global_reset_notifications = on;
        crate::config::save(&cfg);
    }
    if !on {
        crate::reset_alert::disable_global(&app);
    }
    broadcast(&app);
    on
}

#[tauri::command]
pub fn preview_global_reset_alert(app: AppHandle) -> Result<(), String> {
    crate::reset_alert::preview_global(app)
}

#[tauri::command]
pub fn open_global_reset_source() {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", SOURCE_URL])
            .creation_flags(0x0800_0000)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open")
            .arg(SOURCE_URL)
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: u64 = 1_791_400_000_000;

    fn reset(id: &str, at: u64) -> ConfirmedReset {
        ConfirmedReset {
            id: id.into(),
            announced_at: at,
            summary: "Reset confirmed".into(),
            url: None,
            audience: vec!["codex".into()],
        }
    }

    #[test]
    fn timeline_ignores_hints_banked_previews_future_and_duplicate_rows() {
        let good = json!({"id":"confirmed","group":"reset","announcement_state":"announced","announced_at":"2026-10-07T03:35:09Z","summary":"Processed","url":"https://x.com/example/status/1"});
        let mut hint = good.clone();
        hint["id"] = json!("hint");
        hint["announcement_state"] = json!("none");
        let mut banked = good.clone();
        banked["id"] = json!("banked");
        banked["group"] = json!("credits");
        let mut preview = good.clone();
        preview["id"] = json!("preview");
        preview["preview"] = json!(true);
        let mut future = good.clone();
        future["id"] = json!("future");
        future["announced_at"] = json!("2099-01-01T00:00:00Z");
        let value = json!({"updated_at":"2026-10-07T19:00:00Z","events":[hint,banked,preview,future,good.clone(),good,{"id":"bad","group":"reset","announcement_state":"announced","announced_at":"bad"}]});
        let (_, resets) = parse_timeline(&value, NOW).unwrap();
        assert_eq!(resets.len(), 1);
        assert_eq!(resets[0].id, "confirmed");
        assert_eq!(
            resets[0].url.as_deref(),
            Some("https://x.com/example/status/1")
        );
        assert!(source_link(Some(&json!("javascript:alert(1)"))).is_none());
        assert!(parse_timeline(&json!({"events":{}}), NOW).is_err());
        let (unknown_update, empty) = parse_timeline(&json!({"events":[]}), NOW).unwrap();
        assert_eq!(unknown_update, 0);
        assert!(empty.is_empty());
    }

    #[test]
    fn startup_and_restart_never_replay_a_reset_but_new_announcements_alert_once() {
        let old = reset("old", 100);
        let new = reset("new", 200);
        let mut store = Store::default();
        assert!(store.observe(&[old.clone()], NOW).is_none());
        assert!(store.observe(&[old.clone()], NOW).is_none());
        assert_eq!(
            store.observe(&[new.clone(), old.clone()], NOW),
            Some(new.clone())
        );
        let saved = serde_json::to_string(&store).unwrap();
        let mut restarted: Store = serde_json::from_str(&saved).unwrap();
        assert!(restarted
            .observe(&[new.clone(), old.clone()], NOW)
            .is_none());
        assert!(restarted.observe(&[old], NOW).is_none());
        assert!(restarted.observe(&[new], NOW).is_none());
    }

    #[test]
    fn equal_timestamp_corrections_stay_quiet_and_empty_baseline_suppresses_old_history() {
        let mut store = Store::default();
        assert!(store.observe(&[], 1000).is_none());
        assert!(store.observe(&[reset("historical", 100)], 1001).is_none());
        assert!(store.observe(&[reset("first", 1100)], 1200).is_some());
        assert!(store
            .observe(&[reset("second", 1100), reset("first", 1100)], 1201)
            .is_none());
        assert!(store
            .observe(&[reset("second", 1100), reset("first", 1100)], 1202)
            .is_none());
    }

    #[test]
    fn forecast_is_a_separate_estimate_and_unknown_numbers_are_not_guessed() {
        let forecast = parse_forecast(&json!({"updated_at":"2026-10-07T19:00:00Z","probabilities":{"rounded_24h":15,"rounded_48h":28},"confidence":"low","confidence_note":"Experimental","last_reset_at":"2026-10-07T03:35:09Z","age_days":0.6,"official_signal":null})).unwrap();
        assert_eq!(forecast.probability_24h, Some(15));
        assert_eq!(forecast.probability_48h, Some(28));
        assert_eq!(forecast.confidence, "low");
        assert_eq!(forecast.official_signal, None);
        assert_eq!(probability(Some(&json!(101))), None);
        assert_eq!(probability(Some(&json!(-2))), None);
        assert_eq!(probability(Some(&json!("28"))), None);
        assert!(parse_forecast(&json!({"probabilities":{}})).is_err());
    }

    #[test]
    fn retry_after_seconds_or_http_date_can_only_raise_one_minute_floor() {
        assert_eq!(retry_after_ms(Some("120"), NOW), NOW + 120_000);
        assert_eq!(retry_after_ms(Some("1"), NOW), NOW + POLL_MS);
        assert_eq!(retry_after_ms(Some("invalid"), NOW), NOW + POLL_MS);
        let future = chrono::DateTime::from_timestamp_millis((NOW + 3600_000) as i64)
            .unwrap()
            .to_rfc2822();
        assert_eq!(retry_after_ms(Some(&future), NOW), NOW + 3600_000);
        assert_eq!(published_time(Some("1791400000")), Some(NOW));
        assert_eq!(published_time(Some("1791400000000")), Some(NOW));
        assert_eq!(published_time(Some("2026-10-07T19:06:40Z")), Some(NOW));
        assert_eq!(published_time(Some("invalid")), None);
    }

    #[test]
    fn saved_data_expires_honestly_without_losing_history() {
        let mut store = Store::default();
        store.snapshot.status = "ok".into();
        store.snapshot.fetched_at = 100;
        store.snapshot.last_reset = Some(reset("old", 50));
        let snapshot = store.view(false, 100 + STALE_MS + 1);
        assert_eq!(snapshot.status, "stale");
        assert!(snapshot.cached);
        assert!(!snapshot.notifications);
        assert_eq!(snapshot.last_reset.unwrap().id, "old");
    }
}
