//! Claude usage adapter (official), implemented from the upstream Codenotch's documented behaviour.
//! Endpoint: GET https://api.anthropic.com/api/oauth/usage
//! Headers: Authorization: Bearer <token>; anthropic-beta: oauth-2025-04-20; 15 s timeout
//! Rules (upstream's discipline):
//!   - the credential comes from Claude Code's own store (Windows: ~/.claude/.credentials.json), read only
//!   - Settings selects one native profile. Its CLAUDE_CONFIG_DIR, credential, usage cache,
//!     renewal and reset deadlines stay together. Local .claude-* directories remain discoverable
//!     and observable for activity, but never merge another account's freshness into the selected quota.
//!   - 401 → re-read the credential once and retry (Claude Code may have just refreshed the token) → still
//!     failing means needsAuth; 403 is access denied, not proof of lost authentication
//!   - 429 → back off 60 s × 2^n capped at 15 min, Retry-After only raises it, even past the cap; the deadline is persisted
//!   - an expired token is never sent: the endpoint answers it with 429 + Retry-After ≈ 3600, not 401, so sending it
//!     reads as "rate limited" for as long as the token stays stale (upstream's credentialExpired, no network)
//!   - the token is renewed by running the standalone `claude -p` with an empty stdin shortly before it expires
//!     (upstream's ClaudeTokenRefresher). Only that CLI writes ~/.claude/.credentials.json — Claude Code inside the
//!     desktop app renews its own copy elsewhere — so without this the file rots eight hours after the last CLI run
//!   - never invent a percentage on failure: keep the last reading marked stale, and the UI shows how old it is
//!
//! Reply (snake_case): { limits:[{kind,percent,resets_at}], five_hour:{utilization,resets_at}, seven_day:{...} }
//! limits is the forward-compatible main shape; five_hour/seven_day are merged in as a fallback (a window that just rolled over disappears from limits).

use crate::AppState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const POLL_ACTIVE_SECS: u64 = 60;
const POLL_IDLE_SECS: u64 = 60;
const BACKOFF_BASE_SECS: u64 = 60;
const BACKOFF_CAP_SECS: u64 = 900;
/// Renew when this close to expiry. Must stay under Claude Code's own five minutes: its start-up renews the token
/// only when now + 300 s >= expiresAt, so launching any earlier is a no-op that would be judged a failure
const RENEW_MARGIN_MS: u64 = 4 * 60 * 1000;
const RENEW_COOLDOWN_MS: u64 = 10 * 60 * 1000;
/// A token that did not renew is tried again, each wait twice the last, never more than an hour apart
const RENEW_RETRY_CAP_MS: u64 = 60 * 60 * 1000;
const RENEW_TIMEOUT_SECS: u64 = 30;
const EXPIRED_NOTE: &str = "Credential expired — run claude once in a terminal to renew it";

static REFRESH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Immediate refresh from the tray or a command
pub fn request_refresh() {
    REFRESH.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Sleep in slices so request_refresh can interrupt it
fn sleep_interruptible(total_secs: u64) {
    for _ in 0..total_secs {
        if REFRESH.swap(false, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// The server supplies absolute reset deadlines, so wake at the earliest one
/// rather than delaying a reset confirmation until the next regular poll.
pub(crate) fn refresh_delay_secs(windows: &[LimitWindow], now: u64, base: u64) -> u64 {
    windows.iter().filter_map(|w| w.resets_at).filter(|r| *r > now)
        .map(|r| ((r - now) / 1000).max(1)).min().unwrap_or(base).min(base).max(1)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

const CRED_NAMES: [&str; 2] = [".credentials.json", "credentials.json"];

/// One Claude Code account, as its config directory. `slug` is None for the default ~/.claude and
/// Some("work") for ~/.claude-work.
#[derive(Debug, Clone, PartialEq)]
struct Profile {
    dir: PathBuf,
    slug: Option<String>,
}

impl Profile {
    fn name(&self) -> String {
        self.slug.clone().unwrap_or_else(|| "default".into())
    }

    /// The heading the card files this account's windows under. The plan is what tells two accounts
    /// apart at a glance ("max" against "pro"); the slug is what stays unique when both plans match.
    fn group(&self, plan: Option<&str>) -> String {
        match plan {
            Some(p) if !p.is_empty() => format!("{} · {p}", self.name()),
            _ => self.name(),
        }
    }
}

fn has_credential(dir: &Path) -> bool {
    CRED_NAMES.iter().any(|n| dir.join(n).is_file())
}

/// Every account on the machine: the default first, then ~/.claude-<slug> in name order. A secondary
/// directory counts only once it holds a credential, so a half-made one never shows up on the card as
/// an account waiting to be signed in.
fn discovered_profiles() -> Vec<Profile> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let default_dir = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|p| !p.is_empty())
        .map(PathBuf::from).unwrap_or_else(|| home.join(".claude"));
    let mut out = vec![Profile { dir: default_dir, slug: None }];
    let mut extra: Vec<Profile> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&home) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Some(slug) = name.strip_prefix(".claude-") else {
                continue;
            };
            let dir = e.path();
            if slug.is_empty() || !dir.is_dir() || !has_credential(&dir) {
                continue;
            }
            extra.push(Profile { dir, slug: Some(slug.to_string()) });
        }
    }
    extra.sort_by(|a, b| a.slug.cmp(&b.slug));
    extra.retain(|p| p.dir != out[0].dir);
    out.append(&mut extra);
    out
}

