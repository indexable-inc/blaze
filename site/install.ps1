# Installs blaze on Windows (x86-64), so that `blaze build`, `blaze test` and `blaze run` work, on a
# machine with or without Rust:
#
#   irm https://ix.dev/blaze/install.ps1 | iex
#   & ([scriptblock]::Create((irm https://ix.dev/blaze/install.ps1))) -Channel canary
#   & ([scriptblock]::Create((irm https://ix.dev/blaze/install.ps1))) -Version 0.20261028.0
#
# It does what install.sh does (docs/install.md): downloads the release built for this machine from
# https://blaze-releases.ix.dev (docs/release.md, "Where the bytes are"), checks it against the
# checksums published with it, puts `blaze` and the programs it runs in %CARGO_HOME%\bin
# (%USERPROFILE%\.cargo\bin when CARGO_HOME is not set), and has blaze download the Rust it builds
# with, once. It never asks anything. Running it again installs the newest release of the channel
# over the one there. It runs in Windows PowerShell 5.1 and in PowerShell 7.
#
#   -Channel C        stable (the default: releases that passed the heavy tests) or canary (the
#                     newest release, a prerelease until it is promoted); or $env:BLAZE_CHANNEL.
#   -Version V        install release V (or $env:BLAZE_VERSION), whatever the channel
#   -NoToolchain      download Rust at the first build instead ($env:BLAZE_NO_TOOLCHAIN=1)
#   -Detail           say each step
#
# $env:BLAZE_RELEASES=https://... installs from another place laid out as blaze's (what blaze reads
# too: <it>/v<version>/<asset>, <it>/channels/<channel>). $env:BLAZE_ARCHIVE=file installs that
# archive, a build of blaze laid out as a release's, instead of downloading one.

param(
    [string]$Channel = '',
    [string]$Version = '',
    [switch]$NoToolchain,
    [switch]$Detail
)

