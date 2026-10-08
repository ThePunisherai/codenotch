//! Official interactive sign-in processes, launched only from an account button.
//! Every child receives its own config environment; the app never changes HOME.
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter};
static BUSY: Mutex<Option<HashSet<String>>> = Mutex::new(None);
fn busy_key(provider: &str, id: &str) -> String {
    format!("{provider}:{id}")
}
pub fn is_busy(provider: &str, id: &str) -> bool {
    match crate::accounts::canonical_provider(provider) {
        "codex" => crate::codex::login_busy(),
        "claude" => crate::claude_auth::state().busy,
        p => BUSY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|s| s.contains(&busy_key(p, id))),
    }
}
fn find_cli(name: &str) -> Option<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(p) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    if let Some(p) = dirs::config_dir() {
        dirs.push(p.join("npm"));
    }
    if let Some(p) = dirs::data_local_dir() {
        dirs.push(p.join("Microsoft/WinGet/Links"));
        dirs.push(p.join("Programs/opencode"));
    }
    if let Some(h) = dirs::home_dir() {
        dirs.push(h.join(".opencode/bin"));
        dirs.push(h.join("scoop/shims"));
    }
    dirs.into_iter()
        .flat_map(|d| {
            crate::usage::command_names(name)
                .into_iter()
                .map(move |n| d.join(n))
        })
        .find(|p| p.is_file())
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
        "codex" => ("cli", crate::codex::find_executable().is_some(), "Browser sign-in through Codex CLI, isolated from your existing login. ChatGPT account quota requires browser sign-in."),
        "claude" => ("cli", crate::usage::find_cli().is_some(), "Browser sign-in through Claude Code CLI in an isolated profile."),
        "copilot" => ("cli", crate::copilot::find_executable().is_some(), "Sign in with GitHub CLI. A saved GitHub token can also connect this profile."),
        "grok" => ("cli", crate::grok::find_cli().is_some(), "Browser sign-in through Grok CLI using this profile's GROK_HOME."),
        "opencode" => ("cli", find_cli("opencode").is_some(), "Sign in with OpenCode CLI and choose OpenCode. You can also connect an OpenCode API key."),
        "glm" => ("api_key", true, "Connect a Z.ai Coding Plan API key. Detected BigModel accounts keep their original console."),
        "cursor" => ("desktop", desktop_cli("cursor").is_some(), "Sign in inside Cursor, then choose Detect accounts. Repeat after changing the desktop login to save another account."),
        "antigravity" => ("desktop", desktop_cli("antigravity").is_some(), "Sign in inside Antigravity, then choose Detect accounts. Saved accounts use their captured session; the desktop bridge only belongs to its current login."),
        _ => ("desktop", false, "Unsupported provider."),
    }
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
            let exe = desktop_cli(provider).ok_or(
                "Desktop app not found. Sign in in the installed app, then choose Detect accounts.",
            )?;
            Command::new(exe)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|_| "Unable to open the provider app.")?;
            emit(&app, provider, &account_id, false, "Complete sign-in in the provider app, then choose Detect accounts to save its current session.");
            Ok(())
        }
        "copilot" | "grok" | "opencode" => start_cli(&app, provider, &account_id, ctx.root),
        "glm" => Err("Use Connect key to add this account's API key.".into()),
        _ => Err("Unsupported provider.".into()),
    })
}
fn emit(app: &AppHandle, provider: &str, id: &str, busy: bool, note: &str) {
    let provider = if provider == "antigravity" {
        "gemini"
    } else {
        provider
    };
    let _ = app.emit(
        "provider-account-login",
        serde_json::json!({"provider":provider,"profile_id":id,"busy":busy,"note":note}),
    );
}
#[cfg(windows)]
fn login_command(provider: &str, cli: &Path, root: &Path) -> Result<Command, String> {
    // CLI paths travel as environment data. These scripts contain no account labels,
    // tokens, user paths, or other interpolated shell text.
    let script = match provider {
        "copilot" => "$Host.UI.RawUI.WindowTitle='Codenotch - GitHub sign-in'; & $env:CODENOTCH_AUTH_CLI auth login --web --hostname github.com --git-protocol https --skip-ssh-key --insecure-storage; exit $LASTEXITCODE",
        "grok" => "$Host.UI.RawUI.WindowTitle='Codenotch - Grok sign-in'; & $env:CODENOTCH_AUTH_CLI login; exit $LASTEXITCODE",
        "opencode" => "$Host.UI.RawUI.WindowTitle='Codenotch - OpenCode sign-in'; Write-Host 'Choose OpenCode to track your OpenCode quota.'; & $env:CODENOTCH_AUTH_CLI auth login; exit $LASTEXITCODE",
        _ => return Err("Unsupported sign-in provider.".into()),
    };
    let system = std::env::var_os("SystemRoot").ok_or("Windows directory unavailable.")?;
    let mut cmd =
        Command::new(PathBuf::from(system).join("System32/WindowsPowerShell/v1.0/powershell.exe"));
    cmd.args(["-NoLogo", "-NoProfile", "-Command", script])
        .env("CODENOTCH_AUTH_CLI", cli);
    isolate(&mut cmd, provider, root);
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0000_0010);
    Ok(cmd)
}
#[cfg(not(windows))]
fn login_command(provider: &str, cli: &Path, root: &Path) -> Result<Command, String> {
    let term = find_cli("x-terminal-emulator")
        .or_else(|| find_cli("xterm"))
        .ok_or("No terminal emulator found. Use the Windows build for native sign-in.")?;
    let script = match provider {
        "copilot" => "\"$CODENOTCH_AUTH_CLI\" auth login --web --hostname github.com --git-protocol https --skip-ssh-key --insecure-storage",
        "grok" => "\"$CODENOTCH_AUTH_CLI\" login",
        "opencode" => "echo 'Choose OpenCode to track your OpenCode quota.'; \"$CODENOTCH_AUTH_CLI\" auth login",
        _ => return Err("Unsupported sign-in provider.".into()),
    };
    let mut cmd = Command::new(term);
    cmd.args(["-e", "sh", "-c", script])
        .env("CODENOTCH_AUTH_CLI", cli);
    isolate(&mut cmd, provider, root);
    Ok(cmd)
}
fn isolate(cmd: &mut Command, provider: &str, root: &Path) {
    cmd.current_dir(root);
    for key in [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        "GROK_HOME",
        "XAI_API_KEY",
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
            cmd.env("GH_CONFIG_DIR", root);
        }
        "grok" => {
            cmd.env("GROK_HOME", root);
        }
        "opencode" => {
            cmd.env("XDG_DATA_HOME", root.join("data"))
                .env("XDG_CONFIG_HOME", root.join("config"));
        }
        _ => {}
    }
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
fn wait(child: &mut Child) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15 * 60);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(250)),
            _ => {
                terminate(child);
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
            let token = token?;
            Some((
                serde_json::json!({"token":token,"username":username}).to_string(),
                path,
            ))
        }
        "opencode" => {
            let raw = crate::opencode::capture_credential_in(root)?;
            let json = root.join("data/opencode/auth.json");
            let path = if json.is_file() {
                json
            } else {
                root.join("data/opencode/opencode.db")
            };
            Some((raw, path))
        }
        // GROK_HOME is an official isolated OAuth store; retaining it lets the
        // Grok CLI renew only this account rather than the desktop's session.
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
    let cli = match provider {
        "copilot" => crate::copilot::find_executable(),
        "grok" => crate::grok::find_cli(),
        "opencode" => find_cli("opencode"),
        _ => None,
    }
    .ok_or("Official provider CLI not found. Install it first, or connect a supported API key.")?;
    let key = busy_key(provider, id);
    {
        let mut busy = BUSY.lock().unwrap_or_else(|e| e.into_inner());
        let set = busy.get_or_insert_with(HashSet::new);
        if !set.insert(key.clone()) {
            return Err("Sign-in for this account is already running.".into());
        }
    }
    let child = login_command(provider, &cli, &root).and_then(|mut cmd| {
        cmd.spawn()
            .map_err(|_| "Unable to open the provider sign-in window.".into())
    });
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            if let Some(busy) = BUSY.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                busy.remove(&key);
            }
            return Err(e);
        }
    };
    let app = app.clone();
    let provider = provider.to_string();
    let id = id.to_string();
    emit(
        &app,
        &provider,
        &id,
        true,
        "Complete sign-in in the browser and terminal window.",
    );
    std::thread::spawn(move || {
        let success = wait(&mut child);
        let result = if success {
            capture_owned(&provider, &root)
                .ok_or("Sign-in completed without a supported account credential.")
                .and_then(|(raw, path)| {
                    if provider != "grok" {
                        crate::accounts::save_profile_secret(&app, &provider, &id, &raw)
                            .map_err(|_| "Unable to protect the signed-in account.")?;
                        // GH uses --insecure-storage only in this isolated directory;
                        // after DPAPI succeeds, remove the temporary plaintext token.
                        let mut cleanup = vec![path];
                        if provider == "opencode" {
                            for name in ["auth.json","opencode.db","opencode.db-wal","opencode.db-shm"] {
                                cleanup.push(root.join("data/opencode").join(name));
                                cleanup.push(root.join(name));
                            }
                        }
                        cleanup.sort(); cleanup.dedup();
                        for path in cleanup {
                            if path.is_file() { std::fs::remove_file(path).map_err(|_| "Account saved, but a temporary credential file could not be deleted.")?; }
                        }
                    }
                    Ok(())
                })
        } else {
            Err("Sign-in cancelled, failed or timed out.")
        };
        if let Some(busy) = BUSY.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            busy.remove(&key);
        }
        if result.is_ok() {
            crate::accounts::login_finished(&app, &provider, &id);
        }
        emit(
            &app,
            &provider,
            &id,
            false,
            result
                .err()
                .unwrap_or("Sign-in complete. Refreshing this account's usage."),
        );
    });
    Ok(())
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
    fn open_code_and_grok_receive_separate_official_config_paths() {
        let root = Path::new("fixture");
        let mut cmd = Command::new("fixture");
        isolate(&mut cmd, "opencode", root);
        assert!(cmd
            .get_envs()
            .any(|(k, v)| k == "XDG_DATA_HOME" && v == Some(root.join("data").as_os_str())));
        let mut cmd = Command::new("fixture");
        isolate(&mut cmd, "grok", root);
        assert!(cmd
            .get_envs()
            .any(|(k, v)| k == "GROK_HOME" && v == Some(root.as_os_str())));
    }
}
