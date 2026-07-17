use value
use shell

fn param_str(params: Value, key: string, fallback: string) -> string {
    if let Some(v) = params.get(key) { if let Some(s) = v.as_string() { return s } }
    fallback
}

fn param_bool(params: Value, key: string, fallback: bool) -> bool {
    if let Some(v) = params.get(key) { if let Some(b) = v.as_bool() { return b } }
    fallback
}

fn ps_q(s: string) -> string { "'" + s.replace("'", "''") + "'" }

// True when this machine is already a DC whose domain matches `domain`.
// Win32_ComputerSystem.DomainRole: 4 = backup DC, 5 = primary DC.
fn is_dc_for(domain: string) -> Result[bool, string] {
    let script = "$ErrorActionPreference='Stop'; $c = Get-CimInstance Win32_ComputerSystem; " +
        "if ($c.DomainRole -ge 4 -and $c.Domain -eq " + ps_q(domain) + ") {{ 'YES' }} else {{ 'NO' }}"
    let out = shell::powershell(script, Value::Null)?
    if !out.success { return Err(out.stderr.trim()) }
    Ok(out.stdout.trim() == "YES")
}

fn check(params: Value) -> Result[CheckResult, string] {
    let domain = param_str(params, "domain_name", "")
    if domain == "" { return Err("missing 'domain_name' parameter") }
    if is_dc_for(domain)? { Ok(CheckResult::AlreadyConfigured) } else { Ok(CheckResult::NotConfigured) }
}

fn apply(params: Value) -> Result[ApplyResult, string] {
    let domain = param_str(params, "domain_name", "")
    let pw = param_str(params, "safe_mode_password", "")
    if domain == "" { return Err("missing 'domain_name' parameter") }
    if pw == "" { return Err("missing 'safe_mode_password' parameter") }

    let nb = param_str(params, "netbios_name", "")
    let o_nb = if nb != "" { " -DomainNetbiosName " + ps_q(nb) } else { "" }
    let dns = if param_bool(params, "install_dns", true) { "$true" } else { "$false" }

    // Promotion can take several minutes on first boot hardware.
    let opts = Value::Map(#{ "timeout": Value::Int(1800) })
    let script = "$ErrorActionPreference='Stop'; Import-Module ADDSDeployment; " +
        "$smp = ConvertTo-SecureString " + ps_q(pw) + " -AsPlainText -Force; " +
        "Install-ADDSForest -DomainName " + ps_q(domain) +
        " -SafeModeAdministratorPassword $smp -InstallDns:" + dns +
        o_nb + " -Force -NoRebootOnCompletion | Out-Null"
    let out = shell::powershell(script, opts)?
    if !out.success { return Err("forest promotion failed: " + out.stderr.trim()) }
    Ok(ApplyResult::RebootRequired)   // DC promotion always needs a reboot to finish
}
