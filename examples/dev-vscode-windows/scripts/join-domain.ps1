# dev01: join the PROBE domain, unless it is already a member.
#
# Runs as the agent identity, which is correct: joining is something done to
# the *machine*, not as the developer. The provision beside this one is the
# other case — anything that must land as the developer rather than as the
# machine belongs in `provision {}` under a login (PRD §19.8).

param(
    [string]$Domain = 'probe.local',
    [string]$User   = 'PROBE\Administrator',
    [string]$Secret = 'vmlab123!'
)

$ErrorActionPreference = 'Stop'

if ((Get-CimInstance Win32_ComputerSystem).PartOfDomain) {
    Write-Output "already joined to $((Get-CimInstance Win32_ComputerSystem).Domain)"
    return
}

# The domain segment declares dc01 as its DNS server, so the DHCP lease
# already points here; this only matters if you re-point the segment.
Write-Output "resolving $Domain"
Resolve-DnsName -Name $Domain -Type A -ErrorAction Stop | Out-Null

$cred = New-Object System.Management.Automation.PSCredential(
    $User, (ConvertTo-SecureString $Secret -AsPlainText -Force))

Write-Output "joining $Domain"
Add-Computer -DomainName $Domain -Credential $cred -Force
Write-Output 'REBOOT-REQUIRED'
