# UmaDev native installer for Windows — no Node, no npm, no admin rights.
#
#   irm https://umadev.goder.ai/install.ps1 | iex
#
# Downloads the official release binary from GitHub Releases, verifies its
# published SHA-256, and installs it to %LOCALAPPDATA%\Programs\umadev (a
# per-user directory — never Program Files, never elevation). Set
# $env:UMADEV_VERSION to the release you need; the default is the latest release.
# Override the target directory with $env:UMADEV_INSTALL_DIR. Windows-on-ARM
# runs the x64 binary via built-in emulation, the same mapping the npm launcher
# uses. Every release binary embeds the curated knowledge corpus; native installs
# start with BM25 and may use UMADEV_EMBED_MODEL_DIR for a local vector model.

$ErrorActionPreference = 'Stop'

$repo = 'umacloud/umadev'
$asset = 'umadev-x86_64-pc-windows-msvc.exe'
$maxBinaryBytes = 512MB
$maxChecksumBytes = 4KB
$maxReleaseMetadataBytes = 1MB
$metadataTimeoutSeconds = 60
$binaryTimeoutSeconds = 900
$maxRedirects = 10
$binaryVersionTimeoutMilliseconds = 10000
$maxBinaryVersionOutputChars = 4096

Add-Type -AssemblyName System.Net.Http

function Normalize-UmaDevVersion {
    param([Parameter(Mandatory = $true)][string]$Value)

    $normalized = $Value.Trim()
    if ($normalized.StartsWith('v', [System.StringComparison]::OrdinalIgnoreCase)) {
        $normalized = $normalized.Substring(1)
    }
    if ($normalized -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$') {
        throw "invalid UmaDev release version: $Value"
    }
    return $normalized
}

function Assert-UmaDevBinaryVersion {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Expected,
        [Parameter(Mandatory = $true)][string]$Phase
    )

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $Path
    $startInfo.Arguments = '--version'
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $started = $false
    try {
        if (-not $process.Start()) { throw 'process did not start' }
        $started = $true
        # Do not drain concurrently into an unbounded in-memory string. Normal
        # --version output fits in the OS pipe; excessive output fills that
        # bounded pipe, stops the child, and is terminated by this same timeout.
        if (-not $process.WaitForExit($binaryVersionTimeoutMilliseconds)) {
            try { $process.Kill() } catch { }
            if (-not $process.WaitForExit(2000)) {
                throw 'candidate --version timed out and could not be terminated'
            }
            throw "candidate --version timed out after $binaryVersionTimeoutMilliseconds ms"
        }
        $stdout = $process.StandardOutput.ReadToEnd().TrimEnd()
        $stderr = $process.StandardError.ReadToEnd().TrimEnd()
        if (($stdout.Length + $stderr.Length) -gt $maxBinaryVersionOutputChars) {
            throw "candidate --version output exceeded $maxBinaryVersionOutputChars characters"
        }
        $exitCode = $process.ExitCode
    } catch {
        if ($started -and -not $process.HasExited) {
            try { $process.Kill() } catch { }
        }
        throw "$Phase failed: candidate did not run successfully: $($_.Exception.Message)"
    } finally {
        $process.Dispose()
    }

    $output = ((@($stdout, $stderr) | Where-Object { $_ }) -join [Environment]::NewLine).Trim()
    if ($exitCode -ne 0) {
        throw "$Phase failed with exit code $exitCode`: $output"
    }
    $match = [regex]::Match(
        $output,
        '^(?i:umadev)\s+v?(?<version>[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?)$'
    )
    if (-not $match.Success -or $match.Groups['version'].Value -ne $Expected) {
        if (-not $output) { $output = 'no version output' }
        throw "$Phase failed: expected UmaDev $Expected, got $output"
    }
}

function Test-UmaDevTrustedUri {
    param(
        [Parameter(Mandatory = $true)][System.Uri]$Uri,
        [Parameter(Mandatory = $true)][ValidateSet('Api', 'Release')][string]$Kind
    )

    if ($Uri.Scheme -cne 'https' -or $Uri.UserInfo -or (-not $Uri.IsDefaultPort -and $Uri.Port -ne 443)) {
        return $false
    }
    if ($Kind -eq 'Api') {
        return $Uri.Host -ieq 'api.github.com' -and
            $Uri.AbsolutePath.StartsWith('/repos/umacloud/umadev/releases/', [System.StringComparison]::Ordinal)
    }
    if ($Uri.Host -ieq 'github.com') {
        return $Uri.AbsolutePath.StartsWith('/umacloud/umadev/releases/download/', [System.StringComparison]::Ordinal)
    }
    return @(
        'release-assets.githubusercontent.com',
        'objects.githubusercontent.com',
        'github-releases.githubusercontent.com'
    ) -contains $Uri.Host.ToLowerInvariant()
}

