# Codex Reset Tracker for Windows

Open **Settings → Reset Tracker** from the notch or the system tray.

The tracker keeps public announcements separate from account readings:

- **Reset announcements:** the latest confirmed public Codex/ChatGPT Work reset reported by [codex-reset.com](https://codex-reset.com/), including its announcement time and history. Ordinary ChatGPT message limits are separate.
- **Reset forecast:** the source's historical estimate. A forecast does not confirm a reset or trigger a reset notification.
- **Banked resets:** public announced, arriving, available or unknown status and its history. Availability for your account, remaining grants and expiry are unknown unless the provider reports them. Banked updates never advance the confirmed global reset clock.
- **Live feed and service status:** recent public updates and source-reported service health.
- **Your Codex windows:** usage and reset countdowns read from the selected Codex account. An elapsed countdown waits for a fresh account reading before declaring a quota renewal.

The tracker keeps the last successful public reading when offline and labels it accordingly. Your credentials are not sent to the public reset tracker.

Public endpoints are checked concurrently every 60 seconds, measured from the start of each check. Opening the tracker, returning to it or choosing Refresh requests a check as soon as the source permits. The interface updates ages and countdowns every second. The source itself caches responses for 60 seconds and permits at most one request per minute per endpoint; it has no documented push subscription. Consequently this is automatic polling, not a guarantee of zero publication delay. HTTP 429 waits are respected.

## Multiple accounts

Open **Settings → Accounts**. Add a named profile, choose **Sign in** or **Connect**, and select **Use** to display its quota. **Detect existing** imports supported existing local CLI/editor sessions into separate profiles. Account selection immediately clears the previous account's displayed quota and requests a fresh reading. Usage caches, rate-limit waits and personal-reset baselines belong to each profile; switching accounts does not count as a reset.

| Provider | Connect method |
|---|---|
| Codex / Claude | Official native CLI login in an isolated profile directory; supported existing CLI credential files can also be imported. |
| Antigravity / Cursor | Import a supported existing local session. Antigravity may also use the installed official CLI. Browser login alone does not expose a session to Codenotch. |
| GitHub Copilot | Official GitHub CLI device login in an isolated profile, or connect a supported token. |
| GLM / OpenCode | Connect the appropriate provider key; supported OpenCode sessions can be detected. |
| Grok | Official Grok CLI browser login in an isolated profile, or import its existing session; only supported issuer credentials are used. |

The account registry contains profile metadata, not tokens. Imported desktop credentials and entered keys are protected with current-user Windows DPAPI; official CLIs manage their own profile auth files. Secrets never appear in account-list responses. A saved credential is marked **Connected** only after a successful fresh usage reading. Removing a profile removes its app-owned credentials without signing out the original CLI/editor installation.

## Notifications

Enable **Global reset notifications** on the Reset Tracker tab to receive the new announcement card. **Test notification** lets you see it immediately. The card's source button opens `https://codex-reset.com/`.

Banked update notifications have a separate switch and preview. They report a public update and ask you to check availability in your account. They never claim that a banked grant has been credited personally.

Personal usage-window notifications remain under **General → Notifications → Reset notifications**. The notification sound is shared. Notifications can appear while the notch is hidden, provided Codenotch is still running.

Existing announcements are used as the initial baseline on first launch. Restarting the app does not replay previously observed announcements.

## Download a Windows build

The [Windows Package workflow](https://github.com/ThePunisherai/codenotch/actions/workflows/windows-package.yml) produces two artifacts for each Windows change on `main` or `codex-reset-tracker`:

| Artifact | Use |
|---|---|
| `Codenotch-Setup-<commit>` | Extract and run `Codenotch-Setup.exe`. Installs for the current user and installs WebView2 if needed. |
| `Codenotch-Portable-<commit>` | Extract the artifact, then `Codenotch-Portable.zip`, and run `codenotch.exe`. Keep the hook executable beside it. Requires Microsoft Edge WebView2 Runtime. |

The installer and portable executable are not code-signed. Windows may show a SmartScreen prompt for the first launch.

If GitHub Actions has not been enabled for the fork, the owner must enable it once on the repository's **Actions** tab. Then select **Windows Package → Run workflow → codex-reset-tracker**, or push another change to that branch. Creating or changing workflow files alone does not enable Actions for a fork.

The packaging workflow validates UI scripts, builds the hook and app, creates the NSIS installer and portable archive, and smoke-tests installation, the diagnostic command, and uninstallation. The **Windows** workflow also runs the Rust tests on Windows and Linux.

## Build locally

Prerequisites on Windows x64:

- Rust with the `x86_64-pc-windows-msvc` toolchain.
- Visual Studio Build Tools with **Desktop development with C++** and the Windows SDK.
- Node.js and npm.

From the repository root, run:

```powershell
powershell -ExecutionPolicy Bypass -File .\windows\scripts\build-windows.ps1
```

The script runs UI and Rust tests, then writes `Codenotch-Setup.exe`, `Codenotch-Portable.zip`, and `SHA256SUMS.txt` into `windows/artifacts/`.

To choose another output directory:

```powershell
powershell -ExecutionPolicy Bypass -File .\windows\scripts\build-windows.ps1 -OutputDirectory C:\CodenotchBuild
```

Run `codenotch.exe doctor` for the existing local diagnostic report.
