//! Background updates use Tauri's signed artifacts. A portable copy never
//! launches an installer; it can still check releases and open their download.

use semver::Version;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

const RELEASE_API: &str = "https://api.github.com/repos/ThePunisherai/codenotch/releases/latest";
const INSTALLER_NAME: &str = "Codenotch-Setup.exe";
const UNSET_PUBKEY: &str = "REPLACE_WITH_TAURI_PUBLIC_KEY";
const CHECK_ERROR: &str = "Could not check for updates";
const INSTALL_ERROR: &str = "Could not install the update";
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const LAUNCH_DELAY: Duration = Duration::from_secs(20);
const NETWORK_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RELEASE_BYTES: usize = 256 * 1024;

#[derive(Clone, Serialize, Default)]
pub struct UpdateState {
    pub available: Option<String>,
    pub checking: bool,
    pub installing: bool,
    /// An unchecked build must not claim to be up to date.
    pub checked: bool,
    /// A configured Tauri feed offered an artifact; bytes are verified before installation.
    pub can_install: bool,
    pub message: Option<String>,
    pub auto_update: bool,
    pub portable: bool,
    /// A public key is compiled into this build; its feed may still be unavailable.
    pub signing_ready: bool,
    pub checked_at: Option<u64>,
    pub next_check_at: Option<u64>,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
}

static STATE: Mutex<Option<UpdateState>> = Mutex::new(None);
/// One parked timer; no polling loop or retained HTTP client. Threads keep the
/// standard stack reserve: Windows commits pages as needed, while native TLS,
/// the async runtime and event delivery retain their normal stack headroom.
static SCHEDULE: (Mutex<Option<Instant>>, Condvar) = (Mutex::new(None), Condvar::new());

fn state() -> std::sync::MutexGuard<'static, Option<UpdateState>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn configured(app: &AppHandle) -> bool {
    app.config()
        .plugins
        .0
        .get("updater")
        .and_then(|u| u.get("pubkey"))
        .and_then(|k| k.as_str())
        .is_some_and(|k| !k.is_empty() && k != UNSET_PUBKEY)
}

/// Only the NSIS installer writes this marker. Absence is conservative:
/// running a copied executable must not silently install an app.
fn portable_at(executable: &Path) -> bool {
    let Some(directory) = executable.parent() else {
        return true;
    };
    directory.join("codenotch-portable.marker").exists()
        || !directory.join(".codenotch-installed").is_file()
}

fn portable() -> bool {
    std::env::current_exe().map_or(true, |path| portable_at(&path))
}

fn automatic_enabled(app: &AppHandle) -> bool {
    app.state::<crate::AppState>()
        .cfg
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .auto_update
}

fn with_policy(app: &AppHandle, mut next: UpdateState) -> UpdateState {
    next.auto_update = automatic_enabled(app);
    next.portable = portable();
    next.signing_ready = configured(app);
    next.can_install &= next.signing_ready && !next.portable;
    next.next_check_at = SCHEDULE
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|deadline| {
            now_ms().saturating_add(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis() as u64,
            )
        });
    next
}

fn set(app: &AppHandle, next: UpdateState) {
    let next = with_policy(app, next);
    *state() = Some(next.clone());
    let _ = app.emit("update_state", &next);
}

/// A preference change must not overwrite a worker's fresh busy/offer state.
fn refresh_policy(app: &AppHandle) -> UpdateState {
    let policy = with_policy(app, UpdateState::default());
    let next = {
        let mut stored = state();
        let current = stored.get_or_insert_with(UpdateState::default);
        merge_policy(current, &policy);
        current.clone()
    };
    let _ = app.emit("update_state", &next);
    next
}

fn merge_policy(current: &mut UpdateState, policy: &UpdateState) {
    current.auto_update = policy.auto_update;
    current.portable = policy.portable;
    current.signing_ready = policy.signing_ready;
    current.next_check_at = policy.next_check_at;
    current.can_install &= policy.signing_ready && !policy.portable;
}

#[tauri::command]
pub fn get_update_state(app: AppHandle) -> UpdateState {
    let current = state().clone().unwrap_or_default();
    with_policy(&app, current)
}

fn schedule(enabled: bool, delay: Duration) {
    *SCHEDULE.0.lock().unwrap_or_else(|e| e.into_inner()) = enabled.then(|| Instant::now() + delay);
    SCHEDULE.1.notify_one();
}

#[tauri::command]
pub fn set_auto_update(app: AppHandle, on: bool) -> UpdateState {
    {
        let state = app.state::<crate::AppState>();
        let mut cfg = state.cfg.lock().unwrap_or_else(|e| e.into_inner());
        cfg.auto_update = on;
        crate::config::save(&cfg);
        // Keep concurrent preference writes and their timer changes ordered.
        // The timer releases its schedule lock before reading this config.
        schedule(on, Duration::ZERO);
    }
    refresh_policy(&app)
}

