param([switch]$VirtualDisplayEnabled)
$ErrorActionPreference = 'Stop'
if (-not $VirtualDisplayEnabled) {
    throw 'The machine must enable gpu.virtual_display. No changes were made.'
}
if (-not [Environment]::Is64BitOperatingSystem -or [Environment]::OSVersion.Version.Build -lt 22000) {
    throw 'VMLab display requires x64 Windows 11.'
}
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'An elevated agent or Administrator session is required.'
}
$pci = Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -like 'PCI\VEN_1AF4&DEV_1110*' }
if (-not $pci) { throw 'The virtual-display IVSHMEM PCI device is absent. No changes were made.' }
$boot = & bcdedit.exe /enum '{current}'
if ($LASTEXITCODE -ne 0 -or -not ($boot -match 'testsigning\s+Yes')) {
    throw 'Enable Windows Test Mode and reboot before installing the test-signed driver.'
}
foreach ($file in @('vmlab-display.inf', 'vmlab-display.cat', 'vmlab-display.dll', 'vmlab-display.cer',
                     'install.exe', 'ivshmem\ivshmem.inf', 'ivshmem\ivshmem.cat', 'ivshmem\ivshmem.sys')) {
    if (-not (Test-Path (Join-Path $PSScriptRoot $file))) { throw "Package file missing: $file" }
}
$upstream = Get-AuthenticodeSignature (Join-Path $PSScriptRoot 'ivshmem\ivshmem.cat')
if ($upstream.Status -ne 'Valid' -or $upstream.SignerCertificate.Thumbprint -ne '66ED6843B0F6DC9B636C6814E53A26983EF32B4E') {
    throw 'The IVSHMEM catalog does not have the pinned Red Hat publisher signature.'
}
# PnP requires explicit publisher trust for unattended installation, even with a valid chain.
$publishers = New-Object Security.Cryptography.X509Certificates.X509Store('TrustedPublisher', 'LocalMachine')
$publishers.Open([Security.Cryptography.X509Certificates.OpenFlags]::ReadWrite)
try { $publishers.Add($upstream.SignerCertificate) } finally { $publishers.Close() }
Import-Certificate -FilePath (Join-Path $PSScriptRoot 'vmlab-display.cer') -CertStoreLocation Cert:\LocalMachine\Root | Out-Null
Import-Certificate -FilePath (Join-Path $PSScriptRoot 'vmlab-display.cer') -CertStoreLocation Cert:\LocalMachine\TrustedPublisher | Out-Null
& pnputil.exe /add-driver (Join-Path $PSScriptRoot 'ivshmem\ivshmem.inf') /install
if ($LASTEXITCODE -notin @(0, 259, 3010)) { throw "IVSHMEM installation failed: $LASTEXITCODE" }
$service = Get-PnpDeviceProperty -InstanceId $pci[0].InstanceId -KeyName DEVPKEY_Device_Service
if ($service.Data -ne 'IVSHMEM') { throw 'The IVSHMEM PCI device did not bind to the IVSHMEM driver.' }
& (Join-Path $PSScriptRoot 'install.exe') --virtual-display-enabled (Join-Path $PSScriptRoot 'vmlab-display.inf')
if ($LASTEXITCODE -notin @(0, 3010)) { throw "Display installation failed: $LASTEXITCODE" }
if ($LASTEXITCODE -eq 3010) { Write-Output 'Reboot required.' }
$deadline = (Get-Date).AddSeconds(30)
do {
    $display = Get-PnpDevice -PresentOnly -Class Display | Where-Object FriendlyName -EQ 'VMLab shared-memory display'
    if ($display -and $display.Status -eq 'OK') {
        & (Join-Path $PSScriptRoot 'install.exe') --activate-console
        if ($LASTEXITCODE -ne 0) { throw "Activating the indirect console failed: $LASTEXITCODE" }
        $console = Get-Service -Name VMLabDisplayConsole -ErrorAction SilentlyContinue
        if ($console) { Stop-Service VMLabDisplayConsole }
        $serviceRoot = Join-Path $env:ProgramFiles 'VMLab\Display'
        New-Item -ItemType Directory -Force -Path $serviceRoot | Out-Null
        $executable = Join-Path $serviceRoot 'console.exe'
        Copy-Item (Join-Path $PSScriptRoot 'install.exe') $executable -Force
        $binaryPath = '"' + $executable + '" --service'
        if ($console) {
            $registered = Get-CimInstance Win32_Service -Filter "Name='VMLabDisplayConsole'"
            $change = Invoke-CimMethod -InputObject $registered -MethodName Change `
                -Arguments @{ PathName = $binaryPath; StartMode = 'Automatic' }
            if ($change.ReturnValue -ne 0) { throw "Updating the console service failed: $($change.ReturnValue)" }
        } else {
            New-Service -Name VMLabDisplayConsole -BinaryPathName $binaryPath -StartupType Automatic `
                -DisplayName 'VMLab virtual display console' | Out-Null
        }
        Start-Service VMLabDisplayConsole
        Write-Output 'VMLab display device started and selected for the console.'
        exit 0
    }
    Start-Sleep -Seconds 1
} while ((Get-Date) -lt $deadline)
throw "VMLab display did not start: $($display.Status). Inspect the Kernel-PnP configuration log."
