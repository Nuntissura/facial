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
$installerScript = Join-Path $installer "facial.iss"
$lockPath = Join-Path $productRoot "Cargo.lock"

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
$surrealDbSmokePassed = $false

function Get-PeSubsystem {
    param([Parameter(Mandatory = $true)][string]$Path)
    $bytes = [IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 256 -or $bytes[0] -ne 0x4D -or $bytes[1] -ne 0x5A) { return $null }
    $peOffset = [BitConverter]::ToInt32($bytes, 0x3C)
    $subsystemOffset = $peOffset + 4 + 20 + 68
    if ($peOffset -lt 0 -or $subsystemOffset + 1 -ge $bytes.Length) { return $null }
    return [BitConverter]::ToUInt16($bytes, $subsystemOffset)
}

function Get-InnoSection {
    param(
        [Parameter(Mandatory = $true)][string]$Raw,
        [Parameter(Mandatory = $true)][string]$Name
    )
    $match = [regex]::Match($Raw, "(?ms)^\[$([regex]::Escape($Name))\]\s*(.*?)(?=^\[|\z)")
    if ($match.Success) { return $match.Groups[1].Value }
    return ""
}

function Get-LockPackageVersion {
    param(
        [Parameter(Mandatory = $true)][string]$Raw,
        [Parameter(Mandatory = $true)][string]$Name
    )
    $pattern = '(?ms)^\[\[package\]\]\s*name\s*=\s*"{0}"\s*version\s*=\s*"([^"]+)"' -f [regex]::Escape($Name)
    $match = [regex]::Match($Raw, $pattern)
    if ($match.Success) { return $match.Groups[1].Value }
    return $null
}

