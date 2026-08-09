@echo off
setlocal

set "FAST_FOCUS_CODEX=D:\Projects\LLM projects\Fast Focus\FF - gov\FAST_FOCUS_CODEX.md"
set "FAST_FOCUS_START=D:\Projects\LLM projects\Fast Focus\FF - gov\START_HERE.md"

if not exist "%FAST_FOCUS_CODEX%" (
    echo FACESTART_ERROR: Required authority file not found: "%FAST_FOCUS_CODEX%" 1>&2
    exit /b 1
)

if not exist "%FAST_FOCUS_START%" (
    echo FACESTART_ERROR: Required authority file not found: "%FAST_FOCUS_START%" 1>&2
    exit /b 1
)

echo FACESTART_AUTHORITY_INJECTION
echo.
echo MODEL INSTRUCTION: Before taking any other action in this repository, read these files completely:
echo 1. "%FAST_FOCUS_CODEX%"
echo 2. "%FAST_FOCUS_START%"
echo.
echo After reading both files, explicitly acknowledge the repository rules and follow all rules and instructions they contain for all subsequent work.

endlocal
