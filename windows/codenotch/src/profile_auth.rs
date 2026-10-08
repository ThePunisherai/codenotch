//! Browser sign-in through official provider flows. CLI helpers run without a
//! visible terminal and every account has an isolated credential directory.
//! Only verified browser links and public one-time user codes cross IPC.
use std::{
    collections::HashMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, LazyLock, Mutex,
    },
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Url};
static BUSY: LazyLock<Mutex<HashMap<String, Arc<AtomicBool>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CHALLENGES: LazyLock<Mutex<HashMap<String, PublicChallenge>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PUBLIC_STATES: LazyLock<Mutex<HashMap<String, serde_json::Value>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static EVENT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const OPENCODE_CONSOLE: &str = "https://opencode.ai/console";
const OPENCODE_CLIENT: &str = "codenotch";

#[derive(Clone)]
struct PublicChallenge {
    url: String,
    user_code: Option<String>,
    expires_at: Option<u64>,
}
pub(crate) struct LoginGuard {
    key: String,
    cancelled: Arc<AtomicBool>,
}
impl Drop for LoginGuard {
    fn drop(&mut self) {
        let mut busy = BUSY.lock().unwrap_or_else(|e| e.into_inner());
        if busy
            .get(&self.key)
            .is_some_and(|flag| Arc::ptr_eq(flag, &self.cancelled))
        {
            busy.remove(&self.key);
        }
    }
}
impl LoginGuard {
    pub(crate) fn cancelled(&self) -> &AtomicBool {
        &self.cancelled
    }
    fn finish(self, app: &AppHandle, provider: &str, id: &str, note: &str) {
        let mut busy = BUSY.lock().unwrap_or_else(|e| e.into_inner());
        if busy
            .get(&self.key)
            .is_some_and(|flag| Arc::ptr_eq(flag, &self.cancelled))
        {
            self.cancelled.store(true, Ordering::Release);
            emit(app, provider, id, false, note);
            busy.remove(&self.key);
        }
    }
}
pub(crate) fn reserve_profile_work(provider: &str, id: &str) -> Result<LoginGuard, String> {
    reserve(provider, id)
}
fn busy_key(provider: &str, id: &str) -> String {
    format!("{}:{id}", crate::accounts::canonical_provider(provider))
}
fn reserve(provider: &str, id: &str) -> Result<LoginGuard, String> {
    let key = busy_key(provider, id);
    let mut busy = BUSY.lock().unwrap_or_else(|e| e.into_inner());
    if busy.contains_key(&key) {
        return Err("Sign-in or token renewal for this account is already running.".into());
    }
    if busy.len() >= 64 {
        return Err(
            "Too many sign-ins are running. Finish or cancel an existing account first.".into(),
        );
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    busy.insert(key.clone(), cancelled.clone());
    Ok(LoginGuard { key, cancelled })
}
pub fn is_busy(provider: &str, id: &str) -> bool {
    match crate::accounts::canonical_provider(provider) {
        "codex" => crate::codex::login_busy(),
        "claude" => crate::claude_auth::state().busy,
        p => BUSY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&busy_key(p, id)),
    }
}
fn desktop_cli(provider: &str) -> Option<PathBuf> {
    let (folder, executable) = if provider == "cursor" {
        ("Cursor", "Cursor.exe")
    } else {
        ("Antigravity", "Antigravity.exe")
    };
    let mut candidates = Vec::new();
    if let Some(local) = dirs::data_local_dir() {
        candidates.push(local.join("Programs").join(folder).join(executable));
    }
    for key in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(p) = std::env::var_os(key) {
            candidates.push(PathBuf::from(p).join(folder).join(executable));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}
pub fn capability(provider: &str) -> (&'static str, bool, &'static str) {
    match crate::accounts::canonical_provider(provider) {
        "codex" => ("browser", crate::codex::browser_login_available(), "Sign in on the official OpenAI website. Install the current official Codex CLI if browser sign-in is unavailable."),
        "claude" => ("browser", crate::usage::find_browser_cli().is_some(), "Sign in on the official Claude website. Install the current native Claude Code CLI if browser sign-in is unavailable."),
        "copilot" => ("browser", crate::copilot::find_executable().is_some(), "Sign in on github.com and enter the one-time code shown here. GitHub CLI runs in the background for this account."),
        "grok" => ("browser", grok_browser_cli().is_some(), "Sign in on the official xAI website with a one-time device code. Install the current native Grok CLI if browser sign-in is unavailable."),
        "opencode" => ("browser", true, "Sign in on opencode.ai, choose your workspace and approve this account. No terminal or installed OpenCode CLI is required."),
        "glm" => ("api_key", true, "Open the Z.ai website to sign in and create a Coding Plan API key, then use Connect key. Website sign-in alone does not connect quota."),
        "cursor" => ("desktop_web", desktop_cli("cursor").is_some(), "Cursor opens its own official browser login. Complete it from the editor, then Detect accounts to save that session. Website login alone cannot expose editor quota."),
        "antigravity" => ("desktop_web", desktop_cli("antigravity").is_some(), "Antigravity opens its own Google browser login. Complete it from the app, then Detect accounts to save that session. Its shared Windows login cannot be isolated by opening a website alone."),
        _ => ("unsupported", false, "Unsupported provider."),
    }
}
pub(crate) fn grok_browser_cli() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let mut dirs = Vec::new();
        if let Some(home) = dirs::home_dir() {
            dirs.push(home.join(".grok").join("bin"));
        }
        if let Some(path) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&path));
        }
        return dirs
            .into_iter()
            .map(|dir| dir.join("grok.exe"))
            .find(|p| p.is_file());
    }
    #[cfg(not(windows))]
    crate::grok::find_cli()
}
#[tauri::command]
pub async fn provider_sign_in(
    app: AppHandle,
    provider: String,
    account_id: String,
) -> Result<(), String> {
    crate::accounts::account_io(move || provider_sign_in_now(app, provider, account_id)).await
}
pub(crate) fn provider_sign_in_now(
    app: AppHandle,
    provider: String,
    account_id: String,
) -> Result<(), String> {
    let provider = crate::accounts::canonical_provider(&provider);
    crate::accounts::with_profile(provider, &account_id, |ctx| match provider {
        "codex" => crate::codex::login_profile(&app, &account_id, ctx.root),
        "claude" => crate::claude_auth::login_profile(&app, &account_id, ctx.root),
        "cursor" | "antigravity" => {
            let exe = desktop_cli(provider).ok_or("Desktop app not found. Install the provider app to complete its official browser login, then Detect accounts.")?;
            Command::new(exe)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|_| "Unable to open the provider app.")?;
            emit(&app, provider, &account_id, false, "Use Sign in in the provider app to open its official website, then Detect accounts to save that session.");
            Ok(())
        }
        "copilot" | "grok" => start_cli(&app, provider, &account_id, ctx.root),
        "opencode" => start_opencode(&app, &account_id),
        "glm" => Err(
            "Sign in on the Z.ai website, then use Connect key for its Coding Plan API key.".into(),
        ),
        _ => Err("Unsupported provider.".into()),
    })
}
fn emit(app: &AppHandle, provider: &str, id: &str, busy: bool, note: &str) {
    emit_profile_status(app, provider, id, busy, note);
}
pub(crate) fn emit_profile_status(
    app: &AppHandle,
    provider: &str,
    id: &str,
    busy: bool,
    note: &str,
) {
    if !busy {
        clear_browser_challenge(provider, id);
    }
    let provider = if provider == "antigravity" {
        "gemini"
    } else {
        provider
    };
    publish_public_event(
        app,
        provider,
        id,
        serde_json::json!({"provider":provider,"profile_id":id,"busy":busy,"note":note}),
    );
}
fn publish_public_event(app: &AppHandle, provider: &str, id: &str, mut event: serde_json::Value) {
    let mut states = PUBLIC_STATES.lock().unwrap_or_else(|e| e.into_inner());
    event["sequence"] = serde_json::json!(EVENT_SEQUENCE
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1));
    let key = busy_key(provider, id);
    if event.get("busy").and_then(|v| v.as_bool()) == Some(true) {
        states.insert(key, event.clone());
    } else {
        states.remove(&key);
    }
    let _ = app.emit("provider-account-login", event);
}
#[tauri::command]
pub fn get_provider_sign_in_states() -> Vec<serde_json::Value> {
    PUBLIC_STATES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .cloned()
        .collect()
}
pub fn cancel_all_logins() {
    {
        let busy = BUSY.lock().unwrap_or_else(|e| e.into_inner());
        for flag in busy.values() {
            flag.store(true, Ordering::Release);
        }
    }
    CHALLENGES.lock().unwrap_or_else(|e| e.into_inner()).clear();
    PUBLIC_STATES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    // Native Grok opens the browser itself and cannot use a kill-tree Job.
    // Give its owner worker time to kill/reap only the CLI before app exit.
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if !BUSY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .any(|key| key.starts_with("grok:"))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn validated_browser_url(provider: &str, value: &str) -> Option<String> {
    if value.len() > 4096 || value.chars().any(char::is_control) {
        return None;
    }
    let url = Url::parse(value).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let host = url.host_str()?;
    let accepted = match crate::accounts::canonical_provider(provider) {
        "codex" => {
            host == "auth.openai.com" && matches!(url.path(), "/authorize" | "/oauth/authorize")
        }
        "claude" => matches!(
            (host, url.path()),
            ("claude.com", "/cai/oauth/authorize")
                | ("claude.ai", "/oauth/authorize")
                | ("platform.claude.com", "/oauth/authorize")
                | ("console.anthropic.com", "/oauth/authorize")
        ),
        "copilot" => {
            host == "github.com" && matches!(url.path(), "/login/device" | "/login/device/")
        }
        "grok" => host == "auth.x.ai" && !url.path().contains("authorize"),
        "opencode" => {
            host == "opencode.ai" && matches!(url.path(), "/console/device" | "/console/device/")
        }
        _ => false,
    };
    if !accepted {
        return None;
    }
    for (key, _) in url.query_pairs() {
        let key = key.to_ascii_lowercase();
        if matches!(
            key.as_str(),
            "access_token"
                | "id_token"
                | "refresh_token"
                | "client_secret"
                | "code_verifier"
                | "token"
                | "device_code"
        ) {
            return None;
        }
        if matches!(provider, "grok" | "copilot" | "opencode")
            && !matches!(key.as_str(), "user_code" | "code" | "usercode")
        {
            return None;
        }
    }
    Some(url.into())
}
fn valid_user_code(value: &str) -> bool {
    (6..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
}
fn publish_challenge(
    app: &AppHandle,
    provider: &str,
    id: &str,
    challenge: PublicChallenge,
    active: Option<&AtomicBool>,
    cancelled: Option<&AtomicBool>,
) -> Result<(), String> {
    let url = validated_browser_url(provider, &challenge.url)
        .ok_or("The provider returned an unsupported sign-in link.")?;
    if challenge
        .user_code
        .as_deref()
        .is_some_and(|code| !valid_user_code(code))
    {
        return Err("The provider returned an unsupported one-time code.".into());
    }
    let challenge = PublicChallenge { url, ..challenge };
    let mut challenges = CHALLENGES.lock().unwrap_or_else(|e| e.into_inner());
    // CLI drain threads can outlive the child by a few milliseconds. The
    // per-attempt flag is checked under the same lock used by final cleanup.
    if active.is_some_and(|flag| !flag.load(Ordering::Acquire))
        || cancelled.is_some_and(|flag| flag.load(Ordering::Acquire))
    {
        return Err("This browser sign-in is no longer active.".into());
    }
    challenges.insert(busy_key(provider, id), challenge.clone());
    publish_public_event(
        app,
        provider,
        id,
        serde_json::json!({
            "provider":provider,"profile_id":id,"busy":true,
            "note":if challenge.user_code.is_some() { "Complete sign-in in your browser using this one-time code." } else { "Complete sign-in on the official provider website." },
            "url":challenge.url,"user_code":challenge.user_code,"expires_at":challenge.expires_at,
        }),
    );
    Ok(())
}
pub(crate) fn set_browser_challenge(
    app: &AppHandle,
    provider: &str,
    id: &str,
    url: &str,
    usercode: Option<&str>,
) -> Result<(), String> {
    publish_challenge(
        app,
        provider,
        id,
        PublicChallenge {
            url: url.into(),
            user_code: usercode.map(String::from),
            expires_at: None,
        },
        None,
        None,
    )
}
pub(crate) fn set_browser_challenge_if_active(
    app: &AppHandle,
    provider: &str,
    id: &str,
    url: &str,
    usercode: Option<&str>,
    active: &AtomicBool,
) -> Result<(), String> {
    publish_challenge(
        app,
        provider,
        id,
        PublicChallenge {
            url: url.into(),
            user_code: usercode.map(String::from),
            expires_at: None,
        },
        Some(active),
        None,
    )
}
pub(crate) fn clear_browser_challenge(provider: &str, id: &str) {
    CHALLENGES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&busy_key(provider, id));
}
fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(windows)]
    let mut cmd = {
        let system = std::env::var_os("SystemRoot").ok_or("Windows directory unavailable.")?;
        let mut cmd = Command::new(PathBuf::from(system).join("System32/rundll32.exe"));
        cmd.arg("url.dll,FileProtocolHandler").arg(url);
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
        cmd
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(url);
        cmd
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "Unable to open the browser. Use the displayed official sign-in link.")?;
    Ok(())
}
#[tauri::command]
pub async fn reopen_provider_sign_in(
    app: AppHandle,
    provider: String,
    account_id: String,
) -> Result<(), String> {
    crate::accounts::account_io(move || {
        let _ = app;
        let challenge = CHALLENGES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&busy_key(&provider, &account_id))
            .cloned()
            .ok_or("There is no pending browser sign-in for this account.")?;
        if challenge.expires_at.is_some_and(|t| t <= crate::now_ms()) {
            return Err("This sign-in code has expired. Start sign-in again.".into());
        }
        let url =
            validated_browser_url(&provider, &challenge.url).ok_or("Unsupported sign-in link.")?;
        open_browser(&url)
    })
    .await
}
#[tauri::command]
pub async fn cancel_provider_sign_in(
    app: AppHandle,
    provider: String,
    account_id: String,
) -> Result<(), String> {
    crate::accounts::account_io(move || {
        let provider = crate::accounts::canonical_provider(&provider);
        match provider {
            "codex" => crate::codex::cancel_profile_login(&app, &account_id)?,
            "claude" => crate::claude_auth::cancel_profile_login(&app, &account_id)?,
            _ => {
                let busy = BUSY.lock().unwrap_or_else(|e| e.into_inner());
                let cancelled = busy
                    .get(&busy_key(provider, &account_id))
                    .ok_or("There is no pending sign-in for this account.")?;
                cancelled.store(true, Ordering::Release);
                clear_browser_challenge(provider, &account_id);
                emit(&app, provider, &account_id, true, "Cancelling sign-in…");
            }
        }
        Ok(())
    })
    .await
}
#[tauri::command]
pub async fn complete_provider_sign_in(
    provider: String,
    account_id: String,
    mut code: String,
) -> Result<(), String> {
    crate::accounts::account_io(move || {
        if crate::accounts::canonical_provider(&provider) != "claude" {
            code.clear();
            return Err("This provider completes sign-in automatically in your browser.".into());
        }
        let result = crate::claude_auth::submit_code(&account_id, &code);
        code.clear();
        result
    })
    .await
}

