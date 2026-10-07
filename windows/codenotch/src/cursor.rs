//! Cursor usage adapter, implemented from the upstream Codenotch's documented behaviour.
//!
//! Data path (same trade-off as upstream: borrow the editor's own session):
//!   1. Credential: the editor keeps its sign-in in the global state database it inherited from
//!      VS Code, `%APPDATA%\Cursor\User\globalStorage\state.vscdb` (SQLite, table ItemTable(key,value)):
//!      `cursorAuth/accessToken` + `cursorAuth/stripeMembershipAuthId`, joined into the cookie
//!      `WorkosCursorSessionToken=<authId>::<token>`. Non-secret identity cache:
//!      `cursorAuth/cachedEmail`, `cursorAuth/stripeMembershipType` (only the plan is shown).
//!   2. Endpoint: `GET https://cursor.com/api/usage-summary` (Cookie + Accept: application/json, 15 s).
//!      Reply:
//!      ```text
//!      { billingCycleEnd, membershipType, isUnlimited,
//!        individualUsage: { plan: { totalPercentUsed, apiPercentUsed, used, limit, breakdown },
//!                           onDemand: { enabled, used, limit } } }
//!      ```
//!      Cursor meters a percentage of the allowance, not requests: the dashboard's
//!      "Included usage · N% used" is totalPercentUsed. On the free plan used/limit are always 0
//!      (the allowance arrives as breakdown.bonus), so reading used/limit would report 10 % as 0 %.
//!      0 is a reading, not a gap (upstream's lesson). "API usage" is listed separately when
//!      apiPercentUsed > 0; "On demand" when onDemand has a real limit.
//!
//! SQLite opening rule: `mode=ro` first (it sees the token the editor just rotated into the WAL),
//! then `immutable=1` (once the editor has exited and the -shm is gone, mode=ro fails to open; by
//! then the WAL has been checkpointed, so ignoring it costs nothing).
//! Read only, never written; token values never reach logs, events or the UI.

use crate::usage::{LimitWindow, UsageSnapshot};
use crate::AppState;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const ENDPOINT: &str = "https://cursor.com/api/usage-summary";
const POLL_SECS: u64 = 300;

static REFRESH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn request_refresh() {
    REFRESH.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Windows: %APPDATA%\Cursor\User\globalStorage\state.vscdb (macOS: ~/Library/Application Support/Cursor/...)
pub fn store_url() -> Option<PathBuf> {
    if let Some(profile) = crate::accounts::active("cursor") {
        return Some(profile.credential_path.unwrap_or_else(|| {
            profile
                .root
                .join("User")
                .join("globalStorage")
                .join("state.vscdb")
        }));
    }
    external_store_url()
}

fn external_store_url() -> Option<PathBuf> {
    dirs::config_dir().map(|c| {
        c.join("Cursor")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb")
    })
}

fn store_path() -> PathBuf {
    crate::accounts::cache_path("cursor", "cursor.json")
}

pub fn load_persisted() -> UsageSnapshot {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|t| serde_json::from_str::<UsageSnapshot>(&t).ok())
        .map(|mut s| {
            if !s.windows.is_empty() {
                s.status = "stale".into();
            }
            s
        })
        .unwrap_or_default()
}

pub fn present() -> bool {
    crate::accounts::active("cursor").is_some() || store_url().map(|p| p.is_file()).unwrap_or(false)
}

// ---------------- SQLite, read only ----------------

/// mode=ro first, immutable=1 as the fallback (see the module doc)
fn open_ro(path: &std::path::Path) -> Option<rusqlite::Connection> {
    use rusqlite::OpenFlags;
    if !path.is_file() {
        return None;
    }
    if let Ok(c) = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        // Actually verify that reads work (with the -shm missing, open can succeed and the first query fail)
        if c.prepare("SELECT 1 FROM ItemTable LIMIT 1")
            .and_then(|mut s| s.query([]).map(|_| ()))
            .is_ok()
        {
            return Some(c);
        }
    }
    // Only the URI form takes immutable=1; a Windows path becomes file:///C:/... with \ → /
    let mut uri = String::from("file:///");
    uri.push_str(
        &path
            .to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches('/')
            .replace('#', "%23")
            .replace('?', "%3F"),
    );
    uri.push_str("?immutable=1");
    rusqlite::Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

