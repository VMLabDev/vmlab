// A PTY session: type a line, expect its computed answer.
use vmlab

fn run(lab: Lab) -> Result[unit, string] {
    let vm = lab.vm("vm01")?
    let t = vm.terminal()?
    t.send_line("echo term-$((6*7))-$(id -un)")?
    let out = t.expect("term-42-[a-z]+", 20)?
    t.close()
    lab.log("e2e-term-saw " + out.trim())
    Ok(())
}

fn main(lab: Lab) {
    match run(lab) {
        Ok(u) => u,
        Err(e) => {
            let failed: Result[unit, string] = Err(e)
            failed.expect("terminal failed: " + e)
        },
    }
}