#[cfg(windows)]
fn login_command(provider: &str, cli: &Path, root: &Path) -> Result<Command, String> {
    if provider == "grok" {
        let mut cmd = Command::new(cli);
        cmd.args(["login", "--device-auth"]);
        isolate(&mut cmd, provider, root);
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
        return Ok(cmd);
    }
    let script = match provider {
        "copilot" => "if ([Console]::ReadLine() -ne 'CODENOTCH_AUTH_START') { exit 1 }; '' | & $env:CODENOTCH_AUTH_CLI auth login --web --hostname github.com --git-protocol https --skip-ssh-key --insecure-storage; exit $LASTEXITCODE",
        _ => return Err("Unsupported sign-in provider.".into()),
    };
    let system = std::env::var_os("SystemRoot").ok_or("Windows directory unavailable.")?;
    let mut cmd =
        Command::new(PathBuf::from(system).join("System32/WindowsPowerShell/v1.0/powershell.exe"));
    cmd.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        script,
    ])
    .env("CODENOTCH_AUTH_CLI", cli);
    isolate(&mut cmd, provider, root);
    // A provider CLI stays behind the account dialog. It never opens PowerShell.
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000);
    Ok(cmd)
}
#[cfg(not(windows))]
fn login_command(provider: &str, cli: &Path, root: &Path) -> Result<Command, String> {
    let mut cmd = Command::new(cli);
    match provider {
        "copilot" => {
            cmd.args([
                "auth",
                "login",
                "--web",
                "--hostname",
                "github.com",
                "--git-protocol",
                "https",
                "--skip-ssh-key",
                "--insecure-storage",
            ]);
        }
        "grok" => {
            cmd.args(["login", "--device-auth"]);
        }
        _ => return Err("Unsupported sign-in provider.".into()),
    }
    isolate(&mut cmd, provider, root);
    Ok(cmd)
}
fn isolate(cmd: &mut Command, provider: &str, root: &Path) {
    cmd.current_dir(root).env("NO_COLOR", "1");
    for key in [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        "GH_DEBUG",
        "DEBUG",
        "GROK_HOME",
        "GROK_API_KEY",
        "XAI_API_KEY",
        "GROK_OIDC_ISSUER",
        "GROK_OIDC_CLIENT_ID",
        "OPENCODE_API_KEY",
        "OPENCODE_API_TOKEN",
        "OPENCODE_ZEN_API_KEY",
        "OPENCODE_GO_API_KEY",
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "GH_CONFIG_DIR",
    ] {
        cmd.env_remove(key);
    }
    match provider {
        "copilot" => {
            cmd.env("GH_CONFIG_DIR", root)
                .env("GH_PROMPT_DISABLED", "1")
                .env("GH_NO_UPDATE_NOTIFIER", "1");
            // Codenotch opens the verified device URL after it receives the public
            // user code, so gh must not open a second tab or terminal browser.
            #[cfg(windows)]
            cmd.env("GH_BROWSER", "cmd /c exit 0");
            #[cfg(not(windows))]
            cmd.env("GH_BROWSER", "/bin/true");
        }
        "grok" => sanitize_grok_auth_env(cmd, root),
        _ => {}
    }
}
pub(crate) fn sanitize_grok_auth_env(cmd: &mut Command, root: &Path) {
    // The official CLI supports environment overrides for its credential file,
    // issuer and external auth commands. Freeze credential/endpoint inputs;
    // retain admin team restrictions, API-key lockdown and custom TLS roots.
    for key in [
        "GROK_AUTH_PATH",
        "GROK_AUTH",
        "GROK_AUTH_PROVIDER_COMMAND",
        "GROK_AUTH_PROVIDER_LABEL",
        "GROK_AUTH_PROVIDER_TOKEN_TTL",
        "GROK_CODE_XAI_API_KEY",
        "GROK_OAUTH2_ISSUER",
        "GROK_OAUTH2_CLIENT_ID",
        "GROK_OAUTH2_PRINCIPAL_TYPE",
        "GROK_OAUTH2_PRINCIPAL_ID",
        "GROK_OAUTH2_SCOPES",
        "GROK_OAUTH2_REDIRECT_URI",
        "GROK_OAUTH2_REFERRER",
        "GROK_LOCAL_AUTH",
        "GROK_API_KEY",
        "GROK_OIDC_ISSUER",
        "GROK_OIDC_CLIENT_ID",
        "GROK_OIDC_SCOPES",
        "GROK_OIDC_AUDIENCE",
        "GROK_CLI_CHAT_PROXY_BASE_URL",
        "GROK_WS_ORIGIN",
        "GROK_WS_URL",
        "XAI_API_KEY",
    ] {
        cmd.env_remove(key);
    }
    cmd.current_dir(root).env("GROK_HOME", root);
}
fn terminate(child: &mut Child) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if let Some(root) = std::env::var_os("SystemRoot") {
            let _ = Command::new(PathBuf::from(root).join("System32/taskkill.exe"))
                .args(["/PID", &child.id().to_string(), "/T", "/F"])
                .creation_flags(0x0800_0000)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

// gh's browser is disabled with GH_BROWSER; Codenotch opens its link outside
// this job. Grok, Codex and Claude open a user browser themselves and therefore
// cannot be placed in a job that kills descendants.
#[cfg(windows)]
struct DeviceJob(Option<windows::Win32::Foundation::HANDLE>);
#[cfg(windows)]
unsafe impl Send for DeviceJob {}
#[cfg(windows)]
impl Drop for DeviceJob {
    fn drop(&mut self) {
        unsafe {
            if let Some(handle) = self.0 {
                let _ = windows::Win32::Foundation::CloseHandle(handle);
            }
        }
    }
}
#[cfg(not(windows))]
struct DeviceJob;
#[cfg(windows)]
fn own_device_process(child: &mut Child, provider: &str) -> Result<DeviceJob, String> {
    if provider == "grok" {
        return Ok(DeviceJob(None));
    }
    use std::os::windows::io::AsRawHandle;
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::HANDLE,
            System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
        },
    };
    let result = unsafe {
        CreateJobObjectW(None, PCWSTR::null()).and_then(|handle| {
            let job = DeviceJob(Some(handle));
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of_val(&info) as u32,
            )?;
            AssignProcessToJobObject(handle, HANDLE(child.as_raw_handle()))?;
            Ok(job)
        })
    };
    match result {
        Ok(job) => Ok(job),
        Err(_) => {
            terminate(child);
            Err("Unable to protect the background sign-in process. Retry sign-in.".into())
        }
    }
}
#[cfg(not(windows))]
fn own_device_process(_: &mut Child, _: &str) -> Result<DeviceJob, String> {
    Ok(DeviceJob)
}
fn public_code_from_line(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let labelled = [
        "one-time code:",
        "user code:",
        "verification code:",
        "enter code:",
    ]
    .iter()
    .find_map(|label| lower.find(label).map(|i| i + label.len()));
    // A bare public `Code:` label is accepted only at the start of a line;
    // `device_code:` and token diagnostic lines must never reach the UI.
    let index = labelled.or_else(|| {
        lower
            .trim_start()
            .starts_with("code:")
            .then(|| lower.len() - lower.trim_start().len() + 5)
    })?;
    let code = line
        .get(index..)?
        .split_whitespace()
        .next()?
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
    valid_user_code(code).then(|| code.to_string())
}
#[derive(Default)]
struct DeviceParser {
    url: Option<String>,
    code: Option<String>,
    next_line_is_code: bool,
}
impl DeviceParser {
    fn accept(&mut self, provider: &str, line: &str) -> Option<PublicChallenge> {
        let trimmed = line.trim();
        if self.next_line_is_code && !trimmed.is_empty() {
            self.next_line_is_code = false;
            if valid_user_code(trimmed) {
                self.code = Some(trimmed.into());
            }
        }
        if matches!(
            trimmed,
            "Confirm this code in your browser:" | "Then enter this code:"
        ) {
            self.next_line_is_code = true;
        }
        if let Some(code) = public_code_from_line(line) {
            self.code = Some(code);
        }
        if provider == "copilot" {
            if self.code.is_some() {
                self.url = Some("https://github.com/login/device".into());
            }
        } else {
            for token in line.split_whitespace() {
                let token = token.trim_matches(|c: char| {
                    matches!(
                        c,
                        '"' | '\'' | '(' | ')' | '[' | ']' | '<' | '>' | ',' | '.'
                    )
                });
                if let Some(url) = validated_browser_url(provider, token) {
                    if let Ok(parsed) = Url::parse(&url) {
                        if let Some((_, code)) = parsed.query_pairs().find(|(name, _)| {
                            matches!(name.as_ref(), "user_code" | "code" | "userCode")
                        }) {
                            if valid_user_code(&code) {
                                self.code = Some(code.into());
                            }
                        }
                    }
                    self.url = Some(url);
                }
            }
        }
        Some(PublicChallenge {
            url: self.url.clone()?,
            user_code: Some(self.code.clone()?),
            expires_at: None,
        })
    }
}
fn drain_lines(reader: impl Read + Send + 'static, sender: std::sync::mpsc::SyncSender<String>) {
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut bytes = [0u8; 1024];
        let mut line = Vec::with_capacity(4096);
        let mut overflow = false;
        while let Ok(count) = reader.read(&mut bytes) {
            if count == 0 {
                break;
            }
            for byte in &bytes[..count] {
                if *byte == b'\n' || *byte == b'\r' {
                    if !overflow
                        && !line.is_empty()
                        && sender.send(String::from_utf8_lossy(&line).into()).is_err()
                    {
                        return;
                    }
                    line.clear();
                    overflow = false;
                } else if line.len() < 4096 {
                    line.push(*byte);
                } else {
                    overflow = true;
                }
            }
        }
        if !overflow && !line.is_empty() {
            let _ = sender.send(String::from_utf8_lossy(&line).into());
        }
    });
}
fn wait_for_browser_cli(
    child: &mut Child,
    app: &AppHandle,
    provider: &str,
    id: &str,
    cancelled: &AtomicBool,
) -> bool {
    let (sender, receiver) = std::sync::mpsc::sync_channel(16);
    if let Some(stdout) = child.stdout.take() {
        drain_lines(stdout, sender.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        drain_lines(stderr, sender.clone());
    }
    drop(sender);
    let deadline = Instant::now() + Duration::from_secs(15 * 60);
    let mut parser = DeviceParser::default();
    let mut last = None;
    loop {
        if cancelled.load(Ordering::Relaxed) || Instant::now() >= deadline {
            if provider == "grok" {
                let _ = child.kill();
                let _ = child.wait();
            } else {
                terminate(child);
            }
            return false;
        }
        if let Ok(line) = receiver.recv_timeout(Duration::from_millis(100)) {
            if let Some(challenge) = parser.accept(provider, &line) {
                let stamp = (challenge.url.clone(), challenge.user_code.clone());
                if last.as_ref() != Some(&stamp)
                    && publish_challenge(
                        app,
                        provider,
                        id,
                        challenge.clone(),
                        None,
                        Some(cancelled),
                    )
                    .is_ok()
                {
                    if provider != "grok" {
                        let _ = open_browser(&challenge.url);
                    }
                    last = Some(stamp);
                }
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {}
            Err(_) => {
                if provider == "grok" {
                    let _ = child.kill();
                    let _ = child.wait();
                } else {
                    terminate(child);
                }
                return false;
            }
        }
    }
}
fn capture_owned(provider: &str, root: &Path) -> Option<(String, PathBuf)> {
    match provider {
        "copilot" => {
            let path = root.join("hosts.yml");
            let text = std::fs::read_to_string(&path).ok()?;
            let (username, token) = crate::copilot::parse_hosts(&text);
            Some((
                serde_json::json!({"token":token?,"username":username}).to_string(),
                path,
            ))
        }
        "grok" => {
            let path = root.join("auth.json");
            let raw = std::fs::read_to_string(&path).ok()?;
            crate::grok::pick(&serde_json::from_str::<serde_json::Value>(&raw).ok()?)?;
            Some((raw, path))
        }
        _ => None,
    }
}
fn start_cli(app: &AppHandle, provider: &str, id: &str, root: PathBuf) -> Result<(), String> {
    let cli=match provider { "copilot"=>crate::copilot::find_executable(),"grok"=>grok_browser_cli(),_=>None }
        .ok_or("Official provider login helper not found. Install its CLI first to sign in in your browser.")?;
    let guard = reserve(provider, id)?;
    let credential_names = if provider == "grok" {
        vec!["auth.json"]
    } else {
        vec!["hosts.yml"]
    };
    let before = crate::claude_auth::credential_stamp(&root, &credential_names);
    let mut cmd = login_command(provider, &cli, &root)?;
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|_| "Unable to start the provider's browser sign-in helper.")?;
    let job = own_device_process(&mut child, provider)?;
    // On Windows the wrapper waits before spawning the vendor helper. Release
    // this fixed gate only after Job assignment, so descendants inherit the Job.
    #[cfg(windows)]
    if provider == "copilot" {
        let started = child
            .stdin
            .take()
            .ok_or("The background sign-in helper did not accept its startup gate.")
            .and_then(|mut stdin| {
                stdin
                    .write_all(b"CODENOTCH_AUTH_START\n")
                    .and_then(|_| stdin.flush())
                    .map_err(|_| "Unable to start the background sign-in helper.")
            });
        if let Err(error) = started {
            terminate(&mut child);
            return Err(error.into());
        }
    }
    // gh's browser device grant can wait for Enter before it begins polling.
    // On Windows its wrapper supplies this fixed line after the startup gate.
    #[cfg(not(windows))]
    if provider == "copilot" {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(b"\n");
        }
    }
    let app = app.clone();
    let provider = provider.to_string();
    let id = id.to_string();
    emit(
        &app,
        &provider,
        &id,
        true,
        "Preparing your official browser sign-in…",
    );
    std::thread::spawn(move || {
        let success = wait_for_browser_cli(&mut child, &app, &provider, &id, &guard.cancelled);
        // Close the job before credentials are captured or BUSY is released;
        // any remaining helper descendant must stop writing to this profile.
        drop(job);
        let result = if success && !guard.cancelled.load(Ordering::Relaxed) {
            capture_owned(&provider,&root).ok_or("Sign-in completed without a supported account credential.")
                .and_then(|(raw,path)| {
                    if provider!="grok" {
                        crate::accounts::save_profile_secret(&app,&provider,&id,&raw).map_err(|_| "Unable to protect the signed-in account.")?;
                        if path.is_file() { std::fs::remove_file(path).map_err(|_| "Account saved, but its temporary credential file could not be deleted.")?; }
                    }
                    Ok(())
                })
        } else {
            Err("Browser sign-in cancelled, failed or timed out. Retry with the current official provider CLI.")
        };
        let changed = crate::claude_auth::credential_stamp(&root, &credential_names) != before;
        let cleanup_error = if provider == "copilot" && result.is_err() {
            let path = root.join("hosts.yml");
            path.is_file() && std::fs::remove_file(path).is_err()
        } else {
            false
        };
        if result.is_ok() || changed {
            crate::accounts::login_finished(&app, &provider, &id);
        }
        guard.finish(
            &app,
            &provider,
            &id,
            if cleanup_error { "Sign-in ended, but its temporary credential could not be deleted. Retry sign-in or remove this account." } else { result
                .err()
                .unwrap_or("Browser sign-in complete. Refreshing this account's usage.") },
        );
    });
    Ok(())
}