# A program of the installer that stops is an error with a plain message, not a stack of them; and
# `exit` would close the shell of whoever piped this into `iex`, so nothing here calls it.
function Install-Blaze {
    param([string]$Channel, [string]$Version, [bool]$NoToolchain, [bool]$Detail)

    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'

    $repo = 'indexable-inc/blaze'
    # harness/toolchain; harness/toolchain_bump.py writes it
    $toolchain = 'nightly-2026-10-27'
    $target = 'x86_64-pc-windows-msvc'
    $machine = 'Windows on x86-64'

    function Say([string]$text) { Write-Host $text }
    function Step([string]$text) { if ($Detail) { Write-Host "  $text" } }
    # What went wrong in plain words, then the details for whoever wants them.
    function Fail([string]$what, [string]$details = '') {
        $message = "error: $what"
        if ($details) { $message += "`n" + (($details -split "`r?`n" | ForEach-Object { "  $_" }) -join "`n") }
        throw $message
    }

    $releases = if ($env:BLAZE_RELEASES) { $env:BLAZE_RELEASES } else { 'https://blaze-releases.ix.dev' }
    $releases = $releases.TrimEnd('/')
    if (-not $releases.StartsWith('https://')) {
        Fail "BLAZE_RELEASES is $releases, which is not an https:// address." 'A release and its checksums come from there together, so they are fetched over HTTPS only.'
    }
    if (-not $Version) { $Version = $env:BLAZE_VERSION }
    if (-not $Channel) { $Channel = $env:BLAZE_CHANNEL }
    if (-not $NoToolchain -and $env:BLAZE_NO_TOOLCHAIN) { $NoToolchain = $true }
    $archive = $env:BLAZE_ARCHIVE

    # A workspace pins blaze with a version or a channel's name, and so does -Version.
    if ($Version -eq 'stable' -or $Version -eq 'canary') { $Channel = $Version; $Version = '' }
    if (-not $Channel -or $Channel -eq 'stable') { $Channel = 'stable' }
    elseif ($Channel -ne 'canary') { Fail "blaze has no channel $Channel." 'The channels are stable (the default) and canary.' }

    # -----------------------------------------------------------------------------------------
    # Which build this machine takes

    # A 32-bit PowerShell on 64-bit Windows says x86; PROCESSOR_ARCHITEW6432 says what the machine is.
    $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    if ($arch -ne 'AMD64') {
        Fail "blaze has no build for Windows on $arch yet." 'It runs on Windows on x86-64, on Linux (x86-64, ARM64) and on macOS with Apple silicon.'
    }

    # PowerShell 5.1 asks for TLS 1.0 and 1.1 unless told otherwise, and the release server takes neither.
    try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch { }

    # A program's exit code and what it said. Windows PowerShell 5.1 turns anything a program writes
    # to stderr into an error, which `Stop` makes terminating before the exit code is read.
    function Run([string]$program, [string[]]$arguments) {
        $was = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            $said = & $program @arguments 2>&1 | ForEach-Object { "$_" }
            $code = $LASTEXITCODE
        } finally { $ErrorActionPreference = $was }
        [pscustomobject]@{ Code = $code; Said = ($said -join "`n") }
    }
    # Three tries, as install.sh's `curl --retry 3`: a connection to the release server sometimes
    # stalls for good in Windows PowerShell 5.1 (about one in four here), and a request with a
    # time limit ends and goes through the next time. The limit is how long the answer may take to
    # begin; the bytes of a long download are not on the clock.
    function Fetch([string]$url, [string]$to, [int]$seconds = 30) {
        for ($try = 1; ; $try++) {
            try {
                Invoke-WebRequest -Uri $url -OutFile $to -UseBasicParsing -MaximumRedirection 5 -TimeoutSec $seconds
                return
            } catch {
                if ($try -ge 3) { throw }
                Step "trying again: $($_.Exception.Message)"
            }
        }
    }
    function Fetch-Text([string]$url) {
        $path = Join-Path $tmp ([IO.Path]::GetRandomFileName())
        Fetch $url $path
        Get-Content -Raw -LiteralPath $path
    }

    $tar = Join-Path $env:SystemRoot 'System32\tar.exe'
    if (-not (Test-Path -LiteralPath $tar)) { $tar = (Get-Command tar.exe -ErrorAction SilentlyContinue).Source }
    if (-not $tar) { Fail 'unpacking blaze needs tar.exe, which Windows 10 (1803) and newer have, and this system does not.' }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ('blaze-install-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        $tag = ''
        $shown = ''
        if ($archive) {
            if (-not (Test-Path -LiteralPath $archive -PathType Leaf)) {
                Fail "there is no file $archive to install blaze from." "BLAZE_ARCHIVE names a release's archive, such as blaze-v0.2.0-$target.tar.gz."
            }
            $asset = Split-Path -Leaf $archive
            Say "Installing blaze from $asset for $machine..."
            Copy-Item -LiteralPath $archive -Destination (Join-Path $tmp $asset)
        } else {
            # The release: the one asked for, else the one the channel's pointer names
            # (<releases>/channels/<channel>: its version and a newline, which release.yaml,
            # promote.yaml and yank.yaml keep at the channel's newest release that is not yanked).
            if ($Version) {
                $tag = 'v' + $Version.TrimStart('v')
            } else {
                try { $named = (Fetch-Text "$releases/channels/$Channel").Trim() }
                catch { Fail "which release is blaze's $Channel could not be read; is the network up?" "$($_.Exception.Message)`n$releases/channels/$Channel" }
                if ($named -notmatch '^[0-9]+\.[0-9]+\.[0-9]+') { Fail "blaze's $Channel channel names $named, which is not a version." "$releases/channels/$Channel" }
                $tag = "v$named"
            }
            Step "release $tag from $releases ($Channel)"

            try { $sums = Fetch-Text "$releases/$tag/SHA256SUMS" }
            catch {
                if ($Version) { Fail "there is no blaze release $Version." "Releases: https://github.com/$repo/releases" }
                Fail 'the list of blaze''s releases could not be read; is the network up?' "$($_.Exception.Message)`n$releases/$tag/SHA256SUMS"
            }
            # "<sha256>  <file>" or "<sha256> *<file>", a line each.
            $listed = @{}
            foreach ($line in ($sums -split "`r?`n")) {
                if ($line -match '^([0-9a-fA-F]{64})\s+\*?(.+?)\s*$') { $listed[$Matches[2]] = $Matches[1].ToLowerInvariant() }
            }
            # The .tar.xz, a third of the size, where the release has one (Windows' tar is bsdtar,
            # which reads xz itself); else the .tar.gz every release has.
            $asset = "blaze-$tag-$target.tar.gz"
            if ($listed.ContainsKey("blaze-$tag-$target.tar.xz")) { $asset = "blaze-$tag-$target.tar.xz" }
            if (-not $listed.ContainsKey($asset)) {
                Fail "blaze release $tag has no build for $machine." "Its files: https://github.com/$repo/releases/tag/$tag"
            }
            $want = $listed[$asset]

            $shown = 'blaze ' + $tag.TrimStart('v')
            if (-not $Version -and $Channel -eq 'canary') { $shown += ' (canary)' }
            Say "Installing $shown for $machine..."
            Step "downloading $asset"
            try { Fetch "$releases/$tag/$asset" (Join-Path $tmp $asset) 300 }
            catch { Fail 'the download of blaze stopped before it finished; running this again retries it.' "$($_.Exception.Message)`n$releases/$tag/$asset" }
            $got = (Get-FileHash -LiteralPath (Join-Path $tmp $asset) -Algorithm SHA256).Hash.ToLowerInvariant()
            if ($got -ne $want) {
                Fail 'the download is not the file the release published, so it was not installed. Running this again downloads it again.' "${asset}: sha256 $got, the release's SHA256SUMS says $want"
            }
        }

        $unpacked = Join-Path $tmp 'unpacked'
        New-Item -ItemType Directory -Path $unpacked | Out-Null
        # tar tells .tar.xz from .tar.gz itself.
        $unpack = Run $tar @('-xf', (Join-Path $tmp $asset), '-C', $unpacked)
        if ($unpack.Code -ne 0) { Fail 'the downloaded archive could not be unpacked.' $unpack.Said }
        # The archive's one folder, whatever the file is named.
        $folders = @(Get-ChildItem -LiteralPath $unpacked -Directory)
        $hint = "Download blaze-<version>-$target.tar.gz from https://github.com/$repo/releases and name it in BLAZE_ARCHIVE."
        if ($folders.Count -ne 1) { Fail "$asset is not a blaze release's archive: it does not hold one folder of programs." $hint }
        $folder = $folders[0].FullName
        if (-not (Test-Path -LiteralPath (Join-Path $folder 'blaze.exe') -PathType Leaf)) {
            Fail "$asset is not a blaze release's archive: it has no blaze.exe in $($folders[0].Name)." $hint
        }

        # -------------------------------------------------------------------------------------
        # Installing

        $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
        $bin = Join-Path $cargoHome 'bin'
        try { New-Item -ItemType Directory -Force -Path $bin | Out-Null }
        catch { Fail "blaze's folder could not be made." $_.Exception.Message }

        # Windows refuses to overwrite or delete a program that runs, such as the daemon, but it
        # lets one be renamed: the one there is moved aside, the new one put in its place, and the
        # ones moved aside by an earlier install go if nothing runs them any more. A daemon
        # running the one before goes on running it, and is replaced by the next command.
        Get-ChildItem -LiteralPath $bin -Filter '*.blaze-old' -Force -ErrorAction SilentlyContinue |
            ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }
        function Put([string]$from, [string]$name) {
            $to = Join-Path $bin $name
            $new = Join-Path $bin ".$name.blaze-new"
            Copy-Item -LiteralPath $from -Destination $new -Force
            if (Test-Path -LiteralPath $to) {
                $old = Join-Path $bin "$name.$PID.blaze-old"
                Move-Item -LiteralPath $to -Destination $old -Force
                Remove-Item -LiteralPath $old -Force -ErrorAction SilentlyContinue
            }
            Move-Item -LiteralPath $new -Destination $to -Force
        }
        foreach ($program in Get-ChildItem -LiteralPath $folder -File -Filter '*.exe') {
            Step "installing $($program.Name)"
            Put $program.FullName $program.Name
        }
        # The explorer core the daemon serves the page, read from beside its binary.
        $core = Join-Path $folder 'explore-core.wasm'
        if (Test-Path -LiteralPath $core -PathType Leaf) {
            Step 'installing explore-core.wasm'
            Put $core 'explore-core.wasm'
        }

        # blaze's updates (docs/install.md, "Updates"): the blaze installed last runs, so a default
        # an earlier `blaze update` installed goes; and a canary install has `blaze update` follow
        # canary, unless its settings name a channel already.
        $blazeHome = if ($env:BLAZE_HOME) { $env:BLAZE_HOME } else { Join-Path $env:USERPROFILE '.blaze' }
        Remove-Item -LiteralPath (Join-Path $blazeHome 'default') -Force -ErrorAction SilentlyContinue
        if ($Channel -eq 'canary' -and -not $Version) {
            $config = Join-Path $blazeHome 'config.toml'
            $lines = if (Test-Path -LiteralPath $config) { @(Get-Content -LiteralPath $config) } else { @() }
            # config.toml read as TOML enough for this: a table header (comments after it allowed),
            # and the key `channel` in the table [update] (or `update.channel` before any table).
            $here = $false; $top = $true; $named = $false; $header = -1
            for ($i = 0; $i -lt $lines.Count; $i++) {
                $line = $lines[$i]
                if ($line -match '^\s*\[') {
                    $top = $false
                    $here = $line -match '^\s*\[\s*update\s*\]\s*(#.*)?$'
                    if ($here -and $header -lt 0) { $header = $i }
                } elseif ($here -and $line -match '^\s*channel\s*=') { $named = $true }
                elseif ($top -and $line -match '^\s*update\s*\.\s*channel\s*=') { $named = $true }
            }
            if (-not $named) {
                New-Item -ItemType Directory -Force -Path $blazeHome | Out-Null
                if ($header -ge 0) {
                    $out = @($lines[0..$header]) + 'channel = "canary"'
                    if ($header + 1 -lt $lines.Count) { $out += $lines[($header + 1)..($lines.Count - 1)] }
                    Set-Content -LiteralPath $config -Value $out
                } else {
                    Add-Content -LiteralPath $config -Value @('', '[update]', 'channel = "canary"')
                }
                Step "``blaze update`` follows canary: [update] channel in $config"
            }
        }

        # -------------------------------------------------------------------------------------
        # The Rust blaze builds with

        # blaze downloads its own copy, once, unless rustup has the one it builds with.
        if ($NoToolchain) {
            Step "Rust $toolchain is downloaded at the first build"
        } else {
            $rust = Run (Join-Path $bin 'blaze.exe') @('toolchain')
            if ($rust.Code -ne 0) {
                Fail 'blaze is installed, but the Rust it builds with could not be downloaded; the first build tries again.' $rust.Said
            }
            Step $rust.Said.Trim()
        }

        # Rust links a program with MSVC's linker, which blaze does not bring.
        $linkerNote = ''
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
        $tools = $null
        if (Test-Path -LiteralPath $vswhere) {
            $tools = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
        }
        if (-not $tools) {
            $linkerNote = "blaze links programs with the Visual C++ build tools (link.exe and the Windows SDK), and none is installed. Install them with:`n`n" +
                "    winget install Microsoft.VisualStudio.2022.BuildTools --override `"--passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended`""
        }

        # -------------------------------------------------------------------------------------
        # What to do next

        $pathNote = ''
        $onPath = @($env:PATH -split ';' | Where-Object { $_ }) | Where-Object { $_.TrimEnd('\') -ieq $bin.TrimEnd('\') }
        if (-not $onPath) {
            $pathNote = "Your shell finds blaze once your PATH has $bin, which it does not yet. Add it for your user with:`n`n" +
                "    [Environment]::SetEnvironmentVariable('Path', `"$bin;`" + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User')`n`n" +
                'and open a new terminal to use it.'
        }

        Say ''
        if ($tag) { Say "$shown is installed." } else { Say 'blaze is installed.' }
        if ($linkerNote) { Say ''; Say $linkerNote }
        if ($pathNote) { Say ''; Say $pathNote }
        Say ''
        Say 'In a Rust project, run:'
        Say ''
        Say '    blaze run'
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

try {
    Install-Blaze -Channel $Channel -Version $Version -NoToolchain ([bool]$NoToolchain) -Detail ([bool]$Detail)
} catch {
    # Write-Host, not stderr: it is the one stream that reaches the screen under `irm | iex`, and a
    # caller captures it with `*>&1`.
    Write-Host $_.Exception.Message -ForegroundColor Red
    $global:LASTEXITCODE = 1
    # Run as a file (`./install.ps1`, `-File`) it ends with the code; under `irm | iex` and from
    # `[scriptblock]::Create` there is no file, and `exit` would close the caller's shell.
    if ($PSCommandPath) { exit 1 }
}
