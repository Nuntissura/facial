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
$packageReleaseScript = Join-Path $scriptDir "package-release.ps1"
$retirementToolSource = Join-Path $scriptDir "retire-legacy-media-db.ps1"
$frozenRetirementToolSha256 = "D5593C45712EE253D48805E4237F90B2BB230ADB4A21BA84321CA9AC68D1878A"
$releaseDefaultConfigSource = Join-Path $productRoot "config\release-default.json"
$releaseDefaultConfigContract = "facial-sanitized-release-seed-v1"
$lockPath = Join-Path $productRoot "Cargo.lock"
$topologyPath = Join-Path $repoRoot "topology.yaml"

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
$surrealMediaSmokePassed = $false
$updateModeContractPassed = $false
$installerVerifierSourceContractPassed = $false
$installerSourceBindingPassed = $false
$installerVerifierReceiptPassed = $false
$installerVerifierNoSideEffectsPassed = $false
$installerVerifierOutputBoundaryPassed = $false
$installerVerifierCallGraphCounterfactualsPassed = $false
$installerPreprocessorCounterfactualPassed = $false
$installerNormalizedSourceCounterfactualPassed = $false
$installerPayloadPortableMatchPassed = $false
$sha256PairCounterfactualPassed = $false
$forcedExeEnumerationPassed = $false
$releaseVersionAgreementPassed = $false
$defaultDataReparseSourceContractPassed = $false
$defaultDataReparseCounterfactualPassed = $false
$releaseDefaultConfigPassed = $false
$releaseDefaultConfigSourcePassed = $false
$releaseDefaultConfigSha256 = $null
$retirementToolSourceSha256 = $null
$retirementToolPayloadSha256 = $null
$retirementToolExportedSha256 = $null
$retirementToolHashContractPassed = $false
$retirementToolHashCounterfactualPassed = $false
$retirementToolSourcePinnedPassed = $false
$packageReleaseSourceContractPassed = $false
$packageReleaseForceCounterfactualPassed = $false
$registryFingerprintReadContractProbePassed = $false
$predecessorSafetyContractPassed = $false
$predecessorSourceContractPassed = $false
$predecessorReceiptContractPassed = $false
$installerSourceSha256 = $null
$expectedVerifierExitCode = $null
$installerAppGuid = $null
$expectedVerifierSourceTemplateSha256 = '918e99a0a04f3d34aea9ce18356d1075e3f62ddb3ac39d84bf2c67322d0e3912'
$expectedInstallerNormalizedSourceSha256 = 'fcc522eeb144e6676748d2babe41e4ef0e364af026f4d072a09e7f0e6ae03df5'
# A real 0.1.7 -> current uninstall transition cannot be run safely here: the
# published 0.1.7 binary hardcodes Facial's live AppId and user-data root, while
# rewriting/recompiling it with a synthetic AppId would no longer test that
# published predecessor. The checker therefore proves the compiled verifier and
# exact current-source handoff contract without invoking the predecessor.
$disposablePredecessorTransition = "not-run:published-0.1.7-hardcodes-live-appid-and-data-root"

function Get-PeSubsystem {
    param([Parameter(Mandatory = $true)][string]$Path)
    $bytes = [IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 256 -or $bytes[0] -ne 0x4D -or $bytes[1] -ne 0x5A) { return $null }
    $peOffset = [BitConverter]::ToInt32($bytes, 0x3C)
    $subsystemOffset = $peOffset + 4 + 20 + 68
    if ($peOffset -lt 0 -or $subsystemOffset + 1 -ge $bytes.Length) { return $null }
    return [BitConverter]::ToUInt16($bytes, $subsystemOffset)
}

function Get-LockPackageVersion {
    param(
        [Parameter(Mandatory = $true)][string]$Raw,
        [Parameter(Mandatory = $true)][string]$Name
    )
    $pattern = '(?ms)^\[\[package\]\]\s*name\s*=\s*"{0}"\s*version\s*=\s*"([^"]+)"' -f [regex]::Escape($Name)
    $matches = @([regex]::Matches($Raw, $pattern))
    if ($matches.Count -eq 1) { return $matches[0].Groups[1].Value }
    return $null
}

# Every EXE inventory goes through one forced enumerator. Hidden/system delivery
# files must be visible to the exact-two and stray-artifact gates.
function Get-ExeFilesForce {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [switch]$Recurse
    )
    $parameters = @{
        LiteralPath = $Path
        Filter = "*.exe"
        File = $true
        Force = $true
        ErrorAction = "SilentlyContinue"
    }
    if ($Recurse) { $parameters.Recurse = $true }
    return @(Get-ChildItem @parameters)
}

function Test-Sha256PairMatch {
    param(
        [AllowNull()][string]$Left,
        [AllowNull()][string]$Right
    )
    return ($Left -cmatch '^[0-9a-f]{64}$') -and
        ($Right -cmatch '^[0-9a-f]{64}$') -and
        ($Left -ceq $Right)
}

function Test-FrozenRetirementToolHashSet {
    param([Parameter(Mandatory = $true)][string[]]$Hashes)

    if ($Hashes.Count -eq 0) { return $false }
    foreach ($hash in $Hashes) {
        if ($hash -cnotmatch '^[0-9A-F]{64}$' -or
            $hash -cne $frozenRetirementToolSha256) {
            return $false
        }
    }
    return $true
}

function Test-PackageReleaseSafetySourceContract {
    param([Parameter(Mandatory = $true)][string]$Raw)

    $sourceHashIndex = $Raw.IndexOf('$retirementToolSourceSha256 =')
    $manifestMutationBoundary = $Raw.IndexOf('$manifestOriginal =')
    $stagedHashIndex = $Raw.IndexOf('$stagedRetirementToolSha256 =')
    $isccIndex = $Raw.IndexOf('& $iscc')
    $archiveIndex = $Raw.IndexOf('foreach ($oldRootExe in @(')
    $legacyArchiveIndex = $Raw.IndexOf('foreach ($legacy in @(Get-ChildItem -LiteralPath $legacyArchive')
    $legacyOutIndex = $Raw.IndexOf('foreach ($legacy in @(Get-ChildItem -LiteralPath $legacyOutDir')
    $frozenLiteralMatches = @([regex]::Matches(
        $Raw,
        '(?m)^\$frozenRetirementToolSha256 = "D5593C45712EE253D48805E4237F90B2BB230ADB4A21BA84321CA9AC68D1878A"\s*$'
    ))
    $forcedRootArchiveMatches = @([regex]::Matches(
        $Raw,
        '(?m)^\s*foreach \(\$oldRootExe in @\(Get-ChildItem -LiteralPath \$installerDir -Filter "\*\.exe" -File -Force -ErrorAction SilentlyContinue\)\) \{\s*$'
    ))
    $forcedLegacyArchiveMatches = @([regex]::Matches(
        $Raw,
        '(?m)^\s*foreach \(\$legacy in @\(Get-ChildItem -LiteralPath \$legacyArchive -File -Force\)\) \{\s*$'
    ))
    $forcedLegacyOutMatches = @([regex]::Matches(
        $Raw,
        '(?m)^\s*foreach \(\$legacy in @\(Get-ChildItem -LiteralPath \$legacyOutDir -File -Force\)\) \{\s*$'
    ))

    return (
        ($frozenLiteralMatches.Count -eq 1) -and
        ($Raw -match '(?m)^\$retirementToolSource = Join-Path \$scriptDir "retire-legacy-media-db\.ps1"\s*$') -and
        ($Raw -match '(?s)function Test-FrozenRetirementToolHashSet \{.*?\$hash -cnotmatch ''\^\[0-9A-F\]\{64\}\$''.*?\$hash -cne \$frozenRetirementToolSha256.*?return \$true\s*\}') -and
        ($Raw -match '(?s)\$retirementToolCounterfactualHash = "A" \* 64.*?Test-FrozenRetirementToolHashSet -Hashes @\(\s*\$retirementToolCounterfactualHash,\s*\$retirementToolCounterfactualHash\s*\).*?if \(-not \$retirementToolMutationCounterfactualRejected\) \{\s*throw') -and
        ($Raw -match '(?s)\$retirementToolSourceSha256 = \(Get-FileHash -LiteralPath \$retirementToolSource -Algorithm SHA256\)\.Hash\.ToUpperInvariant\(\).*?Test-FrozenRetirementToolHashSet -Hashes @\(\$retirementToolSourceSha256\).*?throw') -and
        ($Raw -match '(?s)\$stagedRetirementTool = Join-Path \$payloadScriptRoot "retire-legacy-media-db\.ps1".*?\$stagedRetirementToolSha256 = \(Get-FileHash -LiteralPath \$stagedRetirementTool -Algorithm SHA256\)\.Hash\.ToUpperInvariant\(\).*?Test-FrozenRetirementToolHashSet -Hashes @\(\s*\$retirementToolSourceSha256,\s*\$stagedRetirementToolSha256\s*\).*?throw') -and
        ($sourceHashIndex -ge 0) -and
        ($manifestMutationBoundary -gt $sourceHashIndex) -and
        ($stagedHashIndex -gt $manifestMutationBoundary) -and
        ($isccIndex -gt $stagedHashIndex) -and
        ($archiveIndex -gt $isccIndex) -and
        ($legacyArchiveIndex -gt $archiveIndex) -and
        ($legacyOutIndex -gt $legacyArchiveIndex) -and
        ($forcedRootArchiveMatches.Count -eq 1) -and
        ($forcedLegacyArchiveMatches.Count -eq 1) -and
        ($forcedLegacyOutMatches.Count -eq 1)
    )
}

