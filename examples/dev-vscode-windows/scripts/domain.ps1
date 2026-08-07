# dc01: stand up the PROBE forest, create the developer's domain account,
# and publish one domain share (PRD §19.8's worked example).
#
# Idempotent by observation: a second `vmlab up` finds each piece already
# there and does nothing. Promotion is the only step that reboots, and
# scripts/domain.ws is what waits for the guest to come back.
#
# vmlab pushes this file and runs it. It does not read it: "vmlab moves bytes
# it is told to move and never interprets them" (§19.8).

param(
    [string]$Domain  = 'probe.local',
    [string]$NetBios = 'PROBE',
    [string]$Secret  = 'vmlab123!'
)

$ErrorActionPreference = 'Stop'

function Test-Promoted {
    (Get-CimInstance Win32_ComputerSystem).Domain -eq $Domain
}

if (Test-Promoted) {
    Write-Output "already $Domain"
} else {
    Write-Output 'installing AD DS'
    Install-WindowsFeature AD-Domain-Services -IncludeManagementTools | Out-Null

    Write-Output "promoting to the $Domain forest"
    $safe = ConvertTo-SecureString $Secret -AsPlainText -Force
    Install-ADDSForest `
        -DomainName $Domain `
        -DomainNetbiosName $NetBios `
        -SafeModeAdministratorPassword $safe `
        -InstallDns `
        -NoRebootOnCompletion `
        -Force | Out-Null

    # The caller reboots and waits; saying so is how it knows to.
    Write-Output 'REBOOT-REQUIRED'
}
