//! Codex usage adapter, implemented from the upstream Codenotch's documented behaviour.
//!
//! Live endpoint, native-client recovery, and a local rollout fallback:
//!   1. Live: borrow the session Codex keeps in `~/.codex/auth.json` (`tokens.access_token` +
//!      `tokens.account_id`) and GET `https://chatgpt.com/backend-api/wham/usage`. The reply carries
//!      `rate_limit.{primary_window,secondary_window}` with `used_percent / limit_window_seconds /
//!      reset_at (seconds) | reset_after_seconds`, plus a top-level `plan_type`. That is the number
//!      for *now*, and it starts no process. The token is read only — never refreshed, never written
//!      back; 401/403 becomes needsAuth and Codex renews it on its own.
//!   2. If the direct read fails (but not during a 429 backoff), a native codex.exe can read
//!      account/rateLimits/read using Codex's own authentication. No cmd/node wrapper is spawned;
//!      the owned process is hidden, bounded to 20 seconds, killed and reaped. The explicit
//!      `codex` bucket wins over the legacy single-bucket view, which can refer to Spark.
//!      This is recovery for a stored-token HTTP failure while the installed client can still
//!      authenticate, not the old unconditional cmd/node process tree removed in 1.5.0.
//!      Codex owns any managed OAuth refresh; Codenotch sends no login/refresh request itself.
//!   3. Fallback: Codex writes the limits it saw on each turn into the thread's rollout log
//!      `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, as lines like
//!      `{"timestamp":"…","type":"event_msg","payload":{"type":"token_count","rate_limits":{
//!         "primary":{"used_percent":0.0,"window_minutes":300,"resets_at":1790585719},
//!         "secondary":{…}|null,"plan_type":"free"}}}`
//!      The reset is **resets_at, absolute seconds** (the documented resets_in_seconds is accepted
//!      too). This is the number from the *last run* — reading a file always succeeds instantly, so
//!      the reading is marked stale by the line's own timestamp (> 5 min).
//!      Like macOS, prefer the thread index in state_5.sqlite (read-only, WAL-aware, at most
//!      eight paths). If unavailable, retain the bounded three-date-directory scan. A resumed
//!      old thread keeps its creation directory, but the index records its latest activity.
//!
//! Numbers come from Codex's own sign-in and endpoint. The selected CODEX_HOME also owns
//! native-client recovery, rollout fallback and its cache. Settings may explicitly launch the
//! installed CLI to sign in to an isolated profile; only that CLI writes its OAuth credential.
//! No sign-in and no session history at all means absent (no cell is shown).

use crate::usage::{LimitWindow, UsageSnapshot};
use crate::AppState;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const TAIL_BYTES: u64 = 256 * 1024;
const CURRENT_FOR_MS: u64 = 5 * 60 * 1000;
const ENDPOINT: &str = "https://chatgpt.com/backend-api/wham/usage";
const BACKOFF_MIN_SECS: u64 = 60; // wait at least this long after a 429; Retry-After only raises it

static REFRESH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Retry deadline given by the server (ms epoch): neither a manual refresh nor a restart may bypass it
static BACKOFF_UNTIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// When the native client may be tried again after it came back with nothing.
///
/// `read_app_server` spawns `codex app-server` and waits on it. On a machine that has Codex
/// installed but is signed out, that call fails every time, and the poll runs every five minutes —
/// a hidden process, up to twenty seconds long, for as long as the app is open. The native read is
/// a fallback for managed sign-ins `auth.json` cannot describe, not something worth paying for on
/// every poll, so a failed attempt stands the path down for half an hour.
static NATIVE_RETRY_AFTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const NATIVE_STAND_DOWN_MS: u64 = 30 * 60 * 1000;

pub fn request_refresh() {
    REFRESH.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn codex_home() -> Option<PathBuf> {
    crate::accounts::active("codex").map(|p| p.root)
        .or_else(|| std::env::var_os("CODEX_HOME").filter(|p| !p.is_empty()).map(PathBuf::from))
        .or_else(|| dirs::home_dir().map(|h| h.join(".codex")))
}

fn store_path() -> PathBuf {
    crate::accounts::cache_path("codex", "codex.json")
}

pub fn load_persisted() -> UsageSnapshot {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|t| serde_json::from_str::<UsageSnapshot>(&t).ok())
        .map(|mut s| {
            if !s.windows.is_empty() {
                s.status = "stale".into();
            }
            BACKOFF_UNTIL.store(s.backoff_until, std::sync::atomic::Ordering::Relaxed);
            s
        })
        .unwrap_or_default()
}

fn persist(s: &UsageSnapshot) { persist_at(&store_path(), s); }

fn persist_at(path: &Path, s: &UsageSnapshot) {
    if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
    if let Ok(t) = serde_json::to_string_pretty(s) { let _ = std::fs::write(path, t); }
}

// ---------------- Locating the executable ----------------

/// Candidates in order: the native exe inside the global npm package (cleanest — no cmd/node
/// wrapper) → ~/.codex/bin → codex.exe / codex.cmd on PATH.
pub fn find_executable() -> Option<PathBuf> { executable_candidates().into_iter().find(|p| p.is_file()) }

fn executable_candidates() -> Vec<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(appdata) = dirs::config_dir() {
        let pkg = appdata.join("npm").join("node_modules").join("@openai").join("codex");
        if let Ok(rd) = std::fs::read_dir(pkg.join("bin")) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_lowercase();
                let native = if cfg!(windows) {
                    n.contains("windows") && n.ends_with(".exe")
                } else {
                    n.contains("linux") && !n.ends_with(".exe")
                };
                if n.starts_with("codex-") && native {
                    cands.push(e.path());
                }
            }
        }
        if let Ok(rd) = std::fs::read_dir(pkg.join("vendor")) {
            // Newer packages keep the native exe at vendor/<triple>/codex/codex.exe
            for e in rd.flatten() {
                for n in crate::usage::command_names("codex") {
                    let p = e.path().join("codex").join(n);
                    if p.exists() {
                        cands.push(p);
                    }
                }
            }
        }
        // Current npm releases keep the native binary in a platform-specific optional package.
        for platform in ["codex-win32-x64", "codex-win32-arm64"] {
            let vendor = appdata.join("npm/node_modules/@openai").join(platform).join("vendor");
            for triple in ["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"] {
                cands.push(vendor.join(triple).join("codex/codex.exe"));
            }
        }
        for n in crate::usage::command_names("codex") {
            cands.push(appdata.join("npm").join(n));
        }
    }
    if let Some(h) = codex_home() {
        cands.push(h.join("bin").join("codex.exe"));
        cands.push(h.join("bin").join("codex"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for n in crate::usage::command_names("codex") {
                cands.push(dir.join(n));
            }
        }
    }
    cands
}

// ---------------- Live: the usage endpoint ----------------

fn auth_path() -> Option<PathBuf> {
    codex_home().map(|h| h.join("auth.json"))
}

struct Credential {
    access_token: String,
    account_id: String,
    /// chatgpt_plan_type from the id_token (pro / plus / free…), used only as a label
    plan: Option<String>,
    /// The access_token's exp has passed: the request is still sent (the server decides); this only changes the 401 wording
    expired: bool,
}

/// Second JWT segment (base64url) → claims. Used only for labels and a local expiry hint; nothing is verified here — that is the server's job
fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let part = token.split('.').nth(1)?;
    let raw = crate::antigravity::b64_decode(part)?;
    serde_json::from_slice(&raw).ok()
}

