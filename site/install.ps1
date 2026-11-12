# Installs blaze on Windows from a release-compatible ZIP archive. Windows PowerShell 5.1 and
# PowerShell 7 are both supported.
[CmdletBinding()]
param(
    [string] $Version = $env:BLAZE_VERSION,
    [ValidateSet("stable", "canary")]
    [string] $Channel = $(if ($env:BLAZE_CHANNEL) { $env:BLAZE_CHANNEL } else { "stable" }),
    [switch] $NoToolchain,
    [switch] $Yes
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$Target = "x86_64-pc-windows-msvc"
$Releases = if ($env:BLAZE_RELEASES) { $env:BLAZE_RELEASES.TrimEnd("/") } else { "https://blaze-releases.ix.dev" }
$ArchiveOverride = $env:BLAZE_ARCHIVE
$ChecksumOverride = $env:BLAZE_ARCHIVE_SHA256
$Toolchain = "nightly-2026-10-27" # harness/toolchain; harness/toolchain_bump.py writes it.

function Fail([string] $Message, [string] $Detail = "") {
    [Console]::Error.WriteLine("error: $Message")
    if ($Detail) { [Console]::Error.WriteLine("  $Detail") }
    exit 1
}

function Download([string] $Url, [string] $Destination) {
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $Destination
    } catch {
        throw "the download of $Url did not finish: $($_.Exception.Message)"
    }
}

function DownloadText([string] $Url) {
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        $client = New-Object Net.WebClient
        try { return $client.DownloadString($Url) } finally { $client.Dispose() }
    } catch {
        throw "the download of $Url did not finish: $($_.Exception.Message)"
    }
}

if (-not [Environment]::Is64BitOperatingSystem) {
    Fail "blaze has no Windows 32-bit release." "Use 64-bit Windows on x86-64."
}
if (-not $ArchiveOverride -and -not $Releases.StartsWith("https://", [StringComparison]::OrdinalIgnoreCase)) {
    Fail "BLAZE_RELEASES is not an HTTPS address." "A release and its checksum must come from the same HTTPS origin."
}
if ($Version -in @("stable", "canary")) { $Channel = $Version; $Version = "" }

