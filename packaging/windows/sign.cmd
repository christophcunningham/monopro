@echo off
setlocal

if "%MONOPRO_CERT_SHA1%"=="" (
  echo MONOPRO_CERT_SHA1 is required for a signed release. 1>&2
  exit /b 2
)

if "%MONOPRO_TIMESTAMP_URL%"=="" (
  echo MONOPRO_TIMESTAMP_URL is required for a signed release. 1>&2
  exit /b 2
)

where signtool.exe >nul 2>nul
if errorlevel 1 (
  echo signtool.exe was not found on PATH. 1>&2
  exit /b 2
)

signtool.exe sign /sha1 "%MONOPRO_CERT_SHA1%" /fd SHA256 /tr "%MONOPRO_TIMESTAMP_URL%" /td SHA256 /d "monopro" "%~1"
exit /b %errorlevel%
