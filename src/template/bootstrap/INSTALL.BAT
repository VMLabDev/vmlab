@ECHO OFF
REM vmlab guest bootstrap, legacy tier (PRD 7.4): DOS. The agent is
REM VMLABAGT.EXE on COM1, started from AUTOEXEC.BAT; it is the foreground
REM program, and the guest is the agent while it runs. Run from the VMLAB
REM medium by the template's provision (keystrokes): A:\INSTALL.BAT or
REM D:\INSTALL.BAT. Every name here is 8.3: DOS reads no Joliet. The source
REM drive is probed rather than derived from %0, which FreeCOM cannot do.
SET VMLSRC=
IF EXIST A:\LEGACY\DOS\VMLABAGT.EXE SET VMLSRC=A:
IF EXIST D:\LEGACY\DOS\VMLABAGT.EXE SET VMLSRC=D:
IF EXIST E:\LEGACY\DOS\VMLABAGT.EXE SET VMLSRC=E:
IF EXIST F:\LEGACY\DOS\VMLABAGT.EXE SET VMLSRC=F:
IF "%VMLSRC%"=="" GOTO NOSRC
IF NOT EXIST C:\VMLAB\NUL MKDIR C:\VMLAB
COPY %VMLSRC%\LEGACY\DOS\VMLABAGT.EXE C:\VMLAB\VMLABAGT.EXE
IF ERRORLEVEL 1 GOTO FAIL
FIND "VMLABAGT" C:\AUTOEXEC.BAT >NUL
IF NOT ERRORLEVEL 1 GOTO START
ECHO C:\VMLAB\VMLABAGT.EXE>>C:\AUTOEXEC.BAT
:START
SET VMLSRC=
C:\VMLAB\VMLABAGT.EXE
GOTO END
:NOSRC
ECHO vmlab bootstrap: no LEGACY\DOS\VMLABAGT.EXE on drives A B D E F
GOTO END
:FAIL
ECHO vmlab bootstrap: copy failed
:END
