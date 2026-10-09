param(
    [Parameter(Mandatory = $true)][string]$PortableExe,
    [Parameter(Mandatory = $true)][Alias('PackageAssetsRoot')][string]$VerifiedPayloadRoot,
    [Parameter(Mandatory = $true)][string]$CandidateIdentity,
    [Parameter(Mandatory = $true)][string]$HardwareManifest,
    [Parameter(Mandatory = $true)][string]$DisplayProfile,
    [Parameter(Mandatory = $true)][string]$OutputRoot,
    [string]$PythonExe = 'python',
    [switch]$DiagnosticPhaseProfile
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
$digests = @{}
foreach ($field in $inputs.Keys) { $digests[$field] = Get-Digest $inputs[$field] }
foreach ($field in @('hardware_manifest_path', 'display_profile_path', 'input_script_path', 'candidate_identity_path')) {
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
$environmentNames = @('FACIAL_REPO_ROOT', 'FACIAL_WORKSPACE_ROOT', 'FACIAL_CONFIG_PATH', 'FACIAL_MATCH_BENCHMARK_CONFIG', 'FACIAL_DATA_ROOT', 'FACIAL_WORKTREES_ROOT', 'FACIAL_API_ROOT', 'FACIAL_DEBUG_LOG', 'FACIAL_MODEL_REGISTRY', 'FACIAL_FONT_SIZE', 'FACIAL_MEDIA_LABEL_PHASE_PROFILE')
foreach ($name in $environmentNames) { $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
$runs = @()
$receipts = @()
$failure = $null
try {
    foreach ($name in $environmentNames) { Remove-Item -LiteralPath ('Env:' + $name) -ErrorAction SilentlyContinue }
    if ($DiagnosticPhaseProfile) { $env:FACIAL_MEDIA_LABEL_PHASE_PROFILE = '1' }
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
                    $phaseProfile = Read-Json $profilePath (20 * 1024 * 1024)
                    Require ($phaseProfile.diagnostic_only -is [bool] -and $phaseProfile.diagnostic_only -and $phaseProfile.acceptance_verdict -ceq 'not_canonical_acceptance_evidence' -and $phaseProfile.outcome -ceq 'diagnostic_complete') 'Phase diagnostic export is missing, incomplete or mislabeled'
                    Require ($phaseProfile.source_identity.run_id -ceq $runId -and $phaseProfile.source_identity.state -ceq $state -and $phaseProfile.raw_sha256 -ceq (Get-Digest $rawPath)) 'Phase diagnostic export differs from its actual raw run'
                    Require (($phaseProfile.record_count -is [int] -or $phaseProfile.record_count -is [long]) -and $phaseProfile.record_count -ge 7200 -and $phaseProfile.record_count -le 20000 -and $phaseProfile.record_limit -eq 20000 -and $phaseProfile.record_count -eq $terminal.sample_count -and $phaseProfile.records.Count -eq $terminal.sample_count) 'Phase diagnostic did not retain every measured native frame within its record bound'
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