/// Reads Codex's sign-in state; a missing file or missing field both mean "not signed in"
fn load_credential() -> Option<Credential> { load_credential_in(&codex_home()?) }

fn load_credential_in(home: &Path) -> Option<Credential> {
    let text = std::fs::read_to_string(home.join("auth.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let tokens = v.get("tokens")?;
    let access_token = tokens.get("access_token")?.as_str()?.trim().to_string();
    let claims = tokens.get("id_token").and_then(|x| x.as_str()).and_then(jwt_claims);
    let account_id = tokens.get("account_id").and_then(|x| x.as_str())
        .or_else(|| claims.as_ref()?.pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")?.as_str())?
        .trim().to_string();
    if access_token.is_empty() || account_id.is_empty() {
        return None;
    }
    let expired = jwt_claims(&access_token)
        .and_then(|c| c.get("exp").and_then(|x| x.as_f64()))
        .map(|exp| exp * 1000.0 <= now_ms() as f64)
        .unwrap_or(false);
    let plan = tokens
        .get("id_token")
        .and_then(|x| x.as_str())
        .and_then(jwt_claims)
        .and_then(|c| {
            c.get("https://api.openai.com/auth")?
                .get("chatgpt_plan_type")?
                .as_str()
                .map(String::from)
        });
    Some(Credential { access_token, account_id, plan, expired })
}

enum LiveErr {
    NeedsAuth,
    /// Suggested wait in seconds (BACKOFF_MIN_SECS already applied)
    RateLimited(u64),
    Other(String),
}

fn fetch_usage(cred: &Credential) -> Result<serde_json::Value, LiveErr> {
    let resp = ureq::get(ENDPOINT)
        .set("Authorization", &format!("Bearer {}", cred.access_token))
        .set("ChatGPT-Account-Id", &cred.account_id)
        .set("Accept", "application/json")
        .set("Cache-Control", "no-cache, no-store")
        .set("User-Agent", concat!("codenotch/", env!("CARGO_PKG_VERSION"), " (Windows)"))
        .timeout(Duration::from_secs(15))
        .call();
    match resp {
        Ok(r) => r.into_json().map_err(|e| LiveErr::Other(format!("parse: {e}"))),
        Err(ureq::Error::Status(code @ (401 | 403), r)) => {
            // 401 is about the token; 403 can also be an edge node rejecting the user agent — record the status and the start of the body rather than folding both into "please sign in"
            let head: String = r
                .into_string()
                .unwrap_or_default()
                .chars()
                .filter(|c| !c.is_control())
                .take(160)
                .collect();
            crate::applog(&format!("codex: usage endpoint HTTP {code}: {head}"));
            Err(LiveErr::NeedsAuth)
        }
        Err(ureq::Error::Status(429, r)) => {
            let ra = r.header("retry-after").and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
            Err(LiveErr::RateLimited(ra.max(BACKOFF_MIN_SECS)))
        }
        Err(ureq::Error::Status(code, _)) => Err(LiveErr::Other(format!("HTTP {code}"))),
        Err(e) => Err(LiveErr::Other(format!("{e}"))),
    }
}

/// Upstream's label rule: Codex names windows only by length, and "5h limit" says more than "primary"
fn label_for(window_minutes: Option<f64>, id: &str) -> String {
    match window_minutes {
        Some(m) if m > 0.0 => {
            if m < 60.0 {
                format!("{}m limit", m as i64)
            } else if m < 60.0 * 24.0 {
                format!("{}h limit", (m / 60.0) as i64)
            } else {
                let days = (m / (60.0 * 24.0)).round() as i64;
                match days {
                    7 => "Weekly limit".into(),
                    30 => "Monthly limit".into(),
                    d => format!("{d}d limit"),
                }
            }
        }
        _ => {
            if id == "primary" {
                "Current session".into()
            } else {
                "Longer window".into()
            }
        }
    }
}

fn num(v: Option<&serde_json::Value>) -> Option<f64> {
    v.and_then(|x| x.as_f64())
}

/// Seconds → ms. Negative / non-finite values are treated as missing so a
/// garbage extra cannot wrap `now + ms` (debug overflow panics).
fn secs_to_ms(s: f64) -> Option<u64> {
    if !s.is_finite() || s < 0.0 {
        None
    } else {
        Some((s * 1000.0) as u64)
    }
}

fn reset_at_ms(w: &serde_json::Value, now: u64, epoch_key: &str, delay_key: &str) -> Option<u64> {
    num(w.get(epoch_key))
        .and_then(secs_to_ms)
        .or_else(|| num(w.get(delay_key)).and_then(secs_to_ms).map(|ms| now.saturating_add(ms)))
}

/// Skip a window with no `used_percent`. Extra ids still pass primary/secondary to `label_for`.
fn window_from(
    w: &serde_json::Value,
    id: &str,
    fallback: &str,
    now: u64,
    group: Option<&str>,
) -> Option<LimitWindow> {
    if !w.is_object() {
        return None;
    }
    let pct = num(w.get("used_percent"))?;
    Some(LimitWindow {
        id: id.into(),
        label: label_for(num(w.get("limit_window_seconds")).map(|s| s / 60.0), fallback),
        used: (pct / 100.0).clamp(0.0, 1.0),
        resets_at: reset_at_ms(w, now, "reset_at", "reset_after_seconds"),
        group: group.map(str::to_string),
        ..Default::default()
    })
}

fn names_spark(extra: &serde_json::Value) -> bool {
    if !extra.is_object() {
        return false;
    }
    ["limit_name", "metered_feature"].iter().any(|key| {
        extra
            .get(*key)
            .and_then(|x| x.as_str())
            .is_some_and(|s| s.to_lowercase().contains("spark"))
    })
}

/// Spark / code review sit after the main pair and remain separate detail rows.
/// The group is what the hover card uses to box them; omitting it leaves them
/// as extra ungrouped bars under the main windows.
fn append_extra(
    rl: Option<&serde_json::Value>,
    primary_id: &str,
    secondary_id: &str,
    group: &str,
    now: u64,
    out: &mut Vec<LimitWindow>,
) {
    let Some(rl) = rl.filter(|x| x.is_object()) else {
        return;
    };
    if let Some(w) = rl
        .get("primary_window")
        .and_then(|x| window_from(x, primary_id, "primary", now, Some(group)))
    {
        push_unique(out, w);
    }
    if let Some(w) = rl
        .get("secondary_window")
        .and_then(|x| window_from(x, secondary_id, "secondary", now, Some(group)))
    {
        push_unique(out, w);
    }
}

fn push_unique(out: &mut Vec<LimitWindow>, window: LimitWindow) {
    if out.iter().any(|w| w.id == window.id) {
        return;
    }
    out.push(window);
}

/// Usage reply → windows. Primary and secondary feed the ring; Spark
/// (`additional_rate_limits`) and Code review (`code_review_rate_limit`) belong
/// on the hover card, not as extra rings. The window id records which field it
/// came from and the label is derived from the length — the primary window is
/// not always five hours (a free plan has shown 30 days), and recognising only
/// fixed lengths would drop a window that is genuinely in use.
fn windows_from_usage(v: &serde_json::Value) -> Vec<LimitWindow> {
    let now = now_ms();
    let mut out = Vec::new();
    for (id, key) in [("primary", "primary_window"), ("secondary", "secondary_window")] {
        if let Some(w) = v
            .pointer(&format!("/rate_limit/{key}"))
            .and_then(|x| window_from(x, id, id, now, None))
        {
            out.push(w);
        }
    }
    // A non-array (null, object, string) is the same as omitting the field —
    // one junk extra must not discard the main pair or a later Spark row.
    if let Some(extras) = v.get("additional_rate_limits").and_then(|x| x.as_array()) {
        for extra in extras {
            if !names_spark(extra) {
                continue;
            }
            append_extra(extra.get("rate_limit"), "spark", "spark-secondary", "Spark", now, &mut out);
        }
    }
    append_extra(
        v.get("code_review_rate_limit"),
        "code-review",
        "code-review-secondary",
        "Code review",
        now,
        &mut out,
    );
    out
}

// ---------------- Fallback: the rollout snapshot ----------------

/// Creation dates do not indicate activity: resumed threads keep their original directory.
pub fn newest_rollout() -> Option<PathBuf> {
    newest_rollout_in(&codex_home()?)
}

fn newest_rollout_in(home: &Path) -> Option<PathBuf> {
    indexed_rollout(&home.join("state_5.sqlite"))
        .or_else(|| newest_recent_rollout(&home.join("sessions")))
}

fn indexed_rollout(database: &Path) -> Option<PathBuf> {
    use rusqlite::{Connection, OpenFlags};
    // No immutable=1: resumed-thread updates may still be in the writer's WAL.
    // Never create/migrate the database; schema changes or contention use the bounded fallback.
    let db = Connection::open_with_flags(
        database, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ).ok()?;
    db.busy_timeout(Duration::from_millis(50)).ok()?;
    let mut query = db.prepare(
        "SELECT rollout_path FROM threads WHERE archived = 0 ORDER BY updated_at_ms DESC LIMIT 8",
    ).or_else(|_| db.prepare(
        "SELECT rollout_path FROM threads WHERE archived = 0 ORDER BY updated_at DESC LIMIT 8",
    )).ok()?;
    let paths = query.query_map([], |row| row.get::<_, String>(0)).ok()?;
    let found = paths.filter_map(Result::ok).map(PathBuf::from).find(|path| {
        path.file_name().and_then(|name| name.to_str())
            .map(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
            .unwrap_or(false) && path.is_file()
    });
    found
}

fn newest_recent_rollout(root: &Path) -> Option<PathBuf> {
    let mut days: Vec<PathBuf> = Vec::new();
    let mut years = list_dirs(root);
    years.sort_by(|a, b| b.cmp(a));
    'outer: for y in years {
        let mut months = list_dirs(&y);
        months.sort_by(|a, b| b.cmp(a));
        for m in months {
            let mut ds = list_dirs(&m);
            ds.sort_by(|a, b| b.cmp(a));
            for d in ds {
                days.push(d);
                if days.len() >= 3 {
                    break 'outer;
                }
            }
        }
    }
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for d in days {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                if !(name.starts_with("rollout-") && name.ends_with(".jsonl")) {
                    continue;
                }
                let Ok(md) = e.metadata() else { continue };
                let Ok(mt) = md.modified() else { continue };
                if best.as_ref().map(|(t, _)| mt > *t).unwrap_or(true) {
                    best = Some((mt, p));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

fn list_dirs(p: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(p)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default()
}

pub fn tail_text(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)));
    let mut raw = Vec::new();
    f.read_to_end(&mut raw).ok()?;
    Some(String::from_utf8_lossy(&raw).into_owned())
}

/// The last rate_limits snapshot at the tail of a rollout → (windows, recorded-at ms, plan)
pub fn snapshot_from_rollout(text: &str) -> Option<(Vec<LimitWindow>, Option<u64>, Option<String>)> {
    for line in text.lines().rev().filter(|l| l.contains("rate_limits")) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        // rate_limits may sit at the top level or under payload
        let rl = v
            .get("rate_limits")
            .or_else(|| v.pointer("/payload/rate_limits"))
            .filter(|x| x.is_object());
        let Some(rl) = rl else { continue };
        // Multiple buckets are emitted separately. Spark must never stand in for core Codex.
        // Legacy snapshots without an id are still accepted.
        if rl.get("limit_id").or_else(|| rl.get("limitId"))
            .and_then(|v| v.as_str()).map(|id| id != "codex").unwrap_or(false) {
            continue;
        }
        let recorded = v
            .get("timestamp")
            .and_then(|x| x.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp_millis().max(0) as u64);
        let now = now_ms();
        let mut out = Vec::new();
        for id in ["primary", "secondary"] {
            let Some(w) = rl.get(id).filter(|x| x.is_object()) else { continue };
            let Some(pct) = num(w.get("used_percent")) else { continue };
            out.push(LimitWindow {
                id: id.into(),
                label: label_for(num(w.get("window_minutes")), id),
                used: (pct / 100.0).clamp(0.0, 1.0),
                resets_at: reset_at_ms(w, now, "resets_at", "resets_in_seconds"),
                ..Default::default()
            });
        }
        if out.is_empty() {
            continue;
        }
        let plan = rl.get("plan_type").and_then(|x| x.as_str()).map(String::from);
        return Some((out, recorded, plan));
    }
    None
}

// ---------------- Putting it together ----------------

/// Prefer the installed native Codex client: it understands the desktop's managed sign-in.
/// Only initialize + account/rateLimits/read are sent; no login or inference commands.
fn native_codex() -> Option<PathBuf> {
    if let Some(local) = dirs::data_local_dir() {
        let mut bins = list_dirs(&local.join("OpenAI/Codex/bin"));
        bins.sort_by_key(|p| std::cmp::Reverse(std::fs::metadata(p).and_then(|m| m.modified()).ok()));
        if let Some(exe) = bins.into_iter().map(|p| p.join("codex.exe")).find(|p| p.is_file()) {
            return Some(exe);
        }
    }
    #[cfg(windows)]
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        // The Store build and the standalone CLI share the same account API. Read only
        // known Codex package directories; inability to enumerate is an ordinary fallback.
        if let Ok(entries) = std::fs::read_dir(PathBuf::from(program_files).join("WindowsApps")) {
            for entry in entries.flatten() {
                if !entry.file_name().to_string_lossy().to_ascii_lowercase().starts_with("openai.codex_") { continue; }
                for relative in ["app/resources/codex.exe", "resources/codex.exe", "codex.exe"] {
                    let path = entry.path().join(relative);
                    if path.is_file() { return Some(path); }
                }
            }
        }
    }
    executable_candidates().into_iter().find(|p| p.is_file() && (!cfg!(windows)
        || p.extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case("exe"))))
}