fn item(conn: &rusqlite::Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |r| {
        r.get::<_, String>(0)
    })
    .ok()
    .filter(|s| !s.is_empty())
}

struct Creds {
    cookie: String,
    plan: Option<String>,
}

/// Re-read every time: the editor rotates the token, and holding on to an old value signs us out
fn read_credentials() -> Option<Creds> {
    if crate::accounts::active("cursor").is_some() {
        if let Some(secret) = crate::accounts::secret("cursor") {
            return credential_from_saved(&secret);
        }
    }
    credentials_at(&store_url()?)
}

fn credential_from_saved(secret: &str) -> Option<Creds> {
    let v: serde_json::Value = serde_json::from_str(secret).ok()?;
    let token = v.get("accessToken")?.as_str()?.trim();
    let auth_id = v.get("authId")?.as_str()?.trim();
    if token.is_empty() || auth_id.is_empty() {
        return None;
    }
    Some(Creds {
        cookie: format!("WorkosCursorSessionToken={auth_id}::{token}"),
        plan: v.get("plan").and_then(|v| v.as_str()).map(String::from),
    })
}

/// Captures only the editor's current login; the caller encrypts this natively.
/// This string must never be returned through IPC or written to preferences.
pub(crate) fn capture_current_credential() -> Option<String> {
    let conn = open_ro(&external_store_url()?)?;
    Some(
        serde_json::json!({
            "accessToken": item(&conn, "cursorAuth/accessToken")?,
            "authId": item(&conn, "cursorAuth/stripeMembershipAuthId")?,
            "plan": item(&conn, "cursorAuth/stripeMembershipType"),
        })
        .to_string(),
    )
}

fn credentials_at(path: &std::path::Path) -> Option<Creds> {
    let conn = open_ro(path)?;
    let token = item(&conn, "cursorAuth/accessToken")?;
    let auth_id = item(&conn, "cursorAuth/stripeMembershipAuthId")?;
    let plan = item(&conn, "cursorAuth/stripeMembershipType");
    Some(Creds {
        cookie: format!("WorkosCursorSessionToken={auth_id}::{token}"),
        plan,
    })
}

/// For doctor: contains no secret values
pub fn probe() -> String {
    let Some(p) = store_url() else {
        return "Cursor: cannot locate %APPDATA%".into();
    };
    if !p.is_file() {
        return format!(
            "Cursor: {} not found (not installed, or not signed in)",
            p.display()
        );
    }
    match read_credentials() {
        Some(c) => format!(
            "Cursor: session borrowed (cookie {} chars, plan={})",
            c.cookie.len(),
            c.plan.unwrap_or_else(|| "?".into())
        ),
        None => format!("Cursor: {} exists but cursorAuth/* could not be read (editor not signed in, or SQLite failed to open)", p.display()),
    }
}

// ---------------- Parsing ----------------

fn pct(v: Option<&serde_json::Value>) -> Option<f64> {
    v.and_then(|x| x.as_f64())
        .map(|p| (p / 100.0).clamp(0.0, 1.0))
}

fn parse_iso(v: Option<&serde_json::Value>) -> Option<u64> {
    v.and_then(|x| x.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis().max(0) as u64)
}

