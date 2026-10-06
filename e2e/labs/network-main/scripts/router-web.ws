// Provision: serve a marker on router:8080, so `up` waits on this machine
// and the segment forward to it is installed at readiness and again after
// provisioning, on the same host port (#144).
use vmlab

fn setup(lab: Lab) -> Result[unit, string] {
    let vm = lab.this_vm()?
    vm.wait_ready(300)?
    let r = vm.exec("/bin/sh", ["-c", "mkdir -p /tmp/www && echo e2e-router-marker > /tmp/www/r && (setsid nohup python3 -m http.server 8080 --directory /tmp/www >/dev/null 2>&1 </dev/null &) && for i in $(seq 1 40); do wget -q -O /dev/null http://127.0.0.1:8080/r && exit 0; sleep 0.5; done; exit 1"])?
    if r.exit_code != 0 {
        return Err("router web server did not start: " + r.stderr)
    }
    Ok(())
}

fn main(lab: Lab) {
    match setup(lab) {
        Ok(u) => u,
        Err(e) => {
            let failed: Result[unit, string] = Err(e)
            failed.expect("router-web failed: " + e)
        },
    }
}