$Temp = Join-Path ([IO.Path]::GetTempPath()) ("blaze-install-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $Temp | Out-Null
try {
    if ($ArchiveOverride) {
        $Archive = [IO.Path]::GetFullPath($ArchiveOverride)
        if (-not (Test-Path -LiteralPath $Archive -PathType Leaf)) {
            Fail "there is no file $Archive to install blaze from."
        }
        $Asset = [IO.Path]::GetFileName($Archive)
        $Wanted = $ChecksumOverride
        if (-not $Wanted) { $Wanted = (Get-FileHash -Algorithm SHA256 -LiteralPath $Archive).Hash }
        $Shown = "blaze from $Asset"
    } else {
        try {
            if ($Version) {
                $Tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }
            } else {
                $named = (DownloadText "$Releases/channels/$Channel").Trim()
                if ($named -notmatch '^v?[0-9]+\.[0-9]+\.[0-9]+$') {
                    Fail "blaze's $Channel channel names '$named', which is not a version."
                }
                $Tag = if ($named.StartsWith("v")) { $named } else { "v$named" }
            }
            $Asset = "blaze-$Tag-$Target.zip"
            $Sums = DownloadText "$Releases/$Tag/SHA256SUMS"
            $line = $Sums -split "`r?`n" | Where-Object { $_ -match ("^[0-9a-fA-F]{64}\s+\*?" + [regex]::Escape($Asset) + "$") } | Select-Object -First 1
            if (-not $line) { Fail "blaze release $Tag has no build for Windows on x86-64." }
            $Wanted = ($line -split '\s+')[0]
            $Archive = Join-Path $Temp $Asset
            Download "$Releases/$Tag/$Asset" $Archive
            $Shown = "blaze $($Tag.TrimStart('v'))"
        } catch {
            if ($Version) { Fail "there is no usable blaze release $Version." $_.Exception.Message }
            Fail "the list or archive for blaze's $Channel channel could not be downloaded." $_.Exception.Message
        }
    }

    $Got = (Get-FileHash -Algorithm SHA256 -LiteralPath $Archive).Hash
    if ($Got -ne $Wanted) {
        Fail "the archive checksum does not match, so nothing was installed." "$Asset has SHA256 $Got; expected $Wanted."
    }

    $Expanded = Join-Path $Temp "expanded"
    New-Item -ItemType Directory -Path $Expanded | Out-Null
    try { Expand-Archive -LiteralPath $Archive -DestinationPath $Expanded } catch {
        Fail "the downloaded archive could not be unpacked." $_.Exception.Message
    }
    $Roots = @(Get-ChildItem -LiteralPath $Expanded -Directory)
    if ($Roots.Count -ne 1) { Fail "$Asset is incomplete: it must contain exactly one release directory." }
    $Root = $Roots[0].FullName
    foreach ($Required in @("blaze.exe", "blaze-engine.exe")) {
        if (-not (Test-Path -LiteralPath (Join-Path $Root $Required) -PathType Leaf)) {
            Fail "$Asset is incomplete: it has no $Required."
        }
    }

    $CargoHome = if ($env:CARGO_HOME) { [IO.Path]::GetFullPath($env:CARGO_HOME) } else { Join-Path $HOME ".cargo" }
    $Bin = Join-Path $CargoHome "bin"
    try { New-Item -ItemType Directory -Force -Path $Bin | Out-Null } catch {
        Fail "blaze's installation directory $Bin is not writable." $_.Exception.Message
    }

    $Current = Join-Path $Bin "blaze.exe"
    if (Test-Path -LiteralPath $Current) {
        try { & $Current stop *> $null } catch { }
    }

    $Payload = @(Get-ChildItem -LiteralPath $Root -File | Where-Object { $_.Extension -in @(".exe", ".wasm") })
    $Stage = Join-Path $Bin (".blaze-stage-" + [Guid]::NewGuid().ToString("N"))
    $Backup = Join-Path $Bin (".blaze-backup-" + [Guid]::NewGuid().ToString("N"))
    $Names = @()
    try {
        New-Item -ItemType Directory -Path $Stage | Out-Null
        New-Item -ItemType Directory -Path $Backup | Out-Null
        foreach ($File in $Payload) { Copy-Item -LiteralPath $File.FullName -Destination (Join-Path $Stage $File.Name) }

        $Names = @($Payload | ForEach-Object { $_.Name })
        $Obsolete = @("ra-jit.exe", "cargo-ra-jit.exe", "ra-jit-rustc.exe", "ra-jit-rustc-first.exe", "cargo-blaze.exe")
        foreach ($Name in @($Names + $Obsolete | Select-Object -Unique)) {
            $Installed = Join-Path $Bin $Name
            if (Test-Path -LiteralPath $Installed) { Move-Item -LiteralPath $Installed -Destination (Join-Path $Backup $Name) -Force }
        }
        foreach ($Name in $Names) { Move-Item -LiteralPath (Join-Path $Stage $Name) -Destination (Join-Path $Bin $Name) -Force }
    } catch {
        $SwapError = $_.Exception.Message
        foreach ($Name in $Names) {
            Remove-Item -LiteralPath (Join-Path $Bin $Name) -Force -ErrorAction SilentlyContinue
        }
        foreach ($File in @(Get-ChildItem -LiteralPath $Backup -File -ErrorAction SilentlyContinue)) {
            $Destination = Join-Path $Bin $File.Name
            Remove-Item -LiteralPath $Destination -Force -ErrorAction SilentlyContinue
            Move-Item -LiteralPath $File.FullName -Destination $Destination -Force
        }
        Fail "the upgrade could not replace the installed programs; the previous installation was restored." $SwapError
    } finally {
        Remove-Item -LiteralPath $Stage -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $Backup -Recurse -Force -ErrorAction SilentlyContinue
    }

    if (-not $NoToolchain -and -not $env:BLAZE_NO_TOOLCHAIN) {
        try { & (Join-Path $Bin "blaze.exe") toolchain | Out-Null } catch {
            Fail "blaze is installed, but Rust $Toolchain could not be downloaded; the first build can retry it." $_.Exception.Message
        }
    }
    Write-Output ""
    Write-Output "$Shown is installed."
    Write-Output "Run: $Bin\blaze.exe run"
} finally {
    Remove-Item -LiteralPath $Temp -Recurse -Force -ErrorAction SilentlyContinue
}
