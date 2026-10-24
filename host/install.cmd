@echo off
rem alc-cam install (no admin needed). See install.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" %*
pause