/// A Mac-only release cannot be advertised as a Windows update.
fn newer_windows_release(body: &str, current: &Version) -> Result<Option<Version>, String> {
    let release: GithubRelease = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let tag = release
        .tag_name
        .strip_prefix('v')
        .ok_or("release tag has no v prefix")?;
    let version = Version::parse(tag).map_err(|e| e.to_string())?;
    if !release
        .assets
        .iter()
        .any(|asset| asset.name == INSTALLER_NAME)
    {
        return Err(format!(
            "release {} has no Windows installer",
            release.tag_name
        ));
    }
    Ok((version > *current).then_some(version))
}

fn latest_windows_release(current: &Version) -> Result<Option<Version>, String> {
    use std::io::Read;
    let agent = ureq::AgentBuilder::new().timeout(NETWORK_TIMEOUT).build();
    let response = agent
        .get(RELEASE_API)
        .set("Accept", "application/vnd.github+json")
        .set(
            "User-Agent",
            concat!("Codenotch/", env!("CARGO_PKG_VERSION")),
        )
        .call()
        .map_err(|e| e.to_string())?;
    let mut body = String::new();
    response
        .into_reader()
        .take((MAX_RELEASE_BYTES + 1) as u64)
        .read_to_string(&mut body)
        .map_err(|e| e.to_string())?;
    if body.len() > MAX_RELEASE_BYTES {
        return Err("release response is too large".into());
    }
    newer_windows_release(&body, current)
}

fn newer_signed_release(signed: &str, current: &Version) -> Option<Version> {
    Version::parse(signed)
        .ok()
        .filter(|version| version > current)
}

fn signed_feed_offer(app: &AppHandle) -> Result<Option<Update>, String> {
    tauri::async_runtime::block_on(async {
        app.updater_builder()
            .timeout(NETWORK_TIMEOUT)
            .build()?
            .check()
            .await
    })
    .map_err(|e| e.to_string())
}

/// Reserve under one lock: manual and timer-triggered work cannot overlap.
fn reserve_check(current: &mut UpdateState) -> bool {
    if current.checking || current.installing {
        return false;
    }
    current.checking = true;
    current.message = None;
    true
}

fn reserve_install(current: &mut UpdateState) -> Option<String> {
    if current.checking || current.installing || !current.can_install {
        return None;
    }
    let version = current.available.clone()?;
    current.installing = true;
    current.message = None;
    Some(version)
}

fn fail_check(app: &AppHandle) {
    let checked_at = state().as_ref().and_then(|s| s.checked_at);
    set(
        app,
        UpdateState {
            checked_at,
            message: Some(CHECK_ERROR.into()),
            ..Default::default()
        },
    );
}

fn successful_check(app: &AppHandle, version: Option<String>, can_install: bool) {
    set(
        app,
        UpdateState {
            available: version,
            checked: true,
            can_install,
            checked_at: Some(now_ms()),
            ..Default::default()
        },
    );
}

fn run_check(app: &AppHandle) {
    let current = match Version::parse(env!("CARGO_PKG_VERSION")) {
        Ok(version) => version,
        Err(_) => {
            fail_check(app);
            return;
        }
    };
    // A working signed feed needs a single request. The release API is only a
    // fallback when the signed feed is unavailable or not configured.
    if configured(app) {
        match signed_feed_offer(app) {
            Ok(Some(update)) => {
                if let Some(version) = newer_signed_release(&update.version, &current) {
                    successful_check(app, Some(version.to_string()), true);
                    if automatic_enabled(app) && !portable() {
                        start_install(app.clone(), Some(update), true);
                    }
                    return;
                }
                successful_check(app, None, false);
                return;
            }
            Ok(None) => {
                successful_check(app, None, false);
                return;
            }
            Err(_) => {
                crate::applog("updater: signed feed unavailable; checking published releases")
            }
        }
    }
    match latest_windows_release(&current) {
        Ok(version) => successful_check(app, version.map(|v| v.to_string()), false),
        Err(_) => {
            crate::applog("updater: release check failed");
            fail_check(app);
        }
    }
}

/// Returns immediately; network work happens off the UI thread.
#[tauri::command]
pub fn check_for_update(app: AppHandle) {
    let next = {
        let mut stored = state();
        let current = stored.get_or_insert_with(UpdateState::default);
        if !reserve_check(current) {
            return;
        }
        current.clone()
    };
    // Do not write a stale snapshot after releasing the reservation lock.
    let _ = app.emit("update_state", with_policy(&app, next));
    let worker_app = app.clone();
    if std::thread::Builder::new()
        .name("codenotch-update-check".into())
        .spawn(move || run_check(&worker_app))
        .is_err()
    {
        fail_check(&app);
    }
}

