<#
.SYNOPSIS
Opens a page in Chrome or Edge in the user's desktop session and reports
whether the browser's GPU process survived it, and which d3d12.dll it loaded.

.DESCRIPTION
For `tests/e2e/chromium.sh` (docs/chromium.md, GitHub #238). The page
(`tests/e2e/chromium/venc.html`) writes its result to the browser console as
`WIN3D-RESULT <base64 JSON>`. The browser runs with a new profile and with
--enable-logging --log-file, so the line lands in a file this script reads
after -Seconds, when Run-Probe.ps1 has closed the browser.

While the browser runs, a background job looks every half second for the
profile's GPU processes (`--type=gpu-process`) and records each process ID,
every d3d12.dll it has loaded and its MicrosoftSignedOnly mitigation. A GPU
process that ended itself shows as a second process ID and as a dump in the
profile's Crashpad\reports.

Writes <OutDir>\<Name>.browser.json and prints it. Run it elevated, as
Run-Probe.ps1 is. Exit code: 0 when the page reported, 1 when it did not, 2
when the browser is not installed.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidateSet('chrome', 'edge')][string]$Browser,
    [Parameter(Mandatory = $true)][string]$Page,
    [Parameter(Mandatory = $true)][string]$Name,
    [string]$OutDir = 'C:\win3d\results\chromium',
    [int]$Seconds = 45,
    # Chromium features to turn on, comma separated.
    [string]$Features = '',
    [string]$ChromeDir = 'C:\apps\chrome-win64',
    [string]$RunProbe = (Join-Path $PSScriptRoot 'Run-Probe.ps1')
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
$outFile = Join-Path $OutDir "$Name.browser.json"
$summary = [ordered]@{
    name = $Name; browser = $Browser; exe = $exe; version = $null; page = $Page; features = $Features
    reported = $false; result = $null; crash_dumps = 0; gpu_processes = @(); run_probe_exit = $null; log_lines = 0
}
function Write-Summary {
    ConvertTo-Json -InputObject $summary -Depth 6 | Set-Content -LiteralPath $outFile -Encoding UTF8
    Get-Content -LiteralPath $outFile -Raw
}
if (-not $exe -or -not (Test-Path -LiteralPath $exe)) { Write-Summary; exit 2 }
$summary.version = [Diagnostics.FileVersionInfo]::GetVersionInfo($exe).ProductVersion

$url = 'file:///' + ((Resolve-Path -LiteralPath $Page).ProviderPath -replace '\\', '/')
$profileDir = Join-Path $OutDir "$Name-profile"
$log = Join-Path $OutDir "$Name.browser.log"
$watchFile = Join-Path $OutDir "$Name.gpu.json"
$stopFile = Join-Path $OutDir "$Name.stop"
if (Test-Path -LiteralPath $profileDir) { Remove-Item -LiteralPath $profileDir -Recurse -Force -ErrorAction SilentlyContinue }
foreach ($f in $log, $watchFile, $stopFile) { Remove-Item -LiteralPath $f -ErrorAction SilentlyContinue }

$browserArgs = @(
    "--user-data-dir=$profileDir", '--no-first-run', '--no-default-browser-check',
    '--disable-search-engine-choice-screen', '--enable-logging', "--log-file=$log", '--log-level=0', '--new-window'
)
if ($Features) { $browserArgs += "--enable-features=$Features" }
$browserArgs += $url

$watch = Start-Job -ArgumentList $profileDir, $watchFile, $stopFile, ([IO.Path]::GetFileName($exe)) -ScriptBlock {
    param($ProfileDir, $OutFile, $StopFile, $ExeName)
    $seen = [ordered]@{}
    $started = Get-Date
    while (-not (Test-Path -LiteralPath $StopFile) -and ((Get-Date) - $started).TotalSeconds -lt 900) {
        $all = @(Get-CimInstance Win32_Process -Filter "Name='$ExeName'" -ErrorAction SilentlyContinue | Where-Object { $_.CommandLine })
        $browsers = @($all | Where-Object { $_.CommandLine.Contains($ProfileDir) -and -not $_.CommandLine.Contains('--type=') } | ForEach-Object { $_.ProcessId })
        foreach ($p in $all) {
            if (-not $p.CommandLine.Contains('--type=gpu-process')) { continue }
            if (-not ($p.CommandLine.Contains($ProfileDir) -or $browsers -contains $p.ParentProcessId)) { continue }
            $id = [string]$p.ProcessId
            if (-not $seen.Contains($id)) {
                $seen[$id] = [ordered]@{
                    pid = [int]$p.ProcessId; first_seen_s = [math]::Round(((Get-Date) - $started).TotalSeconds, 1)
                    last_seen_s = $null; d3d12 = @(); microsoft_signed_only = $null; error = $null
                }
            }
            $e = $seen[$id]
            $e.last_seen_s = [math]::Round(((Get-Date) - $started).TotalSeconds, 1)
            try {
                foreach ($m in @((Get-Process -Id $p.ProcessId -ErrorAction Stop).Modules)) {
                    if ($m.ModuleName -ieq 'd3d12.dll' -and $e.d3d12 -notcontains $m.FileName) { $e.d3d12 += $m.FileName }
                }
            } catch { $e.error = "$_" }
            if ($null -eq $e.microsoft_signed_only) {
                try { $e.microsoft_signed_only = "$((Get-ProcessMitigation -Id $p.ProcessId -ErrorAction Stop).BinarySignature.MicrosoftSignedOnly)" } catch {}
            }
        }
        Start-Sleep -Milliseconds 500
    }
    ConvertTo-Json -InputObject @($seen.Values) -Depth 4 | Set-Content -LiteralPath $OutFile -Encoding UTF8
}

try {
    & $RunProbe -Path $exe -Raw -NoModules -Name $Name -OutDir $OutDir -Seconds $Seconds -TimeoutSec $Seconds `
        -ArgumentList $browserArgs | Out-Null
} catch {
    $summary['error'] = "$_"
}
$summary.run_probe_exit = $LASTEXITCODE
Set-Content -LiteralPath $stopFile -Value 'stop'
Wait-Job -Job $watch -Timeout 60 | Out-Null
Remove-Job -Job $watch -Force -ErrorAction SilentlyContinue
if (Test-Path -LiteralPath $watchFile) {
    $summary.gpu_processes = @(Get-Content -LiteralPath $watchFile -Raw | ConvertFrom-Json)
}
foreach ($f in $watchFile, $stopFile) { Remove-Item -LiteralPath $f -ErrorAction SilentlyContinue }

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
Write-Summary
# The profile is large and holds nothing else of use.
Remove-Item -LiteralPath $profileDir -Recurse -Force -ErrorAction SilentlyContinue
if ($summary.reported) { exit 0 }
exit 1