/// Only the selected account contributes quota. Automatic discovery belongs in Settings,
/// where a user can distinguish accounts before choosing whose reset to track.
fn profiles() -> Vec<Profile> {
    match crate::accounts::active("claude") {
        Some(p) => vec![Profile { dir: p.root, slug: None }],
        None => discovered_profiles().into_iter().take(1).collect(),
    }
}

/// Activity watchers still observe local Claude session directories.
pub fn profile_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = discovered_profiles().into_iter().map(|p| p.dir).collect();
    for p in profiles() {
        if !dirs.contains(&p.dir) { dirs.push(p.dir); }
    }
    dirs
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LimitWindow {
    pub id: String,
    pub label: String,
    /// 0.0–1.0 (fraction used)
    pub used: f64,
    /// Reset time, ms epoch (None = unknown)
    pub resets_at: Option<u64>,
    /// Pure count window (no published denominator, e.g. Antigravity's requests today) — the cell shows ~N and the ring draws only its track
    #[serde(default)]
    pub count: Option<i64>,
    /// The number is ours, not the vendor's (upstream fidelity=.derived) — the card adds a ~ prefix
    #[serde(default)]
    pub derived: bool,
    /// The heading the window sits under on the card, for a provider that reports the same windows
    /// for several things (Antigravity: a 5-hour and a weekly lane per model family). None = ungrouped
    #[serde(default)]
    pub group: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageSnapshot {
    /// ok | stale | needsAuth | backoff | error
    pub status: String,
    pub windows: Vec<LimitWindow>,
    pub fetched_at: u64,
    pub note: String,
    #[serde(default)]
    pub backoff_until: u64,
}

fn store_path() -> std::path::PathBuf {
    crate::accounts::cache_path("claude", "usage.json")
}

pub fn load_persisted() -> UsageSnapshot {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|t| serde_json::from_str::<UsageSnapshot>(&t).ok())
        .map(|mut snap| {
            // Older versions merged ~/.claude-* into the default cache. Those rows cannot
            // establish the selected account's freshness or trigger its reset notifications.
            if snap.windows.iter().any(|w| w.id.contains('@')) {
                snap.windows.retain(|w| !w.id.contains('@'));
                snap.status = "stale".into();
                snap.fetched_at = 0;
                snap.note = "Refreshing the selected Claude account...".into();
            }
            snap
        })
        // The status it was saved with is the status it comes back with: a reading persisted a
        // minute before a restart is a minute old, not stale, and `fetched_at` came back with it,
        // so whatever reads this can tell the difference on its own.
        .unwrap_or_default()
}

fn persist(s: &UsageSnapshot) { persist_at(&store_path(), s); }

fn persist_at(path: &Path, s: &UsageSnapshot) {
    if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
    if let Ok(t) = serde_json::to_string_pretty(s) { let _ = std::fs::write(path, t); }
}

#[derive(Default)]
struct Credential {
    token: String,
    /// ms epoch (None = the file names no expiry)
    expires_at: Option<u64>,
    /// "max" | "pro" | … as the credential names it, for the card's account heading
    plan: Option<String>,
}

impl Credential {
    fn expired(&self, now: u64) -> bool {
        self.expires_at.map(|e| e <= now).unwrap_or(false)
    }
}

/// Reads Claude Code's OAuth credential.
fn read_credentials(dir: &Path) -> Option<Credential> {
    for name in CRED_NAMES {
        let p = dir.join(name);
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let oauth = v.get("claudeAiOauth").unwrap_or(&v);
        if let Some(tok) = oauth.get("accessToken").and_then(|x| x.as_str()) {
            // An empty token is signed out, not expired: fall through to the next
            // candidate file rather than report a credential that cannot be used.
            if tok.trim().is_empty() {
                continue;
            }
            let expires_at = oauth.get("expiresAt").and_then(|x| x.as_f64()).map(|ms| ms as u64);
            let plan = oauth.get("subscriptionType").and_then(|x| x.as_str()).map(String::from);
            return Some(Credential { token: tok.to_string(), expires_at, plan });
        }
    }
    None
}

/// For doctor: credential probe report (prints no secret values)
pub fn profile_has_credential(root: &Path) -> bool { read_credentials(root).is_some() }

/// The native global profile config carries email, while .credentials carries OAuth.
/// Return a label only; never expose a token or a refresh secret over IPC.
pub fn profile_identity(root: &Path) -> Option<String> {
    let candidates = [root.join(".claude.json"), root.join("config.json"), root.with_extension("json")];
    for path in candidates {
        let Some(v) = std::fs::read_to_string(path).ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) else { continue; };
        if let Some(email) = v.pointer("/oauthAccount/emailAddress").or_else(|| v.get("email"))
            .and_then(|s| s.as_str()).filter(|s| s.len() <= 254) {
            return Some(email.to_owned());
        }
    }
    None
}

