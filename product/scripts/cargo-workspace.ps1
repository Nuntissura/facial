# WP-088: one portable, serialized Cargo entrypoint; no machine-wide settings.
[CmdletBinding()]
param(
    [string[]]$CargoArgs = @(),
    [switch]$Clean,
    [switch]$CleanLegacy,
    [switch]$Probe,
    [switch]$CompatibilityCandidate,
    [switch]$PrepareCompatibilityCandidateLock,
    [switch]$PrepareVendoredCoreLock
)

$ErrorActionPreference = 'Stop'
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..')).TrimEnd('\', '/')
$cargoRoot = Join-Path $repoRoot 'build-artifacts/cargo'
$tempRoot = Join-Path $repoRoot 'build-artifacts/tmp'
$manifest = Join-Path $repoRoot 'product/Cargo.toml'
$candidateMode = $CompatibilityCandidate -or $PrepareCompatibilityCandidateLock
if ($candidateMode) { $manifest = Join-Path $repoRoot 'product/tests/engine-compatibility-candidate/Cargo.toml' }
$cleanupRoots = @($cargoRoot, $tempRoot)
if ($CleanLegacy) { $cleanupRoots += @((Join-Path $repoRoot 'target'), (Join-Path $repoRoot 'product/target')) }

function Assert-PlainPath([string]$Path) {
    $full = [IO.Path]::GetFullPath($Path)
    if (-not $full.StartsWith($repoRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Cargo path escapes repository: $full"
    }
    $cursor = $full
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            $item = Get-Item -LiteralPath $cursor -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Cargo path contains a reparse point: $cursor"
            }
        }
        $cursor = [IO.Path]::GetDirectoryName($cursor)
    }
}

function Assert-PlainTree([string]$Path) {
    Assert-PlainPath $Path
    if (-not (Test-Path -LiteralPath $Path)) { return }
    $pending = New-Object 'Collections.Generic.Queue[string]'
    $pending.Enqueue($Path)
    $directories = 0
    while ($pending.Count) {
        $directory = $pending.Dequeue()
        $directoryInfo = [IO.DirectoryInfo]$directory
        if (($directoryInfo.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Cargo refuses reparse point: $directory"
        }
        # Stream entries rather than materializing a PowerShell provider listing.
        foreach ($item in $directoryInfo.EnumerateFileSystemInfos()) {
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Cargo refuses reparse point: $($item.FullName)"
            }
            if (($item.Attributes -band [IO.FileAttributes]::Directory) -ne 0) { $pending.Enqueue($item.FullName) }
        }
        $directories++
        if ($Clean -and $directories % 1000 -eq 0) { Write-Host "Cargo cleanup: checked $directories directories under $Path" }
    }
}

function Assert-CompatibilityCandidate([switch]$RequireLock) {
    # Diagnostic candidate only; never updates the shipped engine decision or manifest.
    Assert-PlainTree ([IO.Path]::GetDirectoryName($manifest))
    Assert-PlainPath $manifest
    $lockPath = Join-Path ([IO.Path]::GetDirectoryName($manifest)) 'Cargo.lock'
    Assert-PlainPath $lockPath
    if (-not [IO.File]::Exists($manifest)) { throw "Missing fixed compatibility candidate manifest: $manifest" }
    $toml = [IO.File]::ReadAllText($manifest)
    if ($toml -notmatch '(?m)^\s*\[workspace\]\s*(?:#.*)?$' -or
        $toml -notmatch '(?m)^\s*surrealdb\s*=\s*\{[^\r\n]*\bversion\s*=\s*["\x27]=3\.3\.0["\x27][^\r\n]*\}\s*(?:#.*)?$' -or
        $toml -match '(?m)(?:^|[\s,{])(?:path|git)\s*=' -or
        $toml -match '(?m)^\s*\[(?:patch|replace)(?:[.\]])') {
        throw 'Candidate must be an isolated workspace with exact surrealdb =3.3.0 and no path/git dependencies.'
    }
    if ($RequireLock -or -not $PrepareCompatibilityCandidateLock) {
        if (-not [IO.File]::Exists($lockPath)) { throw 'Prepare the fixed candidate lock explicitly before candidate compilation.' }
        $locked = [IO.File]::ReadAllText($lockPath)
        $sdk = @([regex]::Matches($locked, '(?ms)^\[\[package\]\]\r?\n(?:(?!^\[\[package\]\]).)*?^name = "surrealdb"\r?\nversion = "([^"]+)"'))
        if ($sdk.Count -ne 1 -or $sdk[0].Groups[1].Value -cne '3.3.0') { throw 'Candidate Cargo.lock must contain exactly one surrealdb SDK pinned to 3.3.0.' }
    }
}

