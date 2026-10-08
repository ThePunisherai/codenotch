//! User-initiated sign-in through the standalone Claude Code CLI. OAuth stays in
//! the CLI. A hidden helper opens the real browser flow; only validated authorization
//! links and status enter widget IPC, never bearer/refresh tokens or credentials.
use serde::Serialize;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter};
use std::{process::{Child, ChildStdin, Command, Stdio}, sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex}, time::{Duration, Instant}};

static BUSY: AtomicBool = AtomicBool::new(false);
static MESSAGE: Mutex<String> = Mutex::new(String::new());

/// The lock guards one short line of status text. A panic while it is held would
/// poison it and take the whole card down with `unwrap`, which is a steep price
/// for a string nobody has to trust — so take it back and carry on.
fn message() -> std::sync::MutexGuard<'static, String> {
    MESSAGE.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Serialize)]
pub struct AuthState { pub busy: bool, pub message: String }

pub fn state() -> AuthState {
    AuthState { busy: BUSY.load(Ordering::Acquire), message: message().clone() }
}

/// Shared with background renewal so the two native clients cannot rotate the
/// same credential at once. Drop also releases the gate on spawn/error paths.
pub struct AuthGuard;
pub fn try_acquire() -> Option<AuthGuard> {
    BUSY.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).ok().map(|_| AuthGuard)
}
impl Drop for AuthGuard {
    fn drop(&mut self) { BUSY.store(false, Ordering::Release); }
}

pub fn usage_succeeded() {
    if !BUSY.load(Ordering::Acquire) { message().clear(); }
}

// Run the native official client directly: its callback and manual-code stdin
// belong to this process, with no terminal or intermediate shell.
fn login_command(cli: &Path, profile_root: &Path) -> Result<Command, String> {
    #[cfg(windows)]
    if !cli.extension().and_then(|s| s.to_str()).is_some_and(|s| s.eq_ignore_ascii_case("exe")) {
        return Err("Install the current native Claude Code CLI to use secure browser sign-in.".into());
    }
    let mut cmd = Command::new(cli);
    cmd.args(["auth", "login", "--claudeai"]);
    cmd.current_dir(profile_root);
    for (key, _) in std::env::vars_os() {
        let k = key.to_string_lossy();
        if k == "CLAUDECODE" || k.starts_with("CLAUDE_CODE_") { cmd.env_remove(&key); }
    }
    cmd.env("CLAUDE_CONFIG_DIR", profile_root)
        .env_remove("ANTHROPIC_API_KEY").env_remove("ANTHROPIC_AUTH_TOKEN").env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .env("NO_COLOR", "1")
        // Keep stdin open: Claude's documented code fallback belongs to this same OAuth flow.
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)] { use std::os::windows::process::CommandExt; cmd.creation_flags(0x0800_0000); }
    Ok(cmd)
}

struct PendingLogin { profile_id: String, cancelled: Arc<AtomicBool>, active: Arc<AtomicBool>, input: Option<ChildStdin> }
static PENDING: Mutex<Option<PendingLogin>> = Mutex::new(None);

fn pending(profile_id: &str, child: &mut Child, active: Arc<AtomicBool>) -> Arc<AtomicBool> {
    let cancelled = Arc::new(AtomicBool::new(false));
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(PendingLogin {
        profile_id: profile_id.into(), cancelled: cancelled.clone(), active, input: child.stdin.take(),
    });
    cancelled
}
fn finish_pending(profile_id: &str) {
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    if pending.as_ref().is_some_and(|p| p.profile_id == profile_id) { pending.take(); }
}

pub fn cancel_login(profile_id: &str) -> Result<(), String> {
    cancel_login_inner(None, profile_id)
}

pub fn cancel_profile_login(app: &AppHandle, profile_id: &str) -> Result<(), String> {
    cancel_login_inner(Some(app), profile_id)
}

fn cancel_login_inner(app: Option<&AppHandle>, profile_id: &str) -> Result<(), String> {
    let pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let login = pending.as_ref().filter(|p| p.profile_id == profile_id).ok_or("No browser sign-in is running for this account.")?;
    login.cancelled.store(true, Ordering::Release);
    login.active.store(false, Ordering::Release);
    if let Some(app) = app {
        // Finish removes PENDING under this same lock before publishing its final
        // result. The cancelling event therefore cannot overtake completion.
        crate::profile_auth::clear_browser_challenge("claude", profile_id);
        emit_profile_login(app, "claude", profile_id, true, "Cancelling browser sign-in…");
    }
    Ok(())
}

