@echo off
rem Perennos DEMON MODE: one-click install for Windows. Never needs admin rights.
rem   install.cmd            install demon.exe into %LOCALAPPDATA%\Perennos\demon-mode and add the shortcuts
rem   install.cmd /dry-run   print the steps and change nothing (no downloads either)
rem 1. The prebuilt release binary, checked against the release's SHA256SUMS.
rem 2. Otherwise, if cargo is installed, a local build from this checkout.
rem 3. Otherwise, the one-line Rust install command, and a stop.
rem Env INSTALL_BIN_DIR: install demon.exe there instead.
rem Env INSTALL_SHORTCUT_DIR: write both shortcuts into that folder (used by the tests).
rem Env INSTALL_NO_PATH: do not add the install folder to your user PATH.
rem Env INSTALL_NO_PAUSE: do not wait for a key at the end.
rem Env DEMON_INSTALL_FROM_SOURCE=1: skip the download and build with cargo.
setlocal EnableExtensions
cd /d "%~dp0"
set "VERSION=0.2.0"
set "DRY="
if /i "%~1"=="/dry-run" set "DRY=1"
if not defined INSTALL_BIN_DIR set "INSTALL_BIN_DIR=%LOCALAPPDATA%\Perennos\demon-mode"
set "DM_EXE=%INSTALL_BIN_DIR%\demon.exe"
set "DM_NAME=demon-v%VERSION%-x86_64-pc-windows-msvc"
set "DM_BASE=https://github.com/Perennos/demon-mode/releases/download/v%VERSION%"
set "DM_ICON=%~dp0brand\icon.ico"

if defined DRY goto :dry

if "%DEMON_INSTALL_FROM_SOURCE%"=="1" goto :source
echo Looking for the prebuilt v%VERSION% release binary
powershell -NoProfile -ExecutionPolicy Bypass -Command "$ErrorActionPreference='Stop'; $ProgressPreference='SilentlyContinue'; [Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12; $t=Join-Path $env:TEMP ('demon-install-'+[guid]::NewGuid()); New-Item -ItemType Directory $t | Out-Null; try { try { Invoke-WebRequest -UseBasicParsing -Uri ($env:DM_BASE+'/SHA256SUMS') -OutFile (Join-Path $t 'SHA256SUMS'); Invoke-WebRequest -UseBasicParsing -Uri ($env:DM_BASE+'/'+$env:DM_NAME+'.zip') -OutFile (Join-Path $t 'demon.zip') } catch { exit 3 }; $want=''; foreach ($l in Get-Content (Join-Path $t 'SHA256SUMS')) { $p=$l -split '\s+'; if ($p.Count -ge 2 -and ($p[1] -eq ($env:DM_NAME+'.zip') -or $p[1] -eq ('*'+$env:DM_NAME+'.zip'))) { $want=$p[0].ToLower() } }; $got=(Get-FileHash -Algorithm SHA256 (Join-Path $t 'demon.zip')).Hash.ToLower(); if (-not $want -or $want -ne $got) { Write-Host 'The download does not match SHA256SUMS. Nothing was installed.'; exit 1 }; Expand-Archive -Path (Join-Path $t 'demon.zip') -DestinationPath $t -Force; New-Item -ItemType Directory -Force $env:INSTALL_BIN_DIR | Out-Null; Copy-Item -Force (Join-Path $t ($env:DM_NAME+'\demon.exe')) $env:DM_EXE; exit 0 } finally { Remove-Item -Recurse -Force $t -ErrorAction SilentlyContinue }"
if errorlevel 3 goto :nodownload
if errorlevel 1 goto :fail
echo Installed the verified release binary: %DM_EXE%
goto :installed

