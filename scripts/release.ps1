# Formal release pipeline for ouuutput:
#   1. Derive version from Cargo.toml (and check CHANGELOG.md entry)
#   2. Package dist\ouuutput-<ver>-win64.zip via make-portable.ps1 (unless -SkipPack)
#   3. Upload zip + latest.json to Cloudflare R2 (ouuut-media / media.ouuut.com) and verify
#   4. Ensure GitHub Release v<ver> has the matching zip asset
#   5. Sync & deploy official site (cnjpinput -> https://input.ouuut.com/)
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts\release.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\release.ps1 -SkipPack
#
# Keep this file pure ASCII (PowerShell 5.1 parses un-BOM scripts as system ANSI).

param(
    [string]$Version = "",
    [switch]$SkipPack,
    [switch]$SkipR2,
    [switch]$SkipFullSha,
    [switch]$SkipGitHub,
    [switch]$ClobberGitHub,
    [switch]$SkipSite,
    [string]$SiteDir = ""
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

# 1. Version SSOT from Cargo.toml
$Cargo = Get-Content (Join-Path $Root "Cargo.toml") -Raw
$m = [regex]::Match($Cargo, '(?m)^version\s*=\s*"([^"]+)"')
if (-not $m.Success) { throw "Cannot find workspace version in Cargo.toml" }
$CargoVersion = $m.Groups[1].Value.Trim()
if ([string]::IsNullOrWhiteSpace($Version)) {
    $Version = $CargoVersion
} else {
    $Version = $Version.Trim().TrimStart('v')
    if ($Version -ne $CargoVersion) {
        throw "Version mismatch: Cargo.toml=$CargoVersion, -Version=$Version"
    }
}
$Tag = "v$Version"
Write-Host "[release] version = $Version (tag $Tag)"

$ChangelogPath = Join-Path $Root "CHANGELOG.md"
if (Test-Path $ChangelogPath) {
    $Changelog = Get-Content $ChangelogPath -Raw
    if ($Changelog -notmatch [regex]::Escape("## [$Version]")) {
        throw "CHANGELOG.md does not contain section '## [$Version]'"
    }
}

# 2. Build & pack portable zip
if (-not $SkipPack) {
    Write-Host "[release] running scripts\make-portable.ps1 -Overwrite"
    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot "make-portable.ps1") -Version $Version -Overwrite
    if ($LASTEXITCODE -ne 0) { throw "make-portable.ps1 failed ($LASTEXITCODE)" }
}

$Zip = Join-Path $Root "dist\ouuutput-$Version-win64.zip"
if (-not (Test-Path $Zip)) { throw "Release zip missing: $Zip" }
$Bytes = (Get-Item $Zip).Length
$Sha256 = (Get-FileHash $Zip -Algorithm SHA256).Hash.ToLower()
Write-Host "[release] archive = $Zip ($Bytes bytes, sha256=$Sha256)"

$R2Url = "https://media.ouuut.com/ouuutput/releases/$Tag/ouuutput-$Version-win64.zip"
$GhUrl = "https://github.com/nakamotosai/ouuutput/releases/download/$Tag/ouuutput-$Version-win64.zip"
$NotesUrl = "https://github.com/nakamotosai/ouuutput/releases/tag/$Tag"

# 3. Upload to Cloudflare R2 (mandatory in formal releases)
if (-not $SkipR2) {
    Write-Host "[release] publishing archive to Cloudflare R2"
    $PyArgs = @(
        (Join-Path $PSScriptRoot "publish-r2.py"),
        "--version", $Version,
        "--file", $Zip,
        "--json"
    )
    if (-not $SkipFullSha) { $PyArgs += "--verify-full-sha" }
    $r2Raw = (& python @PyArgs | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) { throw "publish-r2.py failed ($LASTEXITCODE)`n$r2Raw" }
    $r2LastLine = ($r2Raw -split "`r?`n" | Select-Object -Last 1)
    $r2Info = $r2LastLine | ConvertFrom-Json
    if (-not $r2Info.ok) { throw "R2 publish did not return ok=true: $r2LastLine" }
    if ($r2Info.sha256 -ne $Sha256 -or [int64]$r2Info.bytes -ne [int64]$Bytes) {
        throw "R2 manifest mismatch: local=($Bytes,$Sha256) vs r2=($($r2Info.bytes),$($r2Info.sha256))"
    }
    $R2Url = $r2Info.r2_url
    Write-Host "[release] R2 verified: $R2Url"
}

