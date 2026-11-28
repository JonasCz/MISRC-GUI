@echo off
rem Build mwadc (release) and stage it in dist\. Arguments are passed to build.ps1, e.g.
rem   scripts\build.cmd -SdkDll "C:\path\to\LibMWCapture.dll"
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1" %*
