//! Native account profiles. The registry contains labels and identifiers only;
//! browser tokens never cross IPC. Official CLIs own their isolated auth files,
//! while imported desktop sessions and API keys use current-user Windows DPAPI.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::SystemTime,
};
use tauri::{AppHandle, Emitter};

pub const PROVIDERS: [&str; 8] = [
    "claude",
    "codex",
    "cursor",
    "antigravity",
    "grok",
    "copilot",
    "glm",
    "opencode",
];
static STORE: Mutex<RegistryCache> = Mutex::new(RegistryCache { entry: None });
// Separate from STORE: publication closures may read profiles/cache paths safely.
static SELECTION: Mutex<()> = Mutex::new(());
static NEXT_ID: AtomicU64 = AtomicU64::new(0);
const MAX_CREDENTIAL_BYTES: usize = 1024 * 1024;
const MAX_REGISTRY_BYTES: u64 = 1024 * 1024;
const MAX_ACCOUNTS_PER_PROVIDER: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    Local,
    Imported,
}
#[derive(Debug, Clone)]
pub struct AccountContext {
    pub id: String,
    pub provider: String,
    pub root: PathBuf,
    pub kind: AccountKind,
    pub credential_path: Option<PathBuf>,
    pub selection_key: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    id: String,
    provider: String,
    label: String,
    #[serde(default)]
    identity: Option<String>,
    source: AccountKind,
    #[serde(default)]
    source_key: Option<String>,
    created_at: u64,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Registry {
    #[serde(skip)]
    broken: bool,
    #[serde(default)]
    accounts: Vec<Record>,
    #[serde(default)]
    active: BTreeMap<String, String>,
    #[serde(default)]
    generation: BTreeMap<String, u64>,
}

/// Keep one small, nonsecret registry instead of parsing it on every account
/// guard. Metadata is still checked on every read: external edits, replacement,
/// deletion and unreadable settings must invalidate the account selection.
struct RegistryCache {
    entry: Option<CachedRegistry>,
}
struct CachedRegistry {
    path: PathBuf,
    stamp: RegistryStamp,
    registry: Registry,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum RegistryStamp {
    Missing,
    Unavailable,
    File {
        len: u64,
        modified: SystemTime,
        created: Option<SystemTime>,
        #[cfg(unix)]
        device: u64,
        #[cfg(unix)]
        inode: u64,
        #[cfg(unix)]
        changed: (i64, i64),
        #[cfg(windows)]
        identity: (u32, u64),
        #[cfg(windows)]
        changed: i64,
    },
}
impl RegistryStamp {
    fn read(path: &Path) -> Self {
        #[cfg(windows)]
        {
            return Self::read_windows(path);
        }
        #[cfg(not(windows))]
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_file() => {
                let Ok(modified) = meta.modified() else {
                    return Self::Unavailable;
                };
                #[cfg(unix)]
                use std::os::unix::fs::MetadataExt;
                Self::File {
                    len: meta.len(),
                    modified,
                    created: meta.created().ok(),
                    #[cfg(unix)]
                    device: meta.dev(),
                    #[cfg(unix)]
                    inode: meta.ino(),
                    #[cfg(unix)]
                    changed: (meta.ctime(), meta.ctime_nsec()),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::Missing,
            _ => Self::Unavailable,
        }
    }
    #[cfg(windows)]
    fn read_windows(path: &Path) -> Self {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{
                FileBasicInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
                BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO,
            },
        };
        // Read access is checked even on cache hits. A revoked ACL must fail
        // closed, and the native file ID detects replacement with copied times.
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::Missing,
            Err(_) => return Self::Unavailable,
        };
        let Ok(meta) = file.metadata() else {
            return Self::Unavailable;
        };
        let Ok(modified) = meta.modified() else {
            return Self::Unavailable;
        };
        if !meta.is_file() {
            return Self::Unavailable;
        }
        let handle = HANDLE(file.as_raw_handle());
        let mut identity = BY_HANDLE_FILE_INFORMATION::default();
        let mut basic = FILE_BASIC_INFO::default();
        unsafe {
            if GetFileInformationByHandle(handle, &mut identity).is_err()
                || GetFileInformationByHandleEx(
                    handle,
                    FileBasicInfo,
                    (&mut basic as *mut FILE_BASIC_INFO).cast(),
                    std::mem::size_of::<FILE_BASIC_INFO>() as u32,
                )
                .is_err()
            {
                return Self::Unavailable;
            }
        }
        Self::File {
            len: meta.len(),
            modified,
            created: meta.created().ok(),
            identity: (
                identity.dwVolumeSerialNumber,
                (identity.nFileIndexHigh as u64) << 32 | identity.nFileIndexLow as u64,
            ),
            changed: basic.ChangeTime,
        }
    }
}
impl RegistryCache {
    fn read(&mut self, path: &Path) -> &Registry {
        let stamp = RegistryStamp::read(path);
        if !self.entry.as_ref().is_some_and(|entry| {
            entry.path == path && entry.stamp == stamp && !entry.registry.broken
        }) {
            // If a file changes while being read, retry once. A moving or
            // unreadable registry fails closed rather than borrowing a login.
            let mut before = stamp;
            let mut registry = unavailable_registry();
            for _ in 0..2 {
                registry = read_registry_file(path, &before);
                let after = RegistryStamp::read(path);
                if before == after {
                    break;
                }
                before = after;
                registry = unavailable_registry();
            }
            self.entry = Some(CachedRegistry {
                path: path.to_owned(),
                stamp: before,
                registry,
            });
        }
        &self.entry.as_ref().unwrap().registry
    }
    fn invalidate(&mut self) {
        // Reload after a successful save rather than assuming another process
        // could not have changed the file between our rename and metadata read.
        self.entry = None;
    }
}
#[derive(Clone, Serialize)]
pub struct AccountView {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub identity: Option<String>,
    pub status: String,
    pub source: AccountKind,
    pub active: bool,
    pub created_at: u64,
    pub verified_at: Option<u64>,
}
#[derive(Serialize)]
pub struct ProviderAccounts {
    pub id: String,
    pub label: String,
    pub login_mode: String,
    pub login_available: bool,
    pub login_note: String,
    pub manual_key_available: bool,
    pub accounts: Vec<AccountView>,
    pub active_account_id: Option<String>,
}
#[derive(Serialize)]
pub struct AccountsSnapshot {
    pub providers: Vec<ProviderAccounts>,
}

pub fn canonical_provider(provider: &str) -> &str {
    if provider == "gemini" {
        "antigravity"
    } else {
        provider
    }
}
fn ui_provider(provider: &str) -> &str {
    if provider == "antigravity" {
        "gemini"
    } else {
        provider
    }
}
fn checked_provider(provider: &str) -> Result<(), String> {
    if PROVIDERS.contains(&provider) {
        Ok(())
    } else {
        Err("Unknown provider.".into())
    }
}
fn valid_id(id: &str) -> bool {
    id.len() >= 2
        && id.len() <= 64
        && id.starts_with('p')
        && id[1..].bytes().all(|b| b.is_ascii_hexdigit())
}
fn base() -> PathBuf {
    crate::config::config_path()
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf()
}
fn registry_path() -> PathBuf {
    base().join("accounts.json")
}
fn profile_root(provider: &str, id: &str) -> PathBuf {
    base().join("profiles").join(provider).join(id)
}
fn sanitize(mut registry: Registry) -> Registry {
    let mut seen = std::collections::HashSet::new();
    let mut counts = BTreeMap::<String, usize>::new();
    registry.accounts.retain(|r| {
        if !PROVIDERS.contains(&r.provider.as_str())
            || !valid_id(&r.id)
            || !seen.insert(r.id.clone())
        {
            return false;
        }
        let count = counts.entry(r.provider.clone()).or_default();
        *count += 1;
        *count <= MAX_ACCOUNTS_PER_PROVIDER
    });
    registry.active.retain(|p, id| {
        registry
            .accounts
            .iter()
            .any(|r| &r.provider == p && &r.id == id)
    });
    registry
        .generation
        .retain(|p, _| PROVIDERS.contains(&p.as_str()));
    registry
}
fn unavailable_registry() -> Registry {
    Registry {
        broken: true,
        ..Default::default()
    }
}
fn read_registry_file(path: &Path, stamp: &RegistryStamp) -> Registry {
    match stamp {
        RegistryStamp::Missing => return Registry::default(),
        RegistryStamp::File { len, .. } if *len <= MAX_REGISTRY_BYTES => {}
        _ => return unavailable_registry(),
    }
    let Ok(file) = std::fs::File::open(path) else {
        return unavailable_registry();
    };
    let mut bytes = Vec::new();
    if file
        .take(MAX_REGISTRY_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_REGISTRY_BYTES
    {
        return unavailable_registry();
    }
    serde_json::from_slice(&bytes)
        .map(sanitize)
        .unwrap_or_else(|_| unavailable_registry())
}
fn read_registry(store: &mut RegistryCache) -> &Registry {
    store.read(&registry_path())
}
fn write_registry(store: &mut RegistryCache, registry: &Registry) -> Result<(), String> {
    if registry.broken {
        return Err("Saved account settings could not be read. Existing profiles were preserved; restore accounts.json before changing accounts.".into());
    }
    private_dir(&base())?;
    let bytes =
        serde_json::to_vec_pretty(registry).map_err(|_| "Unable to save account settings.")?;
    write_atomic(&registry_path(), &bytes)?;
    store.invalidate();
    Ok(())
}
fn new_id() -> String {
    format!(
        "p{:x}{:x}",
        crate::now_ms(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    )
}
fn label(value: &str, provider: &str) -> String {
    let clean: String = value
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(80)
        .collect();
    if clean.is_empty() {
        crate::provider_label(provider).into()
    } else {
        clean
    }
}
fn context(record: &Record) -> AccountContext {
    let root = profile_root(&record.provider, &record.id);
    let name = match record.provider.as_str() {
        "claude" => ".credentials.json",
        "codex" | "grok" => "auth.json",
        "copilot" => "hosts.yml",
        "cursor" => "User/globalStorage/state.vscdb",
        "opencode" => "data/opencode/auth.json",
        _ => "credential.dpapi",
    };
    let credential = root.join(name);
    AccountContext {
        id: record.id.clone(),
        provider: record.provider.clone(),
        root,
        kind: record.source.clone(),
        credential_path: Some(credential),
        selection_key: String::new(),
    }
}
pub fn profile(provider: &str, account_id: &str) -> Option<AccountContext> {
    let provider = canonical_provider(provider);
    checked_provider(provider).ok()?;
    let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
    let registry = read_registry(&mut store);
    registry
        .accounts
        .iter()
        .find(|r| r.provider == provider && r.id == account_id)
        .map(|r| {
            let mut ctx = context(r);
            ctx.selection_key = key_for(&registry, provider);
            ctx
        })
}
/// Serialize profile existence checks with login's busy registration/spawn.
/// The callback runs without STORE held; native CLI launch does not wait for
/// authentication here. Inactive profiles can be signed in safely too.
pub fn with_profile<T>(
    provider: &str,
    account_id: &str,
    f: impl FnOnce(AccountContext) -> Result<T, String>,
) -> Result<T, String> {
    let provider = canonical_provider(provider);
    checked_provider(provider)?;
    let _gate = SELECTION.lock().unwrap_or_else(|e| e.into_inner());
    let ctx =
        profile(provider, account_id).ok_or("Account profile not found for this provider.")?;
    f(ctx)
}
pub fn active(provider: &str) -> Option<AccountContext> {
    let provider = canonical_provider(provider);
    checked_provider(provider).ok()?;
    let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
    let registry = read_registry(&mut store);
    if registry.broken {
        // An unreadable selected-profile registry must never fall back to a
        // different external app's login.
        let mut ctx = context(&Record {
            id: "pblocked".into(),
            provider: provider.into(),
            label: "Account settings unavailable".into(),
            identity: None,
            source: AccountKind::Local,
            source_key: None,
            created_at: 0,
        });
        ctx.selection_key = "registry-unavailable".into();
        return Some(ctx);
    }
    let id = registry.active.get(provider)?;
    registry
        .accounts
        .iter()
        .find(|r| r.provider == provider && &r.id == id)
        .map(|r| {
            let mut ctx = context(r);
            ctx.selection_key = key_for(&registry, provider);
            ctx
        })
}
fn key_for(registry: &Registry, provider: &str) -> String {
    if registry.broken {
        return "registry-unavailable".into();
    }
    format!(
        "{}:{}",
        registry
            .active
            .get(provider)
            .map(String::as_str)
            .unwrap_or("external"),
        registry.generation.get(provider).unwrap_or(&0)
    )
}
pub fn selection_key(provider: &str) -> String {
    let provider = canonical_provider(provider);
    let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
    let registry = read_registry(&mut store);
    key_for(&registry, provider)
}
pub fn with_selection<T>(provider: &str, expected: &str, f: impl FnOnce() -> T) -> Option<T> {
    let _guard = SELECTION.lock().unwrap_or_else(|e| e.into_inner());
    (selection_key(provider) == expected).then(f)
}
/// A rejected in-flight request can still carry a real server Retry-After.
/// Keep only that deadline, never its old account windows or verification proof.
pub fn preserve_backoff(cache: &Path, deadline: u64) {
    if deadline == 0 {
        return;
    }
    let _gate = SELECTION.lock().unwrap_or_else(|e| e.into_inner());
    if !cache.parent().is_some_and(|p| p.is_dir()) {
        return;
    }
    let mut snapshot = std::fs::read_to_string(cache)
        .ok()
        .and_then(|s| serde_json::from_str::<crate::usage::UsageSnapshot>(&s).ok())
        .unwrap_or_default();
    if deadline <= snapshot.backoff_until {
        return;
    }
    snapshot.backoff_until = deadline;
    if snapshot.windows.is_empty() {
        snapshot.status = "backoff".into();
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(&snapshot) {
        let _ = write_atomic(cache, &bytes);
    }
}
pub fn cache_path(provider: &str, legacy_filename: &str) -> PathBuf {
    let provider = canonical_provider(provider);
    if let Some(profile) = active(provider) {
        let cache = profile.root.join("cache");
        let _ = private_dir(&cache);
        cache.join(Path::new(legacy_filename).file_name().unwrap_or_default())
    } else {
        let variable = match provider {
            "codex" => Some("CODEX_HOME"),
            "claude" => Some("CLAUDE_CONFIG_DIR"),
            _ => None,
        };
        let custom = variable
            .and_then(std::env::var_os)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        if let Some(root) =
            custom.filter(|p| dirs::home_dir().is_none_or(|h| p != &h.join(format!(".{provider}"))))
        {
            let cache = base()
                .join("external-cache")
                .join(provider)
                .join(fingerprint(&root.to_string_lossy()));
            let _ = private_dir(&cache);
            cache.join(Path::new(legacy_filename).file_name().unwrap_or_default())
        } else {
            crate::config::config_path().with_file_name(legacy_filename)
        }
    }
}
fn bump(registry: &mut Registry, provider: &str) {
    let n = registry.generation.entry(provider.into()).or_default();
    *n = n.saturating_add(1);
}
fn changed(app: &AppHandle, provider: &str) {
    crate::accounts_changed(app, provider);
    let _ = app.emit("accounts-updated", provider);
}
fn native_credential(root: &Path, provider: &str) -> Option<Vec<u8>> {
    let names: &[&str] = match provider {
        "codex" | "grok" => &["auth.json"],
        "claude" => &[".credentials.json", "credentials.json"],
        _ => &[],
    };
    for name in names {
        let path = root.join(name);
        if std::fs::metadata(&path)
            .ok()
            .is_some_and(|m| m.len() <= MAX_CREDENTIAL_BYTES as u64)
        {
            if let Ok(bytes) = std::fs::read(path) {
                return Some(bytes);
            }
        }
    }
    None
}
fn field<'a>(v: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        v.get(key)
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
    })
}
// Local-only JWT claims are display hints, never proof that the vendor accepts the token.
fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    if payload.len() > 64 * 1024 {
        return None;
    }
    let mut out = Vec::new();
    let mut bits = 0u32;
    let mut count = 0;
    for b in payload.bytes() {
        let digit = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        bits = (bits << 6) | digit as u32;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    serde_json::from_slice(&out).ok()
}
fn identity_from(v: &serde_json::Value) -> Option<String> {
    if let Some(s) = field(
        v,
        &[
            "email",
            "username",
            "user",
            "accountEmail",
            "account_id",
            "accountId",
        ],
    ) {
        return Some(s.chars().take(160).collect());
    }
    for key in [
        "tokens",
        "claudeAiOauth",
        "token",
        "oauthAccount",
        "user",
        "profile",
    ] {
        if let Some(child) = v.get(key) {
            if let Some(id) = identity_from(child) {
                return Some(id);
            }
        }
    }
    for key in [
        "id_token",
        "access_token",
        "accessToken",
        "access",
        "key",
        "token",
    ] {
        if let Some(token) = v.get(key).and_then(|x| x.as_str()) {
            if let Some(claims) = jwt_claims(token) {
                if let Some(id) = field(&claims, &["email", "preferred_username", "sub"]) {
                    return Some(id.chars().take(160).collect());
                }
            }
        }
    }
    // Grok auth is an issuer-keyed object; do not recurse arbitrary strings.
    v.as_object()?
        .values()
        .filter(|x| x.is_object())
        .find_map(identity_from)
}
fn expiry_from(v: &serde_json::Value) -> Option<u64> {
    for key in ["expiresAt", "expires_at", "expires", "expiry"] {
        if let Some(value) = v.get(key) {
            if let Some(n) = value.as_u64() {
                return Some(if n < 100_000_000_000 { n * 1000 } else { n });
            }
            if let Some(s) = value.as_str() {
                if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
                    return Some(d.timestamp_millis().max(0) as u64);
                }
            }
        }
    }
    for key in ["tokens", "claudeAiOauth", "token"] {
        if let Some(v) = v.get(key) {
            if let Some(x) = expiry_from(v) {
                return Some(x);
            }
        }
    }
    None
}
fn cache_filename(provider: &str) -> &str {
    match provider {
        "claude" => "usage.json",
        "glm" => "glm-usage.json",
        "opencode" => "opencode-usage.json",
        "codex" => "codex.json",
        "cursor" => "cursor.json",
        "grok" => "grok.json",
        "copilot" => "copilot.json",
        _ => "antigravity.json",
    }
}
fn view(record: &Record, registry: &Registry) -> AccountView {
    let ctx = context(record);
    let native = native_credential(&ctx.root, &record.provider);
    let saved = load_secret_at(&ctx.root);
    let plaintext = native
        .as_deref()
        .and_then(|v| serde_json::from_slice::<serde_json::Value>(v).ok())
        .or_else(|| {
            saved
                .as_deref()
                .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
        });
    let native_valid = match record.provider.as_str() {
        "codex" => crate::codex::profile_has_credential(&ctx.root),
        "claude" => crate::usage::profile_has_credential(&ctx.root),
        "grok" => plaintext
            .as_ref()
            .is_some_and(|v| crate::grok::pick(v).is_some()),
        _ => false,
    };
    let mut status = if native_valid || saved.is_some() {
        "saved"
    } else {
        "needsAuth"
    };
    if plaintext
        .as_ref()
        .and_then(expiry_from)
        .is_some_and(|expiry| expiry <= crate::now_ms())
    {
        status = "expired";
    }
    let snap = std::fs::read_to_string(
        ctx.root
            .join("cache")
            .join(cache_filename(&record.provider)),
    )
    .ok()
    .and_then(|s| serde_json::from_str::<crate::usage::UsageSnapshot>(&s).ok());
    let verified_at = snap
        .filter(|s| {
            status == "saved"
                && s.status == "ok"
                && s.fetched_at > 0
                && s.fetched_at <= crate::now_ms().saturating_add(60_000)
                && crate::now_ms().saturating_sub(s.fetched_at) < 10 * 60 * 1000
        })
        .map(|s| s.fetched_at);
    if verified_at.is_some() {
        status = "connected";
    }
    AccountView {
        id: record.id.clone(),
        provider: ui_provider(&record.provider).into(),
        label: record.label.clone(),
        identity: plaintext
            .as_ref()
            .and_then(identity_from)
            .or_else(|| record.identity.clone()),
        status: status.into(),
        source: record.source.clone(),
        active: registry.active.get(&record.provider) == Some(&record.id),
        created_at: record.created_at,
        verified_at,
    }
}
fn snapshot(registry: &Registry) -> AccountsSnapshot {
    AccountsSnapshot { providers: PROVIDERS.iter().map(|p| {
        let (mode, available, note) = crate::profile_auth::capability(p);
        ProviderAccounts { id: ui_provider(p).into(), label: crate::provider_label(p).into(), login_mode: mode.into(), login_available: available && !registry.broken, login_note: if registry.broken { "Saved account settings could not be read. Profiles are preserved and external login fallback is disabled.".into() } else { note.into() }, manual_key_available: matches!(*p,"glm"|"copilot"|"opencode") && !registry.broken, accounts: registry.accounts.iter().filter(|r| &r.provider == p).map(|r| view(r, registry)).collect(), active_account_id: registry.active.get(*p).cloned() }
    }).collect() }
}
#[tauri::command]
pub fn list_provider_accounts() -> AccountsSnapshot {
    let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
    snapshot(read_registry(&mut store))
}
#[tauri::command]
pub fn add_provider_account(
    app: AppHandle,
    provider: String,
    label: String,
) -> Result<AccountView, String> {
    let provider = canonical_provider(&provider).to_string();
    checked_provider(&provider)?;
    let _gate = SELECTION.lock().unwrap_or_else(|e| e.into_inner());
    let record = Record {
        id: new_id(),
        provider: provider.clone(),
        label: self::label(&label, &provider),
        identity: None,
        source: AccountKind::Local,
        source_key: None,
        created_at: crate::now_ms(),
    };
    private_dir(&context(&record).root)?;
    let result = {
        let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let mut registry = read_registry(&mut store).clone();
        if registry
            .accounts
            .iter()
            .filter(|r| r.provider == provider)
            .count()
            >= MAX_ACCOUNTS_PER_PROVIDER
        {
            return Err("Maximum of 32 profiles per provider reached.".into());
        }
        registry.active.insert(provider.clone(), record.id.clone());
        registry.accounts.push(record.clone());
        bump(&mut registry, &provider);
        write_registry(&mut store, &registry)?;
        view(&record, &registry)
    };
    changed(&app, &provider);
    Ok(result)
}
#[tauri::command]
pub fn select_provider_account(
    app: AppHandle,
    provider: String,
    account_id: String,
) -> Result<AccountsSnapshot, String> {
    let provider = canonical_provider(&provider).to_string();
    checked_provider(&provider)?;
    let _gate = SELECTION.lock().unwrap_or_else(|e| e.into_inner());
    {
        let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let mut registry = read_registry(&mut store).clone();
        if !registry
            .accounts
            .iter()
            .any(|r| r.provider == provider && r.id == account_id)
        {
            return Err("Account profile not found for this provider.".into());
        }
        registry.active.insert(provider.clone(), account_id);
        bump(&mut registry, &provider);
        write_registry(&mut store, &registry)?;
    }
    changed(&app, &provider);
    Ok(list_provider_accounts())
}
#[tauri::command]
pub fn remove_provider_account(
    app: AppHandle,
    provider: String,
    account_id: String,
) -> Result<AccountsSnapshot, String> {
    let provider = canonical_provider(&provider).to_string();
    checked_provider(&provider)?;
    let _gate = SELECTION.lock().unwrap_or_else(|e| e.into_inner());
    if crate::profile_auth::is_busy(&provider, &account_id) {
        return Err("Close or complete this account's sign-in window before removing it.".into());
    }
    let root = {
        let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let mut registry = read_registry(&mut store).clone();
        let index = registry
            .accounts
            .iter()
            .position(|r| r.provider == provider && r.id == account_id)
            .ok_or("Account profile not found for this provider.")?;
        let root = context(&registry.accounts.remove(index)).root;
        if registry.active.get(&provider) == Some(&account_id) {
            registry.active.remove(&provider);
        }
        bump(&mut registry, &provider);
        write_registry(&mut store, &registry)?;
        root
    };
    // root is constructed from validated generated identifiers, never an IPC path.
    let deletion = std::fs::remove_dir_all(root);
    changed(&app, &provider);
    if deletion.is_err() {
        return Err("Profile removed, but its files could not be deleted. Close its sign-in window and delete the unused profile directory.".into());
    }
    Ok(list_provider_accounts())
}
fn fingerprint(text: &str) -> String {
    // Stable identifier, not encryption; this value never authenticates anything.
    let mut hash = 0xcbf29ce484222325u64;
    for b in text.bytes() {
        hash = (hash ^ b as u64).wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}
fn external_roots(provider: &str) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let env = match provider {
        "claude" => "CLAUDE_CONFIG_DIR",
        "codex" => "CODEX_HOME",
        _ => return roots,
    };
    if let Some(path) = std::env::var_os(env).filter(|s| !s.is_empty()) {
        roots.push(PathBuf::from(path));
    }
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(format!(".{provider}")));
        if let Ok(entries) = std::fs::read_dir(home) {
            let prefix = format!(".{provider}-");
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(&prefix) && entry.path().is_dir()
                {
                    roots.push(entry.path());
                }
            }
        }
    }
    roots.sort();
    roots.dedup();
    roots
}
fn insert_import(
    registry: &mut Registry,
    provider: &str,
    source: &str,
    raw: &str,
    native_name: Option<&str>,
) -> Result<bool, String> {
    let value = serde_json::from_str::<serde_json::Value>(raw).ok();
    let identity = value.as_ref().and_then(identity_from);
    let stable = identity.clone().unwrap_or_else(|| fingerprint(raw));
    let source_key = fingerprint(&format!("{provider}:{source}:{stable}"));
    let found = registry
        .accounts
        .iter()
        .find(|r| r.provider == provider && r.source_key.as_deref() == Some(&source_key))
        .cloned();
    let empty_active = registry
        .active
        .get(provider)
        .and_then(|id| {
            registry
                .accounts
                .iter()
                .find(|r| &r.id == id && r.provider == provider)
        })
        .filter(|r| {
            let ctx = context(r);
            native_credential(&ctx.root, provider).is_none()
                && !ctx.root.join("credential.dpapi").is_file()
        })
        .cloned();
    let mut record = found.or(empty_active).unwrap_or_else(|| Record {
        id: new_id(),
        provider: provider.into(),
        label: identity
            .clone()
            .unwrap_or_else(|| format!("{} · imported", crate::provider_label(provider))),
        identity,
        source: AccountKind::Imported,
        source_key: Some(source_key.clone()),
        created_at: crate::now_ms(),
    });
    record.identity = identity_from(&value.clone().unwrap_or_default());
    record.source = AccountKind::Imported;
    record.source_key = Some(source_key);
    let ctx = context(&record);
    private_dir(&ctx.root)?;
    let old = native_name
        .and_then(|name| std::fs::read_to_string(ctx.root.join(name)).ok())
        .or_else(|| load_secret_at(&ctx.root));
    if old.as_deref() != Some(raw) {
        invalidate_cache(&ctx.root, provider)?;
    }
    if let Some(name) = native_name {
        write_atomic(&ctx.root.join(name), raw.as_bytes())?;
    } else {
        save_secret_at(&ctx.root, raw)?;
    }
    if let Some(existing) = registry.accounts.iter_mut().find(|r| r.id == record.id) {
        *existing = record.clone();
    } else {
        registry.accounts.push(record.clone());
    }
    if !registry.active.contains_key(provider) {
        registry.active.insert(provider.into(), record.id.clone());
    }
    bump(registry, provider);
    Ok(true)
}
#[tauri::command]
pub fn detect_provider_accounts(
    app: AppHandle,
    provider: Option<String>,
) -> Result<AccountsSnapshot, String> {
    let provider = provider.map(|p| canonical_provider(&p).to_string());
    if let Some(p) = provider.as_deref() {
        checked_provider(p)?;
    }
    // Capture native app credentials before holding STORE; adapters may read profile metadata.
    let wanted: Vec<&str> = provider
        .as_deref()
        .map(|p| vec![p])
        .unwrap_or_else(|| PROVIDERS.to_vec());
    let mut captures: Vec<(String, String, String, Option<String>)> = Vec::new();
    for p in &wanted {
        if matches!(*p, "codex" | "claude") {
            for root in external_roots(p) {
                // Never import Codenotch's own isolated profile into itself.
                if root.starts_with(base().join("profiles")) {
                    continue;
                }
                if let Some(bytes) = native_credential(&root, p) {
                    if let Ok(raw) = String::from_utf8(bytes) {
                        let valid = if *p == "codex" {
                            crate::codex::profile_has_credential(&root)
                        } else {
                            crate::usage::profile_has_credential(&root)
                        };
                        if valid {
                            captures.push((
                                p.to_string(),
                                root.to_string_lossy().into(),
                                raw,
                                Some(
                                    if *p == "claude" {
                                        ".credentials.json"
                                    } else {
                                        "auth.json"
                                    }
                                    .into(),
                                ),
                            ));
                        }
                    }
                }
            }
        } else {
            let raw = match *p {
                "cursor" => crate::cursor::capture_current_credential(),
                "antigravity" => crate::antigravity::capture_current_credential(),
                "grok" => crate::grok::capture_current_credential(),
                "copilot" => crate::copilot::capture_current_credential(),
                "glm" => crate::glm::capture_current_credential(),
                "opencode" => crate::opencode::capture_current_credential(),
                _ => None,
            };
            if let Some(raw) = raw {
                captures.push((p.to_string(), "installed-app".into(), raw, None));
            }
        }
    }
    let _gate = SELECTION.lock().unwrap_or_else(|e| e.into_inner());
    let mut changed_providers = std::collections::BTreeSet::new();
    {
        let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let mut registry = read_registry(&mut store).clone();
        for (p, source, raw, name) in captures {
            if registry.accounts.iter().filter(|r| r.provider == p).count()
                >= MAX_ACCOUNTS_PER_PROVIDER
            {
                continue;
            }
            if insert_import(&mut registry, &p, &source, &raw, name.as_deref())? {
                changed_providers.insert(p);
            }
        }
        write_registry(&mut store, &registry)?;
    }
    for p in changed_providers {
        changed(&app, &p);
    }
    Ok(list_provider_accounts())
}
#[tauri::command]
pub fn connect_provider_account(
    app: AppHandle,
    provider: String,
    account_id: String,
    mut credential: String,
) -> Result<AccountsSnapshot, String> {
    let provider = canonical_provider(&provider).to_string();
    if !matches!(provider.as_str(), "glm" | "opencode" | "copilot") {
        credential.clear();
        return Err(
            "This provider needs its official sign-in or a detected desktop session.".into(),
        );
    }
    let _gate = SELECTION.lock().unwrap_or_else(|e| e.into_inner());
    let ctx = profile(&provider, &account_id).ok_or("Account profile not found.")?;
    let token = credential.trim();
    if token.is_empty() || token.len() > 32 * 1024 || token.chars().any(char::is_control) {
        return Err("Enter a valid API key or GitHub token.".into());
    }
    let raw = match provider.as_str() {
        "glm" => serde_json::json!({"token":token,"base":"https://api.z.ai"}).to_string(),
        "opencode" => serde_json::json!({"type":"api","key":token}).to_string(),
        _ => serde_json::json!({"token":token}).to_string(),
    };
    invalidate_cache(&ctx.root, &provider)?;
    {
        let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let mut registry = read_registry(&mut store).clone();
        bump(&mut registry, &provider);
        write_registry(&mut store, &registry)?;
    }
    save_secret_at(&ctx.root, &raw)?;
    // The caller gets no token echo, including on validation and native errors.
    credential.clear();
    changed(&app, &provider);
    Ok(list_provider_accounts())
}
pub fn save_profile_secret(
    app: &AppHandle,
    provider: &str,
    account_id: &str,
    raw: &str,
) -> Result<(), String> {
    let provider = canonical_provider(provider);
    let _gate = SELECTION.lock().unwrap_or_else(|e| e.into_inner());
    let ctx = profile(provider, account_id).ok_or("Account profile no longer exists.")?;
    invalidate_cache(&ctx.root, provider)?;
    {
        let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let mut registry = read_registry(&mut store).clone();
        bump(&mut registry, provider);
        write_registry(&mut store, &registry)?;
    }
    save_secret_at(&ctx.root, raw)?;
    if active(provider).is_some_and(|p| p.id == account_id) {
        changed(app, provider);
    } else {
        let _ = app.emit("accounts-updated", ui_provider(provider));
    }
    Ok(())
}
pub fn login_finished(app: &AppHandle, provider: &str, account_id: &str) {
    let provider = canonical_provider(provider);
    let _gate = SELECTION.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(ctx) = profile(provider, account_id) {
        if invalidate_cache(&ctx.root, provider).is_err() {
            let _ = std::fs::remove_file(ctx.root.join("cache").join(cache_filename(provider)));
        }
    }
    {
        let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let mut registry = read_registry(&mut store).clone();
        if !registry
            .accounts
            .iter()
            .any(|r| r.provider == provider && r.id == account_id)
        {
            return;
        }
        bump(&mut registry, provider);
        let _ = write_registry(&mut store, &registry);
    }
    if active(provider).is_some_and(|p| p.id == account_id) {
        changed(app, provider);
    } else {
        let _ = app.emit("accounts-updated", provider);
    }
}
pub fn secret(provider: &str) -> Option<String> {
    let ctx = active(provider)?;
    load_secret_at(&ctx.root)
}
fn save_secret_at(root: &Path, raw: &str) -> Result<(), String> {
    if raw.is_empty() || raw.len() > MAX_CREDENTIAL_BYTES {
        return Err("Invalid credential size.".into());
    }
    private_dir(root)?;
    let encrypted = protect(raw.as_bytes())?;
    write_atomic(&root.join("credential.dpapi"), &encrypted)
}
fn load_secret_at(root: &Path) -> Option<String> {
    let path = root.join("credential.dpapi");
    if std::fs::metadata(&path).ok()?.len() > MAX_CREDENTIAL_BYTES as u64 + 4096 {
        return None;
    }
    String::from_utf8(unprotect(&std::fs::read(path).ok()?).ok()?).ok()
}
fn private_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|_| "Unable to create account profile directory.")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| "Unable to protect account directory.")?;
    }
    Ok(())
}
fn invalidate_cache(root: &Path, provider: &str) -> Result<(), String> {
    let path = root
        .join("cache")
        .join(cache_filename(canonical_provider(provider)));
    if let Some(mut snapshot) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<crate::usage::UsageSnapshot>(&s).ok())
    {
        snapshot.windows.clear();
        snapshot.fetched_at = 0;
        snapshot.status = if snapshot.backoff_until > crate::now_ms() {
            "backoff"
        } else {
            "needsAuth"
        }
        .into();
        snapshot.note = "Account credential changed; waiting for fresh usage.".into();
        let bytes = serde_json::to_vec(&snapshot)
            .map_err(|_| "Unable to invalidate cached account usage.".to_string())?;
        write_atomic(&path, &bytes)?;
    }
    Ok(())
}
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temp = path.with_extension(format!("tmp-{}", new_id()));
    write_private(&temp, bytes)?;
    #[cfg(windows)]
    let result = {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            core::PCWSTR,
            Win32::Storage::FileSystem::{
                MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
            },
        };
        let from: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            MoveFileExW(
                PCWSTR(from.as_ptr()),
                PCWSTR(to.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|_| "Unable to replace protected account data.")
    };
    #[cfg(not(windows))]
    let result =
        std::fs::rename(&temp, path).map_err(|_| "Unable to replace protected account data.");
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result.map_err(String::from)
}
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        private_dir(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|_| "Unable to write protected account data.".to_string())?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| "Unable to save protected account data.".to_string())
}
#[cfg(windows)]
fn protect(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::{LocalFree, HLOCAL},
            Security::Cryptography::{
                CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
            },
        },
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &input,
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|_| "Windows could not protect this credential.")?;
        let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(output.pbData as *mut _));
        Ok(result)
    }
}
#[cfg(windows)]
fn unprotect(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use windows::Win32::{
        Foundation::{LocalFree, HLOCAL},
        Security::Cryptography::{
            CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        },
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|_| "This credential belongs to another Windows user or is damaged.")?;
        let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        std::ptr::write_bytes(output.pbData, 0, output.cbData as usize);
        let _ = LocalFree(HLOCAL(output.pbData as *mut _));
        Ok(result)
    }
}
#[cfg(not(windows))]
fn protect(_: &[u8]) -> Result<Vec<u8>, String> {
    Err("Secure credential import is available in the Windows build.".into())
}
#[cfg(not(windows))]
fn unprotect(_: &[u8]) -> Result<Vec<u8>, String> {
    Err("Windows protected credentials cannot be opened on this platform.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct RegistryFixture(PathBuf);
    impl RegistryFixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "codenotch-registry-cache-{}-{}",
                std::process::id(),
                new_id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> PathBuf {
            self.0.join("accounts.json")
        }
        fn save(&self, registry: &Registry) {
            write_atomic(&self.path(), &serde_json::to_vec(registry).unwrap()).unwrap();
        }
    }
    impl Drop for RegistryFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn record(id: &str, provider: &str) -> Record {
        Record {
            id: id.into(),
            provider: provider.into(),
            label: "work".into(),
            identity: None,
            source: AccountKind::Local,
            source_key: None,
            created_at: 1,
        }
    }
    #[test]
    fn existing_config_migrates_to_empty_registry_without_affecting_external_login() {
        let reg: Registry = serde_json::from_str("{}").unwrap();
        assert!(reg.accounts.is_empty());
        assert!(reg.active.is_empty());
    }
    #[test]
    fn registry_rejects_traversal_unknown_providers_duplicate_ids_and_cross_provider_selection() {
        let mut r = Registry::default();
        r.accounts = vec![
            record("p123", "codex"),
            record("p123", "claude"),
            record("../auth", "codex"),
            record("p456", "unknown"),
        ];
        r.active.insert("claude".into(), "p123".into());
        r.active.insert("codex".into(), "p123".into());
        let r = sanitize(r);
        assert_eq!(r.accounts.len(), 1);
        assert_eq!(r.active.len(), 1);
        assert!(r.active.contains_key("codex"));
    }
    #[test]
    fn generated_ids_and_provider_roots_are_isolated_and_stable() {
        let a = new_id();
        let b = new_id();
        assert!(valid_id(&a));
        assert_ne!(a, b);
        let c = context(&record(&a, "codex"));
        let d = context(&record(&b, "codex"));
        let e = context(&record(&a, "claude"));
        assert_ne!(c.root, d.root);
        assert_ne!(c.root, e.root);
        assert_eq!(context(&record(&a, "codex")).root, c.root);
    }
    #[test]
    fn credential_registry_roundtrip_never_contains_auth_data() {
        let mut r = Registry::default();
        r.accounts.push(record("p123", "codex"));
        let text = serde_json::to_string(&r).unwrap();
        assert!(!text.contains("access_token"));
        assert!(!text.contains("refresh_token"));
        assert!(!text.contains("api_key"));
    }
    #[test]
    fn expiry_and_identity_are_local_hints_not_validation() {
        let v = serde_json::json!({"claudeAiOauth":{"expiresAt":1_800_000_000_000u64,"accessToken":"ignored"},"email":"work@example.test"});
        assert_eq!(expiry_from(&v), Some(1_800_000_000_000));
        assert_eq!(identity_from(&v).as_deref(), Some("work@example.test"));
        assert!(jwt_claims("bad.invalid.token").is_none());
    }
    #[test]
    fn generation_prevents_account_switch_aba_and_is_per_provider() {
        let mut r = Registry::default();
        bump(&mut r, "codex");
        bump(&mut r, "codex");
        assert_eq!(r.generation["codex"], 2);
        assert!(!r.generation.contains_key("claude"));
    }
    #[test]
    fn cached_registry_detects_external_replacement_and_keeps_account_revisions() {
        let fixture = RegistryFixture::new();
        let mut registry = Registry::default();
        registry.accounts = vec![record("p123", "codex"), record("p456", "codex")];
        registry.active.insert("codex".into(), "p123".into());
        fixture.save(&registry);
        let mut cache = RegistryCache { entry: None };
        let initial = key_for(cache.read(&fixture.path()), "codex");
        for _ in 0..100 {
            assert_eq!(key_for(cache.read(&fixture.path()), "codex"), initial);
        }
        registry.active.insert("codex".into(), "p456".into());
        bump(&mut registry, "codex");
        fixture.save(&registry);
        assert_eq!(key_for(cache.read(&fixture.path()), "codex"), "p456:1");
        registry.active.insert("codex".into(), "p123".into());
        bump(&mut registry, "codex");
        fixture.save(&registry);
        assert_ne!(key_for(cache.read(&fixture.path()), "codex"), initial);
    }
    #[test]
    fn corrupt_external_registry_fails_closed_and_recovers_when_repaired() {
        let fixture = RegistryFixture::new();
        let mut registry = Registry::default();
        registry.accounts.push(record("p123", "codex"));
        registry.active.insert("codex".into(), "p123".into());
        fixture.save(&registry);
        let mut cache = RegistryCache { entry: None };
        assert_eq!(key_for(cache.read(&fixture.path()), "codex"), "p123:0");
        write_atomic(&fixture.path(), b"{broken registry").unwrap();
        assert_eq!(
            key_for(cache.read(&fixture.path()), "codex"),
            "registry-unavailable"
        );
        fixture.save(&registry);
        assert_eq!(key_for(cache.read(&fixture.path()), "codex"), "p123:0");
    }
    #[test]
    fn cached_registry_tracks_deletion_missing_file_creation_and_path_changes() {
        let first = RegistryFixture::new();
        let second = RegistryFixture::new();
        let mut cache = RegistryCache { entry: None };
        assert!(!cache.read(&first.path()).broken);
        let mut registry = Registry::default();
        registry.accounts.push(record("p123", "codex"));
        registry.active.insert("codex".into(), "p123".into());
        first.save(&registry);
        assert_eq!(key_for(cache.read(&first.path()), "codex"), "p123:0");
        assert_eq!(key_for(cache.read(&second.path()), "codex"), "external:0");
        assert_eq!(key_for(cache.read(&first.path()), "codex"), "p123:0");
        std::fs::remove_file(first.path()).unwrap();
        assert_eq!(key_for(cache.read(&first.path()), "codex"), "external:0");
    }
    #[test]
    fn registry_cache_bounds_file_size_and_profile_count_without_reading_credentials() {
        let fixture = RegistryFixture::new();
        let file = std::fs::File::create(fixture.path()).unwrap();
        file.set_len(MAX_REGISTRY_BYTES + 1).unwrap();
        let mut cache = RegistryCache { entry: None };
        assert!(cache.read(&fixture.path()).broken);
        let mut registry = Registry::default();
        registry.accounts = (0..MAX_ACCOUNTS_PER_PROVIDER + 10)
            .map(|i| record(&format!("p{:x}", i + 1), "codex"))
            .collect();
        fixture.save(&registry);
        assert_eq!(
            cache.read(&fixture.path()).accounts.len(),
            MAX_ACCOUNTS_PER_PROVIDER
        );
        assert!(!serde_json::to_string(cache.read(&fixture.path()))
            .unwrap()
            .contains("access_token"));
    }
    #[test]
    #[cfg(windows)]
    fn dpapi_roundtrip_is_ciphertext_and_corrupt_blob_is_rejected() {
        let secret = b"fixture-only-example-token";
        let encrypted = protect(secret).unwrap();
        assert!(!encrypted.windows(secret.len()).any(|w| w == secret));
        assert_eq!(unprotect(&encrypted).unwrap(), secret);
        assert!(unprotect(b"not-dpapi").is_err());
    }
}