struct DeviceGrant {
    device_code: String,
    challenge: PublicChallenge,
    interval: u64,
}
fn opencode_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(Duration::from_secs(15))
        .build()
}
fn read_auth_json(reader: impl Read) -> Result<serde_json::Value, String> {
    const LIMIT: usize = 256 * 1024;
    let mut bytes = Vec::new();
    reader
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "Unable to read the provider sign-in response.")?;
    if bytes.len() > LIMIT {
        return Err("The provider sign-in response was too large.".into());
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| "The provider returned an invalid sign-in response.".into())
}
fn parse_device_grant(value: &serde_json::Value, now: u64) -> Result<DeviceGrant, String> {
    let device_code = value
        .get("device_code")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty() && v.len() <= 4096)
        .ok_or("OpenCode returned an invalid device authorization response.")?
        .to_string();
    let user_code = value
        .get("user_code")
        .and_then(|v| v.as_str())
        .filter(|v| valid_user_code(v))
        .ok_or("OpenCode returned an invalid one-time code.")?
        .to_string();
    let uri = value
        .get("verification_uri_complete")
        .and_then(|v| v.as_str())
        .ok_or("OpenCode did not provide its browser sign-in link.")?;
    let uri = if uri.starts_with('/') && !uri.starts_with("//") {
        format!("https://opencode.ai{uri}")
    } else {
        uri.to_string()
    };
    let url = validated_browser_url("opencode", &uri)
        .ok_or("OpenCode returned an unsupported browser sign-in link.")?;
    let parsed =
        Url::parse(&url).map_err(|_| "OpenCode returned an invalid browser sign-in link.")?;
    if parsed
        .query_pairs()
        .find(|(key, _)| key == "user_code")
        .is_some_and(|(_, code)| code != user_code)
    {
        return Err("OpenCode returned mismatched sign-in codes.".into());
    }
    let expires = value
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .filter(|v| (1..=900).contains(v))
        .unwrap_or(600);
    let interval = value
        .get("interval")
        .and_then(|v| v.as_u64())
        .unwrap_or(5)
        .max(5);
    Ok(DeviceGrant {
        device_code,
        challenge: PublicChallenge {
            url,
            user_code: Some(user_code),
            expires_at: Some(now.saturating_add(expires * 1000)),
        },
        interval,
    })
}
fn parse_opencode_token(
    value: &serde_json::Value,
    old_refresh: Option<&str>,
    previous_org: Option<&str>,
    now: u64,
) -> Result<String, String> {
    let access = value
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty() && v.len() <= 64 * 1024 && !v.chars().any(char::is_control))
        .ok_or("OpenCode did not return a usable account token.")?;
    let refresh = value
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty() && v.len() <= 64 * 1024 && !v.chars().any(char::is_control))
        .ok_or("OpenCode did not return its rotating renewal token.")?;
    if old_refresh == Some(refresh) {
        return Err("OpenCode did not rotate the renewal token. Sign in again to renew this account safely.".into());
    }
    let expires = value
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .filter(|v| (1..=366 * 24 * 3600).contains(v))
        .ok_or("OpenCode did not return a usable token expiry.")?;
    let supplied_org = value
        .get("org_id")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty() && v.len() <= 256 && !v.chars().any(char::is_control));
    if supplied_org
        .zip(previous_org)
        .is_some_and(|(new, old)| new != old)
    {
        return Err(
            "OpenCode returned a different workspace during token renewal. Sign in again.".into(),
        );
    }
    let org = supplied_org
        .or(previous_org)
        .ok_or("OpenCode did not return the workspace selected in your browser.")?;
    Ok(serde_json::json!({"type":"oauth","access":access,"refresh":refresh,"expires":now.saturating_add(expires*1000),"authClientID":OPENCODE_CLIENT,"metadata":{"orgID":org,"server":OPENCODE_CONSOLE}}).to_string())
}
fn wait_cancelled(cancelled: &AtomicBool, seconds: u64) -> bool {
    for _ in 0..seconds.saturating_mul(4) {
        if cancelled.load(Ordering::Relaxed) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    cancelled.load(Ordering::Relaxed)
}
fn run_opencode_device(app: &AppHandle, id: &str, cancelled: &AtomicBool) -> Result<(), String> {
    let agent = opencode_agent();
    let response = agent
        .post(&format!("{OPENCODE_CONSOLE}/auth/device/code"))
        .send_form(&[
            ("client_id", OPENCODE_CLIENT),
            ("supports_org_scope", "true"),
        ])
        .map_err(|_| {
            "Unable to request OpenCode browser sign-in. Check the connection and retry."
        })?;
    let value = read_auth_json(response.into_reader())?;
    let grant = parse_device_grant(&value, crate::now_ms())?;
    if cancelled.load(Ordering::Relaxed) {
        return Err("Browser sign-in cancelled.".into());
    }
    publish_challenge(
        app,
        "opencode",
        id,
        grant.challenge.clone(),
        None,
        Some(cancelled),
    )?;
    let _ = open_browser(&grant.challenge.url);
    let mut interval = grant.interval;
    loop {
        let remaining = grant
            .challenge
            .expires_at
            .unwrap_or(crate::now_ms())
            .saturating_sub(crate::now_ms())
            / 1000;
        if wait_cancelled(cancelled, interval.min(remaining.max(1))) {
            return Err("Browser sign-in cancelled.".into());
        }
        if grant
            .challenge
            .expires_at
            .is_some_and(|t| crate::now_ms() >= t)
        {
            return Err(
                "This OpenCode browser sign-in code has expired. Start sign-in again.".into(),
            );
        }
        let result = agent
            .post(&format!("{OPENCODE_CONSOLE}/auth/device/token"))
            .send_form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", grant.device_code.as_str()),
                ("client_id", OPENCODE_CLIENT),
            ]);
        let response = match result {
            Ok(response) => response,
            Err(ureq::Error::Status(400, response)) => response,
            Err(ureq::Error::Status(429, response)) => {
                interval = response
                    .header("retry-after")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(interval.saturating_add(5))
                    .max(5);
                continue;
            }
            Err(_) => {
                return Err("OpenCode browser sign-in could not finish. Retry the sign-in.".into())
            }
        };
        let value = read_auth_json(response.into_reader())?;
        if value.get("access_token").is_some() {
            let raw = parse_opencode_token(&value, None, None, crate::now_ms())?;
            if cancelled.load(Ordering::Relaxed) {
                return Err("Browser sign-in cancelled.".into());
            }
            crate::accounts::save_profile_secret(app, "opencode", id, &raw)?;
            return Ok(());
        }
        match value.get("error").and_then(|v| v.as_str()).unwrap_or("") {
            "authorization_pending" => {}
            "slow_down" => interval = interval.saturating_add(5),
            "access_denied" => {
                return Err(
                    "OpenCode browser sign-in was declined. Start sign-in again to retry.".into(),
                )
            }
            "expired_token" => {
                return Err("This OpenCode sign-in code has expired. Start sign-in again.".into())
            }
            _ => return Err("OpenCode declined this browser sign-in. Start sign-in again.".into()),
        }
    }
}
fn start_opencode(app: &AppHandle, id: &str) -> Result<(), String> {
    let guard = reserve("opencode", id)?;
    let app = app.clone();
    let id = id.to_string();
    emit(
        &app,
        "opencode",
        &id,
        true,
        "Preparing your OpenCode browser sign-in…",
    );
    std::thread::spawn(move || {
        let result = run_opencode_device(&app, &id, &guard.cancelled);
        if result.is_ok() {
            crate::accounts::login_finished(&app, "opencode", &id);
        }
        guard.finish(
            &app,
            "opencode",
            &id,
            result
                .err()
                .as_deref()
                .unwrap_or("Browser sign-in complete. Refreshing this account's usage."),
        );
    });
    Ok(())
}

