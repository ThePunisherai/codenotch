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
const FEED_URL: &str = "https://codex-reset.com/api/feed";
const STATUS_URL: &str = "https://codex-reset.com/api/status-history";
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

/// Public announcements and lifecycle updates. Banked availability belongs to the public source,
/// never to the signed-in account, and lifecycle rows are not a count of grants.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PublicEvent {
    pub id: String,
    pub announced_at: u64,
    pub kind: String,
    pub group: String,
    pub announcement_state: String,
    pub banked_state: Option<String>,
    pub confirmed: bool,
    pub preview: bool,
    pub summary: String,
    pub url: Option<String>,
    pub audience: Vec<String>,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FeedSnapshot {
    /// Our successful check time, separate from the source's own ingestion/publication times.
    pub checked_at: u64,
    pub fetched_at: u64,
    pub source_checked_at: u64,
    pub source_expires_at: u64,
    pub newest_post_at: Option<u64>,
    pub stale: bool,
    pub signal: Option<PublicEvent>,
    pub posts: Vec<PublicEvent>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceSurface {
    pub id: String,
    pub label: String,
    pub status: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub checked_at: u64,
    pub status: String,
    pub description: String,
    pub stale: bool,
    pub surfaces: Vec<ServiceSurface>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    /// loading = no successful read yet; stale/offline retain the last successful data.
    pub status: String,
    pub refreshing: bool,
    pub cached: bool,
    pub fetched_at: u64,
    pub checked_at: u64,
    pub last_attempt_at: u64,
    pub source_checked_at: u64,
    pub source_expires_at: u64,
    pub poll_interval_ms: u64,
    pub updated_at: u64,
    pub backoff_until: u64,
    pub next_check_at: u64,
    pub error: String,
    pub source_name: String,
    pub source_url: String,
    pub notifications: bool,
    pub banked_notifications: bool,
    pub last_reset: Option<ConfirmedReset>,
    pub history: Vec<ConfirmedReset>,
    pub forecast: Option<Forecast>,
    pub events: Vec<PublicEvent>,
    pub banked: Option<PublicEvent>,
    pub banked_history: Vec<PublicEvent>,
    pub feed: Option<FeedSnapshot>,
    pub service: Option<ServiceStatus>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            status: "loading".into(),
            refreshing: false,
            cached: false,
            fetched_at: 0,
            checked_at: 0,
            last_attempt_at: 0,
            source_checked_at: 0,
            source_expires_at: 0,
            poll_interval_ms: POLL_MS,
            updated_at: 0,
            backoff_until: 0,
            next_check_at: 0,
            error: String::new(),
            source_name: SOURCE_NAME.into(),
            source_url: SOURCE_URL.into(),
            notifications: true,
            banked_notifications: true,
            last_reset: None,
            history: Vec::new(),
            forecast: None,
            events: Vec::new(),
            banked: None,
            banked_history: Vec::new(),
            feed: None,
            service: None,
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
    global_seen_ids: Vec<String>,
    banked_initialized: bool,
    banked_high_watermark_at: u64,
    banked_latest_id: String,
    banked_seen: Vec<String>,
    timeline_events: Vec<PublicEvent>,
}

impl Store {
    fn observe(&mut self, resets: &[ConfirmedReset], now: u64) -> Option<ConfirmedReset> {
        let newest = resets.first();
        if self.initialized && self.global_seen_ids.is_empty() {
            // Upgrade old cursors without replaying the most recent known announcement if the
            // source later corrects its timestamp. Only confirmed IDs enter this memory.
            for event in self.snapshot.history.iter().rev() {
                if !self.global_seen_ids.contains(&event.id) {
                    self.global_seen_ids.push(event.id.clone());
                }
            }
            for id in &self.high_watermark_ids {
                if !self.global_seen_ids.contains(id) {
                    self.global_seen_ids.push(id.clone());
                }
            }
        }
        if !self.initialized {
            self.initialized = true;
            self.high_watermark_at = newest.map(|e| e.announced_at).unwrap_or(now);
            self.high_watermark_ids = resets
                .iter()
                .filter(|e| e.announced_at == self.high_watermark_at)
                .map(|e| e.id.clone())
                .collect();
            self.global_seen_ids = resets
                .iter()
                .take(256)
                .rev()
                .map(|event| event.id.clone())
                .collect();
            return None;
        }
        let new = resets
            .iter()
            .find(|e| {
                e.announced_at > self.high_watermark_at && !self.global_seen_ids.contains(&e.id)
            })
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
        for event in resets.iter().rev() {
            if !self.global_seen_ids.contains(&event.id) {
                self.global_seen_ids.push(event.id.clone());
            }
        }
        if self.global_seen_ids.len() > 256 {
            self.global_seen_ids
                .drain(..self.global_seen_ids.len() - 256);
        }
        new
    }

    fn view(&self, notifications: bool, banked_notifications: bool, now: u64) -> Snapshot {
        let mut snapshot = self.snapshot.clone();
        snapshot.notifications = notifications;
        snapshot.banked_notifications = banked_notifications;
        if snapshot.status == "ok"
            && (now.saturating_sub(snapshot.fetched_at) > STALE_MS
                || snapshot.source_expires_at > 0 && snapshot.source_expires_at < now)
        {
            snapshot.status = "stale".into();
            snapshot.cached = true;
        }
        if let Some(feed) = snapshot.feed.as_mut() {
            feed.stale |= now.saturating_sub(feed.checked_at) > STALE_MS
                || feed.source_expires_at > 0 && feed.source_expires_at < now;
        }
        snapshot
    }

    fn observe_banked(&mut self, events: &[PublicEvent], now: u64) -> Option<PublicEvent> {
        let banked: Vec<&PublicEvent> = events
            .iter()
            .filter(|e| e.kind == "banked" && !e.preview && e.announced_at <= now)
            .collect();
        let latest = banked.first().copied();
        let key = |event: &PublicEvent| {
            format!(
                "{}:{}",
                event.id,
                event.banked_state.as_deref().unwrap_or("unknown")
            )
        };
        let alert = if self.banked_initialized {
            banked
                .iter()
                .find(|event| {
                    let prefix = format!("{}:", event.id);
                    let previous_stage = self
                        .banked_seen
                        .iter()
                        .filter_map(|seen| seen.strip_prefix(&prefix))
                        .map(|state| banked_stage(Some(state)))
                        .max();
                    !self.banked_seen.contains(&key(event))
                        && previous_stage
                            .is_none_or(|stage| banked_stage(event.banked_state.as_deref()) > stage)
                        && (event.announced_at > self.banked_high_watermark_at
                            || event.announced_at == self.banked_high_watermark_at
                                && event.id == self.banked_latest_id
                                && event
                                    .banked_state
                                    .as_deref()
                                    .is_some_and(|state| state != "unknown"))
                })
                .map(|event| (*event).clone())
        } else {
            self.banked_initialized = true;
            self.banked_high_watermark_at = latest.map(|e| e.announced_at).unwrap_or(now);
            None
        };
        if let Some(latest) = latest {
            if latest.announced_at >= self.banked_high_watermark_at {
                self.banked_high_watermark_at = latest.announced_at;
                self.banked_latest_id = latest.id.clone();
            }
        }
        for event in banked.iter().rev() {
            let key = key(event);
            if !self.banked_seen.contains(&key) {
                self.banked_seen.push(key);
            }
        }
        if self.banked_seen.len() > 256 {
            self.banked_seen.drain(..self.banked_seen.len() - 256);
        }
        alert
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
    store.snapshot.poll_interval_ms = POLL_MS;
    if store.snapshot.checked_at == 0 {
        store.snapshot.checked_at = store.snapshot.fetched_at;
    }
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

fn banked_state(value: Option<&Value>) -> Option<String> {
    value.filter(|v| !v.is_null()).map(|v| match v.as_str() {
        Some("announced" | "arriving" | "available") => v.as_str().unwrap().into(),
        _ => "unknown".into(),
    })
}

fn banked_stage(state: Option<&str>) -> u8 {
    match state {
        Some("available") => 3,
        Some("arriving") => 2,
        Some("announced") => 1,
        _ => 0,
    }
}

fn parse_public_event(event: &Value, source: &str, tweet: bool, now: u64) -> Option<PublicEvent> {
    let announced_at = timestamp(event.get(if tweet { "at" } else { "announced_at" }))?;
    let preview = event
        .get("preview")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if announced_at > now && !preview {
        return None;
    }
    let mut id = text(event.get("id"), 128);
    if id.is_empty() {
        id = text(event.get("tweet_id"), 128);
    }
    if id.is_empty() {
        return None;
    }
    let raw = text(event.get(if tweet { "kind" } else { "group" }), 64);
    let state = banked_state(event.get("banked_state"));
    let is_banked = state.is_some()
        || raw == "banked"
        || event.get("reset_kind").and_then(Value::as_str) == Some("banked");
    let kind = if is_banked {
        "banked"
    } else {
        match raw.as_str() {
            "reset" | "boost" | "unlock" | "credits" | "signal" | "limits" => raw.as_str(),
            "candidate" => "signal",
            _ => "other",
        }
    }
    .to_string();
    let group = if tweet {
        if is_banked {
            "credits".into()
        } else {
            raw
        }
    } else {
        raw
    };
    let announcement_state = text(event.get("announcement_state"), 64);
    let confirmed = source == "timeline"
        && group == "reset"
        && kind == "reset"
        && announcement_state == "announced"
        && !preview;
    let mut summary = text(event.get("summary"), 1000);
    if summary.is_empty() {
        summary = text(event.get("text"), 1000);
    }
    Some(PublicEvent {
        id,
        announced_at,
        kind,
        group,
        announcement_state,
        banked_state: if is_banked {
            Some(state.unwrap_or_else(|| "unknown".into()))
        } else {
            None
        },
        confirmed,
        preview,
        summary,
        url: source_link(event.get("url")),
        audience: event
            .get("audience")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .take(20)
                    .map(|s| s.chars().take(64).collect())
                    .collect()
            })
            .unwrap_or_default(),
        source: source.into(),
    })
}

fn sort_events(events: &mut [PublicEvent]) {
    events.sort_by(|a, b| {
        b.announced_at
            .cmp(&a.announced_at)
            .then_with(|| b.id.cmp(&a.id))
    });
}

fn parse_public_timeline(value: &Value, now: u64) -> Result<(u64, Vec<PublicEvent>), String> {
    // Timeline update metadata is useful but not part of the documented stable event contract.
    // Publication freshness comes from the response headers; an omitted date stays unknown.
    let updated_at = timestamp(value.get("updated_at")).unwrap_or(0);
    let events = value
        .get("events")
        .and_then(Value::as_array)
        .ok_or("Timeline has no events array")?;
    let mut records = Vec::new();
    for event in events.iter().take(2000) {
        if let Some(record) = parse_public_event(event, "timeline", false, now) {
            if !records
                .iter()
                .any(|existing: &PublicEvent| existing.id == record.id)
            {
                records.push(record);
            }
        }
    }
    sort_events(&mut records);
    Ok((updated_at, records))
}

fn confirmed_resets(events: &[PublicEvent]) -> Vec<ConfirmedReset> {
    events
        .iter()
        .filter(|event| event.confirmed)
        .map(|event| ConfirmedReset {
            id: event.id.clone(),
            announced_at: event.announced_at,
            summary: event.summary.clone(),
            url: event.url.clone(),
            audience: event.audience.clone(),
        })
        .collect()
}

#[cfg(test)]
fn parse_timeline(value: &Value, now: u64) -> Result<(u64, Vec<ConfirmedReset>), String> {
    let (updated_at, events) = parse_public_timeline(value, now)?;
    Ok((updated_at, confirmed_resets(&events)))
}

fn parse_feed(
    value: &Value,
    now: u64,
    published_at: u64,
    expires_at: u64,
) -> Result<FeedSnapshot, String> {
    let fetched_at =
        timestamp(value.get("fetched_at")).ok_or("Live feed has no valid fetch time")?;
    let tweets = value
        .get("tweets")
        .and_then(Value::as_array)
        .ok_or("Live feed has no tweets array")?;
    let mut posts = Vec::new();
    for tweet in tweets.iter().take(200) {
        if let Some(record) = parse_public_event(tweet, "feed", true, now) {
            if !posts
                .iter()
                .any(|event: &PublicEvent| event.id == record.id)
            {
                posts.push(record);
            }
        }
    }
    sort_events(&mut posts);
    posts.truncate(32);
    let stale = value.get("stale").and_then(Value::as_bool).unwrap_or(true)
        || now.saturating_sub(fetched_at) > STALE_MS
        || fetched_at > now.saturating_add(POLL_MS);
    Ok(FeedSnapshot {
        checked_at: now,
        fetched_at,
        source_checked_at: published_at,
        source_expires_at: expires_at,
        newest_post_at: timestamp(value.get("newest_post_at")),
        stale,
        signal: value
            .get("signal")
            .and_then(|signal| parse_public_event(signal, "feed", true, now)),
        posts,
    })
}

fn parse_service(value: &Value, now: u64) -> Result<ServiceStatus, String> {
    let checked_at =
        timestamp(value.get("checked_at")).ok_or("Service status has no valid check time")?;
    let current = value
        .get("current")
        .and_then(Value::as_object)
        .ok_or("Service status has no current state")?;
    Ok(ServiceStatus {
        checked_at,
        status: text(current.get("codex"), 80),
        description: text(current.get("description"), 300),
        stale: value.get("stale").and_then(Value::as_bool).unwrap_or(true)
            || now.saturating_sub(checked_at) > STALE_MS
            || checked_at > now.saturating_add(POLL_MS),
        surfaces: value
            .get("surfaces")
            .and_then(Value::as_array)
            .map(|surfaces| {
                surfaces
                    .iter()
                    .take(20)
                    .map(|surface| ServiceSurface {
                        id: text(surface.get("id"), 80),
                        label: text(surface.get("label"), 120),
                        status: text(surface.get("status"), 80),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    })
}

fn merge_events(timeline: &[PublicEvent], feed: Option<&FeedSnapshot>) -> Vec<PublicEvent> {
    let mut events = timeline.to_vec();
    if let Some(feed) = feed.filter(|feed| !feed.stale) {
        for post in &feed.posts {
            if let Some(event) = events.iter_mut().find(|event| event.id == post.id) {
                // A feed reply can advance the banked lifecycle before the next timeline build.
                // It can never promote a live classification into a confirmed automatic reset.
                if post.kind == "banked"
                    && !post.preview
                    && post.announced_at >= event.announced_at
                    && banked_stage(post.banked_state.as_deref())
                        > banked_stage(event.banked_state.as_deref())
                {
                    event.kind = "banked".into();
                    event.banked_state = post.banked_state.clone();
                    event.confirmed = false;
                    event.source = post.source.clone();
                    if !post.summary.is_empty() {
                        event.summary = post.summary.clone();
                    }
                }
            } else {
                events.push(post.clone());
            }
        }
    }
    sort_events(&mut events);
    events
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

struct PublishedResponse {
    value: Value,
    received_at: u64,
    checked_at: u64,
    expires_at: u64,
}

fn fetch(url: &str, now: u64) -> Result<PublishedResponse, FetchError> {
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
        || published_at.is_some_and(|at| {
            response_at.saturating_sub(at) > STALE_MS || at > response_at.saturating_add(POLL_MS)
        })
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
    let value = serde_json::from_slice(&bytes).map_err(|_| FetchError {
        message: "Reset source returned invalid JSON".into(),
        backoff_until: 0,
    })?;
    Ok(PublishedResponse {
        value,
        received_at: crate::now_ms(),
        checked_at: published_at.unwrap_or(0),
        expires_at: expires_at.unwrap_or(0),
    })
}

fn next_poll_at(started_at: u64, backoff_until: u64) -> u64 {
    started_at.saturating_add(POLL_MS).max(backoff_until)
}

fn fetch_batch(now: u64) -> [Result<PublishedResponse, FetchError>; 4] {
    // The documented service exposes published GET copies rather than a subscription stream.
    // Independent endpoints run together so one slow forecast cannot hold up the reset feed.
    std::thread::scope(|scope| {
        let tasks = [TIMELINE_URL, FORECAST_URL, FEED_URL, STATUS_URL]
            .map(|url| scope.spawn(move || fetch(url, now)));
        tasks.map(|task| {
            task.join().unwrap_or_else(|_| {
                Err(FetchError {
                    message: "Reset source worker failed".into(),
                    backoff_until: 0,
                })
            })
        })
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
        store.snapshot.last_attempt_at = now;
        store.snapshot.next_check_at = next_poll_at(now, store.snapshot.backoff_until);
        store.snapshot.refreshing = true;
        // Record every endpoint's common attempt start before issuing any requests. Restarts and
        // manual/focus refreshes share this exact floor, including the service's Retry-After.
        if let Err(error) = persist(&store) {
            crate::applog(&format!("global reset cache: {error}"));
        }
        store.clone()
    };
    broadcast(app);
    let [timeline, forecast, feed, service] = fetch_batch(now);
    let mut errors = Vec::new();
    let mut new_reset = None;
    let mut timeline_fresh = false;
    let mut feed_fresh = false;
    match timeline {
        Ok(response) => match parse_public_timeline(&response.value, response.received_at) {
            Ok((updated_at, events)) => {
                let resets = confirmed_resets(&events);
                new_reset = store.observe(&resets, response.received_at);
                store.snapshot.updated_at = updated_at;
                store.snapshot.source_checked_at = response.checked_at;
                store.snapshot.source_expires_at = response.expires_at;
                store.snapshot.last_reset = resets.first().cloned();
                store.snapshot.history = resets.into_iter().take(6).collect();
                store.timeline_events = events.into_iter().take(256).collect();
                timeline_fresh = true;
            }
            Err(error) => errors.push(format!("Timeline: {error}")),
        },
        Err(error) => {
            store.snapshot.backoff_until = store.snapshot.backoff_until.max(error.backoff_until);
            errors.push(format!("Timeline: {}", error.message));
        }
    }
    match forecast {
        Ok(response) => match parse_forecast(&response.value) {
            Ok(forecast) => store.snapshot.forecast = Some(forecast),
            Err(error) => errors.push(format!("Forecast: {error}")),
        },
        Err(error) => {
            store.snapshot.backoff_until = store.snapshot.backoff_until.max(error.backoff_until);
            errors.push(format!("Forecast: {}", error.message));
        }
    }
    match feed {
        Ok(response) => match parse_feed(
            &response.value,
            response.received_at,
            response.checked_at,
            response.expires_at,
        ) {
            Ok(feed) => {
                feed_fresh = !feed.stale;
                if feed.stale {
                    errors.push("Live feed source is stale".into());
                }
                store.snapshot.feed = Some(feed);
            }
            Err(error) => {
                if let Some(feed) = store.snapshot.feed.as_mut() {
                    feed.stale = true;
                }
                errors.push(format!("Live feed: {error}"));
            }
        },
        Err(error) => {
            if let Some(feed) = store.snapshot.feed.as_mut() {
                feed.stale = true;
            }
            store.snapshot.backoff_until = store.snapshot.backoff_until.max(error.backoff_until);
            errors.push(format!("Live feed: {}", error.message));
        }
    }
    match service {
        Ok(response) => match parse_service(&response.value, response.received_at) {
            Ok(service) => {
                if service.stale {
                    errors.push("Service status source is stale".into());
                }
                store.snapshot.service = Some(service);
            }
            Err(error) => {
                if let Some(service) = store.snapshot.service.as_mut() {
                    service.stale = true;
                }
                errors.push(format!("Service: {error}"));
            }
        },
        Err(error) => {
            if let Some(service) = store.snapshot.service.as_mut() {
                service.stale = true;
            }
            store.snapshot.backoff_until = store.snapshot.backoff_until.max(error.backoff_until);
            errors.push(format!("Service: {}", error.message));
        }
    }
    let events = merge_events(&store.timeline_events, store.snapshot.feed.as_ref());
    let alert_events = merge_events(
        if timeline_fresh {
            &store.timeline_events
        } else {
            &[]
        },
        if feed_fresh {
            store.snapshot.feed.as_ref()
        } else {
            None
        },
    );
    let new_banked = if timeline_fresh || feed_fresh {
        store.observe_banked(&alert_events, crate::now_ms())
    } else {
        None
    };
    store.snapshot.banked = events.iter().find(|event| event.kind == "banked").cloned();
    store.snapshot.banked_history = events
        .iter()
        .filter(|event| event.kind == "banked")
        .take(8)
        .cloned()
        .collect();
    store.snapshot.events = events.into_iter().take(32).collect();
    store.snapshot.refreshing = false;
    store.snapshot.checked_at = crate::now_ms();
    // A 12-second network round trip does not add another 12 seconds to the one-minute cadence.
    // Requests started in parallel before a 429 may finish, but no future cycle bypasses its floor.
    store.snapshot.next_check_at = next_poll_at(now, store.snapshot.backoff_until);
    if errors.is_empty() {
        store.snapshot.status = "ok".into();
        store.snapshot.cached = false;
        store.snapshot.fetched_at = store.snapshot.checked_at;
        store.snapshot.backoff_until = 0;
        store.snapshot.error.clear();
    } else {
        store.snapshot.status = if store.snapshot.last_reset.is_some()
            || store.snapshot.forecast.is_some()
            || store.snapshot.feed.is_some()
            || !store.snapshot.events.is_empty()
        {
            "stale"
        } else {
            "offline"
        }
        .into();
        store.snapshot.cached = true;
        store.snapshot.error = errors.join(" · ");
    }
    // Persist before presenting either card so a crash/restart cannot replay delivered updates.
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
        if let Some(event) = new_banked {
            crate::reset_alert::enqueue_banked(app, event);
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
    let (notifications, banked_notifications) = {
        let cfg = state.cfg.lock().unwrap();
        (
            cfg.global_reset_notifications,
            cfg.banked_reset_notifications,
        )
    };
    let snapshot = state.global_resets.lock().unwrap().view(
        notifications,
        banked_notifications,
        crate::now_ms(),
    );
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
        cfg.public_reset_notifications_v125 = true;
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
pub fn set_banked_reset_notifications(app: AppHandle, on: bool) -> bool {
    {
        let state = app.state::<AppState>();
        let mut cfg = state.cfg.lock().unwrap();
        cfg.banked_reset_notifications = on;
        cfg.public_reset_notifications_v125 = true;
        crate::config::save(&cfg);
    }
    if !on {
        crate::reset_alert::disable_banked(&app);
    }
    broadcast(&app);
    on
}

#[tauri::command]
pub fn preview_banked_reset_alert(app: AppHandle) -> Result<(), String> {
    crate::reset_alert::preview_banked(app)
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
    fn corrected_known_ids_advance_cursor_without_replaying_notifications() {
        let mut store = Store::default();
        assert!(store.observe(&[reset("known", 100)], NOW).is_none());
        assert!(store.observe(&[reset("known", 200)], NOW).is_none());
        let saved = serde_json::to_string(&store).unwrap();
        let mut restarted: Store = serde_json::from_str(&saved).unwrap();
        assert!(restarted.observe(&[reset("known", 300)], NOW).is_none());
        assert_eq!(
            restarted.observe(&[reset("new", 400)], NOW).unwrap().id,
            "new"
        );
        let mut upgraded = Store {
            initialized: true,
            high_watermark_at: 100,
            high_watermark_ids: vec!["existing".into()],
            ..Store::default()
        };
        assert!(upgraded.observe(&[reset("existing", 200)], NOW).is_none());
    }

    #[test]
    fn bounded_global_memory_retains_the_newest_known_ids() {
        let mut store = Store::default();
        let mut initial: Vec<ConfirmedReset> = (0..256)
            .rev()
            .map(|n| reset(&format!("old-{n}"), 1000 + n))
            .collect();
        assert!(store.observe(&initial, NOW).is_none());
        initial.insert(0, reset("new", 1300));
        assert!(store.observe(&initial, NOW).is_some());
        assert!(store.observe(&[reset("old-255", 2000)], NOW).is_none());
        assert!(store.global_seen_ids.len() <= 256);
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
        let snapshot = store.view(false, false, 100 + STALE_MS + 1);
        assert_eq!(snapshot.status, "stale");
        assert!(snapshot.cached);
        assert!(!snapshot.notifications);
        assert_eq!(snapshot.last_reset.unwrap().id, "old");
    }

    fn banked(id: &str, at: u64, state: &str) -> PublicEvent {
        PublicEvent {
            id: id.into(),
            announced_at: at,
            kind: "banked".into(),
            group: "credits".into(),
            announcement_state: "none".into(),
            banked_state: Some(state.into()),
            confirmed: false,
            preview: false,
            summary: "Public banked update".into(),
            url: None,
            audience: Vec::new(),
            source: "timeline".into(),
        }
    }

    #[test]
    fn public_timeline_retains_banked_pending_boost_unlock_and_planned_without_resetting_clock() {
        let value = json!({"events":[
            {"id":"reset","group":"reset","announced_at":"2026-10-07T03:35:09Z","announcement_state":"announced"},
            {"id":"banked","group":"credits","announced_at":"2026-10-07T03:36:09Z","banked_state":"available"},
            {"id":"pending","group":"reset","announced_at":"2026-10-07T03:37:09Z","announcement_state":"none"},
            {"id":"boost","group":"boost","announced_at":"2026-10-07T03:38:09Z"},
            {"id":"unlock","group":"unlock","announced_at":"2026-10-07T03:39:09Z"},
            {"id":"planned","group":"reset","announced_at":"2099-01-01T00:00:00Z","announcement_state":"announced","preview":true}
        ]});
        let (_, events) = parse_public_timeline(&value, NOW).unwrap();
        assert_eq!(events.len(), 6);
        assert_eq!(
            events.iter().find(|e| e.id == "banked").unwrap().kind,
            "banked"
        );
        assert!(events.iter().find(|e| e.id == "planned").unwrap().preview);
        let resets = confirmed_resets(&events);
        assert_eq!(resets.len(), 1);
        assert_eq!(resets[0].id, "reset");
    }

    fn live_feed(stale: bool) -> FeedSnapshot {
        parse_feed(&json!({"fetched_at":"2026-10-07T19:06:40Z","stale":stale,
            "newest_post_at":"2026-10-07T19:05:00Z",
            "signal":{"tweet_id":"candidate","kind":"candidate","at":"2026-10-07T19:03:00Z","summary":"Related reset signal"},
            "tweets":[
                {"id":"banked-reply","kind":"other","banked_state":"arriving","at":"2026-10-07T19:05:00Z","text":"Will be there later"},
                {"id":"candidate","kind":"candidate","at":"2026-10-07T19:03:00Z","text":"Full reset coming"}
            ]
        }), NOW, NOW - 1000, NOW + POLL_MS).unwrap()
    }

    #[test]
    fn feed_reply_advances_public_banked_lifecycle_but_candidate_never_confirms_a_reset() {
        let feed = live_feed(false);
        assert!(!feed.stale);
        assert_eq!(feed.posts[0].kind, "banked");
        assert_eq!(feed.posts[0].banked_state.as_deref(), Some("arriving"));
        assert!(!feed.posts[1].confirmed);
        assert_eq!(feed.signal.unwrap().kind, "signal");
        assert!(parse_feed(&json!({"fetched_at":"2026-10-07T19:06:40Z"}), NOW, 0, 0).is_err());
    }

    #[test]
    fn banked_cursor_baselines_lifecycle_transitions_and_restart_without_touching_reset_cursor() {
        let mut store = Store::default();
        let announced = banked("grant", 100, "announced");
        let arriving = banked("grant", 100, "arriving");
        let available = banked("grant", 100, "available");
        assert!(store.observe_banked(&[announced], NOW).is_none());
        assert_eq!(
            store.observe_banked(&[arriving.clone()], NOW),
            Some(arriving)
        );
        assert_eq!(
            store.observe_banked(&[available.clone()], NOW),
            Some(available.clone())
        );
        let saved = serde_json::to_string(&store).unwrap();
        let mut restarted: Store = serde_json::from_str(&saved).unwrap();
        assert!(restarted.observe_banked(&[available], NOW).is_none());
        assert!(restarted
            .observe_banked(&[banked("grant", 100, "unknown")], NOW)
            .is_none());
        assert!(restarted
            .observe_banked(&[banked("grant", 100, "available")], NOW)
            .is_none());
        assert!(restarted
            .observe_banked(&[banked("new-grant", 200, "unknown")], NOW)
            .is_some());
        assert!(!store.initialized);
        assert_eq!(store.high_watermark_at, 0);
    }

    #[test]
    fn stale_live_posts_cannot_promote_banked_status_or_trigger_cards() {
        let timeline = vec![banked("old", 100, "unknown")];
        let stale = live_feed(true);
        let events = merge_events(&timeline, Some(&stale));
        assert_eq!(events, timeline);
        let mut store = Store::default();
        assert!(store.observe_banked(&events, NOW).is_none());
        assert!(store.observe_banked(&events, NOW).is_none());
        let fresh = live_feed(false);
        let events = merge_events(&timeline, Some(&fresh));
        assert_eq!(
            store
                .observe_banked(&events, NOW)
                .unwrap()
                .banked_state
                .as_deref(),
            Some("arriving")
        );
    }

    #[test]
    fn banked_feed_previews_or_older_states_cannot_promote_real_cards() {
        let timeline = vec![banked("grant", 100, "announced")];
        let mut store = Store::default();
        assert!(store.observe_banked(&timeline, NOW).is_none());
        let mut feed = live_feed(false);
        let mut planned = banked("grant", NOW + 1, "available");
        planned.preview = true;
        planned.source = "feed".into();
        feed.posts = vec![planned];
        let events = merge_events(&timeline, Some(&feed));
        assert_eq!(events, timeline);
        assert!(store.observe_banked(&events, NOW).is_none());
        let available = vec![banked("grant", 100, "available")];
        assert!(store.observe_banked(&available, NOW).is_some());
        feed.posts = vec![banked("grant", 100, "arriving")];
        assert_eq!(merge_events(&available, Some(&feed)), available);
        assert!(store.observe_banked(&feed.posts, NOW).is_none());
        assert!(store
            .observe_banked(&[banked("grant", 200, "unknown")], NOW)
            .is_none());
    }

    #[test]
    fn banked_corrections_planned_future_and_old_history_stay_quiet() {
        let mut store = Store::default();
        assert!(store.observe_banked(&[], NOW).is_none());
        assert!(store
            .observe_banked(&[banked("old", 100, "available")], NOW)
            .is_none());
        let mut planned = banked("future", NOW + POLL_MS, "available");
        planned.preview = true;
        assert!(store.observe_banked(&[planned], NOW).is_none());
        assert!(store
            .observe_banked(&[banked("new", NOW + 1, "unknown")], NOW + 1)
            .is_some());
        assert!(store
            .observe_banked(&[banked("re-id", NOW + 1, "available")], NOW + 2)
            .is_none());
    }

    #[test]
    fn service_ingestion_and_publication_expiry_remain_honest() {
        let mut feed = live_feed(false);
        feed.fetched_at = NOW - STALE_MS - 1;
        let stale = parse_feed(
            &json!({"fetched_at":"2026-10-07T18:00:00Z","stale":false,"tweets":[]}),
            NOW,
            0,
            0,
        )
        .unwrap();
        assert!(stale.stale);
        let future = parse_feed(
            &json!({"fetched_at":"2099-01-01T00:00:00Z","stale":false,"tweets":[]}),
            NOW,
            0,
            0,
        )
        .unwrap();
        assert!(future.stale);
        let service = parse_service(&json!({"checked_at":"2026-10-07T19:06:40Z","stale":false,"current":{"codex":"future_status","description":"Source description"},"surfaces":[{"id":"cli","label":"Codex CLI","status":"maintenance"}]}), NOW).unwrap();
        assert_eq!(service.status, "future_status");
        assert_eq!(service.surfaces[0].status, "maintenance");
        assert!(!service.stale);
        let mut store = Store::default();
        store.snapshot.status = "ok".into();
        store.snapshot.fetched_at = NOW;
        store.snapshot.source_expires_at = NOW - 1;
        assert_eq!(store.view(true, true, NOW).status, "stale");
    }

    #[test]
    fn polling_floor_is_start_to_start_and_retry_after_can_raise_it() {
        assert_eq!(next_poll_at(NOW, 0), NOW + POLL_MS);
        assert_eq!(next_poll_at(NOW, NOW + 3600_000), NOW + 3600_000);
        assert_eq!(next_poll_at(NOW, NOW - 1), NOW + POLL_MS);
    }
}
