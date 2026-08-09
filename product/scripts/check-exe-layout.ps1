<#
  check-exe-layout.ps1  (WP-023)

  Enforces the canonical-executable invariant so the repo cannot silently deviate:
    * exactly one canonical executable: product/facial.exe
    * every other build archived under product/archive/exe/ as facial-<yyyymmdd-hhmmss>.exe
    * no build/test scratch left behind (product/target absent at steady state)
    * retired surfaces absent (product/release, product/dist)
    * nothing built outside the repo (../facial-build, or a cargo target-dir that escapes the repo)

  Exit code 0 = invariant holds; 1 = one or more violations (each listed).
  Dependency-free; safe for a no-context model to run:
    powershell -ExecutionPolicy Bypass -File product/scripts/check-exe-layout.ps1
#>
param([switch]$Quiet)

$ErrorActionPreference = "Stop"
$scriptDir   = Split-Path -Parent $MyInvocation.MyCommand.Path
$productRoot = Resolve-Path (Join-Path $scriptDir "..")
$repoRoot    = Resolve-Path (Join-Path $productRoot "..")
$repoFull    = [IO.Path]::GetFullPath($repoRoot)

$canonical     = Join-Path $productRoot "facial.exe"
$archiveDir    = Join-Path $productRoot "archive\exe"
$canonicalFull = [IO.Path]::GetFullPath($canonical)
$archiveFull   = [IO.Path]::GetFullPath($archiveDir)
$archivePattern = '^facial-\d{8}-\d{6}\.exe$'

$violations = New-Object System.Collections.Generic.List[string]

# 1) Every product *.exe must be the canonical exe or an archived build.
#    (_source_checks holds vendored upstream repos for inspection and is out of scope.)
$exes = Get-ChildItem -LiteralPath $repoRoot -Recurse -Filter *.exe -File -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -notmatch '\\_source_checks\\' -and $_.FullName -notmatch '\\installer\\' }
foreach ($e in $exes) {
    $full = [IO.Path]::GetFullPath($e.FullName)
    if ($full -eq $canonicalFull) { continue }
    $parent = [IO.Path]::GetFullPath($e.Directory.FullName)
    if ($parent -eq $archiveFull) {
        if ($e.Name -notmatch $archivePattern) {
            $violations.Add("archive exe has non-standard name (expected facial-<yyyymmdd-hhmmss>.exe): $($full.Substring($repoFull.Length+1))")
        }
        continue
    }
    $violations.Add("stray executable (must be product/facial.exe or product/archive/exe/facial-<stamp>.exe): $($full.Substring($repoFull.Length+1))")
}

# 2) Build/test scratch must not persist.
$target = Join-Path $productRoot "target"
if (Test-Path -LiteralPath $target) {
    $violations.Add("build scratch present: product/target exists. Run 'cargo clean' (package-release.ps1 auto-cleans).")
}

# 3) Retired surfaces must be absent.
foreach ($r in @("release", "dist")) {
    if (Test-Path -LiteralPath (Join-Path $productRoot $r)) {
        $violations.Add("retired surface present: product/$r must not exist.")
    }
}

# 4) Nothing built outside the repo.
$sibling = Join-Path (Split-Path $repoFull -Parent) "facial-build"
if (Test-Path -LiteralPath $sibling) {
    $violations.Add("out-of-repo build dir present: $sibling must not exist.")
}
$cargoCfg = Join-Path $repoRoot ".cargo\config.toml"
if (Test-Path -LiteralPath $cargoCfg) {
    $cfg = Get-Content -Raw -LiteralPath $cargoCfg
    if ($cfg -match 'target-dir\s*=\s*"([^"]*)"') {
        $td = $Matches[1]
        if ($td -match '\.\.' -or [IO.Path]::IsPathRooted($td)) {
            $violations.Add(".cargo/config.toml sets a build target-dir that may escape the repo: '$td'. Build output must stay in-repo.")
        }
    }
}

# Report
$canonExists  = Test-Path -LiteralPath $canonical
$archiveCount = @(Get-ChildItem -LiteralPath $archiveDir -Filter *.exe -File -ErrorAction SilentlyContinue).Count
if (-not $Quiet) {
    Write-Host "canonical exe (product/facial.exe) present: $canonExists"
    Write-Host "archived builds (product/archive/exe):       $archiveCount"
}
if ($violations.Count -eq 0) {
    if (-not $Quiet) { Write-Host "OK: canonical-exe invariant holds (WP-023)." }
    exit 0
} else {
    Write-Host "FAIL: canonical-exe invariant violated ($($violations.Count)):"
    foreach ($v in $violations) { Write-Host "  - $v" }
    exit 1
}