/// usage-summary → (windows, note). When there are no windows the note says why (Unlimited / free plan without an allowance)
pub fn parse_summary(v: &serde_json::Value) -> (Vec<LimitWindow>, String) {
    let resets_at = parse_iso(v.get("billingCycleEnd"));
    let usage = v
        .get("individualUsage")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let plan = usage
        .get("plan")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let mut out = Vec::new();
    // Headline = the dashboard number; 0 is a reading too
    if let Some(total) = pct(plan.get("totalPercentUsed")) {
        out.push(LimitWindow {
            id: "included".into(),
            label: "Included usage".into(),
            used: total,
            resets_at,
            ..Default::default()
        });
    }
    if let Some(api) = pct(plan.get("apiPercentUsed")) {
        if api > 0.0 {
            out.push(LimitWindow {
                id: "api".into(),
                label: "API usage".into(),
                used: api,
                resets_at,
                ..Default::default()
            });
        }
    }
    if let Some(od) = usage.get("onDemand") {
        let enabled = od.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false);
        let limit = od.get("limit").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let used = od.get("used").and_then(|x| x.as_f64());
        if enabled && limit > 0.0 {
            if let Some(u) = used {
                out.push(LimitWindow {
                    id: "on_demand".into(),
                    label: "On demand".into(),
                    used: (u / limit).clamp(0.0, 1.0),
                    resets_at,
                    ..Default::default()
                });
            }
        }
    }
    if !out.is_empty() {
        return (out, String::new());
    }
    let membership = v
        .get("membershipType")
        .and_then(|x| x.as_str())
        .unwrap_or("this");
    let note = if v.get("isUnlimited").and_then(|x| x.as_bool()) == Some(true) {
        format!("Unlimited on the {membership} plan — nothing to meter")
    } else {
        format!("The {membership} plan has nothing for Cursor to meter yet")
    };
    (out, note)
}

enum FetchErr {
    NeedsAuth,
    RateLimited(u64),
    Other(String),
}

fn fetch_once(cookie: &str) -> Result<serde_json::Value, FetchErr> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build();
    match agent
        .get(ENDPOINT)
        .set("Cookie", cookie)
        .set("Accept", "application/json")
        .call()
    {
        Ok(r) => r
            .into_json::<serde_json::Value>()
            .map_err(|e| FetchErr::Other(format!("parse: {e}"))),
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            Err(FetchErr::NeedsAuth)
        }
        Err(ureq::Error::Status(429, response)) => Err(FetchErr::RateLimited(
            response
                .header("retry-after")
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(60)
                .max(60),
        )),
        Err(ureq::Error::Status(code, _)) => Err(FetchErr::Other(format!("HTTP {code}"))),
        Err(e) => Err(FetchErr::Other(format!("{e}"))),
    }
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn read_once(prev: &UsageSnapshot, captured: Option<Creds>) -> UsageSnapshot {
    let mut snap = prev.clone();
    if snap.backoff_until > now_ms() {
        snap.note = format!(
            "Rate limited — retrying in {}s",
            snap.backoff_until.saturating_sub(now_ms()) / 1000
        );
        return snap;
    }
    let Some(creds) = captured else {
        snap.status = "needsAuth".into();
        snap.note = "Sign in to Cursor (the editor) to see usage.".into();
        return snap;
    };
    match fetch_once(&creds.cookie) {
        Ok(v) => {
            snap.backoff_until = 0;
            let (windows, note) = parse_summary(&v);
            snap.fetched_at = now_ms();
            if windows.is_empty() {
                snap.status = "none".into();
                snap.windows.clear();
                snap.note = note;
            } else {
                snap.status = "ok".into();
                snap.windows = windows;
                snap.note = match (
                    &creds.plan,
                    v.get("membershipType").and_then(|x| x.as_str()),
                ) {
                    (_, Some(m)) => format!("{} · via Cursor", cap(m)),
                    (Some(p), None) => format!("{} · via Cursor", cap(p)),
                    _ => String::new(),
                };
            }
        }
        Err(FetchErr::NeedsAuth) => {
            snap.status = "needsAuth".into();
            snap.note = "Cursor session was rejected — sign in again in the editor".into();
        }
        Err(FetchErr::RateLimited(wait)) => {
            snap.backoff_until = now_ms().saturating_add(wait.saturating_mul(1000));
            snap.status = if snap.windows.is_empty() {
                "error"
            } else {
                "stale"
            }
            .into();
            snap.note = format!("Cursor is rate limiting; retrying in {wait}s");
        }
        Err(FetchErr::Other(msg)) => {
            // Stale beats invented: keep the old reading, marked stale
            snap.status = if snap.windows.is_empty() {
                "error"
            } else {
                "stale"
            }
            .into();
            snap.note = msg;
        }
    }
    snap
}