pub fn browser_login_available() -> bool { native_codex().is_some() }

fn app_server_snapshot(result: &serde_json::Value) -> Option<UsageSnapshot> {
    // A present multi-bucket map is authoritative: never substitute a legacy Spark bucket
    // (or a legacy bucket with no id) when the map does not contain core Codex.
    let core = match result.get("rateLimitsByLimitId").filter(|v| !v.is_null()) {
        Some(buckets) => buckets.get("codex")?,
        None => result.get("rateLimits")?,
    };
    if core.get("limitId").and_then(|x| x.as_str()).map(|id| id != "codex").unwrap_or(false) {
        return None;
    }
    let mut windows = Vec::new();
    for id in ["primary", "secondary"] {
        let Some(w) = core.get(id).filter(|v| v.is_object()) else { continue };
        let Some(used) = w.get("usedPercent").and_then(|x| x.as_f64()) else { continue };
        windows.push(LimitWindow {
            id: id.into(),
            label: label_for(w.get("windowDurationMins").and_then(|x| x.as_f64()), id),
            used: (used / 100.0).clamp(0.0, 1.0),
            resets_at: w.get("resetsAt").and_then(|x| x.as_u64()).map(|s| s.saturating_mul(1000)),
            ..Default::default()
        });
    }
    if windows.is_empty() { return None; }
    Some(UsageSnapshot { status: "ok".into(), windows, fetched_at: now_ms(),
        note: "via Codex app-server".into(), ..Default::default() })
}

