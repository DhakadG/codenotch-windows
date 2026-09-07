<#
.SYNOPSIS
    Builds the installer and files it under dist/ by its exact build identity.

.DESCRIPTION
    `cargo tauri build` always writes the same filename, `Codenotch_<version>_x64-setup.exe`,
    so every build silently replaces the last one. Two installers built from different commits
    are then indistinguishable on disk, which is how a build without a fix in it gets tested
    for an hour under the impression it had one.

    This copies each build to:

        dist/Codenotch-<version>+<shortsha>[-dirty]-<yyyyMMdd-HHmmss>-setup.exe

    The same identity the binary reports from `codenotch.exe version`, so an installer, a
    running process and a log line can always be matched to each other and to a commit.

    The semantic version still comes from tauri.conf.json and only moves when a release is
    made. Granularity between releases comes from the commit and the timestamp, which cannot
    drift the way a hand-incremented number does.

    Also writes a .sha256 beside each artifact, and dist/latest.txt naming the newest build.

.PARAMETER Keep
    How many builds to keep in dist/. Older ones are deleted. Default 10, 0 keeps everything.

.PARAMETER SkipBuild
    File an installer that has already been built, without rebuilding.
#>
[CmdletBinding()]
param(
    [int]$Keep = 10,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
Set-Location $repo

$conf = Get-Content 'codenotch\tauri.conf.json' -Raw | ConvertFrom-Json
$version = $conf.version

# The identity is resolved before the build, so a tree edited mid-build is reported as dirty
# rather than being attributed to a commit it does not match.
$sha = (git rev-parse --short=12 HEAD 2>$null)
if (-not $sha) { $sha = 'nogit' }
if ((git status --porcelain 2>$null)) { $sha = "$sha-dirty" }
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'

if (-not $SkipBuild) {
    Write-Host "Building Codenotch $version ($sha)..."
    Push-Location 'codenotch'
    try {
        & cargo tauri build --config tauri.bundle.conf.json
        if ($LASTEXITCODE -ne 0) { throw "cargo tauri build failed with exit code $LASTEXITCODE" }
    } finally {
        Pop-Location
    }
}

$built = Get-ChildItem 'target\release\bundle\nsis\*-setup.exe' -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $built) { throw 'no installer found under target\release\bundle\nsis' }

$dist = Join-Path $repo 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null

$name = "Codenotch-$version+$sha-$stamp-setup.exe"
$dest = Join-Path $dist $name
Copy-Item $built.FullName $dest -Force

$hash = (Get-FileHash $dest -Algorithm SHA256).Hash.ToLower()
"$hash  $name" | Set-Content "$dest.sha256" -Encoding ascii
$name | Set-Content (Join-Path $dist 'latest.txt') -Encoding ascii

Write-Host ''
Write-Host "  $name"
Write-Host ("  {0:N2} MB   sha256 {1}" -f ($built.Length / 1MB), $hash.Substring(0, 16))

# Retention. Each build is ~3 MB, so an unbounded dist/ is only a slow problem - but it is
# still a problem, and the newest is the one anyone wants.
if ($Keep -gt 0) {
    $old = Get-ChildItem (Join-Path $dist 'Codenotch-*-setup.exe') |
        Sort-Object LastWriteTime -Descending | Select-Object -Skip $Keep
    foreach ($f in $old) {
        Remove-Item $f.FullName -Force
        Remove-Item "$($f.FullName).sha256" -Force -ErrorAction SilentlyContinue
        Write-Host "  pruned $($f.Name)"
    }
}
