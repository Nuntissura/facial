param(
    [string]$PackageName = "facial"
)

$ErrorActionPreference = "Stop"

# --- Layout (WP-023: one canonical exe, no release/ folder, nothing outside the repo) ---
# Canonical operator-facing executable:  product/<name>.exe                (exactly one)
# Superseded builds:                      product/archive/exe/<name>-<stamp>.exe
# Cargo build scratch:                    product/target/  (Cargo default, in-repo,
#   git-ignored, disposable). Not a canonical or handoff surface.
$scriptDir    = Split-Path -Parent $MyInvocation.MyCommand.Path
$productRoot  = Resolve-Path (Join-Path $scriptDir "..")
$archiveDir   = Join-Path $productRoot "archive\exe"
$cargoExe     = Join-Path $productRoot "target\release\$PackageName.exe"
$canonicalExe = Join-Path $productRoot "$PackageName.exe"
$stamp        = Get-Date -Format "yyyyMMdd-HHmmss"

New-Item -ItemType Directory -Force -Path $archiveDir | Out-Null

# Archive the current canonical exe before it is replaced, unless a build with the same
# sha256 is already archived (dedupe). A duplicate is left in place and overwritten by the
# fresh build below, so identical repeat packaging never piles up archive copies.
if (Test-Path -LiteralPath $canonicalExe) {
    $currentHash = (Get-FileHash -LiteralPath $canonicalExe -Algorithm SHA256).Hash
    $duplicate = Get-ChildItem -LiteralPath $archiveDir -Filter "*.exe" -File -ErrorAction SilentlyContinue |
        Where-Object { (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash -eq $currentHash } |
        Select-Object -First 1
    if ($duplicate) {
        Write-Host "archive-skip=duplicate-of=$($duplicate.Name)"
    } else {
        $archiveTarget = Join-Path $archiveDir "$PackageName-$stamp.exe"
        Move-Item -LiteralPath $canonicalExe -Destination $archiveTarget
        Write-Host "archived=$archiveTarget"
    }
}

cargo build --manifest-path (Join-Path $productRoot "Cargo.toml") --release

Copy-Item -LiteralPath $cargoExe -Destination $canonicalExe -Force
$item = Get-Item -LiteralPath $canonicalExe

Write-Host "canonical=$($item.FullName)"
Write-Host "bytes=$($item.Length)"
Write-Host "updated=$($item.LastWriteTime.ToString('s'))"

# Build scratch is invalid once the canonical exe is published and validated; remove it so the
# repo is left with exactly one facial.exe (canonical) + archive, no scratch (operator rule).
# The next build recompiles from scratch.
$targetDir = Join-Path $productRoot "target"
try {
    if (Test-Path -LiteralPath $targetDir) { Remove-Item -LiteralPath $targetDir -Recurse -Force -ErrorAction Stop }
    Write-Host "cleaned-scratch=$targetDir"
} catch {
    Write-Warning "could not remove build scratch ${targetDir}: $($_.Exception.Message)"
}

# --- Installer (WP-025): stage payload + compile facial-setup.exe via Inno Setup --------
$repoRoot     = Split-Path -Parent $productRoot
$installerDir = Join-Path $repoRoot "installer"
$payloadDir   = Join-Path $installerDir "payload"
$outDir       = Join-Path $installerDir "out"
$cargoToml    = Get-Content -Raw -LiteralPath (Join-Path $productRoot "Cargo.toml")
$version      = if ($cargoToml -match '(?m)^\s*version\s*=\s*"([^"]+)"') { $Matches[1] } else { "0.0.0" }

$iscc = $null
foreach ($cand in @(
    (Get-Command ISCC -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty Source),
    (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe"),
    "C:\Program Files (x86)\Inno Setup 6\ISCC.exe",
    "C:\Program Files\Inno Setup 6\ISCC.exe"
)) { if ($cand -and (Test-Path -LiteralPath $cand)) { $iscc = $cand; break } }
if (-not $iscc) {
    throw "Inno Setup (ISCC.exe) not found. Install it once: winget install --id JRSoftware.InnoSetup -e"
}

# Stage a fresh payload: the canonical exe + launcher + the read-only asset subset (no models, no target).
if (Test-Path -LiteralPath $payloadDir) { Remove-Item -LiteralPath $payloadDir -Recurse -Force }
New-Item -ItemType Directory -Force -Path (Join-Path $payloadDir "product"), $outDir | Out-Null
Copy-Item -LiteralPath $canonicalExe -Destination (Join-Path $payloadDir "facial.exe") -Force
Copy-Item -LiteralPath (Join-Path $installerDir "launch-facial.cmd") -Destination $payloadDir -Force
foreach ($sub in @("config", "plugins", "assets", "docs")) {
    $src = Join-Path $productRoot $sub
    if (Test-Path -LiteralPath $src) {
        Copy-Item -LiteralPath $src -Destination (Join-Path $payloadDir "product\$sub") -Recurse -Force
    }
}

# Compile. SourceDir defaults to the .iss dir, so PayloadDir/OutputDir stay relative (no space issues).
& $iscc "/DAppVersion=$version" (Join-Path $installerDir "facial.iss")
if ($LASTEXITCODE -ne 0) { throw "ISCC failed to compile the installer (exit $LASTEXITCODE)." }

# Drop the transient staging; keep only the setup.exe.
Remove-Item -LiteralPath $payloadDir -Recurse -Force
Write-Host "installer=$(Join-Path $outDir "facial-setup-$version.exe")"

# Self-enforce the canonical-exe invariant (WP-023). Packaging fails if the repo deviated.
& (Join-Path $scriptDir "check-exe-layout.ps1")
if ($LASTEXITCODE -ne 0) {
    throw "canonical-exe invariant check failed after packaging (see check-exe-layout.ps1 output above)."
}
