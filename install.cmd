@echo off
rem Perennos DEMON MODE: one-click install for Windows.
rem   install.cmd            create .venv, install the package, add the shortcuts
rem   install.cmd /dry-run   print the steps and change nothing
rem Env INSTALL_SHORTCUT_DIR: write both shortcuts into that folder (used by the tests).
rem Env INSTALL_NO_PAUSE: do not wait for a key at the end.
setlocal EnableExtensions
cd /d "%~dp0"
set "DRY="
if /i "%~1"=="/dry-run" set "DRY=1"

set "PY="
py -3 -c "import sys; sys.exit(0 if sys.version_info >= (3, 10) else 1)" >nul 2>&1 && set "PY=py -3"
if not defined PY python -c "import sys; sys.exit(0 if sys.version_info >= (3, 10) else 1)" >nul 2>&1 && set "PY=python"
if not defined PY goto :nopython

set "VENV=%~dp0.venv"
set "VPY=%VENV%\Scripts\python.exe"

if defined DRY goto :dry

echo Using Python: %PY%
if exist "%VPY%" goto :havevenv
echo Creating the virtual environment in %VENV%
%PY% -m venv "%VENV%"
if errorlevel 1 goto :fail
:havevenv
echo Installing Demon Mode into .venv
"%VPY%" -m pip install --disable-pip-version-check .
if errorlevel 1 goto :fail

echo Creating the shortcuts
set "DM_TARGET=%VENV%\Scripts\pythonw.exe"
set "DM_WORK=%~dp0"
set "DM_ICON=%~dp0brand\icon.ico"
powershell -NoProfile -ExecutionPolicy Bypass -Command "$w=New-Object -ComObject WScript.Shell; if($env:INSTALL_SHORTCUT_DIR){$t=@(@($env:INSTALL_SHORTCUT_DIR,'Demon Mode.lnk'),@($env:INSTALL_SHORTCUT_DIR,'Demon Mode (Start menu).lnk'))}else{$t=@(@([Environment]::GetFolderPath('Desktop'),'Demon Mode.lnk'),@([Environment]::GetFolderPath('Programs'),'Demon Mode.lnk'))}; foreach($x in $t){New-Item -ItemType Directory -Force -Path $x[0] | Out-Null; $l=$w.CreateShortcut((Join-Path $x[0] $x[1])); $l.TargetPath=$env:DM_TARGET; $l.Arguments='-m demon_mode gui'; $l.WorkingDirectory=$env:DM_WORK; $l.IconLocation=$env:DM_ICON; $l.Save(); Write-Host ('Shortcut: ' + (Join-Path $x[0] $x[1]))}"
if errorlevel 1 goto :fail

echo.
echo Installed. Next step: click the Demon Mode shortcut on your Desktop (or find it in the Start menu).
goto :done

:dry
echo Dry run: nothing will be changed.
echo   Python found: %PY%
echo   1. Create the virtual environment %VENV% if it is missing.
echo   2. Install the package: "%VPY%" -m pip install --disable-pip-version-check .
if defined INSTALL_SHORTCUT_DIR echo   3. Create the shortcuts "Demon Mode.lnk" and "Demon Mode (Start menu).lnk" in %INSTALL_SHORTCUT_DIR%
if not defined INSTALL_SHORTCUT_DIR echo   3. Create the shortcut "Demon Mode.lnk" on your Desktop and in your Start menu.
echo      Target: pythonw.exe in .venv, arguments: -m demon_mode gui
echo Next step after a real run: click the Demon Mode shortcut.
goto :done

:nopython
echo Python 3.10 or newer was not found. Install it, then run install.cmd again:
echo winget install --id Python.Python.3.12 -e
exit /b 1

:fail
echo.
echo Install failed. See the message above.
if not defined INSTALL_NO_PAUSE pause
exit /b 1

:done
if not defined DRY if not defined INSTALL_NO_PAUSE echo %CMDCMDLINE% | find /i "install.cmd" >nul && pause
exit /b 0