$surrealDbVersion = if (Test-Path -LiteralPath $lockPath -PathType Leaf) {
    Get-LockPackageVersion -Raw (Get-Content -Raw -LiteralPath $lockPath) -Name "surrealdb"
} else {
    $null
}
if (-not $surrealDbVersion) {
    $violations.Add("Cargo.lock does not identify the embedded SurrealDB package version.")
}

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
    $portablePath = Join-Path $installer $expectedPortable
    if (Test-Path -LiteralPath $portablePath -PathType Leaf) {
        $portableSubsystem = Get-PeSubsystem -Path $portablePath
        if ($portableSubsystem -ne 2) {
            $violations.Add("current portable must use IMAGE_SUBSYSTEM_WINDOWS_GUI (2); observed '$portableSubsystem'.")
        }
    }

    $setupPath = Join-Path $installer $expectedSetup
    if (Test-Path -LiteralPath $setupPath -PathType Leaf) {
        # Independently extract the compiled setup payload. This exercises the
        # actual published installer without installing it or trusting the
        # packaging script's pre-ISCC staging checks.
        $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
        $verifyDir = Join-Path $tempRoot ("facial-installer-verify-" + [guid]::NewGuid().ToString("N"))
        $verifyFull = [IO.Path]::GetFullPath($verifyDir)
        if (-not $verifyFull.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase)) {
            $violations.Add("refused unsafe installer verification path: $verifyFull")
        } else {
            try {
                New-Item -ItemType Directory -Force -Path $verifyFull | Out-Null
                $setupArgs = '/CURRENTUSER /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /NOICONS "/FACIALVERIFY={0}"' -f $verifyFull
                $process = Start-Process -FilePath $setupPath -ArgumentList $setupArgs -Wait -PassThru -WindowStyle Hidden
                $payloadGui = Join-Path $verifyFull "facial.exe"
                $payloadCli = Join-Path $verifyFull "facial-cli.exe"
                if (-not (Test-Path -LiteralPath $payloadGui -PathType Leaf)) {
                    $violations.Add("compiled setup did not export its facial.exe payload (exit $($process.ExitCode)).")
                } elseif ((Get-PeSubsystem -Path $payloadGui) -ne 2) {
                    $violations.Add("compiled setup facial.exe payload is not IMAGE_SUBSYSTEM_WINDOWS_GUI (2).")
                }
                if (-not (Test-Path -LiteralPath $payloadCli -PathType Leaf)) {
                    $violations.Add("compiled setup did not export its facial-cli.exe payload (exit $($process.ExitCode)).")
                } elseif ((Get-PeSubsystem -Path $payloadCli) -ne 3) {
                    $violations.Add("compiled setup facial-cli.exe payload is not IMAGE_SUBSYSTEM_WINDOWS_CUI (3).")
                } else {
                    # Prove the compiled installer payload can initialize the
                    # embedded SurrealKV-backed ledger on a fresh project with
                    # no separately installed SurrealDB server or executable.
                    $smokeRoot = Join-Path $verifyFull "timeline-ledger-smoke"
                    New-Item -ItemType Directory -Force -Path $smokeRoot | Out-Null
                    [IO.File]::WriteAllText(
                        (Join-Path $smokeRoot "timeline-maintenance.yaml"),
                        "project: installer-smoke`n",
                        [Text.UTF8Encoding]::new($false)
                    )
                    $smokeOut = Join-Path $verifyFull "timeline-ledger-smoke.stdout.json"
                    $smokeErr = Join-Path $verifyFull "timeline-ledger-smoke.stderr.txt"
                    $smokeArgs = 'timeline-ledger init --project-root "{0}"' -f $smokeRoot
                    $smoke = Start-Process -FilePath $payloadCli -ArgumentList $smokeArgs -Wait -PassThru -WindowStyle Hidden -RedirectStandardOutput $smokeOut -RedirectStandardError $smokeErr
                    $databaseRoot = Join-Path $smokeRoot ".facial\timeline-ledger\surrealdb"
                    $smokeJson = if (Test-Path -LiteralPath $smokeOut -PathType Leaf) {
                        Get-Content -Raw -LiteralPath $smokeOut
                    } else { "" }
                    $smokeReceipt = $null
                    try {
                        if ($smokeJson) { $smokeReceipt = $smokeJson | ConvertFrom-Json -ErrorAction Stop }
                    } catch {
                        $violations.Add("compiled setup CLI returned malformed timeline-ledger JSON: $($_.Exception.Message)")
                    }
                    if ($smoke.ExitCode -ne 0 -or -not (Test-Path -LiteralPath $databaseRoot -PathType Container) -or $smokeReceipt.status -ne "initialized") {
                        $stderr = if (Test-Path -LiteralPath $smokeErr -PathType Leaf) { Get-Content -Raw -LiteralPath $smokeErr } else { "" }
                        $violations.Add("compiled setup CLI could not initialize its embedded SurrealDB ledger (exit $($smoke.ExitCode)): $stderr")
                    } elseif ($smokeReceipt.engine_version -ne $surrealDbVersion) {
                        $violations.Add("compiled setup CLI reports SurrealDB $($smokeReceipt.engine_version), but Cargo.lock records $surrealDbVersion.")
                    } else {
                        $surrealDbSmokePassed = $true
                    }
                }
            } catch {
                $violations.Add("compiled setup payload verification failed: $($_.Exception.Message)")
            } finally {
                if (Test-Path -LiteralPath $verifyFull -PathType Container) {
                    Remove-Item -LiteralPath $verifyFull -Recurse -Force
                }
            }
        }
    }
}

