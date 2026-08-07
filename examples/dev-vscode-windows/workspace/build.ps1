# The "build" step of the worked example: something to run from the editor's
# integrated terminal that proves the session is a real domain logon.
#
#   cd C:\src
#   .\build.ps1
#
# C:\src is the workspace — the one thing in this lab that is not a
# declaration. It syncs both ways with ./workspace on the host (PRD §19.6),
# so editing here or there is the same edit.

$ErrorActionPreference = 'Stop'

Write-Output "running as $(whoami)"
Write-Output "on $((Get-CimInstance Win32_ComputerSystem).Domain)"

# The domain share, reached with no credential prompt because the session is
# PROBE\dev's own logon and not the agent's SYSTEM (§19.2).
Write-Output '--- \\dc01\team ---'
Get-Content \\dc01\team\README.txt

# The build itself: compile-free, because what is being demonstrated is the
# session, not a toolchain.
$out = Join-Path $PSScriptRoot 'out.txt'
"built by $(whoami) at $(Get-Date -Format o)" | Set-Content $out
Write-Output "wrote $out — it appears in ./workspace on the host within a second"
