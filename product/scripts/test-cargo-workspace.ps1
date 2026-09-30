# WP-088: exercise the actual guard/compiler on a dependency-free isolated repo.
$ErrorActionPreference = 'Stop'
$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..')).TrimEnd('\', '/')
$guard = Join-Path $PSScriptRoot 'cargo-workspace.ps1'
$baselineEnvironment = @{}
foreach ($name in @('CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET_DIR', 'CARGO_BUILD_BUILD_DIR', 'TEMP', 'TMP', 'TMPDIR')) {
    $baselineEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$probe = (& $guard -Probe | ConvertFrom-Json)
$mutex = New-Object Threading.Mutex($false, $probe.mutex)
$held = $false
$fixture = Join-Path $repo ('build-artifacts/tmp/cargo-guard-' + [Guid]::NewGuid().ToString('N'))
$junction = $null
try {
    try { $held = $mutex.WaitOne(0) } catch [Threading.AbandonedMutexException] { $held = $true }
    if (-not $held) { throw 'Facial Cargo is busy; fixture test did not start.' }
    New-Item -ItemType Directory -Path (Join-Path $fixture 'product/scripts'), (Join-Path $fixture 'product/src'), (Join-Path $fixture '.cargo') -Force | Out-Null
    Copy-Item -LiteralPath $guard -Destination (Join-Path $fixture 'product/scripts/cargo-workspace.ps1')
    Copy-Item -LiteralPath (Join-Path $repo '.cargo/config.toml') -Destination (Join-Path $fixture '.cargo/config.toml')
    Set-Content -LiteralPath (Join-Path $fixture 'product/Cargo.toml') -Encoding ascii -Value @'
[package]
name = "cargo-guard-fixture"
version = "0.1.0"
edition = "2021"
'@
    Set-Content -LiteralPath (Join-Path $fixture 'product/Cargo.lock') -Encoding ascii -Value @'
version = 4
[[package]]
name = "cargo-guard-fixture"
version = "0.1.0"
'@
    Set-Content -LiteralPath (Join-Path $fixture 'product/src/main.rs') -Encoding ascii -Value 'fn main() { println!("fixture-ok"); }'
    Set-Content -LiteralPath (Join-Path $fixture 'product/build.rs') -Encoding ascii -Value @'
fn main() {
    let out = std::env::var_os("OUT_DIR").unwrap();
    let temp = std::env::temp_dir();
    std::fs::write(std::path::Path::new(&out).join("guard-paths.txt"),
        format!("{}\n{}", std::path::Path::new(&out).display(), temp.display())).unwrap();
    let probe = temp.join("guard-build-temp.txt");
    std::fs::write(&probe, "temporary-build-proof").unwrap();
    std::fs::remove_file(&probe).unwrap();
}
'@
    $fixtureGuard = Join-Path $fixture 'product/scripts/cargo-workspace.ps1'
    $fixtureProbe = (& $fixtureGuard -Probe | ConvertFrom-Json)
    & $fixtureGuard -CargoArgs @('build', '--locked', '--offline')
    $exe = Join-Path $fixtureProbe.target_directory 'debug/cargo-guard-fixture.exe'
    if (-not (Test-Path -LiteralPath $exe)) { throw "Missing canonical fixture executable: $exe" }
    if ((& $exe) -ne 'fixture-ok') { throw 'Fixture executable failed.' }
    $evidence = @(Get-ChildItem -LiteralPath $fixtureProbe.target_directory -Filter 'guard-paths.txt' -Recurse -File)
    if ($evidence.Count -ne 1) { throw 'Expected one build-script path evidence file.' }
    $paths = @(Get-Content -LiteralPath $evidence[0].FullName)
    if ($paths.Count -ne 2 -or -not [IO.Path]::GetFullPath($paths[0]).StartsWith($fixtureProbe.target_directory + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Build OUT_DIR escaped canonical output.' }
    if ([IO.Path]::GetFullPath($paths[1]).TrimEnd('\', '/') -ine $fixtureProbe.temporary_directory) { throw 'Build temp directory escaped canonical tmp.' }
    foreach ($outside in @('target', 'product/target')) {
        if (Test-Path -LiteralPath (Join-Path $fixture $outside)) { throw "Stray output: $outside" }
    }
    $destination = Join-Path $fixture 'junction-destination'
    New-Item -ItemType Directory -Path $destination | Out-Null
    Set-Content -LiteralPath (Join-Path $destination 'sentinel') -Value 'unchanged'
    $junction = Join-Path $fixtureProbe.target_directory 'debug/junction-test'
    New-Item -ItemType Junction -Path $junction -Target $destination | Out-Null
    foreach ($mode in @('probe', 'build', 'clean')) {
        try {
            if ($mode -eq 'probe') { & $fixtureGuard -Probe | Out-Null }
            elseif ($mode -eq 'clean') { & $fixtureGuard -Clean | Out-Null }
            else { & $fixtureGuard -CargoArgs @('build', '--locked', '--offline') }
            throw "Missing junction rejection: $mode"
        } catch { if ($_.Exception.Message -notmatch 'reparse point') { throw } }
    }
    if ((Get-Content -LiteralPath (Join-Path $destination 'sentinel')) -ne 'unchanged') { throw 'Junction destination changed.' }
    [IO.Directory]::Delete($junction)
    $junction = $null
    & $fixtureGuard -Clean
    if ((Test-Path -LiteralPath $fixtureProbe.target_directory) -or (Test-Path -LiteralPath $fixtureProbe.temporary_directory)) { throw 'Fixture cleanup failed.' }
    foreach ($name in $baselineEnvironment.Keys) {
        $observed = [Environment]::GetEnvironmentVariable($name, 'Process')
        if (-not [object]::Equals($baselineEnvironment[$name], $observed)) {
            throw "Guard changed caller environment: $name"
        }
    }
    'PASS: real isolated compilation, executable runtime, canonical OUT_DIR and build temp, nested reparse rejection (probe/build/clean), cleanup.'
} finally {
    try {
        if ($junction -and (Test-Path -LiteralPath $junction)) { [IO.Directory]::Delete($junction) }
        $expectedPrefix = [IO.Path]::GetFullPath((Join-Path $repo 'build-artifacts/tmp')) + '\'
        if (-not [IO.Path]::GetFullPath($fixture).StartsWith($expectedPrefix, [StringComparison]::OrdinalIgnoreCase)) { throw 'Unsafe fixture cleanup path.' }
        if (Test-Path -LiteralPath $fixture) { Remove-Item -LiteralPath $fixture -Force -Recurse }
    } finally {
        if ($held) { $mutex.ReleaseMutex() }
        $mutex.Dispose()
    }
}
