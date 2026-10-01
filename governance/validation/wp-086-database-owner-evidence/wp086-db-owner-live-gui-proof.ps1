$ErrorActionPreference = 'Stop'
$taskRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$taskProof = Join-Path $PSScriptRoot 'wp086-db-owner-live-gui'
$taskWorkspace = Join-Path $taskProof 'workspace'
$taskGuiPath = Join-Path $PSScriptRoot 'cargo/release/facial.exe'
$taskCliPath = Join-Path $PSScriptRoot 'cargo/release/facial-cli.exe'
New-Item -ItemType Directory -Path $taskWorkspace -Force | Out-Null
$env:FACIAL_REPO_ROOT = $taskRoot
$env:FACIAL_WORKSPACE_ROOT = $taskWorkspace
$env:FACIAL_CONFIG_PATH = Join-Path $taskProof 'settings.json'
Remove-Item Env:FACIAL_DATA_ROOT -ErrorAction SilentlyContinue
Remove-Item Env:FACIAL_WORKTREES_ROOT -ErrorAction SilentlyContinue
'{}' | Set-Content -LiteralPath $env:FACIAL_CONFIG_PATH -Encoding utf8
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class OwnerProofWindows {
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);
}
'@
function Invoke-OwnedIntent([string[]]$Arguments, [switch]$AllowUnavailable) {
    $raw = & $taskCliPath @Arguments
    if ($LASTEXITCODE -ne 0) { throw "CLI intent failed: $Arguments" }
    $accepted = $raw | ConvertFrom-Json
    $path = Join-Path $taskWorkspace ".facial/data/api/receipts/$($accepted.action_id).json"
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    do {
        try { $receipt = Get-Content -LiteralPath $path -Raw | ConvertFrom-Json } catch { $receipt = $null }
        if ($receipt -and $receipt.status -notin @('accepted','processing')) {
            $receipt | ConvertTo-Json -Depth 40 | Set-Content -LiteralPath (Join-Path $taskProof "$($accepted.kind)-$($accepted.action_id).json") -Encoding utf8
            if ($receipt.status -ne 'applied' -and !($AllowUnavailable -and $receipt.error -eq 'Live Match runtime diagnostics unavailable')) { throw "Intent terminal status: $($receipt.status) $($receipt.error) $($receipt.note)" }
            return $receipt
        }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Owned intent did not settle: $Arguments"
}
$taskGui = Start-Process -FilePath $taskGuiPath -ArgumentList '--background' -WindowStyle Hidden -PassThru
$taskStarted = $taskGui.StartTime
try {
    $selected = Invoke-OwnedIntent -Arguments @('select_tab','--tab','match')
    $taskReadyDeadline = [DateTime]::UtcNow.AddSeconds(30)
    do {
        $diagnostics = Invoke-OwnedIntent -Arguments @('match_runtime_diagnostics') -AllowUnavailable
        if ($diagnostics.status -eq 'applied') { break }
        Start-Sleep -Milliseconds 200
    } while ([DateTime]::UtcNow -lt $taskReadyDeadline)
    $owner = $diagnostics.result.snapshot.execution.database_owner
    if ($owner.phase -ne 'ready' -or !$owner.pid) { throw 'GUI owner was not ready in the actual receipt' }
    $taskChild = Get-CimInstance Win32_Process -Filter "ProcessId = $($owner.pid)"
    if (!$taskChild -or $taskChild.ParentProcessId -ne $taskGui.Id -or $taskChild.CommandLine -notlike '*__database-owner-v1*') { throw 'Receipt owner was not the GUI-owned hidden child' }
    $taskChildProcess = Get-Process -Id $owner.pid
    if ($taskChildProcess.MainWindowHandle -ne 0) { throw 'Database owner exposed a main window' }
    [uint32]$taskForeground = 0
    [void][OwnerProofWindows]::GetWindowThreadProcessId([OwnerProofWindows]::GetForegroundWindow(), [ref]$taskForeground)
    if ($taskForeground -eq $taskGui.Id -or $taskForeground -eq $owner.pid) { throw 'Owned background GUI or owner held foreground focus' }
    $manual = Invoke-OwnedIntent -Arguments @('select_tab','--tab','manual')
    $capture = Invoke-OwnedIntent -Arguments @('ui_snapshot','--out','owner-manual.png')
    [uint32]$taskFinalForeground = 0
    [void][OwnerProofWindows]::GetWindowThreadProcessId([OwnerProofWindows]::GetForegroundWindow(), [ref]$taskFinalForeground)
    if ($taskFinalForeground -eq $taskGui.Id -or $taskFinalForeground -eq $owner.pid) { throw 'Owned GUI or owner held foreground focus after capture' }
    $taskResult = [pscustomobject]@{status='passed';gui_pid=$taskGui.Id;owner_pid=$owner.pid;owner_phase=$owner.phase;owner_window_handle=0;foreground_pid=$taskForeground;final_foreground_pid=$taskFinalForeground;diagnostics_action_id=$diagnostics.action_id;capture=$capture.result}
} finally {
    $taskCurrent = Get-Process -Id $taskGui.Id -ErrorAction SilentlyContinue
    if ($taskCurrent -and $taskCurrent.StartTime -eq $taskStarted -and $taskCurrent.Path -eq $taskGuiPath) { Stop-Process -Id $taskCurrent.Id -ErrorAction Stop }
}
$taskExitDeadline = [DateTime]::UtcNow.AddSeconds(5)
do {
    $taskGui.Refresh()
    $taskChildProcess.Refresh()
    if ($taskGui.HasExited -and $taskChildProcess.HasExited) { break }
    Start-Sleep -Milliseconds 100
} while ([DateTime]::UtcNow -lt $taskExitDeadline)
if (!$taskGui.HasExited -or !$taskChildProcess.HasExited) { throw 'Owned GUI or hidden owner did not exit after parent close' }
$taskResult | Add-Member -NotePropertyName owned_processes_exited -NotePropertyValue $true
$taskResult | ConvertTo-Json -Depth 30 | Set-Content -LiteralPath (Join-Path $taskProof 'result.json') -Encoding utf8
