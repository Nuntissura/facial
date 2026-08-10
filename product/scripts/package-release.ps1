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
$installerDir    = Join-Path $repoRoot "installer"
$archiveDir      = Join-Path $installerDir "installer-portable-archive"
$payloadDir      = Join-Path $installerDir "payload"
$compiledDir     = Join-Path $payloadDir "compiled"
$manifestPath    = Join-Path $productRoot "Cargo.toml"
$lockPath        = Join-Path $productRoot "Cargo.lock"
$topologyPath    = Join-Path $repoRoot "topology.yaml"
$cargoExe        = Join-Path $productRoot "target\release\$PackageName.exe"
$legacyCanonical = Join-Path $productRoot "$PackageName.exe"
$legacyCanonicalHash = Join-Path $productRoot "$PackageName.exe.sha256"
$legacyReleaseHash = Join-Path $productRoot "release-artifacts.sha256"
$legacyArchive   = Join-Path $productRoot "archive\exe"
$legacyOutDir    = Join-Path $installerDir "out"
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

    cargo build --manifest-path $manifestPath --release
    if ($LASTEXITCODE -ne 0) { throw "cargo release build failed (exit $LASTEXITCODE)." }
    if (-not (Test-Path -LiteralPath $cargoExe -PathType Leaf)) {
        throw "cargo reported success but release executable is missing: $cargoExe"
    }

    # Stage the installer without touching the current delivery pair. The
    # compiled setup remains under transient payload until ISCC succeeds.
    if (Test-Path -LiteralPath $payloadDir) {
        Remove-Item -LiteralPath $payloadDir -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path (Join-Path $payloadDir "product"), $compiledDir | Out-Null
    Copy-Item -LiteralPath $cargoExe -Destination (Join-Path $payloadDir "facial.exe") -Force
    Copy-Item -LiteralPath $cargoExe -Destination $stagedPortable -Force
    Copy-Item -LiteralPath (Join-Path $installerDir "launch-facial.cmd") -Destination $payloadDir -Force
    foreach ($sub in @("config", "plugins", "assets", "docs")) {
        $source = Join-Path $productRoot $sub
        if (Test-Path -LiteralPath $source) {
            Copy-Item -LiteralPath $source -Destination (Join-Path $payloadDir "product\$sub") -Recurse -Force
        }
    }

    & $iscc "/DAppVersion=$version" "/DPayloadDir=payload" "/DOutputDir=payload\compiled" (Join-Path $installerDir "facial.iss")
    if ($LASTEXITCODE -ne 0) { throw "ISCC failed to compile the installer (exit $LASTEXITCODE)." }
    if (-not (Test-Path -LiteralPath $compiledSetup -PathType Leaf)) {
        throw "ISCC reported success but setup output is missing: $compiledSetup"
    }

    # Only after both artifacts exist do we archive the prior delivery set.
    foreach ($oldRootExe in @(Get-ChildItem -LiteralPath $installerDir -Filter "*.exe" -File -ErrorAction SilentlyContinue)) {
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
        foreach ($legacy in @(Get-ChildItem -LiteralPath $legacyArchive -File)) {
            $archiveMoves.Add((Move-ToDeliveryArchive -Path $legacy.FullName))
        }
    }
    if (Test-Path -LiteralPath $legacyOutDir -PathType Container) {
        foreach ($legacy in @(Get-ChildItem -LiteralPath $legacyOutDir -File)) {
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
    $targetDir = Join-Path $productRoot "target"
    if (Test-Path -LiteralPath $targetDir) {
        Remove-Item -LiteralPath $targetDir -Recurse -Force
    }
    Write-Host "cleaned-scratch=$targetDir"

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
    $failedTargetDir = Join-Path $productRoot "target"
    if ((-not $published) -and (Test-Path -LiteralPath $failedTargetDir)) {
        Remove-Item -LiteralPath $failedTargetDir -Recurse -Force
    }
    throw
}
