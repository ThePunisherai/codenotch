param(
    [string] $OutputDirectory = 'artifacts'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ($env:OS -ne 'Windows_NT') {
    throw 'Run this script on Windows with the Rust MSVC toolchain installed.'
}

foreach ($tool in 'cargo', 'rustc', 'node', 'npx') {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "Missing $tool. Install Rust (MSVC) and Node.js, then run this script again."
    }
}

function Invoke-Checked {
    param([string] $Command, [string[]] $Arguments)
    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Command failed with exit code $LASTEXITCODE."
    }
}

$windowsRoot = Split-Path -Parent $PSScriptRoot
Push-Location $windowsRoot
try {
    $destination = [System.IO.Path]::GetFullPath($OutputDirectory)
    $hostInfo = (& rustc -vV) -join "`n"
    if ($LASTEXITCODE -ne 0 -or $hostInfo -notmatch 'host: x86_64-pc-windows-msvc') {
        throw 'Use the x86_64-pc-windows-msvc Rust toolchain for this Windows x64 package.'
    }

    Invoke-Checked -Command node -Arguments @('scripts/check-ui-scripts.mjs')
    Invoke-Checked -Command node -Arguments @('scripts/test-update-ui.cjs')
    Invoke-Checked -Command node -Arguments @('scripts/test-claude-auth-ui.cjs')
    Invoke-Checked -Command node -Arguments @('--test', 'test-light-surface.cjs', 'test-carry.cjs', 'test-codex-headline.cjs', 'test-reset-card.cjs', 'test-reset-tracker.cjs', 'scripts/test-ko-i18n.cjs')
    Invoke-Checked -Command cargo -Arguments @('test', '--release', '--locked')

    $originalRustFlags = $env:RUSTFLAGS
    try {
        $env:RUSTFLAGS = "$originalRustFlags -C target-feature=+crt-static".Trim()
        Invoke-Checked -Command cargo -Arguments @('build', '--release', '--locked', '-p', 'codenotch-hook', '--target-dir', 'target/hook')
    }
    finally {
        $env:RUSTFLAGS = $originalRustFlags
    }

    Push-Location codenotch
    try {
        Invoke-Checked -Command npx -Arguments @('--yes', '@tauri-apps/cli@2.11.4', 'build', '--config', 'tauri.bundle.conf.json', '--', '--locked')
    }
    finally {
        Pop-Location
    }

    $installers = @(Get-ChildItem target/release/bundle/nsis/*-setup.exe)
    if ($installers.Count -ne 1) {
        throw "Expected one NSIS installer, found $($installers.Count)."
    }
    New-Item -ItemType Directory -Path $destination -Force | Out-Null
    Copy-Item $installers[0].FullName (Join-Path $destination 'Codenotch-Setup.exe') -Force

    $portable = Join-Path $windowsRoot 'target/portable'
    New-Item -ItemType Directory -Path $portable -Force | Out-Null
    Copy-Item target/release/codenotch.exe (Join-Path $portable 'codenotch.exe') -Force
    Copy-Item target/hook/release/codenotch-hook.exe (Join-Path $portable 'codenotch-hook.exe') -Force
    @(
        'Codenotch for Windows'
        ''
        'Extract this entire folder and run codenotch.exe.'
        'Keep codenotch-hook.exe beside it for Claude Code hooks.'
        'The portable app requires Microsoft Edge WebView2 Runtime.'
        'Use Codenotch-Setup.exe if WebView2 is not installed.'
        ''
        'Open Settings > Reset Tracker to see Codex reset announcements and your usage windows.'
        'Keep Codenotch running to receive reset notifications.'
    ) -join "`r`n" | Set-Content (Join-Path $portable 'README.txt') -Encoding utf8
    Compress-Archive -Path "$portable/*" -DestinationPath (Join-Path $destination 'Codenotch-Portable.zip') -Force

    Get-ChildItem $destination -Include 'Codenotch-Setup.exe', 'Codenotch-Portable.zip' -File -Recurse |
        Get-FileHash -Algorithm SHA256 |
        ForEach-Object { "$($_.Hash.ToLower())  $(Split-Path -Leaf $_.Path)" } |
        Set-Content (Join-Path $destination 'SHA256SUMS.txt') -Encoding ascii
    Write-Host "Windows installer and portable app are ready in $destination"
}
finally {
    Pop-Location
}