function Get-CandidateLockIdentities([string]$LockPath) {
    $text = [IO.File]::ReadAllText($LockPath)
    foreach ($block in [regex]::Matches($text, '(?ms)^\[\[package\]\]\r?\n.*?(?=^\[\[package\]\]|\z)')) {
        $fields = @{}
        foreach ($key in @('name', 'version', 'source', 'checksum')) {
            $value = [regex]::Match($block.Value, ('(?m)^' + $key + ' = "([^"\r\n]+)"\r?$'))
            $fields[$key] = if ($value.Success) { $value.Groups[1].Value } else { '' }
        }
        if (-not $fields.name -or -not $fields.version) { throw 'Malformed candidate lock package identity.' }
        # Root dependencies may change; resolved dependency identities must remain.
        if ($fields.name -ceq 'facial-engine-compatibility-candidate' -and -not $fields.source) { continue }
        if (-not $fields.source -or -not $fields.checksum) { throw 'Candidate dependency lacks registry source/checksum.' }
        $fields.name + '|' + $fields.version + '|' + $fields.source + '|' + $fields.checksum
    }
}

function Get-ProductLockIdentities([string]$LockPath) {
    $text = [IO.File]::ReadAllText($LockPath)
    foreach ($block in [regex]::Matches($text, '(?ms)^\[\[package\]\]\r?\n.*?(?=^\[\[package\]\]|\z)')) {
        $fields = @{}
        foreach ($key in @('name', 'version', 'source', 'checksum')) {
            $value = [regex]::Match($block.Value, ('(?m)^' + $key + ' = "([^"\r\n]+)"\r?$'))
            $fields[$key] = if ($value.Success) { $value.Groups[1].Value } else { '' }
        }
        if (-not $fields.name -or -not $fields.version) { throw 'Malformed product lock identity.' }
        $fields.name + '|' + $fields.version + '|' + $fields.source + '|' + $fields.checksum
    }
}

