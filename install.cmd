@echo off
setlocal EnableExtensions
rem DEAD AIR one-click install for Windows. "install.cmd /dry-run" prints the steps and changes nothing.
rem Needs Python 3.11 or newer (dead_air.py imports tomllib). It never installs Python or FFmpeg for you.

set "DRY="
if /i "%~1"=="/dry-run" set "DRY=1"
for %%I in ("%~dp0.") do set "ROOT=%%~fI"
cd /d "%ROOT%" || exit /b 1

set "PY="
py -3 -c "import sys; sys.exit(sys.version_info < (3, 11))" >nul 2>&1 && set "PY=py -3"
if not defined PY python -c "import sys; sys.exit(sys.version_info < (3, 11))" >nul 2>&1 && set "PY=python"
if not defined PY (
    echo Python 3.11 or newer was not found. Install it, then run install.cmd again:
    echo winget install --id Python.Python.3.12 -e
    exit /b 1
)
echo DEAD AIR install, using %PY%
if defined DRY echo [dry-run] Nothing will be changed.

if defined DRY (
    echo [dry-run] Step 1: create .venv with %PY% if it is missing
    echo [dry-run] Step 2: .venv\Scripts\python.exe -m pip install --disable-pip-version-check -r requirements.txt
) else (
    if not exist ".venv\Scripts\python.exe" (
        echo Step 1: creating .venv
        %PY% -m venv .venv || exit /b 1
    )
    echo Step 2: installing requirements.txt
    ".venv\Scripts\python.exe" -m pip install --disable-pip-version-check -r requirements.txt || exit /b 1
)

set "NEEDFF="
where ffmpeg >nul 2>&1 || set "NEEDFF=1"
where ffprobe >nul 2>&1 || set "NEEDFF=1"
if defined NEEDFF (
    echo.
    echo ffmpeg and ffprobe must both be on PATH. This script does not install them.
    echo Official downloads: https://ffmpeg.org/download.html
    echo One option on Windows:
    echo winget install --id Gyan.FFmpeg.Shared -e
    echo.
)

if defined DRY (
    echo [dry-run] Step 3: create "Dead Air.lnk" in the Start menu and on the Desktop,
    echo [dry-run]         or under the INSTALL_SHORTCUT_DIR folder when that variable is set
    echo [dry-run] Done. Nothing was changed.
    exit /b 0
)
echo Step 3: creating the Dead Air shortcuts
powershell -NoProfile -Command "$ErrorActionPreference = 'Stop'; if ($env:INSTALL_SHORTCUT_DIR) { $dirs = @((Join-Path $env:INSTALL_SHORTCUT_DIR 'Start Menu'), (Join-Path $env:INSTALL_SHORTCUT_DIR 'Desktop')) } else { $dirs = @([Environment]::GetFolderPath('Programs'), [Environment]::GetFolderPath('Desktop')) }; $shell = New-Object -ComObject WScript.Shell; foreach ($d in $dirs) { New-Item -ItemType Directory -Force -Path $d | Out-Null; $path = Join-Path $d 'Dead Air.lnk'; $l = $shell.CreateShortcut($path); $l.TargetPath = Join-Path $env:ROOT '.venv\Scripts\pythonw.exe'; $l.Arguments = 'dead_air.py gui --config dead-air.toml'; $l.WorkingDirectory = $env:ROOT; $l.IconLocation = (Join-Path $env:ROOT 'brand\icon.ico') + ',0'; $l.Description = 'DEAD AIR stream watchdog'; $l.Save(); Write-Output ('  ' + $path) }" || exit /b 1
echo.
echo Done. Open Dead Air from the Start menu or the Desktop.
echo The page shows what the watcher has recorded. To run the checks, start the watcher:
echo   .venv\Scripts\python.exe dead_air.py watch --config dead-air.toml --continuous
exit /b 0
