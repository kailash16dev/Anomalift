# Install anomalift on Windows. No Rust, no MSVC build tools, no admin rights.
#
#   irm https://raw.githubusercontent.com/kailash16dev/Anomalift/main/install.ps1 | iex
#
# Windows PowerShell 5.1 is the floor, because it is what ships in the box on
# Windows 10 and 11 - a friend who must install PowerShell 7 first has already
# stopped. So nothing below uses 7+ syntax: no ternaries, no ??, no -Parallel,
# no Invoke-RestMethod niceties that only exist in Core.

# irm | iex evaluates this in the caller's live session, so any preference we
# set here would outlive the install and quietly change how their shell behaves
# afterwards. Remember both and put them back in the finally block.
$anomaliftPrevErrorAction = $ErrorActionPreference
$anomaliftPrevProgress    = $ProgressPreference

$ErrorActionPreference = 'Stop'
# Invoke-WebRequest in 5.1 repaints a progress bar on every chunk it receives,
# which for a 671KB download costs more wall clock than the transfer does.
$ProgressPreference = 'SilentlyContinue'

# Named to match say/die in install.sh - the two scripts should read as one
# pair. They do leak into the caller's session under `irm | iex`, along with the
# variables below; that is the standing trade every piped installer makes, and
# the preferences restored in `finally` are the part that actually matters.
function Say([string]$m) { Write-Host "  $m" }
# Deliberately `throw`, not `exit`. Under `irm | iex` this script runs *as* the
# user's session, and `exit` there closes their terminal window - error message
# and all. The trailing `if` at the bottom turns a failure back into a real
# exit code when the file is run as a script, which is when that matters.
function Die([string]$m) { throw $m }

$tmp = $null
$anomaliftFailed = $false

