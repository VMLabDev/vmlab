param(
    [ValidateSet('msvc', 'sign-only')][string]$Mode = 'msvc',
    [string]$Package,
    [string]$ToolsRoot = 'C:\vdisplay-tools'
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
if (-not $Package) { $Package = Join-Path $PSScriptRoot 'target\package' }
Add-Type -AssemblyName System.IO.Compression.FileSystem
$toolchain = Get-Content (Join-Path $PSScriptRoot 'build-vm\toolchain\toolchain.json') -Raw | ConvertFrom-Json
$dependencies = Get-Content (Join-Path $PSScriptRoot 'dependencies.json') -Raw | ConvertFrom-Json
$packages = @{}
New-Item -ItemType Directory -Force -Path "$ToolsRoot\downloads", "$ToolsRoot\packages", $Package | Out-Null

function Get-Sha256([string]$Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

# Size and first bytes of a download, so a mismatch shows whether an error or interstitial page arrived.
function Get-DownloadSummary([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    try {
        $buffer = New-Object byte[] 64
        $count = $stream.Read($buffer, 0, $buffer.Length)
        $length = $stream.Length
    } finally { $stream.Dispose() }
    $head = New-Object byte[] $count
    [Array]::Copy($buffer, $head, $count)
    $text = [Text.Encoding]::ASCII.GetString($head) -replace '[^\x20-\x7E]', '.'
    "$length bytes, first $count bytes: $([BitConverter]::ToString($head)) ($text)"
}

$attempts = 5
$entries = @($dependencies.PSObject.Properties) + @($toolchain.packages.PSObject.Properties)
# sign-only runs only sign.ps1 from the WDK and SDK; ivshmem already comes inside the package that build.py made.
if ($Mode -eq 'sign-only') { $entries = @($entries | Where-Object { $_.Name -in @('wdk', 'sdk') }) }
foreach ($entry in $entries) {
    $name = $entry.Name
    $url = $entry.Value.url
    $expected = $entry.Value.sha256
    $archive = Join-Path "$ToolsRoot\downloads" "$name.zip"
    # The downloads directory is restored from the CI cache, so a stale or damaged archive is fetched again.
    if ((Test-Path -LiteralPath $archive) -and ((Get-Sha256 $archive) -ne $expected)) {
        Write-Output "Discarding $archive`: SHA256 does not match the pin ($(Get-DownloadSummary $archive))"
        Remove-Item -LiteralPath $archive
    }
    if (-not (Test-Path -LiteralPath $archive)) {
        Write-Output "Fetching $name"
        for ($attempt = 1; $attempt -le $attempts; $attempt++) {
            $partial = $archive + '.partial-' + [Guid]::NewGuid().ToString('N')
            try {
                Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $partial
                # A server can answer 200 with an error page, so verify before the archive takes its final name.
                $actual = Get-Sha256 $partial
                if ($actual -ne $expected) {
                    throw "SHA256 mismatch for $name from $url`: got $actual, expected $expected; $(Get-DownloadSummary $partial)"
                }
                Move-Item -LiteralPath $partial -Destination $archive
                break
            } catch {
                Write-Output "Fetching $name failed (attempt $attempt of $attempts): $($_.Exception.Message)"
                if (Test-Path -LiteralPath $partial) { Remove-Item -LiteralPath $partial }
                if ($attempt -eq $attempts) { throw }
                Start-Sleep -Seconds (5 * $attempt)
            }
        }
    }
    $destination = Join-Path "$ToolsRoot\packages" $name
    # Re-expand verified inputs so a previous interrupted build cannot leave damaged headers or libraries.
    Expand-Archive -Path $archive -DestinationPath $destination -Force
    Set-Content -Path "$destination\complete" -Value $entry.Value.sha256 -Encoding ASCII
    $packages[$name] = $destination
}

if ($Mode -eq 'msvc') {
    $vswhere = 'C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe'
    $installation = & $vswhere -products Microsoft.VisualStudio.Product.BuildTools -property installationPath `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64
    $version = & $vswhere -products Microsoft.VisualStudio.Product.BuildTools -property installationVersion
    if ($version -ne $toolchain.visual_studio.version) { throw "Unexpected Visual Studio version: $version" }
    $vc = Get-ChildItem (Join-Path $installation 'VC\Tools\MSVC') -Directory | Sort-Object Name | Select-Object -Last 1
    $compiler = Join-Path $vc.FullName 'bin\Hostx64\x64\cl.exe'
    $linker = Join-Path $vc.FullName 'bin\Hostx64\x64\link.exe'
    $sdk = Join-Path $packages['sdk'] 'c\Include\10.0.26100.0'
    $wdk = Join-Path $packages['wdk'] 'c'
    $crt = Get-Item "$($packages['crt-headers'])\Contents\VC\Tools\MSVC\*\include"
    $crtLib = Get-Item "$($packages['crt-libs'])\Contents\VC\Tools\MSVC\*\lib\x64"
    $aliasLib = Get-Item "$($packages['crt-aliases'])\Contents\VC\Tools\MSVC\*\lib\x64\store"
    $sdkLib = Join-Path $packages['sdk-libs'] 'c\um\x64'
    $ucrtLib = Join-Path $packages['sdk-libs'] 'c\ucrt\x64'
    $work = Join-Path $PSScriptRoot 'target\msvc-build'
    New-Item -ItemType Directory -Force -Path $work | Out-Null
    $includes = @("$sdk\um", "$sdk\shared", "$sdk\ucrt", $crt.FullName,
        "$wdk\Include\10.0.26100.0\um", "$wdk\Include\10.0.26100.0\shared",
        "$wdk\Include\wdf\umdf\2.27", "$wdk\Include\10.0.26100.0\um\iddcx\1.4")
    $env:INCLUDE = $includes -join ';'
    $env:LIB = @($crtLib.FullName, $aliasLib.FullName, $sdkLib, $ucrtLib) -join ';'
    $common = @('/nologo', '/c', '/std:c++17', '/O2', '/MT', '/GS', '/W4', '/WX', '/Brepro', '/D_WIN32_WINNT=0x0A00')
    & $compiler @common /GR- /DUMDF_DRIVER /D_WIN64 /D_AMD64_ /DAMD64 /DUMDF_VERSION_MAJOR=2 /DUMDF_VERSION_MINOR=27 /DUMDF_MINIMUM_VERSION_REQUIRED=27 `
        /DIDDCX_VERSION_MAJOR=1 /DIDDCX_VERSION_MINOR=4 /DIDDCX_MINIMUM_VERSION_REQUIRED=4 `
        (Join-Path $PSScriptRoot 'driver.cpp') "/Fo$work\driver.obj"
    if ($LASTEXITCODE -ne 0) { throw 'Driver compilation failed.' }
    & $linker /nologo /dll /machine:x64 /subsystem:windows,10.0 /Brepro /dynamicbase /nxcompat `
        "/out:$Package\vmlab-display.dll" "/implib:$work\vmlab-display.lib" "$work\driver.obj" `
        kernel32.lib ntdll.lib cfgmgr32.lib d3d11.lib dxgi.lib dxguid.lib avrt.lib uuid.lib advapi32.lib `
        "$wdk\Lib\wdf\umdf\x64\2.27\WdfDriverStubUm.lib" "$wdk\Lib\10.0.26100.0\um\x64\iddcx\1.4\iddcxstub.lib"
    if ($LASTEXITCODE -ne 0) { throw 'Driver link failed.' }
    & $compiler @common (Join-Path $PSScriptRoot 'install.cpp') "/Fo$work\install.obj"
    if ($LASTEXITCODE -ne 0) { throw 'Installer compilation failed.' }
    & $linker /nologo /machine:x64 /subsystem:console,10.0 /Brepro /dynamicbase /nxcompat `
        "/out:$Package\install.exe" "$work\install.obj" kernel32.lib setupapi.lib newdev.lib uuid.lib user32.lib advapi32.lib
    if ($LASTEXITCODE -ne 0) { throw 'Installer link failed.' }
    foreach ($file in @('vmlab-display.inf', 'install.ps1', 'sign.ps1', 'LICENSE-Microsoft.txt', 'LICENSE-IVSHMEM.txt')) {
        Copy-Item (Join-Path $PSScriptRoot $file) $Package -Force
    }
    New-Item -ItemType Directory -Force -Path "$Package\ivshmem" | Out-Null
    foreach ($file in @('ivshmem.inf', 'ivshmem.sys', 'ivshmem.cat')) {
        Copy-Item "$($packages['ivshmem'])\Win10\amd64\$file" "$Package\ivshmem\$file" -Force
    }
    $sources = @{}
    foreach ($file in @('driver.cpp', 'protocol.h', 'ivshmem.h', 'install.cpp', 'vmlab-display.inf',
        'build.py', 'build.ps1', 'sign.ps1', 'install.ps1', 'dependencies.json', 'build-vm/toolchain/toolchain.json')) {
        $sources[$file] = (Get-FileHash (Join-Path $PSScriptRoot $file) -Algorithm SHA256).Hash.ToLowerInvariant()
    }
    $identityText = ($sources.GetEnumerator() | Sort-Object Name | ForEach-Object { "$($_.Key)=$($_.Value)`n" }) -join ''
    $hasher = [Security.Cryptography.SHA256]::Create()
    try { $identity = [BitConverter]::ToString($hasher.ComputeHash([Text.Encoding]::UTF8.GetBytes($identityText))).Replace('-', '').ToLowerInvariant() }
    finally { $hasher.Dispose() }
    Add-Type @'
using System.Runtime.InteropServices;
public static class VmlabInf {
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern bool WritePrivateProfileString(string section, string key, string value, string path);
}
'@
    $driverVersion = '11/04/2026,1.0.' + [Convert]::ToUInt32($identity.Substring(0, 4), 16) + '.' + [Convert]::ToUInt32($identity.Substring(4, 4), 16)
    if (-not [VmlabInf]::WritePrivateProfileString('Version', 'DriverVer', $driverVersion, (Join-Path $Package 'vmlab-display.inf'))) {
        throw 'Updating the INF build version failed.'
    }
    @{ sources = $sources; compiler = $compiler; visual_studio = $version } | ConvertTo-Json -Depth 5 |
        Set-Content "$Package\BUILDINFO.json" -Encoding UTF8
}
& (Join-Path $PSScriptRoot 'sign.ps1') -Package $Package `
    -WdkBin "$($packages['wdk'])\c\bin\10.0.26100.0\x86" -SdkBin "$($packages['sdk'])\c\bin\10.0.26100.0\x64"
if ($LASTEXITCODE -ne 0) { throw 'Package signing failed.' }
