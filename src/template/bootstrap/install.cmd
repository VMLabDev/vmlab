@echo off
rem vmlab guest bootstrap: install the vmlab-agent service from the VMLAB
rem ISO. Run by autounattend FirstLogonCommands during a template build
rem (elevated Administrator). The install path is deliberately space-free:
rem `binPath=` rides as one unquoted token, the form verified live.
set VMLAB_DIR=C:\ProgramData\vmlab
if not exist "%VMLAB_DIR%" mkdir "%VMLAB_DIR%"
copy /y "%~dp0windows\x86_64\vmlab-agent.exe" "%VMLAB_DIR%\vmlab-agent.exe"
if errorlevel 1 exit /b 1
rem `sc create` fails when the service exists (rebuilds); reconfigure then.
sc create vmlab-agent binPath= C:\ProgramData\vmlab\vmlab-agent.exe start= auto
if errorlevel 1 sc config vmlab-agent binPath= C:\ProgramData\vmlab\vmlab-agent.exe start= auto
sc failure vmlab-agent reset= 86400 actions= restart/5000/restart/5000/restart/5000
sc start vmlab-agent
exit /b 0
