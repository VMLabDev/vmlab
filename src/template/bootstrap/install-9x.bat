@echo off
rem vmlab guest bootstrap, legacy tier (PRD §7.4): Windows 95/98/ME. The
rem agent is vmlab-agent-legacy on COM1; --install registers it under
rem HKLM\...\RunServices (before logon, no accounts on 9x) and starts it.
rem Run from the VMLAB ISO by the template's provision, e.g. from
rem MSBATCH.INF's RunOnce or by keystrokes: D:\INSTALL-9X.BAT
rem The source drive is probed: COMMAND.COM has no %~dp0.
set VMLSRC=
if exist A:\legacy\9x\vmlab-agent-legacy.exe set VMLSRC=A:
if exist D:\legacy\9x\vmlab-agent-legacy.exe set VMLSRC=D:
if exist E:\legacy\9x\vmlab-agent-legacy.exe set VMLSRC=E:
if exist F:\legacy\9x\vmlab-agent-legacy.exe set VMLSRC=F:
if "%VMLSRC%"=="" goto nosrc
if not exist C:\VMLAB\NUL mkdir C:\VMLAB
copy %VMLSRC%\legacy\9x\vmlab-agent-legacy.exe C:\VMLAB\VMLABAGT.EXE
if errorlevel 1 goto fail
set VMLSRC=
C:\VMLAB\VMLABAGT.EXE --install
goto end
:nosrc
echo vmlab bootstrap: no legacy\9x\vmlab-agent-legacy.exe on drives A D E F
goto end
:fail
echo vmlab bootstrap: copy failed
:end