/// Open only the fixed GitHub asset named by a successful release check.
/// The webview supplies neither the version nor a URL.
#[tauri::command]
pub fn open_update_installer() -> Result<(), String> {
    let version = {
        let current = state();
        let offer = current.as_ref().ok_or("No update has been checked")?;
        if offer.checking || offer.installing || offer.can_install {
            return Err("No manual installer is available".into());
        }
        offer
            .available
            .clone()
            .ok_or("No Windows update is available")?
    };
    let version = Version::parse(&version).map_err(|e| e.to_string())?;
    let url = format!(
        "https://github.com/ThePunisherai/codenotch/releases/download/v{version}/{INSTALLER_NAME}"
    );
    let mut command = std::process::Command::new("cmd");
    command.args(["/C", "start", "", &url]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command.spawn().map(|_| ()).map_err(|e| e.to_string())
}

fn run_install(app: &AppHandle, offered: &str, known: Option<Update>, automatic: bool) {
    let outcome = tauri::async_runtime::block_on(async {
        let update = match known {
            Some(update) => Some(update),
            None => {
                app.updater_builder()
                    .timeout(NETWORK_TIMEOUT)
                    .build()?
                    .check()
                    .await?
            }
        };
        let Some(mut update) = update else {
            return Ok::<bool, tauri_plugin_updater::Error>(false);
        };
        if update.version != offered {
            return Ok(false);
        }
        update.timeout = Some(Duration::from_secs(120));
        // Tauri verifies the signature and configured signed version. The byte
        // buffer lives only during an actual update, not while the app is idle.
        let bytes = update.download(|_, _| {}, || {}).await?;
        if (automatic && !automatic_enabled(app)) || portable() {
            return Ok(false);
        }
        update.restart_after_install(true).install(bytes)?;
        Ok(true)
    });
    match outcome {
        Ok(true) => crate::applog("updater: signed update installed, restarting"),
        Ok(false) | Err(_) => {
            crate::applog("updater: update was paused, changed, or could not be installed");
            let checked_at = state().as_ref().and_then(|s| s.checked_at);
            set(
                app,
                UpdateState {
                    available: Some(offered.to_owned()),
                    checked: true,
                    can_install: true,
                    checked_at,
                    message: Some(INSTALL_ERROR.into()),
                    ..Default::default()
                },
            );
        }
    }
}

fn start_install(app: AppHandle, known: Option<Update>, automatic: bool) {
    if !configured(&app) || portable() || (automatic && !automatic_enabled(&app)) {
        return;
    }
    let (offered, next) = {
        let mut stored = state();
        let current = stored.get_or_insert_with(UpdateState::default);
        let Some(offered) = reserve_install(current) else {
            return;
        };
        (offered, current.clone())
    };
    let _ = app.emit("update_state", with_policy(&app, next));
    let worker_app = app.clone();
    let worker_offered = offered.clone();
    if std::thread::Builder::new()
        .name("codenotch-update-install".into())
        .spawn(move || run_install(&worker_app, &worker_offered, known, automatic))
        .is_err()
    {
        let checked_at = state().as_ref().and_then(|s| s.checked_at);
        set(
            &app,
            UpdateState {
                available: Some(offered),
                checked: true,
                can_install: true,
                checked_at,
                message: Some(INSTALL_ERROR.into()),
                ..Default::default()
            },
        );
    }
}

/// Only a signed feed offer can be installed, from an installed copy.
#[tauri::command]
pub fn install_update(app: AppHandle) {
    start_install(app, None, false);
}

pub fn check_on_launch(app: &AppHandle) {
    schedule(automatic_enabled(app), LAUNCH_DELAY);
    refresh_policy(app);
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("codenotch-update-timer".into())
        .spawn(move || loop {
            let mut deadline = SCHEDULE.0.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                match *deadline {
                    None => deadline = SCHEDULE.1.wait(deadline).unwrap_or_else(|e| e.into_inner()),
                    Some(next) if next > Instant::now() => {
                        deadline = SCHEDULE
                            .1
                            .wait_timeout(deadline, next.saturating_duration_since(Instant::now()))
                            .unwrap_or_else(|e| e.into_inner())
                            .0;
                    }
                    Some(_) => {
                        *deadline = Some(Instant::now() + CHECK_INTERVAL);
                        break;
                    }
                }
            }
            drop(deadline);
            if automatic_enabled(&app) {
                check_for_update(app.clone());
            }
        });
}

#[cfg(test)]
mod tests {
    use super::{
        merge_policy, newer_signed_release, newer_windows_release, portable_at, reserve_check,
        reserve_install, UpdateState,
    };
    use semver::Version;
    use std::sync::{Arc, Barrier, Mutex};

