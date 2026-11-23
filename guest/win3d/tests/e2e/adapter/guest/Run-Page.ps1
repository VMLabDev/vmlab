<#
.SYNOPSIS
Opens one of the adapter suite's pages in Chrome or Edge in the user's
desktop session and collects what the page reported.

.DESCRIPTION
The page (tests/e2e/adapter/pages) writes its result to the browser console
as `WIN3D-RESULT <base64 JSON>`. The browser runs with a new profile and with
--enable-logging --log-file, so the line lands in a file this script reads
after -Seconds, when Run-Probe.ps1 has closed the browser. A GPU process that
crashed leaves a dump in the profile's Crashpad\reports; they are counted.

Writes <OutDir>\<Name>.page.json and prints it. Run it elevated, as
tests\guest\Run-Probe.ps1 is. Exit code: 0 when the page reported, 1 when it
did not, 2 when the browser is not installed.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidateSet('chrome', 'edge')][string]$Browser,
    [Parameter(Mandatory = $true)][string]$Page,
    [Parameter(Mandatory = $true)][string]$Name,
    [string]$OutDir = 'C:\win3d\results\adapter',
    [int]$Seconds = 30,
    # Chromium features to turn on, comma separated.
    [string]$Features = '',
    [string]$ChromeDir = 'C:\apps\chrome-win64',
    [string]$RunProbe = 'C:\win3d\guest\Run-Probe.ps1'
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2

$exe = $null
if ($Browser -eq 'chrome') {
    $exe = Join-Path $ChromeDir 'chrome.exe'
} else {
    foreach ($root in @(${env:ProgramFiles(x86)}, $env:ProgramFiles)) {
        if (-not $root) { continue }
        $candidate = Join-Path $root 'Microsoft\Edge\Application\msedge.exe'
        if (Test-Path -LiteralPath $candidate) { $exe = $candidate; break }
    }
}
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
$outFile = Join-Path $OutDir "$Name.page.json"
$summary = [ordered]@{
    name = $Name; browser = $Browser; exe = $exe; version = $null; page = $Page; features = $Features
    reported = $false; result = $null; crash_dumps = 0; run_probe_exit = $null; log_lines = 0
}
if (-not $exe -or -not (Test-Path -LiteralPath $exe)) {
    $summary.exe = $exe
    ConvertTo-Json -InputObject $summary -Depth 6 | Set-Content -LiteralPath $outFile -Encoding UTF8
    Get-Content -LiteralPath $outFile -Raw
    exit 2
}
$summary.version = [Diagnostics.FileVersionInfo]::GetVersionInfo($exe).ProductVersion

$pagePath = Join-Path (Join-Path (Split-Path -Parent $PSScriptRoot) 'pages') $Page
$url = 'file:///' + ($pagePath -replace '\\', '/')
$profileDir = Join-Path $OutDir "$Name-profile"
$log = Join-Path $OutDir "$Name.browser.log"
if (Test-Path -LiteralPath $profileDir) { Remove-Item -LiteralPath $profileDir -Recurse -Force -ErrorAction SilentlyContinue }
Remove-Item -LiteralPath $log -ErrorAction SilentlyContinue

$browserArgs = @(
    "--user-data-dir=$profileDir", '--no-first-run', '--no-default-browser-check',
    '--disable-search-engine-choice-screen', '--enable-logging', "--log-file=$log", '--log-level=0', '--new-window'
)
if ($Features) { $browserArgs += "--enable-features=$Features" }
$browserArgs += $url

try {
    & $RunProbe -Path $exe -Raw -NoModules -Name $Name -OutDir $OutDir -Seconds $Seconds -TimeoutSec $Seconds `
        -ArgumentList $browserArgs | Out-Null
} catch {
    $summary['error'] = "$_"
}
$summary.run_probe_exit = $LASTEXITCODE

# Run-Probe closed the browser process; its children can outlive it for a moment.
Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -and $_.CommandLine.Contains($profileDir) } | ForEach-Object {
    try { Stop-Process -Id $_.ProcessId -Force -ErrorAction Stop } catch {}
}
Start-Sleep -Seconds 1

# Without --log-file support the log is chrome_debug.log in the profile.
if (-not (Test-Path -LiteralPath $log)) {
    $fallback = Join-Path $profileDir 'chrome_debug.log'
    if (Test-Path -LiteralPath $fallback) { Copy-Item -LiteralPath $fallback -Destination $log -Force }
}
if (Test-Path -LiteralPath $log) {
    $lines = @(Get-Content -LiteralPath $log -ErrorAction SilentlyContinue)
    $summary.log_lines = $lines.Count
    foreach ($line in $lines) {
        if ($line -match 'WIN3D-RESULT ([A-Za-z0-9+/=]+)') {
            try {
                $json = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($Matches[1]))
                $summary.result = $json | ConvertFrom-Json
                $summary.reported = $true
            } catch {}
        }
    }
}
$reports = Join-Path $profileDir 'Crashpad\reports'
if (Test-Path -LiteralPath $reports) {
    $summary.crash_dumps = @(Get-ChildItem -LiteralPath $reports -Filter *.dmp -ErrorAction SilentlyContinue).Count
}
ConvertTo-Json -InputObject $summary -Depth 6 | Set-Content -LiteralPath $outFile -Encoding UTF8
Get-Content -LiteralPath $outFile -Raw
# The profile is large and holds nothing else of use.
Remove-Item -LiteralPath $profileDir -Recurse -Force -ErrorAction SilentlyContinue
if ($summary.reported) { exit 0 }
exit 1
