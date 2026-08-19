param(
    [string]$Version
)

$ErrorActionPreference = "Stop"

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
Set-Location $repoRoot

$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
if (Test-Path $cargoBin) {
    $env:PATH += ";$cargoBin"
}

$tauriConfigPath = Join-Path $repoRoot "src-tauri\tauri.conf.json"
$tauriConfig = Get-Content $tauriConfigPath -Raw | ConvertFrom-Json
$gitTag = if ($Version) { $Version.Trim() } else { $env:HKM_RELEASE_VERSION }
if (-not $gitTag) {
    # Pick the target version from changelog/vX.Y.Z.md. Prefer the highest changelog that
    # has NOT been released yet (no git tag, no releases/ dir) so a minor/major bump there
    # is honored. If every changelog is already released, fall back to the highest existing
    # one and rebuild it, so pressing Build always produces a build instead of failing.
    $releasedTags = @(& git tag 2>$null | ForEach-Object { $_.Trim() })
    $changelogs = @(
        Get-ChildItem -Path (Join-Path $repoRoot "changelog") -Filter "v*.md" -File -ErrorAction SilentlyContinue |
        ForEach-Object {
            if ($_.BaseName -match '^v(\d+)\.(\d+)\.(\d+)$') {
                $tag = "v$($matches[1]).$($matches[2]).$($matches[3])"
                $isReleased = ($releasedTags -contains $tag) -or (Test-Path (Join-Path $repoRoot "releases\$tag"))
                [pscustomobject]@{
                    Tag        = $tag
                    Version    = [version]"$($matches[1]).$($matches[2]).$($matches[3])"
                    IsReleased = $isReleased
                }
            }
        }
    )
    if (-not $changelogs) {
        throw "No changelog found in changelog\. Create changelog\v<next>.md first (see AGENTS.md)."
    }
    $unreleased = @($changelogs | Where-Object { -not $_.IsReleased } | Sort-Object Version -Descending)
    if ($unreleased.Count) {
        $gitTag = $unreleased[0].Tag
    } else {
        $latest = $changelogs | Sort-Object Version -Descending | Select-Object -First 1
        $gitTag = $latest.Tag
        Write-Host "No unreleased changelog; rebuilding the latest released version $($latest.Tag)."
    }
}
$tagName = if ($gitTag.StartsWith("v")) { $gitTag } else { "v$gitTag" }
$version = $tagName.TrimStart("v")
if (-not $version) {
    throw "Could not determine release version from git tag '$gitTag'."
}

$tauriConfig.version = $version
$tauriConfig | ConvertTo-Json -Depth 20 | Set-Content $tauriConfigPath

$cargoTomlPath = Join-Path $repoRoot "src-tauri\Cargo.toml"
$cargoToml = Get-Content $cargoTomlPath
$updatedCargoVersion = $false
$cargoToml = $cargoToml | ForEach-Object {
    if (-not $updatedCargoVersion -and $_ -match '^version\s*=\s*"') {
        $updatedCargoVersion = $true
        "version = `"$version`""
    } else {
        $_
    }
}
$cargoToml | Set-Content $cargoTomlPath

$releaseDir = Join-Path $repoRoot "releases\$tagName"
$bundleDir = Join-Path $repoRoot "src-tauri\target\release\bundle"
$changelogPath = Join-Path $repoRoot "changelog\$tagName.md"
$sourceAhkExe = $env:HKM_AHK_EXE
if (-not $sourceAhkExe) {
    $sourceAhkExe = "D:\Apps\AutoHotkey\_App\v2\AutoHotkey64.exe"
}
$bundledAhkDir = Join-Path $repoRoot "src-tauri\resources\autohotkey"
$bundledAhkExe = Join-Path $bundledAhkDir "AutoHotkey64.exe"

if (-not (Test-Path -LiteralPath $sourceAhkExe -PathType Leaf)) {
    throw "AutoHotkey v2 not found at '$sourceAhkExe'. Set HKM_AHK_EXE to the AutoHotkey64.exe path."
}
if (-not (Test-Path -LiteralPath $changelogPath -PathType Leaf)) {
    throw "Changelog not found at changelog\$tagName.md"
}
New-Item -ItemType Directory -Path $bundledAhkDir -Force | Out-Null
Copy-Item -LiteralPath $sourceAhkExe -Destination $bundledAhkExe -Force

if (Test-Path $releaseDir) {
    Remove-Item -LiteralPath $releaseDir -Recurse -Force
}
New-Item -ItemType Directory -Path $releaseDir | Out-Null

if (Test-Path $bundleDir) {
    Remove-Item -LiteralPath $bundleDir -Recurse -Force
}

npm run tauri build

$artifactPatterns = @(
    "nsis\*.exe",
    "msi\*.msi"
)

$copied = @()
foreach ($pattern in $artifactPatterns) {
    $matches = Get-ChildItem -Path (Join-Path $bundleDir $pattern) -File -ErrorAction SilentlyContinue
    foreach ($artifact in $matches) {
        if (-not $artifact.Name.Contains($version)) {
            continue
        }
        Copy-Item -LiteralPath $artifact.FullName -Destination $releaseDir
        $copied += $artifact.Name
    }
}

if ($copied.Count -eq 0) {
    throw "Build finished, but no release artifacts matching version $version were found in src-tauri\target\release\bundle"
}

Write-Host ""
Write-Host "Release artifacts copied to releases\${tagName}:"
$copied | ForEach-Object { Write-Host " - $_" }
Write-Host ""
Write-Host "Next:"
Write-Host "  git add ."
Write-Host "  git commit -m `"Release $tagName`""
Write-Host "  git tag $tagName"
Write-Host "  git push"
Write-Host "  git push origin $tagName"
