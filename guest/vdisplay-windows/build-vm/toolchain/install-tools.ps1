param([switch]$Check)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$root = 'C:\vdisplay-tools'
$installation = 'C:\vdisplay-tools\VS'
$lock = Get-Content (Join-Path $PSScriptRoot 'toolchain.json') -Raw | ConvertFrom-Json
$vswhere = 'C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe'

function Test-Toolchain {
    if (-not (Test-Path $vswhere)) { return $false }
    $instance = & $vswhere -products Microsoft.VisualStudio.Product.BuildTools -property installationVersion `
        -requires Microsoft.VisualStudio.Workload.VCTools Microsoft.VisualStudio.Component.VC.Tools.x86.x64
    if ($instance -ne $lock.visual_studio.version) { return $false }
    foreach ($property in $lock.packages.PSObject.Properties) {
        $marker = Join-Path $root "packages\$($property.Name)\complete"
        if (-not (Test-Path $marker) -or (Get-Content $marker -Raw).Trim() -ne $property.Value.sha256) { return $false }
    }
    return $true
}

if ($Check) {
    if (Test-Toolchain) { exit 0 }
    exit 1
}

New-Item -ItemType Directory -Force -Path "$root\downloads", "$root\packages" | Out-Null
function Get-LockedFile($entry, [string]$name) {
    $path = Join-Path "$root\downloads" $name
    if (-not (Test-Path $path)) { Invoke-WebRequest -UseBasicParsing -Uri $entry.url -OutFile $path }
    if ((Get-FileHash $path -Algorithm SHA256).Hash.ToLowerInvariant() -ne $entry.sha256) {
        throw "SHA256 mismatch: $path"
    }
    return $path
}

foreach ($property in $lock.packages.PSObject.Properties) {
    $archive = Get-LockedFile $property.Value "$($property.Name).zip"
    $destination = Join-Path "$root\packages" $property.Name
    if (-not (Test-Path "$destination\complete") -or (Get-Content "$destination\complete" -Raw).Trim() -ne $property.Value.sha256) {
        Expand-Archive -Path $archive -DestinationPath $destination -Force
        Set-Content -Path "$destination\complete" -Value $property.Value.sha256 -Encoding ASCII
    }
}

$bootstrapper = Get-LockedFile $lock.visual_studio.bootstrapper 'vs_Setup.exe'
$catalog = Get-LockedFile $lock.visual_studio.catalog 'VisualStudio.vsman'
$channel = Get-LockedFile $lock.visual_studio.channel 'VisualStudio.chman'
$arguments = @('--quiet', '--wait', '--norestart', '--noUpdateInstaller',
    '--productId', 'Microsoft.VisualStudio.Product.BuildTools', '--channelId', 'VisualStudio.17.Release',
    '--channelUri', $channel, '--installChannelUri', $channel, '--installCatalogUri', $catalog,
    '--installPath', $installation, '--add', 'Microsoft.VisualStudio.Workload.VCTools',
    '--add', 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64', '--add', 'Microsoft.VisualStudio.Component.Windows11SDK.26100')
$process = Start-Process -FilePath $bootstrapper -ArgumentList $arguments -Wait -PassThru
if ($process.ExitCode -in @(3010, 1641)) { exit $process.ExitCode }
if ($process.ExitCode -ne 0) { throw "Visual Studio installation failed: $($process.ExitCode)" }
if (-not (Test-Toolchain)) { throw 'The installed toolchain does not match toolchain.json.' }
exit 0
