<#
  retire-legacy-media-db.ps1  (WP-079)

  Safely retires only Facial's exact media-database paths from a selected
  workspace. It never deletes source state and never initializes a replacement
  database. Execute mode creates two verified copies outside live discovery:

    <workspace>/.facial-media-retirement/<run-id>/cold-backup/...
    <workspace>/.facial-media-retirement/<run-id>/quarantine/...

  The cold backup is copied first. Originals are then moved into quarantine.
  Exact retirement targets and Timeline artifacts retain deterministic
  per-file content SHA-256 records. Large protected workspace/cache/state trees
  use a bounded-memory metadata contract instead: relative path/type plus file
  size, UTC creation/last-write ticks, and attributes. Protected file contents
  are never read and protected per-file records are never serialized. Audit
  mode is read-only and emits one JSON value on stdout. Execute requires that
  Audit response's state-bound audit_token; a changed target or protected
  inventory invalidates the token before any retirement directory is created.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("Audit", "Execute")]
    [string]$Mode,

    [Parameter(Mandatory = $true)]
    [string]$WorkspaceRoot,

    [string]$TimelineRoot,

    [switch]$NoTimelineLedger,

    [switch]$ApprovePathOnlyLegacyFiles,

    [string]$AuditToken,

    [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9._-]*$')]
    [string]$RunId
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Get-NormalizedFullPath {
    param([Parameter(Mandatory = $true)][string]$Path)

    $full = [IO.Path]::GetFullPath($Path)
    $root = [IO.Path]::GetPathRoot($full)
    if ($full.Length -gt $root.Length) {
        $full = $full.TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    }
    return $full
}

function Test-SameOrDescendantPath {
    param(
        [Parameter(Mandatory = $true)][string]$Parent,
        [Parameter(Mandatory = $true)][string]$Candidate
    )

    $parentFull = Get-NormalizedFullPath -Path $Parent
    $candidateFull = Get-NormalizedFullPath -Path $Candidate
    if ($candidateFull.Equals($parentFull, [StringComparison]::OrdinalIgnoreCase)) {
        return $true
    }
    $prefix = $parentFull + [IO.Path]::DirectorySeparatorChar
    return $candidateFull.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)
}

function Assert-DescendantPath {
    param(
        [Parameter(Mandatory = $true)][string]$Parent,
        [Parameter(Mandatory = $true)][string]$Candidate,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $parentFull = Get-NormalizedFullPath -Path $Parent
    $candidateFull = Get-NormalizedFullPath -Path $Candidate
    if ($candidateFull.Equals($parentFull, [StringComparison]::OrdinalIgnoreCase) -or
        -not (Test-SameOrDescendantPath -Parent $parentFull -Candidate $candidateFull)) {
        throw "$Label must be a strict descendant of $parentFull; observed $candidateFull"
    }
}

function Convert-ToPortablePath {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$Path
    )

    $rootFull = Get-NormalizedFullPath -Path $Root
    $pathFull = Get-NormalizedFullPath -Path $Path
    Assert-DescendantPath -Parent $rootFull -Candidate $pathFull -Label "Portable path"
    return $pathFull.Substring($rootFull.Length + 1).Replace('\', '/')
}

function Assert-NoReparsePoint {
    param(
        [Parameter(Mandatory = $true)][string]$Workspace,
        [Parameter(Mandatory = $true)][string]$Path
    )

    $workspaceFull = Get-NormalizedFullPath -Path $Workspace
    $pathFull = Get-NormalizedFullPath -Path $Path
    Assert-DescendantPath -Parent $workspaceFull -Candidate $pathFull -Label "Inspected path"
    $relative = $pathFull.Substring($workspaceFull.Length + 1)
    $current = $workspaceFull
    foreach ($component in $relative.Split([IO.Path]::DirectorySeparatorChar)) {
        if ([string]::IsNullOrWhiteSpace($component)) { continue }
        $current = Join-Path $current $component
        if (-not (Test-Path -LiteralPath $current)) { break }
        $item = Get-Item -LiteralPath $current -Force
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Reparse points are not accepted in a WP-079 source or archive path: $current"
        }
    }
}

function Assert-FacialProcessesStopped {
    try {
        $running = @(
            Get-CimInstance -ClassName Win32_Process -ErrorAction Stop |
                Where-Object {
                    $_.Name -match '^(?i:facial|facial-cli|facial-portable-[0-9]+(?:\.[0-9]+)*)\.exe$'
                }
        )
    } catch {
        throw "Cannot prove that Facial processes are stopped: $($_.Exception.Message)"
    }
    if ($running.Count -gt 0) {
        $details = @(
            $running |
                Sort-Object ProcessId |
                ForEach-Object { "$($_.Name) pid=$($_.ProcessId)" }
        ) -join ", "
        throw "Refusing WP-079 media retirement while Facial is running: $details. Close it and retry; this tool never stops processes."
    }
}

function Get-Sha256Hex {
    param([Parameter(Mandatory = $true)][string]$Path)

    $stream = [IO.File]::Open(
        $Path,
        [IO.FileMode]::Open,
        [IO.FileAccess]::Read,
        [IO.FileShare]::Read
    )
    try {
        $sha = [Security.Cryptography.SHA256]::Create()
        try {
            return ([BitConverter]::ToString($sha.ComputeHash($stream))).Replace('-', '').ToLowerInvariant()
        } finally {
            $sha.Dispose()
        }
    } finally {
        $stream.Dispose()
    }
}

function Get-TextSha256Hex {
    param([Parameter(Mandatory = $true)][AllowEmptyString()][string]$Text)

    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [Text.UTF8Encoding]::new($false).GetBytes($Text)
        return ([BitConverter]::ToString($sha.ComputeHash($bytes))).Replace('-', '').ToLowerInvariant()
    } finally {
        $sha.Dispose()
    }
}

function Get-OrdinalDirectoryChildren {
    param([Parameter(Mandatory = $true)][string]$Directory)

    [object[]]$children = @(Get-ChildItem -LiteralPath $Directory -Force)
    [string[]]$keys = @($children | ForEach-Object { $_.Name })
    if ($children.Length -gt 1) {
        [Array]::Sort($keys, $children, [StringComparer]::Ordinal)
    }
    return $children
}

