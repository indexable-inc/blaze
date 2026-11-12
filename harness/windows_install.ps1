# End-to-end checks for site/install.ps1 against a release-compatible Windows archive.
# One JSON value: {"archive":"...","version":"0.0.0-dev","work":"..."}.
param([Parameter(Mandatory = $true, Position = 0)][string] $Json)

$ErrorActionPreference = "Stop"
$Input = $Json | ConvertFrom-Json
$Archive = [IO.Path]::GetFullPath([string]$Input.archive)
$Version = [string]$Input.version
$PreviousArchive = if ($Input.previous_archive) { [IO.Path]::GetFullPath([string]$Input.previous_archive) } else { "" }
$PreviousVersion = [string]$Input.previous_version
$Work = if ($Input.work) { [IO.Path]::GetFullPath([string]$Input.work) } else {
    Join-Path ([IO.Path]::GetTempPath()) ("blaze-install-test-" + [Guid]::NewGuid().ToString("N"))
}
$Root = Split-Path -Parent $PSScriptRoot
$Installer = Join-Path $Root "site\install.ps1"
$Failures = New-Object Collections.Generic.List[string]

function Check([bool] $Condition, [string] $Message) {
    if (-not $Condition) { $script:Failures.Add($Message) }
}

function RunInstaller([string] $Shell, [string] $CargoHome, [string] $Source, [string] $Checksum = "", [bool] $ShouldPass = $true, [string] $ProfileHome = "") {
    $OldCargo, $OldArchive, $OldChecksum, $OldNoToolchain, $OldHome, $OldProfile = $env:CARGO_HOME, $env:BLAZE_ARCHIVE, $env:BLAZE_ARCHIVE_SHA256, $env:BLAZE_NO_TOOLCHAIN, $env:HOME, $env:USERPROFILE
    try {
        $env:CARGO_HOME, $env:BLAZE_ARCHIVE, $env:BLAZE_ARCHIVE_SHA256, $env:BLAZE_NO_TOOLCHAIN = $CargoHome, $Source, $Checksum, "1"
        if ($ProfileHome) { $env:HOME, $env:USERPROFILE = $ProfileHome, $ProfileHome }
        $said = & $Shell -NoLogo -NoProfile -ExecutionPolicy Bypass -File $Installer -NoToolchain 2>&1 | Out-String
        $code = $LASTEXITCODE
    } finally {
        $env:CARGO_HOME, $env:BLAZE_ARCHIVE, $env:BLAZE_ARCHIVE_SHA256, $env:BLAZE_NO_TOOLCHAIN, $env:HOME, $env:USERPROFILE = $OldCargo, $OldArchive, $OldChecksum, $OldNoToolchain, $OldHome, $OldProfile
    }
    if ($ShouldPass -and $code -ne 0) { throw "$Shell installer failed ($code):`n$said" }
    if (-not $ShouldPass -and $code -eq 0) { throw "$Shell installer unexpectedly succeeded:`n$said" }
    return $said
}

function Installed([string] $Home) { Join-Path $Home "bin\blaze.exe" }