    fn release(tag: &str, windows: bool) -> String {
        let asset = if windows {
            r#"[{"name":"Codenotch-Setup.exe"}]"#
        } else {
            "[]"
        };
        format!(r#"{{"tag_name":"{tag}","assets":{asset}}}"#)
    }

    #[test]
    fn compares_only_windows_releases() {
        let current = Version::parse("1.18.0").unwrap();
        assert_eq!(
            newer_windows_release(&release("v1.19.0", true), &current).unwrap(),
            Some(Version::parse("1.19.0").unwrap())
        );
        assert_eq!(
            newer_windows_release(&release("v1.18.0", true), &current).unwrap(),
            None
        );
        assert_eq!(
            newer_windows_release(&release("v1.17.0", true), &current).unwrap(),
            None
        );
        assert!(newer_windows_release(&release("v1.19.0", false), &current).is_err());
        assert!(newer_windows_release("not json", &current).is_err());
        assert!(newer_windows_release(&release("latest", true), &current).is_err());
    }

    #[test]
    fn signed_offer_must_be_newer() {
        let current = Version::parse("1.18.0").unwrap();
        assert_eq!(
            newer_signed_release("1.19.0", &current),
            Some(Version::parse("1.19.0").unwrap())
        );
        for version in ["1.18.0", "1.17.0", "bad version"] {
            assert_eq!(newer_signed_release(version, &current), None);
        }
    }

    #[test]
    fn simultaneous_checks_have_a_single_reservation() {
        let current = Arc::new(Mutex::new(UpdateState::default()));
        let barrier = Arc::new(Barrier::new(12));
        let workers: Vec<_> = (0..12)
            .map(|_| {
                let current = current.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    reserve_check(&mut current.lock().unwrap())
                })
            })
            .collect();
        let winners = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
        assert!(current.lock().unwrap().checking);
    }

    #[test]
    fn check_and_install_cannot_overlap_or_install_unsigned() {
        let mut current = UpdateState {
            available: Some("1.19.0".into()),
            ..Default::default()
        };
        assert!(reserve_install(&mut current).is_none());
        current.can_install = true;
        assert!(reserve_check(&mut current));
        assert!(reserve_install(&mut current).is_none());
        current.checking = false;
        assert_eq!(reserve_install(&mut current).as_deref(), Some("1.19.0"));
        assert!(!reserve_check(&mut current));
        assert!(reserve_install(&mut current).is_none());
    }

    #[test]
    fn preference_changes_preserve_an_active_worker_and_its_offer() {
        let mut current = UpdateState {
            available: Some("1.25.0".into()),
            installing: true,
            can_install: true,
            checked_at: Some(123),
            ..Default::default()
        };
        let paused = UpdateState {
            auto_update: false,
            signing_ready: true,
            ..Default::default()
        };
        merge_policy(&mut current, &paused);
        assert!(current.installing);
        assert!(current.can_install);
        assert!(!current.auto_update);
        assert_eq!(current.available.as_deref(), Some("1.25.0"));
        assert_eq!(current.checked_at, Some(123));
        assert!(!reserve_check(&mut current));
        assert!(reserve_install(&mut current).is_none());
    }

    #[test]
    fn timer_manual_checks_and_preference_changes_share_one_reservation() {
        let current = Arc::new(Mutex::new(UpdateState::default()));
        let barrier = Arc::new(Barrier::new(16));
        let workers: Vec<_> = (0..16)
            .map(|index| {
                let current = current.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let mut current = current.lock().unwrap();
                    if index % 2 == 0 {
                        reserve_check(&mut current)
                    } else {
                        merge_policy(
                            &mut current,
                            &UpdateState {
                                auto_update: index % 3 == 0,
                                signing_ready: true,
                                ..Default::default()
                            },
                        );
                        false
                    }
                })
            })
            .collect();
        let winners = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
        assert!(current.lock().unwrap().checking);
    }

    #[test]
    fn shipping_config_requires_signed_version_and_refuses_downgrades() {
        let source: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let updater: tauri_plugin_updater::Config =
            serde_json::from_value(source["plugins"]["updater"].clone()).unwrap();
        assert!(updater.require_signed_version);
        assert!(!updater.allow_downgrades);
        assert_ne!(updater.pubkey, super::UNSET_PUBKEY);
        assert!(!updater.pubkey.is_empty());
        assert!(updater.endpoints.iter().all(|url| url.scheme() == "https"));
    }

    #[test]
    fn portable_without_installer_marker_is_never_installed() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "codenotch-updater-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let executable = directory.join("codenotch.exe");
        assert!(portable_at(&executable));
        std::fs::write(directory.join(".codenotch-installed"), "Codenotch NSIS\n").unwrap();
        assert!(!portable_at(&executable));
        std::fs::write(directory.join("codenotch-portable.marker"), "").unwrap();
        assert!(portable_at(&executable));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
