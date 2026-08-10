<#
  check-exe-layout.ps1  (WP-059 supersedes WP-023 delivery layout)

  Steady-state delivery invariant:
    * installer/ contains exactly two root-level EXEs:
        facial-portable-<CargoVersion>.exe
        facial-setup-<CargoVersion>.exe
    * superseded installers and portable builds live only under
      installer/installer-portable-archive/
    * legacy product/facial.exe, product/archive/exe, and installer/out are absent
    * product/target is transient and absent after validation
    * nothing is built outside the repository

  Exit 0 = invariant holds; exit 1 = every deviation is listed.
#>
param([switch]$Quiet)

$ErrorActionPreference = "Stop"
$scriptDir   = Split-Path -Parent $MyInvocation.MyCommand.Path
$productRoot = Resolve-Path (Join-Path $scriptDir "..")
$repoRoot    = Resolve-Path (Join-Path $productRoot "..")
$repoFull    = [IO.Path]::GetFullPath($repoRoot)
$installer   = Join-Path $repoRoot "installer"
$archiveDir  = Join-Path $installer "installer-portable-archive"
$manifest    = Join-Path $productRoot "Cargo.toml"

$manifestRaw = Get-Content -Raw -LiteralPath $manifest
$versionMatch = [regex]::Match(
    $manifestRaw,
    '(?ms)^\[package\]\s*.*?^version\s*=\s*"(\d+\.\d+\.\d+)"'
)
$version = if ($versionMatch.Success) { $versionMatch.Groups[1].Value } else { $null }
$expectedPortable = if ($version) { "facial-portable-$version.exe" } else { $null }
$expectedSetup = if ($version) { "facial-setup-$version.exe" } else { $null }
$archiveFull = [IO.Path]::GetFullPath($archiveDir)
$violations = New-Object System.Collections.Generic.List[string]

if (-not $version) {
    $violations.Add("product/Cargo.toml has no numeric [package] version (major.minor.patch).")
}

# The installer root must expose exactly the current portable/setup pair.
$rootExes = @(Get-ChildItem -LiteralPath $installer -Filter "*.exe" -File -ErrorAction SilentlyContinue)
if ($rootExes.Count -ne 2) {
    $violations.Add("installer/ must contain exactly two root EXEs (one portable + one setup); found $($rootExes.Count).")
}
if ($version) {
    foreach ($required in @($expectedPortable, $expectedSetup)) {
        if (-not (Test-Path -LiteralPath (Join-Path $installer $required) -PathType Leaf)) {
            $violations.Add("missing current delivery artifact: installer/$required")
        }
    }
    foreach ($rootExe in $rootExes) {
        if ($rootExe.Name -notin @($expectedPortable, $expectedSetup)) {
            $violations.Add("unexpected root installer executable: installer/$($rootExe.Name)")
        }
    }
}

# Every repository EXE outside transient build scratch must be either one of the
# two current root artifacts or a file in the one delivery archive.
$allowedRootPaths = @{}
foreach ($rootExe in $rootExes) {
    $allowedRootPaths[[IO.Path]::GetFullPath($rootExe.FullName)] = $true
}
$allExes = Get-ChildItem -LiteralPath $repoRoot -Recurse -Filter "*.exe" -File -ErrorAction SilentlyContinue |
    Where-Object {
        $_.FullName -notmatch '\\_source_checks\\' -and
        $_.FullName -notmatch '\\product\\target\\'
    }
foreach ($exe in $allExes) {
    $full = [IO.Path]::GetFullPath($exe.FullName)
    if ($allowedRootPaths.ContainsKey($full)) { continue }
    $parent = [IO.Path]::GetFullPath($exe.Directory.FullName)
    if ($parent -eq $archiveFull) { continue }
    $relative = $full.Substring($repoFull.Length + 1)
    $violations.Add("stray executable outside installer root/archive: $relative")
}

# Build scratch and retired delivery surfaces cannot persist at steady state.
$target = Join-Path $productRoot "target"
if (Test-Path -LiteralPath $target) {
    $violations.Add("build scratch present: product/target exists; package-release.ps1 must clean it.")
}
foreach ($retired in @(
    (Join-Path $productRoot "facial.exe"),
    (Join-Path $productRoot "facial.exe.sha256"),
    (Join-Path $productRoot "release-artifacts.sha256"),
    (Join-Path $productRoot "archive\exe"),
    (Join-Path $productRoot "release"),
    (Join-Path $productRoot "dist"),
    (Join-Path $installer "out"),
    (Join-Path $installer "payload")
)) {
    if (Test-Path -LiteralPath $retired) {
        $relative = [IO.Path]::GetFullPath($retired).Substring($repoFull.Length + 1)
        $violations.Add("retired/transient artifact surface present: $relative")
    }
}

# No Cargo target relocation or legacy sibling build directory may escape the repo.
$sibling = Join-Path (Split-Path $repoFull -Parent) "facial-build"
if (Test-Path -LiteralPath $sibling) {
    $violations.Add("out-of-repo build directory present: $sibling")
}
$cargoCfg = Join-Path $repoRoot ".cargo\config.toml"
if (Test-Path -LiteralPath $cargoCfg) {
    $cfg = Get-Content -Raw -LiteralPath $cargoCfg
    if ($cfg -match 'target-dir\s*=\s*"([^"]*)"') {
        $targetDir = $Matches[1]
        if ($targetDir -match '\.\.' -or [IO.Path]::IsPathRooted($targetDir)) {
            $violations.Add(".cargo/config.toml target-dir may escape the repo: '$targetDir'.")
        }
    }
}

$archiveCount = @(Get-ChildItem -LiteralPath $archiveDir -Filter "*.exe" -File -ErrorAction SilentlyContinue).Count
if (-not $Quiet) {
    Write-Host "cargo-version=$version"
    Write-Host "installer-root-exes=$($rootExes.Count)"
    Write-Host "archived-delivery-exes=$archiveCount"
}
if ($violations.Count -eq 0) {
    if (-not $Quiet) { Write-Host "OK: installer delivery invariant holds (WP-059)." }
    exit 0
}

Write-Host "FAIL: installer delivery invariant violated ($($violations.Count)):"
foreach ($violation in $violations) { Write-Host "  - $violation" }
exit 1
