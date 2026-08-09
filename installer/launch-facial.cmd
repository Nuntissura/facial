@echo off
REM Facial launcher (WP-025).
REM An installed build keeps READ-ONLY assets in the install dir and writes settings +
REM projects to a per-user, writable location. These are set per-process only, so a repo
REM dev build (cargo run) is never affected by them.
setlocal
set "APPDIR=%~dp0"
if "%APPDIR:~-1%"=="\" set "APPDIR=%APPDIR:~0,-1%"
set "FACIAL_REPO_ROOT=%APPDIR%"
set "FACIAL_CONFIG_PATH=%LOCALAPPDATA%\Facial\config\default.json"
set "FACIAL_WORKSPACE_ROOT=%LOCALAPPDATA%\Facial"
start "" "%APPDIR%\facial.exe"
endlocal