:nodownload
echo No prebuilt v%VERSION% binary was found; building from source instead.
:source
where cargo >nul 2>&1
if errorlevel 1 goto :norust
echo Building Demon Mode from source with cargo (a few minutes the first time)
cargo build --release --locked --manifest-path "%~dp0Cargo.toml"
if errorlevel 1 goto :fail
if not exist "%INSTALL_BIN_DIR%" mkdir "%INSTALL_BIN_DIR%"
copy /y "%~dp0target\release\demon.exe" "%DM_EXE%" >nul
if errorlevel 1 goto :fail
echo Installed: %DM_EXE%

:installed
if defined INSTALL_NO_PATH goto :shortcuts
echo Adding %INSTALL_BIN_DIR% to your user PATH (new terminals will find demon)
powershell -NoProfile -ExecutionPolicy Bypass -Command "$p=[Environment]::GetEnvironmentVariable('Path','User'); if (-not $p) { $p='' }; if (($p -split ';') -notcontains $env:INSTALL_BIN_DIR) { [Environment]::SetEnvironmentVariable('Path', (($p.TrimEnd(';')+';'+$env:INSTALL_BIN_DIR).TrimStart(';')), 'User') }"

:shortcuts
echo Creating the shortcuts
powershell -NoProfile -ExecutionPolicy Bypass -Command "$w=New-Object -ComObject WScript.Shell; if($env:INSTALL_SHORTCUT_DIR){$t=@(@($env:INSTALL_SHORTCUT_DIR,'Demon Mode.lnk'),@($env:INSTALL_SHORTCUT_DIR,'Demon Mode (Start menu).lnk'))}else{$t=@(@([Environment]::GetFolderPath('Desktop'),'Demon Mode.lnk'),@([Environment]::GetFolderPath('Programs'),'Demon Mode.lnk'))}; foreach($x in $t){New-Item -ItemType Directory -Force -Path $x[0] | Out-Null; $l=$w.CreateShortcut((Join-Path $x[0] $x[1])); $l.TargetPath=$env:DM_EXE; $l.Arguments='gui'; $l.WorkingDirectory=$env:INSTALL_BIN_DIR; $l.IconLocation=$env:DM_ICON; $l.WindowStyle=7; $l.Save(); Write-Host ('Shortcut: ' + (Join-Path $x[0] $x[1]))}"
if errorlevel 1 goto :fail

echo.
echo Upgrading from the Python release? Your ledger stays where it was. Run:
echo   "%DM_EXE%" hook-config --client claude   (or --client codex)
echo and replace only the old "python -m demon_mode" hook command; keep every other hook.
echo The old .venv folder in this checkout is no longer used and can be deleted.
echo.
echo Installed. Next step: click the Demon Mode shortcut on your Desktop (or find it in the Start menu).
goto :done

:dry
echo Dry run: nothing will be changed.
echo   1. Download %DM_BASE%/%DM_NAME%.zip and SHA256SUMS, verify the SHA-256,
echo      and copy demon.exe to %DM_EXE%
echo      (no release found: build with cargo from this checkout instead).
if defined INSTALL_NO_PATH echo   2. Leave your user PATH unchanged.
if not defined INSTALL_NO_PATH echo   2. Add %INSTALL_BIN_DIR% to your user PATH.
if defined INSTALL_SHORTCUT_DIR echo   3. Create the shortcuts "Demon Mode.lnk" and "Demon Mode (Start menu).lnk" in %INSTALL_SHORTCUT_DIR%
if not defined INSTALL_SHORTCUT_DIR echo   3. Create the shortcut "Demon Mode.lnk" on your Desktop and in your Start menu.
echo      Target: %DM_EXE%, arguments: gui
echo Next step after a real run: click the Demon Mode shortcut.
goto :done

:norust
echo No prebuilt binary was found and Rust is not installed. Install Rust (no admin needed), then run install.cmd again:
echo winget install --id Rustlang.Rustup -e
goto :fail

:fail
echo.
echo Install failed. See the message above.
if not defined INSTALL_NO_PAUSE pause
exit /b 1

:done
if not defined DRY if not defined INSTALL_NO_PAUSE echo %CMDCMDLINE% | find /i "install.cmd" >nul && pause
exit /b 0