/// Native OpenCode browser credentials rotate once before expiry. Network I/O
/// runs outside the account locks; a compare-and-swap protects changed profiles.
pub(crate) fn refresh_opencode_if_due(app: &AppHandle) {
    let Some(ctx) = crate::accounts::active("opencode") else {
        return;
    };
    let Some(Some(raw)) = crate::accounts::with_selection("opencode", &ctx.selection_key, || {
        crate::accounts::secret("opencode")
    }) else {
        return;
    };
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return;
    };
    if value.get("authClientID").and_then(|v| v.as_str()) != Some(OPENCODE_CLIENT) {
        return;
    }
    let Some(expires) = value.get("expires").and_then(|v| v.as_u64()) else {
        return;
    };
    if expires > crate::now_ms().saturating_add(5 * 60 * 1000) {
        return;
    }
    let Some(refresh) = value
        .get("refresh")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .map(String::from)
    else {
        return;
    };
    let Ok(guard) =
        crate::accounts::with_profile("opencode", &ctx.id, |_| reserve("opencode", &ctx.id))
    else {
        return;
    };
    if crate::accounts::with_selection("opencode", &ctx.selection_key, || {
        crate::accounts::secret("opencode") == Some(raw.clone())
    }) != Some(true)
    {
        return;
    }
    if guard.cancelled.load(Ordering::Relaxed) {
        return;
    }
    let previous_org = value
        .get("metadata")
        .and_then(|v| v.get("orgID"))
        .and_then(|v| v.as_str())
        .map(String::from);
    // Persist retirement before sending the one-use token. A process crash or a
    // timed-out response must never cause the old token to be submitted again.
    value["refresh"] = serde_json::Value::Null;
    value["renewalFailed"] = serde_json::Value::Bool(true);
    let retired = value.to_string();
    if !matches!(
        crate::accounts::replace_profile_secret_if_matches("opencode", &ctx.id, &raw, &retired),
        Ok(true)
    ) {
        return;
    }
    if guard.cancelled.load(Ordering::Relaxed) {
        return;
    }
    let response = opencode_agent()
        .post(&format!("{OPENCODE_CONSOLE}/auth/device/token"))
        .send_form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
            ("client_id", OPENCODE_CLIENT),
        ]);
    let renewed = response
        .ok()
        .and_then(|r| read_auth_json(r.into_reader()).ok())
        .and_then(|v| {
            parse_opencode_token(&v, Some(&refresh), previous_org.as_deref(), crate::now_ms()).ok()
        });
    if let Some(next) = renewed {
        let _ = crate::accounts::replace_profile_secret_if_matches(
            "opencode", &ctx.id, &retired, &next,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn isolated_commands_do_not_change_global_environment_or_accept_token_overrides() {
        let original = std::env::var_os("GH_CONFIG_DIR");
        let root = Path::new("fixture profile/O'Brien");
        let mut cmd = Command::new("fixture");
        isolate(&mut cmd, "copilot", root);
        assert_eq!(std::env::var_os("GH_CONFIG_DIR"), original);
        assert!(cmd
            .get_envs()
            .any(|(k, v)| k == "GH_CONFIG_DIR" && v == Some(root.as_os_str())));
        assert!(cmd.get_envs().any(|(k, v)| k == "GH_TOKEN" && v.is_none()));
    }
    #[test]
    fn grok_receives_its_own_official_config_path() {
        let root = Path::new("fixture");
        let mut cmd = Command::new("fixture");
        isolate(&mut cmd, "grok", root);
        assert!(cmd
            .get_envs()
            .any(|(k, v)| k == "GROK_HOME" && v == Some(root.as_os_str())));
        for key in [
            "GROK_AUTH_PATH",
            "GROK_AUTH_PROVIDER_COMMAND",
            "GROK_OAUTH2_ISSUER",
            "GROK_OAUTH2_CLIENT_ID",
            "GROK_LOCAL_AUTH",
            "XAI_API_KEY",
        ] {
            assert!(
                cmd.get_envs().any(|(k, v)| k == key && v.is_none()),
                "{key}"
            );
        }
    }

    #[test]
    fn browser_links_allow_only_official_auth_and_public_device_fields() {
        assert!(validated_browser_url(
            "codex",
            "https://auth.openai.com/authorize?state=fixture&code_challenge=public"
        )
        .is_some());
        assert!(validated_browser_url(
            "claude",
            "https://claude.com/cai/oauth/authorize?state=fixture"
        )
        .is_some());
        for url in [
            "https://github.com.evil.test/login/device",
            "https://user:pass@github.com/login/device",
            "https://github.com:444/login/device",
            "http://github.com/login/device",
            "https://github.com/login/device?access_token=fixture",
            "https://github.com/login/device?device_code=fixture",
        ] {
            assert!(validated_browser_url("copilot", url).is_none(), "{url}");
        }
        assert!(validated_browser_url(
            "opencode",
            "https://opencode.ai/console/device?user_code=ABCD-EFGH"
        )
        .is_some());
        assert!(validated_browser_url(
            "opencode",
            "https://opencode.ai/console/device?refresh_token=fixture"
        )
        .is_none());
    }
    #[test]
    fn cli_parser_extracts_the_public_device_challenge_without_token_output() {
        let mut parser = DeviceParser::default();
        assert!(parser.accept("copilot", "token: secret-fixture").is_none());
        assert!(parser
            .accept("copilot", "device_code: PRIVATE-FIXTURE")
            .is_none());
        assert!(parser
            .accept("copilot", "refresh_token_code: PRIVATE-FIXTURE")
            .is_none());
        let challenge = parser
            .accept("copilot", "First copy your one-time code: ABCD-1234")
            .unwrap();
        assert_eq!(challenge.url, "https://github.com/login/device");
        assert_eq!(challenge.user_code.as_deref(), Some("ABCD-1234"));
        let mut parser = DeviceParser::default();
        assert!(parser
            .accept("grok", "https://auth.x.ai/activate?user_code=ZXCV-1234")
            .is_some());
        for label in [
            "Confirm this code in your browser:",
            "Then enter this code:",
        ] {
            let mut parser = DeviceParser::default();
            assert!(parser
                .accept("grok", "  https://auth.x.ai/device")
                .is_none());
            assert!(parser.accept("grok", label).is_none());
            assert!(parser.accept("grok", "").is_none());
            let challenge = parser.accept("grok", "  ZXCV-1234").unwrap();
            assert_eq!(challenge.user_code.as_deref(), Some("ZXCV-1234"));
        }
    }
    #[test]
    fn native_device_response_binds_public_url_and_code_and_private_grant() {
        let fixture = serde_json::json!({"device_code":"private-fixture","user_code":"ABCD-1234","verification_uri_complete":"/console/device?user_code=ABCD-1234","expires_in":600,"interval":5});
        let grant = parse_device_grant(&fixture, 1000).unwrap();
        assert_eq!(grant.device_code, "private-fixture");
        assert_eq!(grant.challenge.expires_at, Some(601000));
        let mut slower = fixture.clone();
        slower["interval"] = serde_json::json!(120);
        assert_eq!(parse_device_grant(&slower, 0).unwrap().interval, 120);
        let mut mismatch = fixture.clone();
        mismatch["verification_uri_complete"] =
            serde_json::json!("/console/device?user_code=WXYZ-5678");
        assert!(parse_device_grant(&mismatch, 0).is_err());
        mismatch["verification_uri_complete"] = serde_json::json!("https://evil.test/device");
        assert!(parse_device_grant(&mismatch, 0).is_err());
    }
    #[test]
    fn renewal_requires_rotation_and_keeps_the_approved_workspace() {
        let token = serde_json::json!({"access_token":"access-fixture","refresh_token":"renew-fixture","expires_in":3600,"org_id":"org-fixture"});
        let raw = parse_opencode_token(&token, None, None, 1000).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["expires"], 3601000u64);
        assert_eq!(value["authClientID"], "codenotch");
        assert!(
            parse_opencode_token(&token, Some("renew-fixture"), Some("org-fixture"), 0).is_err()
        );
        assert!(parse_opencode_token(&token, Some("old-fixture"), Some("other-org"), 0).is_err());
        let mut rotated = token.clone();
        rotated.as_object_mut().unwrap().remove("org_id");
        let value: serde_json::Value = serde_json::from_str(
            &parse_opencode_token(&rotated, Some("old-fixture"), Some("org-fixture"), 0).unwrap(),
        )
        .unwrap();
        assert_eq!(value["metadata"]["orgID"], "org-fixture");
    }
    #[test]
    fn auth_responses_are_bounded_before_json_parsing() {
        assert!(
            read_auth_json(std::io::Cursor::new(vec![b' '; 256 * 1024 + 1]))
                .unwrap_err()
                .contains("too large")
        );
        assert_eq!(
            read_auth_json(std::io::Cursor::new(br#"{"ok":true}"#)).unwrap()["ok"],
            true
        );
    }
    #[test]
    fn already_cancelled_poll_does_not_wait_for_the_next_provider_interval() {
        assert!(wait_cancelled(&AtomicBool::new(true), 900));
    }
    #[test]
    fn concurrent_auth_for_the_same_profile_is_rejected_until_guard_release() {
        let guard = reserve("opencode", "test-reservation").unwrap();
        assert!(reserve("opencode", "test-reservation").is_err());
        drop(guard);
        assert!(reserve("opencode", "test-reservation").is_ok());
    }
}
