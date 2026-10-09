param(
    [Parameter(Mandatory = $true)][string]$PortableExe,
    [Parameter(Mandatory = $true)][Alias('PackageAssetsRoot')][string]$VerifiedPayloadRoot,
    [Parameter(Mandatory = $true)][string]$CandidateIdentity,
    [Parameter(Mandatory = $true)][string]$HardwareManifest,
    [Parameter(Mandatory = $true)][string]$DisplayProfile,
    [Parameter(Mandatory = $true)][string]$OutputRoot,
    [string]$PythonExe = 'python',
    [switch]$DiagnosticPhaseProfile,
    [switch]$PuffinSwapProfile,
    [string]$ResolvedFeatureGraph,
    [string]$ResolvedFeatureGraphReceipt
)

$ErrorActionPreference = 'Stop'
$utf8 = New-Object Text.UTF8Encoding($false)
function Resolve-Existing([string]$Path) { return (Resolve-Path -LiteralPath $Path).Path }
function Get-Digest([string]$Path) { return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }
function Normalized-WindowsPath([string]$Path) {
    $full = [IO.Path]::GetFullPath($Path)
    if ($full.StartsWith('\\?\UNC\', [StringComparison]::OrdinalIgnoreCase)) { return '\\' + $full.Substring(8) }
    if ($full.StartsWith('\\?\', [StringComparison]::OrdinalIgnoreCase)) { return $full.Substring(4) }
    return $full
}
function Write-Json([string]$Path, $Value) { [IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 30), $utf8) }
function Read-Json([string]$Path, [long]$MaxBytes = 65536) {
    $file = Get-Item -LiteralPath $Path -ErrorAction Stop
    Require (-not $file.PSIsContainer -and -not ($file.Attributes -band [IO.FileAttributes]::ReparsePoint) -and $file.Length -le $MaxBytes) 'JSON evidence must be a bounded regular file'
    return (Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json)
}
function Require([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }

$portable = Resolve-Existing $PortableExe
$payload = Resolve-Existing $VerifiedPayloadRoot
$cli = Resolve-Existing (Join-Path $payload 'facial-cli.exe')
$identityPath = Resolve-Existing $CandidateIdentity
$hardwarePath = Resolve-Existing $HardwareManifest
$displayPath = Resolve-Existing $DisplayProfile
$python = (Get-Command $PythonExe -ErrorAction Stop).Source
$analyzer = Join-Path $PSScriptRoot 'analyze-match-render-samples.py'
$scriptPath = $MyInvocation.MyCommand.Path
Require ([IO.File]::Exists($portable)) 'PortableExe must be an existing regular file'
Require ([IO.Directory]::Exists($payload)) 'VerifiedPayloadRoot must contain the independently verified FACIALVERIFY minimal payload'
$identity = Read-Json $identityPath
if ($DiagnosticPhaseProfile) {
    $guardedGui = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../build-artifacts/cargo/release/facial.exe'))
    Require ([string]::Equals((Normalized-WindowsPath $portable), (Normalized-WindowsPath $guardedGui), [StringComparison]::OrdinalIgnoreCase)) 'Diagnostic GUI must be the guard-owned release build; diagnostic observations cannot accept a release'
    $artifactVersion = $identity.app_version
} else {
    Require ((Split-Path -Leaf $portable) -match '^facial-portable-(\d+\.\d+\.\d+)\.exe$') 'PortableExe must be the versioned canonical portable artifact'
    $artifactVersion = $Matches[1]
}
Require ($artifactVersion -cmatch '^\d+\.\d+\.\d+$') 'Candidate app_version must be a numeric version'
Require ($identity.git_commit -cmatch '^[0-9a-f]{40}([0-9a-f]{24})?$') 'Candidate git_commit must be a full lowercase Git object ID'
Require ($identity.app_version -ceq $artifactVersion) 'Candidate identity app_version differs from portable artifact name'
foreach ($field in @('cargo_lock_sha256', 'build_ui_sha256', 'build_lib_sha256', 'build_collector_sha256', 'packaged_cli_sha256')) {
    Require ($identity.$field -cmatch '^[0-9a-f]{64}$') "Candidate identity lacks exact $field"
}
Require ((Get-Digest $cli) -ceq $identity.packaged_cli_sha256) 'Extracted CLI differs from candidate package identity'
Require ($identity.schema_generation -is [string] -and -not [string]::IsNullOrWhiteSpace($identity.schema_generation)) 'Candidate identity requires independently verified string schema_generation'
$hardware = Read-Json $hardwarePath
Require (-not [string]::IsNullOrWhiteSpace($hardware.power_mode)) 'Hardware manifest requires independently observed power_mode'
$display = Read-Json $displayPath
Require ($display.viewport_physical_px.Count -eq 2 -and $display.viewport_physical_px[0] -eq 1920 -and $display.viewport_physical_px[1] -eq 1080 -and $display.dpi_scale_percent -eq 100 -and $display.egui_pixels_per_point -eq 1 -and $display.font_family -ceq 'Inter' -and $display.font_size_pt -eq 19) 'Reference display must be 1920x1080, 100% DPI, 1 pixel per point, Inter 19pt'

Require (-not $PuffinSwapProfile -or $DiagnosticPhaseProfile) 'Puffin swap profiling requires DiagnosticPhaseProfile'
Require ($PuffinSwapProfile -or (-not $ResolvedFeatureGraph -and -not $ResolvedFeatureGraphReceipt)) 'Feature graph inputs require PuffinSwapProfile'
if ($PuffinSwapProfile) {
    $graphPath = Resolve-Existing $ResolvedFeatureGraph
    $graphReceiptPath = Resolve-Existing $ResolvedFeatureGraphReceipt
    $graph = Read-Json $graphPath (32 * 1024 * 1024)
    $graphReceipt = Read-Json $graphReceiptPath
    Require ($graphReceipt.schema_version -eq 1 -and $graphReceipt.host_triple -ceq 'x86_64-pc-windows-msvc') 'Graph receipt requires actual guarded Windows x64 MSVC target'
    $expectedArgs = @('metadata', '--format-version', '1', '--locked', '--features', 'media-label-puffin-profile', '--filter-platform', 'x86_64-pc-windows-msvc')
    Require (($graphReceipt.command_args | ConvertTo-Json -Compress) -ceq ($expectedArgs | ConvertTo-Json -Compress)) 'Graph receipt command differs from selected locked feature proof'
    Require ($graphReceipt.metadata_sha256 -ceq (Get-Digest $graphPath) -and $graphReceipt.cargo_lock_sha256 -ceq $identity.cargo_lock_sha256 -and $graphReceipt.cargo_manifest_sha256 -ceq (Get-Digest (Join-Path $PSScriptRoot '../Cargo.toml'))) 'Resolved graph source/lock binding differs from candidate'
    foreach ($field in @('build_ui_sha256', 'build_lib_sha256', 'build_collector_sha256')) { Require ($graphReceipt.$field -ceq $identity.$field) "Graph receipt differs from compiled candidate: $field" }
    Require ($graph.packages.Count -le 8192 -and $graph.resolve.nodes.Count -le 8192) 'Resolved graph exceeds node/package bounds'
    $puffinIds = @($graph.packages | Where-Object { $_.name -ceq 'puffin' -and $graph.resolve.nodes.id -ccontains $_.id } | ForEach-Object { $_.id })
    Require ($puffinIds.Count -eq 1) 'Resolved puffin identity is ambiguous'
    $active = @()
    foreach ($package in $graph.packages) {
        $nodes = @($graph.resolve.nodes | Where-Object { $_.id -ceq $package.id })
        Require ($nodes.Count -le 1) 'Duplicate resolved graph node'
        if ($nodes.Count -eq 0) { continue }
        $features = @($nodes[0].features)
        Require ($nodes[0].deps.Count -le 8192) 'Resolved dependency count exceeds bound'
        if ($nodes[0].deps.pkg -ccontains $puffinIds[0]) { Require ($package.name -cin @('facial', 'eframe', 'egui', 'epaint', 'egui-winit', 'egui_glow', 'egui-wgpu')) 'Uninspected puffin consumer could profile another thread' }
        if ($package.name -cin @('egui-winit', 'egui_glow', 'egui-wgpu')) { Require ($package.version -ceq '0.27.2') 'Integration graph differs from inspected pinned source' }
        Require ($features.Count -le 256) 'Resolved feature count exceeds bound'
        if ($package.name -ceq 'puffin') { Require ($package.version -ceq '0.19.1' -and @($features | Where-Object { $_ -cne 'default' }).Count -eq 0) 'Puffin graph must use exact 0.19.1 with only its empty default feature' }
        if ($package.name -ceq 'epaint') { Require (-not ($features -ccontains 'rayon')) 'Profiled background tessellation is forbidden' }
        if ($package.name -cin @('eframe', 'egui', 'epaint')) { Require ($package.version -ceq '0.27.2' -and $features -ccontains 'puffin') 'Framework graph differs from inspected pinned source' }
        if ($package.name -ceq 'eframe') { Require ($features -ccontains 'glow') 'Resolved graph lacks inspected Glow renderer' }
        if ($package.name -ceq 'facial') { Require ($package.version -ceq $identity.app_version -and $features -ccontains 'media-label-puffin-profile') 'Resolved graph lacks selected Facial diagnostic feature' }
        $active += $package
    }
    foreach ($name in @('facial', 'puffin', 'eframe', 'egui', 'epaint')) { Require (@($active | Where-Object { $_.name -ceq $name }).Count -eq 1) "Resolved graph package identity is ambiguous: $name" }
}

$output = [IO.Path]::GetFullPath($OutputRoot)
Require (-not (Test-Path -LiteralPath $output)) 'OutputRoot must be fresh; rejected runs are never overwritten'
[void][IO.Directory]::CreateDirectory($output)
$evidence = Join-Path $output 'evidence'
[void][IO.Directory]::CreateDirectory($evidence)
$inputs = @{
    portable_executable_path = $portable
    hardware_manifest_path = $hardwarePath
    display_profile_path = $displayPath
    input_script_path = $scriptPath
    candidate_identity_path = $identityPath
    packaged_cli_path = $cli
}
if ($PuffinSwapProfile) { $inputs.resolved_feature_graph_path = $graphPath; $inputs.resolved_feature_graph_receipt_path = $graphReceiptPath }
$digests = @{}
foreach ($field in $inputs.Keys) { $digests[$field] = Get-Digest $inputs[$field] }
$copyFields = @('hardware_manifest_path', 'display_profile_path', 'input_script_path', 'candidate_identity_path')
if ($PuffinSwapProfile) { $copyFields += @('resolved_feature_graph_path', 'resolved_feature_graph_receipt_path') }
foreach ($field in $copyFields) {
    $copy = Join-Path $evidence (Split-Path -Leaf $inputs[$field])
    Require (-not (Test-Path -LiteralPath $copy)) 'Evidence basenames collide'
    Copy-Item -LiteralPath $inputs[$field] -Destination $copy
    Require ((Get-Digest $copy) -ceq $digests[$field]) 'Evidence changed while copying'
    $inputs[$field] = $copy
}

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class MediaLabelBenchmarkFocus {
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
}
'@

$savedEnvironment = @{}
$environmentNames = @('FACIAL_REPO_ROOT', 'FACIAL_WORKSPACE_ROOT', 'FACIAL_CONFIG_PATH', 'FACIAL_MATCH_BENCHMARK_CONFIG', 'FACIAL_DATA_ROOT', 'FACIAL_WORKTREES_ROOT', 'FACIAL_API_ROOT', 'FACIAL_DEBUG_LOG', 'FACIAL_MODEL_REGISTRY', 'FACIAL_FONT_SIZE', 'FACIAL_MEDIA_LABEL_PHASE_PROFILE', 'FACIAL_MEDIA_LABEL_PUFFIN_SWAP_PROFILE', 'FACIAL_MEDIA_LABEL_PUFFIN_GRAPH', 'FACIAL_MEDIA_LABEL_PUFFIN_GRAPH_RECEIPT')
foreach ($name in $environmentNames) { $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
$runs = @()
$receipts = @()
$failure = $null
try {
    foreach ($name in $environmentNames) { Remove-Item -LiteralPath ('Env:' + $name) -ErrorAction SilentlyContinue }
    if ($DiagnosticPhaseProfile) { $env:FACIAL_MEDIA_LABEL_PHASE_PROFILE = '1' }
    if ($PuffinSwapProfile) {
        $env:FACIAL_MEDIA_LABEL_PUFFIN_SWAP_PROFILE = '1'
        $env:FACIAL_MEDIA_LABEL_PUFFIN_GRAPH = $inputs.resolved_feature_graph_path
        $env:FACIAL_MEDIA_LABEL_PUFFIN_GRAPH_RECEIPT = $inputs.resolved_feature_graph_receipt_path
    }
    for ($index = 0; $index -lt 4; $index++) {
        foreach ($field in $inputs.Keys) { Require ((Get-Digest $inputs[$field]) -ceq $digests[$field]) "Immutable input changed before run: $field" }
        $state = if ($index -in @(0, 3)) { 'media_labels_baseline' } else { 'media_labels_candidate' }
        $runId = 'media-label-' + $index + '-' + [guid]::NewGuid().ToString('N')
        $workspace = Join-Path $output ('workspace-' + $index)
        [void][IO.Directory]::CreateDirectory($workspace)
        $configPath = Join-Path $output ($runId + '-config.json')
        $config = @{
            schema_version = 1; run_id = $runId; state = $state
            git_commit = $identity.git_commit; model_generation = 'unconfigured'
            schema_generation = $identity.schema_generation; fixture_generation = 'wp087-native-media-labels-50000-v1'
            cache_state = 'fresh-process-workspace-30s-warmup'; input_script_sha256 = $digests.input_script_path
            hardware_manifest_sha256 = $digests.hardware_manifest_path; display_profile_sha256 = $digests.display_profile_path
            power_mode = $hardware.power_mode; admission_counts = $null; admission_evidence = $null
            media_labels_workspace = $workspace
        }
        Write-Json $configPath $config
        $env:FACIAL_REPO_ROOT = $payload
        $env:FACIAL_WORKSPACE_ROOT = Join-Path $output 'configuration-workspace'
        $env:FACIAL_CONFIG_PATH = Join-Path $output ($runId + '-settings.json')
        $env:FACIAL_MATCH_BENCHMARK_CONFIG = $configPath
        $env:FACIAL_FONT_SIZE = '19'
        $stdout = Join-Path $output ($runId + '-stdout.log')
        $stderr = Join-Path $output ($runId + '-stderr.log')
        $rawPath = Join-Path $workspace ('.facial/benchmarks/' + $runId + '.jsonl')
        $process = Start-Process -FilePath $portable -ArgumentList @('--background', '--media-label-benchmark') -WindowStyle Hidden -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
        $started = $process.StartTime
        $timer = [Diagnostics.Stopwatch]::StartNew()
        $terminal = $null
        $focusViolation = $false
        $captureStarted = $false
        $receipt = [ordered]@{ run_id = $runId; state = $state; gui_pid = $process.Id; process_start_utc = $started.ToUniversalTime().ToString('o'); raw_path = $rawPath; terminal_outcome = $null; owned_process_exited = $false; foreground_samples = 0; foreground_violation = $false; acceptance = 'unverified'; startup_elapsed_ms = $null; header_observed_utc = $null; startup_bound_seconds = 180; capture_bound_seconds = 180; capture_wait_elapsed_ms = $null }
        try {
            $header = $null
            while ($timer.Elapsed.TotalSeconds -lt 180) {
                [uint32]$foreground = 0
                [void][MediaLabelBenchmarkFocus]::GetWindowThreadProcessId([MediaLabelBenchmarkFocus]::GetForegroundWindow(), [ref]$foreground)
                $receipt.foreground_samples++
                if ($foreground -eq $process.Id) { $focusViolation = $true }
                $process.Refresh()
                Require (-not $process.HasExited) 'Owned GUI exited before correlated capture header'
                if (Test-Path -LiteralPath $rawPath -PathType Leaf) {
                    $firstLine = Get-Content -LiteralPath $rawPath -TotalCount 1
                    # A writer may still be publishing its first JSON line.
                    # Only JSON decoding is retried; valid-but-wrong evidence is rejected.
                    try { $header = $firstLine | ConvertFrom-Json -ErrorAction Stop } catch { $header = $null }
                    if ($null -ne $header) {
                        Require ($header.schema_version -eq 1 -and $header.record_type -ceq 'run' -and $header.run_id -ceq $runId -and $header.state -ceq $state) 'Actual capture header differs from requested run identity'
                        if ($DiagnosticPhaseProfile) {
                            Require ($header.diagnostic_only -is [bool] -and $header.diagnostic_only) 'Diagnostic producer must mark its raw capture ineligible for canonical acceptance'
                        } else {
                            Require ($null -eq $header.diagnostic_only) 'Canonical acquisition rejects diagnostic-only capture headers'
                        }
                        Require ($header.measurement_start_us -eq 30000000 -and $header.measurement_end_us -eq 150000000 -and $header.warmup_seconds -eq 30) 'Actual capture header differs from unchanged 30-second warmup and 120-second measurement'
                        Require ($header.package_sha256 -ceq $digests.portable_executable_path -and $header.app_version -ceq $identity.app_version -and $header.cargo_lock_sha256 -ceq $identity.cargo_lock_sha256 -and $header.git_commit -ceq $identity.git_commit -and $header.schema_generation -ceq $identity.schema_generation) 'Actual producer build identity differs from independently supplied candidate identity'
                        foreach ($field in @('build_ui_sha256', 'build_lib_sha256', 'build_collector_sha256')) {
                            Require ($header.media_labels_fixture.$field -ceq $identity.$field) "Actual compiled source differs from candidate identity: $field"
                        }
                        $receipt.header_observed_utc = [DateTime]::UtcNow.ToString('o')
                        break
                    }
                }
                Start-Sleep -Milliseconds 250
            }
            $receipt.startup_elapsed_ms = $timer.ElapsedMilliseconds
            Require ($null -ne $header) 'Owned GUI exceeded 180-second startup bound without correlated valid capture header'
            $timer.Restart()
            $captureStarted = $true
            while ($timer.Elapsed.TotalSeconds -lt 180) {
                [uint32]$foreground = 0
                [void][MediaLabelBenchmarkFocus]::GetWindowThreadProcessId([MediaLabelBenchmarkFocus]::GetForegroundWindow(), [ref]$foreground)
                $receipt.foreground_samples++
                if ($foreground -eq $process.Id) { $focusViolation = $true }
                if (Test-Path -LiteralPath $rawPath -PathType Leaf) {
                    try {
                        $last = Get-Content -LiteralPath $rawPath -Tail 1 | ConvertFrom-Json
                        if ($last.record_type -eq 'end') { $terminal = $last; break }
                    } catch { }
                }
                $process.Refresh()
                if ($process.HasExited) { break }
                Start-Sleep -Milliseconds 250
            }
            $receipt.capture_wait_elapsed_ms = $timer.ElapsedMilliseconds
            if ($terminal) { $receipt.terminal_outcome = $terminal.outcome }
            $receipt.foreground_violation = $focusViolation
            Require ($null -ne $terminal) 'Owned capture exited or exceeded 180 seconds without terminal evidence'
            Require ($terminal.outcome -ceq 'completed') ('Native capture invalid: ' + $terminal.outcome)
            Require ($terminal.runtime_evidence -is [pscustomobject]) 'Native capture omitted runtime admission evidence'
            foreach ($field in @('match_database_requests', 'match_workers', 'model_loads', 'match_index_queries')) {
                $observedCount = $terminal.runtime_evidence.$field
                Require (($observedCount -is [long] -or $observedCount -is [int]) -and $observedCount -eq 0) "Native zero-admission baseline rejected: $field=$observedCount"
            }
            Require (-not $focusViolation) 'Owned background GUI held foreground focus during sampled observation'
            # Measurement is terminal before CLI dispatch or exact-framebuffer capture.
            $env:FACIAL_WORKSPACE_ROOT = $workspace
            Remove-Item Env:FACIAL_MATCH_BENCHMARK_CONFIG -ErrorAction SilentlyContinue
            $snapshotStdout = Join-Path $output ($runId + '-snapshot-cli.json')
            $snapshotStderr = Join-Path $output ($runId + '-snapshot-cli-stderr.log')
            Require ((Get-Digest $cli) -ceq $identity.packaged_cli_sha256) 'Verified extracted CLI changed before capture'
            $snapshotProcess = Start-Process -FilePath $cli -ArgumentList @('ui_snapshot', '--out', 'benchmark-final.png') -WindowStyle Hidden -PassThru -RedirectStandardOutput $snapshotStdout -RedirectStandardError $snapshotStderr
            $receipt.snapshot_cli_pid = $snapshotProcess.Id
            $snapshotStarted = $snapshotProcess.StartTime
            try {
                Require ($snapshotProcess.WaitForExit(5000)) 'Owned snapshot CLI exceeded dispatch deadline'
                Require ($snapshotProcess.ExitCode -eq 0) 'Owned snapshot CLI rejected dispatch'
                $accepted = Read-Json $snapshotStdout
                Require ($accepted.kind -ceq 'ui_snapshot' -and $accepted.action_id -cmatch '^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$') 'Snapshot CLI did not return a correlated ui_snapshot action'
                $snapshotReceiptPath = Join-Path $workspace ('.facial/data/api/receipts/' + $accepted.action_id + '.json')
                $snapshotTimer = [Diagnostics.Stopwatch]::StartNew()
                $snapshotReceipt = $null
                while ($snapshotTimer.Elapsed.TotalSeconds -lt 20) {
                    try { $snapshotReceipt = Read-Json $snapshotReceiptPath } catch { $snapshotReceipt = $null }
                    if ($snapshotReceipt -and $snapshotReceipt.status -notin @('accepted', 'processing')) { break }
                    Start-Sleep -Milliseconds 100
                }
                Require ($snapshotReceipt -and $snapshotReceipt.action_id -ceq $accepted.action_id -and $snapshotReceipt.kind -ceq 'ui_snapshot' -and $snapshotReceipt.status -ceq 'applied') 'Exact live snapshot did not produce an applied correlated receipt'
                $snapshotPath = Join-Path $workspace '.facial/ui-snapshots/live-ui/benchmark-final.png'
                Require ($snapshotReceipt.result.foreground_activation -eq $false -and $snapshotReceipt.result.capture_exists -eq $true -and [string]::Equals((Normalized-WindowsPath $snapshotReceipt.result.capture_path), (Normalized-WindowsPath $snapshotPath), [StringComparison]::OrdinalIgnoreCase)) 'Snapshot receipt does not attest the exact confined background capture'
                Require ($snapshotReceipt.result.width_px -eq 1920 -and $snapshotReceipt.result.height_px -eq 1080) 'Exact live framebuffer differs from reference display dimensions'
                Require ((Get-Digest $snapshotPath) -ceq $snapshotReceipt.result.capture_sha256) 'Exact live PNG differs from capture receipt hash'
                $receipt.snapshot_path = $snapshotPath
                $receipt.snapshot_sha256 = $snapshotReceipt.result.capture_sha256
                $receipt.snapshot_action_id = $accepted.action_id
                Write-Json (Join-Path $output ($runId + '-snapshot-receipt.json')) $snapshotReceipt
                if ($DiagnosticPhaseProfile) {
                    $profilePath = Join-Path $workspace '.facial/benchmarks/media-label-phase-profile.json'
                    $profileByteLimit = if ($PuffinSwapProfile) { 40 * 1024 * 1024 } else { 32 * 1024 * 1024 }
                    $phaseProfile = Read-Json $profilePath $profileByteLimit
                    Require ($phaseProfile.byte_limit -eq $profileByteLimit) 'Diagnostic sidecar producer byte bound differs from acquisition mode'
                    Require ($phaseProfile.diagnostic_only -is [bool] -and $phaseProfile.diagnostic_only -and $phaseProfile.acceptance_verdict -ceq 'not_canonical_acceptance_evidence' -and $phaseProfile.outcome -ceq 'diagnostic_complete') 'Phase diagnostic export is missing, incomplete or mislabeled'
                    Require ($phaseProfile.source_identity.run_id -ceq $runId -and $phaseProfile.source_identity.state -ceq $state -and $phaseProfile.raw_sha256 -ceq (Get-Digest $rawPath)) 'Phase diagnostic export differs from its actual raw run'
                    Require (($phaseProfile.record_count -is [int] -or $phaseProfile.record_count -is [long]) -and $phaseProfile.record_count -ge 7200 -and $phaseProfile.record_count -le 20000 -and $phaseProfile.record_limit -eq 20000 -and $phaseProfile.record_count -eq $terminal.sample_count -and $phaseProfile.records.Count -eq $terminal.sample_count) 'Phase diagnostic did not retain every measured native frame within its record bound'
                    if ($PuffinSwapProfile) {
                        Require ($phaseProfile.swap_profile_inconsistent -is [bool] -and -not $phaseProfile.swap_profile_inconsistent -and $phaseProfile.swap_failure_code -eq 0 -and $phaseProfile.swap_graph_sha256 -ceq $digests.resolved_feature_graph_path) 'Actual swap diagnostic lacks valid bound feature graph'
                        Require ((Get-Digest (Join-Path $workspace '.facial/benchmarks/puffin-feature-graph.json')) -ceq $digests.resolved_feature_graph_path -and (Get-Digest (Join-Path $workspace '.facial/benchmarks/puffin-feature-graph-receipt.json')) -ceq $digests.resolved_feature_graph_receipt_path) 'Runtime confined graph copies differ from actual source proof'
                        foreach ($row in $phaseProfile.records) { Require ($null -ne $row.swap_buffers -and $row.swap_buffers.frame_number -eq $row.frame_number) 'Swap diagnostic lacks exactly paired native frame evidence' }
                        $receipt.swap_graph_sha256 = $digests.resolved_feature_graph_path
                        $receipt.swap_graph_receipt_sha256 = $digests.resolved_feature_graph_receipt_path
                    } else { Require ($null -eq $phaseProfile.swap_graph_sha256) 'Phase-only acquisition rejects unexpected swap profiling' }
                    $receipt.phase_profile_path = $profilePath
                    $receipt.phase_profile_sha256 = Get-Digest $profilePath
                }
            } finally {
                $snapshotProcess.Refresh()
                if (-not $snapshotProcess.HasExited) {
                    $snapshotCurrent = Get-Process -Id $snapshotProcess.Id -ErrorAction SilentlyContinue
                    Require ($snapshotCurrent -and $snapshotCurrent.StartTime -eq $snapshotStarted -and $snapshotCurrent.Path -eq $cli) 'Owned snapshot CLI identity changed; refusing stop'
                    Stop-Process -Id $snapshotCurrent.Id -ErrorAction Stop
                    Require ($snapshotProcess.WaitForExit(5000)) 'Owned snapshot CLI did not exit'
                }
            }
            $runs += @{ state = $state; path = $rawPath }
            $receipt.acceptance = if ($DiagnosticPhaseProfile) { 'diagnostic-only-not-release-evidence' } else { 'completed-awaiting-analyzer-and-independent-review' }
        } finally {
            if ($captureStarted -and $null -eq $receipt.capture_wait_elapsed_ms) { $receipt.capture_wait_elapsed_ms = $timer.ElapsedMilliseconds }
            elseif ($null -eq $receipt.startup_elapsed_ms) { $receipt.startup_elapsed_ms = $timer.ElapsedMilliseconds }
            $receipt.foreground_violation = $focusViolation
            $process.Refresh()
            if (-not $process.HasExited) {
                $current = Get-Process -Id $process.Id -ErrorAction SilentlyContinue
                Require ($current -and $current.StartTime -eq $started -and $current.Path -eq $portable) 'Owned process identity changed; refusing process stop'
                Stop-Process -Id $current.Id -ErrorAction Stop
                $receipt.owned_process_exited = $process.WaitForExit(5000)
            } else { $receipt.owned_process_exited = $true }
            $receipts += $receipt
            Write-Json (Join-Path $output 'owned-run-receipts.json') @{ schema_version = 1; runs = $receipts; focus_proof_scope = 'sampled-background-observation-not-continuous'; release_verdict = $(if ($DiagnosticPhaseProfile) { 'diagnostic-only-not-release-evidence' } else { 'pending-independent-review' }) }
        }
        Require ($receipt.owned_process_exited) 'Owned GUI did not exit within cleanup bound'
    }
    foreach ($field in $inputs.Keys) { Require ((Get-Digest $inputs[$field]) -ceq $digests[$field]) "Immutable input changed during ABBA: $field" }
    $manifestPath = Join-Path $output 'media-label-ab-manifest.json'
    Write-Json $manifestPath @{ schema_version = 1; runs = $runs; portable_executable_path = $inputs.portable_executable_path; hardware_manifest_path = $inputs.hardware_manifest_path; display_profile_path = $inputs.display_profile_path; input_script_path = $inputs.input_script_path }
    if ($DiagnosticPhaseProfile) {
        Write-Json (Join-Path $output 'diagnostic-result.json') @{ schema_version = 1; diagnostic_only = $true; acceptance_verdict = 'not_canonical_acceptance_evidence'; run_manifest_path = $manifestPath; run_manifest_sha256 = Get-Digest $manifestPath; runtime_artifact_kind = 'guarded-unpackaged-release-gui'; snapshot_cli_provenance = 'separately-verified-payload-cli-not-matched-source-build'; runs = $receipts }
    } else {
        & $python $analyzer --media-label-ab-manifest $manifestPath --output (Join-Path $output 'media-label-ab-result.json')
        if ($LASTEXITCODE -ne 0) { throw "Native Media ABBA analyzer failed with exit $LASTEXITCODE; retained evidence at $output" }
    }
} catch {
    $failure = $_.Exception.Message
    Write-Json (Join-Path $output 'rejected-run.json') @{ error = $failure; release_verdict = 'not-proven'; retained_workspaces = $true }
    throw
} finally {
    foreach ($name in $environmentNames) {
        if ($null -eq $savedEnvironment[$name]) { Remove-Item -LiteralPath ('Env:' + $name) -ErrorAction SilentlyContinue }
        else { [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process') }
    }
}
if ($DiagnosticPhaseProfile) { Write-Host "Phase diagnostics retained at $output; not canonical acceptance evidence" }
else { Write-Host "Native Media ABBA observations retained at $output; independent package/runtime review remains required" }
