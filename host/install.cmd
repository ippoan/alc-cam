@echo off
rem alc-cam install (admin prompt appears once). See install.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" %*
pause