fn read_app_server(home: &Path) -> Option<UsageSnapshot> {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    let executable = native_codex()?;
    // The official app-server can renew credentials. Serialize it with browser
    // sign-in so the old and new OAuth sessions cannot rotate the same file.
    let _auth = try_acquire_login()?;
    let mut command = Command::new(executable);
    command.arg("app-server").env("CODEX_HOME", home).env_remove("OPENAI_API_KEY").env_remove("CODEX_API_KEY").env_remove("CODEX_ACCESS_TOKEN")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    { use std::os::windows::process::CommandExt; command.creation_flags(0x0800_0000); }
    let mut child = command.spawn().ok()?;
    let result = (|| {
        let mut input = child.stdin.take()?;
        let output = child.stdout.take()?;
        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                if tx.send(line).is_err() { break; }
            }
        });
        writeln!(input, "{}", serde_json::json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"codenotch","version":env!("CARGO_PKG_VERSION")}}})).ok()?;
        input.flush().ok()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let line = rx.recv_timeout(deadline.checked_duration_since(std::time::Instant::now())?).ok()?;
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
            match v.get("id").and_then(|x| x.as_u64()) {
                Some(1) => {
                    v.get("result")?;
                    writeln!(input, "{}", serde_json::json!({"method":"initialized","params":{}})).ok()?;
                    writeln!(input, "{}", serde_json::json!({"id":2,"method":"account/rateLimits/read"})).ok()?;
                    input.flush().ok()?;
                }
                Some(2) => return app_server_snapshot(v.get("result")?),
                _ => {}
            }
        }
    })();
    // This is a directly launched native executable, never a cmd/node wrapper or a running app.
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// Is Codex present on this machine (CLI installed, signed in, or has had sessions)? If not, no cell is shown
pub fn present() -> bool { codex_home().map(|h| present_in(&h)).unwrap_or(false) }

fn present_in(home: &Path) -> bool {
    native_codex().is_some()
        || find_executable().is_some()
        || home.join("auth.json").is_file()
        || home.join("sessions").is_dir()
}

fn read_once(home: &Path) -> UsageSnapshot {
    let mut snap = UsageSnapshot::default();
    // Note attached to the fallback reading when the live read failed; needs_auth picks the empty state when there is no fallback either
    let mut live_note: Option<String> = None;
    let mut needs_auth = false;
    let held_until = BACKOFF_UNTIL.load(std::sync::atomic::Ordering::Relaxed);
    let now = now_ms();
    if held_until > now {
        snap.backoff_until = held_until;
        live_note = Some(format!("Rate limited — retrying in {}s", (held_until - now) / 1000));
    } else {
        match load_credential_in(home) {
            None => {
                if home.join("auth.json").is_file() {
                    crate::applog("codex: auth.json has no usable access_token/account_id, falling back to the rollout");
                }
            }
            Some(cred) => match fetch_usage(&cred) {
                Ok(v) => {
                    let windows = windows_from_usage(&v);
                    if !windows.is_empty() {
                        let plan = v.get("plan_type").and_then(|x| x.as_str()).map(String::from).or(cred.plan);
                        snap.status = "ok".into();
                        snap.windows = windows;
                        snap.fetched_at = now_ms();
                        snap.note = plan.map(|p| format!("{} · via Codex", cap(&p))).unwrap_or_default();
                        return snap;
                    }
                    let keys: Vec<String> = v.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
                    crate::applog(&format!("codex: usage reply has no windows (top-level keys {keys:?}), falling back to the rollout"));
                    live_note = Some("Codex reported no usage windows".into());
                }
                Err(LiveErr::NeedsAuth) => {
                    needs_auth = true;
                    live_note = Some(if cred.expired {
                        "Codex sign-in expired — open Codex once to refresh it".into()
                    } else {
                        "Codex rejected its sign-in — sign in to Codex again".into()
                    });
                }
                Err(LiveErr::RateLimited(secs)) => {
                    let until = now_ms() + secs * 1000;
                    BACKOFF_UNTIL.store(until, std::sync::atomic::Ordering::Relaxed);
                    snap.backoff_until = until;
                    live_note = Some(format!("Rate limited — retrying in {secs}s"));
                    crate::applog(&format!("codex: usage endpoint returned 429, retrying in {secs}s"));
                }
                Err(LiveErr::Other(e)) => {
                    crate::applog(&format!("codex: live read failed ({e}), falling back to the rollout"));
                    live_note = Some(format!("Live read failed ({e})"));
                }
            },
        }
    }
    // Keep the no-process HTTP path first, and do not use a second transport to bypass
    // its Retry-After. The native client can handle managed sign-in that auth.json cannot.
    if snap.backoff_until <= now_ms()
        && NATIVE_RETRY_AFTER.load(std::sync::atomic::Ordering::Relaxed) <= now_ms()
    {
        match read_app_server(home) {
            Some(native) => return native,
            None => NATIVE_RETRY_AFTER.store(
                now_ms() + NATIVE_STAND_DOWN_MS,
                std::sync::atomic::Ordering::Relaxed,
            ),
        }
    }
    // Fallback: rollout
    match newest_rollout_in(home).and_then(|p| tail_text(&p)).and_then(|t| snapshot_from_rollout(&t)) {
        Some((windows, recorded, plan)) => {
            let rec = recorded.unwrap_or(0);
            let fresh = rec > 0 && now_ms().saturating_sub(rec) <= CURRENT_FOR_MS;
            snap.status = if fresh { "ok" } else { "stale" }.into();
            snap.windows = windows;
            snap.fetched_at = rec; // the recorded time is what counts; the UI shows Updated N ago from it
            snap.note = match plan {
                Some(p) => format!("{} · from last Codex run", cap(&p)),
                None => "from last Codex run".into(),
            };
            if let Some(n) = live_note {
                snap.note = format!("{n} · {}", snap.note);
            }
        }
        None => {
            snap.status = if needs_auth {
                "needsAuth"
            } else if present_in(home) {
                "none"
            } else {
                "absent"
            }
            .into();
            snap.note = match live_note {
                Some(n) => n,
                None if present_in(home) => "Codex has not recorded a usage snapshot yet".into(),
                None => String::new(),
            };
        }
    }
    snap
}

