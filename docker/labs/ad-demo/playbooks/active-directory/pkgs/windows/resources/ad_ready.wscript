use value
use shell

// Whether AD DS on this DC answers a domain query right now.
fn ad_answers() -> Result[bool, string] {
    let script = "$ErrorActionPreference='Stop'; " +
        "try {{ Import-Module ActiveDirectory -ErrorAction Stop; Get-ADDomain -ErrorAction Stop | Out-Null; 'YES' }} catch {{ 'NO' }}"
    let out = shell::powershell(script, Value::Null)?
    if !out.success { return Ok(false) }
    Ok(out.stdout.trim() == "YES")
}

fn check(params: Value) -> Result[CheckResult, string] {
    if ad_answers()? { Ok(CheckResult::AlreadyConfigured) } else { Ok(CheckResult::NotConfigured) }
}

// Poll inside one PowerShell invocation: the first boot after a forest
// promotion can take several minutes before LDAP answers. 60 × 15s ≈ 15min.
fn apply(params: Value) -> Result[ApplyResult, string] {
    let script = "$ErrorActionPreference='SilentlyContinue'; " +
        "for ($i = 0; $i -lt 60; $i++) {{ " +
        "try {{ Import-Module ActiveDirectory -ErrorAction Stop; Get-ADDomain -ErrorAction Stop | Out-Null; exit 0 }} " +
        "catch {{ Start-Sleep -Seconds 15 }} " +
        "}}; exit 1"
    let opts = Value::Map(#{ "timeout": Value::Int(960) })
    let out = shell::powershell(script, opts)?
    if !out.success { return Err("Active Directory did not answer within 15 minutes") }
    Ok(ApplyResult::Success)
}
