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
$signingConfig = $null
Push-Location $windowsRoot
try {
    $destination = [System.IO.Path]::GetFullPath($OutputDirectory)
    $signed = -not [string]::IsNullOrWhiteSpace($env:TAURI_SIGNING_PRIVATE_KEY)
    $hostInfo = (& rustc -vV) -join "`n"
    if ($LASTEXITCODE -ne 0 -or $hostInfo -notmatch 'host: x86_64-pc-windows-msvc') {
        throw 'Use the x86_64-pc-windows-msvc Rust toolchain for this Windows x64 package.'
    }

    Invoke-Checked -Command node -Arguments @('scripts/check-ui-scripts.mjs')
    Invoke-Checked -Command node -Arguments @('scripts/test-update-ui.cjs')
    Invoke-Checked -Command node -Arguments @('scripts/test-claude-auth-ui.cjs')
    Invoke-Checked -Command node -Arguments @('--test', 'test-light-surface.cjs', 'test-carry.cjs', 'test-codex-headline.cjs', 'test-reset-card.cjs', 'test-reset-tracker.cjs', 'test-account-profiles.cjs', 'test-notch-accounts.cjs', 'scripts/test-ko-i18n.cjs')
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
        $bundleArguments = @('--yes', '@tauri-apps/cli@2.12.1', 'build', '--ci', '--config', 'tauri.bundle.conf.json')
        if ($signed) {
            # This file contains configuration only. The private key stays in the environment.
            $signingConfig = Join-Path ([System.IO.Path]::GetTempPath()) "codenotch-signing-$([guid]::NewGuid().ToString('N')).json"
            [System.IO.File]::WriteAllText($signingConfig, '{"bundle":{"createUpdaterArtifacts":"v1Compatible"}}', [System.Text.UTF8Encoding]::new($false))
            $bundleArguments += @('--config', $signingConfig)
        }
        Invoke-Checked -Command npx -Arguments ($bundleArguments + @('--', '--locked'))
    }
    finally {
        Pop-Location
    }

    $packageVersion = (Get-Content codenotch/tauri.conf.json -Raw | ConvertFrom-Json).version
    $installer = "target/release/bundle/nsis/Codenotch_${packageVersion}_x64-setup.exe"
    if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) {
        throw "The installer for version $packageVersion was not produced."
    }
    New-Item -ItemType Directory -Path $destination -Force | Out-Null
    Copy-Item -LiteralPath $installer -Destination (Join-Path $destination 'Codenotch-Setup.exe') -Force
    $artifactNames = @('Codenotch-Setup.exe', 'Codenotch-Portable.zip')

    if ($signed) {
        $archives = @(Get-ChildItem target/release/bundle/nsis/*-setup.nsis.zip -File |
            Where-Object { $_.Name -like "*_${packageVersion}_*-setup.nsis.zip" })
        if ($archives.Count -ne 1) { throw "Expected one updater archive for $packageVersion, found $($archives.Count)." }
        $signaturePath = "$($archives[0].FullName).sig"
        if (-not (Test-Path -LiteralPath $signaturePath -PathType Leaf)) { throw 'The updater signature was not produced.' }
        $signature = (Get-Content -LiteralPath $signaturePath -Raw).Trim()
        if ([string]::IsNullOrWhiteSpace($signature)) { throw 'The updater signature is empty.' }
        $signatureText = [System.Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($signature))
        $trustedComment = ($signatureText -split "`n" | Where-Object { $_.StartsWith('trusted comment: ') }) -join ''
        if (($trustedComment.Trim() -split "`t") -notcontains "version:$packageVersion") {
            throw 'The updater signature does not bind the published version. Use Tauri CLI 2.12.1 or newer.'
        }
        Copy-Item -LiteralPath $archives[0].FullName -Destination (Join-Path $destination 'Codenotch-Setup.nsis.zip') -Force
        Copy-Item -LiteralPath $signaturePath -Destination (Join-Path $destination 'Codenotch-Setup.nsis.zip.sig') -Force
        $feed = [ordered]@{
            version = $packageVersion
            pub_date = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
            platforms = [ordered]@{
                'windows-x86_64' = [ordered]@{
                    signature = $signature
                    url = "https://github.com/ThePunisherai/codenotch/releases/download/v$packageVersion/Codenotch-Setup.nsis.zip"
                }
            }
        }
        [System.IO.File]::WriteAllText((Join-Path $destination 'latest.json'), ($feed | ConvertTo-Json -Depth 5), [System.Text.UTF8Encoding]::new($false))
        $artifactNames += @('Codenotch-Setup.nsis.zip', 'Codenotch-Setup.nsis.zip.sig', 'latest.json')
    }
    else {
        # Do not leave a feed from a previous signed build beside a new unsigned package.
        foreach ($name in 'Codenotch-Setup.nsis.zip', 'Codenotch-Setup.nsis.zip.sig', 'latest.json') {
            $oldArtifact = Join-Path $destination $name
            if (Test-Path -LiteralPath $oldArtifact -PathType Leaf) { Remove-Item -LiteralPath $oldArtifact -Force }
        }
        Write-Host 'No signing key configured: producing the installer and portable app without an update feed.'
    }

    $portable = Join-Path $windowsRoot 'target/portable'
    New-Item -ItemType Directory -Path $portable -Force | Out-Null
    $installedMarker = Join-Path $portable '.codenotch-installed'
    if (Test-Path -LiteralPath $installedMarker -PathType Leaf) { Remove-Item -LiteralPath $installedMarker -Force }
    [System.IO.File]::WriteAllText((Join-Path $portable 'codenotch-portable.marker'), 'Codenotch portable', [System.Text.UTF8Encoding]::new($false))
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
        'Open Settings > Accounts to connect and select separate provider profiles.'
        'Keep Codenotch running to receive reset notifications.'
        ''
        'Automatic update checks run shortly after launch and every six hours by default.'
        'Portable copies offer an installer download and never install an update automatically.'
        'Keep codenotch-portable.marker beside the app to preserve portable update behavior.'
    ) -join "`r`n" | Set-Content (Join-Path $portable 'README.txt') -Encoding utf8
    Compress-Archive -Path "$portable/*" -DestinationPath (Join-Path $destination 'Codenotch-Portable.zip') -Force

    $artifactNames | ForEach-Object { Get-FileHash -LiteralPath (Join-Path $destination $_) -Algorithm SHA256 } |
        ForEach-Object { "$($_.Hash.ToLower())  $(Split-Path -Leaf $_.Path)" } |
        Set-Content (Join-Path $destination 'SHA256SUMS.txt') -Encoding ascii
    Write-Host "Windows installer and portable app are ready in $destination"
}
finally {
    if ($signingConfig -and (Test-Path -LiteralPath $signingConfig -PathType Leaf)) {
        Remove-Item -LiteralPath $signingConfig -Force
    }
    Pop-Location
}
