@echo off
rem Double-click to build Ferrocap. Installs Rust, MinGW and FFmpeg first if they're missing.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1" %*
if errorlevel 1 (echo. & echo Build failed. & pause & exit /b 1)
echo.
echo Done: %~dp0dist\Ferrocap.exe
pause