pub fn cancel_all_logins() {
    let running = {
        let pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(login) = pending.as_ref() {
            login.active.store(false, Ordering::Release);
            login.cancelled.store(true, Ordering::Release);
            true
        } else { false }
    };
    // Give the owner time to kill/reap its helper during an ordinary application
    // exit. No browser process is part of this cleanup.
    if running {
        let deadline = Instant::now() + Duration::from_secs(1);
        while BUSY.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Claude may show a one-use CODE#STATE when the browser cannot reach localhost.
/// Write it to the original hidden process only; never store, echo or log it.
pub fn submit_code(profile_id: &str, code: &str) -> Result<(), String> {
    use std::io::Write;
    let code = code.trim();
    if !valid_sign_in_code(code) { return Err("Paste the complete sign-in code shown in the browser, including # and its state.".into()) }
    let (mut input, session) = {
        let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        let login = pending.as_mut().filter(|p| p.profile_id == profile_id && !p.cancelled.load(Ordering::Acquire) && p.active.load(Ordering::Acquire))
            .ok_or("No browser sign-in is waiting for this account.")?;
        let input = login.input.take().ok_or("A sign-in code is already being sent. Wait for the browser result.")?;
        (input, login.cancelled.clone())
    };
    // The native command may stop reading. Never hold the cancellation mutex while a pipe write waits.
    let result = input.write_all(code.as_bytes()).and_then(|_| input.write_all(b"\n")).and_then(|_| input.flush())
        .map_err(|_| "The browser sign-in has ended. Start a new sign-in.".to_owned());
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(login) = pending.as_mut().filter(|p| p.profile_id == profile_id && Arc::ptr_eq(&p.cancelled, &session)
        && !p.cancelled.load(Ordering::Acquire)) { login.input = Some(input); }
    result
}

fn valid_sign_in_code(code: &str) -> bool {
    code.len() <= 8192 && code.split_once('#').is_some_and(|(a, b)| !a.is_empty() && !b.is_empty() && !b.contains('#'))
        && code.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.~+/=#".contains(&b))
}

/// Credential change hint only; it contains file metadata and no OAuth bytes.
pub(crate) fn credential_stamp(root: &Path, names: &[&str]) -> Vec<Option<(std::time::SystemTime, u64)>> {
    names.iter().map(|name| std::fs::metadata(root.join(name)).ok()
        .and_then(|m| Some((m.modified().ok()?, m.len())))).collect()
}

pub(crate) fn wait_child_cancel(child: &mut Child, timeout: Duration, cancelled: &AtomicBool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline && !cancelled.load(Ordering::Acquire) => std::thread::sleep(Duration::from_millis(100)),
            // Browser authentication runs the native client directly. Kill only
            // that helper: a browser launched by it belongs to the user.
            _ => { let _ = child.kill(); let _ = child.wait(); return false; }
        }
    }
}

/// Drain auth output without retaining it. Only native authorization links are
/// offered for re-opening; token/error bodies are discarded, including on failures.
pub(crate) fn watch_browser_output(app: &AppHandle, provider: &str, profile_id: &str, child: &mut Child, active: Arc<AtomicBool>) {
    fn watch<R: AuthPipe>(output: R, app: AppHandle, provider: String, id: String, active: Arc<AtomicBool>) {
        let attempt = active.clone();
        drain_output(output, active, move |line| {
            if let Some(url) = authorization_link(&String::from_utf8_lossy(line), &provider) {
                // The shared challenge lock checks this attempt's flag immediately
                // before storage and emission, so completed attempts cannot revive a URL.
                let _ = crate::profile_auth::set_browser_challenge_if_active(&app, &provider, &id, &url, None, &attempt);
            }
        });
    }
    if let Some(output) = child.stdout.take() { watch(output, app.clone(), provider.into(), profile_id.into(), active.clone()); }
    if let Some(output) = child.stderr.take() { watch(output, app.clone(), provider.into(), profile_id.into(), active); }
}