# Prove the compiled payload contract is backed by direct installer entries and
# GUI shortcuts, not a shell wrapper that can flash a console.
if (-not (Test-Path -LiteralPath $installerScript -PathType Leaf)) {
    $violations.Add("missing installer source: installer/facial.iss")
} else {
    $issRaw = Get-Content -Raw -LiteralPath $installerScript
    foreach ($sectionName in @("Files", "Icons", "Run", "InstallDelete")) {
        $escapedSectionName = [regex]::Escape($sectionName)
        $sectionCount = [regex]::Matches(
            $issRaw,
            "(?im)^\s*\[$escapedSectionName\]\s*$"
        ).Count
        if ($sectionCount -ne 1) {
            $violations.Add("installer must contain exactly one [$sectionName] section; found $sectionCount.")
        }
    }
    $filesSection = Get-InnoSection -Raw $issRaw -Name "Files"
    $iconsSection = Get-InnoSection -Raw $issRaw -Name "Icons"
    $runSection = Get-InnoSection -Raw $issRaw -Name "Run"
    $deleteSection = Get-InnoSection -Raw $issRaw -Name "InstallDelete"
    # ISPP can generate executable installer entries through #include aliases,
    # #emit, and other directives that are absent from the raw sections parsed
    # below. Require the complete, case-sensitive directive sequence used by
    # this project rather than attempting a fragile blacklist.
    $expectedPreprocessorLines = @(
        '#ifndef AppVersion',
        '#define AppVersion "0.0.0"',
        '#endif',
        '#ifndef PayloadDir',
        '#define PayloadDir "payload"',
        '#endif',
        '#ifndef OutputDir',
        '#define OutputDir "."',
        '#endif',
        '#define AppName "Facial"',
        '#define AppExe "facial.exe"'
    )
    $actualPreprocessorLines = @(
        [regex]::Matches($issRaw, '(?im)^\s*#.*$') |
            ForEach-Object { $_.Value.Trim() }
    )
    if (($actualPreprocessorLines -join "`n") -cne ($expectedPreprocessorLines -join "`n")) {
        $violations.Add("installer preprocessor directives differ from the exact project allowlist.")
    }
    $appExeDirectives = @([regex]::Matches(
        $issRaw,
        '(?im)^\s*#\s*(define|undef)\s+AppExe(?:\s+"([^"]*)")?.*$'
    ))
    if ($appExeDirectives.Count -ne 1 -or
        $appExeDirectives[0].Groups[1].Value -ine 'define' -or
        $appExeDirectives[0].Groups[2].Value -cne 'facial.exe') {
        $violations.Add("installer must contain exactly one AppExe directive defining facial.exe and no redefinition or undefinition.")
    }
    if ($filesSection -notmatch 'Source:\s*"\{#PayloadDir\}\\facial\.exe";\s*DestDir:\s*"\{app\}"') {
        $violations.Add("installer [Files] does not install facial.exe directly from the staged payload.")
    }
    if ($filesSection -notmatch 'Source:\s*"\{#PayloadDir\}\\facial-cli\.exe";\s*DestDir:\s*"\{app\}"') {
        $violations.Add("installer [Files] does not install facial-cli.exe directly from the staged payload.")
    }
    $iconFilenameFields = [regex]::Matches($iconsSection, '(?im)^\s*(?!;).*?\bFilename\s*:')
    $iconTargets = @([regex]::Matches($iconsSection, '(?im)^\s*(?!;).*?\bFilename\s*:\s*"([^"]+)"') | ForEach-Object { $_.Groups[1].Value })
    if ($iconTargets.Count -ne $iconFilenameFields.Count) {
        $violations.Add("installer [Icons] contains an unquoted or unparseable Filename target.")
    }
    foreach ($targetValue in $iconTargets) {
        if ($targetValue -notin @('{app}\{#AppExe}', '{uninstallexe}')) {
            $violations.Add("installer [Icons] target is outside the exact GUI allowlist: '$targetValue'.")
        }
    }
    $runFilenameFields = [regex]::Matches($runSection, '(?im)^\s*(?!;).*?\bFilename\s*:')
    $runTargets = @([regex]::Matches($runSection, '(?im)^\s*(?!;).*?\bFilename\s*:\s*"([^"]+)"') | ForEach-Object { $_.Groups[1].Value })
    if ($runTargets.Count -ne 1 -or $runFilenameFields.Count -ne 1 -or $runTargets[0] -ne '{app}\{#AppExe}') {
        $violations.Add("installer [Run] must contain exactly one quoted direct facial.exe target.")
    }
    $directIconCount = @($iconTargets | Where-Object { $_ -eq '{app}\{#AppExe}' }).Count
    if ($directIconCount -ne 2) {
        $violations.Add("installer must define exactly two direct Facial GUI shortcuts; found $directIconCount.")
    }
    $uninstallIconCount = @($iconTargets | Where-Object { $_ -eq '{uninstallexe}' }).Count
    if ($uninstallIconCount -ne 1) {
        $violations.Add("installer must define exactly one uninstall shortcut; found $uninstallIconCount.")
    }
    if ($deleteSection -notmatch 'Name:\s*"\{app\}\\launch-facial\.cmd"') {
        $violations.Add("installer does not remove the retired launch-facial.cmd during upgrade.")
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
    (Join-Path $installer "launch-facial.cmd"),
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
    Write-Host "surrealdb-embedded-version=$surrealDbVersion"
    Write-Host "surrealdb-installer-smoke=$surrealDbSmokePassed"
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