function Test-ForcedExeEnumerationProbe {
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
    $probeRoot = [IO.Path]::GetFullPath(
        (Join-Path $tempRoot ("facial-exe-enumeration-probe-" + [guid]::NewGuid().ToString('N')))
    )
    if ([IO.Path]::GetFullPath((Split-Path -Parent $probeRoot)) -cne $tempRoot) {
        throw "refused unsafe forced-EXE enumeration probe path: $probeRoot"
    }

    try {
        New-Item -ItemType Directory -Path $probeRoot -ErrorAction Stop | Out-Null
        $hiddenExe = Join-Path $probeRoot 'hidden-root.exe'
        [IO.File]::WriteAllBytes($hiddenExe, [byte[]](0x4d, 0x5a))
        [IO.File]::SetAttributes($hiddenExe, [IO.FileAttributes]::Hidden)

        $hiddenDir = Join-Path $probeRoot 'hidden-dir'
        New-Item -ItemType Directory -Path $hiddenDir -ErrorAction Stop | Out-Null
        [IO.File]::SetAttributes($hiddenDir, [IO.FileAttributes]::Hidden)
        $nestedExe = Join-Path $hiddenDir 'hidden-nested.exe'
        [IO.File]::WriteAllBytes($nestedExe, [byte[]](0x4d, 0x5a))
        [IO.File]::SetAttributes(
            $nestedExe,
            ([IO.FileAttributes]::Hidden -bor [IO.FileAttributes]::System)
        )

        $shallowNames = @(Get-ExeFilesForce -Path $probeRoot).Name | Sort-Object
        $recursiveNames = @(Get-ExeFilesForce -Path $probeRoot -Recurse).Name | Sort-Object
        return (
            (($shallowNames -join "`n") -ceq 'hidden-root.exe') -and
            (($recursiveNames -join "`n") -ceq (@('hidden-nested.exe', 'hidden-root.exe') -join "`n"))
        )
    } finally {
        if ((Test-Path -LiteralPath $probeRoot) -and
            ([IO.Path]::GetFullPath((Split-Path -Parent $probeRoot)) -ceq $tempRoot)) {
            Remove-Item -LiteralPath $probeRoot -Recurse -Force
        }
    }
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

function Get-Sha256Lower {
    param([Parameter(Mandatory = $true)][string]$Path)
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-TextSha256Lower {
    param([Parameter(Mandatory = $true)][AllowEmptyString()][string]$Text)
    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [Text.UTF8Encoding]::new($false).GetBytes($Text)
        return ([BitConverter]::ToString($sha.ComputeHash($bytes))).Replace('-', '').ToLowerInvariant()
    } finally {
        $sha.Dispose()
    }
}

function Get-NormalizedSourceSha256Lower {
    param([Parameter(Mandatory = $true)][string]$Text)

    $lines = @(
        ($Text -replace "`r`n?", "`n") -split "`n" |
            ForEach-Object { $_.TrimEnd() }
    )
    return Get-TextSha256Lower -Text ([string]::Join("`n", $lines)).Trim()
}

function Get-InnoRoutineMap {
    param([Parameter(Mandatory = $true)][string]$Code)

    $declarations = @([regex]::Matches(
        $Code,
        '(?im)^(?:function|procedure)\s+([A-Za-z_][A-Za-z0-9_]*)\s*(?:\(|:|;)'
    ))
    $map = @{}
    for ($index = 0; $index -lt $declarations.Count; $index++) {
        $start = $declarations[$index].Index
        $end = if ($index + 1 -lt $declarations.Count) {
            $declarations[$index + 1].Index
        } else {
            $Code.Length
        }
        $map[$declarations[$index].Groups[1].Value] = $Code.Substring($start, $end - $start)
    }
    return $map
}

function Get-InnoVerifierAnalysis {
    param([Parameter(Mandatory = $true)][string]$Code)

    $routineMap = Get-InnoRoutineMap -Code $Code
    if (-not $routineMap.ContainsKey('InitializeSetup')) {
        return [pscustomobject]@{ passed = $false; reachable = @(); calls = @(); source = '' }
    }

    $queue = New-Object 'System.Collections.Generic.Queue[string]'
    $queue.Enqueue('InitializeSetup')
    $visited = @{}
    while ($queue.Count -gt 0) {
        $name = $queue.Dequeue()
        if ($visited.ContainsKey($name)) { continue }
        if (-not $routineMap.ContainsKey($name)) {
            return [pscustomobject]@{ passed = $false; reachable = @(); calls = @(); source = '' }
        }
        $visited[$name] = $true
        foreach ($call in [regex]::Matches(
            $routineMap[$name],
            '\b([A-Za-z_][A-Za-z0-9_]*)\s*(?:\(|;)'
        )) {
            $calledName = $call.Groups[1].Value
            if ($routineMap.ContainsKey($calledName) -and -not $visited.ContainsKey($calledName)) {
                $queue.Enqueue($calledName)
            }
        }
    }

    $reachable = @($visited.Keys | Sort-Object)
    $reachableSource = [string]::Join("`n", @($reachable | ForEach-Object { $routineMap[$_] }))
    $callSource = [regex]::Replace($reachableSource, '(?s)\{.*?\}', ' ')
    $callSource = [regex]::Replace($callSource, "'(?:''|[^'])*'", "''")
    $keywords = @('and', 'if', 'not', 'or', 'while')
    $calls = @(
        [regex]::Matches($callSource, '\b([A-Za-z_][A-Za-z0-9_]*)\s*\(') |
            ForEach-Object { $_.Groups[1].Value } |
            Where-Object { $_ -notin $keywords } |
            Sort-Object -Unique
    )

    $expectedReachable = @(
        'AssertVerifierTargetAbsent',
        'InitializeSetup',
        'IsHexString',
        'IsRetirementArchiveChild',
        'IsSafeVerifierDestination',
        'ModeCleansProgramTree',
        'ModeDeletesRelocatedState',
        'ModeDeletesUserData'
    ) | Sort-Object
    $expectedCalls = @(
        'AddBackslash',
        'AssertVerifierTargetAbsent',
        'CompareText',
        'Copy',
        'CopyFile',
        'DirExists',
        'ExpandConstant',
        'ExpandFileName',
        'ExtractFileDir',
        'ExtractFileName',
        'ExtractTemporaryFile',
        'FileExists',
        'ForceDirectories',
        'IntToStr',
        'IsHexString',
        'IsRetirementArchiveChild',
        'IsSafeVerifierDestination',
        'Length',
        'ModeCleansProgramTree',
        'ModeDeletesRelocatedState',
        'ModeDeletesUserData',
        'Pos',
        'RaiseException',
        'RemoveBackslashUnlessRoot',
        'SaveStringToFile'
    ) | Sort-Object

    $initializeSetupBlock = $routineMap['InitializeSetup']
    $verifyExtractNames = @(
        [regex]::Matches($initializeSetupBlock, "(?i)ExtractTemporaryFile\s*\(\s*'([^']+)'\s*\)") |
            ForEach-Object { $_.Groups[1].Value }
    )
    $expectedVerifyExtractNames = @(
        '_verify-facial.exe',
        '_verify-facial-cli.exe',
        '_verify-retire-legacy-media-db.ps1',
        '_verify-release-default-config.json'
    )
    $verifyTargetNames = @(
        [regex]::Matches($initializeSetupBlock, "(?i)AssertVerifierTargetAbsent\s*\(\s*VerifyDir\s*,\s*'([^']+)'\s*\)") |
            ForEach-Object { $_.Groups[1].Value }
    )
    $expectedVerifyTargetNames = @(
        'facial.exe',
        'facial-cli.exe',
        'retire-legacy-media-db.ps1',
        'release-default-config.json',
        'update-mode-contract.txt',
        'facialverify-receipt.json'
    )

    $copyBindings = @(
        "CopyFile\s*\(\s*GuiSource\s*,\s*VerifyDir\s*\+\s*'\\facial\.exe'\s*,\s*True\s*\)",
        "CopyFile\s*\(\s*CliSource\s*,\s*VerifyDir\s*\+\s*'\\facial-cli\.exe'\s*,\s*True\s*\)",
        "CopyFile\s*\(\s*ExpandConstant\s*\(\s*'\{tmp\}\\_verify-retire-legacy-media-db\.ps1'\s*\)\s*,\s*VerifyDir\s*\+\s*'\\retire-legacy-media-db\.ps1'\s*,\s*True\s*\)",
        "CopyFile\s*\(\s*DefaultConfigSource\s*,\s*VerifyDir\s*\+\s*'\\release-default-config\.json'\s*,\s*True\s*\)",
        "CopyFile\s*\(\s*ContractSource\s*,\s*VerifyDir\s*\+\s*'\\update-mode-contract\.txt'\s*,\s*True\s*\)",
        "CopyFile\s*\(\s*ReceiptSource\s*,\s*VerifyDir\s*\+\s*'\\facialverify-receipt\.json'\s*,\s*True\s*\)"
    )
    $copyBindingsMatch = @($copyBindings | Where-Object {
        $initializeSetupBlock -match "(?s)$_"
    }).Count -eq $copyBindings.Count

    $assignmentBindingsMatch =
        (@([regex]::Matches($initializeSetupBlock, '(?im)^\s*VerifyDir\s*:=' )).Count -eq 2) -and
        ($initializeSetupBlock -match "(?s)VerifyDir\s*:=\s*ExpandConstant\s*\(\s*'\{param:FACIALVERIFY\|\}'\s*\).*?if\s+not\s+IsSafeVerifierDestination\(VerifyDir\).*?VerifyDir\s*:=\s*RemoveBackslashUnlessRoot\(ExpandFileName\(VerifyDir\)\)") -and
        (@([regex]::Matches($initializeSetupBlock, '(?im)^\s*GuiSource\s*:=' )).Count -eq 1) -and
        ($initializeSetupBlock -match "GuiSource\s*:=\s*ExpandConstant\s*\(\s*'\{tmp\}\\_verify-facial\.exe'\s*\)") -and
        (@([regex]::Matches($initializeSetupBlock, '(?im)^\s*CliSource\s*:=' )).Count -eq 1) -and
        ($initializeSetupBlock -match "CliSource\s*:=\s*ExpandConstant\s*\(\s*'\{tmp\}\\_verify-facial-cli\.exe'\s*\)") -and
        (@([regex]::Matches($initializeSetupBlock, '(?im)^\s*DefaultConfigSource\s*:=' )).Count -eq 1) -and
        ($initializeSetupBlock -match "DefaultConfigSource\s*:=\s*ExpandConstant\s*\(\s*'\{tmp\}\\_verify-release-default-config\.json'\s*\)") -and
        (@([regex]::Matches($initializeSetupBlock, '(?im)^\s*ContractSource\s*:=' )).Count -eq 1) -and
        ($initializeSetupBlock -match "ContractSource\s*:=\s*ExpandConstant\s*\(\s*'\{tmp\}\\_verify-update-mode-contract\.txt'\s*\)") -and
        (@([regex]::Matches($initializeSetupBlock, '(?im)^\s*ReceiptSource\s*:=' )).Count -eq 1) -and
        ($initializeSetupBlock -match "ReceiptSource\s*:=\s*ExpandConstant\s*\(\s*'\{tmp\}\\_verify-facialverify-receipt\.json'\s*\)")

    $writeBindingsMatch =
        $assignmentBindingsMatch -and
        (@([regex]::Matches($initializeSetupBlock, '(?i)\bForceDirectories\s*\(')).Count -eq 1) -and
        ($initializeSetupBlock -match '(?s)ForceDirectories\s*\(\s*VerifyDir\s*\)') -and
        (@([regex]::Matches($initializeSetupBlock, '(?i)\bExtractTemporaryFile\s*\(')).Count -eq 4) -and
        (($verifyExtractNames -join "`n") -ceq ($expectedVerifyExtractNames -join "`n")) -and
        (@([regex]::Matches($initializeSetupBlock, '(?i)\bCopyFile\s*\(')).Count -eq 6) -and
        $copyBindingsMatch -and
        (@([regex]::Matches($initializeSetupBlock, '(?i)\bSaveStringToFile\s*\(')).Count -eq 2) -and
        ($initializeSetupBlock -match "ContractSource\s*:=\s*ExpandConstant\s*\(\s*'\{tmp\}\\_verify-update-mode-contract\.txt'\s*\)") -and
        ($initializeSetupBlock -match "ReceiptSource\s*:=\s*ExpandConstant\s*\(\s*'\{tmp\}\\_verify-facialverify-receipt\.json'\s*\)") -and
        ($initializeSetupBlock -match '(?s)SaveStringToFile\s*\(\s*ContractSource\s*,.*?,\s*False\s*\)') -and
        ($initializeSetupBlock -match '(?s)SaveStringToFile\s*\(\s*ReceiptSource\s*,.*?,\s*False\s*\)') -and
        (($verifyTargetNames -join "`n") -ceq ($expectedVerifyTargetNames -join "`n"))

    $passed =
        (($reachable -join "`n") -ceq ($expectedReachable -join "`n")) -and
        (($calls -join "`n") -ceq ($expectedCalls -join "`n")) -and
        $writeBindingsMatch
    return [pscustomobject]@{
        passed = $passed
        reachable = $reachable
        calls = $calls
        source = $reachableSource
        assignment_bindings = $assignmentBindingsMatch
        copy_bindings = $copyBindingsMatch
        write_bindings = $writeBindingsMatch
        extract_names = $verifyExtractNames
        target_names = $verifyTargetNames
    }
}

function Add-InnoCounterfactualStatement {
    param(
        [Parameter(Mandatory = $true)][string]$Code,
        [Parameter(Mandatory = $true)][string]$RoutineName,
        [Parameter(Mandatory = $true)][string]$Statement
    )
    $map = Get-InnoRoutineMap -Code $Code
    if (-not $map.ContainsKey($RoutineName)) { return $Code }
    $original = $map[$RoutineName]
    $replacement = [regex]::Replace(
        $original,
        '(?im)^begin\s*$',
        "begin`r`n  $Statement",
        1
    )
    return $Code.Replace($original, $replacement)
}

function Test-DefaultDataReparseSourceContract {
    param([Parameter(Mandatory = $true)][string]$Code)

    $map = Get-InnoRoutineMap -Code $Code
    foreach ($required in @(
        'GetFileAttributesW',
        'GetPathAttributesOrAbsent',
        'AssertPathAndExistingAncestorsNotReparse',
        'AssertDefaultDataCleanupPathsNotReparse',
        'RemoveDirectoryIfEmptyChecked',
        'DeleteDefaultAppOwnedDataChecked',
        'DeleteRelocatedWorkspaceStateChecked'
    )) {
        if (-not $map.ContainsKey($required)) { return $false }
    }

    $attributeBlock = $map['GetPathAttributesOrAbsent']
    $ancestorBlock = $map['AssertPathAndExistingAncestorsNotReparse']
    $defaultGuardBlock = $map['AssertDefaultDataCleanupPathsNotReparse']
    $emptyCleanupBlock = $map['RemoveDirectoryIfEmptyChecked']
    $defaultCleanupBlock = $map['DeleteDefaultAppOwnedDataChecked']
    $relocatedCleanupBlock = $map['DeleteRelocatedWorkspaceStateChecked']

    $firstDefaultMutation = [regex]::Match(
        $defaultCleanupBlock,
        '(?i)\b(?:DelTree|DeleteFile|RemoveDir|RenameFile)\s*\('
    )
    $firstDefaultGuard = [regex]::Match(
        $defaultCleanupBlock,
        '(?i)\bAssertDefaultDataCleanupPathsNotReparse\s*\('
    )
    $firstRelocatedMutation = [regex]::Match(
        $relocatedCleanupBlock,
        '(?i)\b(?:DelTree|DeleteFile|RemoveDir|RenameFile)\s*\('
    )
    $firstRelocatedGuard = [regex]::Match(
        $relocatedCleanupBlock,
        '(?i)\bAssertPathAndExistingAncestorsNotReparse\s*\('
    )

    return (
        ($Code -notmatch '(?im)^\s*FILE_ATTRIBUTE_REPARSE_POINT\s*=') -and
        ($Code -match '(?im)^\s*INVALID_FILE_ATTRIBUTES\s*=\s*\$FFFFFFFF\s*;') -and
        ($Code -match "(?im)^function\s+GetFileAttributesW\(lpFileName:\s*String\):\s*Cardinal;\s*`r?`n\s*external\s+'GetFileAttributesW@kernel32\.dll stdcall';") -and
        ($attributeBlock -match '(?s)Attributes\s*:=\s*GetFileAttributesW\(Path\).*?Attributes\s*<>\s*INVALID_FILE_ATTRIBUTES.*?DLLGetLastError.*?ERROR_FILE_NOT_FOUND.*?ERROR_PATH_NOT_FOUND.*?RaiseException') -and
        ($ancestorBlock -match '(?s)CurrentPath\s*:=\s*RemoveBackslashUnlessRoot\(ExpandFileName\(Path\)\).*?while\s+CurrentPath\s*<>\s*''''\s+do.*?GetPathAttributesOrAbsent\(CurrentPath,\s*Description,\s*Attributes\).*?Attributes\s+and\s+FILE_ATTRIBUTE_REPARSE_POINT.*?RaiseException.*?ExtractFileDir\(CurrentPath\).*?CompareText\(ParentPath,\s*CurrentPath\)') -and
        ($defaultGuardBlock -match '(?s)AssertPathAndExistingAncestorsNotReparse\(DataRoot,.*?AssertPathAndExistingAncestorsNotReparse\(ConfigDir,.*?AssertPathAndExistingAncestorsNotReparse\(ManagedStateDir,.*?AssertPathAndExistingAncestorsNotReparse\(ConfigFile,') -and
        ($emptyCleanupBlock -match '(?s)if\s+not\s+AnyPathExists\(Path\)\s+then\s+exit.*?AssertPathAndExistingAncestorsNotReparse\(Path,\s*Description\).*?DirectoryHasChildren\(Path\).*?RemoveDir\(Path\)') -and
        $firstDefaultMutation.Success -and $firstDefaultGuard.Success -and
        ($firstDefaultGuard.Index -lt $firstDefaultMutation.Index) -and
        (@([regex]::Matches($defaultCleanupBlock, '(?i)\bAssertDefaultDataCleanupPathsNotReparse\s*\(')).Count -eq 3) -and
        ($defaultCleanupBlock -match '(?s)AssertDefaultDataCleanupPathsNotReparse\(DataRoot,\s*ConfigDir,\s*ManagedStateDir,\s*ConfigFile\).*?if\s+ManagedStateWasPresent\s+then.*?AssertDefaultDataCleanupPathsNotReparse\(DataRoot,\s*ConfigDir,\s*ManagedStateDir,\s*ConfigFile\).*?DelTree\(ManagedStateDir,\s*True,\s*True,\s*True\).*?if\s+ConfigFileWasPresent\s+then.*?AssertDefaultDataCleanupPathsNotReparse\(DataRoot,\s*ConfigDir,\s*ManagedStateDir,\s*ConfigFile\).*?DeleteFile\(ConfigFile\)') -and
        $firstRelocatedMutation.Success -and $firstRelocatedGuard.Success -and
        ($firstRelocatedGuard.Index -lt $firstRelocatedMutation.Index) -and
        ($relocatedCleanupBlock -match '(?s)WorkspaceRoot\s*:=\s*RemoveBackslashUnlessRoot\(ExtractFileDir\(NormalizedStateDir\)\).*?CompareText\(NormalizedStateDir,\s*AddBackslash\(WorkspaceRoot\)\s*\+\s*''\.facial''\).*?AssertPathAndExistingAncestorsNotReparse\(WorkspaceRoot,.*?AssertPathAndExistingAncestorsNotReparse\(NormalizedStateDir,.*?AssertPathAndExistingAncestorsNotReparse\(WorkspaceRoot,.*?AssertPathAndExistingAncestorsNotReparse\(NormalizedStateDir,.*?DelTree\(NormalizedStateDir,\s*True,\s*True,\s*True\)')
    )
}

function Test-InnoPreprocessorDirectiveAllowlist {
    param([Parameter(Mandatory = $true)][string]$Raw)

    $expected = @(
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
        '#define AppExe "facial.exe"',
        '#define InstallerSourceSha256 GetSHA256OfFile(SourcePath + "\facial.iss")',
        '#define InstallerSourceSha256First Copy(InstallerSourceSha256, 1, 32)',
        '#define InstallerSourceSha256Last Copy(InstallerSourceSha256, 33, 32)',
        '#define ReleaseDefaultConfigSha256 GetSHA256OfFile(PayloadDir + "\product\config\default.json")',
        '#define RetirementToolSha256 GetSHA256OfFile(PayloadDir + "\product\scripts\retire-legacy-media-db.ps1")'
    )
    $actual = @(
        [regex]::Matches($Raw, '(?im)^\s*#.*$') |
            ForEach-Object { $_.Value.Trim() }
    )
    return (($actual -join "`n") -ceq ($expected -join "`n"))
}

function Test-InnoSectionSequenceAllowlist {
    param([Parameter(Mandatory = $true)][string]$Raw)

    $expected = @('Setup', 'Files', 'InstallDelete', 'Tasks', 'Icons', 'Run', 'Code')
    $actual = @(
        [regex]::Matches($Raw, '(?im)^\s*\[([A-Za-z][A-Za-z0-9]*)\]\s*$') |
            ForEach-Object { $_.Groups[1].Value }
    )
    return (($actual -join "`n") -ceq ($expected -join "`n"))
}

function Get-InnoVerifierSourceTemplateSha256 {
    param(
        [Parameter(Mandatory = $true)][string]$IssRaw,
        [Parameter(Mandatory = $true)][string]$Code
    )

    $dataDirBoundary = [regex]::Match($Code, '(?im)^function\s+DataDir\s*\(\s*\)')
    if (-not $dataDirBoundary.Success) { return $null }
    $preprocessorLines = @(
        [regex]::Matches($IssRaw, '(?im)^\s*#.*$') |
            ForEach-Object { $_.Value.Trim() }
    )
    $verifierPrefix = $Code.Substring(0, $dataDirBoundary.Index)
    $normalizedLines = @(
        ($verifierPrefix -replace "`r`n?", "`n") -split "`n" |
            ForEach-Object { $_.TrimEnd() }
    )
    $normalized =
        "PREPROCESSOR`n" +
        ([string]::Join("`n", $preprocessorLines)) +
        "`nCODE-PREFIX-THROUGH-INITIALIZESETUP`n" +
        ([string]::Join("`n", $normalizedLines)).Trim()
    return Get-TextSha256Lower -Text $normalized
}

try {
    $forcedExeEnumerationPassed = Test-ForcedExeEnumerationProbe
} catch {
    $forcedExeEnumerationPassed = $false
    $violations.Add("forced hidden/system EXE enumeration probe failed: $($_.Exception.Message)")
}
if (-not $forcedExeEnumerationPassed) {
    $violations.Add("forced EXE enumeration does not discover both a hidden root EXE and an EXE below a hidden directory.")
}

$shaProbeA = 'a' * 64
$shaProbeB = 'b' * 64
$sha256PairCounterfactualPassed =
    (Test-Sha256PairMatch -Left $shaProbeA -Right $shaProbeA) -and
    (-not (Test-Sha256PairMatch -Left $shaProbeA -Right $shaProbeB))
if (-not $sha256PairCounterfactualPassed) {
    $violations.Add("SHA-256 equality helper did not reject a controlled payload/portable mismatch.")
}

$retirementToolCounterfactualHash = "A" * 64
$retirementToolHashCounterfactualPassed =
    (Test-FrozenRetirementToolHashSet -Hashes @(
        $frozenRetirementToolSha256,
        $frozenRetirementToolSha256,
        $frozenRetirementToolSha256
    )) -and
    (-not (Test-FrozenRetirementToolHashSet -Hashes @(
        $retirementToolCounterfactualHash,
        $retirementToolCounterfactualHash,
        $retirementToolCounterfactualHash
    )))
if (-not $retirementToolHashCounterfactualPassed) {
    $violations.Add("frozen WP-079 retirement-tool hash gate did not reject a synchronized mutated source/payload/export counterfactual.")
}

if (-not (Test-Path -LiteralPath $packageReleaseScript -PathType Leaf)) {
    $violations.Add("missing release packager source: product/scripts/package-release.ps1")
} else {
    $packageReleaseRaw = Get-Content -Raw -LiteralPath $packageReleaseScript
    $packageReleaseSourceContractPassed =
        Test-PackageReleaseSafetySourceContract -Raw $packageReleaseRaw
    $forcedPackageExeEnumerationLines = @(
        '    foreach ($oldRootExe in @(Get-ChildItem -LiteralPath $installerDir -Filter "*.exe" -File -Force -ErrorAction SilentlyContinue)) {',
        '        foreach ($legacy in @(Get-ChildItem -LiteralPath $legacyArchive -File -Force)) {',
        '        foreach ($legacy in @(Get-ChildItem -LiteralPath $legacyOutDir -File -Force)) {'
    )
    $packageReleaseForceCounterfactualResults = @(
        foreach ($forcedLine in $forcedPackageExeEnumerationLines) {
            $unforcedLine = $forcedLine.Replace(' -Force', '')
            $counterfactual = $packageReleaseRaw.Replace($forcedLine, $unforcedLine)
            ($counterfactual -cne $packageReleaseRaw) -and
                (-not (Test-PackageReleaseSafetySourceContract -Raw $counterfactual))
        }
    )
    $packageReleaseForceCounterfactualPassed =
        ($packageReleaseForceCounterfactualResults.Count -eq 3) -and
        ($packageReleaseForceCounterfactualResults -notcontains $false)
    if (-not $packageReleaseSourceContractPassed) {
        $violations.Add("package-release.ps1 lacks the exact frozen WP-079 source/staged-payload hash gate or forced superseded-root EXE archive contract.")
    }
    if (-not $packageReleaseForceCounterfactualPassed) {
        $violations.Add("package-release.ps1 source contract did not reject removal of -Force from every installer-root or retired-delivery EXE enumeration.")
    }
}

if (-not (Test-Path -LiteralPath $retirementToolSource -PathType Leaf)) {
    $violations.Add("repository WP-079 retirement tool is missing.")
} else {
    $retirementToolSourceSha256 =
        (Get-FileHash -LiteralPath $retirementToolSource -Algorithm SHA256).Hash.ToUpperInvariant()
    $retirementToolSourcePinnedPassed =
        Test-FrozenRetirementToolHashSet -Hashes @($retirementToolSourceSha256)
    if (-not $retirementToolSourcePinnedPassed) {
        $violations.Add("repository WP-079 retirement tool SHA-256 must equal frozen live-audited digest $frozenRetirementToolSha256; observed $retirementToolSourceSha256.")
    }
}

# Produce a deterministic, traversal-safe state fingerprint for the exact
# filesystem surfaces that a regressed verifier could otherwise modify.
function Get-PathStateFingerprint {
    param([Parameter(Mandatory = $true)][string]$Path)

    $full = [IO.Path]::GetFullPath($Path)
    if (-not (Test-Path -LiteralPath $full)) { return "absent" }
    $rootItem = Get-Item -LiteralPath $full -Force
    if (-not $rootItem.PSIsContainer) {
        return "file:$([Int64]$rootItem.Length):$(Get-Sha256Lower -Path $full)"
    }

    $lines = New-Object System.Collections.Generic.List[string]
    $stack = New-Object System.Collections.Generic.Stack[string]
    $stack.Push($full)
    while ($stack.Count -gt 0) {
        $directory = $stack.Pop()
        $children = @(Get-ChildItem -LiteralPath $directory -Force | Sort-Object FullName)
        foreach ($child in $children) {
            $relative = $child.FullName.Substring($full.Length + 1).Replace('\', '/')
            if (($child.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                $lines.Add("R`t$relative`t$([int]$child.Attributes)")
            } elseif ($child.PSIsContainer) {
                $lines.Add("D`t$relative")
                $stack.Push($child.FullName)
            } else {
                $lines.Add("F`t$relative`t$([Int64]$child.Length)`t$(Get-Sha256Lower -Path $child.FullName)")
            }
        }
    }
    $lineArray = [string[]]$lines.ToArray()
    [Array]::Sort($lineArray, [StringComparer]::Ordinal)
    return "directory:$(Get-TextSha256Lower -Text ([string]::Join("`n", $lineArray)))"
}

# Registry reads are intentionally external and read-only. Capturing the exact
# query output before and after FACIALVERIFY proves that a verifier regression
# did not register or alter the current Facial AppId.
function Get-RegistryStateFingerprint {
    param(
        [Parameter(Mandatory = $true)][string]$Key,
        [ValidateSet("32", "64")][string]$View = "64"
    )

    if ($Key.Contains('"') -or $Key.Contains("`r") -or
        $Key.Contains("`n") -or $Key.Contains([string][char]0)) {
        throw "Registry fingerprint key contains an unsupported command-line character."
    }

    $regExe = Join-Path $env:SystemRoot "System32\reg.exe"
    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = $regExe
    $startInfo.Arguments = 'query "{0}" /s /reg:{1}' -f $Key, $View
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true

    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $startInfo
    $stdout = ""
    $stderr = ""
    $exitCode = $null
    try {
        # Process.Start failures deliberately propagate: a missing or unusable
        # reg.exe is not equivalent to a successfully queried missing key.
        if (-not $process.Start()) {
            throw "Could not start the read-only registry fingerprint query."
        }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(15000)) {
            try {
                $process.Kill()
            } catch {
                throw "Read-only registry fingerprint query exceeded 15000 ms and could not be stopped: $($_.Exception.Message)"
            }
            if (-not $process.WaitForExit(5000)) {
                throw "Read-only registry fingerprint query did not stop within 5000 ms after timeout termination."
            }
            throw "Read-only registry fingerprint query exceeded 15000 ms."
        }
        # Flush asynchronous redirected-stream completion on .NET Framework.
        $process.WaitForExit()
        $stdout = $stdoutTask.GetAwaiter().GetResult()
        $stderr = $stderrTask.GetAwaiter().GetResult()
        $exitCode = $process.ExitCode
    } finally {
        $process.Dispose()
    }

    $normalizedOutput =
        "stdout`n" + ([string]$stdout -replace "`r`n?", "`n") +
        "`nstderr`n" + ([string]$stderr -replace "`r`n?", "`n")
    return "exit=$exitCode;sha256=$(Get-TextSha256Lower -Text $normalizedOutput)"
}

function Test-RegistryFingerprintReadContractProbe {
    $missingKey = "HKCU\Software\Facial-Registry-Fingerprint-Missing-Key-Probe-" +
        [guid]::NewGuid().ToString("N")
    # HKLM\SYSTEM is shared rather than redirected by WOW64. This small key is
    # present in both views, and its space-containing path exercises quoting.
    $existingKey = "HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment"

    foreach ($view in @("32", "64")) {
        $missingFirst = Get-RegistryStateFingerprint -Key $missingKey -View $view
        $missingSecond = Get-RegistryStateFingerprint -Key $missingKey -View $view
        $missingExit = [regex]::Match(
            $missingFirst,
            '^exit=(\d+);sha256=[0-9a-f]{64}$'
        )
        if (($missingFirst -cne $missingSecond) -or
            (-not $missingExit.Success) -or
            ([int]$missingExit.Groups[1].Value -ne 1)) {
            return $false
        }

        $existingFirst = Get-RegistryStateFingerprint -Key $existingKey -View $view
        $existingSecond = Get-RegistryStateFingerprint -Key $existingKey -View $view
        $existingExit = [regex]::Match(
            $existingFirst,
            '^exit=(\d+);sha256=[0-9a-f]{64}$'
        )
        if (($existingFirst -cne $existingSecond) -or
            (-not $existingExit.Success) -or
            ([int]$existingExit.Groups[1].Value -ne 0)) {
            return $false
        }
    }
    return $true
}

try {
    $registryFingerprintReadContractProbePassed =
        Test-RegistryFingerprintReadContractProbe
} catch {
    $violations.Add("read-only registry fingerprint 32/64-view contract probe threw: $($_.Exception.Message)")
}
if (-not $registryFingerprintReadContractProbePassed) {
    $violations.Add("read-only registry fingerprints must be stable with exact missing-key exit 1 and known-existing-key exit 0 in both 32-bit and 64-bit views.")
}

function Test-RootedUserPathString {
    param([AllowEmptyString()][string]$Value)

    $candidate = $Value.Trim()
    if ([string]::IsNullOrWhiteSpace($candidate)) { return $false }
    try {
        if ([IO.Path]::IsPathRooted($candidate)) { return $true }
    } catch {
        return $true
    }
    if ($candidate -match '(?i)^(?:[a-z]:|\\\\|//|~(?:[\\/]|$)|%(?:userprofile|localappdata|appdata|programdata|programfiles(?:\(x86\))?|temp|tmp)%|\$(?:env:)?(?:home|userprofile|localappdata|appdata|temp|tmp)(?:[\\/]|$)|\$\{(?:home|userprofile|localappdata|appdata|temp|tmp)\})') {
        return $true
    }
    return $candidate -match '(?i)(?:^|[\\/])(?:users|home)[\\/][^\\/]+'
}

function Find-RootedJsonStrings {
    param(
        [AllowNull()]$Value,
        [Parameter(Mandatory = $true)][string]$JsonPath
    )

    if ($null -eq $Value) { return }
    if ($Value -is [string]) {
        if (Test-RootedUserPathString -Value $Value) {
            [pscustomobject]@{ path = $JsonPath; value = $Value }
        }
        return
    }
    if ($Value -is [System.Collections.IDictionary]) {
        foreach ($key in @($Value.Keys | Sort-Object)) {
            Find-RootedJsonStrings -Value $Value[$key] -JsonPath "$JsonPath.$key"
        }
        return
    }
    if ($Value -is [System.Collections.IEnumerable] -and -not ($Value -is [string])) {
        $index = 0
        foreach ($entry in $Value) {
            Find-RootedJsonStrings -Value $entry -JsonPath "$JsonPath[$index]"
            $index++
        }
        return
    }
    if ($Value -is [pscustomobject]) {
        foreach ($property in @($Value.PSObject.Properties | Sort-Object Name)) {
            Find-RootedJsonStrings -Value $property.Value -JsonPath "$JsonPath.$($property.Name)"
        }
    }
}

if (-not (Test-Path -LiteralPath $releaseDefaultConfigSource -PathType Leaf)) {
    $violations.Add("repository release default config is missing: product/config/release-default.json ($releaseDefaultConfigContract)")
} else {
    $releaseDefaultConfigSha256 = Get-Sha256Lower -Path $releaseDefaultConfigSource
    $releaseDefaultConfigSourceJson = $null
    try {
        $releaseDefaultConfigSourceJson = Get-Content -Raw -LiteralPath $releaseDefaultConfigSource | ConvertFrom-Json -ErrorAction Stop
    } catch {
        $violations.Add("repository release default config is malformed JSON ($releaseDefaultConfigContract): $($_.Exception.Message)")
    }
    if ($null -ne $releaseDefaultConfigSourceJson) {
        $rootedSourceStrings = @(Find-RootedJsonStrings -Value $releaseDefaultConfigSourceJson -JsonPath '$')
        foreach ($rooted in $rootedSourceStrings) {
            $violations.Add("repository release default config contains a rooted user/machine path at $($rooted.path): '$($rooted.value)' ($releaseDefaultConfigContract).")
        }
        if ($rootedSourceStrings.Count -eq 0) {
            $releaseDefaultConfigSourcePassed = $true
        }
    }
}

# Never execute a setup artifact until the source that package-release.ps1
# compiles has proved the exact validator-only early-exit branch. Without this
# preflight, a typo or removal of FACIALVERIFY could turn this checker into a
# silent current-user installation before the missing exports are noticed.
if (Test-Path -LiteralPath $installerScript -PathType Leaf) {
    $installerSourceSha256 = Get-Sha256Lower -Path $installerScript
    $preflightIssRaw = Get-Content -Raw -LiteralPath $installerScript
    $installerExactSourceHashPassed =
        (Get-NormalizedSourceSha256Lower -Text $preflightIssRaw) -ceq
            $expectedInstallerNormalizedSourceSha256
    $installerNormalizedSourceCounterfactualPassed =
        (Get-NormalizedSourceSha256Lower `
            -Text ($preflightIssRaw + "`r`n; counterfactual source mutation`r`n")) -cne
                $expectedInstallerNormalizedSourceSha256
    $preflightSectionSequencePassed =
        Test-InnoSectionSequenceAllowlist -Raw $preflightIssRaw
    $sectionCounterfactual = $preflightIssRaw +
        "`r`n[Code]`r`nprocedure CounterfactualSecondCode; begin end;`r`n"
    $preflightSectionCounterfactualPassed =
        -not (Test-InnoSectionSequenceAllowlist -Raw $sectionCounterfactual)
    $preflightSetup = Get-InnoSection -Raw $preflightIssRaw -Name "Setup"
    $preflightCode = Get-InnoSection -Raw $preflightIssRaw -Name "Code"
    $observedVerifierSourceTemplateSha256 = Get-InnoVerifierSourceTemplateSha256 `
        -IssRaw $preflightIssRaw `
        -Code $preflightCode
    $verifierSourceTemplateHashPassed =
        $observedVerifierSourceTemplateSha256 -ceq $expectedVerifierSourceTemplateSha256
    $preprocessorAllowlistPassed =
        Test-InnoPreprocessorDirectiveAllowlist -Raw $preflightIssRaw
    $includeCounterfactual = $preflightIssRaw +
        "`r`n#include `"counterfactual-verifier-mutation.iss`"`r`n"
    $installerPreprocessorCounterfactualPassed =
        (-not (Test-InnoPreprocessorDirectiveAllowlist -Raw $includeCounterfactual)) -and
        ((Get-InnoVerifierSourceTemplateSha256 `
            -IssRaw $includeCounterfactual `
            -Code $preflightCode) -cne $expectedVerifierSourceTemplateSha256)
    $verifierAnalysis = Get-InnoVerifierAnalysis -Code $preflightCode
    $deleteHelperCounterfactual = Add-InnoCounterfactualStatement `
        -Code $preflightCode `
        -RoutineName 'IsSafeVerifierDestination' `
        -Statement 'DeleteFile(Path);'
    $externalWriteCounterfactual = Add-InnoCounterfactualStatement `
        -Code $preflightCode `
        -RoutineName 'InitializeSetup' `
        -Statement "SaveStringToFile(ExpandConstant('{localappdata}\Facial\.facial-media-retirement\counterfactual-write.txt'), 'mutated', False);"
    $bareHelperCounterfactual = $preflightCode + @'

procedure CounterfactualWrite;
begin
  SaveStringToFile(ExpandConstant('{localappdata}\Facial\.facial-media-retirement\counterfactual-bare-write.txt'),
    'mutated', False);
end;
'@
    $bareHelperCounterfactual = Add-InnoCounterfactualStatement `
        -Code $bareHelperCounterfactual `
        -RoutineName 'InitializeSetup' `
        -Statement 'CounterfactualWrite;'
    $contractSourceReassignmentCounterfactual = Add-InnoCounterfactualStatement `
        -Code $preflightCode `
        -RoutineName 'InitializeSetup' `
        -Statement "ContractSource := ExpandConstant('{localappdata}\Facial\.facial-media-retirement\counterfactual-contract-write.txt');"
    $verifyDirReassignmentCounterfactual = Add-InnoCounterfactualStatement `
        -Code $preflightCode `
        -RoutineName 'InitializeSetup' `
        -Statement "VerifyDir := ExpandConstant('{localappdata}\Facial\.facial-media-retirement\counterfactual-export');"
    $deleteHelperCounterfactualRejected = -not (
        Get-InnoVerifierAnalysis -Code $deleteHelperCounterfactual
    ).passed
    $externalWriteCounterfactualRejected = -not (
        Get-InnoVerifierAnalysis -Code $externalWriteCounterfactual
    ).passed
    $bareHelperCounterfactualRejected = -not (
        Get-InnoVerifierAnalysis -Code $bareHelperCounterfactual
    ).passed
    $contractSourceReassignmentRejected = -not (
        Get-InnoVerifierAnalysis -Code $contractSourceReassignmentCounterfactual
    ).passed
    $verifyDirReassignmentRejected = -not (
        Get-InnoVerifierAnalysis -Code $verifyDirReassignmentCounterfactual
    ).passed
    $templateCounterfactualsRejected = @(
        $deleteHelperCounterfactual,
        $externalWriteCounterfactual,
        $bareHelperCounterfactual,
        $contractSourceReassignmentCounterfactual,
        $verifyDirReassignmentCounterfactual
    ) | ForEach-Object {
        (Get-InnoVerifierSourceTemplateSha256 -IssRaw $preflightIssRaw -Code $_) -cne
            $expectedVerifierSourceTemplateSha256
    }
    $installerVerifierCallGraphCounterfactualsPassed =
        $deleteHelperCounterfactualRejected -and
        $externalWriteCounterfactualRejected -and
        $bareHelperCounterfactualRejected -and
        $contractSourceReassignmentRejected -and
        $verifyDirReassignmentRejected -and
        ($templateCounterfactualsRejected -notcontains $false)

    $defaultDataReparseSourceContractPassed =
        Test-DefaultDataReparseSourceContract -Code $preflightCode
    $reparseCounterfactual = $preflightCode.Replace(
        "  AssertPathAndExistingAncestorsNotReparse(DataRoot, 'Facial data-root');",
        "  { counterfactual omitted DataRoot ancestor guard }"
    )
    $defaultDataReparseCounterfactualPassed =
        ($reparseCounterfactual -cne $preflightCode) -and
        (-not (Test-DefaultDataReparseSourceContract -Code $reparseCounterfactual))
    $appIdMatch = [regex]::Match($preflightSetup, '(?im)^\s*AppId\s*=\s*\{\{([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\}\s*$')
    if ($appIdMatch.Success) {
        $installerAppGuid = $appIdMatch.Groups[1].Value.ToUpperInvariant()
    }
    $verifyExitMatches = @([regex]::Matches(
        $preflightCode,
        '(?im)^\s*FACIAL_VERIFY_EXIT_CODE\s*=\s*(\d+)\s*;\s*$'
    ))
    if ($verifyExitMatches.Count -eq 1) {
        $expectedVerifierExitCode = [int]$verifyExitMatches[0].Groups[1].Value
    }
    $initializeSetupMatch = [regex]::Match(
        $preflightCode,
        '(?s)function\s+InitializeSetup\s*:\s*Boolean\s*;.*?end\s*;\s*(?=function\s+DataDir\s*\()'
    )
    $initializeSetupBlock = if ($initializeSetupMatch.Success) { $initializeSetupMatch.Value } else { "" }
    $verifyExtractNames = @(
        [regex]::Matches($initializeSetupBlock, "(?i)ExtractTemporaryFile\s*\(\s*'([^']+)'\s*\)") |
            ForEach-Object { $_.Groups[1].Value }
    )
    $expectedVerifyExtractNames = @(
        "_verify-facial.exe",
        "_verify-facial-cli.exe",
        "_verify-retire-legacy-media-db.ps1",
        "_verify-release-default-config.json"
    )
    $verifyTargetNames = @(
        [regex]::Matches($initializeSetupBlock, "(?i)AssertVerifierTargetAbsent\s*\(\s*VerifyDir\s*,\s*'([^']+)'\s*\)") |
            ForEach-Object { $_.Groups[1].Value }
    )
    $expectedVerifyTargetNames = @(
        "facial.exe",
        "facial-cli.exe",
        "retire-legacy-media-db.ps1",
        "release-default-config.json",
        "update-mode-contract.txt",
        "facialverify-receipt.json"
    )
    $verifierSourceContractChecks = @(
        ($preflightIssRaw -match '(?im)^\s*#define\s+InstallerSourceSha256\s+GetSHA256OfFile\(SourcePath\s*\+\s*"\\facial\.iss"\)\s*$'),
        $installerExactSourceHashPassed,
        $installerNormalizedSourceCounterfactualPassed,
        $preflightSectionSequencePassed,
        $preflightSectionCounterfactualPassed,
        ($preflightIssRaw -match '(?im)^\s*#define\s+InstallerSourceSha256First\s+Copy\(InstallerSourceSha256,\s*1,\s*32\)\s*$'),
        ($preflightIssRaw -match '(?im)^\s*#define\s+InstallerSourceSha256Last\s+Copy\(InstallerSourceSha256,\s*33,\s*32\)\s*$'),
        ($preflightIssRaw -match '(?im)^\s*#define\s+ReleaseDefaultConfigSha256\s+GetSHA256OfFile\(PayloadDir\s*\+\s*"\\product\\config\\default\.json"\)\s*$'),
        ($preflightIssRaw -match '(?im)^\s*#define\s+RetirementToolSha256\s+GetSHA256OfFile\(PayloadDir\s*\+\s*"\\product\\scripts\\retire-legacy-media-db\.ps1"\)\s*$'),
        ($preflightSetup -match '(?im)^\s*VersionInfoDescription\s*=\s*Facial sha256\[0:32\] \{#InstallerSourceSha256First\}\s*$'),
        ($preflightSetup -match '(?im)^\s*VersionInfoProductName\s*=\s*Facial sha256\[32:64\] \{#InstallerSourceSha256Last\}\s*$'),
        ($preflightSetup -match '(?im)^\s*PrivilegesRequiredOverridesAllowed\s*=\s*commandline\s*$'),
        ($installerAppGuid -ceq "8F2A9C7E-3B41-4D6E-9A1F-FAC1A100D025"),
        ($verifyExitMatches.Count -eq 1 -and $expectedVerifierExitCode -eq 1),
        $initializeSetupMatch.Success,
        ($initializeSetupBlock -match "VerifyDir\s*:=\s*ExpandConstant\s*\(\s*'\{param:FACIALVERIFY\|\}'\s*\)"),
        ($preflightCode -match '(?s)function\s+IsSafeVerifierDestination\s*\(\s*Path:\s*String\s*\).*?ExpandConstant\(''\{localappdata\}\\Temp''\).*?AddBackslash\(TempRoot\).*?ExtractFileDir\(NormalizedPath\).*?TempRoot.*?''facial-installer-verify-''.*?Length\(Suffix\)\s*=\s*32.*?IsHexString\(Suffix\)'),
        ($preflightCode -match '(?s)procedure\s+AssertVerifierTargetAbsent\s*\(\s*VerifyDir:\s*String;\s*Name:\s*String\s*\).*?FileExists\(Target\)\s+or\s+DirExists\(Target\).*?RaiseException'),
        ($initializeSetupBlock -match '(?s)if\s+not\s+IsSafeVerifierDestination\(VerifyDir\)\s+then\s+RaiseException.*?AssertVerifierTargetAbsent\(VerifyDir,\s*''facial\.exe''\).*?AssertVerifierTargetAbsent\(VerifyDir,\s*''facialverify-receipt\.json''\).*?ForceDirectories\(VerifyDir\)'),
        ($initializeSetupBlock -match '(?s)if\s+FileExists\(VerifyDir\)\s+or\s+DirExists\(VerifyDir\)\s+then\s+RaiseException.*?requires a new export directory'),
        (($verifyTargetNames -join "`n") -ceq ($expectedVerifyTargetNames -join "`n")),
        (($verifyExtractNames -join "`n") -ceq ($expectedVerifyExtractNames -join "`n")),
        (@([regex]::Matches($initializeSetupBlock, '(?i)\bCopyFile\s*\(')).Count -eq 6),
        (@([regex]::Matches($initializeSetupBlock, '(?i),\s*True\s*\)\s*then')).Count -eq 6),
        ($initializeSetupBlock -match "CopyFile\s*\(\s*DefaultConfigSource\s*,\s*VerifyDir\s*\+\s*'\\release-default-config\.json'"),
        ($initializeSetupBlock -match "SaveStringToFile\s*\(\s*ReceiptSource"),
        ($initializeSetupBlock -match "CopyFile\s*\(\s*ReceiptSource\s*,\s*VerifyDir\s*\+\s*'\\facialverify-receipt\.json'\s*,\s*True\s*\)"),
        ($initializeSetupBlock -match '"installer_source_sha256":\s*"\{#InstallerSourceSha256\}"'),
        ($initializeSetupBlock -match '"verifier_exit_code":\s*''\s*\+\s*IntToStr\s*\(\s*FACIAL_VERIFY_EXIT_CODE\s*\)'),
        ($initializeSetupBlock -match '"release_default_config_sha256":\s*"\{#ReleaseDefaultConfigSha256\}"'),
        ($initializeSetupBlock -match '"retirement_tool_sha256":\s*"\{#RetirementToolSha256\}"'),
        ($initializeSetupBlock -match 'retirement_tool_sha256=\{#RetirementToolSha256\}'),
        ($initializeSetupBlock -match '(?s)Result\s*:=\s*False\s*;\s*exit\s*;\s*end\s*;\s*Result\s*:=\s*True\s*;\s*end\s*;'),
        ($initializeSetupBlock -notmatch '(?i)\b(?:Exec|ShellExec|RegWrite\w*|DelTree|DeleteFile|RenameFile|CreateShellLink|RestartReplace)\s*\('),
        $verifierAnalysis.passed,
        $verifierSourceTemplateHashPassed,
        $installerVerifierCallGraphCounterfactualsPassed,
        $preprocessorAllowlistPassed,
        $installerPreprocessorCounterfactualPassed,
        $defaultDataReparseSourceContractPassed,
        $defaultDataReparseCounterfactualPassed
    )
    if ($verifierSourceContractChecks -notcontains $false) {
        $installerVerifierSourceContractPassed = $true
    } else {
        $violations.Add("installer FACIALVERIFY prelaunch source/call-graph/write-binding or data reparse contract is missing or malformed; refusing to execute setup for payload verification.")
    }
} else {
    $violations.Add("missing installer source before compiled setup verification: installer/facial.iss")
}

$lockRaw = if (Test-Path -LiteralPath $lockPath -PathType Leaf) {
    Get-Content -Raw -LiteralPath $lockPath
} else {
    $null
}
$surrealDbVersion = if ($null -ne $lockRaw) {
    Get-LockPackageVersion -Raw $lockRaw -Name "surrealdb"
} else {
    $null
}
if (-not $surrealDbVersion) {
    $violations.Add("Cargo.lock does not identify the embedded SurrealDB package version.")
}

if (-not $version) {
    $violations.Add("product/Cargo.toml has no numeric [package] version (major.minor.patch).")
}

$facialLockVersion = if ($null -ne $lockRaw) {
    Get-LockPackageVersion -Raw $lockRaw -Name "facial"
} else {
    $null
}
$topologyVersion = $null
if (Test-Path -LiteralPath $topologyPath -PathType Leaf) {
    $topologyRaw = Get-Content -Raw -LiteralPath $topologyPath
    $topologyVersionMatch = [regex]::Match(
        $topologyRaw,
        '(?ms)\Aproject:\s*.*?^\s{2}version:\s*(\d+\.\d+\.\d+)\s*$'
    )
    if ($topologyVersionMatch.Success) {
        $topologyVersion = $topologyVersionMatch.Groups[1].Value
    }
}
$versionSourcesAgree =
    ($null -ne $version) -and
    ($null -ne $facialLockVersion) -and
    ($null -ne $topologyVersion) -and
    ($version -ceq $facialLockVersion) -and
    ($version -ceq $topologyVersion)
if (-not $versionSourcesAgree) {
    $violations.Add("release version disagreement: Cargo.toml='$version', Cargo.lock facial='$facialLockVersion', topology.yaml project.version='$topologyVersion'.")
}

# The installer root must expose exactly the current portable/setup pair.
$rootExes = @(Get-ExeFilesForce -Path $installer)
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
    $expectedArtifactNames = @($expectedPortable, $expectedSetup) | Sort-Object
    $observedArtifactNames = @($rootExes.Name | Sort-Object)
    $releaseVersionAgreementPassed =
        $versionSourcesAgree -and
        (($observedArtifactNames -join "`n") -ceq ($expectedArtifactNames -join "`n"))
    $portablePath = Join-Path $installer $expectedPortable
    if (Test-Path -LiteralPath $portablePath -PathType Leaf) {
        $portableArtifactSha256 = Get-Sha256Lower -Path $portablePath
        $portableSubsystem = Get-PeSubsystem -Path $portablePath
        if ($portableSubsystem -ne 2) {
            $violations.Add("current portable must use IMAGE_SUBSYSTEM_WINDOWS_GUI (2); observed '$portableSubsystem'.")
        }
    }

    $setupPath = Join-Path $installer $expectedSetup
    if (Test-Path -LiteralPath $setupPath -PathType Leaf) {
        $setupArtifactSha256 = Get-Sha256Lower -Path $setupPath
        $setupVersionInfo = [Diagnostics.FileVersionInfo]::GetVersionInfo($setupPath)
        $expectedFileDescription = "Facial sha256[0:32] $($installerSourceSha256.Substring(0, 32))"
        $expectedProductName = "Facial sha256[32:64] $($installerSourceSha256.Substring(32, 32))"
        $observedFileDescription = ([string]$setupVersionInfo.FileDescription).TrimEnd()
        $observedProductName = ([string]$setupVersionInfo.ProductName).TrimEnd()
        if ($observedFileDescription -cne $expectedFileDescription -or
            $observedProductName -cne $expectedProductName) {
            $violations.Add("compiled setup PE metadata is not bound to current installer/facial.iss (expected FileDescription/ProductName hash halves '$expectedFileDescription' / '$expectedProductName', observed '$($setupVersionInfo.FileDescription)' / '$($setupVersionInfo.ProductName)').")
        } else {
            $installerSourceBindingPassed = $true
        }
    }
    if ((Test-Path -LiteralPath $setupPath -PathType Leaf) -and
        ($null -ne $portableArtifactSha256) -and
        $installerVerifierSourceContractPassed -and
        $installerSourceBindingPassed -and
        $sha256PairCounterfactualPassed -and
        $retirementToolHashCounterfactualPassed -and
        $retirementToolSourcePinnedPassed -and
        $packageReleaseSourceContractPassed -and
        $packageReleaseForceCounterfactualPassed) {
        # Independently extract the compiled setup payload. This exercises the
        # actual published installer without installing it or trusting the
        # packaging script's pre-ISCC staging checks.
        $tempRoot = [IO.Path]::GetFullPath((Join-Path ([Environment]::GetFolderPath("LocalApplicationData")) "Temp"))
        $verifyDir = Join-Path $tempRoot ("facial-installer-verify-" + [guid]::NewGuid().ToString("N"))
        $verifyFull = [IO.Path]::GetFullPath($verifyDir)
        if (-not $verifyFull.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase)) {
            $violations.Add("refused unsafe installer verification path: $verifyFull")
        } else {
            try {
                if ((Get-Sha256Lower -Path $installerScript) -cne $installerSourceSha256) {
                    throw "installer/facial.iss changed after the PE source-binding preflight; refusing to execute setup."
                }
                if ((Get-Sha256Lower -Path $releaseDefaultConfigSource) -cne $releaseDefaultConfigSha256) {
                    throw "product/config/release-default.json changed after release-seed preflight; refusing to execute setup."
                }
                $currentRetirementToolSourceSha256 =
                    (Get-FileHash -LiteralPath $retirementToolSource -Algorithm SHA256).Hash.ToUpperInvariant()
                if ($currentRetirementToolSourceSha256 -cne $retirementToolSourceSha256 -or
                    -not (Test-FrozenRetirementToolHashSet -Hashes @($currentRetirementToolSourceSha256))) {
                    throw "product/scripts/retire-legacy-media-db.ps1 changed or left the frozen live-audited digest after preflight; refusing to execute setup."
                }
                if ((Get-Sha256Lower -Path $setupPath) -cne $setupArtifactSha256) {
                    throw "compiled setup changed after its PE source-binding preflight; refusing to execute setup."
                }
                if ((Get-Sha256Lower -Path $portablePath) -cne $portableArtifactSha256) {
                    throw "root portable changed after its payload-identity preflight; refusing to execute setup."
                }
                $fallbackInstallDir = Join-Path $verifyFull "unexpected-install"
                $shortcutProbePaths = @(
                    $fallbackInstallDir,
                    (Join-Path ([Environment]::GetFolderPath("ProgramFiles")) "Facial"),
                    (Join-Path ([Environment]::GetFolderPath("ProgramFilesX86")) "Facial"),
                    (Join-Path ([Environment]::GetFolderPath("Programs")) "Facial"),
                    (Join-Path ([Environment]::GetFolderPath("CommonPrograms")) "Facial"),
                    (Join-Path ([Environment]::GetFolderPath("DesktopDirectory")) "Facial.lnk"),
                    (Join-Path ([Environment]::GetFolderPath("CommonDesktopDirectory")) "Facial.lnk")
                ) | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
                $pathStateBefore = @{}
                foreach ($probePath in $shortcutProbePaths) {
                    $pathStateBefore[[IO.Path]::GetFullPath($probePath)] = Get-PathStateFingerprint -Path $probePath
                }
                $uninstallSubKey = "Software\Microsoft\Windows\CurrentVersion\Uninstall\{$installerAppGuid}_is1"
                $registryProbeKeys = @(
                    [pscustomobject]@{ id = "hkcu64"; key = "HKCU\$uninstallSubKey"; view = "64" },
                    [pscustomobject]@{ id = "hkcu32"; key = "HKCU\$uninstallSubKey"; view = "32" },
                    [pscustomobject]@{ id = "hklm64"; key = "HKLM\$uninstallSubKey"; view = "64" },
                    [pscustomobject]@{ id = "hklm32"; key = "HKLM\$uninstallSubKey"; view = "32" }
                )
                $registryStateBefore = @{}
                foreach ($probe in $registryProbeKeys) {
                    $registryStateBefore[$probe.id] = Get-RegistryStateFingerprint -Key $probe.key -View $probe.view
                }

                $setupArgs = '/CURRENTUSER /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /NOICONS "/DIR={0}" /FACIALVERIFY="{1}"' -f $fallbackInstallDir, $verifyFull
                $process = Start-Process -FilePath $setupPath -ArgumentList $setupArgs -Wait -PassThru -WindowStyle Hidden
                $sideEffectsUnchanged = $true
                if ($process.ExitCode -ne $expectedVerifierExitCode) {
                    $sideEffectsUnchanged = $false
                    $violations.Add("compiled setup FACIALVERIFY did not use the exact early-abort exit code $expectedVerifierExitCode; observed $($process.ExitCode).")
                }
                foreach ($probePath in $shortcutProbePaths) {
                    $probeFull = [IO.Path]::GetFullPath($probePath)
                    $afterState = Get-PathStateFingerprint -Path $probePath
                    if ($afterState -cne $pathStateBefore[$probeFull]) {
                        $sideEffectsUnchanged = $false
                        $violations.Add("compiled setup FACIALVERIFY changed a fallback-install or shortcut surface: $probeFull")
                    }
                }
                if (Test-Path -LiteralPath $fallbackInstallDir) {
                    $sideEffectsUnchanged = $false
                    $violations.Add("compiled setup FACIALVERIFY created its fallback installation directory: $fallbackInstallDir")
                }
                foreach ($probe in $registryProbeKeys) {
                    $afterState = Get-RegistryStateFingerprint -Key $probe.key -View $probe.view
                    if ($afterState -cne $registryStateBefore[$probe.id]) {
                        $sideEffectsUnchanged = $false
                        $violations.Add("compiled setup FACIALVERIFY changed the Facial uninstall registry key ($($probe.id)).")
                    }
                }
                $overwriteStateBefore = Get-PathStateFingerprint -Path $verifyFull
                $overwriteProcess = Start-Process -FilePath $setupPath -ArgumentList $setupArgs -Wait -PassThru -WindowStyle Hidden
                if ($overwriteProcess.ExitCode -ne $expectedVerifierExitCode) {
                    $sideEffectsUnchanged = $false
                    $violations.Add("compiled setup FACIALVERIFY overwrite-refusal probe returned $($overwriteProcess.ExitCode); expected the exact early-abort code $expectedVerifierExitCode.")
                }
                if ((Get-PathStateFingerprint -Path $verifyFull) -cne $overwriteStateBefore) {
                    $sideEffectsUnchanged = $false
                    $violations.Add("compiled setup FACIALVERIFY changed a pre-existing export target instead of refusing the whole export.")
                }

                $unsafeVerifyDir = Join-Path $tempRoot ("facial-verifier-unsafe-" + [guid]::NewGuid().ToString("N"))
                $unsafeFallbackInstallDir = Join-Path $tempRoot ("facial-verifier-unsafe-install-" + [guid]::NewGuid().ToString("N"))
                $unsafeSetupArgs = '/CURRENTUSER /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /NOICONS "/DIR={0}" /FACIALVERIFY="{1}"' -f $unsafeFallbackInstallDir, $unsafeVerifyDir
                $unsafeProcess = Start-Process -FilePath $setupPath -ArgumentList $unsafeSetupArgs -Wait -PassThru -WindowStyle Hidden
                if ($unsafeProcess.ExitCode -ne $expectedVerifierExitCode -or
                    (Test-Path -LiteralPath $unsafeVerifyDir) -or
                    (Test-Path -LiteralPath $unsafeFallbackInstallDir)) {
                    $sideEffectsUnchanged = $false
                    $violations.Add("compiled setup FACIALVERIFY did not fail closed on a non-contract temporary output path.")
                }
                foreach ($probe in $registryProbeKeys) {
                    $boundaryAfterState = Get-RegistryStateFingerprint -Key $probe.key -View $probe.view
                    if ($boundaryAfterState -cne $registryStateBefore[$probe.id]) {
                        $sideEffectsUnchanged = $false
                        $violations.Add("compiled setup FACIALVERIFY boundary probes changed the Facial uninstall registry key ($($probe.id)).")
                    }
                }
                foreach ($probePath in $shortcutProbePaths) {
                    $probeFull = [IO.Path]::GetFullPath($probePath)
                    if ((Get-PathStateFingerprint -Path $probePath) -cne $pathStateBefore[$probeFull]) {
                        $sideEffectsUnchanged = $false
                        $violations.Add("compiled setup FACIALVERIFY boundary probes changed a fallback-install or shortcut surface: $probeFull")
                    }
                }
                if ($sideEffectsUnchanged) {
                    $installerVerifierNoSideEffectsPassed = $true
                    $installerVerifierOutputBoundaryPassed = $true
                }
                $payloadGui = Join-Path $verifyFull "facial.exe"
                $payloadCli = Join-Path $verifyFull "facial-cli.exe"
                $payloadRetirementTool = Join-Path $verifyFull "retire-legacy-media-db.ps1"
                $payloadReleaseDefaultConfig = Join-Path $verifyFull "release-default-config.json"
                $updateContractPath = Join-Path $verifyFull "update-mode-contract.txt"
                $verifierReceiptPath = Join-Path $verifyFull "facialverify-receipt.json"
                if (-not (Test-Path -LiteralPath $payloadGui -PathType Leaf)) {
                    $violations.Add("compiled setup did not export its facial.exe payload (exit $($process.ExitCode)).")
                } elseif ((Get-PeSubsystem -Path $payloadGui) -ne 2) {
                    $violations.Add("compiled setup facial.exe payload is not IMAGE_SUBSYSTEM_WINDOWS_GUI (2).")
                } elseif (-not (Test-Sha256PairMatch `
                    -Left (Get-Sha256Lower -Path $portablePath) `
                    -Right $portableArtifactSha256)) {
                    $violations.Add("root portable EXE changed during compiled setup payload extraction.")
                } elseif (-not (Test-Sha256PairMatch `
                    -Left (Get-Sha256Lower -Path $payloadGui) `
                    -Right $portableArtifactSha256)) {
                    $violations.Add("compiled setup facial.exe payload SHA-256 differs from the current root portable EXE.")
                } else {
                    $installerPayloadPortableMatchPassed = $true
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

                    # WP-079: exercise the packaged media store itself. Seed exact
                    # historical filenames with importable-looking data, prove they
                    # are ignored and byte-identical, then prove a new value survives
                    # a separate-process reopen and whole-workspace relocation.
                    $mediaRootA = Join-Path $verifyFull "media-store-smoke-a"
                    $mediaRootB = Join-Path $verifyFull "media-store-smoke-b"
                    $mediaStateA = Join-Path $mediaRootA ".facial\media"
                    New-Item -ItemType Directory -Force -Path $mediaStateA | Out-Null
                    $probeA = Join-Path $mediaRootA "assets\probe.jpg"
                    $legacyJsonA = Join-Path $mediaRootA ".facial\media_metadata.json"
                    $legacyRedbA = Join-Path $mediaStateA "media.redb"
                    $legacyInventoryA = Join-Path $mediaStateA "inventory.redb"
                    $legacyClipA = Join-Path $mediaStateA "clip_index.redb"
                    $legacyJsonPayload = @{
                        version = 1
                        notes = @{ $probeA = "legacy-must-not-import" }
                        tags = @{ $probeA = "legacy-only" }
                        color_labels = @{ $probeA = "red" }
                        favorites = @($probeA)
                    } | ConvertTo-Json -Depth 6
                    [IO.File]::WriteAllText($legacyJsonA, $legacyJsonPayload, [Text.UTF8Encoding]::new($false))
                    [IO.File]::WriteAllBytes($legacyRedbA, [Text.Encoding]::UTF8.GetBytes("wp079-media-redb-sentinel"))
                    [IO.File]::WriteAllBytes($legacyInventoryA, [Text.Encoding]::UTF8.GetBytes("wp079-inventory-redb-sentinel"))
                    [IO.File]::WriteAllBytes($legacyClipA, [Text.Encoding]::UTF8.GetBytes("wp079-clip-redb-sentinel"))
                    $legacyBefore = @{}
                    foreach ($legacyPath in @($legacyJsonA, $legacyRedbA, $legacyInventoryA, $legacyClipA)) {
                        $legacyBefore[[IO.Path]::GetFileName($legacyPath)] = (Get-FileHash -Algorithm SHA256 -LiteralPath $legacyPath).Hash
                    }

                    $envNames = @(
                        "FACIAL_REPO_ROOT",
                        "FACIAL_CONFIG_PATH",
                        "FACIAL_WORKSPACE_ROOT",
                        "FACIAL_DATA_ROOT",
                        "FACIAL_WORKTREES_ROOT"
                    )
                    $priorEnv = @{}
                    foreach ($envName in $envNames) {
                        $priorEnv[$envName] = [Environment]::GetEnvironmentVariable($envName, "Process")
                    }
                    try {
                        [Environment]::SetEnvironmentVariable("FACIAL_REPO_ROOT", $verifyFull, "Process")
                        [Environment]::SetEnvironmentVariable("FACIAL_CONFIG_PATH", (Join-Path $verifyFull "media-store-smoke-config.json"), "Process")
                        [Environment]::SetEnvironmentVariable("FACIAL_WORKSPACE_ROOT", $mediaRootA, "Process")
                        [Environment]::SetEnvironmentVariable("FACIAL_DATA_ROOT", (Join-Path $mediaRootA ".facial\data"), "Process")
                        [Environment]::SetEnvironmentVariable("FACIAL_WORKTREES_ROOT", (Join-Path $mediaRootA ".facial\worktrees"), "Process")

                        $mediaListOut = Join-Path $verifyFull "media-store-list.stdout.json"
                        $mediaListErr = Join-Path $verifyFull "media-store-list.stderr.txt"
                        $mediaList = Start-Process -FilePath $payloadCli -ArgumentList "media_meta_list" -Wait -PassThru -WindowStyle Hidden -RedirectStandardOutput $mediaListOut -RedirectStandardError $mediaListErr
                        $mediaListReceipt = $null
                        try {
                            $mediaListReceipt = Get-Content -Raw -LiteralPath $mediaListOut | ConvertFrom-Json -ErrorAction Stop
                        } catch {
                            $violations.Add("compiled setup CLI returned malformed fresh-media list JSON: $($_.Exception.Message)")
                        }
                        if ($mediaList.ExitCode -ne 0 -or $mediaListReceipt.status -ne "ok" -or $mediaListReceipt.result.count -ne 0) {
                            $stderr = if (Test-Path -LiteralPath $mediaListErr -PathType Leaf) { Get-Content -Raw -LiteralPath $mediaListErr } else { "" }
                            $violations.Add("compiled setup CLI did not open an empty fresh media store (exit $($mediaList.ExitCode), count $($mediaListReceipt.result.count)): $stderr")
                        }

                        $mediaFavOut = Join-Path $verifyFull "media-store-favorites.stdout.json"
                        $mediaFavErr = Join-Path $verifyFull "media-store-favorites.stderr.txt"
                        $mediaFav = Start-Process -FilePath $payloadCli -ArgumentList "media_fav_list" -Wait -PassThru -WindowStyle Hidden -RedirectStandardOutput $mediaFavOut -RedirectStandardError $mediaFavErr
                        $mediaFavReceipt = $null
                        try {
                            $mediaFavReceipt = Get-Content -Raw -LiteralPath $mediaFavOut | ConvertFrom-Json -ErrorAction Stop
                        } catch {
                            $violations.Add("compiled setup CLI returned malformed fresh-media favorites JSON: $($_.Exception.Message)")
                        }
                        if ($mediaFav.ExitCode -ne 0 -or $mediaFavReceipt.status -ne "ok" -or $mediaFavReceipt.result.count -ne 0) {
                            $stderr = if (Test-Path -LiteralPath $mediaFavErr -PathType Leaf) { Get-Content -Raw -LiteralPath $mediaFavErr } else { "" }
                            $violations.Add("compiled setup CLI imported a legacy favorite or failed the empty-favorites probe (exit $($mediaFav.ExitCode), count $($mediaFavReceipt.result.count)): $stderr")
                        }

                        $mediaStatusOut = Join-Path $verifyFull "media-store-status.stdout.json"
                        $mediaStatusErr = Join-Path $verifyFull "media-store-status.stderr.txt"
                        $mediaStatus = Start-Process -FilePath $payloadCli -ArgumentList "media_db_status" -Wait -PassThru -WindowStyle Hidden -RedirectStandardOutput $mediaStatusOut -RedirectStandardError $mediaStatusErr
                        $mediaStatusReceipt = $null
                        try {
                            $mediaStatusReceipt = Get-Content -Raw -LiteralPath $mediaStatusOut | ConvertFrom-Json -ErrorAction Stop
                        } catch {
                            $violations.Add("compiled setup CLI returned malformed fresh-media status JSON: $($_.Exception.Message)")
                        }
                        $freshStats = $mediaStatusReceipt.result.stats
                        if ($mediaStatus.ExitCode -ne 0 -or
                            $mediaStatusReceipt.status -ne "ok" -or
                            $mediaStatusReceipt.result.clean_user_state -ne $true -or
                            $freshStats.notes -ne 0 -or
                            $freshStats.tags -ne 0 -or
                            $freshStats.label_rows -ne 0 -or
                            $freshStats.label_assignments -ne 0 -or
                            $freshStats.favorites -ne 0 -or
                            $freshStats.settings_user -ne 0 -or
                            $freshStats.settings_internal -ne 2 -or
                            $freshStats.inventory_manifests -ne 0 -or
                            $freshStats.inventory_items -ne 0 -or
                            $freshStats.inventory_staging -ne 0 -or
                            $mediaStatusReceipt.result.clip_embeddings -ne 0) {
                            $stderr = if (Test-Path -LiteralPath $mediaStatusErr -PathType Leaf) { Get-Content -Raw -LiteralPath $mediaStatusErr } else { "" }
                            $violations.Add("compiled setup CLI fresh-media component counts are not clean (exit $($mediaStatus.ExitCode)): $stderr")
                        }

                        $mediaSetOut = Join-Path $verifyFull "media-store-set.stdout.json"
                        $mediaSetErr = Join-Path $verifyFull "media-store-set.stderr.txt"
                        $mediaSetArgs = 'media_meta_set --path "{0}" --notes "wp079-fresh-probe" --tags "fresh-only"' -f $probeA
                        $mediaSet = Start-Process -FilePath $payloadCli -ArgumentList $mediaSetArgs -Wait -PassThru -WindowStyle Hidden -RedirectStandardOutput $mediaSetOut -RedirectStandardError $mediaSetErr
                        $mediaSetReceipt = $null
                        try {
                            $mediaSetReceipt = Get-Content -Raw -LiteralPath $mediaSetOut | ConvertFrom-Json -ErrorAction Stop
                        } catch {
                            $violations.Add("compiled setup CLI returned malformed fresh-media write JSON: $($_.Exception.Message)")
                        }
                        if ($mediaSet.ExitCode -ne 0 -or $mediaSetReceipt.status -ne "ok" -or $mediaSetReceipt.result.notes -ne "wp079-fresh-probe" -or $mediaSetReceipt.result.tags -ne "fresh-only") {
                            $stderr = if (Test-Path -LiteralPath $mediaSetErr -PathType Leaf) { Get-Content -Raw -LiteralPath $mediaSetErr } else { "" }
                            $violations.Add("compiled setup CLI could not write the fresh media store (exit $($mediaSet.ExitCode)): $stderr")
                        }

                        $mediaGetOut = Join-Path $verifyFull "media-store-reopen.stdout.json"
                        $mediaGetErr = Join-Path $verifyFull "media-store-reopen.stderr.txt"
                        $mediaGetArgs = 'media_meta_get --path "{0}"' -f $probeA
                        $mediaGet = Start-Process -FilePath $payloadCli -ArgumentList $mediaGetArgs -Wait -PassThru -WindowStyle Hidden -RedirectStandardOutput $mediaGetOut -RedirectStandardError $mediaGetErr
                        $mediaGetReceipt = $null
                        try {
                            $mediaGetReceipt = Get-Content -Raw -LiteralPath $mediaGetOut | ConvertFrom-Json -ErrorAction Stop
                        } catch {
                            $violations.Add("compiled setup CLI returned malformed fresh-media reopen JSON: $($_.Exception.Message)")
                        }
                        if ($mediaGet.ExitCode -ne 0 -or $mediaGetReceipt.status -ne "ok" -or $mediaGetReceipt.result.notes -ne "wp079-fresh-probe" -or $mediaGetReceipt.result.tags -ne "fresh-only") {
                            $stderr = if (Test-Path -LiteralPath $mediaGetErr -PathType Leaf) { Get-Content -Raw -LiteralPath $mediaGetErr } else { "" }
                            $violations.Add("compiled setup CLI fresh-media value did not survive separate-process reopen (exit $($mediaGet.ExitCode)): $stderr")
                        }

                        $engineMarkerA = Join-Path $mediaStateA "engine.json"
                        $databaseRootA = Join-Path $mediaStateA "surrealdb"
                        $engineMarker = $null
                        try {
                            $engineMarker = Get-Content -Raw -LiteralPath $engineMarkerA | ConvertFrom-Json -ErrorAction Stop
                        } catch {
                            $violations.Add("fresh media engine marker is missing or malformed: $($_.Exception.Message)")
                        }
                        if (-not (Test-Path -LiteralPath $databaseRootA -PathType Container) -or
                            $engineMarker.engine -ne "surrealdb" -or
                            $engineMarker.engine_version -ne $surrealDbVersion -or
                            $engineMarker.namespace -ne "facial" -or
                            $engineMarker.database -ne "application" -or
                            $engineMarker.schema_version -ne 1) {
                            $violations.Add("compiled setup CLI did not create the expected schema-marked media SurrealDB root.")
                        }

                        Move-Item -LiteralPath $mediaRootA -Destination $mediaRootB
                        $probeB = Join-Path $mediaRootB "assets\probe.jpg"
                        [Environment]::SetEnvironmentVariable("FACIAL_WORKSPACE_ROOT", $mediaRootB, "Process")
                        [Environment]::SetEnvironmentVariable("FACIAL_DATA_ROOT", (Join-Path $mediaRootB ".facial\data"), "Process")
                        [Environment]::SetEnvironmentVariable("FACIAL_WORKTREES_ROOT", (Join-Path $mediaRootB ".facial\worktrees"), "Process")
                        $mediaRelocateOut = Join-Path $verifyFull "media-store-relocate.stdout.json"
                        $mediaRelocateErr = Join-Path $verifyFull "media-store-relocate.stderr.txt"
                        $mediaRelocateArgs = 'media_meta_get --path "{0}"' -f $probeB
                        $mediaRelocate = Start-Process -FilePath $payloadCli -ArgumentList $mediaRelocateArgs -Wait -PassThru -WindowStyle Hidden -RedirectStandardOutput $mediaRelocateOut -RedirectStandardError $mediaRelocateErr
                        $mediaRelocateReceipt = $null
                        try {
                            $mediaRelocateReceipt = Get-Content -Raw -LiteralPath $mediaRelocateOut | ConvertFrom-Json -ErrorAction Stop
                        } catch {
                            $violations.Add("compiled setup CLI returned malformed relocated-media JSON: $($_.Exception.Message)")
                        }
                        if ($mediaRelocate.ExitCode -ne 0 -or $mediaRelocateReceipt.status -ne "ok" -or $mediaRelocateReceipt.result.notes -ne "wp079-fresh-probe" -or $mediaRelocateReceipt.result.tags -ne "fresh-only") {
                            $stderr = if (Test-Path -LiteralPath $mediaRelocateErr -PathType Leaf) { Get-Content -Raw -LiteralPath $mediaRelocateErr } else { "" }
                            $violations.Add("compiled setup CLI fresh-media value did not survive workspace relocation (exit $($mediaRelocate.ExitCode)): $stderr")
                        }

                        $legacyAfterPaths = @(
                            (Join-Path $mediaRootB ".facial\media_metadata.json"),
                            (Join-Path $mediaRootB ".facial\media\media.redb"),
                            (Join-Path $mediaRootB ".facial\media\inventory.redb"),
                            (Join-Path $mediaRootB ".facial\media\clip_index.redb")
                        )
                        $legacyUnchanged = $true
                        foreach ($legacyPath in $legacyAfterPaths) {
                            $name = [IO.Path]::GetFileName($legacyPath)
                            if (-not (Test-Path -LiteralPath $legacyPath -PathType Leaf) -or (Get-FileHash -Algorithm SHA256 -LiteralPath $legacyPath).Hash -ne $legacyBefore[$name]) {
                                $legacyUnchanged = $false
                                $violations.Add("packaged media runtime changed or removed ignored legacy candidate '$name'.")
                            }
                        }
                        if (Test-Path -LiteralPath (Join-Path $mediaRootB ".facial\media_metadata.json.migrated")) {
                            $legacyUnchanged = $false
                            $violations.Add("packaged media runtime renamed ignored legacy JSON to .json.migrated.")
                        }
                        if ($mediaListReceipt.status -eq "ok" -and $mediaListReceipt.result.count -eq 0 -and
                            $mediaFavReceipt.status -eq "ok" -and $mediaFavReceipt.result.count -eq 0 -and
                            $mediaStatusReceipt.status -eq "ok" -and $mediaStatusReceipt.result.clean_user_state -eq $true -and
                            $mediaSetReceipt.status -eq "ok" -and $mediaGetReceipt.status -eq "ok" -and
                            $mediaRelocateReceipt.status -eq "ok" -and $legacyUnchanged) {
                            $surrealMediaSmokePassed = $true
                        }
                    } finally {
                        foreach ($envName in $envNames) {
                            [Environment]::SetEnvironmentVariable($envName, $priorEnv[$envName], "Process")
                        }
                    }
                }
                if (-not (Test-Path -LiteralPath $payloadRetirementTool -PathType Leaf)) {
                    $violations.Add("compiled setup did not export the WP-079 retirement tool.")
                } else {
                    $retirementToolExportedSha256 =
                        (Get-FileHash -LiteralPath $payloadRetirementTool -Algorithm SHA256).Hash.ToUpperInvariant()
                    if (-not (Test-FrozenRetirementToolHashSet -Hashes @($retirementToolExportedSha256))) {
                        $violations.Add("compiled setup exported WP-079 retirement tool SHA-256 must equal frozen live-audited digest $frozenRetirementToolSha256; observed $retirementToolExportedSha256.")
                    }
                }

                if (-not (Test-Path -LiteralPath $payloadReleaseDefaultConfig -PathType Leaf)) {
                    $violations.Add("compiled setup did not export release-default-config.json.")
                } elseif ($null -ne $releaseDefaultConfigSha256 -and
                          (Get-Sha256Lower -Path $payloadReleaseDefaultConfig) -cne $releaseDefaultConfigSha256) {
                    $violations.Add("compiled setup release-default-config.json differs from product/config/release-default.json ($releaseDefaultConfigContract).")
                } else {
                    $releaseDefaultConfig = $null
                    try {
                        $releaseDefaultConfig = Get-Content -Raw -LiteralPath $payloadReleaseDefaultConfig | ConvertFrom-Json -ErrorAction Stop
                    } catch {
                        $violations.Add("compiled setup release-default-config.json is malformed: $($_.Exception.Message)")
                    }
                    if ($null -ne $releaseDefaultConfig) {
                        $rootedConfigStrings = @(Find-RootedJsonStrings -Value $releaseDefaultConfig -JsonPath '$')
                        foreach ($rooted in $rootedConfigStrings) {
                            $violations.Add("compiled release default config contains a rooted user/machine path at $($rooted.path): '$($rooted.value)'.")
                        }
                        if ($rootedConfigStrings.Count -eq 0 -and $releaseDefaultConfigSourcePassed) {
                            $releaseDefaultConfigPassed = $true
                        }
                    }
                }

                $verifierReceipt = $null
                if (-not (Test-Path -LiteralPath $verifierReceiptPath -PathType Leaf)) {
                    $violations.Add("compiled setup did not export facialverify-receipt.json.")
                } else {
                    try {
                        $verifierReceipt = Get-Content -Raw -LiteralPath $verifierReceiptPath | ConvertFrom-Json -ErrorAction Stop
                    } catch {
                        $violations.Add("compiled setup FACIALVERIFY receipt is malformed JSON: $($_.Exception.Message)")
                    }
                }
                if ($null -ne $verifierReceipt) {
                    $expectedReceiptFields = @(
                        "default_data_all_children_delete_forbidden",
                        "default_data_ancestor_delete_forbidden",
                        "default_data_delete_targets",
                        "default_data_empty_directory_cleanup",
                        "default_data_reparse_points_forbidden",
                        "default_data_unknown_siblings_preserved",
                        "full_cleans_program_tree",
                        "full_deletes_user_data",
                        "installer_source_sha256",
                        "prior_uninstaller_arguments",
                        "prior_uninstaller_cleanup_check",
                        "prior_uninstaller_cleanup_order",
                        "prior_uninstaller_data_dir_hold",
                        "prior_uninstaller_data_dir_postcondition",
                        "prior_uninstaller_data_dir_restore",
                        "prior_uninstaller_exact_executable",
                        "prior_uninstaller_exit_required",
                        "prior_uninstaller_hkcu_execution",
                        "prior_uninstaller_hkcu_trust_boundary",
                        "prior_uninstaller_hklm_execution",
                        "prior_uninstaller_hklm_trust_boundary",
                        "prior_uninstaller_registry_provenance",
                        "release_default_config_name",
                        "release_default_config_sha256",
                        "relocated_delete_target",
                        "relocated_reparse_points_forbidden",
                        "retirement_archive_preserved",
                        "retirement_test_script_packaged",
                        "retirement_tool_packaged",
                        "retirement_tool_sha256",
                        "soft_cleans_program_tree",
                        "soft_deletes_user_data",
                        "update_cleans_program_tree",
                        "update_deletes_relocated_state",
                        "update_deletes_user_data",
                        "verifier_destination_contract",
                        "verifier_exit_code",
                        "verifier_overwrite_policy"
                    ) | Sort-Object
                    $observedReceiptFields = @($verifierReceipt.PSObject.Properties.Name | Sort-Object)
                    $receiptShapeMatches = (($observedReceiptFields -join "`n") -ceq ($expectedReceiptFields -join "`n"))
                    $receiptTypesMatch =
                        (($verifierReceipt.verifier_exit_code -is [int]) -or
                         ($verifierReceipt.verifier_exit_code -is [long])) -and
                        (($verifierReceipt.prior_uninstaller_exit_required -is [int]) -or
                         ($verifierReceipt.prior_uninstaller_exit_required -is [long])) -and
                        ($verifierReceipt.update_cleans_program_tree -is [bool]) -and
                        ($verifierReceipt.update_deletes_user_data -is [bool]) -and
                        ($verifierReceipt.update_deletes_relocated_state -is [bool]) -and
                        ($verifierReceipt.soft_cleans_program_tree -is [bool]) -and
                        ($verifierReceipt.soft_deletes_user_data -is [bool]) -and
                        ($verifierReceipt.full_cleans_program_tree -is [bool]) -and
                        ($verifierReceipt.full_deletes_user_data -is [bool]) -and
                        ($verifierReceipt.default_data_unknown_siblings_preserved -is [bool]) -and
                        ($verifierReceipt.default_data_ancestor_delete_forbidden -is [bool]) -and
                        ($verifierReceipt.default_data_all_children_delete_forbidden -is [bool]) -and
                        ($verifierReceipt.default_data_reparse_points_forbidden -is [bool]) -and
                        ($verifierReceipt.relocated_reparse_points_forbidden -is [bool]) -and
                        ($verifierReceipt.retirement_tool_packaged -is [bool]) -and
                        ($verifierReceipt.retirement_tool_sha256 -is [string]) -and
                        ($verifierReceipt.retirement_test_script_packaged -is [bool]) -and
                        ($verifierReceipt.retirement_archive_preserved -is [bool])
                    $receiptValuesMatch =
                        ([string]$verifierReceipt.installer_source_sha256 -ceq $installerSourceSha256) -and
                        ([int]$verifierReceipt.verifier_exit_code -eq $expectedVerifierExitCode) -and
                        ([string]$verifierReceipt.release_default_config_name -ceq "release-default-config.json") -and
                        ([string]$verifierReceipt.release_default_config_sha256 -ceq $releaseDefaultConfigSha256) -and
                        ($verifierReceipt.update_cleans_program_tree -eq $false) -and
                        ($verifierReceipt.update_deletes_user_data -eq $false) -and
                        ($verifierReceipt.update_deletes_relocated_state -eq $false) -and
                        ($verifierReceipt.soft_cleans_program_tree -eq $true) -and
                        ($verifierReceipt.soft_deletes_user_data -eq $false) -and
                        ($verifierReceipt.full_cleans_program_tree -eq $true) -and
                        ($verifierReceipt.full_deletes_user_data -eq $true) -and
                        ([string]$verifierReceipt.relocated_delete_target -ceq ".facial") -and
                        ($verifierReceipt.relocated_reparse_points_forbidden -eq $true) -and
                        ((@($verifierReceipt.default_data_delete_targets) -join "`n") -ceq (@(".facial", "config\default.json") -join "`n")) -and
                        ((@($verifierReceipt.default_data_empty_directory_cleanup) -join "`n") -ceq (@("config", "data-root") -join "`n")) -and
                        ($verifierReceipt.default_data_unknown_siblings_preserved -eq $true) -and
                        ($verifierReceipt.default_data_ancestor_delete_forbidden -eq $true) -and
                        ($verifierReceipt.default_data_all_children_delete_forbidden -eq $true) -and
                        ($verifierReceipt.default_data_reparse_points_forbidden -eq $true) -and
                        ($verifierReceipt.retirement_tool_packaged -eq $true) -and
                        ([string]$verifierReceipt.retirement_tool_sha256).ToUpperInvariant() -ceq $frozenRetirementToolSha256 -and
                        ($verifierReceipt.retirement_test_script_packaged -eq $false) -and
                        ($verifierReceipt.retirement_archive_preserved -eq $true) -and
                        ([string]$verifierReceipt.verifier_destination_contract -ceq "temp-root\facial-installer-verify-<32-hex>") -and
                        ([string]$verifierReceipt.verifier_overwrite_policy -ceq "refuse-all-export-targets") -and
                        ([string]$verifierReceipt.prior_uninstaller_arguments -ceq "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /REDIRECTIONGUARD") -and
                        ([int]$verifierReceipt.prior_uninstaller_exit_required -eq 0) -and
                        ([string]$verifierReceipt.prior_uninstaller_cleanup_check -ceq "registry-and-shortcuts") -and
                        ([string]$verifierReceipt.prior_uninstaller_registry_provenance -ceq "same-root-key-view") -and
                        ([string]$verifierReceipt.prior_uninstaller_exact_executable -ceq "InstallLocation\unins000.exe") -and
                        ([string]$verifierReceipt.prior_uninstaller_hkcu_execution -ceq "ExecAsOriginalUser") -and
                        ([string]$verifierReceipt.prior_uninstaller_hkcu_trust_boundary -ceq "LocalAppData") -and
                        ([string]$verifierReceipt.prior_uninstaller_hklm_execution -ceq "Exec") -and
                        ([string]$verifierReceipt.prior_uninstaller_hklm_trust_boundary -ceq "ProgramFiles") -and
                        ([string]$verifierReceipt.prior_uninstaller_data_dir_hold -ceq "Facial.wp079-data-dir-hold") -and
                        ([string]$verifierReceipt.prior_uninstaller_data_dir_restore -ceq "finally-before-exact-cleanup") -and
                        ([string]$verifierReceipt.prior_uninstaller_data_dir_postcondition -ceq "source-present-hold-absent") -and
                        ([string]$verifierReceipt.prior_uninstaller_cleanup_order -ceq "restore-then-exact-app-owned-targets")
                    if (-not $receiptShapeMatches -or -not $receiptTypesMatch -or -not $receiptValuesMatch) {
                        $violations.Add("compiled setup FACIALVERIFY receipt does not match the exact source/config/mode/predecessor safety contract.")
                    } else {
                        $installerVerifierReceiptPassed = $true
                        $predecessorReceiptContractPassed = $true
                    }
                }
                if ($null -ne $verifierReceipt -and
                    $null -ne $retirementToolSourceSha256 -and
                    $null -ne $retirementToolExportedSha256) {
                    $retirementToolPayloadSha256 =
                        ([string]$verifierReceipt.retirement_tool_sha256).ToUpperInvariant()
                    $retirementToolHashContractPassed =
                        Test-FrozenRetirementToolHashSet -Hashes @(
                            $retirementToolSourceSha256,
                            $retirementToolPayloadSha256,
                            $retirementToolExportedSha256
                        )
                }
                if (-not $retirementToolHashContractPassed) {
                    $violations.Add("WP-079 retirement-tool source, compiled payload receipt, and FACIALVERIFY export must all equal frozen live-audited SHA-256 $frozenRetirementToolSha256.")
                }
                $expectedUpdateContract = @(
                    "update_cleans_program_tree=false",
                    "update_deletes_user_data=false",
                    "update_deletes_relocated_state=false",
                    "soft_cleans_program_tree=true",
                    "soft_deletes_user_data=false",
                    "full_cleans_program_tree=true",
                    "full_deletes_user_data=true",
                    "relocated_delete_target=.facial",
                    "relocated_reparse_points_forbidden=true",
                    "default_data_delete_targets=.facial|config\default.json",
                    "default_data_empty_directory_cleanup=config|data-root",
                    "default_data_unknown_siblings_preserved=true",
                    "default_data_ancestor_delete_forbidden=true",
                    "default_data_all_children_delete_forbidden=true",
                    "default_data_reparse_points_forbidden=true",
                    "retirement_tool_packaged=true",
                    "retirement_tool_sha256=$([string]$verifierReceipt.retirement_tool_sha256)",
                    "retirement_test_script_packaged=false",
                    "retirement_archive_preserved=true",
                    "verifier_destination_contract=temp-root\facial-installer-verify-<32-hex>",
                    "verifier_overwrite_policy=refuse-all-export-targets"
                ) -join "`r`n"
                if (-not (Test-Path -LiteralPath $updateContractPath -PathType Leaf)) {
                    $violations.Add("compiled setup did not export its update-mode preservation contract.")
                } else {
                    $observedUpdateContract = (Get-Content -Raw -LiteralPath $updateContractPath).TrimEnd("`r", "`n")
                    if ($observedUpdateContract -cne $expectedUpdateContract) {
                        $violations.Add("compiled setup update-mode preservation contract is incorrect.")
                    } else {
                        $updateModeContractPassed = $true
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
    foreach ($sectionName in @("Setup", "Files", "Icons", "Run", "InstallDelete", "Code")) {
        $escapedSectionName = [regex]::Escape($sectionName)
        $sectionCount = [regex]::Matches(
            $issRaw,
            "(?im)^\s*\[$escapedSectionName\]\s*$"
        ).Count
        if ($sectionCount -ne 1) {
            $violations.Add("installer must contain exactly one [$sectionName] section; found $sectionCount.")
        }
    }
    $setupSection = Get-InnoSection -Raw $issRaw -Name "Setup"
    $filesSection = Get-InnoSection -Raw $issRaw -Name "Files"
    $iconsSection = Get-InnoSection -Raw $issRaw -Name "Icons"
    $runSection = Get-InnoSection -Raw $issRaw -Name "Run"
    $deleteSection = Get-InnoSection -Raw $issRaw -Name "InstallDelete"
    $codeSection = Get-InnoSection -Raw $issRaw -Name "Code"
    if ($setupSection -notmatch '(?im)^\s*RedirectionGuard\s*=\s*yes\s*$') {
        $violations.Add("installer must explicitly enable Inno Setup RedirectionGuard for elevated user-state cleanup.")
    }
    # ISPP can generate executable installer entries through #include aliases,
    # #emit, and other directives that are absent from the raw sections parsed
    # below. Require the complete, case-sensitive directive sequence used by
    # this project rather than attempting a fragile blacklist.
    if (-not (Test-InnoPreprocessorDirectiveAllowlist -Raw $issRaw)) {
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
    if ($filesSection -notmatch 'Source:\s*"\{#PayloadDir\}\\product\\scripts\\retire-legacy-media-db\.ps1";\s*DestName:\s*"_verify-retire-legacy-media-db\.ps1";\s*Flags:\s*dontcopy\s+noencryption') {
        $violations.Add("installer [Files] does not embed the exact WP-079 retirement tool for compiled-payload verification.")
    }
    $retirementTestHarnessMentions = @([regex]::Matches($filesSection, '(?i)test-retire-legacy-media-db\.ps1')).Count
    if ($filesSection -notmatch 'Source:\s*"\{#PayloadDir\}\\product\\\*";\s*DestDir:\s*"\{app\}\\product";\s*Excludes:\s*"scripts\\test-retire-legacy-media-db\.ps1";\s*Flags:\s*ignoreversion\s+recursesubdirs\s+createallsubdirs' -or
        $filesSection -match '(?im)^\s*Source:\s*"[^"]*test-retire-legacy-media-db\.ps1"' -or
        $retirementTestHarnessMentions -ne 1) {
        $violations.Add("installer [Files] must explicitly exclude the WP-079 retirement test harness from the shipped product tree.")
    }
    if ($filesSection -notmatch 'Source:\s*"\{#PayloadDir\}\\product\\config\\default\.json";\s*DestName:\s*"_verify-release-default-config\.json";\s*Flags:\s*dontcopy\s+noencryption') {
        $violations.Add("installer [Files] does not embed the exact compiled release default config for verification.")
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
    if ($codeSection -match '(?i)DelTree\s*\(\s*(?:ws|stateDir|RelocatedStateDir)\s*,') {
        $violations.Add("installer may not recursively delete the configured workspace root.")
    }
    $relocatedCleanupBlockMatch = [regex]::Match(
        $codeSection,
        '(?s)function\s+CachedRelocatedWorkspaceStateDir\s*\(\s*\).*?procedure\s+MaybeDeleteRelocatedWorkspaceState\s*\(\s*\).*?end\s*;\s*(?=procedure\s+InitializeWizard\s*\()'
    )
    $relocatedDeleteCalls = @([regex]::Matches(
        $codeSection,
        '(?i)DelTree\s*\(\s*NormalizedStateDir\s*,\s*True\s*,\s*True\s*,\s*True\s*\)'
    )).Count
    $relocatedCleanupDelTreeCalls = if ($relocatedCleanupBlockMatch.Success) {
        @([regex]::Matches($relocatedCleanupBlockMatch.Value, '(?i)\bDelTree\s*\(')).Count
    } else { 0 }
    if ($codeSection -notmatch '(?s)function\s+CachedRelocatedWorkspaceStateDir\s*\(\s*\).*?Result\s*:=\s*AddBackslash\(ws\)\s*\+\s*''\.facial''' -or
        $codeSection -notmatch '(?s)function\s+ConfirmCachedRelocatedWorkspaceStateDeletion\s*\(\s*var\s+StateDir:\s*String\s*\).*?StateDir\s*:=\s*CachedRelocatedWorkspaceStateDir\(\)' -or
        $codeSection -notmatch '(?s)procedure\s+DeleteRelocatedWorkspaceStateChecked\s*\(\s*StateDir:\s*String\s*\).*?ExtractFileName\(NormalizedStateDir\).*?''\.facial''.*?if\s+not\s+DelTree\(NormalizedStateDir,\s*True,\s*True,\s*True\)\s+then\s+RaiseException.*?if\s+AnyPathExists\(NormalizedStateDir\)\s+then\s+RaiseException' -or
        $codeSection -notmatch '(?s)procedure\s+MaybeDeleteRelocatedWorkspaceState\s*\(\s*\).*?if\s+ConfirmCachedRelocatedWorkspaceStateDeletion\(StateDir\)\s+then\s+DeleteRelocatedWorkspaceStateChecked\(StateDir\)' -or
        -not $relocatedCleanupBlockMatch.Success -or
        $relocatedDeleteCalls -ne 1 -or
        $relocatedCleanupDelTreeCalls -ne 1) {
        $violations.Add("installer relocated cleanup must cache an exact .facial target, contain exactly one checked DelTree, and prove the postcondition; observed $relocatedDeleteCalls checked target calls and $relocatedCleanupDelTreeCalls total calls in the relocated-cleanup block.")
    }
    if ($codeSection -notmatch 'Raw media and all files outside \.facial will be kept\.') {
        $violations.Add("installer relocated-state confirmation does not explicitly promise raw-media preservation.")
    }
    if ($codeSection -notmatch 'Recovery archives in \.facial-media-retirement will be kept\.' -or
        $codeSection -match '(?i)DelTree\s*\([^\r\n;]*\.facial-media-retirement') {
        $violations.Add("installer must explicitly preserve the WP-079 recovery archive outside .facial cleanup.")
    }
    if ($codeSection -match '(?i)DelTree\s*\(\s*(?:DataDir\s*\(\s*\)|DataRoot|ConfigDir|ChildPath)\s*,' -or
        $codeSection -match '(?s)FindFirst\s*\(\s*AddBackslash\(DataDir\(\)\)\s*\+\s*''\*''.*?DelTree') {
        $violations.Add("installer may not delete the default data-root ancestor or enumerate/delete all of its children.")
    }
    $exactDataCleanupCalls = @([regex]::Matches(
        $codeSection,
        '(?im)^\s*DeleteDefaultAppOwnedDataChecked\s*\(\s*\)\s*;\s*$'
    )).Count
    $exactDataCleanupBlock = [regex]::Match(
        $codeSection,
        '(?s)procedure\s+DeleteDefaultAppOwnedDataChecked\s*\(\s*\).*?end\s*;\s*(?=function\s+DefaultDataDirHoldPath\s*\()'
    )
    $exactDataCleanupDelTreeCalls = if ($exactDataCleanupBlock.Success) {
        @([regex]::Matches($exactDataCleanupBlock.Value, '(?i)\bDelTree\s*\(')).Count
    } else { 0 }
    $exactDataCleanupDeleteFileCalls = if ($exactDataCleanupBlock.Success) {
        @([regex]::Matches($exactDataCleanupBlock.Value, '(?i)\bDeleteFile\s*\(')).Count
    } else { 0 }
    $emptyDirectoryCleanupBlock = [regex]::Match(
        $codeSection,
        '(?s)function\s+DirectoryHasChildren\s*\(.*?procedure\s+RemoveDirectoryIfEmptyChecked\s*\(.*?end\s*;\s*(?=\{ Full and uninstall)'
    )
    if (-not $exactDataCleanupBlock.Success -or
        $exactDataCleanupDelTreeCalls -ne 1 -or
        $exactDataCleanupDeleteFileCalls -ne 1 -or
        $exactDataCleanupBlock.Value -notmatch '(?s)ManagedStateDir\s*:=.*?AddBackslash\(DataRoot\)\s*\+\s*''\.facial''.*?CompareText\(ManagedStateDir,\s*AddBackslash\(DataRoot\)\s*\+\s*''\.facial''\).*?ManagedStateWasPresent\s*:=\s*AnyPathExists\(ManagedStateDir\).*?if\s+ManagedStateWasPresent\s+and\s+\(not\s+DirExists\(ManagedStateDir\)\).*?All target paths, types, and ancestor attributes are proven before the.*?if\s+ManagedStateWasPresent\s+then\s+begin\s+if\s+not\s+DirExists\(ManagedStateDir\)\s+then\s+RaiseException.*?if\s+not\s+DelTree\(ManagedStateDir,\s*True,\s*True,\s*True\)\s+then\s+RaiseException.*?if\s+AnyPathExists\(ManagedStateDir\)\s+then\s+RaiseException' -or
        $exactDataCleanupBlock.Value -notmatch '(?s)ConfigFile\s*:=.*?AddBackslash\(ConfigDir\)\s*\+\s*''default\.json''.*?CompareText\(ConfigFile,\s*AddBackslash\(DataRoot\)\s*\+\s*''config\\default\.json''\).*?ConfigDirWasPresent\s*:=\s*AnyPathExists\(ConfigDir\).*?if\s+ConfigDirWasPresent\s+and\s+\(not\s+DirExists\(ConfigDir\)\).*?ConfigFileWasPresent\s*:=\s*AnyPathExists\(ConfigFile\).*?if\s+ConfigFileWasPresent\s+and\s+\(not\s+FileExists\(ConfigFile\)\).*?All target paths, types, and ancestor attributes are proven before the.*?if\s+ConfigFileWasPresent\s+then\s+begin\s+if\s+not\s+FileExists\(ConfigFile\)\s+then\s+RaiseException.*?if\s+not\s+DeleteFile\(ConfigFile\)\s+then\s+RaiseException.*?if\s+AnyPathExists\(ConfigFile\)\s+then\s+RaiseException' -or
        $exactDataCleanupBlock.Value -notmatch '(?s)DataRootWasPresent\s*:=\s*AnyPathExists\(DataRoot\).*?if\s+DataRootWasPresent\s+and\s+\(not\s+DirExists\(DataRoot\)\).*?if\s+ConfigDirWasPresent\s+then\s+RemoveDirectoryIfEmptyChecked\(ConfigDir,\s*''Facial config directory''\).*?if\s+DataRootWasPresent\s+then\s+RemoveDirectoryIfEmptyChecked\(DataRoot,\s*''Facial data root''\)' -or
        -not $emptyDirectoryCleanupBlock.Success -or
        $emptyDirectoryCleanupBlock.Value -notmatch '(?s)if\s+DirectoryHasChildren\(Path\)\s+then\s+exit.*?if\s+not\s+RemoveDir\(Path\)\s+then\s+RaiseException.*?if\s+AnyPathExists\(Path\)\s+then\s+RaiseException' -or
        $exactDataCleanupCalls -ne 4 -or
        -not $defaultDataReparseSourceContractPassed -or
        -not $defaultDataReparseCounterfactualPassed) {
        $violations.Add("Full/fallback/current-uninstall/registered-predecessor cleanup must call one exact-target routine that deletes only DataDir\\.facial and DataDir\\config\\default.json, preserves unknown siblings, and removes config/root only when empty; observed $exactDataCleanupCalls calls, $exactDataCleanupDelTreeCalls DelTree calls, and $exactDataCleanupDeleteFileCalls DeleteFile calls in the routine.")
    }
    $programTreeCleanupBlock = [regex]::Match(
        $codeSection,
        '(?s)procedure\s+CurStepChanged\s*\(.*?programTree\s*:=\s*ExpandConstant\(''\{app\}\\product''\)\s*;.*?if\s+ModeCleansProgramTree\(mode\)\s+and\s+DirExists\(programTree\)\s+then\s+begin.*?if\s+not\s+DelTree\(programTree,\s*True,\s*True,\s*True\)\s+then\s+RaiseException.*?if\s+DirExists\(programTree\)\s+then\s+RaiseException.*?end\s*;'
    )
    $programTreeDeleteCalls = @([regex]::Matches(
        $codeSection,
        '(?i)DelTree\s*\(\s*programTree\s*,\s*True\s*,\s*True\s*,\s*True\s*\)'
    )).Count
    if (-not $programTreeCleanupBlock.Success -or
        $programTreeDeleteCalls -ne 1 -or
        $codeSection -notmatch '(?s)if\s+ModeDeletesUserData\(mode\)\s+then.*?if\s+ModeDeletesRelocatedState\(mode\)\s+then\s+MaybeDeleteRelocatedWorkspaceState\(\)') {
        $violations.Add("installer mutation branches must use compiled update/soft/full predicates and exactly one fail-closed program-tree DelTree with an absence postcondition; observed $programTreeDeleteCalls program-tree calls.")
    }

    $predecessorMachineExecCalls = @([regex]::Matches($codeSection, '(?i)\bExec\s*\(')).Count
    $predecessorUserExecCalls = @([regex]::Matches($codeSection, '(?i)\bExecAsOriginalUser\s*\(')).Count
    $installerExternalLaunchCalls = @([regex]::Matches($codeSection, '(?i)\b(?:Exec|ExecAsOriginalUser|ShellExec)\s*\(')).Count
    $priorInstallLocationQueries = @([regex]::Matches($codeSection, "(?i)RegQueryStringValue\s*\(\s*RootKey\s*,\s*UninstKey\s*,\s*'InstallLocation'")).Count
    $priorUninstallStringQueries = @([regex]::Matches($codeSection, "(?i)RegQueryStringValue\s*\(\s*RootKey\s*,\s*UninstKey\s*,\s*'UninstallString'")).Count
    $predecessorSourceChecks = @(
        ($codeSection -match '(?im)^\s*DataDirHoldName\s*=\s*''Facial\.wp079-data-dir-hold''\s*;\s*$'),
        ($codeSection -match '(?im)^\s*PriorUninstallArguments\s*=\s*''/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /REDIRECTIONGUARD''\s*;\s*$'),
        ($codeSection -match '(?im)^\s*PriorUninstallCleanupAttempts\s*=\s*100\s*;\s*$'),
        ($codeSection -match '(?im)^\s*PriorUninstallCleanupDelayMs\s*=\s*100\s*;\s*$'),
        ($codeSection -match '(?s)function\s+DefaultDataDirHoldPath\s*\(\s*\).*?Result\s*:=\s*AddBackslash\(ExpandConstant\(''\{localappdata\}''\)\)\s*\+\s*DataDirHoldName'),
        ($codeSection -match '(?s)procedure\s+HoldDefaultDataDir\s*\(.*?if\s+AnyPathExists\(HoldPath\)\s+then\s+RaiseException.*?if\s+not\s+DirExists\(DataDir\(\)\)\s+then\s+RaiseException.*?if\s+not\s+RenameFile\(DataDir\(\),\s*HoldPath\)\s+then\s+RaiseException.*?DataDirHeld\s*:=\s*True.*?AnyPathExists\(DataDir\(\)\).*?not\s+DirExists\(HoldPath\)'),
        ($codeSection -match '(?s)function\s+RestoreDefaultDataDir\s*\(.*?if\s+not\s+DirExists\(HoldPath\)\s+then.*?Result\s*:=\s*False.*?if\s+AnyPathExists\(DataDir\(\)\)\s+then.*?HoldPath.*?Result\s*:=\s*False.*?if\s+not\s+RenameFile\(HoldPath,\s*DataDir\(\)\)\s+then.*?HoldPath.*?Result\s*:=\s*False.*?not\s+DirExists\(DataDir\(\)\).*?AnyPathExists\(HoldPath\)'),
        ($codeSection -match '(?s)procedure\s+AssertDefaultDataDirPostconditions\s*\(.*?if\s+AnyPathExists\(HoldPath\)\s+then\s+RaiseException.*?DataDirWasHeld\s+and\s*\(not\s+DirExists\(DataDir\(\)\)\)\s+then\s+RaiseException.*?HoldPath'),
        ($codeSection -match '(?s)function\s+NormalizeRegisteredPath\s*\(.*?Trim\(Value\).*?malformed quoting.*?embedded quoting or arguments.*?path-escape segment.*?not an absolute drive path.*?ExpandFileName\(Candidate\)'),
        ($codeSection -match '(?s)function\s+ResolvePriorFromExactView\s*\(\s*RootKey:\s*Integer;\s*CurrentUserOrigin:\s*Boolean;.*?RegKeyExists\(RootKey,\s*UninstKey\).*?RegQueryStringValue\(RootKey,\s*UninstKey,\s*''InstallLocation''.*?RegQueryStringValue\(RootKey,\s*UninstKey,\s*''UninstallString''.*?AddBackslash\(InstallLocation\)\s*\+\s*''unins000\.exe''.*?ExtractFileName\(Uninstaller\).*?''unins000\.exe''.*?CompareText\(Uninstaller,\s*ExpectedUninstaller\).*?CurrentUserOrigin.*?IsPathBelow\(InstallLocation,\s*ExpandConstant\(''\{localappdata\}''\)\).*?IsTrustedMachineInstallLocation\(InstallLocation\).*?if\s+not\s+FileExists\(Uninstaller\)\s+then\s+RaiseException'),
        ($codeSection -match '(?s)function\s+IsTrustedMachineInstallLocation\s*\(.*?\{commonpf32\}.*?IsWin64.*?\{commonpf64\}'),
        ($codeSection -match '(?s)function\s+ResolvePriorUninstaller\s*\(.*?RegKeyExists\(HKLM64,\s*UninstKey\).*?ResolvePriorFromExactView\(HKLM64,\s*False,\s*''HKLM64''.*?RegKeyExists\(HKLM32,\s*UninstKey\).*?ResolvePriorFromExactView\(HKLM32,\s*False,\s*''HKLM32''.*?RegKeyExists\(HKCU64,\s*UninstKey\).*?CurrentUserOrigin\s*:=\s*True.*?ResolvePriorFromExactView\(HKCU64,\s*True,\s*''HKCU64''.*?RegKeyExists\(HKCU32,\s*UninstKey\).*?CurrentUserOrigin\s*:=\s*True.*?ResolvePriorFromExactView\(HKCU32,\s*True,\s*''HKCU32'''),
        ($codeSection -match '(?s)function\s+PriorUninstallRegistrationExists\s*\(\s*\).*?RegKeyExists\(HKLM32,\s*UninstKey\).*?RegKeyExists\(HKCU32,\s*UninstKey\).*?RegKeyExists\(HKLM64,\s*UninstKey\).*?RegKeyExists\(HKCU64,\s*UninstKey\)'),
        ($codeSection -match '(?s)function\s+PriorShortcutExists\s*\(\s*\).*?\{commonprograms\}\\Facial\\Facial\.lnk.*?\{commonprograms\}\\Facial\\Uninstall Facial\.lnk.*?\{userprograms\}\\Facial\\Facial\.lnk.*?\{userprograms\}\\Facial\\Uninstall Facial\.lnk.*?\{commondesktop\}\\Facial\.lnk.*?\{userdesktop\}\\Facial\.lnk'),
        ($codeSection -match '(?s)function\s+WaitForPriorUninstallCleanup\s*\(\s*\).*?not\s+PriorUninstallRegistrationExists\(\).*?not\s+PriorShortcutExists\(\).*?Sleep\(PriorUninstallCleanupDelayMs\).*?Result\s*:=\s*\(not\s+PriorUninstallRegistrationExists\(\)\)\s+and\s*\(not\s+PriorShortcutExists\(\)\)'),
        ($codeSection -match '(?s)procedure\s+RunPriorUninstallerSafely\s*\(\s*Uninstaller:\s*String;\s*CurrentUserOrigin:\s*Boolean\s*\).*?ConfirmCachedRelocatedWorkspaceStateDeletion\(RelocatedStateDir\).*?try\s+try\s+HoldDefaultDataDir\(DataDirHeld,\s*HoldPath\).*?if\s+not\s+FileExists\(Uninstaller\)\s+then\s+RaiseException.*?if\s+CurrentUserOrigin\s+then\s+begin\s+if\s+not\s+ExecAsOriginalUser\(Uninstaller,\s*PriorUninstallArguments,\s*'''',\s*SW_HIDE,\s*ewWaitUntilTerminated,\s*ExitCode\).*?else\s+begin\s+if\s+not\s+Exec\(Uninstaller,\s*PriorUninstallArguments,\s*'''',\s*SW_HIDE,\s*ewWaitUntilTerminated,\s*ExitCode\).*?if\s+ExitCode\s*<>\s*0\s+then\s+RaiseException.*?if\s+not\s+WaitForPriorUninstallCleanup\(\)\s+then\s+RaiseException.*?finally\s+if\s+not\s+RestoreDefaultDataDir\(DataDirHeld,\s*HoldPath,\s*RestoreError\).*?AssertDefaultDataDirPostconditions\(DataDirHeld,\s*HoldPath\).*?if\s+DeleteRelocatedState\s+then\s+DeleteRelocatedWorkspaceStateChecked\(RelocatedStateDir\).*?DeleteDefaultAppOwnedDataChecked\(\)'),
        ($codeSection -match '(?s)function\s+PrepareToInstall\s*\(.*?if\s+SelectedMode\(\)\s*=\s*MODE_UNINST\s+then.*?priorFound\s*:=\s*ResolvePriorUninstaller\(unins,\s*currentUserOrigin\).*?if\s+priorFound\s+then\s+RunPriorUninstallerSafely\(unins,\s*currentUserOrigin\)'),
        ($predecessorMachineExecCalls -eq 1),
        ($predecessorUserExecCalls -eq 1),
        ($installerExternalLaunchCalls -eq 2),
        ($priorInstallLocationQueries -eq 1),
        ($priorUninstallStringQueries -eq 1)
    )
    if ($predecessorSourceChecks -notcontains $false) {
        $predecessorSourceContractPassed = $true
    } else {
        $violations.Add("installer predecessor-uninstall source contract lacks same-key/view registry provenance, exact trusted InstallLocation\\unins000.exe validation, source-specific execution, whole-DataDir hold/finally restore, or restore-before-exact-cleanup sequencing; observed $predecessorMachineExecCalls machine Exec calls, $predecessorUserExecCalls original-user calls, and $installerExternalLaunchCalls total external-launch calls.")
    }
}

$predecessorSafetyContractPassed = $predecessorSourceContractPassed -and $predecessorReceiptContractPassed

# Every repository EXE outside transient build scratch must be either one of the
# two current root artifacts or a file in the one delivery archive.
$allowedRootPaths = @{}
foreach ($rootExe in $rootExes) {
    $allowedRootPaths[[IO.Path]::GetFullPath($rootExe.FullName)] = $true
}
$allExes = Get-ExeFilesForce -Path $repoRoot -Recurse |
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

$archiveCount = @(Get-ExeFilesForce -Path $archiveDir).Count
if (-not $Quiet) {
    Write-Host "cargo-version=$version"
    Write-Host "surrealdb-embedded-version=$surrealDbVersion"
    Write-Host "surrealdb-installer-smoke=$surrealDbSmokePassed"
    Write-Host "surrealdb-media-installer-smoke=$surrealMediaSmokePassed"
    Write-Host "installer-update-preservation-contract=$updateModeContractPassed"
    Write-Host "installer-verifier-source-contract=$installerVerifierSourceContractPassed"
    Write-Host "installer-verifier-callgraph-counterfactuals=$installerVerifierCallGraphCounterfactualsPassed"
    Write-Host "installer-verifier-preprocessor-counterfactual=$installerPreprocessorCounterfactualPassed"
    Write-Host "installer-normalized-source-counterfactual=$installerNormalizedSourceCounterfactualPassed"
    Write-Host "installer-source-pe-binding=$installerSourceBindingPassed"
    Write-Host "installer-payload-portable-sha256-match=$installerPayloadPortableMatchPassed"
    Write-Host "installer-sha256-counterfactual=$sha256PairCounterfactualPassed"
    Write-Host "installer-verifier-receipt=$installerVerifierReceiptPassed"
    Write-Host "installer-verifier-no-side-effects=$installerVerifierNoSideEffectsPassed"
    Write-Host "installer-verifier-output-boundary=$installerVerifierOutputBoundaryPassed"
    Write-Host "installer-release-default-config=$releaseDefaultConfigPassed"
    Write-Host "installer-predecessor-source-contract=$predecessorSourceContractPassed"
    Write-Host "installer-predecessor-receipt-contract=$predecessorReceiptContractPassed"
    Write-Host "installer-predecessor-safety-contract=$predecessorSafetyContractPassed"
    Write-Host "installer-default-data-reparse-source-contract=$defaultDataReparseSourceContractPassed"
    Write-Host "installer-default-data-reparse-counterfactual=$defaultDataReparseCounterfactualPassed"
    Write-Host "installer-forced-exe-enumeration=$forcedExeEnumerationPassed"
    Write-Host "package-release-source-contract=$packageReleaseSourceContractPassed"
    Write-Host "package-release-force-counterfactual=$packageReleaseForceCounterfactualPassed"
    Write-Host "retirement-tool-source-pinned=$retirementToolSourcePinnedPassed"
    Write-Host "retirement-tool-frozen-hash-contract=$retirementToolHashContractPassed"
    Write-Host "retirement-tool-mutated-lineage-counterfactual=$retirementToolHashCounterfactualPassed"
    Write-Host "retirement-tool-frozen-sha256=$frozenRetirementToolSha256"
    Write-Host "retirement-tool-source-sha256=$retirementToolSourceSha256"
    Write-Host "retirement-tool-payload-sha256=$retirementToolPayloadSha256"
    Write-Host "retirement-tool-exported-sha256=$retirementToolExportedSha256"
    Write-Host "registry-fingerprint-read-contract-32-and-64=$registryFingerprintReadContractProbePassed"
    Write-Host "release-version-agreement=$releaseVersionAgreementPassed"
    Write-Host "installer-disposable-predecessor-transition=$disposablePredecessorTransition"
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