#[cfg(windows)]
trait AuthPipe: std::io::Read + std::os::windows::io::AsRawHandle + Send + 'static {}
#[cfg(windows)]
impl<T: std::io::Read + std::os::windows::io::AsRawHandle + Send + 'static> AuthPipe for T {}
#[cfg(unix)]
trait AuthPipe: std::io::Read + std::os::fd::AsRawFd + Send + 'static {}
#[cfg(unix)]
impl<T: std::io::Read + std::os::fd::AsRawFd + Send + 'static> AuthPipe for T {}

/// Anonymous pipes may outlive the CLI when a launched browser inherits a handle.
/// Poll before reading, so completion/cancellation always releases readers promptly.
fn read_available<R: AuthPipe>(output: &mut R, bytes: &mut [u8]) -> std::io::Result<Option<usize>> {
    #[cfg(windows)] {
        use windows::Win32::{Foundation::HANDLE, System::Pipes::PeekNamedPipe};
        let mut available = 0u32;
        let ok = unsafe { PeekNamedPipe(HANDLE(output.as_raw_handle()), None, 0, None, Some(&mut available), None) };
        if ok.is_err() { return Ok(Some(0)); }
        if available == 0 { std::thread::sleep(Duration::from_millis(100)); return Ok(None); }
        let length = bytes.len().min(available as usize);
        output.read(&mut bytes[..length]).map(Some)
    }
    #[cfg(unix)] {
        let mut pipe = libc::pollfd { fd: output.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let result = unsafe { libc::poll(&mut pipe, 1, 100) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted { return Ok(None); }
            return Err(error);
        }
        if result == 0 { return Ok(None); }
        output.read(bytes).map(Some)
    }
}

fn drain_output<R: AuthPipe>(mut output: R, active: Arc<AtomicBool>, mut on_line: impl FnMut(&[u8]) + Send + 'static) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut line = Vec::with_capacity(4096);
        let mut bytes = [0; 1024];
        let mut too_long = false;
        while active.load(Ordering::Acquire) {
            let n = match read_available(&mut output, &mut bytes) {
                Ok(None) => continue,
                Ok(Some(0)) | Err(_) => break,
                Ok(Some(n)) => n,
            };
            for b in &bytes[..n] {
                if *b == b'\n' {
                    if !too_long && active.load(Ordering::Acquire) { on_line(&line); }
                    line.clear(); too_long = false;
                } else if line.len() < 16 * 1024 && !too_long { line.push(*b); }
                else { line.clear(); too_long = true; }
            }
        }
    })
}

fn authorization_link(line: &str, provider: &str) -> Option<String> {
    let prefixes: &[&str] = if provider == "codex" { &["https://auth.openai.com/authorize?"] }
        else if provider == "claude" { &["https://claude.com/cai/oauth/authorize?", "https://claude.ai/oauth/authorize?"] } else { return None; };
    line.split_ascii_whitespace().find_map(|word| {
        let start = word.find("https://")?;
        let url = word[start..].split('\u{1b}').next()?.trim_end_matches(|c| matches!(c, '\'' | '"' | '>' | ')'));
        (url.len() <= 16 * 1024 && prefixes.iter().any(|p| url.starts_with(p))).then(|| url.to_owned())
    })
}

pub(crate) fn terminate(child: &mut Child) {
    #[cfg(windows)] {
        use std::os::windows::process::CommandExt;
        use std::process::Stdio;
        if let Some(root) = std::env::var_os("SystemRoot") {
            // A .cmd CLI may have node children: terminate only this owned tree.
            let _ = Command::new(std::path::PathBuf::from(root).join("System32/taskkill.exe"))
                .args(["/PID", &child.id().to_string(), "/T", "/F"])
                .creation_flags(0x0800_0000).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub(crate) fn wait_child(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(250)),
            _ => { terminate(child); return false; }
        }
    }
}

