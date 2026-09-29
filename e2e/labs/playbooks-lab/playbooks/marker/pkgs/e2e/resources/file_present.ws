use value
use fs
use path
use log

fn param_str(params: Value, key: string) -> string {
    if let Some(v) = params.get(key) {
        if let Some(s) = v.as_string() {
            return s
        }
    }
    ""
}

fn check(params: Value) -> Result[CheckResult, string] {
    let p = param_str(params, "path")
    if !fs::exists(p) {
        return Ok(CheckResult::NotConfigured)
    }
    if fs::read(p)? == param_str(params, "content") {
        Ok(CheckResult::AlreadyConfigured)
    } else {
        Ok(CheckResult::NotConfigured)
    }
}

fn apply(params: Value) -> Result[ApplyResult, string] {
    let p = param_str(params, "path")
    log::info("writing " + p)
    fs::mkdir(path::parent(p))?
    fs::write(p, param_str(params, "content"))?
    Ok(ApplyResult::Success)
}
