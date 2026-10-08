use value
use fs
use shell

fn check(params: Value) -> Result[CheckResult, string] {
    if fs::exists("/tmp/e2e-slow") {
        Ok(CheckResult::NotConfigured)
    } else {
        Ok(CheckResult::AlreadyConfigured)
    }
}

fn apply(params: Value) -> Result[ApplyResult, string] {
    shell::run("sleep 20", value::Value::Null)?
    Ok(ApplyResult::Success)
}