pub fn probe_credentials() -> String {
    let cli = match find_cli() {
        Some(p) => format!("renews via {}", p.display()),
        None => "no standalone claude CLI found to renew it".into(),
    };
    let list = profiles();
    if list.is_empty() {
        return format!("credential: no home directory to read ~/.claude from; {cli}");
    }
    let lines: Vec<String> = list
        .iter()
        .map(|p| match read_credentials(&p.dir) {
            Some(c) => format!(
                "credential[{}]: found (token {} chars, {}, plan {})",
                p.name(),
                c.token.len(),
                if c.expired(now_ms()) { "expired" } else { "valid" },
                c.plan.as_deref().unwrap_or("?")
            ),
            None => format!(
                "credential[{}]: {} not found (needsAuth; the desktop app may use another store — signing in once with the Claude Code CLI creates it)",
                p.name(),
                p.dir.join(CRED_NAMES[0]).display()
            ),
        })
        .collect();
    format!("{}; {cli}", lines.join("
  "))
}

// ---------------- token renewal (upstream's ClaudeTokenRefresher) ----------------

/// Anything under these belongs to the desktop app: its bundled Claude Code keeps its token in the desktop app's
/// own store and never writes ~/.claude/.credentials.json, so renewing with it would change nothing here
fn is_desktop_owned(p: &std::path::Path) -> bool {
    let s = p.to_string_lossy().to_ascii_lowercase().replace('/', "\\");
    s.contains("\\anthropicclaude\\") || s.contains("\\claude\\claude-code\\") || s.contains("\\windowsapps\\")
}

/// The command file names to try for a CLI, most specific first. Windows needs the
/// native exe before the .cmd shim; elsewhere there is only the bare name.
pub(crate) fn command_names(stem: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![format!("{stem}.exe"), format!("{stem}.cmd")]
    } else {
        vec![stem.to_string()]
    }
}

/// The standalone Claude Code command: its own installer's location first, then global npm/pnpm/Volta, then PATH
pub(crate) fn find_cli() -> Option<std::path::PathBuf> {
    let names = command_names("claude");
    let mut v = Vec::new();
    let push_all = |dir: std::path::PathBuf, v: &mut Vec<std::path::PathBuf>| {
        for n in &names {
            v.push(dir.join(n));
        }
    };
    if let Some(h) = dirs::home_dir() {
        push_all(h.join(".local").join("bin"), &mut v);
    }
    if let Some(d) = dirs::config_dir() {
        push_all(d.join("npm"), &mut v);
    }
    if let Some(d) = dirs::data_local_dir() {
        push_all(d.join("pnpm"), &mut v);
    }
    if let Some(h) = dirs::home_dir() {
        push_all(h.join(".volta").join("bin"), &mut v);
        // npm's global prefix and nvm's per-version bin are where a Linux install usually lands
        push_all(h.join(".npm-global").join("bin"), &mut v);
        push_all(h.join(".bun").join("bin"), &mut v);
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            push_all(dir, &mut v);
        }
    }
    v.into_iter().find(|p| p.is_file() && !is_desktop_owned(p))
}

/// Whether a launch is worth making. Pure, so every branch is testable without a clock or a subprocess.
/// grok.rs times its CLI launches by the same rule.
pub(crate) fn should_renew(
    expires_at: Option<u64>,
    now: u64,
    attempted_for: Option<u64>,
    last_attempt: Option<u64>,
    failures: u32,
) -> bool {
    // Nothing read yet: never launch on a guess
    let Some(exp) = expires_at else { return false };
    // Plenty of time left — also where launching would do nothing, because the CLI's own gate has not opened
    if exp > now + RENEW_MARGIN_MS {
        return false;
    }
    let Some(t) = last_attempt else { return true };
    // A launch that failed to move the expiry leaves the same value here. One failed launch — asleep, offline, a
    // busy CLI — must not freeze the ring until someone opens a terminal, so the same token is tried again, but
    // on a doubling wait, so a token that cannot renew does not become a launch every tick
    let wait = if attempted_for == Some(exp) { retry_wait_ms(failures) } else { RENEW_COOLDOWN_MS };
    now.saturating_sub(t) >= wait
}

fn retry_wait_ms(failures: u32) -> u64 {
    RENEW_COOLDOWN_MS.saturating_mul(1u64 << failures.min(16)).min(RENEW_RETRY_CAP_MS)
}

/// `claude -p` with a null stdin starts up (which is where it renews an aged token), then exits non-zero for want
/// of a prompt: no conversation, no transcript. Output goes nowhere — a token could in principle be echoed into it.
fn renewal_command(cli: &Path, dir: &Path) -> std::process::Command {
    use std::process::{Command, Stdio};
    #[cfg(windows)]
    let mut cmd = if cli.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("cmd")) {
        // A cmd shim cannot be CreateProcess'd directly. PowerShell invokes the path as data.
        let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let mut command = Command::new(root.join("System32/WindowsPowerShell/v1.0/powershell.exe"));
        command.args(["-NoLogo", "-NoProfile", "-Command", "& $env:CODENOTCH_CLAUDE_CLI -p; exit $LASTEXITCODE"])
            .env("CODENOTCH_CLAUDE_CLI", cli);
        command
    } else { let mut command = Command::new(cli); command.arg("-p"); command };
    #[cfg(not(windows))]
    let mut cmd = { let mut command = Command::new(cli); command.arg("-p"); command };
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // Launched from inside a Claude Code session, the child would take the host's auth and leave the file alone
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy();
        if k == "CLAUDECODE" || k.starts_with("CLAUDE_CODE_")
            || matches!(k.as_ref(), "ANTHROPIC_API_KEY" | "ANTHROPIC_AUTH_TOKEN" | "CLAUDE_CODE_OAUTH_TOKEN") {
            cmd.env_remove(k.as_ref());
        }
    }
    // Which account gets renewed is said here, never inherited: CLAUDE_CONFIG_DIR is not CLAUDE_CODE_*, so it
    // survives the loop above, and a Codenotch started from a shell pointed at another account used to renew
    // that one while the account on screen stayed expired.
    cmd.env("CLAUDE_CONFIG_DIR", dir)
        .env_remove("ANTHROPIC_API_KEY").env_remove("ANTHROPIC_AUTH_TOKEN").env_remove("CLAUDE_CODE_OAUTH_TOKEN");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    // Use a neutral profile directory, never a repository with unrelated project hooks.
    cmd.current_dir(dir);
    cmd
}

