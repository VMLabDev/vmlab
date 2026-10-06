@ECHO OFF
REM vmlab guest bootstrap, legacy tier (PRD 7.4): DOS. The agent is
REM VMLABAGT.EXE on COM1, started from the boot batch file; it is the
REM foreground program, and the guest is the agent while it runs. Run from
REM the VMLAB medium by the template's provision (keystrokes): A:\INSTALL.BAT
REM or D:\INSTALL.BAT. Every name here is 8.3: DOS reads no Joliet. The source
REM drive is probed rather than derived from %0, which FreeCOM cannot do.
REM FreeDOS 1.2+ boots C:\FDAUTO.BAT (FDCONFIG.SYS SHELL /P=) and never reads
REM AUTOEXEC.BAT, so the agent goes in FDAUTO.BAT where it exists.
SET VMLSRC=
IF EXIST A:\LEGACY\DOS\VMLABAGT.EXE SET VMLSRC=A:
IF EXIST D:\LEGACY\DOS\VMLABAGT.EXE SET VMLSRC=D:
IF EXIST E:\LEGACY\DOS\VMLABAGT.EXE SET VMLSRC=E:
IF EXIST F:\LEGACY\DOS\VMLABAGT.EXE SET VMLSRC=F:
IF "%VMLSRC%"=="" GOTO NOSRC
IF NOT EXIST C:\VMLAB\NUL MKDIR C:\VMLAB
COPY %VMLSRC%\LEGACY\DOS\VMLABAGT.EXE C:\VMLAB\VMLABAGT.EXE
IF ERRORLEVEL 1 GOTO FAIL
SET VMLAUTO=C:\AUTOEXEC.BAT
IF EXIST C:\FDAUTO.BAT SET VMLAUTO=C:\FDAUTO.BAT
FIND "VMLABAGT" %VMLAUTO% >NUL
IF NOT ERRORLEVEL 1 GOTO START
REM A file whose last line has no line break would swallow the appended
REM line into it (FreeDOS's FDAUTO.BAT ends on a bare :END label).
ECHO.>>%VMLAUTO%
ECHO C:\VMLAB\VMLABAGT.EXE>>%VMLAUTO%
:START
SET VMLSRC=
SET VMLAUTO=
C:\VMLAB\VMLABAGT.EXE
GOTO END
:NOSRC
ECHO vmlab bootstrap: no LEGACY\DOS\VMLABAGT.EXE on drives A D E F
GOTO END
:FAIL
ECHO vmlab bootstrap: copy failed
:END
