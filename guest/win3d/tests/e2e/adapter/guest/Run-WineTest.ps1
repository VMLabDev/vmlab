<#
.SYNOPSIS
Runs one of Wine's Direct3D test programs in the user's desktop session and
writes what it counted.

.DESCRIPTION
The programs are built from Wine's source by tests/e2e/adapter/build.sh, one
per test file (d3d9_device.exe, d3d11_d3d11.exe, ...). A program prints a
line per failed check and ends with
  <pid>:<test>: N tests executed (N marked as todo, N as flaky, N failures), N skipped.
Its exit code is the number of failures, 255 at most.

The program runs through cmd.exe, which redirects its output to
<OutDir>\<Name>.txt, under tests\guest\Run-Probe.ps1. After -TimeoutSec it is
stopped and counts as timed out. Writes <OutDir>\<Name>.wine.json and prints
it. `completed` says that a summary line was printed; `crashed` and a
negative `exit_code` say that the program then ended on an exception. Exit
code 0 when the program printed a summary line, 1 otherwise.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Path,
    [string]$OutDir = 'C:\win3d\results\adapter',
    [int]$TimeoutSec = 900,
    [string]$RunProbe = 'C:\win3d\guest\Run-Probe.ps1'
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2

$Path = (Resolve-Path -LiteralPath $Path).ProviderPath
$base = [IO.Path]::GetFileNameWithoutExtension($Path)
$Name = "wine-$base"
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
$txt = Join-Path $OutDir "$Name.txt"
$cmd = Join-Path $OutDir "$Name.cmd"
Remove-Item -LiteralPath $txt -ErrorAction SilentlyContinue
Set-Content -LiteralPath $cmd -Encoding ASCII -Value @(
    '@echo off'
    "`"$Path`" > `"$txt`" 2>&1"
    'exit /b %errorlevel%'
)

$started = Get-Date
$runError = $null
try {
    & $RunProbe -Path (Join-Path $env:windir 'System32\cmd.exe') -Raw -NoModules -Name $Name -OutDir $OutDir `
        -Seconds $TimeoutSec -TimeoutSec $TimeoutSec -ArgumentList '/d', '/c', $cmd | Out-Null
} catch {
    $runError = "$_"
}
$exit = $LASTEXITCODE
$timedOut = ($exit -eq 124)
# On a timeout Run-Probe stopped cmd.exe; the test program is its child.
Get-Process -Name $base -ErrorAction SilentlyContinue | ForEach-Object {
    try { $_.Kill() } catch {}
}
$duration = [math]::Round(((Get-Date) - $started).TotalSeconds, 1)

$summary = [ordered]@{
    name = $base; exe = $Path; exit_code = $exit; timed_out = $timedOut; duration_s = $duration
    completed = $false; summaries = 0; executed = $null; todo = $null; flaky = $null; failures = $null; skipped = $null
    failed_lines = 0; crashed = $false; first_failures = @(); output_bytes = 0; error = $runError
}
if (Test-Path -LiteralPath $txt) {
    $summary.output_bytes = (Get-Item -LiteralPath $txt).Length
    $failed = New-Object System.Collections.ArrayList
    foreach ($line in [IO.File]::ReadLines($txt)) {
        if ($line -match ': Test (failed|succeeded inside todo block): ') {
            $summary.failed_lines++
            if ($failed.Count -lt 15) {
                [void]$failed.Add($(if ($line.Length -gt 240) { $line.Substring(0, 240) } else { $line }))
            }
        } elseif ($line -match 'unhandled exception') {
            $summary.crashed = $true
            if ($failed.Count -lt 16) { [void]$failed.Add($line) }
        } elseif ($line -match ': (\d+) tests executed \((\d+) marked as todo, (\d+) as flaky, (\d+) failures?\), (\d+) skipped') {
            # A program can print the summary of a child before it goes on; the last one counts.
            $summary.completed = $true
            $summary.summaries++
            $summary.executed = [int]$Matches[1]; $summary.todo = [int]$Matches[2]; $summary.flaky = [int]$Matches[3]
            $summary.failures = [int]$Matches[4]; $summary.skipped = [int]$Matches[5]
        }
    }
    $summary.first_failures = @($failed)
    # Keep only the lines that say what failed: the full output of a visual
    # test is tens of megabytes.
    $keep = Join-Path $OutDir "$Name.failures.txt"
    Select-String -LiteralPath $txt -Pattern ': Test failed: |: Test succeeded inside todo block: |unhandled exception|tests executed|: Tests skipped: ' |
        Select-Object -First 2000 | ForEach-Object Line | Set-Content -LiteralPath $keep -Encoding UTF8
}
$outFile = Join-Path $OutDir "$Name.wine.json"
ConvertTo-Json -InputObject $summary -Depth 4 | Set-Content -LiteralPath $outFile -Encoding UTF8
Get-Content -LiteralPath $outFile -Raw
if ($summary.completed) { exit 0 }
exit 1
