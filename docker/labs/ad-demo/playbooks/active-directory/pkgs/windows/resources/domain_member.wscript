use value
use shell

fn param_str(params: Value, key: string, fallback: string) -> string {
    if let Some(v) = params.get(key) { if let Some(s) = v.as_string() { return s } }
    fallback
}

fn ps_q(s: string) -> string { "'" + s.replace("'", "''") + "'" }

// PowerShell that binds $cred to a PSCredential for the given account.
fn ps_cred(user: string, pw: string) -> string {
    "$cp = ConvertTo-SecureString " + ps_q(pw) + " -AsPlainText -Force; " +
    "$cred = New-Object System.Management.Automation.PSCredential(" + ps_q(user) + ", $cp); "
}

// True when this machine is already a member of `domain`.
fn member_of(domain: string) -> Result[bool, string] {
    let script = "$ErrorActionPreference='Stop'; $c = Get-CimInstance Win32_ComputerSystem; " +
        "if ($c.PartOfDomain -and $c.Domain -eq " + ps_q(domain) + ") {{ 'YES' }} else {{ 'NO' }}"
    let out = shell::powershell(script, Value::Null)?
    if !out.success { return Err(out.stderr.trim()) }
    Ok(out.stdout.trim() == "YES")
}

fn check(params: Value) -> Result[CheckResult, string] {
    let domain = param_str(params, "domain_name", "")
    if domain == "" { return Err("missing 'domain_name' parameter") }
    if member_of(domain)? { Ok(CheckResult::AlreadyConfigured) } else { Ok(CheckResult::NotConfigured) }
}

fn apply(params: Value) -> Result[ApplyResult, string] {
    let domain = param_str(params, "domain_name", "")
    let user = param_str(params, "credential_user", "")
    let cpw = param_str(params, "credential_password", "")
    if domain == "" { return Err("missing 'domain_name' parameter") }
    if user == "" { return Err("missing 'credential_user' parameter") }
    if cpw == "" { return Err("missing 'credential_password' parameter") }

    let nn = param_str(params, "new_name", "")
    let o_nn = if nn != "" { " -NewName " + ps_q(nn) } else { "" }

    let script = "$ErrorActionPreference='Stop'; " +
        ps_cred(user, cpw) +
        "Add-Computer -DomainName " + ps_q(domain) +
        " -Credential $cred" + o_nn + " -Force | Out-Null"
    let out = shell::powershell(script, Value::Null)?
    if !out.success { return Err("domain join failed: " + out.stderr.trim()) }
    Ok(ApplyResult::RebootRequired)   // joining a domain requires a reboot to finish
}