fn run_renewal(cli: &Path, dir: &Path) -> std::io::Result<()> {
    let mut child = renewal_command(cli, dir).spawn()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(RENEW_TIMEOUT_SECS);
    while child.try_wait()?.is_none() {
        if std::time::Instant::now() >= deadline {
            crate::claude_auth::terminate(&mut child);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[derive(Default)]
struct Renewer {
    attempted_for: Option<u64>,
    last_attempt: Option<u64>,
    /// Launches in a row that left `attempted_for` where it was
    failures: u32,
}

impl Renewer {
    /// Renews if the token is about to expire. Some(true) = the expiry moved; judged on the outcome, never on the
    /// exit status, because refusing the empty prompt is a non-zero exit and a successful renewal at the same time
    fn maybe_renew(&mut self, cred: &Credential, dir: &Path, who: &str) -> Option<bool> {
        let _auth = crate::claude_auth::try_acquire()?;
        let now = now_ms();
        if !should_renew(cred.expires_at, now, self.attempted_for, self.last_attempt, self.failures) {
            return None;
        }
        if self.attempted_for != cred.expires_at {
            self.failures = 0;
        }
        self.last_attempt = Some(now);
        self.attempted_for = cred.expires_at;
        self.failures = self.failures.saturating_add(1);
        let Some(cli) = find_cli() else {
            crate::applog(&format!(
                "claude[{who}]: token about to expire and no standalone claude CLI found to renew it"
            ));
            return Some(false);
        };
        if let Err(e) = run_renewal(&cli, dir) {
            crate::applog(&format!("claude[{who}]: token renewal could not start ({}): {e}", cli.display()));
            return Some(false);
        }
        let after = read_credentials(dir).and_then(|c| c.expires_at);
        let renewed = matches!((after, cred.expires_at), (Some(a), Some(b)) if a > b);
        crate::applog(&if renewed {
            format!("claude[{who}]: token renewed via {}", cli.display())
        } else {
            format!("claude[{who}]: ran {} but the token expiry did not move", cli.display())
        });
        Some(renewed)
    }
}

fn parse_reset(v: &serde_json::Value) -> Option<u64> {
    v.as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis().max(0) as u64)
}

fn label_for(kind: &str) -> String {
    match kind {
        "session" => "Current session".into(),
        "seven_day" | "weekly_all" => "Weekly (all models)".into(),
        "seven_day_opus" | "weekly_opus" => "Weekly (Opus)".into(),
        "weekly_scoped" => "Weekly (model-scoped)".into(),
        other => {
            // Forward compatibility: an unknown kind gets a readable label
            let mut s = other.replace('_', " ");
            if let Some(c) = s.get_mut(0..1) {
                c.make_ascii_uppercase();
            }
            s
        }
    }
}

fn parse_response(v: &serde_json::Value) -> Vec<LimitWindow> {
    let mut out: Vec<LimitWindow> = Vec::new();
    if let Some(arr) = v.get("limits").and_then(|x| x.as_array()) {
        for l in arr {
            let Some(kind) = l.get("kind").and_then(|x| x.as_str()) else {
                continue;
            };
            let Some(pct) = l.get("percent").and_then(|x| x.as_f64()) else {
                continue;
            };
            let resets = l.get("resets_at").and_then(parse_reset);
            if resets.is_none() {
                continue; // upstream rule: a window without a reset time is not shown
            }
            out.push(LimitWindow {
                id: kind.to_string(),
                label: label_for(kind),
                used: (pct / 100.0).clamp(0.0, 1.0),
                resets_at: resets, ..Default::default()
            });
        }
    }
    // Fallback merge: a window that just rolled over disappears from limits while the named field remains.
    // In practice the kinds in limits are weekly_all/weekly_scoped, not seven_day — deduplicating by id
    // alone would add the seven_day fallback a second time (the card showed "Weekly all" and
    // "Weekly (all models)" as twins). Three dedupe rules: id alias / same resets_at and percentage / same label.
    let aliases: [(&str, &str, &[&str]); 2] = [
        ("five_hour", "session", &["session", "five_hour"]),
        ("seven_day", "seven_day", &["seven_day", "weekly_all", "weekly"]),
    ];
    for (field, id, alias) in aliases {
        let Some(w) = v.get(field) else { continue };
        let Some(u) = w.get("utilization").and_then(|x| x.as_f64()) else { continue };
        let used = (u / 100.0).clamp(0.0, 1.0);
        let resets_at = w.get("resets_at").and_then(parse_reset);
        let label = label_for(id);
        let dup = out.iter().any(|x| {
            alias.contains(&x.id.as_str())
                || x.label == label
                || (resets_at.is_some()
                    && x.resets_at.map(|r| r / 1000) == resets_at.map(|r| r / 1000)
                    && (x.used - used).abs() < 0.005)
        });
        if dup {
            continue;
        }
        out.push(LimitWindow { id: id.into(), label, used, resets_at, ..Default::default() });
    }
    // session always comes first (upstream display order)
    out.sort_by_key(|w| if w.id == "session" { 0 } else { 1 });
    out
}

enum FetchErr {
    NeedsAuth,
    RateLimited(u64), // suggested wait in seconds (the Retry-After before the floor is applied)
    Other(String),
}

fn fetch_once(token: &str) -> Result<Vec<LimitWindow>, FetchErr> {
    let resp = ureq::get(ENDPOINT)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .timeout(Duration::from_secs(15))
        .call();
    match resp {
        Ok(r) => {
            let v: serde_json::Value = r
                .into_json()
                .map_err(|e| FetchErr::Other(format!("parse: {e}")))?;
            Ok(parse_response(&v))
        }
        Err(ureq::Error::Status(401, _)) => Err(FetchErr::NeedsAuth),
        Err(ureq::Error::Status(403, _)) => Err(FetchErr::Other(
            "Claude HTTP 403: access denied. Check network or account access; sign-in may still be valid.".into())),
        Err(ureq::Error::Status(429, r)) => {
            let ra = r
                .header("retry-after")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            Err(FetchErr::RateLimited(ra))
        }
        Err(ureq::Error::Status(code, _)) => Err(FetchErr::Other(format!("HTTP {code}"))),
        Err(e) => Err(FetchErr::Other(format!("{e}"))),
    }
}

fn backoff_secs(consecutive: u32, retry_after_floor: u64) -> u64 {
    let exp = BACKOFF_BASE_SECS.saturating_mul(1u64 << consecutive.min(4));
    // The server's Retry-After is honoured in full: with expired tokens no longer
    // sent, a long one is a real rate limit, and retrying early only earns another.
    exp.clamp(BACKOFF_BASE_SECS, BACKOFF_CAP_SECS).max(retry_after_floor)
}

fn set_and_broadcast(app: &AppHandle, selection: &str, snap: UsageSnapshot) -> bool {
    crate::accounts::with_selection("claude", selection, || {
        let st = app.state::<AppState>();
        *st.usage.lock().unwrap() = snap.clone();
        persist(&snap);
        let _ = app.emit("usage", &snap);
    }).is_some()
}

/// What one account contributes to the shared reading. Kept across ticks so a refresh that fails for
/// one account keeps showing that account's last good windows, and never blanks the other one.
#[derive(Default)]
struct Account {
    renewer: Renewer,
    consecutive_429: u32,
    backoff_until: u64,
    windows: Vec<LimitWindow>,
    status: String,
    note: String,
    fetched_at: u64,
}

fn key(p: &Profile) -> String {
    p.dir.to_string_lossy().to_string()
}

/// The account's windows as they go on the card: its name in `group`, and for a secondary account an
/// id suffixed with the slug -- which is what keeps `by_id("session")` in the notch meaning the default
/// account's session and not whichever account answered first.
fn decorate(mut windows: Vec<LimitWindow>, p: &Profile, group: Option<&str>) -> Vec<LimitWindow> {
    for w in &mut windows {
        if let Some(g) = group {
            w.group = Some(g.to_string());
        }
        if let Some(slug) = &p.slug {
            w.id = format!("{}@{slug}", w.id);
        }
    }
    windows
}

/// Hands each account back the windows it contributed before the restart: a secondary account's ids
/// carry `@slug`, so both accounts come back from disk instead of only the default one.
fn split_persisted(snap: &UsageSnapshot, order: &[Profile]) -> HashMap<String, Vec<LimitWindow>> {
    let mut out: HashMap<String, Vec<LimitWindow>> = HashMap::new();
    for w in &snap.windows {
        let owner = order.iter().find(|p| match &p.slug {
            Some(sl) => w.id.ends_with(&format!("@{sl}")),
            None => !w.id.contains('@'),
        });
        if let Some(p) = owner {
            out.entry(key(p)).or_default().push(w.clone());
        }
    }
    out
}

/// One reading out of every account's, in profile order. The status is the best news any account has:
/// a second account that needs signing in must not dim a first one that just answered.
fn aggregate(order: &[Profile], accounts: &HashMap<String, Account>) -> UsageSnapshot {
    let rank = |s: &str| match s {
        "ok" => 0,
        "stale" => 1,
        "error" => 2,
        _ => 3, // needsAuth, and anything not set yet
    };
    let multi = order.len() > 1;
    let mut snap = UsageSnapshot::default();
    let mut notes: Vec<String> = Vec::new();
    let mut best = 4;
    for p in order {
        let Some(a) = accounts.get(&key(p)) else {
            continue;
        };
        snap.windows.extend(a.windows.iter().cloned());
        snap.fetched_at = snap.fetched_at.max(a.fetched_at);
        if !a.status.is_empty() && rank(a.status.as_str()) < best {
            best = rank(a.status.as_str());
            snap.status = a.status.clone();
        }
        if !a.note.is_empty() {
            notes.push(if multi { format!("{}: {}", p.name(), a.note) } else { a.note.clone() });
        }
        // The soonest deadline is the one worth waking for
        if a.backoff_until > 0 && (snap.backoff_until == 0 || a.backoff_until < snap.backoff_until) {
            snap.backoff_until = a.backoff_until;
        }
    }
    if snap.status.is_empty() {
        snap.status = "needsAuth".into();
    }
    snap.note = notes.join(" · ");
    snap
}

/// One account's turn: renew if the token is aging, then read it, exactly as the single-account loop did.
fn poll_account(p: &Profile, acc: &mut Account, group: Option<&str>) {
    let who = p.name();
    // Ahead of the back-off: renewing never touches the usage endpoint, and a fresh token deserves a fresh try
    if let Some(cred) = read_credentials(&p.dir) {
        if acc.renewer.maybe_renew(&cred, &p.dir, &who) == Some(true) {
            acc.consecutive_429 = 0;
            acc.backoff_until = 0;
        }
    }
    // No requests inside this account's back-off window
    if acc.backoff_until > now_ms() {
        return;
    }
    match read_credentials(&p.dir) {
        None => {
            acc.status = "needsAuth".into();
            acc.note = "No Claude Code credential found".into();
        }
        // Expired is not signed out: keep the last reading, dimmed and dated, and send nothing
        Some(cred) if cred.expired(now_ms()) => {
            acc.status = if acc.windows.is_empty() { "needsAuth" } else { "stale" }.into();
            acc.note = EXPIRED_NOTE.into();
        }
        Some(cred) => {
            let token = cred.token;
            // On 401 re-read the credential and retry once (Claude Code may have just refreshed it)
            let result = match fetch_once(&token) {
                Err(FetchErr::NeedsAuth) => match read_credentials(&p.dir) {
                    Some(c2) if c2.token != token => fetch_once(&c2.token),
                    _ => Err(FetchErr::NeedsAuth),
                },
                other => other,
            };
            match result {
                Ok(windows) => {
                    crate::claude_auth::usage_succeeded();
                    acc.consecutive_429 = 0;
                    acc.status = "ok".into();
                    acc.windows = decorate(windows, p, group);
                    acc.fetched_at = now_ms();
                    acc.note.clear();
                    acc.backoff_until = 0;
                }
                Err(FetchErr::NeedsAuth) => {
                    acc.status = "needsAuth".into();
                    acc.note = "Credential rejected (switched accounts?)".into();
                }
                Err(FetchErr::RateLimited(ra)) => {
                    acc.consecutive_429 += 1;
                    let wait = backoff_secs(acc.consecutive_429 - 1, ra);
                    // The status is left alone: a refused refresh says nothing about the reading we are
                    // holding, which is exactly as old as it was a moment ago. Marking it stale here
                    // dimmed the ring on the first 429, which on Windows is often the first minute of a
                    // rate limit. Age decides, as it does on the Mac (`UsageStore` keeps the previous
                    // status until `staleAfter`), and the note says why it is not moving.
                    acc.note = format!("Rate limited, retrying in {wait}s");
                    acc.backoff_until = now_ms() + wait * 1000;
                }
                Err(FetchErr::Other(msg)) => {
                    // No reading at all is an error worth showing; a reading we could not refresh is
                    // just a reading, and its own age is what makes it stale.
                    if acc.windows.is_empty() {
                        acc.status = "error".into();
                    }
                    acc.note = msg;
                }
            }
        }
    }
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        let mut selection = String::new();
        let mut accounts: HashMap<String, Account> = HashMap::new();
        loop {
            let current = crate::accounts::selection_key("claude");
            let Some((order, cache, previous)) = crate::accounts::with_selection("claude", &current,
                || (profiles(), store_path(), load_persisted())) else { continue; };
            if current != selection {
                selection = current;
                accounts.clear();
                for p in &order {
                    accounts.insert(key(p), Account {
                        windows: previous.windows.clone(), status: previous.status.clone(),
                        note: previous.note.clone(), fetched_at: previous.fetched_at,
                        backoff_until: previous.backoff_until, ..Default::default()
                    });
                }
                set_and_broadcast(&app, &selection, previous);
            }
            if crate::claude_auth::state().busy { sleep_interruptible(1); continue; }
            for p in &order {
                let acc = accounts.entry(key(p)).or_default();
                poll_account(p, acc, None);
            }
            let snap = aggregate(&order, &accounts);
            let backoff_until = snap.backoff_until;
            let next_reset = refresh_delay_secs(&snap.windows, now_ms(), POLL_ACTIVE_SECS.min(POLL_IDLE_SECS));
            // A late result cannot restore quota after a re-login. Preserve only a
            // server Retry-After for the captured account, under the mutation gate.
            if !set_and_broadcast(&app, &selection, snap) && backoff_until > now_ms() {
                crate::accounts::preserve_backoff(&cache, backoff_until);
            }
            let now = now_ms();
            let base = next_reset;
            let secs = if backoff_until > now {
                ((backoff_until - now) / 1000).clamp(1, base.min(30))
            } else { base };
            for _ in 0..secs {
                if REFRESH.swap(false, std::sync::atomic::Ordering::Relaxed)
                    || crate::accounts::selection_key("claude") != selection { break; }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reset_deadline_interrupts_the_normal_refresh_period() {
        let window = LimitWindow { resets_at: Some(109_500), ..Default::default() };
        assert_eq!(refresh_delay_secs(&[window.clone()], 100_000, 60), 9);
        assert_eq!(refresh_delay_secs(&[window.clone()], 109_499, 60), 1);
        assert_eq!(refresh_delay_secs(&[window], 110_000, 60), 60);
        assert_eq!(refresh_delay_secs(&[], 100_000, 60), 60);
    }

    #[test]
    fn credentials_and_identity_stay_inside_the_requested_profile() {
        let root = std::env::temp_dir().join(format!("codenotch-claude-accounts-{}-{}", std::process::id(), now_ms()));
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join(".credentials.json"), r#"{"claudeAiOauth":{"accessToken":"fixture-a","subscriptionType":"pro"}}"#).unwrap();
        std::fs::write(b.join("credentials.json"), r#"{"claudeAiOauth":{"accessToken":"fixture-b","subscriptionType":"max"}}"#).unwrap();
        std::fs::write(b.join(".claude.json"), r#"{"oauthAccount":{"emailAddress":"work@example.test"}}"#).unwrap();
        assert_eq!(read_credentials(&a).unwrap().plan.as_deref(), Some("pro"));
        assert_eq!(read_credentials(&b).unwrap().plan.as_deref(), Some("max"));
        assert_eq!(profile_identity(&b).as_deref(), Some("work@example.test"));
        assert!(read_credentials(&root.join("missing")).is_none());
        assert!(profile_identity(&a).is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn renewal_targets_the_profile_and_does_not_accept_inherited_api_auth() {
        let root = Path::new(r"C:\profiles with spaces\work");
        let cmd = renewal_command(Path::new("claude.exe"), root);
        assert_eq!(cmd.get_current_dir(), Some(root));
        assert!(cmd.get_envs().any(|(k, v)| k == "CLAUDE_CONFIG_DIR" && v == Some(root.as_os_str())));
        assert!(cmd.get_envs().any(|(k, v)| k == "ANTHROPIC_API_KEY" && v.is_none()));
        assert!(cmd.get_envs().any(|(k, v)| k == "ANTHROPIC_AUTH_TOKEN" && v.is_none()));
        assert!(cmd.get_envs().any(|(k, v)| k == "CLAUDE_CODE_OAUTH_TOKEN" && v.is_none()));
        // Secrets are passed neither as CLI arguments nor as Codenotch configuration.
        assert!(cmd.get_args().all(|a| !a.to_string_lossy().contains("profiles with spaces")));
    }

    const EXP: u64 = 1_000_000_000;

    #[test]
    fn renews_only_inside_the_margin() {
        assert!(!should_renew(None, EXP, None, None, 0), "never launch on a guess");
        assert!(!should_renew(Some(EXP), EXP - RENEW_MARGIN_MS - 1, None, None, 0), "plenty of time left");
        assert!(should_renew(Some(EXP), EXP - RENEW_MARGIN_MS, None, None, 0));
        assert!(should_renew(Some(EXP), EXP + 3_600_000, None, None, 0), "already expired still renews");
    }

    #[test]
    fn a_new_token_waits_out_the_cooldown() {
        let now = EXP + 1;
        assert!(!should_renew(Some(EXP + 5), now, Some(EXP), Some(now - 1000), 1), "cooldown holds a new token back");
        assert!(should_renew(Some(EXP + 5), now, Some(EXP), Some(now - RENEW_COOLDOWN_MS), 1));
    }

    #[test]
    fn a_failed_token_is_retried_on_a_doubling_wait() {
        let now = EXP + 1;
        assert!(!should_renew(Some(EXP), now, Some(EXP), Some(now - RENEW_COOLDOWN_MS), 1), "no retry at the plain cooldown");
        assert!(should_renew(Some(EXP), now, Some(EXP), Some(now - 2 * RENEW_COOLDOWN_MS), 1), "retried after twice the cooldown");
        assert!(!should_renew(Some(EXP), now, Some(EXP), Some(now - 2 * RENEW_COOLDOWN_MS), 2), "the wait doubles");
        assert!(should_renew(Some(EXP), now, Some(EXP), Some(now - 4 * RENEW_COOLDOWN_MS), 2));
        assert!(!should_renew(Some(EXP), now, Some(EXP), Some(now - RENEW_RETRY_CAP_MS + 1), 30), "never a tight loop");
        assert!(should_renew(Some(EXP), now, Some(EXP), Some(now - RENEW_RETRY_CAP_MS), 30), "but never more than an hour apart");
    }

    #[test]
    fn retry_after_never_exceeds_the_cap() {
        assert_eq!(backoff_secs(0, 3600), 3600);
        assert_eq!(backoff_secs(0, 0), BACKOFF_BASE_SECS);
        assert_eq!(backoff_secs(1, 300), 300);
        assert_eq!(backoff_secs(9, 0), BACKOFF_CAP_SECS);
    }

    #[test]
    fn desktop_bundled_cli_is_refused() {
        use std::path::Path;
        assert!(is_desktop_owned(Path::new(r"C:\Users\u\AppData\Local\AnthropicClaude\app-1.2.3\claude.exe")));
        assert!(is_desktop_owned(Path::new(r"C:\Users\u\AppData\Roaming\Claude\claude-code\2.1.0\claude.exe")));
        assert!(!is_desktop_owned(Path::new(r"C:\Users\u\.local\bin\claude.exe")));
        assert!(!is_desktop_owned(Path::new(r"C:\Users\u\AppData\Roaming\npm\claude.cmd")));
    }

    #[test]
    #[ignore = "Runs the installed standalone claude CLI; opt in for integration verification"]
    fn live_renewal_runs_the_standalone_cli() {
        let cli = find_cli().expect("a standalone claude CLI");
        assert!(!is_desktop_owned(&cli));
        let p = profiles().into_iter().next().expect("a profile");
        let before = read_credentials(&p.dir).and_then(|c| c.expires_at);
        let t = std::time::Instant::now();
        run_renewal(&cli, &p.dir).expect("spawned");
        assert!(t.elapsed() < Duration::from_secs(RENEW_TIMEOUT_SECS), "returned before the timeout");
        let after = read_credentials(&p.dir).and_then(|c| c.expires_at);
        assert!(after >= before, "the expiry never moves backwards");
        eprintln!("cli: {}", cli.display());
    }

    fn prof(slug: Option<&str>) -> Profile {
        Profile {
            dir: PathBuf::from(match slug {
                Some(s) => format!("/home/u/.claude-{s}"),
                None => "/home/u/.claude".to_string(),
            }),
            slug: slug.map(String::from),
        }
    }

    fn win(id: &str) -> LimitWindow {
        LimitWindow { id: id.into(), label: "Current session".into(), used: 0.5, ..Default::default() }
    }

    #[test]
    fn one_account_reads_exactly_as_before() {
        let w = decorate(vec![win("session")], &prof(None), None);
        assert_eq!(w[0].id, "session", "the only account keeps its ids");
        assert_eq!(w[0].group, None, "and stays ungrouped, so its card is the card that shipped");
    }

    #[test]
    fn a_second_account_is_suffixed_and_grouped() {
        let w = decorate(vec![win("session")], &prof(Some("work")), Some("work · pro"));
        assert_eq!(w[0].id, "session@work", "so by_id(\"session\") still means the default account");
        assert_eq!(w[0].group.as_deref(), Some("work · pro"));
    }

    #[test]
    fn the_group_pairs_the_name_with_the_plan() {
        assert_eq!(prof(None).group(Some("max")), "default · max");
        assert_eq!(prof(Some("work")).group(None), "work");
        assert_eq!(prof(Some("work")).group(Some("")), "work", "an empty plan adds no separator");
    }

    #[test]
    fn persisted_windows_go_back_to_the_account_that_made_them() {
        let order = vec![prof(None), prof(Some("work"))];
        let snap = UsageSnapshot {
            windows: vec![win("session"), win("session@work"), win("weekly@gone")],
            ..Default::default()
        };
        let split = split_persisted(&snap, &order);
        assert_eq!(split[&key(&order[0])].len(), 1);
        assert_eq!(split[&key(&order[1])][0].id, "session@work");
        assert_eq!(split.len(), 2, "windows from an account that is gone are dropped");
    }

    #[test]
    fn status_is_the_best_news_any_account_has() {
        let order = vec![prof(None), prof(Some("work"))];
        let mut accounts: HashMap<String, Account> = HashMap::new();
        accounts.insert(
            key(&order[0]),
            Account { status: "ok".into(), windows: vec![win("session")], fetched_at: 10, ..Default::default() },
        );
        accounts.insert(
            key(&order[1]),
            Account {
                status: "needsAuth".into(),
                note: "No Claude Code credential found".into(),
                ..Default::default()
            },
        );
        let snap = aggregate(&order, &accounts);
        assert_eq!(snap.status, "ok", "a signed-out second account must not dim the first");
        assert_eq!(snap.windows.len(), 1);
        assert_eq!(snap.fetched_at, 10);
        assert!(snap.note.starts_with("work: "), "the note names the account: {}", snap.note);
    }

    #[test]
    fn the_soonest_back_off_is_the_one_waited_out() {
        let order = vec![prof(None), prof(Some("work"))];
        let mut accounts: HashMap<String, Account> = HashMap::new();
        accounts.insert(key(&order[0]), Account { backoff_until: 900, ..Default::default() });
        accounts.insert(key(&order[1]), Account { backoff_until: 300, ..Default::default() });
        assert_eq!(aggregate(&order, &accounts).backoff_until, 300);
    }

    #[test]
    fn the_default_account_is_first_and_always_listed() {
        // Discovery reads the real home, so this asserts only what holds on any machine
        let list = discovered_profiles();
        assert!(!list.is_empty(), "the default account is listed even with no credential");
        assert_eq!(list[0].slug, None, "and comes first, so it owns the notch");
        let slugs: Vec<Option<String>> = list.iter().skip(1).map(|p| p.slug.clone()).collect();
        let mut sorted = slugs.clone();
        sorted.sort();
        assert_eq!(slugs, sorted, "secondary accounts are listed in name order");
        assert!(list.iter().skip(1).all(|p| p.slug.is_some()), "only the default account has no slug");
    }

    #[test]
    fn expired_is_judged_against_now() {
        let c = Credential { token: "t".into(), expires_at: Some(EXP), ..Default::default() };
        assert!(c.expired(EXP));
        assert!(!c.expired(EXP - 1));
        assert!(!Credential { token: "t".into(), expires_at: None, ..Default::default() }.expired(EXP));
    }
}