function Get-ArtifactSnapshot {
    param([Parameter(Mandatory = $true)][string]$Path)

    $full = Get-NormalizedFullPath -Path $Path
    $rootItem = Get-Item -LiteralPath $full -Force
    if (($rootItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "Refusing to inventory a reparse-point artifact: $full"
    }

    $filePaths = New-Object System.Collections.Generic.List[string]
    $directoryPaths = New-Object System.Collections.Generic.List[string]
    if ($rootItem.PSIsContainer) {
        $stack = New-Object System.Collections.Generic.Stack[string]
        $stack.Push($full)
        while ($stack.Count -gt 0) {
            $directory = $stack.Pop()
            foreach ($item in @(Get-ChildItem -LiteralPath $directory -Force)) {
                if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                    throw "Refusing to traverse a reparse point inside an artifact: $($item.FullName)"
                }
                $relative = $item.FullName.Substring($full.Length + 1).Replace('\', '/')
                if ($item.PSIsContainer) {
                    $directoryPaths.Add($relative)
                    $stack.Push($item.FullName)
                } else {
                    $filePaths.Add($item.FullName)
                }
            }
        }
        $kind = "directory"
    } else {
        $filePaths.Add($full)
        $kind = "file"
    }

    $filePathArray = [string[]]$filePaths.ToArray()
    [Array]::Sort($filePathArray, [StringComparer]::Ordinal)
    $directoryPathArray = [string[]]$directoryPaths.ToArray()
    [Array]::Sort($directoryPathArray, [StringComparer]::Ordinal)

    $files = New-Object System.Collections.Generic.List[object]
    $canonicalLines = New-Object System.Collections.Generic.List[string]
    foreach ($directoryPath in $directoryPathArray) {
        $encoded = [Convert]::ToBase64String([Text.UTF8Encoding]::new($false).GetBytes($directoryPath))
        $canonicalLines.Add("D`t$encoded")
    }

    [Int64]$totalBytes = 0
    foreach ($filePath in $filePathArray) {
        $file = Get-Item -LiteralPath $filePath -Force
        $relativePath = if ($kind -eq "file") {
            [IO.Path]::GetFileName($filePath)
        } else {
            $filePath.Substring($full.Length + 1).Replace('\', '/')
        }
        $sha256 = Get-Sha256Hex -Path $filePath
        $totalBytes += [Int64]$file.Length
        $files.Add([pscustomobject][ordered]@{
            relative_path = $relativePath
            size_bytes = [Int64]$file.Length
            sha256 = $sha256
        })
        $encoded = [Convert]::ToBase64String([Text.UTF8Encoding]::new($false).GetBytes($relativePath))
        $canonicalLines.Add("F`t$encoded`t$([Int64]$file.Length)`t$sha256")
    }

    $treeMaterial = [string]::Join("`n", $canonicalLines.ToArray())
    return [pscustomobject][ordered]@{
        snapshot_contract = "facial-wp079-exact-content-tree-v1"
        snapshot_mode = "exact-content-sha256"
        digest_algorithm = "sha256"
        kind = $kind
        size_bytes = $totalBytes
        file_count = $files.Count
        directory_count = $directoryPaths.Count
        tree_sha256 = Get-TextSha256Hex -Text $treeMaterial
        files = $files.ToArray()
    }
}

function Get-BoundedMetadataTreeSnapshot {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [string[]]$ExcludedRoots = @()
    )

    $rootFull = Get-NormalizedFullPath -Path $Root
    $rootItem = Get-Item -LiteralPath $rootFull -Force
    if (-not $rootItem.PSIsContainer) {
        throw "Protected inventory root must be a directory: $rootFull"
    }
    if (($rootItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "Protected inventory root cannot be a reparse point: $rootFull"
    }
    [string[]]$excluded = @(
        $ExcludedRoots |
            ForEach-Object { Get-NormalizedFullPath -Path $_ }
    )
    [Array]::Sort($excluded, [StringComparer]::OrdinalIgnoreCase)
    foreach ($excludedRoot in $excluded) {
        Assert-DescendantPath -Parent $rootFull -Candidate $excludedRoot -Label "Protected inventory exclusion"
    }

    $contract = "facial-wp079-bounded-metadata-tree-v1"
    $utf8 = [Text.UTF8Encoding]::new($false)
    $hasher = [Security.Cryptography.IncrementalHash]::CreateHash(
        [Security.Cryptography.HashAlgorithmName]::SHA256
    )
    try {
        $hasher.AppendData($utf8.GetBytes("H`t$contract`n"))
        $rootEncoded = [Convert]::ToBase64String($utf8.GetBytes("."))
        $hasher.AppendData($utf8.GetBytes("D`t$rootEncoded`n"))

        # Only names from one directory and pending siblings along the current
        # depth-first path are retained. There is no all-files path list,
        # canonical-line list, or per-file manifest material.
        $stack = New-Object System.Collections.Generic.Stack[object]
        [object[]]$rootChildren = @(Get-OrdinalDirectoryChildren -Directory $rootFull)
        for ($index = $rootChildren.Length - 1; $index -ge 0; $index--) {
            $rootChild = $rootChildren[$index]
            $stack.Push([pscustomobject]@{
                item = $rootChild
                relative_path = $rootChild.Name.Replace('\', '/')
            })
        }

        [Int64]$totalBytes = 0
        [Int64]$fileCount = 0
        [Int64]$directoryCount = 0
        while ($stack.Count -gt 0) {
            $entry = $stack.Pop()
            $item = $entry.item
            $itemFull = Get-NormalizedFullPath -Path $item.FullName
            $isExcluded = $false
            foreach ($excludedRoot in $excluded) {
                if (Test-SameOrDescendantPath -Parent $excludedRoot -Candidate $itemFull) {
                    $isExcluded = $true
                    break
                }
            }
            if ($isExcluded) { continue }
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Refusing to traverse a reparse point in protected inventory: $itemFull"
            }
            $relative = [string]$entry.relative_path
            $encoded = [Convert]::ToBase64String($utf8.GetBytes($relative))
            if ($item.PSIsContainer) {
                $directoryCount++
                $hasher.AppendData($utf8.GetBytes("D`t$encoded`n"))
                [object[]]$children = @(Get-OrdinalDirectoryChildren -Directory $itemFull)
                for ($index = $children.Length - 1; $index -ge 0; $index--) {
                    $child = $children[$index]
                    $stack.Push([pscustomobject]@{
                        item = $child
                        relative_path = "$relative/$($child.Name.Replace('\', '/'))"
                    })
                }
            } else {
                $fileCount++
                $length = [Int64]$item.Length
                $totalBytes += $length
                $creationTicks = [Int64]$item.CreationTimeUtc.Ticks
                $lastWriteTicks = [Int64]$item.LastWriteTimeUtc.Ticks
                $attributes = [Int64][int]$item.Attributes
                $hasher.AppendData($utf8.GetBytes(
                    "F`t$encoded`t$length`t$creationTicks`t$lastWriteTicks`t$attributes`n"
                ))
            }
        }
        $treeSha256 = ([BitConverter]::ToString($hasher.GetHashAndReset())).Replace('-', '').ToLowerInvariant()
        return [pscustomobject][ordered]@{
            snapshot_contract = $contract
            snapshot_mode = "bounded-metadata-tree"
            digest_algorithm = "sha256"
            kind = "directory"
            size_bytes = $totalBytes
            file_count = $fileCount
            directory_count = $directoryCount
            tree_sha256 = $treeSha256
        }
    } finally {
        $hasher.Dispose()
    }
}

function New-ProtectedInventoryRecord {
    param(
        [Parameter(Mandatory = $true)][string]$Id,
        [Parameter(Mandatory = $true)][string]$Type,
        [Parameter(Mandatory = $true)][string]$Path,
        [ValidateSet("file", "directory")]
        [string]$ExpectedKind,
        [switch]$BoundedMetadataTree,
        [string[]]$ExcludedRoots = @()
    )

    $canonical = Get-NormalizedFullPath -Path $Path
    $exists = Test-Path -LiteralPath $canonical
    $snapshot = $null
    if ($exists) {
        $item = Get-Item -LiteralPath $canonical -Force
        $observedKind = if ($item.PSIsContainer) { "directory" } else { "file" }
        if ($ExpectedKind -and $observedKind -ne $ExpectedKind) {
            throw "Protected inventory type is ambiguous at ${canonical}: expected $ExpectedKind, observed $observedKind."
        }
        $snapshot = if ($BoundedMetadataTree) {
            Get-BoundedMetadataTreeSnapshot -Root $canonical -ExcludedRoots $ExcludedRoots
        } else {
            Get-ArtifactSnapshot -Path $canonical
        }
    }
    [string[]]$normalizedExclusions = @(
        $ExcludedRoots | ForEach-Object { Get-NormalizedFullPath -Path $_ }
    )
    [Array]::Sort($normalizedExclusions, [StringComparer]::OrdinalIgnoreCase)
    return [pscustomobject][ordered]@{
        id = $Id
        type = $Type
        canonical_path = $canonical
        snapshot_contract = if ($BoundedMetadataTree) { "facial-wp079-bounded-metadata-tree-v1" } else { "facial-wp079-exact-content-tree-v1" }
        snapshot_mode = if ($BoundedMetadataTree) { "bounded-metadata-tree" } else { "exact-content-sha256" }
        digest_algorithm = "sha256"
        excluded_roots = $normalizedExclusions
        before_exists = [bool]$exists
        before = $snapshot
        after_exists = $null
        after = $null
        unchanged = $null
    }
}

function Set-ProtectedInventoriesAfter {
    param([Parameter(Mandatory = $true)][object[]]$Inventories)

    foreach ($inventory in $Inventories) {
        $exists = Test-Path -LiteralPath $inventory.canonical_path
        $inventory.after_exists = [bool]$exists
        if ($exists) {
            $inventory.after = if ($inventory.snapshot_mode -eq "bounded-metadata-tree") {
                Get-BoundedMetadataTreeSnapshot -Root $inventory.canonical_path -ExcludedRoots @($inventory.excluded_roots)
            } else {
                Get-ArtifactSnapshot -Path $inventory.canonical_path
            }
        }
        if ($inventory.before_exists -ne $inventory.after_exists) {
            $inventory.unchanged = $false
            throw "Protected inventory existence changed: $($inventory.canonical_path)"
        }
        if ($inventory.before_exists) {
            try {
                Assert-SnapshotEqual -Expected $inventory.before -Observed $inventory.after -Label "Protected inventory $($inventory.id)"
                $inventory.unchanged = $true
            } catch {
                $inventory.unchanged = $false
                throw
            }
        } else {
            $inventory.unchanged = $true
        }
    }
}

function Assert-SnapshotEqual {
    param(
        [Parameter(Mandatory = $true)]$Expected,
        [Parameter(Mandatory = $true)]$Observed,
        [Parameter(Mandatory = $true)][string]$Label
    )

    foreach ($property in @("snapshot_contract", "snapshot_mode", "digest_algorithm", "kind", "size_bytes", "file_count", "directory_count", "tree_sha256")) {
        if ($Expected.$property -ne $Observed.$property) {
            throw "$Label verification failed: $property expected '$($Expected.$property)', observed '$($Observed.$property)'."
        }
    }
}

function Assert-TargetSetMatchesSnapshots {
    param(
        [Parameter(Mandatory = $true)][string]$Workspace,
        [Parameter(Mandatory = $true)][object[]]$Targets,
        [Parameter(Mandatory = $true)][string]$Phase
    )

    foreach ($target in $Targets) {
        Assert-NoReparsePoint -Workspace $Workspace -Path $target.canonical_path
        $observedExists = Test-Path -LiteralPath $target.canonical_path
        if ([bool]$target.exists -ne [bool]$observedExists) {
            throw "Exact target existence changed during ${Phase}: $($target.canonical_path) (expected $([bool]$target.exists), observed $([bool]$observedExists))."
        }
        if ($observedExists) {
            $observed = Get-ArtifactSnapshot -Path $target.canonical_path
            Assert-SnapshotEqual -Expected $target.snapshot -Observed $observed -Label "Exact target $($target.id) during $Phase"
        }
    }
}

function Assert-AllTargetsAbsent {
    param(
        [Parameter(Mandatory = $true)][string]$Workspace,
        [Parameter(Mandatory = $true)][object[]]$Targets,
        [Parameter(Mandatory = $true)][string]$Phase
    )

    foreach ($target in $Targets) {
        Assert-NoReparsePoint -Workspace $Workspace -Path $target.canonical_path
        if (Test-Path -LiteralPath $target.canonical_path) {
            throw "An exact retired target is present during ${Phase}: $($target.canonical_path)"
        }
    }
}

function Invoke-TestOnlyDelay {
    param([Parameter(Mandatory = $true)][string]$Phase)

    if ([string]::IsNullOrWhiteSpace($env:FACIAL_WP079_TEST_DELAY_PHASE) -or
        -not $env:FACIAL_WP079_TEST_DELAY_PHASE.Equals($Phase, [StringComparison]::Ordinal)) {
        return
    }
    [int]$milliseconds = 0
    if (-not [int]::TryParse($env:FACIAL_WP079_TEST_DELAY_MS, [ref]$milliseconds) -or
        $milliseconds -lt 1 -or
        $milliseconds -gt 30000) {
        throw "FACIAL_WP079_TEST_DELAY_MS must be an integer from 1 through 30000."
    }
    # Failure-path concurrency seam only: it delays a proof gate but cannot
    # approve, mutate, move, or delete a target.
    Start-Sleep -Milliseconds $milliseconds
}

function Assert-ArtifactUnlocked {
    param([Parameter(Mandatory = $true)][string]$Path)

    $snapshot = Get-ArtifactSnapshot -Path $Path
    $full = Get-NormalizedFullPath -Path $Path
    foreach ($file in @($snapshot.files)) {
        $filePath = if ($snapshot.kind -eq "file") {
            $full
        } else {
            Join-Path $full $file.relative_path.Replace('/', '\')
        }
        try {
            $stream = [IO.File]::Open(
                $filePath,
                [IO.FileMode]::Open,
                [IO.FileAccess]::Read,
                [IO.FileShare]::None
            )
            $stream.Dispose()
        } catch {
            throw "Cannot acquire an exclusive read of $filePath; the media store may still be open: $($_.Exception.Message)"
        }
    }
}

function Assert-CurrentEnginePair {
    param(
        [Parameter(Mandatory = $true)][string]$DatabaseRoot,
        [Parameter(Mandatory = $true)][string]$MarkerPath
    )

    $databaseExists = Test-Path -LiteralPath $DatabaseRoot
    $markerExists = Test-Path -LiteralPath $MarkerPath
    if ($databaseExists -xor $markerExists) {
        throw "Ambiguous current media state: surrealdb directory and engine.json must either both exist or both be absent."
    }
    if (-not $databaseExists) { return }
    if (-not (Test-Path -LiteralPath $DatabaseRoot -PathType Container)) {
        throw "Current media SurrealDB path is not a directory: $DatabaseRoot"
    }
    if (-not (Test-Path -LiteralPath $MarkerPath -PathType Leaf)) {
        throw "Current media engine marker is not a file: $MarkerPath"
    }
    try {
        $marker = Get-Content -LiteralPath $MarkerPath -Raw | ConvertFrom-Json
    } catch {
        throw "Current media engine marker is not valid JSON: $MarkerPath ($($_.Exception.Message))"
    }
    [UInt64]$schemaVersion = 0
    $schemaIsNumeric = $marker.schema_version -is [ValueType] -and
        [UInt64]::TryParse([string]$marker.schema_version, [ref]$schemaVersion) -and
        $schemaVersion -gt 0
    if ($marker.engine -ne "surrealdb" -or
        $marker.namespace -ne "facial" -or
        $marker.database -ne "application" -or
        ([string]$marker.engine_version) -notmatch '^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$' -or
        -not $schemaIsNumeric) {
        throw "Current media engine marker does not identify Facial's SurrealDB application store: $MarkerPath"
    }
}

function Copy-VerifiedArtifact {
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Destination,
        [Parameter(Mandatory = $true)]$ExpectedSnapshot
    )

    if (Test-Path -LiteralPath $Destination) {
        throw "Backup destination already exists: $Destination"
    }
    $parent = Split-Path -Parent $Destination
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
    if ($ExpectedSnapshot.kind -eq "file") {
        Copy-Item -LiteralPath $Source -Destination $Destination
    } else {
        New-Item -ItemType Directory -Path $Destination | Out-Null
        foreach ($file in @($ExpectedSnapshot.files)) {
            $destinationFile = Join-Path $Destination $file.relative_path.Replace('/', '\')
            New-Item -ItemType Directory -Force -Path (Split-Path -Parent $destinationFile) | Out-Null
            Copy-Item -LiteralPath (Join-Path $Source $file.relative_path.Replace('/', '\')) -Destination $destinationFile
        }
        $sourceDirectories = New-Object System.Collections.Generic.Stack[string]
        $sourceDirectories.Push((Get-NormalizedFullPath -Path $Source))
        while ($sourceDirectories.Count -gt 0) {
            $sourceDirectory = $sourceDirectories.Pop()
            foreach ($child in @(Get-ChildItem -LiteralPath $sourceDirectory -Force | Where-Object { $_.PSIsContainer })) {
                if (($child.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                    throw "Refusing a reparse point discovered during copy: $($child.FullName)"
                }
                $relativeDirectory = $child.FullName.Substring((Get-NormalizedFullPath -Path $Source).Length + 1)
                New-Item -ItemType Directory -Force -Path (Join-Path $Destination $relativeDirectory) | Out-Null
                $sourceDirectories.Push($child.FullName)
            }
        }
    }
    $observed = Get-ArtifactSnapshot -Path $Destination
    Assert-SnapshotEqual -Expected $ExpectedSnapshot -Observed $observed -Label "Cold backup $Destination"
}

function Move-VerifiedArtifact {
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Destination,
        [Parameter(Mandatory = $true)]$ExpectedSnapshot,
        [Parameter(Mandatory = $true)][string]$ArtifactId
    )

    if (Test-Path -LiteralPath $Destination) {
        throw "Quarantine destination already exists: $Destination"
    }
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Destination) | Out-Null
    Move-Item -LiteralPath $Source -Destination $Destination
    # Failure-only test seam for the rollback negative path. It cannot bypass
    # a guard or approve a target; setting it only forces Execute to abort.
    if ($env:FACIAL_WP079_TEST_FAIL_AFTER_MOVE_ID -eq $ArtifactId) {
        throw "Injected WP-079 post-move verification failure for $ArtifactId."
    }
    if (Test-Path -LiteralPath $Source) {
        throw "Move left the live source in place: $Source"
    }
    $observed = Get-ArtifactSnapshot -Path $Destination
    Assert-SnapshotEqual -Expected $ExpectedSnapshot -Observed $observed -Label "Quarantine $Destination"
}

function Write-ManifestAtomic {
    param(
        [Parameter(Mandatory = $true)]$Manifest,
        [Parameter(Mandatory = $true)][string]$Path
    )

    $json = $Manifest | ConvertTo-Json -Depth 20
    $temporary = "$Path.next"
    [IO.File]::WriteAllText($temporary, $json + "`r`n", [Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $temporary -Destination $Path -Force
}

function Get-AuditToken {
    param(
        [Parameter(Mandatory = $true)][string]$Workspace,
        [AllowNull()][string]$ResolvedTimelineRoot,
        [Parameter(Mandatory = $true)][string]$TimelineProof,
        [Parameter(Mandatory = $true)][object[]]$Targets,
        [Parameter(Mandatory = $true)][object[]]$ProtectedInventories
    )

    $targetProof = @(
        foreach ($target in $Targets) {
            [pscustomobject][ordered]@{
                id = $target.id
                relative_path = $target.relative_path
                canonical_path = $target.canonical_path
                type = $target.type
                ownership_verdict = $target.ownership_verdict
                exists = [bool]$target.exists
                snapshot_contract = if ($null -eq $target.snapshot) { "facial-wp079-exact-content-tree-v1" } else { $target.snapshot.snapshot_contract }
                snapshot_mode = if ($null -eq $target.snapshot) { "exact-content-sha256" } else { $target.snapshot.snapshot_mode }
                digest_algorithm = "sha256"
                kind = if ($null -eq $target.snapshot) { $null } else { $target.snapshot.kind }
                size_bytes = if ($null -eq $target.snapshot) { $null } else { $target.snapshot.size_bytes }
                file_count = if ($null -eq $target.snapshot) { $null } else { $target.snapshot.file_count }
                directory_count = if ($null -eq $target.snapshot) { $null } else { $target.snapshot.directory_count }
                tree_sha256 = if ($null -eq $target.snapshot) { $null } else { $target.snapshot.tree_sha256 }
            }
        }
    )
    $protectedProof = @(
        foreach ($inventory in $ProtectedInventories) {
            [pscustomobject][ordered]@{
                id = $inventory.id
                type = $inventory.type
                canonical_path = $inventory.canonical_path
                snapshot_contract = $inventory.snapshot_contract
                snapshot_mode = $inventory.snapshot_mode
                digest_algorithm = $inventory.digest_algorithm
                excluded_roots = @($inventory.excluded_roots)
                exists = [bool]$inventory.before_exists
                observed_snapshot_contract = if ($null -eq $inventory.before) { $null } else { $inventory.before.snapshot_contract }
                observed_snapshot_mode = if ($null -eq $inventory.before) { $null } else { $inventory.before.snapshot_mode }
                observed_digest_algorithm = if ($null -eq $inventory.before) { $null } else { $inventory.before.digest_algorithm }
                kind = if ($null -eq $inventory.before) { $null } else { $inventory.before.kind }
                size_bytes = if ($null -eq $inventory.before) { $null } else { $inventory.before.size_bytes }
                file_count = if ($null -eq $inventory.before) { $null } else { $inventory.before.file_count }
                directory_count = if ($null -eq $inventory.before) { $null } else { $inventory.before.directory_count }
                tree_sha256 = if ($null -eq $inventory.before) { $null } else { $inventory.before.tree_sha256 }
            }
        }
    )
    $receipt = [pscustomobject][ordered]@{
        schema = "facial-wp079-audit-token-v2"
        workspace_root = $Workspace
        timeline_root = $ResolvedTimelineRoot
        timeline_proof = $TimelineProof
        targets = $targetProof
        protected_inventories = $protectedProof
    }
    return Get-TextSha256Hex -Text ($receipt | ConvertTo-Json -Depth 20 -Compress)
}

if ($env:OS -ne "Windows_NT") {
    throw "WP-079 media retirement is a Windows-only operator tool."
}
if ($Mode -eq "Execute" -and [string]::IsNullOrWhiteSpace($AuditToken)) {
    throw "Execute requires the matching audit_token emitted by a prior -Mode Audit run."
}
if ($Mode -eq "Audit" -and -not [string]::IsNullOrWhiteSpace($AuditToken)) {
    throw "-AuditToken is an Execute-only parameter."
}

$resolvedWorkspace = Resolve-Path -LiteralPath $WorkspaceRoot -ErrorAction Stop
$workspace = Get-NormalizedFullPath -Path $resolvedWorkspace.Path
if (-not (Test-Path -LiteralPath $workspace -PathType Container)) {
    throw "WorkspaceRoot must identify an existing directory: $workspace"
}
if ($workspace.Equals([IO.Path]::GetPathRoot($workspace), [StringComparison]::OrdinalIgnoreCase)) {
    throw "WorkspaceRoot cannot be a filesystem volume root: $workspace"
}
$workspaceItem = Get-Item -LiteralPath $workspace -Force
if (($workspaceItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
    throw "WorkspaceRoot cannot be a reparse point: $workspace"
}

$facialRoot = Join-Path $workspace ".facial"
$mediaRoot = Join-Path $facialRoot "media"
$currentDatabase = Join-Path $mediaRoot "surrealdb"
$currentMarker = Join-Path $mediaRoot "engine.json"
$retirementRoot = Join-Path $workspace ".facial-media-retirement"

Assert-DescendantPath -Parent $workspace -Candidate $facialRoot -Label "Facial state root"
Assert-DescendantPath -Parent $workspace -Candidate $mediaRoot -Label "Media state root"
Assert-DescendantPath -Parent $workspace -Candidate $retirementRoot -Label "Retirement root"
if (Test-SameOrDescendantPath -Parent $mediaRoot -Candidate $retirementRoot) {
    throw "Retirement root must remain outside live media discovery: $retirementRoot"
}
if (Test-SameOrDescendantPath -Parent $facialRoot -Candidate $retirementRoot) {
    throw "Retirement root must remain outside the installer cleanup boundary: $retirementRoot"
}
Assert-NoReparsePoint -Workspace $workspace -Path $facialRoot
Assert-NoReparsePoint -Workspace $workspace -Path $mediaRoot
Assert-NoReparsePoint -Workspace $workspace -Path $retirementRoot
Assert-FacialProcessesStopped
Assert-CurrentEnginePair -DatabaseRoot $currentDatabase -MarkerPath $currentMarker

$targetSpecs = @(
    [pscustomobject][ordered]@{ id = "current-engine-marker"; relative_path = ".facial/media/engine.json"; type = "engine-marker-json"; role = "current-media-application-store"; expected_kind = "file" },
    [pscustomobject][ordered]@{ id = "current-surrealdb"; relative_path = ".facial/media/surrealdb"; type = "surrealdb-directory"; role = "current-media-application-store"; expected_kind = "directory" },
    [pscustomobject][ordered]@{ id = "legacy-media-redb"; relative_path = ".facial/media/media.redb"; type = "redb-file"; role = "legacy-media-state"; expected_kind = "file" },
    [pscustomobject][ordered]@{ id = "legacy-inventory-redb"; relative_path = ".facial/media/inventory.redb"; type = "redb-file"; role = "legacy-media-state"; expected_kind = "file" },
    [pscustomobject][ordered]@{ id = "legacy-clip-redb"; relative_path = ".facial/media/clip_index.redb"; type = "redb-file"; role = "legacy-media-state"; expected_kind = "file" },
    [pscustomobject][ordered]@{ id = "legacy-media-json"; relative_path = ".facial/media_metadata.json"; type = "legacy-json-file"; role = "legacy-media-state"; expected_kind = "file" },
    [pscustomobject][ordered]@{ id = "legacy-media-json-migrated"; relative_path = ".facial/media_metadata.json.migrated"; type = "legacy-json-file"; role = "legacy-media-state"; expected_kind = "file" }
)

$targets = New-Object System.Collections.Generic.List[object]
foreach ($spec in $targetSpecs) {
    $canonical = Get-NormalizedFullPath -Path (Join-Path $workspace $spec.relative_path.Replace('/', '\'))
    Assert-DescendantPath -Parent $workspace -Candidate $canonical -Label "Media retirement source"
    Assert-NoReparsePoint -Workspace $workspace -Path $canonical
    $exists = Test-Path -LiteralPath $canonical
    $snapshot = $null
    if ($exists) {
        $item = Get-Item -LiteralPath $canonical -Force
        $observedKind = if ($item.PSIsContainer) { "directory" } else { "file" }
        if ($observedKind -ne $spec.expected_kind) {
            throw "Ambiguous target type at ${canonical}: expected $($spec.expected_kind), observed $observedKind."
        }
        Assert-ArtifactUnlocked -Path $canonical
        $snapshot = Get-ArtifactSnapshot -Path $canonical
    }
    $targets.Add([pscustomobject][ordered]@{
        id = $spec.id
        relative_path = $spec.relative_path
        canonical_path = $canonical
        type = $spec.type
        role = $spec.role
        ownership_verdict = if (-not $exists) {
            "exact-candidate-path-absent"
        } elseif ($spec.role -eq "current-media-application-store") {
            "marker-verified-facial-surrealdb-application-store"
        } else {
            "path-only-ambiguous"
        }
        exists = [bool]$exists
        live_discovery = [bool]$exists
        snapshot = $snapshot
        cold_backup_path = $null
        cold_backup_status = if ($exists) { "not-started" } else { "not-applicable" }
        quarantine_path = $null
        quarantine_status = if ($exists) { "not-started" } else { "not-applicable" }
        final_disposition = if ($exists) { "present-live" } else { "absent" }
    })
}

$ambiguousLegacyTargets = @(
    $targets | Where-Object { $_.exists -and $_.ownership_verdict -eq "path-only-ambiguous" }
)
if ($Mode -eq "Execute" -and $ambiguousLegacyTargets.Count -gt 0 -and -not $ApprovePathOnlyLegacyFiles) {
    throw "Path-only legacy candidates require explicit -ApprovePathOnlyLegacyFiles before Execute: $(@($ambiguousLegacyTargets.id) -join ', '). Audit and inspect their hashes first."
}

$resolvedTimelineRoot = $null
if (-not [string]::IsNullOrWhiteSpace($TimelineRoot)) {
    $resolvedTimeline = Resolve-Path -LiteralPath $TimelineRoot -ErrorAction Stop
    $resolvedTimelineRoot = Get-NormalizedFullPath -Path $resolvedTimeline.Path
    if (-not (Test-Path -LiteralPath $resolvedTimelineRoot -PathType Container)) {
        throw "TimelineRoot must identify an existing directory: $resolvedTimelineRoot"
    }
} elseif (Test-Path -LiteralPath (Join-Path $workspace "timeline-maintenance.yaml") -PathType Leaf) {
    $resolvedTimelineRoot = $workspace
}
if ($null -ne $resolvedTimelineRoot -and
    -not (Test-Path -LiteralPath (Join-Path $resolvedTimelineRoot "timeline-maintenance.yaml") -PathType Leaf)) {
    throw "TimelineRoot is not anchor-verified; timeline-maintenance.yaml is missing: $resolvedTimelineRoot"
}
if ($NoTimelineLedger -and $null -ne $resolvedTimelineRoot) {
    throw "-NoTimelineLedger conflicts with the anchored Timeline project at $resolvedTimelineRoot."
}
if ($Mode -eq "Execute" -and $null -eq $resolvedTimelineRoot -and -not $NoTimelineLedger) {
    throw "Execute requires an anchor-verified -TimelineRoot, or explicit -NoTimelineLedger when no Timeline ledger is in scope."
}

$protectedInventories = New-Object System.Collections.Generic.List[object]
$protectedInventories.Add((New-ProtectedInventoryRecord `
    -Id "raw-workspace" `
    -Type "raw-workspace" `
    -Path $workspace `
    -ExpectedKind "directory" `
    -BoundedMetadataTree `
    -ExcludedRoots @($facialRoot, $retirementRoot)))
$thumbnailCacheRoot = Join-Path $mediaRoot "thumbs"
$protectedInventories.Add((New-ProtectedInventoryRecord `
    -Id "thumbnail-cache" `
    -Type "thumbnail-cache" `
    -Path $thumbnailCacheRoot `
    -ExpectedKind "directory" `
    -BoundedMetadataTree))

$facialStateExclusions = @($targets | ForEach-Object { $_.canonical_path }) + @($thumbnailCacheRoot)
$protectedInventories.Add((New-ProtectedInventoryRecord `
    -Id "facial-unrelated-state" `
    -Type "facial-unrelated-state" `
    -Path $facialRoot `
    -ExpectedKind "directory" `
    -BoundedMetadataTree `
    -ExcludedRoots $facialStateExclusions))

if ($null -ne $resolvedTimelineRoot) {
    $timelineSpecs = @(
        [pscustomobject]@{ id = "timeline-anchor"; relative_path = "timeline-maintenance.yaml"; type = "timeline-anchor"; expected_kind = "file" },
        [pscustomobject]@{ id = "timeline-event-registry"; relative_path = "timeline-data/event-registry.jsonl"; type = "timeline-canonical-registry"; expected_kind = "file" },
        [pscustomobject]@{ id = "timeline-artifact-registry"; relative_path = "timeline-data/artifact-registry.jsonl"; type = "timeline-canonical-registry"; expected_kind = "file" },
        [pscustomobject]@{ id = "timeline-source-registry"; relative_path = "timeline-data/source-registry.jsonl"; type = "timeline-canonical-registry"; expected_kind = "file" },
        [pscustomobject]@{ id = "timeline-planned-events"; relative_path = "timeline-data/planned-events.jsonl"; type = "timeline-canonical-registry"; expected_kind = "file" },
        [pscustomobject]@{ id = "timeline-surrealdb"; relative_path = ".facial/timeline-ledger/surrealdb"; type = "timeline-surrealdb-directory"; expected_kind = "directory" },
        [pscustomobject]@{ id = "timeline-engine-marker"; relative_path = ".facial/timeline-ledger/engine.json"; type = "timeline-engine-marker"; expected_kind = "file" }
    )
    foreach ($timelineSpec in $timelineSpecs) {
        $protectedInventories.Add((New-ProtectedInventoryRecord `
            -Id $timelineSpec.id `
            -Type $timelineSpec.type `
            -Path (Join-Path $resolvedTimelineRoot $timelineSpec.relative_path.Replace('/', '\')) `
            -ExpectedKind $timelineSpec.expected_kind))
    }
}

$timelineProof = if ($null -ne $resolvedTimelineRoot) {
    "anchor-verified-and-hash-reconciled"
} elseif ($NoTimelineLedger) {
    "explicit-no-timeline-ledger-acknowledgement"
} else {
    "not-provided-for-audit"
}

$manifest = [pscustomobject][ordered]@{
    schema_version = 1
    workpacket_id = "WP-079"
    operation = "facial-media-database-retirement"
    mode = $Mode.ToLowerInvariant()
    status = if ($Mode -eq "Audit") { "audited" } else { "preflight-verified" }
    created_utc = [DateTime]::UtcNow.ToString("o")
    completed_utc = $null
    workspace_root = $workspace
    timeline_root = $resolvedTimelineRoot
    timeline_proof = $timelineProof
    live_media_root = $mediaRoot
    retirement_root = $retirementRoot
    run_id = if ($Mode -eq "Audit") { $null } else { $RunId }
    manifest_path = $null
    runtime_initialization = "not-performed-by-this-tool"
    source_deletion = "never"
    snapshot_contracts = [pscustomobject][ordered]@{
        exact_artifacts = [pscustomobject][ordered]@{
            contract = "facial-wp079-exact-content-tree-v1"
            mode = "exact-content-sha256"
            digest_algorithm = "sha256"
            manifest_rows = "per-file"
        }
        protected_trees = [pscustomobject][ordered]@{
            contract = "facial-wp079-bounded-metadata-tree-v1"
            mode = "bounded-metadata-tree"
            digest_algorithm = "sha256"
            directory_fields = @("relative_path", "type")
            file_fields = @("relative_path", "type", "size_bytes", "creation_utc_ticks", "last_write_utc_ticks", "attributes")
            manifest_rows = "aggregate-only"
        }
    }
    legacy_path_only_approval = [bool]$ApprovePathOnlyLegacyFiles
    audit_token = $null
    protected_inventories = $protectedInventories.ToArray()
    targets = $targets.ToArray()
    rollback = [pscustomobject][ordered]@{ attempted = $false; complete = $null; details = @() }
    error = $null
}

# The target paths are excluded from protected-tree inventories. Reconcile
# every present and absent exact candidate after those potentially long scans,
# before either Audit certifies the state or Execute accepts its token.
Assert-TargetSetMatchesSnapshots -Workspace $workspace -Targets @($manifest.targets) -Phase "$($Mode.ToLowerInvariant()) pre-token reconciliation"

$computedAuditToken = Get-AuditToken `
    -Workspace $workspace `
    -ResolvedTimelineRoot $resolvedTimelineRoot `
    -TimelineProof $timelineProof `
    -Targets @($manifest.targets) `
    -ProtectedInventories @($manifest.protected_inventories)
$manifest.audit_token = $computedAuditToken
if ($Mode -eq "Execute") {
    if ($AuditToken -notmatch '^[0-9a-fA-F]{64}$' -or
        -not $computedAuditToken.Equals($AuditToken, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Audit token mismatch: rerun -Mode Audit with the same WorkspaceRoot and Timeline choice, inspect it, and pass its audit_token without changing state."
    }
    Assert-TargetSetMatchesSnapshots -Workspace $workspace -Targets @($manifest.targets) -Phase "execute token acceptance"
}

if ($Mode -eq "Audit") {
    $manifest.completed_utc = [DateTime]::UtcNow.ToString("o")
    Write-Output ($manifest | ConvertTo-Json -Depth 20)
    return
}

if ([string]::IsNullOrWhiteSpace($RunId)) {
    $RunId = "wp-079-$([DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ'))-$([Guid]::NewGuid().ToString('N').Substring(0, 8))"
}
if ($RunId -eq "." -or $RunId -eq "..") {
    throw "RunId cannot be '.' or '..'."
}
if ($RunId.EndsWith('.') -or $RunId -match '^(?i:con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)') {
    throw "RunId is not a portable Windows filename: $RunId"
}
$runRoot = Get-NormalizedFullPath -Path (Join-Path $retirementRoot $RunId)
Assert-DescendantPath -Parent $retirementRoot -Candidate $runRoot -Label "Retirement run root"
Assert-DescendantPath -Parent $workspace -Candidate $runRoot -Label "Retirement run root"
if (Test-Path -LiteralPath $runRoot) {
    if (@($manifest.targets | Where-Object { $_.exists }).Count -gt 0) {
        throw "Retirement RunId already exists while a live media target is present; refusing to reuse it: $runRoot"
    }
    Assert-NoReparsePoint -Workspace $workspace -Path $runRoot
    $existingManifestPath = Join-Path $runRoot "manifest.json"
    if (-not (Test-Path -LiteralPath $existingManifestPath -PathType Leaf)) {
        throw "Existing retirement RunId has no manifest and is not safely resumable: $runRoot"
    }
    try {
        $existingManifest = Get-Content -LiteralPath $existingManifestPath -Raw | ConvertFrom-Json
    } catch {
        throw "Existing retirement manifest is invalid JSON: $existingManifestPath ($($_.Exception.Message))"
    }
    if ($existingManifest.schema_version -ne 1 -or
        $existingManifest.operation -ne "facial-media-database-retirement" -or
        $existingManifest.mode -ne "execute" -or
        $existingManifest.status -ne "ready-for-clean-initialization" -or
        $existingManifest.run_id -ne $RunId -or
        -not ([string]$existingManifest.workspace_root).Equals($workspace, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Existing retirement RunId is incomplete or belongs to another operation/workspace: $existingManifestPath"
    }
    if (-not ([string]$existingManifest.manifest_path).Equals($existingManifestPath, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Existing retirement manifest path does not match its RunId location: $existingManifestPath"
    }
    if (@($existingManifest.targets).Count -ne @($manifest.targets).Count) {
        throw "Existing retirement manifest does not contain the complete exact target inventory."
    }
    foreach ($knownTarget in @($manifest.targets)) {
        $recordedTargets = @($existingManifest.targets | Where-Object { $_.id -eq $knownTarget.id })
        if ($recordedTargets.Count -ne 1 -or
            $recordedTargets[0].relative_path -ne $knownTarget.relative_path -or
            -not ([string]$recordedTargets[0].canonical_path).Equals([string]$knownTarget.canonical_path, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Existing retirement manifest target inventory is mismatched: $($knownTarget.id)"
        }
    }
    foreach ($existingTarget in @($existingManifest.targets | Where-Object { $_.exists })) {
        $knownTarget = @($manifest.targets | Where-Object { $_.id -eq $existingTarget.id })
        if ($knownTarget.Count -ne 1 -or
            $knownTarget[0].relative_path -ne $existingTarget.relative_path -or
            -not ([string]$knownTarget[0].canonical_path).Equals([string]$existingTarget.canonical_path, [StringComparison]::OrdinalIgnoreCase) -or
            $existingTarget.cold_backup_status -ne "verified" -or
            $existingTarget.quarantine_status -ne "verified" -or
            $existingTarget.final_disposition -ne "quarantined-with-verified-cold-backup") {
            throw "Existing retirement target contract is incomplete or mismatched: $($existingTarget.id)"
        }
        $expectedCold = Get-NormalizedFullPath -Path (Join-Path (Join-Path $runRoot "cold-backup") $existingTarget.relative_path.Replace('/', '\'))
        $expectedQuarantine = Get-NormalizedFullPath -Path (Join-Path (Join-Path $runRoot "quarantine") $existingTarget.relative_path.Replace('/', '\'))
        $copyPairs = @(
            [pscustomobject]@{ observed = $existingTarget.cold_backup_path; expected = $expectedCold },
            [pscustomobject]@{ observed = $existingTarget.quarantine_path; expected = $expectedQuarantine }
        )
        foreach ($copyPair in $copyPairs) {
            $copyPath = $copyPair.observed
            if ([string]::IsNullOrWhiteSpace([string]$copyPath)) {
                throw "Existing retirement manifest is missing a preserved-copy path for $($existingTarget.id)."
            }
            $copyFull = Get-NormalizedFullPath -Path $copyPath
            if (-not $copyFull.Equals($copyPair.expected, [StringComparison]::OrdinalIgnoreCase)) {
                throw "Existing retirement copy is outside its exact planned location: $copyFull"
            }
            Assert-DescendantPath -Parent $runRoot -Candidate $copyFull -Label "Existing preserved copy"
            if (-not (Test-Path -LiteralPath $copyFull)) {
                throw "Existing retirement copy is missing: $copyFull"
            }
            $copySnapshot = Get-ArtifactSnapshot -Path $copyFull
            Assert-SnapshotEqual -Expected $existingTarget.snapshot -Observed $copySnapshot -Label "Existing preserved copy $copyFull"
        }
    }
    $requiredProtectedIds = @("raw-workspace", "thumbnail-cache", "facial-unrelated-state")
    if ($null -ne $existingManifest.timeline_root) {
        $requiredProtectedIds += @(
            "timeline-anchor",
            "timeline-event-registry",
            "timeline-artifact-registry",
            "timeline-source-registry",
            "timeline-planned-events",
            "timeline-surrealdb",
            "timeline-engine-marker"
        )
    } elseif ($existingManifest.timeline_proof -ne "explicit-no-timeline-ledger-acknowledgement") {
        throw "Existing retirement manifest has neither anchored Timeline proof nor the explicit no-ledger acknowledgement."
    }
    foreach ($requiredProtectedId in $requiredProtectedIds) {
        if (@($existingManifest.protected_inventories | Where-Object { $_.id -eq $requiredProtectedId }).Count -ne 1) {
            throw "Existing retirement manifest is missing protected inventory: $requiredProtectedId"
        }
    }
    foreach ($protected in @($existingManifest.protected_inventories)) {
        if (-not $protected.unchanged -or
            $protected.before_exists -ne $protected.after_exists -or
            ($protected.before_exists -and $null -eq $protected.after)) {
            throw "Existing retirement protected-inventory proof is incomplete: $($protected.id)"
        }
        if ($protected.before_exists) {
            Assert-SnapshotEqual -Expected $protected.before -Observed $protected.after -Label "Existing protected inventory $($protected.id)"
        }
    }
    Write-Output ($existingManifest | ConvertTo-Json -Depth 20)
    return
}

if (@($manifest.targets | Where-Object { $_.exists }).Count -eq 0) {
    $manifestPath = Join-Path $runRoot "manifest.json"
    New-Item -ItemType Directory -Path $runRoot | Out-Null
    Assert-NoReparsePoint -Workspace $workspace -Path $runRoot
    $manifest.run_id = $RunId
    $manifest.manifest_path = $manifestPath
    $manifest.status = "empty-baseline-proof-pending"
    Write-ManifestAtomic -Manifest $manifest -Path $manifestPath
    try {
        Set-ProtectedInventoriesAfter -Inventories @($manifest.protected_inventories)
        Invoke-TestOnlyDelay -Phase "empty-before-ready"
        Assert-TargetSetMatchesSnapshots -Workspace $workspace -Targets @($manifest.targets) -Phase "empty-baseline final reconciliation"
        $manifest.status = "ready-for-clean-initialization"
        $manifest.completed_utc = [DateTime]::UtcNow.ToString("o")
        Write-ManifestAtomic -Manifest $manifest -Path $manifestPath
    } catch {
        $manifest.status = "failed-source-preserved"
        $manifest.error = $_.Exception.Message
        $manifest.completed_utc = [DateTime]::UtcNow.ToString("o")
        try { Write-ManifestAtomic -Manifest $manifest -Path $manifestPath } catch { }
        throw "WP-079 empty-baseline proof failed. Inspect $manifestPath. $($manifest.error)"
    }
    Write-Output ($manifest | ConvertTo-Json -Depth 20)
    return
}

$coldRoot = Join-Path $runRoot "cold-backup"
$quarantineRoot = Join-Path $runRoot "quarantine"
$manifestPath = Join-Path $runRoot "manifest.json"
New-Item -ItemType Directory -Force -Path $coldRoot, $quarantineRoot | Out-Null
Assert-NoReparsePoint -Workspace $workspace -Path $runRoot
$manifest.run_id = $RunId
$manifest.manifest_path = $manifestPath
$manifest.status = "preservation-paths-planned"
foreach ($target in @($manifest.targets | Where-Object { $_.exists })) {
    $target.cold_backup_path = Get-NormalizedFullPath -Path (Join-Path $coldRoot $target.relative_path.Replace('/', '\'))
    Assert-DescendantPath -Parent $coldRoot -Candidate $target.cold_backup_path -Label "Cold backup destination"
    $target.cold_backup_status = "planned"
    $target.quarantine_path = Get-NormalizedFullPath -Path (Join-Path $quarantineRoot $target.relative_path.Replace('/', '\'))
    Assert-DescendantPath -Parent $quarantineRoot -Candidate $target.quarantine_path -Label "Quarantine destination"
    $target.quarantine_status = "planned"
}
Write-ManifestAtomic -Manifest $manifest -Path $manifestPath

$moved = New-Object System.Collections.Generic.List[object]
try {
    Assert-TargetSetMatchesSnapshots -Workspace $workspace -Targets @($manifest.targets) -Phase "before cold backup"
    foreach ($target in @($manifest.targets | Where-Object { $_.exists })) {
        $destination = $target.cold_backup_path
        Copy-VerifiedArtifact -Source $target.canonical_path -Destination $destination -ExpectedSnapshot $target.snapshot
        $sourceAfterCopy = Get-ArtifactSnapshot -Path $target.canonical_path
        Assert-SnapshotEqual -Expected $target.snapshot -Observed $sourceAfterCopy -Label "Source stability after cold copy $($target.canonical_path)"
        $target.cold_backup_status = "verified"
    }
    $manifest.status = "cold-backup-verified"
    Write-ManifestAtomic -Manifest $manifest -Path $manifestPath

    Assert-FacialProcessesStopped
    Assert-CurrentEnginePair -DatabaseRoot $currentDatabase -MarkerPath $currentMarker
    Assert-TargetSetMatchesSnapshots -Workspace $workspace -Targets @($manifest.targets) -Phase "before quarantine"
    foreach ($target in @($manifest.targets | Where-Object { $_.exists })) {
        Assert-ArtifactUnlocked -Path $target.canonical_path
        $sourceBeforeMove = Get-ArtifactSnapshot -Path $target.canonical_path
        Assert-SnapshotEqual -Expected $target.snapshot -Observed $sourceBeforeMove -Label "Source stability before quarantine $($target.canonical_path)"
    }

    foreach ($target in @($manifest.targets | Where-Object { $_.exists })) {
        Assert-FacialProcessesStopped
        $destination = $target.quarantine_path
        $moveRecord = [pscustomobject]@{ target = $target; destination = $destination; completed = $false }
        $moved.Add($moveRecord)
        # Same failure-only seam before the filesystem rename.
        if ($env:FACIAL_WP079_TEST_FAIL_BEFORE_MOVE_ID -eq $target.id) {
            throw "Injected WP-079 pre-move failure for $($target.id)."
        }
        Move-VerifiedArtifact -Source $target.canonical_path -Destination $destination -ExpectedSnapshot $target.snapshot -ArtifactId $target.id
        $moveRecord.completed = $true
        $target.quarantine_path = $destination
        $target.quarantine_status = "verified"
        $target.live_discovery = $false
        $target.final_disposition = "quarantined-with-verified-cold-backup"
    }

    Assert-FacialProcessesStopped
    Assert-AllTargetsAbsent -Workspace $workspace -Targets @($manifest.targets) -Phase "after quarantine"
    Set-ProtectedInventoriesAfter -Inventories @($manifest.protected_inventories)
    Invoke-TestOnlyDelay -Phase "nonempty-before-ready"
    Assert-AllTargetsAbsent -Workspace $workspace -Targets @($manifest.targets) -Phase "final protected-state reconciliation"
    $manifest.status = "ready-for-clean-initialization"
    $manifest.completed_utc = [DateTime]::UtcNow.ToString("o")
    Write-ManifestAtomic -Manifest $manifest -Path $manifestPath
} catch {
    $operationError = $_.Exception.Message
    if ($moved.Count -gt 0) {
        $manifest.rollback.attempted = $true
        $rollbackDetails = New-Object System.Collections.Generic.List[object]
        $rollbackComplete = $true
        for ($index = $moved.Count - 1; $index -ge 0; $index--) {
            $record = $moved[$index]
            $target = $record.target
            try {
                $sourceExists = Test-Path -LiteralPath $target.canonical_path
                $destinationExists = Test-Path -LiteralPath $record.destination
                if ($sourceExists -and -not $destinationExists) {
                    $restored = Get-ArtifactSnapshot -Path $target.canonical_path
                    Assert-SnapshotEqual -Expected $target.snapshot -Observed $restored -Label "Rollback live source $($target.canonical_path)"
                    $detailStatus = "not-moved-source-intact"
                } elseif (-not $sourceExists -and $destinationExists) {
                    Move-Item -LiteralPath $record.destination -Destination $target.canonical_path
                    $restored = Get-ArtifactSnapshot -Path $target.canonical_path
                    Assert-SnapshotEqual -Expected $target.snapshot -Observed $restored -Label "Rollback $($target.canonical_path)"
                    $detailStatus = "restored"
                } elseif ($sourceExists -and $destinationExists) {
                    throw "Both the live source and quarantine destination exist; refusing to choose or overwrite either copy."
                } else {
                    throw "Neither the live source nor quarantine destination exists."
                }
                $target.quarantine_status = "rolled-back"
                $target.live_discovery = $true
                $target.final_disposition = "restored-live-after-failure"
                $rollbackDetails.Add([pscustomobject][ordered]@{ id = $target.id; status = $detailStatus; quarantine_path = $record.destination; error = $null })
            } catch {
                $rollbackComplete = $false
                $rollbackDetails.Add([pscustomobject][ordered]@{ id = $target.id; status = "failed"; quarantine_path = $record.destination; error = $_.Exception.Message })
            }
        }
        $manifest.rollback.complete = $rollbackComplete
        $manifest.rollback.details = $rollbackDetails.ToArray()
    }
    $manifest.status = if ($manifest.rollback.attempted -and -not $manifest.rollback.complete) {
        "failed-rollback-incomplete"
    } else {
        "failed-source-preserved"
    }
    $manifest.error = $operationError
    $manifest.completed_utc = [DateTime]::UtcNow.ToString("o")
    try {
        Write-ManifestAtomic -Manifest $manifest -Path $manifestPath
    } catch {
        $operationError = "$operationError; additionally failed to update manifest: $($_.Exception.Message)"
    }
    throw "WP-079 retirement failed. No deletion was attempted. Inspect $manifestPath. $operationError"
}

Write-Output ($manifest | ConvertTo-Json -Depth 20)