New-Item -ItemType Directory -Force -Path $Work | Out-Null
try {
    Check (Test-Path -LiteralPath $Archive -PathType Leaf) "release archive is missing: $Archive"
    $Expanded = Join-Path $Work "archive"
    Expand-Archive -LiteralPath $Archive -DestinationPath $Expanded
    $ArchiveRoot = @(Get-ChildItem -LiteralPath $Expanded -Directory)
    Check ($ArchiveRoot.Count -eq 1) "archive must contain one root directory"
    if ($ArchiveRoot.Count -eq 1) {
        Check (Test-Path (Join-Path $ArchiveRoot[0].FullName "blaze.exe")) "archive has no blaze.exe"
        Check (Test-Path (Join-Path $ArchiveRoot[0].FullName "blaze-engine.exe")) "archive has no blaze-engine.exe"
        foreach ($Exe in Get-ChildItem -LiteralPath $ArchiveRoot[0].FullName -Filter *.exe) {
            $imports = & dumpbin /nologo /dependents $Exe.FullName | Out-String
            Check ($imports -notmatch '(?i)\b(vcruntime\d*|msvcp\d*|ucrtbase)\.dll\b') "$($Exe.Name) requires a dynamic Visual C++ runtime"
        }
    }

    $Shells = @("powershell.exe")
    if (Get-Command pwsh.exe -ErrorAction SilentlyContinue) { $Shells += "pwsh.exe" }
    else { $Failures.Add("PowerShell 7 (pwsh.exe) is not installed on the Windows runner") }
    foreach ($Shell in $Shells) {
        $Home = Join-Path $Work ("fresh-" + [IO.Path]::GetFileNameWithoutExtension($Shell))
        $UseDefault = $Shell -eq "powershell.exe"
        $CargoHome = if ($UseDefault) { "" } else { $Home }
        $ProfileHome = if ($UseDefault) { $Home } else { "" }
        RunInstaller $Shell $CargoHome $Archive "" $true $ProfileHome | Out-Null
        $InstalledHome = if ($UseDefault) { Join-Path $Home ".cargo" } else { $Home }
        $Blaze = Installed $InstalledHome
        Check (Test-Path $Blaze) "$Shell fresh install made no blaze.exe"
        if (Test-Path $Blaze) {
            $said = & $Blaze --version | Out-String
            Check ($LASTEXITCODE -eq 0) "$Shell installed blaze.exe does not launch"
            Check ($said -match ("blaze\s+" + [regex]::Escape($Version))) "$Shell installed version is not $Version`: $said"
        }
    }

    $Home = Join-Path $Work "lifecycle"
    $env:BLAZE_HOME = Join-Path $Work "blaze-home"
    $FirstArchive = if ($PreviousArchive) { $PreviousArchive } else { $Archive }
    RunInstaller "pwsh.exe" $Home $FirstArchive | Out-Null
    $Blaze = Installed $Home
    if ($PreviousArchive) {
        $old = & $Blaze --version | Out-String
        Check ($old -match ("blaze\s+" + [regex]::Escape($PreviousVersion))) "version A is not $PreviousVersion`: $old"
    }
    New-Item -ItemType Directory -Force -Path $env:BLAZE_HOME | Out-Null
    $Config = Join-Path $env:BLAZE_HOME "config.toml"
    Set-Content -LiteralPath $Config -Value "[update]`nchannel = `"canary`"" -Encoding utf8
    $ConfigBefore = Get-Content -LiteralPath $Config -Raw
    Set-Content -LiteralPath (Join-Path $Home "bin\cargo-blaze.exe") -Value "obsolete" -Encoding ascii
    RunInstaller "pwsh.exe" $Home $Archive | Out-Null
    $new = & $Blaze --version | Out-String
    Check ($new -match ("blaze\s+" + [regex]::Escape($Version))) "version B is not $Version`: $new"
    Check (-not (Test-Path (Join-Path $Home "bin\cargo-blaze.exe"))) "upgrade left obsolete cargo-blaze.exe"
    Check ((Get-Content -LiteralPath $Config -Raw) -eq $ConfigBefore) "upgrade changed user configuration"

    $Before = (Get-FileHash -Algorithm SHA256 -LiteralPath $Blaze).Hash
    RunInstaller "pwsh.exe" $Home $Archive ("0" * 64) $false | Out-Null
    Check ((Get-FileHash -Algorithm SHA256 -LiteralPath $Blaze).Hash -eq $Before) "bad checksum damaged the installed blaze"
    RunInstaller "pwsh.exe" $Home (Join-Path $Work "missing.zip") "" $false | Out-Null
    Check ((Get-FileHash -Algorithm SHA256 -LiteralPath $Blaze).Hash -eq $Before) "missing archive damaged the installed blaze"

    $IncompleteRoot = Join-Path $Work "incomplete-root"
    $IncompleteDir = Join-Path $IncompleteRoot "blaze-broken-x86_64-pc-windows-msvc"
    New-Item -ItemType Directory -Force -Path $IncompleteDir | Out-Null
    Set-Content -LiteralPath (Join-Path $IncompleteDir "README.md") -Value "incomplete"
    $Incomplete = Join-Path $Work "incomplete.zip"
    Compress-Archive -Path $IncompleteDir -DestinationPath $Incomplete
    RunInstaller "pwsh.exe" $Home $Incomplete "" $false | Out-Null
    Check ((Get-FileHash -Algorithm SHA256 -LiteralPath $Blaze).Hash -eq $Before) "incomplete archive damaged the installed blaze"

    $Blocked = Join-Path $Work "not-a-directory"
    Set-Content -LiteralPath $Blocked -Value "file"
    RunInstaller "pwsh.exe" $Blocked $Archive "" $false | Out-Null

    $Project = Join-Path $Work "hello"
    New-Item -ItemType Directory -Force -Path (Join-Path $Project "src") | Out-Null
    Set-Content -LiteralPath (Join-Path $Project "Cargo.toml") -Value "[package]`nname=`"hello`"`nversion=`"0.1.0`"`nedition=`"2024`"" -Encoding ascii
    Set-Content -LiteralPath (Join-Path $Project "src\main.rs") -Value 'fn main() { println!("one"); }' -Encoding ascii
    Push-Location $Project
    try {
        $first = & $Blaze run 2>&1 | Out-String
        Check ($LASTEXITCODE -eq 0 -and $first -match 'one') "installed blaze did not clean-build and run the project: $first"
        Set-Content -LiteralPath (Join-Path $Project "src\main.rs") -Value 'fn main() { println!("two"); }' -Encoding ascii
        $second = & $Blaze run 2>&1 | Out-String
        Check ($LASTEXITCODE -eq 0 -and $second -match 'two') "installed blaze did not build and run the incremental edit: $second"
        # The second install happens while the daemon from the edit is alive; the installer must stop it.
        RunInstaller "pwsh.exe" $Home $Archive | Out-Null
    } finally {
        & $Blaze stop *> $null
        Pop-Location
    }

    $OldReleases, $OldVersion, $OldArchive = $env:BLAZE_RELEASES, $env:BLAZE_VERSION, $env:BLAZE_ARCHIVE
    try {
        $env:CARGO_HOME, $env:BLAZE_RELEASES, $env:BLAZE_VERSION = $Home, "https://127.0.0.1:1", "0.0.0-unavailable"
        Remove-Item Env:BLAZE_ARCHIVE -ErrorAction SilentlyContinue
        & pwsh.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File $Installer -NoToolchain *> $null
        Check ($LASTEXITCODE -ne 0) "unavailable release/interrupted download unexpectedly installed"
    } finally {
        $env:BLAZE_RELEASES, $env:BLAZE_VERSION, $env:BLAZE_ARCHIVE = $OldReleases, $OldVersion, $OldArchive
    }

    if ($Failures.Count) { throw ($Failures -join "`n") }
    Write-Output "windows_install.ps1: fresh install, upgrade, rollback, archive and build scenarios passed"
} finally {
    Remove-Item -LiteralPath $Work -Recurse -Force -ErrorAction SilentlyContinue
}
