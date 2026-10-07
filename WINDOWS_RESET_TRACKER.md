# Codex Reset Tracker for Windows

Open **Settings → Reset Tracker** from the notch or the system tray.

The tracker shows three separate sources of information:

- **Reset announcements:** the latest confirmed public Codex/ChatGPT Work reset reported by [codex-reset.com](https://codex-reset.com/), including its announcement time and history. Ordinary ChatGPT message limits are separate.
- **Reset forecast:** the source's historical estimate. A forecast does not confirm a reset or trigger a reset notification.
- **Your Codex windows:** usage and reset countdowns read from the existing local Codex sign-in. An elapsed countdown waits for a fresh account reading before declaring a quota renewal.

The tracker keeps the last successful public reading when offline and labels it accordingly. Your credentials are not sent to the public reset tracker.

## Notifications

Enable **Global reset notifications** on the Reset Tracker tab to receive the new announcement card. **Test notification** lets you see it immediately. The card's source button opens `https://codex-reset.com/`.

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