fn broadcast(app: &AppHandle, snap: UsageSnapshot, selection: &str) {
    let _ = crate::accounts::with_selection("cursor", selection, || {
        let st = app.state::<AppState>();
        *st.cursor.lock().unwrap() = snap.clone();
        let _ = app.emit("cursor", &snap);
    });
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        loop {
            let selection = crate::accounts::selection_key("cursor");
            // Freeze the credential and its cache while account switching is
            // locked. Network I/O then runs without holding the settings lock.
            let Some((prev, cache, creds, installed)) =
                crate::accounts::with_selection("cursor", &selection, || {
                    (
                        load_persisted(),
                        store_path(),
                        read_credentials(),
                        present(),
                    )
                })
            else {
                continue;
            };
            let snap = if installed {
                read_once(&prev, creds)
            } else {
                UsageSnapshot {
                    status: "absent".into(),
                    ..Default::default()
                }
            };
            let hold = snap.backoff_until.saturating_sub(now_ms()) / 1000;
            // An old credential must not restore cached usage after a profile
            // reconnect. Preserve only provider Retry-After on rejected reads.
            if crate::accounts::with_selection("cursor", &selection, || {
                crate::agy_cli::save_persisted_to(&cache, &snap)
            })
            .is_none()
            {
                crate::accounts::preserve_backoff(&cache, snap.backoff_until);
            }
            broadcast(&app, snap, &selection);
            for _ in 0..POLL_SECS.max(hold) {
                if REFRESH.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_selected_accounts_retry_after_is_not_bypassed_by_refresh() {
        let prev = UsageSnapshot {
            backoff_until: now_ms() + 120_000,
            note: "previous".into(),
            ..Default::default()
        };
        let snap = read_once(&prev, None);
        assert_eq!(snap.backoff_until, prev.backoff_until);
        assert!(snap.note.starts_with("Rate limited"));
    }

    use super::*;

    #[test]
    fn saved_sessions_require_their_own_token_and_auth_id() {
        let a = credential_from_saved(r#"{"accessToken":"token-a","authId":"id-a","plan":"pro"}"#)
            .unwrap();
        let b = credential_from_saved(r#"{"accessToken":"token-b","authId":"id-b"}"#).unwrap();
        assert_eq!(a.cookie, "WorkosCursorSessionToken=id-a::token-a");
        assert_eq!(b.cookie, "WorkosCursorSessionToken=id-b::token-b");
        assert!(credential_from_saved(r#"{"accessToken":"","authId":"id-a"}"#).is_none());
        assert!(credential_from_saved(r#"{"accessToken":"token-a"}"#).is_none());
    }

    #[test]
    fn explicit_editor_stores_do_not_borrow_a_different_profile() {
        let base =
            std::env::temp_dir().join(format!("codenotch-cursor-stores-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        for (name, token) in [("a", "token-a"), ("b", "token-b")] {
            let path = base.join(format!("{name}.vscdb"));
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)")
                .unwrap();
            conn.execute(
                "INSERT INTO ItemTable VALUES ('cursorAuth/accessToken', ?1)",
                [token],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO ItemTable VALUES ('cursorAuth/stripeMembershipAuthId', ?1)",
                [name],
            )
            .unwrap();
        }
        assert_eq!(
            credentials_at(&base.join("a.vscdb")).unwrap().cookie,
            "WorkosCursorSessionToken=a::token-a"
        );
        assert_eq!(
            credentials_at(&base.join("b.vscdb")).unwrap().cookie,
            "WorkosCursorSessionToken=b::token-b"
        );
        assert!(credentials_at(&base.join("missing.vscdb")).is_none());
        std::fs::remove_dir_all(base).unwrap();
    }
}
