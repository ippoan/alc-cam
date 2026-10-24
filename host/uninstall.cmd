@echo off
rem alc-cam uninstall. See install.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" -Uninstall
pause