function Invoke-UmaDevBoundedDownload {
    param(
        [Parameter(Mandatory = $true)][string]$Uri,
        [Parameter(Mandatory = $true)][string]$OutFile,
        [Parameter(Mandatory = $true)][long]$MaxBytes,
        [Parameter(Mandatory = $true)][int]$TimeoutSeconds,
        [Parameter(Mandatory = $true)][ValidateSet('Api', 'Release')][string]$Kind
    )

    $handler = [System.Net.Http.HttpClientHandler]::new()
    $handler.AllowAutoRedirect = $false
    $client = [System.Net.Http.HttpClient]::new($handler)
    $client.Timeout = [System.Threading.Timeout]::InfiniteTimeSpan
    $cancel = [System.Threading.CancellationTokenSource]::new(
        [System.TimeSpan]::FromSeconds($TimeoutSeconds)
    )
    $current = [System.Uri]::new($Uri)
    try {
        for ($hop = 0; $hop -le $maxRedirects; $hop++) {
            if (-not (Test-UmaDevTrustedUri -Uri $current -Kind $Kind)) {
                throw "refusing download outside official GitHub hosts: $current"
            }
            $request = [System.Net.Http.HttpRequestMessage]::new(
                [System.Net.Http.HttpMethod]::Get,
                $current
            )
            $request.Headers.UserAgent.ParseAdd('UmaDev-native-installer')
            $response = $null
            try {
                $response = $client.SendAsync(
                    $request,
                    [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead,
                    $cancel.Token
                ).GetAwaiter().GetResult()
                $status = [int]$response.StatusCode
                if ($status -ge 300 -and $status -lt 400) {
                    $location = $response.Headers.Location
                    if (-not $location) { throw "HTTP $status redirect had no Location header" }
                    $current = if ($location.IsAbsoluteUri) {
                        $location
                    } else {
                        [System.Uri]::new($current, $location)
                    }
                    continue
                }
                if (-not $response.IsSuccessStatusCode) {
                    throw "HTTP $status from $current"
                }
                $declared = $response.Content.Headers.ContentLength
                if ($null -ne $declared -and [long]$declared -gt $MaxBytes) {
                    throw "download exceeds $MaxBytes bytes"
                }

                $input = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
                $output = [System.IO.FileStream]::new(
                    $OutFile,
                    [System.IO.FileMode]::Create,
                    [System.IO.FileAccess]::Write,
                    [System.IO.FileShare]::None
                )
                try {
                    $buffer = New-Object byte[] 65536
                    [long]$total = 0
                    while (($read = $input.ReadAsync(
                        $buffer,
                        0,
                        $buffer.Length,
                        $cancel.Token
                    ).GetAwaiter().GetResult()) -gt 0) {
                        $total += $read
                        if ($total -gt $MaxBytes) { throw "download exceeds $MaxBytes bytes" }
                        $output.Write($buffer, 0, $read)
                    }
                    $output.Flush($true)
                } finally {
                    $output.Dispose()
                    $input.Dispose()
                }
                return $current.AbsoluteUri
            } finally {
                if ($response) { $response.Dispose() }
                $request.Dispose()
            }
        }
        throw "too many redirects (more than $maxRedirects)"
    } catch [System.OperationCanceledException] {
        throw "download timed out after $TimeoutSeconds seconds: $current"
    } finally {
        $cancel.Dispose()
        $client.Dispose()
        $handler.Dispose()
    }
}

function Get-UmaDevLatestVersion {
    $metadataFile = [System.IO.Path]::GetTempFileName()
    try {
        Invoke-UmaDevBoundedDownload `
            -Uri "https://api.github.com/repos/$repo/releases/latest" `
            -OutFile $metadataFile `
            -MaxBytes $maxReleaseMetadataBytes `
            -TimeoutSeconds $metadataTimeoutSeconds `
            -Kind Api | Out-Null
        $release = Get-Content -LiteralPath $metadataFile -Raw | ConvertFrom-Json
        if (-not $release.tag_name) { throw 'GitHub latest release did not include a tag_name' }
        return Normalize-UmaDevVersion ([string]$release.tag_name)
    } finally {
        Remove-Item -LiteralPath $metadataFile -Force -ErrorAction SilentlyContinue
    }
}

if ($env:UMADEV_VERSION) {
    $version = Normalize-UmaDevVersion $env:UMADEV_VERSION
    $base = "https://github.com/$repo/releases/download/v$version"
} else {
    # Resolve `latest` once, then pin both downloads and both runtime checks to
    # that exact tag. A release published mid-install cannot mix two versions.
    $version = Get-UmaDevLatestVersion
    $base = "https://github.com/$repo/releases/download/v$version"
}

if ($env:UMADEV_INSTALL_DIR) {
    $expandedDir = [Environment]::ExpandEnvironmentVariables($env:UMADEV_INSTALL_DIR)
    $dir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($expandedDir)
} else {
$dir = Join-Path $env:LOCALAPPDATA 'Programs\umadev'
}
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$dest = Join-Path $dir 'umadev.exe'
$installLockPath = Join-Path $dir '.umadev-install.lock'
$installLock = $null
$ownsInstallLock = $false

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("umadev-install-" + [System.Guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$stage = $null
$backup = $null
$preserveBackup = $false
$replacementStarted = $false
$installVerified = $false
$hadExisting = $false
try {
    Write-Host "Downloading $asset ..."
    $bin = Join-Path $tmp 'umadev.exe'
    $sha = Join-Path $tmp 'umadev.exe.sha256'
    Invoke-UmaDevBoundedDownload `
        -Uri "$base/$asset" `
        -OutFile $bin `
        -MaxBytes $maxBinaryBytes `
        -TimeoutSeconds $binaryTimeoutSeconds `
        -Kind Release | Out-Null
    Invoke-UmaDevBoundedDownload `
        -Uri "$base/$asset.sha256" `
        -OutFile $sha `
        -MaxBytes $maxChecksumBytes `
        -TimeoutSeconds $metadataTimeoutSeconds `
        -Kind Release | Out-Null

    $checksumText = (Get-Content -LiteralPath $sha -Raw).Trim()
    $expected = ($checksumText -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 -Path $bin).Hash.ToLowerInvariant()
    if ($expected -notmatch '^[0-9a-f]{64}$') { throw 'invalid published checksum' }
    if ($expected -ne $actual) {
        throw "SHA-256 mismatch (expected $expected, got $actual) - refusing to install"
    }

    Assert-UmaDevBinaryVersion `
        -Path $bin -Expected $version -Phase 'downloaded binary verification'

    # FileShare.None is an OS-released lock: concurrent installers targeting
    # the same directory serialize, and a killed process cannot leave a stale
    # lock that permanently blocks future updates.
    for ($attempt = 0; $attempt -lt 60 -and -not $installLock; $attempt++) {
        try {
            $installLock = [System.IO.FileStream]::new(
                $installLockPath,
                [System.IO.FileMode]::OpenOrCreate,
                [System.IO.FileAccess]::ReadWrite,
                [System.IO.FileShare]::None
            )
            $ownsInstallLock = $true
        } catch [System.IO.IOException] {
            Start-Sleep -Milliseconds 500
        }
    }
    if (-not $installLock) {
        throw "another UmaDev installer is updating $dest; wait for it to finish and retry"
    }

    $stage = Join-Path $dir ('.umadev-stage.' + [System.Guid]::NewGuid() + '.exe')
    [System.IO.File]::Copy($bin, $stage, $false)
    Assert-UmaDevBinaryVersion `
        -Path $stage -Expected $version -Phase 'staged binary verification'

    if (Test-Path -LiteralPath $dest) {
        $item = Get-Item -LiteralPath $dest -Force
        if ($item.PSIsContainer -or (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0)) {
            throw "refusing to replace non-regular install target: $dest"
        }
    }
    $hadExisting = [System.IO.File]::Exists($dest)
    if ($hadExisting) {
        $backup = Join-Path $dir ('.umadev-backup.' + [System.Guid]::NewGuid() + '.exe')
    }

    try {
        $replacementStarted = $true
        if ($hadExisting) {
            # File.Replace is same-volume and transactional: if Windows denies
            # replacement because an IDE, terminal, or UmaDev process holds the
            # image, the old destination remains intact and no backup is needed.
            [System.IO.File]::Replace($stage, $dest, $backup, $true)
        } else {
            [System.IO.File]::Move($stage, $dest)
        }
        $stage = $null
    } catch [System.IO.IOException] {
        # File.Replace/File.Move either completed or threw; a thrown operation
        # did not install our staged bytes. Do not let the outer finally block
        # delete a destination another concurrent first install may have won.
        $replacementStarted = $false
        throw "Could not replace $dest. The existing installation was left untouched. Close UmaDev, VS Code, Zcode, Codex, and terminals running UmaDev, then retry. Windows reported: $($_.Exception.Message)"
    } catch [System.UnauthorizedAccessException] {
        $replacementStarted = $false
        throw "Could not replace $dest because Windows denied access. The existing installation was left untouched. Close UmaDev, VS Code, Zcode, Codex, and terminals running UmaDev, then retry. Windows reported: $($_.Exception.Message)"
    }

    try {
        Assert-UmaDevBinaryVersion `
            -Path $dest -Expected $version -Phase 'installed binary verification'
    } catch {
        $verificationError = $_.Exception.Message
        if ($hadExisting -and $backup -and [System.IO.File]::Exists($backup)) {
            $failed = Join-Path $dir ('.umadev-failed.' + [System.Guid]::NewGuid() + '.exe')
            try {
                [System.IO.File]::Replace($backup, $dest, $failed, $true)
                $backup = $null
                $replacementStarted = $false
                Remove-Item -LiteralPath $failed -Force -ErrorAction SilentlyContinue
            } catch {
                $preserveBackup = $true
                throw "$verificationError Automatic rollback failed; the previous binary is preserved at $backup. Restore it before retrying. Windows reported: $($_.Exception.Message)"
            }
            throw "$verificationError The previous binary was restored automatically."
        }

        try {
            Remove-Item -LiteralPath $dest -Force -ErrorAction Stop
            $replacementStarted = $false
        } catch {
            throw "$verificationError The incomplete first-install binary could not be removed from $dest. Windows reported: $($_.Exception.Message)"
        }
        throw "$verificationError The incomplete first install was removed."
    }
    $installVerified = $true
    $replacementStarted = $false

    if ($backup -and [System.IO.File]::Exists($backup)) {
        Remove-Item -LiteralPath $backup -Force
        $backup = $null
    }

    Write-Host "Installed: $dest (v$version)"

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (($userPath -split ';') -notcontains $dir) {
        $newUserPath = if ([string]::IsNullOrWhiteSpace($userPath)) { $dir } else { "$userPath;$dir" }
        [Environment]::SetEnvironmentVariable('Path', $newUserPath, 'User')
        Write-Host "Added $dir to your user PATH - open a NEW terminal to use 'umadev'."
    }

    Write-Host "Run 'umadev doctor' to check bases and optional components."
} finally {
    # PowerShell runs `finally` on Ctrl+C. If interruption lands after the
    # destination swap but before its version check completes, restore the old
    # binary (or remove an unverified first install) before cleaning staging.
    if ($replacementStarted -and -not $installVerified) {
        if ($hadExisting -and $backup -and [System.IO.File]::Exists($backup)) {
            $interrupted = Join-Path $dir ('.umadev-interrupted.' + [System.Guid]::NewGuid() + '.exe')
            try {
                [System.IO.File]::Replace($backup, $dest, $interrupted, $true)
                $backup = $null
                Remove-Item -LiteralPath $interrupted -Force -ErrorAction SilentlyContinue
            } catch {
                $preserveBackup = $true
                [Console]::Error.WriteLine(
                    "Automatic rollback after interruption failed; the previous binary is preserved at $backup."
                )
            }
        } elseif (-not $hadExisting) {
            Remove-Item -LiteralPath $dest -Force -ErrorAction SilentlyContinue
        }
    }
    if ($stage) { Remove-Item -LiteralPath $stage -Force -ErrorAction SilentlyContinue }
    if ($backup -and -not $preserveBackup) {
        Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -Recurse -Force -LiteralPath $tmp -ErrorAction SilentlyContinue
    if ($installLock) {
        $installLock.Dispose()
        $installLock = $null
    }
    if ($ownsInstallLock) {
        Remove-Item -LiteralPath $installLockPath -Force -ErrorAction SilentlyContinue
    }
}
