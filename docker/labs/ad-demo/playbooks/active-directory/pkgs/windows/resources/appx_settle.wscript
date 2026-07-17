use value
use shell

// Settle the pending staged-AppX reconciliation after the promotion reboot.
//
// After a Windows Update run, Server 2025 leaves its "Last Known Good" shell
// packages (MicrosoftWindows.LKG.*) staged for no user at the base version.
// Windows reconciles them during the machine's next profile-CREATING logon,
// and explorer.exe starting mid-reconciliation fail-fasts (0xc0000409) —
// permanently breaking that profile's shell. The DC promotion reboot happens
// before anyone has logged on, so the first console logon on the fresh DC is
// exactly that victim. Attempting the removal here as SYSTEM consumes the
// pending work (the LKG system apps refuse removal — the attempt is enough),
// so the operator's first logon gets a working desktop.
//
// Idempotency: the settle leaves no queryable state (the packages stay
// staged), so apply records a marker under HKLM:\SOFTWARE\vmlab.

fn marker_set() -> Result[bool, string] {
    let script = "$v = (Get-ItemProperty -Path 'HKLM:\\SOFTWARE\\vmlab' -Name 'AppxSettledPostPromo' -ErrorAction SilentlyContinue).AppxSettledPostPromo; " +
        "if ($v -eq 1) {{ 'YES' }} else {{ 'NO' }}"
    let out = shell::powershell(script, Value::Null)?
    if !out.success { return Ok(false) }
    Ok(out.stdout.trim() == "YES")
}

fn check(params: Value) -> Result[CheckResult, string] {
    if marker_set()? { Ok(CheckResult::AlreadyConfigured) } else { Ok(CheckResult::NotConfigured) }
}

fn apply(params: Value) -> Result[ApplyResult, string] {
    // Settle the stale staged packages, then hold until the DC is ~5 min
    // past the promotion reboot: a profile-CREATING logon earlier than that
    // permanently breaks the new profile's shell on 24H2 images sysprepped
    // as Local System — the operator's first console logon must land on a
    // warm machine.
    let script = "$ErrorActionPreference = 'Continue'; " +
        "$prov = (Get-AppxProvisionedPackage -Online).DisplayName; " +
        "Get-AppxPackage -AllUsers | Where-Object {{ -not $_.IsFramework -and -not $_.IsResourcePackage -and $prov -notcontains $_.Name -and -not ($_.PackageUserInformation | Where-Object InstallState -eq 'Installed') }} | ForEach-Object {{ try {{ Remove-AppxPackage -Package $_.PackageFullName -AllUsers -ErrorAction Stop }} catch {{ }} }}; " +
        "New-Item -Path 'HKLM:\\SOFTWARE\\vmlab' -Force | Out-Null; " +
        "Set-ItemProperty -Path 'HKLM:\\SOFTWARE\\vmlab' -Name 'AppxSettledPostPromo' -Value 1 -Type DWord; " +
        "$up = [int]((Get-Date) - (Get-CimInstance Win32_OperatingSystem).LastBootUpTime).TotalSeconds; " +
        "if ($up -lt 300) {{ Start-Sleep -Seconds (300 - $up) }}; " +
        "exit 0"
    let opts = Value::Map(#{ "timeout": Value::Int(600) })
    let out = shell::powershell(script, opts)?
    if !out.success { return Err("AppX settle failed: " + out.stderr) }
    Ok(ApplyResult::Success)
}
