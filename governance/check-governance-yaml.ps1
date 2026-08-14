# Parse every machine-readable governance artifact and fail on the first one that
# is not machine-readable.
#
# Why this exists: 15 work packets had been unparseable for months — unquoted
# scalars containing ": ", stray backticks, invalid "\P" escapes in double-quoted
# Windows paths, and prose continuation lines the parser read as new keys. Nothing
# noticed, because nothing ever tried to parse them. A governance system that
# claims to be machine-readable has to prove it.
#
# Usage:
#   pwsh -File governance/check-governance-yaml.ps1
# Exit code 0 = every artifact parses. Non-zero = at least one does not.

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot

$targets = @()
$targets += Get-ChildItem -Path (Join-Path $repoRoot 'governance') -Filter '*.yaml' -Recurse -File
$topology = Join-Path $repoRoot 'topology.yaml'
if (Test-Path $topology) { $targets += Get-Item $topology }

if ($targets.Count -eq 0) {
    Write-Error 'no governance YAML found; check the repo root discovery above'
    exit 2
}

# powershell-yaml is not assumed to be installed, and the repo must stay
# disk-agnostic, so parse with whichever runtime is present. Python's yaml is the
# same parser the agents use to read these files.
$python = Get-Command python -ErrorAction SilentlyContinue
if (-not $python) {
    $python = Get-Command python3 -ErrorAction SilentlyContinue
}
if (-not $python) {
    Write-Error 'no python on PATH; cannot verify YAML is machine-readable'
    exit 2
}

$paths = $targets.FullName -join "`n"
$script = @'
import sys, yaml
paths = [line for line in sys.stdin.read().splitlines() if line.strip()]
bad = 0
for path in paths:
    try:
        with open(path, encoding="utf-8") as handle:
            yaml.safe_load(handle)
    except Exception as error:
        bad += 1
        print("UNPARSEABLE %s\n    %s" % (path, str(error).replace("\n", "\n    ")))
print("checked %d file(s); unparseable: %d" % (len(paths), bad))
# A run that found nothing to check has not proven anything.
sys.exit(1 if (bad or not paths) else 0)
'@

# Feed the list on stdin so no path length or quoting limit applies.
# Encode the embedded program so Windows PowerShell 5.1 cannot strip quotes
# while constructing the native `python -c` command line. PowerShell 7 passes
# the raw script correctly, but the documented validator command uses
# `powershell`, so both hosts must produce the same parse result.
$encodedScript = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($script))
$pythonCommand = "import base64;exec(base64.b64decode('$encodedScript'))"
$result = $paths | & $python.Source -c $pythonCommand
$code = $LASTEXITCODE
$result | ForEach-Object { Write-Output $_ }
if ($code -ne 0) {
    Write-Output "FAIL: governance artifacts must be machine-readable (build_rules FACIAL-GOV-001)."
}
exit $code
