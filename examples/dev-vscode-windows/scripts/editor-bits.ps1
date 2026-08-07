# dev01, step two — and the point of the whole example.
#
# This runs as **PROBE\dev**, a domain account that has never logged on to
# this machine. vmlab mints the logon (LogonUser + LoadUserProfileW, PRD
# §19.2), which is what *creates* the profile these paths live under. Run the
# same script as the agent identity and every path below resolves under
# C:\Windows\system32\config\systemprofile — the silent failure §19.8 wrote
# its one guarantee down to prevent:
#
#   A `provision {}` step can address the dev login's home directory
#   **before that user has ever logged on.**
#
# A `playbook {}` cannot do this: it has no user parameter and no rung on
# §19.2's precedence ladder, so it would half-work on a profile that already
# exists and fail on a fresh domain user — which is the first-run case.

$ErrorActionPreference = 'Stop'

# `%USERPROFILE%` is the whole proof. If this prints a systemprofile path,
# the provision ran as the machine and the rest of the example is a lie.
Write-Output "USERPROFILE=$env:USERPROFILE"
Write-Output "WHOAMI=$(whoami)"

# Where VS Code's *server* half keeps its per-user state on the remote. The
# client/server split is why this is a guest-side, per-user directory at all
# — the Neovim twin has no server and the same problem.
$server = Join-Path $env:USERPROFILE '.vscode-server'
foreach ($dir in @(
    (Join-Path $server 'extensions'),
    (Join-Path $server 'data\Machine'),
    (Join-Path $env:USERPROFILE 'vsix')
)) {
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    Write-Output "made $dir"
}

# Nothing here touches `core.autocrlf`: the workspace syncer turns it off in
# the guest itself as one of §19.6's Windows preconditions, and a provision
# fighting it would only make the two disagree.
