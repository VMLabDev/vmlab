# dc01, after the forest is up: the domain account every surface in this lab
# attaches as, and one share for the attached terminal to reach.
#
# The account is created with the same secret the lab file declares for it —
# a provision script creates *exactly* the account the declaration names,
# rather than the password living in two places that drift (PRD §19.2).

param(
    [string]$Secret = 'vmlab123!'
)

$ErrorActionPreference = 'Stop'

if (-not (Get-ADUser -Filter "SamAccountName -eq 'dev'")) {
    Write-Output 'creating PROBE\dev'
    New-ADUser -Name 'dev' -SamAccountName 'dev' `
        -AccountPassword (ConvertTo-SecureString $Secret -AsPlainText -Force) `
        -Enabled $true -PasswordNeverExpires $true
    # Domain Admins so the member's minted logon is elevated there too; a
    # narrower group works, and `elevated = false` on the login block is the
    # other half of that choice (§19.2).
    Add-ADGroupMember -Identity 'Domain Admins' -Members 'dev'
} else {
    Write-Output 'PROBE\dev already exists'
}

New-Item -ItemType Directory -Force -Path C:\team | Out-Null
'the domain share is reachable from the editor terminal' |
    Set-Content C:\team\README.txt

if (-not (Get-SmbShare -Name team -ErrorAction SilentlyContinue)) {
    Write-Output 'publishing \\dc01\team'
    New-SmbShare -Name team -Path C:\team -FullAccess 'PROBE\dev' | Out-Null
} else {
    Write-Output '\\dc01\team already published'
}
