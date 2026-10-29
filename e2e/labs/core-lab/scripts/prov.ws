// Every core-lab machine's one provision: it says when it begins and ends,
// three seconds apart, so the scenario can read from `up`'s output which
// provisions overlapped and which waited for another's (§7.2).
use vmlab

fn main(lab: Lab) -> Result[unit, string] {
    let me = lab.this_vm()?
    lab.log("e2e-prov " + me.name() + " begin")
    vmlab::sleep_ms(3000)
    lab.log("e2e-prov " + me.name() + " end")
    Ok(())
}
