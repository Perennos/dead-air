@echo off
setlocal EnableExtensions
rem DEAD AIR install for Windows. "install.cmd /dry-run" prints the steps and changes nothing.
rem It installs one dead-air.exe under %LOCALAPPDATA%\Perennos\dead-air\bin: a release download whose
rem SHA-256 is checked against the release's SHA256SUMS, or, when no release exists, a local
rem "cargo install". It never needs administrator rights and never installs FFmpeg for you.

set "VERSION=0.3.0"
if not defined DEAD_AIR_REPO set "DEAD_AIR_REPO=Perennos/dead-air"
if not defined DEAD_AIR_RELEASE_BASE set "DEAD_AIR_RELEASE_BASE=https://github.com/%DEAD_AIR_REPO%/releases/download/v%VERSION%"
set "ASSET=dead-air-v%VERSION%-x86_64-pc-windows-msvc.zip"
if not defined DEAD_AIR_HOME set "DEAD_AIR_HOME=%LOCALAPPDATA%\Perennos\dead-air"
set "EXE=%DEAD_AIR_HOME%\bin\dead-air.exe"
set "DRY="
if /i "%~1"=="/dry-run" set "DRY=1"
for %%I in ("%~dp0.") do set "ROOT=%%~fI"
cd /d "%ROOT%" || exit /b 1

echo DEAD AIR %VERSION% install
if defined DRY echo [dry-run] Nothing will be changed.
echo Step 1: the dead-air binary
set "HAVECARGO="
where cargo >nul 2>&1 && set "HAVECARGO=1"
if defined DRY (
    echo [dry-run] would download %DEAD_AIR_RELEASE_BASE%/%ASSET% and SHA256SUMS,
    echo [dry-run] check the SHA-256, and install dead-air.exe to %EXE%
    if defined HAVECARGO (
        echo [dry-run] if no release exists: cargo install --locked --path "%ROOT%" --root "%DEAD_AIR_HOME%"
    ) else (
        echo [dry-run] if no release exists and cargo is missing: print the rustup instruction and stop
    )
    goto :ffmpeg
)

set "INSTALLED="
powershell -NoProfile -ExecutionPolicy Bypass -Command "$ErrorActionPreference = 'Stop'; $ProgressPreference = 'SilentlyContinue'; $work = Join-Path $env:TEMP ('dead-air-' + [guid]::NewGuid()); New-Item -ItemType Directory -Path $work | Out-Null; try { try { Invoke-WebRequest -UseBasicParsing -Uri ($env:DEAD_AIR_RELEASE_BASE + '/SHA256SUMS') -OutFile (Join-Path $work 'SHA256SUMS'); Invoke-WebRequest -UseBasicParsing -Uri ($env:DEAD_AIR_RELEASE_BASE + '/' + $env:ASSET) -OutFile (Join-Path $work $env:ASSET) } catch { exit 2 }; $line = Get-Content (Join-Path $work 'SHA256SUMS') | Where-Object { ($_ -split '\s+')[1] -eq $env:ASSET -or ($_ -split '\s+')[1] -eq ('*' + $env:ASSET) } | Select-Object -First 1; if (-not $line) { exit 3 }; $expected = ($line -split '\s+')[0].ToLower(); $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $work $env:ASSET)).Hash.ToLower(); if ($expected -ne $actual) { exit 3 }; Expand-Archive -Path (Join-Path $work $env:ASSET) -DestinationPath (Join-Path $work 'unpacked'); $found = Get-ChildItem -Recurse -Filter 'dead-air.exe' (Join-Path $work 'unpacked') | Select-Object -First 1; if (-not $found) { exit 3 }; New-Item -ItemType Directory -Force -Path (Split-Path $env:EXE) | Out-Null; Copy-Item $found.FullName $env:EXE -Force; Write-Output ('  verified release ' + $env:VERSION + ' (sha256 ' + $actual + ')'); exit 0 } finally { Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue }"
set "FETCH=%ERRORLEVEL%"
if "%FETCH%"=="0" set "INSTALLED=1"
if "%FETCH%"=="3" (
    echo The downloaded %ASSET% does not match the release's SHA256SUMS. Nothing was installed.
    exit /b 1
)
if not defined INSTALLED (
    echo   no release download for this machine; building from this checkout instead
    if not defined HAVECARGO (
        echo Rust is needed to build DEAD AIR on this machine. Install it, then run install.cmd again:
        echo winget install --id Rustlang.Rustup -e
        exit /b 1
    )
    cargo install --locked --path "%ROOT%" --root "%DEAD_AIR_HOME%" || exit /b 1
)
echo   %EXE%

:ffmpeg
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

set "DA_ARGS=gui --config dead-air.toml"
if exist dead-air.toml findstr /b /l /c:"[[streams]]" dead-air.toml >nul 2>&1 && set "DA_ARGS=%DA_ARGS% --watch"
if defined DRY (
    echo [dry-run] Step 2: create "Dead Air.lnk" in the Start menu and on the Desktop,
    echo [dry-run]         running: dead-air.exe %DA_ARGS%
    echo [dry-run]         or under the INSTALL_SHORTCUT_DIR folder when that variable is set
    echo [dry-run] Done. Nothing was changed.
    exit /b 0
)
echo Step 2: creating the Dead Air shortcuts
powershell -NoProfile -Command "$ErrorActionPreference = 'Stop'; if ($env:INSTALL_SHORTCUT_DIR) { $dirs = @((Join-Path $env:INSTALL_SHORTCUT_DIR 'Start Menu'), (Join-Path $env:INSTALL_SHORTCUT_DIR 'Desktop')) } else { $dirs = @([Environment]::GetFolderPath('Programs'), [Environment]::GetFolderPath('Desktop')) }; $shell = New-Object -ComObject WScript.Shell; foreach ($d in $dirs) { New-Item -ItemType Directory -Force -Path $d | Out-Null; $path = Join-Path $d 'Dead Air.lnk'; $l = $shell.CreateShortcut($path); $l.TargetPath = $env:EXE; $l.Arguments = $env:DA_ARGS; $l.WorkingDirectory = $env:ROOT; $l.IconLocation = (Join-Path $env:ROOT 'brand\icon.ico') + ',0'; $l.WindowStyle = 7; $l.Description = 'DEAD AIR stream watchdog'; $l.Save(); Write-Output ('  ' + $path) }" || exit /b 1
echo.
echo Done. Open Dead Air from the Start menu or the Desktop.
echo Add your stream in the page, then press Start watching. The page runs the watcher for you.
echo Without the page: "%EXE%" watch --config dead-air.toml --continuous
exit /b 0