pub fn start_login() -> Result<(), String> {
    let cli = crate::usage::find_browser_cli().ok_or("Native Claude Code CLI not found. Install the current standalone CLI for browser sign-in.")?;
    let guard = try_acquire().ok_or("Claude sign-in or renewal is already running.")?;
    let root = crate::accounts::active("claude").map(|p| p.root)
        .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR").filter(|p| !p.is_empty()).map(PathBuf::from))
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude"))).ok_or("Home directory unavailable.")?;
    std::fs::create_dir_all(&root).map_err(|_| "Unable to create the account directory.")?;
    let mut cmd = login_command(&cli, &root)?;
    let mut child = cmd.spawn().map_err(|_| "Unable to start secure Claude browser sign-in.")?;
    let active = Arc::new(AtomicBool::new(true));
    let cancelled = pending("legacy", &mut child, active.clone());
    // Discard native output; this compatibility entry point has no settings account handle.
    if let Some(output) = child.stdout.take() { drain_output(output, active.clone(), |_| {}); }
    if let Some(output) = child.stderr.take() { drain_output(output, active.clone(), |_| {}); }
    *message() = "Opening browser. Complete sign-in there.".into();
    std::thread::spawn(move || {
        let ok = wait_child_cancel(&mut child, Duration::from_secs(15 * 60), &cancelled);
        active.store(false, Ordering::Release);
        finish_pending("legacy");
        *message() = if ok { "Sign-in complete. Refreshing usage..." } else {
            "Sign-in cancelled, failed or timed out. Try again."
        }.into();
        drop(guard);
        crate::usage::request_refresh();
    });
    Ok(())
}

/// Sign in to one isolated account. The standalone CLI writes its own credential;
/// only a busy flag and human-readable result are sent to the settings window.
pub fn login_profile(app: &AppHandle, profile_id: &str, root: PathBuf) -> Result<(), String> {
    let cli = crate::usage::find_browser_cli().ok_or("Native Claude Code CLI not found. Install the current standalone CLI for browser sign-in.")?;
    std::fs::create_dir_all(&root).map_err(|_| "Unable to create the account directory.")?;
    let guard = try_acquire().ok_or("Claude sign-in or renewal is already running.")?;
    let before = credential_stamp(&root, &[".credentials.json", "credentials.json"]);
    let mut cmd = login_command(&cli, &root)?;
    let mut child = cmd.spawn().map_err(|_| "Unable to start secure Claude browser sign-in.")?;
    let app = app.clone();
    let profile_id = profile_id.to_owned();
    let active = Arc::new(AtomicBool::new(true));
    let cancelled = pending(&profile_id, &mut child, active.clone());
    *message() = "Opening browser. Complete sign-in there. If it shows a code, paste it in this account's sign-in panel.".into();
    emit_profile_login(&app, "claude", &profile_id, true, &message());
    watch_browser_output(&app, "claude", &profile_id, &mut child, active.clone());
    std::thread::spawn(move || {
        let ok = wait_child_cancel(&mut child, Duration::from_secs(15 * 60), &cancelled);
        active.store(false, Ordering::Release);
        crate::profile_auth::clear_browser_challenge("claude", &profile_id);
        finish_pending(&profile_id);
        let signed_in = ok && crate::usage::profile_has_credential(&root);
        let note = if signed_in { "Sign-in complete. Refreshing usage..." } else if cancelled.load(Ordering::Acquire) {
            "Browser sign-in cancelled."
        } else { "Browser sign-in failed or expired. Update Claude Code and try again." };
        *message() = note.into();
        // Even cancellation can follow a native credential write. Invalidate old
        // quota whenever the CLI changed/deleted its file, without claiming login succeeded.
        if signed_in || before != credential_stamp(&root, &[".credentials.json", "credentials.json"]) {
            crate::accounts::login_finished(&app, "claude", &profile_id);
        }
        emit_profile_login(&app, "claude", &profile_id, false, note);
        drop(guard);
        crate::usage::request_refresh();
    });
    Ok(())
}

