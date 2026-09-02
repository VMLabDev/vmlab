@echo off
rem vmlab guest bootstrap, legacy tier (PRD §7.4): NT4 through XP/2003 have
rem no virtio-serial, so the agent is vmlab-agent-legacy on COM1. Run by the
rem template's unattended-install hook ([GuiRunOnce] in winnt.sif, or
rem cmdlines.txt) as an administrator, from the VMLAB ISO. The binary
rem registers itself with the SCM (--install), so no sc.exe is needed.
rem The install path is deliberately space-free and 8.3-safe.
set VMLAB_DIR=C:\vmlab
if not exist %VMLAB_DIR% mkdir %VMLAB_DIR%
copy /y "%~dp0legacy\nt\vmlab-agent-legacy.exe" %VMLAB_DIR%\vmlab-agent-legacy.exe
if errorlevel 1 exit /b 1
%VMLAB_DIR%\vmlab-agent-legacy.exe --install
exit /b %errorlevel%
