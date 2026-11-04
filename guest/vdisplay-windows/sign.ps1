param(
    [Parameter(Mandatory = $true)][string]$WdkBin,
    [Parameter(Mandatory = $true)][string]$SdkBin,
    [string]$Package
)
$ErrorActionPreference = 'Stop'
if (-not $Package) { $Package = $PSScriptRoot }
$Package = (Resolve-Path $Package).Path
$inf2cat = Join-Path $WdkBin 'Inf2Cat.exe'
$signtool = Join-Path $SdkBin 'signtool.exe'
foreach ($tool in @($inf2cat, $signtool)) {
    if (-not (Test-Path $tool)) { throw "Required tool missing: $tool" }
}
$certificate = New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN=VMLab display test signing' `
    -CertStoreLocation Cert:\CurrentUser\My -KeyAlgorithm RSA -KeyLength 3072 -HashAlgorithm SHA256 `
    -NotAfter (Get-Date).AddYears(5)
$catalogWork = Join-Path ([IO.Path]::GetTempPath()) ('vdisplay-catalog-' + [Guid]::NewGuid().ToString('N'))
try {
    Export-Certificate -Cert $certificate -FilePath (Join-Path $Package 'vmlab-display.cer') | Out-Null
    & $signtool sign /sha1 $certificate.Thumbprint /s My /fd SHA256 (Join-Path $Package 'vmlab-display.dll')
    if ($LASTEXITCODE -ne 0) { throw "DLL signing failed: $LASTEXITCODE" }
    # Inf2Cat scans subdirectories too. Keep the upstream signed IVSHMEM catalog out of its input.
    New-Item -ItemType Directory -Path $catalogWork | Out-Null
    Copy-Item (Join-Path $Package 'vmlab-display.inf'), (Join-Path $Package 'vmlab-display.dll') $catalogWork
    & $inf2cat "/driver:$catalogWork" /os:10_X64 /uselocaltime
    if ($LASTEXITCODE -ne 0) { throw "Catalog generation failed: $LASTEXITCODE" }
    Copy-Item (Join-Path $catalogWork 'vmlab-display.cat') (Join-Path $Package 'vmlab-display.cat') -Force
    & $signtool sign /sha1 $certificate.Thumbprint /s My /fd SHA256 (Join-Path $Package 'vmlab-display.cat')
    if ($LASTEXITCODE -ne 0) { throw "Catalog signing failed: $LASTEXITCODE" }
    $files = @{}
    foreach ($file in @('vmlab-display.inf', 'vmlab-display.cat', 'vmlab-display.dll', 'vmlab-display.cer',
        'install.exe', 'install.ps1', 'BUILDINFO.json', 'ivshmem/ivshmem.inf', 'ivshmem/ivshmem.cat', 'ivshmem/ivshmem.sys')) {
        $files[$file] = (Get-FileHash (Join-Path $Package $file) -Algorithm SHA256).Hash.ToLowerInvariant()
    }
    @{ files = $files } | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $Package 'PACKAGE.json') -Encoding UTF8
} finally {
    if (Test-Path $catalogWork) { Remove-Item -LiteralPath $catalogWork -Recurse -Force }
    Remove-Item "Cert:\CurrentUser\My\$($certificate.Thumbprint)" -DeleteKey
}
Write-Output "Test-signed driver package: $Package"