function Assert-NoActiveArtifacts {
    # Check exact repository ownership; never stop processes or block other projects.
    $active = @(Get-CimInstance Win32_Process -ErrorAction Stop | Where-Object {
        $process = $_
        $usesArtifacts = $false
        foreach ($root in $cleanupRoots) {
            if ($process.ExecutablePath -and $process.ExecutablePath.StartsWith($root + '\', [StringComparison]::OrdinalIgnoreCase)) {
                $usesArtifacts = $true
            }
        }
        $_.ProcessId -ne $PID -and (
            $usesArtifacts -or
            ($_.Name -match '^(cargo|rustc|rustdoc|build-script[^.]*)\.exe$' -and $_.CommandLine -and
                $_.CommandLine.IndexOf($repoRoot, [StringComparison]::OrdinalIgnoreCase) -ge 0)
        )
    })
    if ($active.Count) {
        throw "Repository Cargo artifacts are in use by PID(s): $($active.ProcessId -join ','). No process was stopped."
    }
}

if ($candidateMode -and ($Clean -or $CleanLegacy)) { throw 'Compatibility candidate mode cannot clean artifacts.' }
if ($PrepareCompatibilityCandidateLock -and ($Probe -or $CargoArgs.Count)) { throw 'Candidate lock preparation is exclusive with Probe and CargoArgs.' }
if ($PrepareVendoredCoreLock -and ($candidateMode -or $Clean -or $CleanLegacy -or $Probe -or $CargoArgs.Count)) {
    throw 'Vendored core lock preparation is an exclusive product-only route.'
}
if (($CleanLegacy -and -not $Clean) -or ($Clean -and $Probe) -or (($Clean -or $Probe) -and $CargoArgs.Count)) {
    throw 'Choose CargoArgs, Clean, or Probe exclusively.'
}
if (-not $Clean -and -not $Probe -and -not $PrepareCompatibilityCandidateLock -and -not $PrepareVendoredCoreLock -and $CargoArgs.Count -eq 0) {
    throw 'Supply -CargoArgs, -Probe, or -Clean.'
}
$allowed = @('build', 'check', 'test', 'run', 'fmt', 'clippy', 'metadata')
if ($CargoArgs.Count -and $CargoArgs[0] -notin $allowed) {
    throw "Unsupported Cargo command. Allowed: $($allowed -join ','); use -Clean for cleanup."
}
if ($candidateMode -and $CargoArgs.Count -and $CargoArgs[0] -eq 'run') { throw 'Candidate run is forbidden; execute the inspected diagnostic binary separately.' }
foreach ($arg in $CargoArgs) {
    if ($arg -match '^(--(target-dir|build-dir|config|manifest-path|lockfile-path|out-dir|artifact-dir)(=|$)|-m[^-]|-m$|-Z)') {
        throw "Cargo output/configuration/manifest overrides are forbidden: $arg"
    }
}
foreach ($name in @('CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET_DIR', 'CARGO_BUILD_BUILD_DIR')) {
    $value = [Environment]::GetEnvironmentVariable($name, 'Process')
    if ($value -and [IO.Path]::GetFullPath($value).TrimEnd('\', '/') -ine $cargoRoot) {
        throw "Conflicting $name=$value; expected $cargoRoot. Remove the override in the calling process."
    }
}
Assert-PlainPath $cargoRoot
Assert-PlainPath $tempRoot
if ($candidateMode) { Assert-CompatibilityCandidate }
$sha = [Security.Cryptography.SHA256]::Create()
try { $hash = [BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($repoRoot.ToLowerInvariant()))).Replace('-', '').ToLowerInvariant() }
finally { $sha.Dispose() }
$mutexName = 'Local\FacialCargo-' + $hash
$mutex = New-Object Threading.Mutex($false, $mutexName)
$held = $false
$saved = @{}
$pushed = $false
try {
    try { $held = $mutex.WaitOne(0) } catch [Threading.AbandonedMutexException] { $held = $true }
    if (-not $held) { throw "Facial Cargo is busy ($mutexName). Reuse the coordinator; do not start another build." }
    Assert-NoActiveArtifacts
    foreach ($path in $cleanupRoots) {
        if ($Clean) { Write-Host "Cargo cleanup: checking $path" }
        Assert-PlainTree $path
    }
    if ($Clean) {
        Assert-NoActiveArtifacts
        foreach ($path in $cleanupRoots) {
            Assert-PlainPath $path
            if (Test-Path -LiteralPath $path) {
                Write-Host "Cargo cleanup: removing $path"
                # Avoid PowerShell provider traversal after the explicit no-link scan.
                # Read-only/locked files fail visibly; do not relax attributes silently.
                [IO.Directory]::Delete($path, $true)
            }
            if (Test-Path -LiteralPath $path) { throw "Cleanup did not remove $path" }
        }
        Write-Output 'Cargo cleanup PASS: canonical cargo and tmp directories absent.'
        return
    }
    $values = @{
        CARGO_TARGET_DIR = $cargoRoot
        CARGO_BUILD_TARGET_DIR = $cargoRoot
        CARGO_BUILD_BUILD_DIR = $cargoRoot
        TMP = $tempRoot
        TEMP = $tempRoot
        TMPDIR = $tempRoot
    }
    foreach ($name in $values.Keys) {
        $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        [Environment]::SetEnvironmentVariable($name, $values[$name], 'Process')
    }
    Push-Location -LiteralPath $repoRoot
    $pushed = $true
    $tomlCargo = $cargoRoot.Replace('\', '/')
    $pin = @('--config', "build.target-dir='$tomlCargo'", '--config', "build.build-dir='$tomlCargo'")
    if ($candidateMode) { Assert-CompatibilityCandidate }
    if ($PrepareVendoredCoreLock) {
        # WP-087: explicitly authorized source patch; preserve every other pin.
        $vendor = Join-Path $repoRoot 'product/vendor/surrealdb-core'
        Assert-PlainTree $vendor
        $productToml = [IO.File]::ReadAllText($manifest)
        $vendorToml = [IO.File]::ReadAllText((Join-Path $vendor 'Cargo.toml'))
        if ($productToml -notmatch '(?m)^surrealdb-core\s*=\s*\{\s*path\s*=\s*"vendor/surrealdb-core"\s*\}\s*$' -or
            $productToml -notmatch '(?m)^surrealdb\s*=\s*\{[^\r\n]*version\s*=\s*"=3\.2\.4"' -or
            $vendorToml -notmatch '(?m)^version\s*=\s*"3\.2\.4"\s*$') { throw 'Expected exact authorized production core 3.2.4 vendor patch.' }
        $productLock = Join-Path $repoRoot 'product/Cargo.lock'
        Assert-PlainPath $productLock
        $beforeLock = [IO.File]::ReadAllBytes($productLock)
        $before = @(Get-ProductLockIdentities $productLock)
        try {
            # Cargo update may unify unrelated Windows versions. Change only the
            # core source identity; locked metadata independently accepts the graph.
            $lockText = [Text.Encoding]::UTF8.GetString($beforeLock)
            $corePattern = '(?ms)(\[\[package\]\]\r?\nname = "surrealdb-core"\r?\nversion = "3\.2\.4"\r?\n)source = "registry\+https://github\.com/rust-lang/crates\.io-index"\r?\nchecksum = "8cd76c36f8b545a9371eec050bb2ae83750a6f60bae8c6d92d06956943348f4c"\r?\n'
            if ([regex]::Matches($lockText, $corePattern).Count -ne 1) { throw 'Expected exact upstream core lock identity.' }
            $localLock = [regex]::Replace($lockText, $corePattern, '$1')
            [IO.File]::WriteAllText($productLock, $localLock, (New-Object Text.UTF8Encoding($false)))
            $metadata = & cargo metadata --locked --offline --format-version 1 --manifest-path $manifest @pin
            if ($LASTEXITCODE -ne 0) { throw 'Locked metadata rejected vendored core substitution.' }
            if ([IO.File]::ReadAllText($productLock) -cne $localLock) { throw 'Metadata changed the prepared lock graph.' }
            $after = @(Get-ProductLockIdentities $productLock)
            $beforeOther = @($before | Where-Object { -not $_.StartsWith('surrealdb-core|') } | Sort-Object)
            $afterOther = @($after | Where-Object { -not $_.StartsWith('surrealdb-core|') } | Sort-Object)
            if (($beforeOther -join "`n") -cne ($afterOther -join "`n") -or
                @($after | Where-Object { $_ -ceq 'surrealdb-core|3.2.4||' }).Count -ne 1 -or
                @($after | Where-Object { $_.StartsWith('surrealdb-core|') }).Count -ne 1) {
                throw 'Vendor resolution changed another package identity or the exact core pin.'
            }
        } catch {
            [IO.File]::WriteAllBytes($productLock, $beforeLock)
            throw
        }
        Write-Output 'Vendored core lock prepared: core remains 3.2.4; all other package identities preserved.'
        return
    }
    if ($PrepareCompatibilityCandidateLock) {
        $candidateLock = Join-Path ([IO.Path]::GetDirectoryName($manifest)) 'Cargo.lock'
        if ([IO.File]::Exists($candidateLock)) {
            Assert-CompatibilityCandidate -RequireLock
            $beforeIdentities = @(Get-CandidateLockIdentities $candidateLock)
            # Workspace-only update retains existing dependency pins while resolving root changes.
            & cargo update --workspace --offline --manifest-path $manifest @pin
            if ($LASTEXITCODE -ne 0) { throw "Candidate lock refresh failed with exit code $LASTEXITCODE" }
            $afterIdentities = @(Get-CandidateLockIdentities $candidateLock)
            foreach ($identity in $beforeIdentities) {
                if ($afterIdentities -cnotcontains $identity) { throw "Candidate lock refresh changed an existing package identity: $identity" }
            }
            foreach ($identity in $afterIdentities) {
                $name = ($identity -split '\|', 2)[0]
                $existing = @($beforeIdentities | Where-Object { ($_ -split '\|', 2)[0] -ceq $name })
                if ($existing.Count -and $beforeIdentities -cnotcontains $identity) { throw "Candidate lock refresh added a different version/source of an existing package: $name" }
            }
        } else {
            & cargo generate-lockfile --manifest-path $manifest @pin
            if ($LASTEXITCODE -ne 0) { throw "Candidate lock preparation failed with exit code $LASTEXITCODE" }
        }
        Assert-CompatibilityCandidate -RequireLock
        Write-Output 'Candidate lock prepared; evaluation remains pending and no product engine decision was changed.'
        return
    }
    $metadataArgs = @('metadata', '--no-deps', '--offline', '--locked', '--format-version', '1', '--manifest-path', $manifest) + $pin
    $raw = & cargo @metadataArgs
    if ($LASTEXITCODE -ne 0) { throw "Cargo metadata failed with exit code $LASTEXITCODE" }
    $metadata = ($raw -join "`n") | ConvertFrom-Json
    foreach ($field in @('target_directory', 'build_directory')) {
        if (-not $metadata.$field -or [IO.Path]::GetFullPath($metadata.$field).TrimEnd('\', '/') -ine $cargoRoot) {
            throw "Cargo containment failed: $field=$($metadata.$field); expected $cargoRoot"
        }
    }
    if ($candidateMode) {
        $rootPackage = @($metadata.packages | Where-Object { [IO.Path]::GetFullPath($_.manifest_path) -ieq $manifest })
        $dependency = @($rootPackage.dependencies | Where-Object { $_.name -ceq 'surrealdb' })
        if ($rootPackage.Count -ne 1 -or $rootPackage[0].name -cne 'facial-engine-compatibility-candidate' -or
            [IO.Path]::GetFullPath($metadata.workspace_root).TrimEnd('\', '/') -ine [IO.Path]::GetDirectoryName($manifest) -or
            $dependency.Count -ne 1 -or $dependency[0].req -cne '=3.3.0' -or $dependency[0].path -or
            $dependency[0].source -cne 'registry+https://github.com/rust-lang/crates.io-index') { throw 'Candidate metadata does not bind the fixed manifest to exact surrealdb =3.3.0.' }
    }
    if ($Probe) {
        if ($candidateMode) {
            [ordered]@{ target_directory = $metadata.target_directory; build_directory = $metadata.build_directory; temporary_directory = $tempRoot; mutex = $mutexName; manifest = $manifest; compatibility_candidate = $true; evaluation_status = 'pending' } | ConvertTo-Json
            return
        }
        [ordered]@{ target_directory = $metadata.target_directory; build_directory = $metadata.build_directory; temporary_directory = $tempRoot; mutex = $mutexName } | ConvertTo-Json
        return
    }
    New-Item -ItemType Directory -Path $cargoRoot, $tempRoot -Force | Out-Null
    $command = $CargoArgs[0]
    $tail = @($CargoArgs | Select-Object -Skip 1)
    $separator = [Array]::IndexOf($tail, '--')
    $front = $tail
    $back = @()
    if ($separator -ge 0) {
        $front = @($tail | Select-Object -First $separator)
        $back = @($tail | Select-Object -Skip ($separator + 1))
    }
    $invokeArgs = @($command) + $front + @('--manifest-path', $manifest)
    if ($command -ne 'fmt') { $invokeArgs += $pin }
    if ($command -in @('build', 'check', 'test', 'run', 'clippy') -and -not ($front -match '^(-j|--jobs)')) {
        $invokeArgs += @('--jobs', '2')
    }
    if ($command -eq 'test' -and -not ($back -match '^--test-threads($|=)')) { $back += '--test-threads=1' }
    if ($separator -ge 0 -or $back.Count) { $invokeArgs += @('--') + $back }
    # The full trees were checked under this mutex before read-only metadata.
    Assert-PlainPath $cargoRoot
    Assert-PlainPath $tempRoot
    & cargo @invokeArgs
    if ($LASTEXITCODE -ne 0) { throw "Cargo $command failed with exit code $LASTEXITCODE" }
} finally {
    if ($pushed) { Pop-Location }
    foreach ($name in $saved.Keys) {
        if ($null -eq $saved[$name]) {
            Remove-Item -LiteralPath ("Env:" + $name) -ErrorAction SilentlyContinue
        } else {
            [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process')
        }
    }
    if ($held) { $mutex.ReleaseMutex() }
    $mutex.Dispose()
}
