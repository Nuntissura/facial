<# Plain PowerShell regression tests for retire-legacy-media-db.ps1 (WP-079). #>
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$scriptPath = Join-Path $PSScriptRoot "retire-legacy-media-db.ps1"
$tempBase = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
$testRoot = Join-Path $tempBase ("facial-wp079-test-" + [Guid]::NewGuid().ToString("N"))

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw "ASSERTION FAILED: $Message" }
}

function Assert-Equal {
    param($Expected, $Observed, [string]$Message)
    if ($Expected -ne $Observed) {
        throw "ASSERTION FAILED: $Message (expected '$Expected', observed '$Observed')"
    }
}

function Assert-HasProperty {
    param($Object, [string]$Name, [string]$Message)
    Assert-True -Condition ($null -ne $Object -and $Object.PSObject.Properties.Name -contains $Name) -Message $Message
}

function Assert-NoProperty {
    param($Object, [string]$Name, [string]$Message)
    Assert-True -Condition ($null -ne $Object -and $Object.PSObject.Properties.Name -notcontains $Name) -Message $Message
}

function Get-TestHash {
    param([string]$Path)
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

try {
    New-Item -ItemType Directory -Path $testRoot | Out-Null

    $workspace = Join-Path $testRoot "workspace"
    $mediaRoot = Join-Path $workspace ".facial\media"
    $databaseRoot = Join-Path $mediaRoot "surrealdb"
    $thumbRoot = Join-Path $mediaRoot "thumbs\aa"
    $timelineDataRoot = Join-Path $workspace "timeline-data"
    $timelineLedgerRoot = Join-Path $workspace ".facial\timeline-ledger\surrealdb"
    New-Item -ItemType Directory -Force -Path $databaseRoot, $thumbRoot, $timelineDataRoot, $timelineLedgerRoot | Out-Null
    [IO.File]::WriteAllBytes((Join-Path $databaseRoot "manifest"), [byte[]](0, 1, 2, 3, 255))
    New-Item -ItemType Directory -Path (Join-Path $databaseRoot "empty-dir") | Out-Null
    [IO.File]::WriteAllText(
        (Join-Path $mediaRoot "engine.json"),
        '{"engine":"surrealdb","engine_version":"3.2.4","namespace":"facial","database":"application","schema_version":1}',
        [Text.UTF8Encoding]::new($false)
    )
    [IO.File]::WriteAllBytes((Join-Path $mediaRoot "media.redb"), [byte[]](82, 69, 68, 66, 0, 9))
    [IO.File]::WriteAllText(
        (Join-Path $workspace ".facial\media_metadata.json"),
        '{"legacy":true}',
        [Text.UTF8Encoding]::new($false)
    )
    $rawMedia = Join-Path $workspace "operator-image.jpg"
    $thumb = Join-Path $thumbRoot "thumb.jpg"
    $timelineAnchor = Join-Path $workspace "timeline-maintenance.yaml"
    $timeline = Join-Path $timelineDataRoot "event-registry.jsonl"
    $timelineLedger = Join-Path $timelineLedgerRoot "manifest"
    $timelineMarker = Join-Path $workspace ".facial\timeline-ledger\engine.json"
    [IO.File]::WriteAllBytes($rawMedia, [byte[]](10, 20, 30, 40))
    [IO.File]::WriteAllBytes($thumb, [byte[]](50, 60, 70, 80))
    [IO.File]::WriteAllText($timelineAnchor, "project: fixture", [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText($timeline, '{"event":"preserve"}', [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllBytes($timelineLedger, [byte[]](90, 91, 92, 93))
    [IO.File]::WriteAllText($timelineMarker, '{"engine":"surrealdb","schema_version":2}', [Text.UTF8Encoding]::new($false))
    $protectedHashes = @{
        raw = Get-TestHash -Path $rawMedia
        thumb = Get-TestHash -Path $thumb
        timeline = Get-TestHash -Path $timeline
        timeline_anchor = Get-TestHash -Path $timelineAnchor
        timeline_ledger = Get-TestHash -Path $timelineLedger
        timeline_marker = Get-TestHash -Path $timelineMarker
    }

    $auditJson = & $scriptPath -Mode Audit -WorkspaceRoot $workspace
    $audit = $auditJson | ConvertFrom-Json
    $auditTargetIds = @($audit.targets | ForEach-Object { $_.id })
    Assert-True `
        -Condition ([Array]::IndexOf($auditTargetIds, "current-engine-marker") -lt [Array]::IndexOf($auditTargetIds, "current-surrealdb")) `
        -Message "Current engine marker is quarantined before the database so a racing app fails closed instead of recreating the store"
    Assert-Equal -Expected "audited" -Observed $audit.status -Message "Audit status"
    Assert-True -Condition ($audit.audit_token -match '^[0-9a-f]{64}$') -Message "Audit emits a state-bound SHA-256 token"
    Assert-Equal -Expected 1 -Observed @($auditJson).Count -Message "Audit emits one JSON value and no console chatter"
    Assert-Equal -Expected "facial-wp079-exact-content-tree-v1" -Observed $audit.snapshot_contracts.exact_artifacts.contract -Message "Manifest names exact artifact contract"
    Assert-Equal -Expected "sha256" -Observed $audit.snapshot_contracts.exact_artifacts.digest_algorithm -Message "Manifest names exact artifact algorithm"
    Assert-Equal -Expected "facial-wp079-bounded-metadata-tree-v1" -Observed $audit.snapshot_contracts.protected_trees.contract -Message "Manifest names protected metadata contract"
    Assert-Equal -Expected "aggregate-only" -Observed $audit.snapshot_contracts.protected_trees.manifest_rows -Message "Manifest declares aggregate-only protected snapshots"
    Assert-Equal -Expected 7 -Observed @($audit.targets).Count -Message "Exact candidate count"
    Assert-Equal -Expected 2 -Observed @($audit.targets | Where-Object { $_.ownership_verdict -eq "path-only-ambiguous" }).Count -Message "Path-only legacy fixtures remain explicitly ambiguous"
    Assert-True -Condition (-not (Test-Path -LiteralPath (Join-Path $workspace ".facial-media-retirement"))) -Message "Audit must not create retirement state"
    Assert-True -Condition (Test-Path -LiteralPath $databaseRoot) -Message "Audit keeps current database live"

    foreach ($target in @($audit.targets | Where-Object { $_.exists })) {
        Assert-Equal -Expected "facial-wp079-exact-content-tree-v1" -Observed $target.snapshot.snapshot_contract -Message "$($target.id) uses exact content snapshot contract"
        Assert-Equal -Expected "exact-content-sha256" -Observed $target.snapshot.snapshot_mode -Message "$($target.id) uses exact content snapshot mode"
        Assert-Equal -Expected "sha256" -Observed $target.snapshot.digest_algorithm -Message "$($target.id) names its digest algorithm"
        Assert-HasProperty -Object $target.snapshot -Name "files" -Message "$($target.id) retains per-file content records"
        Assert-True -Condition (@($target.snapshot.files).Count -gt 0) -Message "$($target.id) has at least one exact file record"
    }

    $expectedThumbnailRoot = [IO.Path]::GetFullPath((Join-Path $mediaRoot "thumbs"))
    foreach ($boundedId in @("raw-workspace", "thumbnail-cache", "facial-unrelated-state")) {
        $boundedProofs = @($audit.protected_inventories | Where-Object { $_.id -eq $boundedId })
        Assert-Equal -Expected 1 -Observed $boundedProofs.Count -Message "$boundedId remains one required protected inventory"
        $boundedProof = $boundedProofs[0]
        Assert-Equal -Expected "facial-wp079-bounded-metadata-tree-v1" -Observed $boundedProof.snapshot_contract -Message "$boundedId record binds bounded metadata contract"
        Assert-Equal -Expected "bounded-metadata-tree" -Observed $boundedProof.snapshot_mode -Message "$boundedId record binds bounded metadata mode"
        Assert-Equal -Expected "sha256" -Observed $boundedProof.digest_algorithm -Message "$boundedId record binds SHA-256"
        if ($boundedProof.before_exists) {
            Assert-Equal -Expected "facial-wp079-bounded-metadata-tree-v1" -Observed $boundedProof.before.snapshot_contract -Message "$boundedId snapshot identifies its contract"
            Assert-Equal -Expected "bounded-metadata-tree" -Observed $boundedProof.before.snapshot_mode -Message "$boundedId snapshot identifies its mode"
            Assert-Equal -Expected "sha256" -Observed $boundedProof.before.digest_algorithm -Message "$boundedId snapshot names SHA-256"
            Assert-NoProperty -Object $boundedProof.before -Name "files" -Message "$boundedId manifest snapshot contains no per-file records"
        }
    }
    $thumbnailAuditProofs = @($audit.protected_inventories | Where-Object { $_.id -eq "thumbnail-cache" })
    Assert-Equal -Expected 1 -Observed $thumbnailAuditProofs.Count -Message "Thumbnail cache remains a required protected inventory"
    $thumbnailAuditProof = $thumbnailAuditProofs[0]
    Assert-True -Condition ([bool]$thumbnailAuditProof.before_exists) -Message "Thumbnail cache protected inventory exists"
    Assert-Equal -Expected $expectedThumbnailRoot -Observed $thumbnailAuditProof.canonical_path -Message "Thumbnail cache protected inventory uses the exact cache root"
    Assert-Equal -Expected 1 -Observed $thumbnailAuditProof.before.file_count -Message "Thumbnail cache metadata inventory counts the fixture"

    $unrelatedAuditProofs = @($audit.protected_inventories | Where-Object { $_.id -eq "facial-unrelated-state" })
    Assert-Equal -Expected 1 -Observed $unrelatedAuditProofs.Count -Message "Unrelated Facial state remains a required protected inventory"
    $unrelatedAuditProof = $unrelatedAuditProofs[0]
    Assert-Equal -Expected 1 -Observed @($unrelatedAuditProof.excluded_roots | Where-Object { $_ -ieq $expectedThumbnailRoot }).Count -Message "Unrelated Facial state excludes the exact thumbnail-cache root"

    foreach ($timelineId in @("timeline-anchor", "timeline-event-registry", "timeline-surrealdb", "timeline-engine-marker")) {
        $timelineProofRecord = @($audit.protected_inventories | Where-Object { $_.id -eq $timelineId })[0]
        Assert-True -Condition ([bool]$timelineProofRecord.before_exists) -Message "$timelineId fixture exists"
        Assert-Equal -Expected "facial-wp079-exact-content-tree-v1" -Observed $timelineProofRecord.before.snapshot_contract -Message "$timelineId uses exact content contract"
        Assert-Equal -Expected "exact-content-sha256" -Observed $timelineProofRecord.before.snapshot_mode -Message "$timelineId uses exact content mode"
        Assert-Equal -Expected "sha256" -Observed $timelineProofRecord.before.digest_algorithm -Message "$timelineId names its digest algorithm"
        Assert-HasProperty -Object $timelineProofRecord.before -Name "files" -Message "$timelineId retains per-file content records"
        Assert-True -Condition (@($timelineProofRecord.before.files).Count -gt 0) -Message "$timelineId exact inventory is nonempty"
    }

    $lockedRaw = [IO.File]::Open($rawMedia, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::None)
    try {
        $lockedAudit = (& $scriptPath -Mode Audit -WorkspaceRoot $workspace) | ConvertFrom-Json
        Assert-Equal -Expected $audit.audit_token -Observed $lockedAudit.audit_token -Message "Protected raw file can remain exclusively locked because metadata inventory never reads its contents"
    } finally {
        $lockedRaw.Dispose()
    }

    $approvalRejected = $false
    try {
        & $scriptPath -Mode Execute -WorkspaceRoot $workspace -RunId "wp-079-test-no-approval" -AuditToken $audit.audit_token | Out-Null
    } catch {
        $approvalRejected = $_.Exception.Message -match "ApprovePathOnlyLegacyFiles"
    }
    Assert-True -Condition $approvalRejected -Message "Path-only legacy files require explicit approval"
    Assert-True -Condition (-not (Test-Path -LiteralPath (Join-Path $workspace ".facial-media-retirement"))) -Message "Unapproved legacy preflight creates no retirement state"

    $staleAuditRejected = $false
    try {
        & $scriptPath -Mode Execute -WorkspaceRoot $workspace -RunId "wp-079-test-stale-audit" -ApprovePathOnlyLegacyFiles -AuditToken ("0" * 64) | Out-Null
    } catch {
        $staleAuditRejected = $_.Exception.Message -match "Audit token mismatch"
    }
    Assert-True -Condition $staleAuditRejected -Message "Execute rejects a mismatched audit receipt"
    Assert-True -Condition (-not (Test-Path -LiteralPath (Join-Path $workspace ".facial-media-retirement"))) -Message "Mismatched audit receipt creates no retirement state"

    $protectedDrift = Join-Path $workspace "protected-state-drift.tmp"
    [IO.File]::WriteAllText($protectedDrift, "drift", [Text.UTF8Encoding]::new($false))
    $protectedDriftRejected = $false
    try {
        & $scriptPath -Mode Execute -WorkspaceRoot $workspace -RunId "wp-079-test-protected-drift" -ApprovePathOnlyLegacyFiles -AuditToken $audit.audit_token | Out-Null
    } catch {
        $protectedDriftRejected = $_.Exception.Message -match "Audit token mismatch"
    } finally {
        Remove-Item -LiteralPath $protectedDrift -Force
    }
    Assert-True -Condition $protectedDriftRejected -Message "Execute rejects real protected metadata-tree drift after Audit"
    Assert-True -Condition (-not (Test-Path -LiteralPath (Join-Path $workspace ".facial-media-retirement"))) -Message "Protected-state drift rejection creates no retirement state"

    $executeJson = & $scriptPath -Mode Execute -WorkspaceRoot $workspace -RunId "wp-079-test-success" -ApprovePathOnlyLegacyFiles -AuditToken $audit.audit_token
    $execute = $executeJson | ConvertFrom-Json
    Assert-Equal -Expected "ready-for-clean-initialization" -Observed $execute.status -Message "Execute status"
    Assert-Equal -Expected ([IO.Path]::GetFullPath((Join-Path $workspace ".facial-media-retirement"))) -Observed $execute.retirement_root -Message "Retirement root is outside installer .facial cleanup"
    Assert-Equal -Expected "not-performed-by-this-tool" -Observed $execute.runtime_initialization -Message "Tool never initializes replacement"
    Assert-True -Condition ([bool]$execute.legacy_path_only_approval) -Message "Legacy path-only approval recorded"
    Assert-True -Condition (-not (Test-Path -LiteralPath $databaseRoot)) -Message "Current DB removed from live discovery"
    Assert-True -Condition (-not (Test-Path -LiteralPath (Join-Path $mediaRoot "engine.json"))) -Message "Engine marker removed from live discovery"
    Assert-True -Condition (-not (Test-Path -LiteralPath (Join-Path $mediaRoot "media.redb"))) -Message "Legacy redb removed from live discovery"
    Assert-True -Condition (-not (Test-Path -LiteralPath (Join-Path $workspace ".facial\media_metadata.json"))) -Message "Legacy JSON removed from live discovery"

    foreach ($target in @($execute.targets | Where-Object { $_.exists })) {
        Assert-Equal -Expected "verified" -Observed $target.cold_backup_status -Message "$($target.id) cold backup verified"
        Assert-Equal -Expected "verified" -Observed $target.quarantine_status -Message "$($target.id) quarantine verified"
        Assert-Equal -Expected "quarantined-with-verified-cold-backup" -Observed $target.final_disposition -Message "$($target.id) final disposition"
        Assert-True -Condition (Test-Path -LiteralPath $target.cold_backup_path) -Message "$($target.id) cold backup exists"
        Assert-True -Condition (Test-Path -LiteralPath $target.quarantine_path) -Message "$($target.id) quarantine exists"
        foreach ($file in @($target.snapshot.files)) {
            $coldFile = if ($target.snapshot.kind -eq "file") {
                $target.cold_backup_path
            } else {
                Join-Path $target.cold_backup_path $file.relative_path.Replace('/', '\')
            }
            $quarantineFile = if ($target.snapshot.kind -eq "file") {
                $target.quarantine_path
            } else {
                Join-Path $target.quarantine_path $file.relative_path.Replace('/', '\')
            }
            Assert-Equal -Expected $file.sha256 -Observed (Get-TestHash -Path $coldFile) -Message "$($target.id) cold file digest"
            Assert-Equal -Expected $file.sha256 -Observed (Get-TestHash -Path $quarantineFile) -Message "$($target.id) quarantine file digest"
        }
    }

    $currentTarget = @($execute.targets | Where-Object { $_.id -eq "current-surrealdb" })[0]
    Assert-True -Condition (Test-Path -LiteralPath (Join-Path $currentTarget.cold_backup_path "empty-dir") -PathType Container) -Message "Cold backup preserves empty directories"
    Assert-True -Condition (Test-Path -LiteralPath (Join-Path $currentTarget.quarantine_path "empty-dir") -PathType Container) -Message "Quarantine preserves empty directories"
    Assert-Equal -Expected $protectedHashes.raw -Observed (Get-TestHash -Path $rawMedia) -Message "Raw media remains byte-identical"
    Assert-Equal -Expected $protectedHashes.thumb -Observed (Get-TestHash -Path $thumb) -Message "Thumbnail remains byte-identical"
    Assert-Equal -Expected $protectedHashes.timeline -Observed (Get-TestHash -Path $timeline) -Message "Timeline remains byte-identical"
    Assert-Equal -Expected $protectedHashes.timeline_anchor -Observed (Get-TestHash -Path $timelineAnchor) -Message "Timeline anchor remains byte-identical"
    Assert-Equal -Expected $protectedHashes.timeline_ledger -Observed (Get-TestHash -Path $timelineLedger) -Message "Timeline ledger remains byte-identical"
    Assert-Equal -Expected $protectedHashes.timeline_marker -Observed (Get-TestHash -Path $timelineMarker) -Message "Timeline marker remains byte-identical"
    foreach ($protected in @($execute.protected_inventories)) {
        Assert-True -Condition ([bool]$protected.unchanged) -Message "$($protected.id) protected reconciliation passed"
        if ($protected.before_exists) {
            Assert-True -Condition ($null -ne $protected.after) -Message "$($protected.id) after inventory recorded"
            Assert-Equal -Expected $protected.before.tree_sha256 -Observed $protected.after.tree_sha256 -Message "$($protected.id) tree digest unchanged"
            if ($protected.snapshot_mode -eq "bounded-metadata-tree") {
                Assert-NoProperty -Object $protected.before -Name "files" -Message "$($protected.id) before snapshot has no per-file records"
                Assert-NoProperty -Object $protected.after -Name "files" -Message "$($protected.id) after snapshot has no per-file records"
            } else {
                Assert-HasProperty -Object $protected.before -Name "files" -Message "$($protected.id) exact before snapshot retains per-file records"
                Assert-HasProperty -Object $protected.after -Name "files" -Message "$($protected.id) exact after snapshot retains per-file records"
            }
        }
    }
    $thumbnailExecuteProofs = @($execute.protected_inventories | Where-Object { $_.id -eq "thumbnail-cache" })
    Assert-Equal -Expected 1 -Observed $thumbnailExecuteProofs.Count -Message "Execute retains one required thumbnail-cache proof"
    Assert-True -Condition ([bool]$thumbnailExecuteProofs[0].unchanged) -Message "Thumbnail cache metadata inventory reconciles unchanged"
    Assert-Equal -Expected 1 -Observed $thumbnailExecuteProofs[0].after.file_count -Message "Thumbnail cache remains present in its bounded after inventory"
    $unrelatedExecuteProof = @($execute.protected_inventories | Where-Object { $_.id -eq "facial-unrelated-state" })[0]
    Assert-Equal -Expected 1 -Observed @($unrelatedExecuteProof.excluded_roots | Where-Object { $_ -ieq $expectedThumbnailRoot }).Count -Message "Execute records thumbnail exclusion on unrelated Facial state"
    Assert-NoProperty -Object $unrelatedExecuteProof.before -Name "files" -Message "Unrelated before inventory never serializes protected paths"
    Assert-NoProperty -Object $unrelatedExecuteProof.after -Name "files" -Message "Unrelated after inventory never serializes protected paths"
    Assert-True -Condition (Test-Path -LiteralPath $execute.manifest_path -PathType Leaf) -Message "Machine-readable manifest exists"
    $persistedManifest = Get-Content -LiteralPath $execute.manifest_path -Raw | ConvertFrom-Json
    Assert-Equal -Expected "ready-for-clean-initialization" -Observed $persistedManifest.status -Message "Persisted manifest final status"

    $postAudit = (& $scriptPath -Mode Audit -WorkspaceRoot $workspace) | ConvertFrom-Json
    Assert-Equal -Expected 0 -Observed @($postAudit.targets | Where-Object { $_.exists }).Count -Message "Retired copies are outside live discovery"
    Assert-True -Condition (Test-Path -LiteralPath $thumb) -Message "Post-retirement audit leaves thumbnail cache present"

    $persistedManifestRaw = Get-Content -LiteralPath $execute.manifest_path -Raw
    $missingThumbnailManifest = $persistedManifestRaw | ConvertFrom-Json
    $missingThumbnailManifest.protected_inventories = @(
        $missingThumbnailManifest.protected_inventories | Where-Object { $_.id -ne "thumbnail-cache" }
    )
    [IO.File]::WriteAllText(
        $execute.manifest_path,
        ($missingThumbnailManifest | ConvertTo-Json -Depth 20),
        [Text.UTF8Encoding]::new($false)
    )
    $missingThumbnailRejected = $false
    try {
        & $scriptPath -Mode Execute -WorkspaceRoot $workspace -RunId "wp-079-test-success" -AuditToken $postAudit.audit_token | Out-Null
    } catch {
        $missingThumbnailRejected = $_.Exception.Message -match "missing protected inventory: thumbnail-cache"
    } finally {
        [IO.File]::WriteAllText($execute.manifest_path, $persistedManifestRaw, [Text.UTF8Encoding]::new($false))
    }
    Assert-True -Condition $missingThumbnailRejected -Message "Completed manifests require the exact thumbnail-cache proof"

    $idempotent = (& $scriptPath -Mode Execute -WorkspaceRoot $workspace -RunId "wp-079-test-success" -AuditToken $postAudit.audit_token) | ConvertFrom-Json
    Assert-Equal -Expected "ready-for-clean-initialization" -Observed $idempotent.status -Message "Completed RunId is idempotent"
    $noOp = (& $scriptPath -Mode Execute -WorkspaceRoot $workspace -RunId "wp-079-test-noop" -AuditToken $postAudit.audit_token) | ConvertFrom-Json
    Assert-Equal -Expected "ready-for-clean-initialization" -Observed $noOp.status -Message "Zero-target Execute persists the uniform initialization gate"
    Assert-True -Condition (Test-Path -LiteralPath $noOp.manifest_path -PathType Leaf) -Message "Zero-target Execute persists a manifest"
    Assert-Equal -Expected 0 -Observed @($noOp.targets | Where-Object { $_.exists }).Count -Message "Zero-target manifest records every target absent"

    $incompleteRun = Join-Path $workspace ".facial-media-retirement\wp-079-test-incomplete"
    New-Item -ItemType Directory -Path $incompleteRun | Out-Null
    $incompleteRejected = $false
    try {
        & $scriptPath -Mode Execute -WorkspaceRoot $workspace -RunId "wp-079-test-incomplete" -AuditToken $postAudit.audit_token | Out-Null
    } catch {
        $incompleteRejected = $_.Exception.Message -match "no manifest"
    }
    Assert-True -Condition $incompleteRejected -Message "Incomplete RunId fails closed"

    $emptyWorkspace = Join-Path $testRoot "empty-workspace"
    New-Item -ItemType Directory -Path $emptyWorkspace | Out-Null
    $emptyAudit = (& $scriptPath -Mode Audit -WorkspaceRoot $emptyWorkspace -NoTimelineLedger) | ConvertFrom-Json
    $emptyExecute = (& $scriptPath -Mode Execute -WorkspaceRoot $emptyWorkspace -NoTimelineLedger -RunId "wp-079-test-empty" -AuditToken $emptyAudit.audit_token) | ConvertFrom-Json
    Assert-Equal -Expected "ready-for-clean-initialization" -Observed $emptyExecute.status -Message "Genuinely empty workspace crosses the persisted initialization gate"
    Assert-True -Condition (Test-Path -LiteralPath $emptyExecute.manifest_path -PathType Leaf) -Message "Empty workspace manifest exists"
    $emptyFacialProof = @($emptyExecute.protected_inventories | Where-Object { $_.id -eq "facial-unrelated-state" })[0]
    Assert-True -Condition ([bool]$emptyFacialProof.unchanged) -Message "Sibling retirement root preserves absent Facial state"
    Assert-True -Condition (-not [bool]$emptyFacialProof.before_exists -and -not [bool]$emptyFacialProof.after_exists) -Message "Empty-baseline proof never creates .facial"
    $emptyPostAudit = (& $scriptPath -Mode Audit -WorkspaceRoot $emptyWorkspace -NoTimelineLedger) | ConvertFrom-Json
    $emptyReplay = (& $scriptPath -Mode Execute -WorkspaceRoot $emptyWorkspace -NoTimelineLedger -RunId "wp-079-test-empty" -AuditToken $emptyPostAudit.audit_token) | ConvertFrom-Json
    Assert-Equal -Expected "ready-for-clean-initialization" -Observed $emptyReplay.status -Message "Persisted empty-baseline RunId is idempotent"

    # Reproduce the false-ready TOCTOU boundary deterministically: the exact
    # target path is absent in Audit and at Execute inventory time, appears
    # after the pending manifest is written, and must remain live while the
    # ready gate fails closed. The target itself is excluded from protected
    # inventory, so only the explicit all-target reconciliation can catch it.
    $appearanceWorkspace = Join-Path $testRoot "target-appearance-workspace"
    $appearanceMedia = Join-Path $appearanceWorkspace ".facial\media"
    New-Item -ItemType Directory -Force -Path $appearanceMedia | Out-Null
    $appearanceAudit = (& $scriptPath -Mode Audit -WorkspaceRoot $appearanceWorkspace -NoTimelineLedger) | ConvertFrom-Json
    $appearanceRunId = "wp-079-test-target-appearance"
    $appearanceRunRoot = Join-Path $appearanceWorkspace ".facial-media-retirement\$appearanceRunId"
    $appearanceManifestPath = Join-Path $appearanceRunRoot "manifest.json"
    $appearanceStdout = Join-Path $testRoot "target-appearance.stdout.json"
    $appearanceStderr = Join-Path $testRoot "target-appearance.stderr.txt"
    $priorDelayPhase = [Environment]::GetEnvironmentVariable("FACIAL_WP079_TEST_DELAY_PHASE", "Process")
    $priorDelayMs = [Environment]::GetEnvironmentVariable("FACIAL_WP079_TEST_DELAY_MS", "Process")
    [Environment]::SetEnvironmentVariable("FACIAL_WP079_TEST_DELAY_PHASE", "empty-before-ready", "Process")
    [Environment]::SetEnvironmentVariable("FACIAL_WP079_TEST_DELAY_MS", "5000", "Process")
    try {
        $appearanceArgs = @(
            "-NoProfile",
            "-ExecutionPolicy", "Bypass",
            "-File", ('"{0}"' -f $scriptPath),
            "-Mode", "Execute",
            "-WorkspaceRoot", ('"{0}"' -f $appearanceWorkspace),
            "-NoTimelineLedger",
            "-RunId", $appearanceRunId,
            "-AuditToken", $appearanceAudit.audit_token
        )
        $appearanceProcess = Start-Process `
            -FilePath "powershell.exe" `
            -ArgumentList $appearanceArgs `
            -WindowStyle Hidden `
            -RedirectStandardOutput $appearanceStdout `
            -RedirectStandardError $appearanceStderr `
            -PassThru
    } finally {
        [Environment]::SetEnvironmentVariable("FACIAL_WP079_TEST_DELAY_PHASE", $priorDelayPhase, "Process")
        [Environment]::SetEnvironmentVariable("FACIAL_WP079_TEST_DELAY_MS", $priorDelayMs, "Process")
    }
    $appearanceDeadline = [DateTime]::UtcNow.AddSeconds(15)
    $appearancePending = $false
    while ([DateTime]::UtcNow -lt $appearanceDeadline -and -not $appearanceProcess.HasExited) {
        if (Test-Path -LiteralPath $appearanceManifestPath -PathType Leaf) {
            try {
                $appearanceStatus = (Get-Content -LiteralPath $appearanceManifestPath -Raw | ConvertFrom-Json).status
                if ($appearanceStatus -eq "empty-baseline-proof-pending") {
                    $appearancePending = $true
                    break
                }
            } catch { }
        }
        Start-Sleep -Milliseconds 50
        $appearanceProcess.Refresh()
    }
    Assert-True -Condition $appearancePending -Message "Target-appearance probe reaches the persisted pending gate"
    $appearedTarget = Join-Path $appearanceMedia "media.redb"
    [IO.File]::WriteAllBytes($appearedTarget, [byte[]](82, 69, 68, 66, 9, 9))
    if (-not $appearanceProcess.WaitForExit(15000)) {
        Stop-Process -Id $appearanceProcess.Id -Force
        $appearanceProcess.WaitForExit()
        throw "ASSERTION FAILED: target-appearance probe did not terminate"
    }
    Assert-True -Condition ($appearanceProcess.ExitCode -ne 0) -Message "A target appearing after inventory rejects Execute"
    $appearanceFailure = Get-Content -LiteralPath $appearanceStderr -Raw
    Assert-True -Condition ($appearanceFailure -match "existence changed during empty-baseline final reconciliation") -Message "Target-appearance rejection names the exact final reconciliation gate"
    $appearanceManifest = Get-Content -LiteralPath $appearanceManifestPath -Raw | ConvertFrom-Json
    Assert-Equal -Expected "failed-source-preserved" -Observed $appearanceManifest.status -Message "Target-appearance manifest never reports ready"
    Assert-True -Condition (Test-Path -LiteralPath $appearedTarget -PathType Leaf) -Message "Unexpected appeared target is preserved for diagnosis"

    # Exercise the same all-seven gate after a nonempty marker-first quarantine.
    # An initially absent legacy candidate appears while both audited current
    # targets are in quarantine; Execute must restore those originals, preserve
    # the unexpected candidate, and refuse a ready manifest.
    $nonemptyRaceWorkspace = Join-Path $testRoot "nonempty-target-appearance-workspace"
    $nonemptyRaceMedia = Join-Path $nonemptyRaceWorkspace ".facial\media"
    $nonemptyRaceDb = Join-Path $nonemptyRaceMedia "surrealdb"
    $nonemptyRaceMarker = Join-Path $nonemptyRaceMedia "engine.json"
    $nonemptyRaceData = Join-Path $nonemptyRaceDb "data"
    New-Item -ItemType Directory -Force -Path $nonemptyRaceDb | Out-Null
    [IO.File]::WriteAllBytes($nonemptyRaceData, [byte[]](1, 3, 3, 7))
    [IO.File]::WriteAllText(
        $nonemptyRaceMarker,
        '{"engine":"surrealdb","engine_version":"3.2.4","namespace":"facial","database":"application","schema_version":1}',
        [Text.UTF8Encoding]::new($false)
    )
    $nonemptyRaceDataHash = Get-TestHash -Path $nonemptyRaceData
    $nonemptyRaceMarkerHash = Get-TestHash -Path $nonemptyRaceMarker
    $nonemptyRaceAudit = (& $scriptPath -Mode Audit -WorkspaceRoot $nonemptyRaceWorkspace -NoTimelineLedger) | ConvertFrom-Json
    $nonemptyRaceRunId = "wp-079-test-nonempty-target-appearance"
    $nonemptyRaceRunRoot = Join-Path $nonemptyRaceWorkspace ".facial-media-retirement\$nonemptyRaceRunId"
    $nonemptyRaceManifestPath = Join-Path $nonemptyRaceRunRoot "manifest.json"
    $nonemptyRaceStdout = Join-Path $testRoot "nonempty-target-appearance.stdout.json"
    $nonemptyRaceStderr = Join-Path $testRoot "nonempty-target-appearance.stderr.txt"
    $priorDelayPhase = [Environment]::GetEnvironmentVariable("FACIAL_WP079_TEST_DELAY_PHASE", "Process")
    $priorDelayMs = [Environment]::GetEnvironmentVariable("FACIAL_WP079_TEST_DELAY_MS", "Process")
    [Environment]::SetEnvironmentVariable("FACIAL_WP079_TEST_DELAY_PHASE", "nonempty-before-ready", "Process")
    [Environment]::SetEnvironmentVariable("FACIAL_WP079_TEST_DELAY_MS", "8000", "Process")
    try {
        $nonemptyRaceArgs = @(
            "-NoProfile",
            "-ExecutionPolicy", "Bypass",
            "-File", ('"{0}"' -f $scriptPath),
            "-Mode", "Execute",
            "-WorkspaceRoot", ('"{0}"' -f $nonemptyRaceWorkspace),
            "-NoTimelineLedger",
            "-RunId", $nonemptyRaceRunId,
            "-AuditToken", $nonemptyRaceAudit.audit_token
        )
        $nonemptyRaceProcess = Start-Process `
            -FilePath "powershell.exe" `
            -ArgumentList $nonemptyRaceArgs `
            -WindowStyle Hidden `
            -RedirectStandardOutput $nonemptyRaceStdout `
            -RedirectStandardError $nonemptyRaceStderr `
            -PassThru
    } finally {
        [Environment]::SetEnvironmentVariable("FACIAL_WP079_TEST_DELAY_PHASE", $priorDelayPhase, "Process")
        [Environment]::SetEnvironmentVariable("FACIAL_WP079_TEST_DELAY_MS", $priorDelayMs, "Process")
    }
    $nonemptyRaceDeadline = [DateTime]::UtcNow.AddSeconds(15)
    $nonemptyRaceQuarantined = $false
    while ([DateTime]::UtcNow -lt $nonemptyRaceDeadline -and -not $nonemptyRaceProcess.HasExited) {
        if ((Test-Path -LiteralPath $nonemptyRaceManifestPath -PathType Leaf) -and
            -not (Test-Path -LiteralPath $nonemptyRaceDb) -and
            -not (Test-Path -LiteralPath $nonemptyRaceMarker)) {
            $nonemptyRaceQuarantined = $true
            break
        }
        Start-Sleep -Milliseconds 50
        $nonemptyRaceProcess.Refresh()
    }
    Assert-True -Condition $nonemptyRaceQuarantined -Message "Nonempty target-appearance probe reaches marker-first quarantine"
    Start-Sleep -Milliseconds 500
    $nonemptyRaceProcess.Refresh()
    Assert-True -Condition (-not $nonemptyRaceProcess.HasExited) -Message "Nonempty target-appearance probe remains inside the final reconciliation delay"
    Assert-True -Condition (-not (Test-Path -LiteralPath $nonemptyRaceDb) -and -not (Test-Path -LiteralPath $nonemptyRaceMarker)) -Message "Current targets remain quarantined at the final delay"
    $nonemptyAppearedTarget = Join-Path $nonemptyRaceMedia "media.redb"
    [IO.File]::WriteAllBytes($nonemptyAppearedTarget, [byte[]](82, 69, 68, 66, 7, 7))
    if (-not $nonemptyRaceProcess.WaitForExit(15000)) {
        Stop-Process -Id $nonemptyRaceProcess.Id -Force
        $nonemptyRaceProcess.WaitForExit()
        throw "ASSERTION FAILED: nonempty target-appearance probe did not terminate"
    }
    Assert-True -Condition ($nonemptyRaceProcess.ExitCode -ne 0) -Message "A target appearing after nonempty quarantine rejects Execute"
    $nonemptyRaceFailure = Get-Content -LiteralPath $nonemptyRaceStderr -Raw
    Assert-True -Condition ($nonemptyRaceFailure -match "exact retired target is present during final protected-state reconciliation") -Message "Nonempty target-appearance rejection names the final all-target gate"
    $nonemptyRaceManifest = Get-Content -LiteralPath $nonemptyRaceManifestPath -Raw | ConvertFrom-Json
    Assert-Equal -Expected "failed-source-preserved" -Observed $nonemptyRaceManifest.status -Message "Nonempty target-appearance manifest never reports ready"
    Assert-True -Condition ([bool]$nonemptyRaceManifest.rollback.attempted) -Message "Nonempty target appearance triggers rollback"
    Assert-True -Condition ([bool]$nonemptyRaceManifest.rollback.complete) -Message "Marker-first target appearance rollback completes"
    Assert-Equal -Expected $nonemptyRaceDataHash -Observed (Get-TestHash -Path $nonemptyRaceData) -Message "Nonempty target appearance restores the original database"
    Assert-Equal -Expected $nonemptyRaceMarkerHash -Observed (Get-TestHash -Path $nonemptyRaceMarker) -Message "Nonempty target appearance restores the original marker"
    Assert-True -Condition (Test-Path -LiteralPath $nonemptyAppearedTarget -PathType Leaf) -Message "Unexpected nonempty appeared target is preserved for diagnosis"

    $scaleWorkspace = Join-Path $testRoot "bounded-manifest-workspace"
    $scaleFanout = Join-Path $scaleWorkspace "bulk"
    New-Item -ItemType Directory -Force -Path $scaleFanout | Out-Null
    for ($index = 0; $index -lt 2048; $index++) {
        $scaleFile = Join-Path $scaleFanout ("bulk-{0:D4}.bin" -f $index)
        [IO.File]::WriteAllBytes($scaleFile, [byte[]]($index % 251))
    }
    $lockedScaleFile = Join-Path $scaleFanout "bulk-0000.bin"
    $scaleLock = [IO.File]::Open($lockedScaleFile, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::None)
    try {
        $scaleAuditJson = & $scriptPath -Mode Audit -WorkspaceRoot $scaleWorkspace -NoTimelineLedger
    } finally {
        $scaleLock.Dispose()
    }
    $scaleAudit = $scaleAuditJson | ConvertFrom-Json
    $scaleRaw = @($scaleAudit.protected_inventories | Where-Object { $_.id -eq "raw-workspace" })[0]
    Assert-Equal -Expected 2048 -Observed $scaleRaw.before.file_count -Message "High-cardinality protected tree counts every file without opening the exclusively locked fixture"
    Assert-NoProperty -Object $scaleRaw.before -Name "files" -Message "High-cardinality protected tree emits no per-file manifest rows"
    Assert-True -Condition ($scaleAuditJson.Length -lt 65536) -Message "High-cardinality Audit JSON remains bounded below 64 KiB"
    Assert-True -Condition ($scaleAuditJson -notmatch "bulk-0001[.]bin") -Message "High-cardinality manifest never leaks protected file paths"
    $scaleAuditRepeat = (& $scriptPath -Mode Audit -WorkspaceRoot $scaleWorkspace -NoTimelineLedger) | ConvertFrom-Json
    Assert-Equal -Expected $scaleAudit.audit_token -Observed $scaleAuditRepeat.audit_token -Message "Locally ordinal metadata traversal is deterministic"

    $ambiguousWorkspace = Join-Path $testRoot "ambiguous-workspace"
    $ambiguousDatabase = Join-Path $ambiguousWorkspace ".facial\media\surrealdb"
    New-Item -ItemType Directory -Force -Path $ambiguousDatabase | Out-Null
    [IO.File]::WriteAllText((Join-Path $ambiguousDatabase "data"), "keep", [Text.UTF8Encoding]::new($false))
    $ambiguousRejected = $false
    try {
        & $scriptPath -Mode Audit -WorkspaceRoot $ambiguousWorkspace | Out-Null
    } catch {
        $ambiguousRejected = $_.Exception.Message -match "Ambiguous current media state"
    }
    Assert-True -Condition $ambiguousRejected -Message "Unmarked current store fails closed"
    Assert-True -Condition (Test-Path -LiteralPath (Join-Path $ambiguousDatabase "data") -PathType Leaf) -Message "Ambiguous store remains untouched"
    Assert-True -Condition (-not (Test-Path -LiteralPath (Join-Path $ambiguousWorkspace ".facial-media-retirement"))) -Message "Ambiguous preflight creates no retirement state"

    $rollbackWorkspace = Join-Path $testRoot "rollback-workspace"
    $rollbackMedia = Join-Path $rollbackWorkspace ".facial\media"
    $rollbackDatabase = Join-Path $rollbackMedia "surrealdb"
    New-Item -ItemType Directory -Force -Path $rollbackDatabase | Out-Null
    $rollbackData = Join-Path $rollbackDatabase "data"
    [IO.File]::WriteAllBytes($rollbackData, [byte[]](101, 102, 103, 104))
    [IO.File]::WriteAllText(
        (Join-Path $rollbackMedia "engine.json"),
        '{"engine":"surrealdb","engine_version":"3.2.4","namespace":"facial","database":"application","schema_version":1}',
        [Text.UTF8Encoding]::new($false)
    )
    $rollbackHash = Get-TestHash -Path $rollbackData
    $timelineUnscopedAudit = (& $scriptPath -Mode Audit -WorkspaceRoot $rollbackWorkspace) | ConvertFrom-Json
    $timelineAckRejected = $false
    try {
        & $scriptPath -Mode Execute -WorkspaceRoot $rollbackWorkspace -RunId "wp-079-test-no-timeline-ack" -AuditToken $timelineUnscopedAudit.audit_token | Out-Null
    } catch {
        $timelineAckRejected = $_.Exception.Message -match "NoTimelineLedger"
    }
    Assert-True -Condition $timelineAckRejected -Message "Execute requires anchored Timeline proof or explicit no-ledger acknowledgement"
    Assert-True -Condition (-not (Test-Path -LiteralPath (Join-Path $rollbackWorkspace ".facial-media-retirement"))) -Message "Missing Timeline acknowledgement creates no retirement state"

    $rollbackAudit = (& $scriptPath -Mode Audit -WorkspaceRoot $rollbackWorkspace -NoTimelineLedger) | ConvertFrom-Json
    $priorFailureInjection = [Environment]::GetEnvironmentVariable("FACIAL_WP079_TEST_FAIL_AFTER_MOVE_ID", "Process")
    [Environment]::SetEnvironmentVariable("FACIAL_WP079_TEST_FAIL_AFTER_MOVE_ID", "current-surrealdb", "Process")
    $rollbackRejected = $false
    $rollbackError = $null
    try {
        & $scriptPath -Mode Execute -WorkspaceRoot $rollbackWorkspace -RunId "wp-079-test-rollback" -NoTimelineLedger -AuditToken $rollbackAudit.audit_token | Out-Null
    } catch {
        $rollbackRejected = $true
        $rollbackError = $_.Exception.Message
    } finally {
        [Environment]::SetEnvironmentVariable("FACIAL_WP079_TEST_FAIL_AFTER_MOVE_ID", $priorFailureInjection, "Process")
    }
    Assert-True -Condition $rollbackRejected -Message "Injected post-move verification failure is surfaced ($rollbackError)"
    Assert-True -Condition (Test-Path -LiteralPath $rollbackData -PathType Leaf) -Message "Post-move verification failure restores live source"
    Assert-Equal -Expected $rollbackHash -Observed (Get-TestHash -Path $rollbackData) -Message "Restored source is byte-identical"
    Assert-True -Condition (Test-Path -LiteralPath (Join-Path $rollbackMedia "engine.json") -PathType Leaf) -Message "Marker-first quarantine restores the marker during rollback"
    $rollbackManifestPath = Join-Path $rollbackWorkspace ".facial-media-retirement\wp-079-test-rollback\manifest.json"
    Assert-True -Condition (Test-Path -LiteralPath $rollbackManifestPath -PathType Leaf) -Message "Rollback manifest exists ($rollbackError)"
    $rollbackManifest = Get-Content -LiteralPath $rollbackManifestPath -Raw | ConvertFrom-Json
    Assert-True -Condition ($rollbackManifest.error -match "Injected WP-079 post-move verification failure") -Message "Manifest records injected verification failure"
    Assert-Equal -Expected "failed-source-preserved" -Observed $rollbackManifest.status -Message "Rollback manifest status"
    Assert-True -Condition ([bool]$rollbackManifest.rollback.attempted) -Message "Rollback attempt recorded"
    Assert-True -Condition ([bool]$rollbackManifest.rollback.complete) -Message "Rollback completion recorded"

    $preMoveAudit = (& $scriptPath -Mode Audit -WorkspaceRoot $rollbackWorkspace -NoTimelineLedger) | ConvertFrom-Json
    $priorPreMoveInjection = [Environment]::GetEnvironmentVariable("FACIAL_WP079_TEST_FAIL_BEFORE_MOVE_ID", "Process")
    [Environment]::SetEnvironmentVariable("FACIAL_WP079_TEST_FAIL_BEFORE_MOVE_ID", "current-surrealdb", "Process")
    $preMoveRejected = $false
    try {
        & $scriptPath -Mode Execute -WorkspaceRoot $rollbackWorkspace -RunId "wp-079-test-before-move" -NoTimelineLedger -AuditToken $preMoveAudit.audit_token | Out-Null
    } catch {
        $preMoveRejected = $_.Exception.Message -match "Injected WP-079 pre-move failure"
    } finally {
        [Environment]::SetEnvironmentVariable("FACIAL_WP079_TEST_FAIL_BEFORE_MOVE_ID", $priorPreMoveInjection, "Process")
    }
    Assert-True -Condition $preMoveRejected -Message "Injected pre-move failure is surfaced"
    Assert-True -Condition (Test-Path -LiteralPath $rollbackData -PathType Leaf) -Message "Pre-move failure leaves source live"
    Assert-Equal -Expected $rollbackHash -Observed (Get-TestHash -Path $rollbackData) -Message "Pre-move source remains byte-identical"
    $preMoveManifestPath = Join-Path $rollbackWorkspace ".facial-media-retirement\wp-079-test-before-move\manifest.json"
    $preMoveManifest = Get-Content -LiteralPath $preMoveManifestPath -Raw | ConvertFrom-Json
    Assert-Equal -Expected "failed-source-preserved" -Observed $preMoveManifest.status -Message "Pre-move manifest status"
    Assert-True -Condition ([bool]$preMoveManifest.rollback.complete) -Message "Pre-move rollback completed"
    Assert-Equal -Expected "not-moved-source-intact" -Observed @($preMoveManifest.rollback.details)[0].status -Message "Pre-move source-intact state recorded"

    $cleanupWorkspace = Join-Path $testRoot "installer-cleanup-workspace"
    $cleanupMedia = Join-Path $cleanupWorkspace ".facial\media"
    $cleanupDatabase = Join-Path $cleanupMedia "surrealdb"
    New-Item -ItemType Directory -Force -Path $cleanupDatabase | Out-Null
    $cleanupData = Join-Path $cleanupDatabase "data"
    [IO.File]::WriteAllBytes($cleanupData, [byte[]](111, 112, 113, 114))
    [IO.File]::WriteAllText(
        (Join-Path $cleanupMedia "engine.json"),
        '{"engine":"surrealdb","engine_version":"3.2.4","namespace":"facial","database":"application","schema_version":1}',
        [Text.UTF8Encoding]::new($false)
    )
    $cleanupAudit = (& $scriptPath -Mode Audit -WorkspaceRoot $cleanupWorkspace -NoTimelineLedger) | ConvertFrom-Json
    $cleanupExecute = (& $scriptPath -Mode Execute -WorkspaceRoot $cleanupWorkspace -NoTimelineLedger -RunId "wp-079-test-installer-cleanup" -AuditToken $cleanupAudit.audit_token) | ConvertFrom-Json
    $cleanupTarget = @($cleanupExecute.targets | Where-Object { $_.id -eq "current-surrealdb" })[0]
    $coldData = Join-Path $cleanupTarget.cold_backup_path "data"
    $quarantineData = Join-Path $cleanupTarget.quarantine_path "data"
    $preservedHash = Get-TestHash -Path $coldData
    Assert-Equal -Expected $preservedHash -Observed (Get-TestHash -Path $quarantineData) -Message "Installer-cleanup fixture has two verified copies"
    $cleanupWorkspaceFull = [IO.Path]::GetFullPath($cleanupWorkspace)
    $cleanupFacialFull = [IO.Path]::GetFullPath((Join-Path $cleanupWorkspace ".facial"))
    $expectedCleanupFacial = [IO.Path]::GetFullPath((Join-Path $cleanupWorkspaceFull ".facial"))
    if (-not $cleanupFacialFull.Equals($expectedCleanupFacial, [StringComparison]::OrdinalIgnoreCase) -or
        -not $cleanupFacialFull.StartsWith($cleanupWorkspaceFull + '\', [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing unsafe installer-cleanup fixture target: $cleanupFacialFull"
    }
    Remove-Item -LiteralPath $cleanupFacialFull -Recurse -Force
    Assert-True -Condition (Test-Path -LiteralPath $coldData -PathType Leaf) -Message "Cold backup survives explicit .facial cleanup"
    Assert-True -Condition (Test-Path -LiteralPath $quarantineData -PathType Leaf) -Message "Quarantine survives explicit .facial cleanup"
    Assert-Equal -Expected $preservedHash -Observed (Get-TestHash -Path $coldData) -Message "Cold backup remains byte-identical after .facial cleanup"
    Assert-Equal -Expected $preservedHash -Observed (Get-TestHash -Path $quarantineData) -Message "Quarantine remains byte-identical after .facial cleanup"

    $fakeFacialExe = Join-Path $testRoot "facial.exe"
    Copy-Item -LiteralPath $env:ComSpec -Destination $fakeFacialExe
    $fakeFacialProcess = Start-Process -FilePath $fakeFacialExe -ArgumentList "/d", "/q", "/k" -WindowStyle Hidden -PassThru
    try {
        Start-Sleep -Milliseconds 250
        $processGuardRejected = $false
        try {
            & $scriptPath -Mode Audit -WorkspaceRoot $workspace | Out-Null
        } catch {
            $processGuardRejected = $_.Exception.Message -match "while Facial is running"
        }
        Assert-True -Condition $processGuardRejected -Message "Running Facial process blocks audit and execute"
    } finally {
        if (-not $fakeFacialProcess.HasExited) {
            Stop-Process -Id $fakeFacialProcess.Id -Force
            $fakeFacialProcess.WaitForExit()
        }
    }
} finally {
    if (Test-Path -LiteralPath $testRoot) {
        $resolvedTestRoot = [IO.Path]::GetFullPath($testRoot)
        if (-not $resolvedTestRoot.StartsWith($tempBase + '\facial-wp079-test-', [StringComparison]::OrdinalIgnoreCase)) {
            throw "Refusing to clean unexpected test root: $resolvedTestRoot"
        }
        Remove-Item -LiteralPath $resolvedTestRoot -Recurse -Force
    }
}

Write-Host "OK: WP-079 retirement script audit/execute safety tests passed."
