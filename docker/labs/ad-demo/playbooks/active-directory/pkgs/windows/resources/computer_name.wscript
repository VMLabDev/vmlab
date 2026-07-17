use value
use shell

fn param_str(params: Value, key: string, fallback: string) -> string {
    if let Some(v) = params.get(key) { if let Some(s) = v.as_string() { return s } }
    fallback
}

fn ps_q(s: string) -> string { "'" + s.replace("'", "''") + "'" }

// NetBIOS name comparison is case-insensitive (PowerShell -eq already is).
fn named(name: string) -> Result[bool, string] {
    let script = "$ErrorActionPreference='Stop'; " +
        "if ($env:COMPUTERNAME -eq " + ps_q(name) + ") {{ 'YES' }} else {{ 'NO' }}"
    let out = shell::powershell(script, Value::Null)?
    if !out.success { return Err(out.stderr.trim()) }
    Ok(out.stdout.trim() == "YES")
}

fn check(params: Value) -> Result[CheckResult, string] {
    let name = param_str(params, "name", "")
    if name == "" { return Err("missing 'name' parameter") }
    if named(name)? { Ok(CheckResult::AlreadyConfigured) } else { Ok(CheckResult::NotConfigured) }
}

fn apply(params: Value) -> Result[ApplyResult, string] {
    let name = param_str(params, "name", "")
    if name == "" { return Err("missing 'name' parameter") }
    let script = "$ErrorActionPreference='Stop'; " +
        "Rename-Computer -NewName " + ps_q(name) + " -Force | Out-Null"
    let out = shell::powershell(script, Value::Null)?
    if !out.success { return Err("rename failed: " + out.stderr.trim()) }
    Ok(ApplyResult::RebootRequired)   // the new name takes effect after a reboot
}