pub(crate) fn emit_profile_login(app: &AppHandle, provider: &str, profile_id: &str, busy: bool, note: &str) {
    crate::profile_auth::emit_profile_status(app, provider, profile_id, busy, note);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gate_excludes_login_and_renewal_and_releases_on_drop() {
        let guard = try_acquire().unwrap();
        assert!(try_acquire().is_none());
        assert!(state().busy);
        drop(guard);
        assert!(!state().busy);
        assert!(try_acquire().is_some());
    }
    #[test]
    fn paths_are_data_not_shell_source() {
        let path = std::path::Path::new(r"C:\fixture with spaces\O'Brien\claude.exe");
        let profile = std::path::Path::new(r"C:\fixture with spaces\Claude profile");
        let cmd = login_command(path, profile).unwrap();
        assert!(cmd.get_envs().any(|(k,v)| k == "CLAUDE_CONFIG_DIR" && v == Some(profile.as_os_str())));
        assert_eq!(cmd.get_program(), path.as_os_str());
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["auth", "login", "--claudeai"]);
        assert_eq!(cmd.get_current_dir(), Some(profile));
        for key in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN"] {
            assert!(cmd.get_envs().any(|(k,v)| k == key && v.is_none()));
        }
    }
    #[test]
    fn only_the_native_providers_authorization_pages_are_captured() {
        let openai = "https://auth.openai.com/authorize?client_id=fixture&state=public-state&code_challenge=public-proof";
        assert_eq!(authorization_link(&format!("Open browser: {openai}\u{1b}[0m"), "codex"), Some(openai.into()));
        for url in ["https://claude.com/cai/oauth/authorize?state=public-state", "https://claude.ai/oauth/authorize?state=public-state"] {
            assert_eq!(authorization_link(url, "claude"), Some(url.into()));
            assert!(authorization_link(url, "codex").is_none());
        }
        for url in [
            "https://auth.openai.com.evil.test/authorize?state=x",
            "http://auth.openai.com/authorize?state=x",
            "http://localhost:1455/auth/callback?code=private-code",
            "https://claude.com/cai/oauth/authorize/callback?code=private-code",
            "https://platform.claude.com/oauth/code/callback?code=private-code",
        ] {
            assert!(authorization_link(url, "codex").is_none());
            assert!(authorization_link(url, "claude").is_none());
        }
        assert!(authorization_link(&format!("{openai}&padding={}", "a".repeat(16 * 1024)), "codex").is_none());
    }
    #[test]
    fn manual_sign_in_code_is_one_line_and_includes_its_state() {
        assert!(valid_sign_in_code("native-code_1#native-state_2"));
        for input in ["", "code", "#state", "code#", "code#state#extra", "code#state\n", "code#state\r", "code#state other", "code#state\0", "code#€"] {
            assert!(!valid_sign_in_code(input));
        }
        assert!(!valid_sign_in_code(&format!("{}#state", "a".repeat(8192))));
    }
    #[test]
    fn idle_output_reader_releases_the_pipe_when_attempt_finishes() {
        #[cfg(unix)]
        let mut command = { let mut c = Command::new("/bin/sleep"); c.arg("5"); c };
        #[cfg(windows)]
        let mut command = {
            use std::os::windows::process::CommandExt;
            let mut c = Command::new(PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/ping.exe"));
            c.args(["-n", "30", "127.0.0.1"]).creation_flags(0x0800_0000); c
        };
        let mut child = command.stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let active = Arc::new(AtomicBool::new(true));
        let reader = drain_output(child.stdout.take().unwrap(), active.clone(), |_| {});
        std::thread::sleep(Duration::from_millis(120));
        let start = Instant::now();
        active.store(false, Ordering::Release);
        reader.join().unwrap();
        let elapsed = start.elapsed();
        let _ = child.kill(); let _ = child.wait();
        assert!(elapsed < Duration::from_millis(500), "idle auth pipe retained its reader after completion");
    }
    #[test]
    #[cfg(unix)]
    fn cancelled_native_helper_is_killed_and_reaped_promptly() {
        let mut child = Command::new("/bin/sleep").arg("5").stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let start = Instant::now();
        assert!(!wait_child_cancel(&mut child, Duration::from_secs(30), &AtomicBool::new(true)));
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(child.try_wait().unwrap().is_some());
    }
    #[test]
    #[cfg(windows)]
    fn shell_shims_cannot_launch_browser_authentication() {
        assert!(login_command(Path::new(r"C:\npm\claude.cmd"), Path::new(r"C:\profiles\work")).is_err());
    }
    #[test]
    #[cfg(windows)]
    fn failed_exit_and_timeout_are_reaped() {
        use std::os::windows::process::CommandExt;
        let root = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap());
        let where_exe = root.join("System32/where.exe");
        let mut child = Command::new(&where_exe)
            .arg("codenotch-no-such-command-394.exe").creation_flags(0x0800_0000)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        assert!(!wait_child(&mut child, Duration::from_secs(5)));
        let mut child = Command::new(&where_exe)
            .arg("cmd.exe").creation_flags(0x0800_0000)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        assert!(wait_child(&mut child, Duration::from_secs(5)));
        let mut child = Command::new(root.join("System32/ping.exe"))
            .args(["-n", "30", "127.0.0.1"]).creation_flags(0x0800_0000)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        let start = Instant::now();
        assert!(!wait_child(&mut child, Duration::from_millis(300)));
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(child.try_wait().unwrap().is_some());
    }
}
