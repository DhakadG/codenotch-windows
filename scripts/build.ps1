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

    The identity is read again after the build and the run fails if it moved, because an
    installer labelled with the wrong commit is worse than one labelled with none: it will be
    believed.

.PARAMETER Keep
    How many builds to keep in dist/. Older ones are deleted. Default 10, 0 keeps everything.
    Negative values are rejected.

.PARAMETER SkipBuild
    File an installer that has already been built, without rebuilding.
#>
[CmdletBinding()]
param(
    # 0 means keep everything. Negative is rejected rather than quietly treated as "keep
    # everything": the two readings of -1 are opposites, and silently picking one of them is
    # how a retention setting deletes something it was never asked to.
    [ValidateRange(0, [int]::MaxValue)]
    [int]$Keep = 10,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
Set-Location $repo

function Get-SourceIdentity {
    $sha = (git rev-parse --short=12 HEAD 2>$null)
    if (-not $sha) { $sha = 'nogit' }
    if ((git status --porcelain 2>$null)) { $sha = "$sha-dirty" }
    return $sha
}

$conf = Get-Content 'codenotch\tauri.conf.json' -Raw | ConvertFrom-Json
$version = $conf.version
$sha = Get-SourceIdentity
$stamp = Get-Date -Format 'yyyyMMdd-HHmmssfff'

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

# The identity was captured before the build. If the tree changed or HEAD moved while cargo
# was running, the binary was compiled from something other than what that identity names -
# and an installer labelled with the wrong commit is worse than one labelled with none, because
# it will be believed. build.rs stamps the binary from its own reading, so the two would also
# disagree with each other.
$after = Get-SourceIdentity
if ($after -ne $sha) {
    throw "the source changed during the build: started at $sha, now $after. Re-run once the tree is settled."
}

$built = Get-ChildItem 'target\release\bundle\nsis\*-setup.exe' -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $built) { throw 'no installer found under target\release\bundle\nsis' }

$dist = Join-Path $repo 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null

$name = "Codenotch-$version+$sha-$stamp-setup.exe"
$dest = Join-Path $dist $name
# Refuse rather than overwrite. The timestamp carries milliseconds, so a collision means two
# builds finished in the same millisecond from the same commit with the same dirty state -
# which should not happen, and if it does, silently replacing the first artifact would break
# the one guarantee this script exists to provide.
if (Test-Path $dest) {
    throw "an artifact already exists at $dest; refusing to overwrite it"
}
Copy-Item $built.FullName $dest

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
