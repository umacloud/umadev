# UmaDev native installer for Windows — no Node, no npm, no admin rights.
#
#   irm https://umadev.goder.ai/install.ps1 | iex
#
# Downloads the official release binary from GitHub Releases, verifies its
# published SHA-256, and installs it to %LOCALAPPDATA%\Programs\umadev (a
# per-user directory — never Program Files, never elevation). Pin a version by
# setting $env:UMADEV_VERSION = '1.0.68' first; the default is the latest
# release. Windows-on-ARM runs the x64 binary via built-in emulation, the same
# mapping the npm launcher uses.

$ErrorActionPreference = 'Stop'

$repo = 'umacloud/umadev'
$asset = 'umadev-x86_64-pc-windows-msvc.exe'
if ($env:UMADEV_VERSION) {
    $base = "https://github.com/$repo/releases/download/v$($env:UMADEV_VERSION)"
} else {
    $base = "https://github.com/$repo/releases/latest/download"
}

$dir = Join-Path $env:LOCALAPPDATA 'Programs\umadev'
New-Item -ItemType Directory -Force -Path $dir | Out-Null

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("umadev-install-" + [System.Guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
try {
    Write-Host "Downloading $asset ..."
    $bin = Join-Path $tmp 'umadev.exe'
    $sha = Join-Path $tmp 'umadev.exe.sha256'
    Invoke-WebRequest -Uri "$base/$asset" -OutFile $bin -UseBasicParsing
    Invoke-WebRequest -Uri "$base/$asset.sha256" -OutFile $sha -UseBasicParsing

    $expected = (Get-Content $sha -Raw).Trim().Split(' ')[0].ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 -Path $bin).Hash.ToLowerInvariant()
    if (-not $expected) { throw 'empty published checksum' }
    if ($expected -ne $actual) {
        throw "SHA-256 mismatch (expected $expected, got $actual) - refusing to install"
    }

    Move-Item -Force -Path $bin -Destination (Join-Path $dir 'umadev.exe')
    Write-Host "Installed: $(Join-Path $dir 'umadev.exe')"

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (($userPath -split ';') -notcontains $dir) {
        [Environment]::SetEnvironmentVariable('Path', "$userPath;$dir", 'User')
        Write-Host "Added $dir to your user PATH - open a NEW terminal to use 'umadev'."
    }

    & (Join-Path $dir 'umadev.exe') --version
    Write-Host "Run 'umadev doctor' to check bases and optional components."
} finally {
    Remove-Item -Recurse -Force -Path $tmp -ErrorAction SilentlyContinue
}
