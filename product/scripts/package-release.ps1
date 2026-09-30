param(
    [string]$PackageName = "facial"
)

$ErrorActionPreference = "Stop"

# WP-059 release contract:
#   installer/facial-portable-<version>.exe   one current portable build
#   installer/facial-setup-<version>.exe      one current installer
#   installer/installer-portable-archive/     every superseded delivery artifact
# A successful package increments the Cargo patch version exactly once. Failed
# compilation restores the prior manifest/topology/lock versions and leaves the
# current delivery pair untouched.
$scriptDir       = Split-Path -Parent $MyInvocation.MyCommand.Path
$productRoot     = Resolve-Path (Join-Path $scriptDir "..")
$repoRoot        = Resolve-Path (Join-Path $productRoot "..")
# WP-088: hold the shared repository lock through build, publication and cleanup.
$cargoGuard = Join-Path $scriptDir 'cargo-workspace.ps1'
$lockHasher = [Security.Cryptography.SHA256]::Create()
try {
    $lockKey = [BitConverter]::ToString($lockHasher.ComputeHash([Text.Encoding]::UTF8.GetBytes(([string]$repoRoot).TrimEnd('\').ToLowerInvariant()))).Replace('-', '').ToLowerInvariant()
} finally { $lockHasher.Dispose() }
$packageMutex = [Threading.Mutex]::new($false, ('Local\FacialCargo-' + $lockKey))
$packageLockHeld = $false
try {
    try { $packageLockHeld = $packageMutex.WaitOne(0) }
    catch [Threading.AbandonedMutexException] { $packageLockHeld = $true }
    if (-not $packageLockHeld) { throw 'Facial Cargo is busy; packaging did not start.' }
    & $cargoGuard -Probe > $null

$installerDir    = Join-Path $repoRoot "installer"
$archiveDir      = Join-Path $installerDir "installer-portable-archive"
$payloadDir      = Join-Path $installerDir "payload"
$compiledDir     = Join-Path $payloadDir "compiled"
$manifestPath    = Join-Path $productRoot "Cargo.toml"
$lockPath        = Join-Path $productRoot "Cargo.lock"
$topologyPath    = Join-Path $repoRoot "topology.yaml"
$cargoExe        = Join-Path $repoRoot "build-artifacts\cargo\release\$PackageName.exe"
$cargoCliExe     = Join-Path $repoRoot "build-artifacts\cargo\release\$PackageName-cli.exe"
$legacyCanonical = Join-Path $productRoot "$PackageName.exe"
$legacyCanonicalHash = Join-Path $productRoot "$PackageName.exe.sha256"
$legacyReleaseHash = Join-Path $productRoot "release-artifacts.sha256"
$legacyArchive   = Join-Path $productRoot "archive\exe"
$legacyOutDir    = Join-Path $installerDir "out"
$releaseSeedPath = Join-Path $productRoot "config\release-default.json"
$releaseSeedContract = "facial-sanitized-release-seed-v1"
$retirementToolSource = Join-Path $scriptDir "retire-legacy-media-db.ps1"
$frozenRetirementToolSha256 = "D5593C45712EE253D48805E4237F90B2BB230ADB4A21BA84321CA9AC68D1878A"
$stamp           = Get-Date -Format "yyyyMMdd-HHmmss"

function Get-ManifestVersion {
    param([Parameter(Mandatory = $true)][string]$Raw)
    $match = [regex]::Match(
        $Raw,
        '(?ms)^\[package\]\s*.*?^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"'
    )
    if (-not $match.Success) {
        throw "product/Cargo.toml [package] version must be numeric SemVer (major.minor.patch)."
    }
    return [pscustomobject]@{
        Value = $match.Groups[1].Value + "." + $match.Groups[2].Value + "." + $match.Groups[3].Value
        Major = [int]$match.Groups[1].Value
        Minor = [int]$match.Groups[2].Value
        Patch = [int]$match.Groups[3].Value
    }
}

function Set-ManifestVersion {
    param(
        [Parameter(Mandatory = $true)][string]$Raw,
        [Parameter(Mandatory = $true)][string]$Version
    )
    $pattern = '(?ms)(^\[package\]\s*.*?^version\s*=\s*")(\d+\.\d+\.\d+)(")'
    $updated = [regex]::Replace(
        $Raw,
        $pattern,
        { param($m) $m.Groups[1].Value + $Version + $m.Groups[3].Value },
        1
    )
    if ($updated -eq $Raw) { throw "Could not update package version in product/Cargo.toml." }
    return $updated
}

function Set-TopologyVersion {
    param(
        [Parameter(Mandatory = $true)][string]$Raw,
        [Parameter(Mandatory = $true)][string]$Version
    )
    $pattern = '(?m)^(\s{2}version:\s*)(\d+\.\d+\.\d+)(\s*)$'
    $updated = [regex]::Replace(
        $Raw,
        $pattern,
        { param($m) $m.Groups[1].Value + $Version + $m.Groups[3].Value },
        1
    )
    if ($updated -eq $Raw) { throw "Could not update project.version in topology.yaml." }
    return $updated
}

function Move-ToDeliveryArchive {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [string]$PreferredName
    )
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return }
    New-Item -ItemType Directory -Force -Path $archiveDir | Out-Null
    $name = if ([string]::IsNullOrWhiteSpace($PreferredName)) {
        [IO.Path]::GetFileName($Path)
    } else {
        $PreferredName
    }
    $destination = Join-Path $archiveDir $name
    if (Test-Path -LiteralPath $destination) {
        $base = [IO.Path]::GetFileNameWithoutExtension($name)
        $extension = [IO.Path]::GetExtension($name)
        $destination = Join-Path $archiveDir "$base-$stamp$extension"
        $suffix = 2
        while (Test-Path -LiteralPath $destination) {
            $destination = Join-Path $archiveDir "$base-$stamp-$suffix$extension"
            $suffix++
        }
    }
    Move-Item -LiteralPath $Path -Destination $destination
    Write-Host "archived=$destination"
    return [pscustomobject]@{
        Source = $Path
        Destination = $destination
    }
}

function Remove-EmptyDirectory {
    param([Parameter(Mandatory = $true)][string]$Path)
    if ((Test-Path -LiteralPath $Path -PathType Container) -and
        @(Get-ChildItem -LiteralPath $Path -Force).Count -eq 0) {
        Remove-Item -LiteralPath $Path -Force
    }
}

function Get-PeSubsystem {
    param([Parameter(Mandatory = $true)][string]$Path)
    $bytes = [IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 256 -or $bytes[0] -ne 0x4D -or $bytes[1] -ne 0x5A) {
        throw "Not a valid PE executable: $Path"
    }
    $peOffset = [BitConverter]::ToInt32($bytes, 0x3C)
    $subsystemOffset = $peOffset + 4 + 20 + 68
    if ($peOffset -lt 0 -or $subsystemOffset + 1 -ge $bytes.Length) {
        throw "Invalid PE optional header: $Path"
    }
    return [BitConverter]::ToUInt16($bytes, $subsystemOffset)
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

function Get-ReleaseSeedStringLeaves {
    param(
        [AllowNull()]$Value,
        [string]$JsonPath = '$',
        [string]$PropertyName = ''
    )

    if ($null -eq $Value) { return }
    if ($Value -is [string]) {
        Write-Output ([pscustomobject]@{
            JsonPath = $JsonPath
            PropertyName = $PropertyName
            Value = [string]$Value
        })
        return
    }
    if ($Value -is [pscustomobject]) {
        foreach ($property in $Value.PSObject.Properties) {
            Get-ReleaseSeedStringLeaves `
                -Value $property.Value `
                -JsonPath "$JsonPath.$($property.Name)" `
                -PropertyName $property.Name
        }
        return
    }
    if ($Value -is [System.Collections.IDictionary]) {
        foreach ($key in $Value.Keys) {
            Get-ReleaseSeedStringLeaves `
                -Value $Value[$key] `
                -JsonPath "$JsonPath.$key" `
                -PropertyName ([string]$key)
        }
        return
    }
    if ($Value -is [System.Collections.IEnumerable]) {
        $index = 0
        foreach ($item in $Value) {
            Get-ReleaseSeedStringLeaves `
                -Value $item `
                -JsonPath "$JsonPath[$index]" `
                -PropertyName $PropertyName
            $index++
        }
    }
}

function Test-RootedOrUserMachinePath {
    param([Parameter(Mandatory = $true)][string]$Value)

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

function Assert-SanitizedReleaseSeed {
    param([Parameter(Mandatory = $true)][string]$Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Sanitized release seed is missing: $Path"
    }
    $raw = [IO.File]::ReadAllText($Path)
    try {
        $parsed = $raw | ConvertFrom-Json
    } catch {
        throw "Sanitized release seed is not valid JSON: $Path ($($_.Exception.Message))"
    }
    if (-not ($parsed -is [pscustomobject])) {
        throw "Sanitized release seed must be one JSON object: $Path"
    }

    $unsafe = @(
        Get-ReleaseSeedStringLeaves -Value $parsed |
            Where-Object { Test-RootedOrUserMachinePath -Value $_.Value }
    )
    if ($unsafe.Count -gt 0) {
        $details = @($unsafe | ForEach-Object { "$($_.JsonPath)=$($_.Value)" }) -join '; '
        throw "Sanitized release seed contains rooted or user-machine path data: $details"
    }

    return [pscustomobject]@{
        Contract = $releaseSeedContract
        Path = [IO.Path]::GetFullPath($Path)
        Sha256 = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToUpperInvariant()
    }
}

# Validate the dedicated release seed before any version or build authority changes.
$releaseSeedProof = Assert-SanitizedReleaseSeed -Path $releaseSeedPath
Write-Host "release-config-source=contract:$($releaseSeedProof.Contract);sha256:$($releaseSeedProof.Sha256);path:product/config/release-default.json"

# WP-079 was live-audited at this exact digest. Equality between two mutable
# copies is insufficient: a synchronized source/payload edit must fail before
# the version, build, or current delivery pair can change.
$retirementToolCounterfactualHash = "A" * 64
$retirementToolMutationCounterfactualRejected = -not (
    Test-FrozenRetirementToolHashSet -Hashes @(
        $retirementToolCounterfactualHash,
        $retirementToolCounterfactualHash
    )
)
if (-not $retirementToolMutationCounterfactualRejected) {
    throw "Frozen WP-079 retirement-tool hash gate accepted a synchronized mutated source/payload counterfactual."
}
if (-not (Test-Path -LiteralPath $retirementToolSource -PathType Leaf)) {
    throw "Frozen WP-079 retirement tool is missing: $retirementToolSource"
}
$retirementToolSourceSha256 = (Get-FileHash -LiteralPath $retirementToolSource -Algorithm SHA256).Hash.ToUpperInvariant()
if (-not (Test-FrozenRetirementToolHashSet -Hashes @($retirementToolSourceSha256))) {
    throw "WP-079 retirement tool source hash differs from the frozen live-audited digest $frozenRetirementToolSha256; observed $retirementToolSourceSha256."
}
Write-Host "retirement-tool-source=sha256:$retirementToolSourceSha256;frozen:true;mutation-counterfactual-rejected:true"

# Resolve the required installer compiler before changing version authority.
$iscc = $null
foreach ($candidate in @(
    (Get-Command ISCC -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty Source),
    (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe"),
    "C:\Program Files (x86)\Inno Setup 6\ISCC.exe",
    "C:\Program Files\Inno Setup 6\ISCC.exe"
)) {
    if ($candidate -and (Test-Path -LiteralPath $candidate)) {
        $iscc = $candidate
        break
    }
}
if (-not $iscc) {
    throw "Inno Setup (ISCC.exe) not found. Install it once: winget install --id JRSoftware.InnoSetup -e"
}

# WP-077/WP-078: never refresh an embedded database engine during packaging.
# An engine change is a separate, explicit migration with a populated
# compatibility fixture and recorded decision.
#
# This gate runs BEFORE the version bump, like the ISCC resolution above:
# bumping the manifest changes this workspace's own package version, which by
# definition makes Cargo.lock stale, so `--locked` could never succeed
# afterwards. It is a precondition, not a build step.
$engineDecisionPath = Join-Path $productRoot "surrealdb-engine-compatibility.json"
$engineDecision = Get-Content -LiteralPath $engineDecisionPath -Raw | ConvertFrom-Json
# `cargo metadata` still runs so `--locked` proves the lock file is current,
# but its JSON is NOT parsed here: the dependency graph contains keys that
# differ only by case, which ConvertFrom-Json rejects outright on both Windows
# PowerShell 5.1 and PowerShell 7 (DuplicateKeysInJsonString), so this gate
# could never execute. The locked version is read from Cargo.lock, which is the
# authority for a locked resolution anyway.
& $cargoGuard -CargoArgs @('metadata', '--locked', '--format-version', '1') > $null
if ($LASTEXITCODE -ne 0) { throw "Locked Cargo metadata failed (exit $LASTEXITCODE)." }
$lockRaw = Get-Content -LiteralPath $lockPath -Raw
$resolvedEngine = @(
    [regex]::Matches($lockRaw, '(?m)^name = "surrealdb"\r?\nversion = "([^"]+)"') |
        ForEach-Object { $_.Groups[1].Value } |
        Select-Object -Unique
)
if ($resolvedEngine.Count -ne 1 -or $resolvedEngine[0] -ne $engineDecision.engine_version) {
    throw "SurrealDB engine upgrade gate failed: compatibility decision requires $($engineDecision.engine_version), resolved $($resolvedEngine -join ',')."
}
if ($engineDecision.populated_fixture.status -ne "passed") {
    throw "SurrealDB populated upgrade fixture is not recorded as passed."
}
Write-Host "surrealdb-upgrade-gate=exact:$($resolvedEngine[0]);fixture:$($engineDecision.populated_fixture.test)"

$manifestOriginal = Get-Content -Raw -LiteralPath $manifestPath
$topologyOriginal = Get-Content -Raw -LiteralPath $topologyPath
$lockOriginal = if (Test-Path -LiteralPath $lockPath) {
    [IO.File]::ReadAllBytes($lockPath)
} else {
    $null
}
$current = Get-ManifestVersion -Raw $manifestOriginal
$version = "$($current.Major).$($current.Minor).$($current.Patch + 1)"
$portableName = "$PackageName-portable-$version.exe"
$setupName = "$PackageName-setup-$version.exe"
$portableRoot = Join-Path $installerDir $portableName
$setupRoot = Join-Path $installerDir $setupName
$compiledSetup = Join-Path $compiledDir $setupName
$stagedPortable = Join-Path $compiledDir $portableName
$published = $false
$newPortablePlaced = $false
$newSetupPlaced = $false
$archiveMoves = New-Object System.Collections.Generic.List[object]

try {
    $manifestUpdated = Set-ManifestVersion -Raw $manifestOriginal -Version $version
    $topologyUpdated = Set-TopologyVersion -Raw $topologyOriginal -Version $version
    [IO.File]::WriteAllText($manifestPath, $manifestUpdated, [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText($topologyPath, $topologyUpdated, [Text.UTF8Encoding]::new($false))
    Write-Host "version-bump=$($current.Value)->$version"

    & $cargoGuard -CargoArgs @('build', '--release', '--bins')
    if ($LASTEXITCODE -ne 0) { throw "cargo release build failed (exit $LASTEXITCODE)." }
    if (-not (Test-Path -LiteralPath $cargoExe -PathType Leaf)) {
        throw "cargo reported success but release executable is missing: $cargoExe"
    }
    if (-not (Test-Path -LiteralPath $cargoCliExe -PathType Leaf)) {
        throw "cargo reported success but CLI executable is missing: $cargoCliExe"
    }
    $guiSubsystem = Get-PeSubsystem -Path $cargoExe
    $cliSubsystem = Get-PeSubsystem -Path $cargoCliExe
    if ($guiSubsystem -ne 2) {
        throw "facial.exe must use IMAGE_SUBSYSTEM_WINDOWS_GUI (2); observed $guiSubsystem."
    }
    if ($cliSubsystem -ne 3) {
        throw "facial-cli.exe must use IMAGE_SUBSYSTEM_WINDOWS_CUI (3); observed $cliSubsystem."
    }
    Write-Host "pe-subsystems=facial.exe:$guiSubsystem,facial-cli.exe:$cliSubsystem"

    # Stage the installer without touching the current delivery pair. The
    # compiled setup remains under transient payload until ISCC succeeds.
    if (Test-Path -LiteralPath $payloadDir) {
        Remove-Item -LiteralPath $payloadDir -Recurse -Force
    }
    $payloadProductRoot = Join-Path $payloadDir "product"
    $payloadScriptRoot = Join-Path $payloadProductRoot "scripts"
    New-Item -ItemType Directory -Force -Path $payloadProductRoot, $payloadScriptRoot, $compiledDir | Out-Null
    Copy-Item -LiteralPath $cargoExe -Destination (Join-Path $payloadDir "facial.exe") -Force
    Copy-Item -LiteralPath $cargoCliExe -Destination (Join-Path $payloadDir "facial-cli.exe") -Force
    Copy-Item -LiteralPath $cargoExe -Destination $stagedPortable -Force
    foreach ($sub in @("plugins", "assets", "docs")) {
        $source = Join-Path $productRoot $sub
        if (Test-Path -LiteralPath $source) {
            Copy-Item -LiteralPath $source -Destination (Join-Path $payloadDir "product\$sub") -Recurse -Force
        }
    }
    $payloadConfigRoot = Join-Path $payloadProductRoot "config"
    $stagedDefaultConfig = Join-Path $payloadConfigRoot "default.json"
    New-Item -ItemType Directory -Force -Path $payloadConfigRoot | Out-Null
    Copy-Item -LiteralPath $releaseSeedPath -Destination $stagedDefaultConfig -Force
    if (Test-Path -LiteralPath (Join-Path $payloadConfigRoot "release-default.json")) {
        throw "Release seed must be staged only as product/config/default.json."
    }
    $stagedReleaseSeedProof = Assert-SanitizedReleaseSeed -Path $stagedDefaultConfig
    if ($stagedReleaseSeedProof.Sha256 -ne $releaseSeedProof.Sha256) {
        throw "Staged product/config/default.json does not exactly match the sanitized release seed."
    }
    Write-Host "release-config-staged=contract:$releaseSeedContract;sha256:$($stagedReleaseSeedProof.Sha256);path:product/config/default.json;release-default-duplicate:false"
    foreach ($operatorScript in @("retire-legacy-media-db.ps1", "test-retire-legacy-media-db.ps1")) {
        $operatorScriptSource = Join-Path $scriptDir $operatorScript
        if (-not (Test-Path -LiteralPath $operatorScriptSource -PathType Leaf)) {
            throw "Required WP-079 operator script is missing: $operatorScriptSource"
        }
        Copy-Item -LiteralPath $operatorScriptSource -Destination (Join-Path $payloadScriptRoot $operatorScript) -Force
    }
    $stagedRetirementTool = Join-Path $payloadScriptRoot "retire-legacy-media-db.ps1"
    $stagedRetirementToolSha256 = (Get-FileHash -LiteralPath $stagedRetirementTool -Algorithm SHA256).Hash.ToUpperInvariant()
    if (-not (Test-FrozenRetirementToolHashSet -Hashes @(
        $retirementToolSourceSha256,
        $stagedRetirementToolSha256
    ))) {
        throw "Staged WP-079 retirement tool and repository source must both equal the frozen live-audited digest $frozenRetirementToolSha256 (source $retirementToolSourceSha256; staged $stagedRetirementToolSha256)."
    }
    Write-Host "retirement-tool-staged=sha256:$stagedRetirementToolSha256;frozen:true"

    & $iscc "/DAppVersion=$version" "/DPayloadDir=payload" "/DOutputDir=payload\compiled" (Join-Path $installerDir "facial.iss")
    if ($LASTEXITCODE -ne 0) { throw "ISCC failed to compile the installer (exit $LASTEXITCODE)." }
    if (-not (Test-Path -LiteralPath $compiledSetup -PathType Leaf)) {
        throw "ISCC reported success but setup output is missing: $compiledSetup"
    }

    # Only after both artifacts exist do we archive the prior delivery set.
    foreach ($oldRootExe in @(Get-ChildItem -LiteralPath $installerDir -Filter "*.exe" -File -Force -ErrorAction SilentlyContinue)) {
        $archiveMoves.Add((Move-ToDeliveryArchive -Path $oldRootExe.FullName))
    }
    if (Test-Path -LiteralPath $legacyCanonical -PathType Leaf) {
        $archiveMoves.Add((Move-ToDeliveryArchive -Path $legacyCanonical -PreferredName "$PackageName-portable-$($current.Value).exe"))
    }
    if (Test-Path -LiteralPath $legacyCanonicalHash -PathType Leaf) {
        $archiveMoves.Add((Move-ToDeliveryArchive -Path $legacyCanonicalHash -PreferredName "$PackageName-portable-$($current.Value).exe.sha256"))
    }
    if (Test-Path -LiteralPath $legacyReleaseHash -PathType Leaf) {
        $archiveMoves.Add((Move-ToDeliveryArchive -Path $legacyReleaseHash))
    }
    if (Test-Path -LiteralPath $legacyArchive -PathType Container) {
        foreach ($legacy in @(Get-ChildItem -LiteralPath $legacyArchive -File -Force)) {
            $archiveMoves.Add((Move-ToDeliveryArchive -Path $legacy.FullName))
        }
    }
    if (Test-Path -LiteralPath $legacyOutDir -PathType Container) {
        foreach ($legacy in @(Get-ChildItem -LiteralPath $legacyOutDir -File -Force)) {
            $archiveMoves.Add((Move-ToDeliveryArchive -Path $legacy.FullName))
        }
    }

    Move-Item -LiteralPath $stagedPortable -Destination $portableRoot
    $newPortablePlaced = $true
    Move-Item -LiteralPath $compiledSetup -Destination $setupRoot
    $newSetupPlaced = $true
    Write-Host "portable=$portableRoot"
    Write-Host "installer=$setupRoot"

    # Remove legacy/transient artifact surfaces after successful migration.
    Remove-EmptyDirectory -Path $legacyArchive
    Remove-EmptyDirectory -Path (Join-Path $productRoot "archive")
    Remove-EmptyDirectory -Path $legacyOutDir
    if (Test-Path -LiteralPath $payloadDir) {
        Remove-Item -LiteralPath $payloadDir -Recurse -Force
    }
    & $cargoGuard -Clean

    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $scriptDir "check-exe-layout.ps1")
    if ($LASTEXITCODE -ne 0) {
        throw "delivery-artifact invariant failed after packaging."
    }
    $published = $true
}
catch {
    if (-not $published) {
        if ($newSetupPlaced -and (Test-Path -LiteralPath $setupRoot -PathType Leaf)) {
            Remove-Item -LiteralPath $setupRoot -Force
        }
        if ($newPortablePlaced -and (Test-Path -LiteralPath $portableRoot -PathType Leaf)) {
            Remove-Item -LiteralPath $portableRoot -Force
        }
        for ($index = $archiveMoves.Count - 1; $index -ge 0; $index--) {
            $move = $archiveMoves[$index]
            if (Test-Path -LiteralPath $move.Destination -PathType Leaf) {
                $sourceParent = Split-Path -Parent $move.Source
                New-Item -ItemType Directory -Force -Path $sourceParent | Out-Null
                Move-Item -LiteralPath $move.Destination -Destination $move.Source
            }
        }
        [IO.File]::WriteAllText($manifestPath, $manifestOriginal, [Text.UTF8Encoding]::new($false))
        [IO.File]::WriteAllText($topologyPath, $topologyOriginal, [Text.UTF8Encoding]::new($false))
        if ($null -ne $lockOriginal) {
            [IO.File]::WriteAllBytes($lockPath, $lockOriginal)
        } elseif (Test-Path -LiteralPath $lockPath) {
            Remove-Item -LiteralPath $lockPath -Force
        }
        Write-Warning "Packaging failed before publish; version authority restored to $($current.Value)."
    }
    if (Test-Path -LiteralPath $payloadDir) {
        Remove-Item -LiteralPath $payloadDir -Recurse -Force
    }
    if (-not $published) { & $cargoGuard -Clean }
    throw
}

} finally {
    if ($packageLockHeld) { $packageMutex.ReleaseMutex() }
    $packageMutex.Dispose()
}