/// Isolated profiles do not necessarily have rollout history. An HTTP error or
/// Retry-After must keep their last real quota, with its original timestamp.
fn retain_last_reading(mut snap: UsageSnapshot, previous: &UsageSnapshot) -> UsageSnapshot {
    if !previous.windows.is_empty() && (snap.windows.is_empty()
        || (snap.note.contains("from last Codex run") && snap.fetched_at < previous.fetched_at)) {
        snap.windows = previous.windows.clone();
        snap.fetched_at = previous.fetched_at;
        if snap.status != "needsAuth" { snap.status = "stale".into(); }
    }
    snap
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn broadcast(app: &AppHandle, selection: &str, snap: UsageSnapshot) -> bool {
    crate::accounts::with_selection("codex", selection, || {
        let st = app.state::<AppState>();
        *st.codex.lock().unwrap() = snap.clone();
        persist(&snap);
        let _ = app.emit("codex", &snap);
    }).is_some()
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        let mut selection = String::new();
        let mut previous_root: Option<PathBuf> = None;
        let mut native_deadlines = std::collections::HashMap::<PathBuf, u64>::new();
        loop {
            let current = crate::accounts::selection_key("codex");
            let Some((home, cache)) = crate::accounts::with_selection("codex", &current,
                || (codex_home(), store_path())) else { continue; };
            let Some(home) = home else { std::thread::sleep(Duration::from_secs(1)); continue; };
            if current != selection {
                if let Some(previous_root) = &previous_root {
                    native_deadlines.insert(previous_root.clone(), NATIVE_RETRY_AFTER.load(std::sync::atomic::Ordering::Relaxed));
                }
                selection = current;
                previous_root = Some(home.clone());
                NATIVE_RETRY_AFTER.store(*native_deadlines.get(&home).unwrap_or(&0), std::sync::atomic::Ordering::Relaxed);
                let mut previous = std::fs::read_to_string(&cache).ok()
                    .and_then(|t| serde_json::from_str::<UsageSnapshot>(&t).ok()).unwrap_or_default();
                if !previous.windows.is_empty() { previous.status = "stale".into(); }
                BACKOFF_UNTIL.store(previous.backoff_until, std::sync::atomic::Ordering::Relaxed);
                broadcast(&app, &selection, previous);
            }
            if login_busy() { std::thread::sleep(Duration::from_secs(1)); continue; }
            let previous = std::fs::read_to_string(&cache).ok()
                .and_then(|t| serde_json::from_str::<UsageSnapshot>(&t).ok()).unwrap_or_default();
            let snap = retain_last_reading(read_once(&home), &previous);
            let hold = snap.backoff_until.saturating_sub(now_ms()) / 1000;
            let next_reset = crate::usage::refresh_delay_secs(&snap.windows, now_ms(), 60);
            // A switch or re-login invalidates late usage. Keep only a server backoff
            // for the captured cache; never resurrect the previous credential's quota.
            let backoff = snap.backoff_until;
            if !broadcast(&app, &selection, snap) && backoff > now_ms() {
                crate::accounts::preserve_backoff(&cache, backoff);
            }
            for _ in 0..next_reset.max(hold) {
                if REFRESH.swap(false, std::sync::atomic::Ordering::Relaxed)
                    || crate::accounts::selection_key("codex") != selection { break; }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
}

static LOGIN_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn login_busy() -> bool { LOGIN_BUSY.load(std::sync::atomic::Ordering::Acquire) }

struct LoginGuard;
fn try_acquire_login() -> Option<LoginGuard> {
    LOGIN_BUSY.compare_exchange(false, true, std::sync::atomic::Ordering::AcqRel,
        std::sync::atomic::Ordering::Acquire).ok().map(|_| LoginGuard)
}
impl Drop for LoginGuard {
    fn drop(&mut self) { LOGIN_BUSY.store(false, std::sync::atomic::Ordering::Release); }
}

// The native official client opens the browser and owns its OAuth callback.
fn login_command(cli: &Path, root: &Path) -> Result<std::process::Command, String> {
    use std::process::{Command, Stdio};
    #[cfg(windows)]
    if !cli.extension().and_then(|s| s.to_str()).is_some_and(|s| s.eq_ignore_ascii_case("exe")) {
        return Err("Install the current native Codex CLI to use secure browser sign-in.".into());
    }
    let mut cmd = Command::new(cli);
    cmd.args(["-c", "cli_auth_credentials_store='file'", "login"]);
    cmd.env("CODEX_HOME", root).env_remove("OPENAI_API_KEY").env_remove("CODEX_API_KEY")
        .env_remove("CODEX_ACCESS_TOKEN").env("NO_COLOR", "1")
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.current_dir(root);
    #[cfg(windows)] { use std::os::windows::process::CommandExt; cmd.creation_flags(0x0800_0000); }
    Ok(cmd)
}

static PENDING_LOGIN: std::sync::Mutex<Option<(String, std::sync::Arc<std::sync::atomic::AtomicBool>, std::sync::Arc<std::sync::atomic::AtomicBool>)>> = std::sync::Mutex::new(None);

pub fn cancel_login(profile_id: &str) -> Result<(), String> {
    cancel_login_inner(None, profile_id)
}

pub fn cancel_profile_login(app: &AppHandle, profile_id: &str) -> Result<(), String> {
    cancel_login_inner(Some(app), profile_id)
}

fn cancel_login_inner(app: Option<&AppHandle>, profile_id: &str) -> Result<(), String> {
    let pending = PENDING_LOGIN.lock().unwrap_or_else(|e| e.into_inner());
    let (_, cancelled, active) = pending.as_ref().filter(|(id, _, _)| id == profile_id)
        .ok_or("No browser sign-in is running for this account.")?;
    cancelled.store(true, std::sync::atomic::Ordering::Release);
    active.store(false, std::sync::atomic::Ordering::Release);
    if let Some(app) = app {
        crate::profile_auth::clear_browser_challenge("codex", profile_id);
        crate::claude_auth::emit_profile_login(app, "codex", profile_id, true, "Cancelling browser sign-in…");
    }
    Ok(())
}

pub fn cancel_all_logins() {
    let running = {
        let pending = PENDING_LOGIN.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, cancelled, active)) = pending.as_ref() {
            active.store(false, std::sync::atomic::Ordering::Release);
            cancelled.store(true, std::sync::atomic::Ordering::Release);
            true
        } else { false }
    };
    if running {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while login_busy() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

pub fn login_profile(app: &AppHandle, profile_id: &str, root: PathBuf) -> Result<(), String> {
    let cli = native_codex().ok_or("Native Codex CLI not found. Install the current official CLI for browser sign-in.")?;
    std::fs::create_dir_all(&root).map_err(|_| "Unable to create the account directory.")?;
    let guard = try_acquire_login().ok_or("Codex sign-in or token renewal is already running.")?;
    let before = crate::claude_auth::credential_stamp(&root, &["auth.json"]);
    let mut child = login_command(&cli, &root)?.spawn().map_err(|_| "Unable to start secure Codex browser sign-in.")?;
    let app = app.clone();
    let profile_id = profile_id.to_owned();
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    *PENDING_LOGIN.lock().unwrap_or_else(|e| e.into_inner()) = Some((profile_id.clone(), cancelled.clone(), active.clone()));
    crate::claude_auth::emit_profile_login(&app, "codex", &profile_id, true, "Opening browser. Complete sign-in there.");
    crate::claude_auth::watch_browser_output(&app, "codex", &profile_id, &mut child, active.clone());
    std::thread::spawn(move || {
        let ok = crate::claude_auth::wait_child_cancel(&mut child, Duration::from_secs(15 * 60), &cancelled);
        active.store(false, std::sync::atomic::Ordering::Release);
        crate::profile_auth::clear_browser_challenge("codex", &profile_id);
        PENDING_LOGIN.lock().unwrap_or_else(|e| e.into_inner()).take();
        let signed_in = ok && load_credential_in(&root).is_some();
        let note = if signed_in { "Sign-in complete. Refreshing usage..." }
            else if cancelled.load(std::sync::atomic::Ordering::Acquire) { "Browser sign-in cancelled." }
            else { "Browser sign-in failed or expired. Update Codex and try again." };
        if signed_in || before != crate::claude_auth::credential_stamp(&root, &["auth.json"]) {
            crate::accounts::login_finished(&app, "codex", &profile_id);
        }
        if crate::accounts::active("codex").is_some_and(|p| p.id == profile_id) {
            NATIVE_RETRY_AFTER.store(0, std::sync::atomic::Ordering::Relaxed);
        }
        crate::claude_auth::emit_profile_login(&app, "codex", &profile_id, false, note);
        drop(guard);
        request_refresh();
    });
    Ok(())
}

/// Account identity for labels only. Claims are unverified; the usage endpoint verifies auth.
pub fn profile_has_credential(root: &Path) -> bool { load_credential_in(root).is_some() }

pub fn profile_identity(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("auth.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let claims = v.pointer("/tokens/id_token").and_then(|x| x.as_str()).and_then(jwt_claims)?;
    claims.get("email").and_then(|x| x.as_str()).filter(|s| s.len() <= 254).map(str::to_owned)
}

/// For doctor: contains no secrets
pub fn probe() -> String {
    let auth = match load_credential() {
        Some(c) => format!(
            "auth.json usable{}{}",
            if c.expired { " (access_token expired)" } else { "" },
            c.plan.map(|p| format!(", plan={p}")).unwrap_or_default()
        ),
        None if auth_path().map(|p| p.is_file()).unwrap_or(false) => "auth.json present but has no token".to_string(),
        None => "auth.json not found".to_string(),
    };
    let exe = find_executable();
    let roll = newest_rollout();
    let age = roll
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|d| format!("{} min ago", d.as_secs() / 60))
        .unwrap_or_else(|| "?".into());
    format!(
        "Codex: {auth} | executable {} | newest rollout {} (modified {})",
        exe.map(|p| p.display().to_string()).unwrap_or_else(|| "not found".into()),
        roll.map(|p| p.display().to_string()).unwrap_or_else(|| "none".into()),
        age
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_keeps_an_isolated_accounts_last_true_reading() {
        let previous = UsageSnapshot { status: "ok".into(), windows: vec![LimitWindow {
            id: "primary".into(), used: 0.42, ..Default::default()
        }], fetched_at: 12345, ..Default::default() };
        let fresh = UsageSnapshot { status: "none".into(), backoff_until: 90000,
            note: "Rate limited".into(), ..Default::default() };
        let held = retain_last_reading(fresh, &previous);
        assert_eq!(held.windows[0].used, 0.42);
        assert_eq!(held.fetched_at, 12345);
        assert_eq!(held.backoff_until, 90000);
        assert_eq!(held.status, "stale");
        let live = UsageSnapshot { status: "ok".into(), windows: vec![LimitWindow {
            id: "primary".into(), used: 0.0, ..Default::default()
        }], fetched_at: 22345, ..Default::default() };
        assert_eq!(retain_last_reading(live, &previous).windows[0].used, 0.0);
    }

    #[test]
    fn credentials_are_read_only_from_the_requested_account_root() {
        let root = std::env::temp_dir().join(format!("codenotch-codex-accounts-{}-{}", std::process::id(), now_ms()));
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        for (dir, id) in [(&a, "account-a"), (&b, "account-b")] {
            std::fs::write(dir.join("auth.json"), serde_json::json!({
                "tokens": {"access_token":"fixture-token", "account_id":id}
            }).to_string()).unwrap();
        }
        assert_eq!(load_credential_in(&a).unwrap().account_id, "account-a");
        assert_eq!(load_credential_in(&b).unwrap().account_id, "account-b");
        assert!(load_credential_in(&root.join("missing")).is_none());
        assert!(profile_identity(&a).is_none()); // no synthetic identity inferred from a token
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sign_in_paths_and_account_root_are_environment_data() {
        let cli = Path::new(r"C:\fixture with spaces\O'Brien\codex.exe");
        let root = Path::new(r"C:\fixture with spaces\profiles\work");
        let cmd = login_command(cli, root).unwrap();
        assert_eq!(cmd.get_program(), cli.as_os_str());
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["-c", "cli_auth_credentials_store='file'", "login"]);
        assert_eq!(cmd.get_current_dir(), Some(root));
        assert!(cmd.get_envs().any(|(k, v)| k == "CODEX_HOME" && v == Some(root.as_os_str())));
        assert!(cmd.get_envs().any(|(k, v)| k == "OPENAI_API_KEY" && v.is_none()));
        assert!(cmd.get_envs().any(|(k, v)| k == "CODEX_ACCESS_TOKEN" && v.is_none()));
    }

    #[test]
    fn app_server_uses_core_bucket_not_legacy_spark() {
        let value = serde_json::json!({"rateLimits":{"limitId":"codex_bengalfox","primary":{"usedPercent":0}},
            "rateLimitsByLimitId":{"codex":{"limitId":"codex","primary":{"usedPercent":32,"windowDurationMins":10080,"resetsAt":1789878630}}}});
        let snapshot = app_server_snapshot(&value).unwrap();
        assert_eq!(snapshot.windows[0].used, 0.32);
        assert_eq!(snapshot.windows[0].label, "Weekly limit");
        assert_eq!(snapshot.windows[0].resets_at, Some(1789878630000));
        assert!(app_server_snapshot(&serde_json::json!({"rateLimits":{"limitId":"codex_bengalfox","primary":{"usedPercent":0}}})).is_none());
    }

    #[test]
    fn app_server_legacy_core_and_both_windows_are_supported() {
        let value = serde_json::json!({"rateLimits": {
            "primary": {"usedPercent": 25, "windowDurationMins": 300, "resetsAt": 1800000000u64},
            "secondary": {"usedPercent": 42, "windowDurationMins": 10080}
        }, "rateLimitsByLimitId": null});
        let snap = app_server_snapshot(&value).unwrap();
        assert_eq!(ids(&snap.windows), ["primary", "secondary"]);
        assert_eq!(labels(&snap.windows), ["5h limit", "Weekly limit"]);
        assert_eq!(snap.windows[0].used, 0.25);
        assert_eq!(snap.windows[1].used, 0.42);
        assert_eq!(snap.windows[0].resets_at, Some(1800000000000));
        assert_eq!(snap.windows[1].resets_at, None);
    }

    #[test]
    fn app_server_missing_core_or_usage_is_not_zero() {
        for value in [
            serde_json::json!({}),
            serde_json::json!({"rateLimits": {"primary": {"usedPercent": null}}}),
            serde_json::json!({"rateLimitsByLimitId": {}, "rateLimits": {"primary": {"usedPercent": 7}}}),
            serde_json::json!({"rateLimitsByLimitId": {"codex": null}}),
            serde_json::json!({"rateLimitsByLimitId": {"codex": {"limitId": "other", "primary": {"usedPercent": 7}}}}),
        ] {
            assert!(app_server_snapshot(&value).is_none(), "{value}");
        }
    }

    #[test]
    fn app_server_skips_malformed_windows_and_clamps_percentages() {
        let value = serde_json::json!({"rateLimits": {"limitId": "codex",
            "primary": {"usedPercent": -5}, "secondary": {"usedPercent": 150}}});
        let snap = app_server_snapshot(&value).unwrap();
        assert_eq!(snap.windows[0].used, 0.0);
        assert_eq!(snap.windows[1].used, 1.0);
        let value = serde_json::json!({"rateLimits": {"limitId": "codex",
            "primary": "invalid", "secondary": {"usedPercent": 20}}});
        assert_eq!(ids(&app_server_snapshot(&value).unwrap().windows), ["secondary"]);
    }

    #[test]
    fn rollout_keeps_legacy_core_and_filters_camel_case_buckets() {
        let core = r#"{"rate_limits":{"secondary":{"used_percent":45,"window_minutes":10080}}}"#;
        let other = r#"{"rate_limits":{"limitId":"other","primary":{"used_percent":1}}}"#;
        let (ws, _, _) = snapshot_from_rollout(&format!("{core}\n{other}")).unwrap();
        assert_eq!(ids(&ws), ["secondary"]);
        assert_eq!(ws[0].used, 0.45);
        assert!(snapshot_from_rollout(other).is_none());
    }

    #[test]
    fn rollout_ignores_newer_spark_events() {
        let core = r#"{"timestamp":"2026-09-14T07:00:00Z","payload":{"rate_limits":{"limit_id":"codex","primary":{"used_percent":32,"window_minutes":10080}}}}"#;
        let spark = r#"{"timestamp":"2026-09-14T07:00:01Z","payload":{"rate_limits":{"limit_id":"codex_bengalfox","primary":{"used_percent":0,"window_minutes":300}}}}"#;
        let (ws, _, _) = snapshot_from_rollout(&format!("{core}\n{spark}")).unwrap();
        assert_eq!(ws[0].used, 0.32);
        assert_eq!(ws[0].label, "Weekly limit");
        assert!(snapshot_from_rollout(spark).is_none());
    }

    #[test]
    fn resumed_thread_in_old_date_directory_is_found() {
        let root = std::env::temp_dir().join(format!("codenotch-rollout-test-{}-{}", std::process::id(), now_ms()));
        std::fs::create_dir(&root).unwrap();
        for day in ["2026/07/31", "2026/09/10", "2026/09/11", "2026/09/12"] {
            std::fs::create_dir_all(root.join("sessions").join(day)).unwrap();
        }
        let earlier = UNIX_EPOCH + Duration::from_secs(1700000000);
        for day in ["2026/09/10", "2026/09/11", "2026/09/12"] {
            let file = std::fs::File::create(root.join("sessions").join(day).join("rollout-inactive.jsonl")).unwrap();
            file.set_times(std::fs::FileTimes::new().set_modified(earlier)).unwrap();
        }
        let active = root.join("sessions/2026/07/31/rollout-active.jsonl");
        std::fs::write(&active, "{}").unwrap();
        // The bounded directory fallback intentionally cannot see this old directory.
        assert_ne!(newest_rollout_in(&root), Some(active.clone()));
        assert!(!root.join("state_5.sqlite").exists(), "read-only lookup must not create a database");
        let db = rusqlite::Connection::open(root.join("state_5.sqlite")).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE threads (rollout_path TEXT, archived INTEGER, updated_at_ms INTEGER);
            CREATE INDEX recent_threads ON threads(archived, updated_at_ms DESC);").unwrap();
        db.execute("INSERT INTO threads VALUES (?1, 0, 100)", [active.to_str().unwrap()]).unwrap();
        // Keep the writer open: the read-only connection must see the committed WAL update.
        assert_eq!(newest_rollout_in(&root), Some(active.clone()));
        db.execute("UPDATE threads SET archived = 1", []).unwrap();
        assert_ne!(newest_rollout_in(&root), Some(active));
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rollout_index_skips_missing_paths_and_accepts_legacy_timestamp_column() {
        let root = std::env::temp_dir().join(format!("codenotch-index-test-{}-{}", std::process::id(), now_ms()));
        std::fs::create_dir(&root).unwrap();
        let database = root.join("state_5.sqlite");
        let active = root.join("rollout-active.jsonl");
        std::fs::write(&active, "{}").unwrap();
        let db = rusqlite::Connection::open(&database).unwrap();
        db.execute_batch("CREATE TABLE threads (rollout_path TEXT, archived INTEGER, updated_at INTEGER);").unwrap();
        db.execute("INSERT INTO threads VALUES (?1, 0, 1)", [active.to_str().unwrap()]).unwrap();
        db.execute("INSERT INTO threads VALUES (?1, 0, 2)", [root.join("rollout-missing.jsonl").to_str().unwrap()]).unwrap();
        assert_eq!(indexed_rollout(&database), Some(active));
        // Bound file metadata work even when the newest entries are unavailable.
        for stamp in 3..10 {
            db.execute("INSERT INTO threads VALUES ('rollout-missing.jsonl', 0, ?1)", [stamp]).unwrap();
        }
        assert_eq!(indexed_rollout(&database), None);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_rollout_index_uses_bounded_directory_fallback() {
        let root = std::env::temp_dir().join(format!("codenotch-index-fallback-{}-{}", std::process::id(), now_ms()));
        let day = root.join("sessions/2026/09/16");
        std::fs::create_dir_all(&day).unwrap();
        let active = day.join("rollout-active.jsonl");
        std::fs::write(&active, "{}").unwrap();
        std::fs::write(root.join("state_5.sqlite"), "not a SQLite database").unwrap();
        assert_eq!(newest_rollout_in(&root), Some(active));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "Reads quota through the installed signed-in native Codex client; opt in explicitly"]
    fn live_native_quota() {
        let snap = read_app_server(&codex_home().expect("Codex home")).expect("native quota read should succeed for this signed-in client");
        assert_eq!(snap.status, "ok");
        assert!(!snap.windows.is_empty());
        assert!(snap.windows.iter().all(|window| matches!(window.id.as_str(), "primary" | "secondary")));
    }

    fn windows(json: &str) -> Vec<LimitWindow> {
        windows_from_usage(&serde_json::from_str(json).unwrap())
    }

    fn ids(ws: &[LimitWindow]) -> Vec<&str> {
        ws.iter().map(|w| w.id.as_str()).collect()
    }

    fn labels(ws: &[LimitWindow]) -> Vec<&str> {
        ws.iter().map(|w| w.label.as_str()).collect()
    }

    fn groups(ws: &[LimitWindow]) -> Vec<Option<&str>> {
        ws.iter().map(|w| w.group.as_deref()).collect()
    }

    #[test]
    fn extra_spark_and_code_review_follow_primary_secondary() {
        let ws = windows(
            r#"{
            "rate_limit":{
              "primary_window":{"used_percent":25,"limit_window_seconds":18000,"reset_at":1800001000},
              "secondary_window":{"used_percent":10,"limit_window_seconds":604800,"reset_at":1800600000}},
            "additional_rate_limits":[{"limit_name":"Spark","rate_limit":{
              "primary_window":{"used_percent":99,"limit_window_seconds":18000}}}],
            "code_review_rate_limit":{"primary_window":{"used_percent":90,"limit_window_seconds":604800}},
            "credits":{"balance":"100"},"model_usage":{"spark":99}
        }"#,
        );
        assert_eq!(ids(&ws), ["primary", "secondary", "spark", "code-review"]);
        assert_eq!(labels(&ws), ["5h limit", "Weekly limit", "5h limit", "Weekly limit"]);
        assert_eq!(groups(&ws), [None, None, Some("Spark"), Some("Code review")]);
        assert!((ws[0].used - 0.25).abs() < 1e-9);
        assert!((ws[2].used - 0.99).abs() < 1e-9);
        assert!((ws[3].used - 0.90).abs() < 1e-9);
        assert_eq!(ws[0].resets_at, Some(1_800_001_000_000));
    }

    #[test]
    fn spark_matches_limit_name_or_metered_feature_case_insensitively() {
        // "GPT-5.3-Codex-Spark" still contains the substring "Spark", so it
        // would pass a case-sensitive contains("Spark"). SPARK / spark would not.
        for (field, name) in [
            ("limit_name", "SPARK"),
            ("limit_name", "spark"),
            ("metered_feature", "GPT-5.3-Codex-SPARK"),
            ("metered_feature", "gpt-5.3-codex-spark"),
        ] {
            let ws = windows(&format!(
                r#"{{
                "rate_limit":{{"primary_window":{{"used_percent":1,"limit_window_seconds":18000}}}},
                "additional_rate_limits":[{{"{field}":"{name}","rate_limit":{{
                  "primary_window":{{"used_percent":40,"limit_window_seconds":18000}},
                  "secondary_window":{{"used_percent":5,"limit_window_seconds":604800}}}}}}]
            }}"#
            ));
            assert_eq!(ids(&ws), ["primary", "spark", "spark-secondary"], "{field}={name}");
            assert_eq!(groups(&ws)[1..], [Some("Spark"), Some("Spark")], "{field}={name}");
            assert_eq!(labels(&ws)[1..], ["5h limit", "Weekly limit"], "{field}={name}");
        }
    }

    #[test]
    fn extras_alone_are_still_a_reading() {
        let ws = windows(
            r#"{"additional_rate_limits":[{"limit_name":"Spark","rate_limit":{
              "primary_window":{"used_percent":40,"limit_window_seconds":18000},
              "secondary_window":{"used_percent":70,"limit_window_seconds":604800}}}]}"#,
        );
        assert_eq!(ids(&ws), ["spark", "spark-secondary"]);
        assert_eq!(groups(&ws), [Some("Spark"), Some("Spark")]);
        assert_eq!(labels(&ws), ["5h limit", "Weekly limit"]);
        assert!((ws[0].used - 0.40).abs() < 1e-9);
        assert!((ws[1].used - 0.70).abs() < 1e-9);
    }

    #[test]
    fn non_spark_additional_limits_are_ignored() {
        let ws = windows(
            r#"{
            "rate_limit":{"primary_window":{"used_percent":1,"limit_window_seconds":18000}},
            "additional_rate_limits":[{"limit_name":"codex_other","metered_feature":"codex_other","rate_limit":{
              "primary_window":{"used_percent":70,"limit_window_seconds":3600}}}]
        }"#,
        );
        assert_eq!(ids(&ws), ["primary"]);
    }

    #[test]
    fn empty_additional_rate_limits_leave_the_main_windows() {
        let ws = windows(
            r#"{
            "rate_limit":{
              "primary_window":{"used_percent":25,"limit_window_seconds":18000},
              "secondary_window":{"used_percent":10,"limit_window_seconds":604800}},
            "additional_rate_limits":[]
        }"#,
        );
        assert_eq!(ids(&ws), ["primary", "secondary"]);
        assert_eq!(groups(&ws), [None, None]);
    }

    #[test]
    fn extras_without_used_percent_are_skipped() {
        let ws = windows(
            r#"{
            "rate_limit":{"primary_window":{"used_percent":1,"limit_window_seconds":18000}},
            "additional_rate_limits":[{"limit_name":"Spark","rate_limit":{
              "primary_window":{"used_percent":null,"limit_window_seconds":18000},
              "secondary_window":{"used_percent":12,"limit_window_seconds":604800}}}],
            "code_review_rate_limit":{
              "primary_window":{"limit_window_seconds":604800},
              "secondary_window":{"used_percent":8,"limit_window_seconds":18000}}
        }"#,
        );
        assert_eq!(ids(&ws), ["primary", "spark-secondary", "code-review-secondary"]);
        assert_eq!(groups(&ws)[1..], [Some("Spark"), Some("Code review")]);
        assert_eq!(ws[1].label, "Weekly limit");
        assert_eq!(ws[2].label, "5h limit");
    }

    #[test]
    fn malformed_extras_do_not_drop_the_main_windows() {
        let ws = windows(
            r#"{
            "rate_limit":{"primary_window":{"used_percent":25,"limit_window_seconds":18000}},
            "additional_rate_limits":[
              "nope",
              42,
              null,
              {"limit_name":"Spark"},
              {"limit_name":"Spark","rate_limit":"nope"},
              {"limit_name":"Spark","rate_limit":{"primary_window":{
                "used_percent":40,"limit_window_seconds":18000,"reset_after_seconds":1e20}}},
              {"limit_name":"Spark","rate_limit":{"primary_window":{
                "used_percent":15,"limit_window_seconds":18000}}}
            ],
            "code_review_rate_limit":"nope"
        }"#,
        );
        assert_eq!(ids(&ws), ["primary", "spark"]);
        assert_eq!(groups(&ws), [None, Some("Spark")]);
        assert!((ws[1].used - 0.40).abs() < 1e-9);
        assert!(ws[1].resets_at.is_some());
    }

    #[test]
    fn two_spark_extras_do_not_duplicate_window_ids() {
        let ws = windows(
            r#"{
            "rate_limit":{
              "primary_window":{"used_percent":25,"limit_window_seconds":18000},
              "secondary_window":{"used_percent":10,"limit_window_seconds":604800}},
            "additional_rate_limits":[
              {"limit_name":"Spark","rate_limit":{
                "primary_window":{"used_percent":40,"limit_window_seconds":18000}}},
              {"limit_name":"GPT-5.3-Codex-Spark","metered_feature":"spark","rate_limit":{
                "primary_window":{"used_percent":99,"limit_window_seconds":18000},
                "secondary_window":{"used_percent":12,"limit_window_seconds":604800}}}
            ]
        }"#,
        );
        assert_eq!(ids(&ws), ["primary", "secondary", "spark", "spark-secondary"]);
        assert_eq!(groups(&ws), [None, None, Some("Spark"), Some("Spark")]);
        assert_eq!(labels(&ws), ["5h limit", "Weekly limit", "5h limit", "Weekly limit"]);
        assert!((ws[2].used - 0.40).abs() < 1e-9);
        assert!((ws[3].used - 0.12).abs() < 1e-9);
    }

    #[test]
    fn a_non_array_additional_rate_limits_is_ignored() {
        for extras in [
            r#"{"x":{"limit_name":"Spark","rate_limit":{"primary_window":{"used_percent":9,"limit_window_seconds":18000}}}}"#,
            r#""nope""#,
            "null",
        ] {
            let ws = windows(&format!(
                r#"{{"rate_limit":{{"primary_window":{{"used_percent":1,"limit_window_seconds":18000}}}},"additional_rate_limits":{extras}}}"#
            ));
            assert_eq!(ids(&ws), ["primary"], "{extras}");
        }
    }

    #[test]
    fn a_monthly_primary_window_is_not_dropped() {
        let ws = windows(
            r#"{"rate_limit":{"primary_window":{"used_percent":16,"limit_window_seconds":2592000,
            "reset_after_seconds":1838382,"reset_at":1790585722},"secondary_window":null},
             "plan_type":"free"}"#,
        );
        assert_eq!(ids(&ws), ["primary"]);
        assert_eq!(ws[0].label, "Monthly limit");
        assert!((ws[0].used - 0.16).abs() < 1e-4);
    }
}