try {
    $Repo    = $env:ANOMALIFT_REPO
    if (-not $Repo)    { $Repo = 'kailash16dev/Anomalift' }
    $Version = $env:ANOMALIFT_VERSION
    if (-not $Version) { $Version = 'latest' }

    $BinDir = $env:ANOMALIFT_BIN_DIR
    if (-not $BinDir) {
        if (-not $env:LOCALAPPDATA) {
            Die 'LOCALAPPDATA is not set - set ANOMALIFT_BIN_DIR to where you want anomalift.exe'
        }
        # Under LOCALAPPDATA\Programs because that is the one place a user can
        # write without a UAC prompt, and where per-user installs are expected
        # to live (VS Code and GitHub Desktop install themselves here too).
        $BinDir = Join-Path $env:LOCALAPPDATA 'Programs\anomalift'
    }

    Write-Host ''
    Write-Host '  anomalift'
    Write-Host ''

    # --- what are we on ------------------------------------------------------

    # PROCESSOR_ARCHITECTURE reports x86 inside a 32-bit PowerShell on a 64-bit
    # OS, so ask .NET first and keep the environment variables as the fallback
    # for anything older than .NET 4.7.1.
    $archRaw = $null
    try {
        $archRaw = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    } catch {
        $archRaw = $null
    }
    if (-not $archRaw) {
        if ($env:PROCESSOR_ARCHITEW6432) { $archRaw = $env:PROCESSOR_ARCHITEW6432 }
        else                             { $archRaw = $env:PROCESSOR_ARCHITECTURE }
    }

    $archName = $null
    switch -Regex ($archRaw) {
        '^(x64|amd64)$' { $archName = 'x86_64' }
        '^arm64$'       { $archName = 'arm64' }
        default         { Die "unsupported architecture: $archRaw" }
    }

    # The release workflow builds exactly one Windows target,
    # x86_64-pc-windows-msvc. On Windows on ARM that binary runs under the OS's
    # built-in x64 emulation: slower than native, but it works, and it is the
    # only thing on offer. Say so out loud - an "arm64" machine silently
    # downloading something labelled x86_64 looks like a bug otherwise.
    if ($archName -eq 'arm64') {
        Say 'no arm64 build is published; using the x86_64 build (Windows on ARM emulates x64)'
    }

    # Note the asymmetry, it is not a typo: the release job names the binary
    # `<name>.exe` but the checksum `<name>.sha256` - the extension comes from
    # matrix.ext, which is appended to the binary and not to the sum file.
    $assetBase = 'anomalift-windows-x86_64'
    $assetFile = "$assetBase.exe"

    # --- download ------------------------------------------------------------

    if ($Version -eq 'latest') {
        $base = "https://github.com/$Repo/releases/latest/download"
    } else {
        $base = "https://github.com/$Repo/releases/download/$Version"
    }
    $url    = "$base/$assetFile"
    $sumUrl = "$base/$assetBase.sha256"

    # 5.1 negotiates whatever the machine default is, and on a stock or
    # unpatched Windows 10 that can still be TLS 1.0, which github.com refuses
    # outright. -bor rather than = so a newer default (1.3 under PowerShell 7)
    # is not downgraded on the way past.
    try {
        [Net.ServicePointManager]::SecurityProtocol =
            [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    } catch {
        # Not fatal: PowerShell 7 ignores ServicePointManager entirely.
    }

    $tmp = Join-Path $env:TEMP ('anomalift-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp -Force | Out-Null
    $exeTmp = Join-Path $tmp 'anomalift.exe'
    $sumTmp = Join-Path $tmp 'expected.sha256'

    Say "downloading $assetFile"
    try {
        # -UseBasicParsing because 5.1 otherwise spins up the Internet Explorer
        # DOM parser, which throws on any machine where IE first-run setup was
        # never completed. PowerShell 7 accepts the switch and ignores it.
        Invoke-WebRequest -Uri $url -OutFile $exeTmp -UseBasicParsing
    } catch {
        Die "download failed: $url`n     If this is a fresh repository, there may be no release yet."
    }

    # --- verify --------------------------------------------------------------
    #
    # A piped installer already asks for a lot of trust. Verifying the published
    # checksum is the least it can do; a mismatch means stop, never "probably
    # fine".

    $expected = $null
    try {
        Invoke-WebRequest -Uri $sumUrl -OutFile $sumTmp -UseBasicParsing
        $line = Get-Content -LiteralPath $sumTmp -TotalCount 1
        if ($line) { $expected = ($line.Trim() -split '\s+')[0] }
    } catch {
        $expected = $null
    }

    if ($expected) {
        $actual = (Get-FileHash -LiteralPath $exeTmp -Algorithm SHA256).Hash
        # -ne on strings is case-insensitive in PowerShell, which is what we
        # want here: Get-FileHash returns uppercase hex and shasum writes
        # lowercase, so a case-sensitive compare would fail on every install.
        if ($actual -ne $expected) {
            Die "checksum mismatch - refusing to install`n     expected $expected`n     got      $actual"
        }
        Say 'checksum verified'
    } elseif ($env:ANOMALIFT_SKIP_CHECKSUM -eq '1') {
        Say 'warning: checksum skipped at your request'
    } else {
        # Matches install.sh. The workflow publishes a .sha256 beside every
        # asset unconditionally, so a missing one means something is wrong with
        # the release - not a condition to shrug at and install anyway. These
        # two scripts shipping opposite postures meant Windows users were held
        # to a weaker standard than everyone else, silently.
        Die "no published checksum for $asset - refusing to install.`n     Re-run with ANOMALIFT_SKIP_CHECKSUM=1 if you accept the risk."
    }

    # --- install -------------------------------------------------------------

    if (-not (Test-Path -LiteralPath $BinDir)) {
        New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
    }
    # Make it absolute before it goes anywhere near PATH. Someone setting
    # ANOMALIFT_BIN_DIR=bin gets a working install and a PATH entry that means
    # something different in every directory they cd into.
    $BinDir = (Resolve-Path -LiteralPath $BinDir).Path
    $target = Join-Path $BinDir 'anomalift.exe'

    $moved = $false
    for ($i = 1; $i -le 3; $i++) {
        try {
            Move-Item -LiteralPath $exeTmp -Destination $target -Force
            $moved = $true
            break
        } catch {
            # A freshly downloaded exe is routinely held open for a second or
            # two by real-time AV scanning, and that lock clears on its own. A
            # running anomalift.exe does not, which is what the check below is
            # for. Windows refuses to overwrite a running image outright, and
            # the raw exception for it ("being used by another process") does
            # not tell anyone what to actually do.
            if ($i -lt 3) { Start-Sleep -Milliseconds 500 }
        }
    }

    if (-not $moved) {
        $running = @(Get-Process -Name 'anomalift' -ErrorAction SilentlyContinue)
        if ($running.Count -gt 0) {
            Die "anomalift is currently running (PID $($running[0].Id)) and Windows will not let an installer replace a running program.`n     Close it, then run this again."
        }
        Die "could not write $target`n     Something is holding that file, or the directory is not writable. Set ANOMALIFT_BIN_DIR to somewhere else and try again."
    }

    # After the move, not before: the mark-of-the-web is an NTFS alternate data
    # stream, and clearing it on the file that will actually be executed is the
    # only version of this that is guaranteed to hold (a cross-volume move is a
    # copy, and what survives a copy is not worth reasoning about).
    #
    # Invoke-WebRequest does not attach a mark in the first place, so this is
    # usually a no-op - but Group Policy and some AV products add one behind our
    # back, and a marked exe is what produces "Windows protected your PC".
    # Clearing it is the same trust decision the user already made by running
    # this script, and the summary below tells them we did it rather than having
    # it happen silently.
    Unblock-File -LiteralPath $target -ErrorAction SilentlyContinue

    Say "installed to $target"

    # --- PATH ----------------------------------------------------------------

    # The *user* PATH, never the machine PATH: the machine one needs admin, and
    # the classic way to wreck a Windows box is to read $env:Path - which is
    # user and machine already merged - and write that back into one of them.
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($null -eq $userPath) { $userPath = '' }

    $wanted = $BinDir.TrimEnd('\')
    $onUserPath = $false
    foreach ($entry in ($userPath -split ';')) {
        $e = $entry.Trim().TrimEnd('\')
        if ($e -and ($e -eq $wanted)) { $onUserPath = $true }
    }

    $resolvableNow = $false
    foreach ($entry in ("$env:Path" -split ';')) {
        $e = $entry.Trim().TrimEnd('\')
        if ($e -and ($e -eq $wanted)) { $resolvableNow = $true }
    }

    $addedToPath = $false
    if (-not $onUserPath -and -not $resolvableNow) {
        if ($userPath.TrimEnd(';')) {
            $newPath = $userPath.TrimEnd(';') + ';' + $BinDir
        } else {
            $newPath = $BinDir
        }
        # Caveat worth knowing: this writes the value back as a plain string,
        # so an entry like %JAVA_HOME%\bin in the user PATH is frozen to
        # whatever it expands to today. Every mainstream installer has the same
        # behaviour, and the alternative - hand-editing HKCU\Environment to
        # preserve REG_EXPAND_SZ - is more ways to break someone's PATH than
        # this is.
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        $addedToPath = $true
    }

    # Environment variables are per-process, so this makes `anomalift` work in
    # *this* window immediately. Every other open terminal keeps its old copy,
    # which is the single most common "I installed it and it didn't work".
    if (-not $resolvableNow) {
        $env:Path = "$env:Path;$BinDir"
    }

    Write-Host ''
    if ($addedToPath) {
        Write-Host "  Added $BinDir to your user PATH."
        Write-Host '  This window works now. Any terminal that was already open must be'
        Write-Host '  closed and reopened before it can see anomalift.'
        Write-Host ''
    }
    Write-Host '  Run it:'
    Write-Host ''
    Write-Host '    anomalift'
    Write-Host ''
    Write-Host '  The binary is unsigned - code signing certificates cost money this'
    Write-Host '  project does not have. Windows may show a SmartScreen prompt ("Windows'
    Write-Host '  protected your PC") or Defender may pause the first run for a scan.'
    Write-Host '  This installer ran Unblock-File on the download to clear the'
    Write-Host '  mark-of-the-web; if a prompt appears anyway, choose More info, then'
    Write-Host '  Run anyway.'
    Write-Host ''
}
catch {
    $anomaliftFailed = $true
    Write-Host ''
    Write-Host "  error: $($_.Exception.Message)" -ForegroundColor Red
    Write-Host ''
}
finally {
    # Every exit path, including a failed download half written to disk.
    if ($tmp -and (Test-Path -LiteralPath $tmp)) {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
    $ErrorActionPreference = $anomaliftPrevErrorAction
    $ProgressPreference    = $anomaliftPrevProgress
}

# $PSCommandPath is empty when this was piped into iex, and set when it was run
# as a saved .ps1. Only in the second case is `exit` safe - and only there does
# anyone care about the exit code.
if ($anomaliftFailed -and $PSCommandPath) { exit 1 }
