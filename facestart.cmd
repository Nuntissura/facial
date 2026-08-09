@echo off
setlocal

set "FACIAL_TOPOLOGY=%~dp0topology.yaml"
set "FACIAL_CODEX=%~dp0CODEX.md"
set "FACIAL_README=%~dp0README.md"

if not exist "%FACIAL_TOPOLOGY%" (
    echo FACESTART_ERROR: Required injection file not found: "%FACIAL_TOPOLOGY%" 1>&2
    exit /b 1
)

if not exist "%FACIAL_CODEX%" (
    echo FACESTART_ERROR: Required injection file not found: "%FACIAL_CODEX%" 1>&2
    exit /b 1
)

if not exist "%FACIAL_README%" (
    echo FACESTART_ERROR: Required injection file not found: "%FACIAL_README%" 1>&2
    exit /b 1
)

echo FACESTART_AUTHORITY_INJECTION
echo.
echo MODEL INSTRUCTION: The following three files are the Facial repository authority:
echo 1. "%FACIAL_TOPOLOGY%"
echo 2. "%FACIAL_CODEX%"
echo 3. "%FACIAL_README%"
echo.
echo REQUIRED: Before taking any other action in this repository, read each of the three files completely.
echo REQUIRED: Explicitly acknowledge that each file was read and that its rules and instructions are understood.
echo REQUIRED: Follow all rules and instructions in all three files for all subsequent work in this repository.

endlocal