# 4. Sync GitHub Release asset
if (-not $SkipGitHub) {
    $ghCmd = Get-Command gh -ErrorAction SilentlyContinue
    if (-not $ghCmd) { throw "gh CLI not found on PATH (pass -SkipGitHub to bypass)" }
    $eap = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $relJson = (& gh release view $Tag -R nakamotosai/ouuutput --json assets 2>$null | Out-String)
    $relCode = $LASTEXITCODE
    $ErrorActionPreference = $eap
    if ($relCode -ne 0) {
        Write-Host "[release] creating GitHub Release $Tag"
        & gh release create $Tag $Zip -R nakamotosai/ouuutput --title "ouuutput $Tag" --notes "Release $Tag (sha256: $Sha256)"
        if ($LASTEXITCODE -ne 0) { throw "gh release create failed ($LASTEXITCODE)" }
    } else {
        $rel = $relJson | ConvertFrom-Json
        $assetName = "ouuutput-$Version-win64.zip"
        $asset = $rel.assets | Where-Object { $_.name -eq $assetName } | Select-Object -First 1
        if (-not $asset -or $ClobberGitHub) {
            Write-Host "[release] uploading asset $assetName to GitHub Release $Tag"
            & gh release upload $Tag $Zip -R nakamotosai/ouuutput --clobber
            if ($LASTEXITCODE -ne 0) { throw "gh release upload failed ($LASTEXITCODE)" }
        } else {
            if ($asset.digest -and $asset.digest -ne "sha256:$Sha256") {
                throw "GitHub Release asset digest mismatch ($($asset.digest) != sha256:$Sha256). Pass -ClobberGitHub to replace."
            }
            Write-Host "[release] GitHub Release $Tag asset already matches sha256"
        }
    }
}

# 5. Sync & deploy official site (cnjpinput -> https://input.ouuut.com/)
if (-not $SkipSite) {
    if ([string]::IsNullOrWhiteSpace($SiteDir)) {
        $SiteDir = Join-Path (Split-Path -Parent $Root) "cnjpinput"
    }
    $SyncScript = Join-Path $SiteDir "scripts\sync-release.mjs"
    $DeployScript = Join-Path $SiteDir "scripts\deploy.ps1"
    if (-not (Test-Path $SyncScript)) {
        throw "Official site sync script not found: $SyncScript (pass -SkipSite to skip)"
    }
    Write-Host "[release] syncing official site at $SiteDir"
    & node $SyncScript --version $Version --r2-url $R2Url --gh-url $GhUrl --notes-url $NotesUrl --sha256 $Sha256 --bytes $Bytes
    if ($LASTEXITCODE -ne 0) { throw "sync-release.mjs failed ($LASTEXITCODE)" }

    Write-Host "[release] deploying official site to Cloudflare Pages (input.ouuut.com)"
    & powershell -NoProfile -ExecutionPolicy Bypass -File $DeployScript
    if ($LASTEXITCODE -ne 0) { throw "site deploy.ps1 failed ($LASTEXITCODE)" }
}

Write-Host ""
Write-Host "[release] DONE: $Tag"
Write-Host "  R2 CDN  : $R2Url"
Write-Host "  GitHub  : $GhUrl"
Write-Host "  SHA-256 : $Sha256"
Write-Host "  Site    : https://input.ouuut.com/"
