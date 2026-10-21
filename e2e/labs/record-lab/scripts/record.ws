// `m.record` from wscript: a handle whose stop() finalises, and one left
// open for the daemon to finalise when the run ends.
use vmlab

fn main(lab: Lab) {
    let vm = lab.vm("vm01").expect("vm01")
    let r = vm.record("recordings/from-script.webm", #{ "fps": "15", "size": "960x540" }).expect("record")
    lab.log("e2e-recording " + r.path())
    vm.send_keys("enter").expect("keys")
    let path = r.stop().expect("stop")
    lab.log("e2e-stopped " + path)
    let open = vm.record("recordings/left-open.mp4", #{}).expect("record again")
    lab.log("e2e-open " + open.path())
}
