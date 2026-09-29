// The segment API at runtime. The host side (a listener, a connect) is the
// scenario's; this script adds and removes rules and reports what the guest
// saw. The host address comes from /etc/e2e-host, which the scenario writes
// into the guest first.
use vmlab

fn probe(vm: Machine, url: string) -> Result[string, string] {
    let r = vm.exec("/bin/sh", ["-c", "wget -q -T 5 -O /dev/null " + url + " >/dev/null 2>&1 && echo WGET-OK || echo WGET-FAIL"])?
    Ok(r.stdout.trim())
}

fn ping(m: Machine, ip: string) -> Result[string, string] {
    let r = m.exec("/bin/sh", ["-c", "ping -c 2 -W 2 " + ip + " >/dev/null 2>&1 && echo PING-OK || echo PING-FAIL"])?
    Ok(r.stdout.trim())
}

fn run(lab: Lab) -> Result[unit, string] {
    let vm = lab.vm("vm01")?
    let lan = lab.segment("lan")?
    let host = vm.exec("/bin/cat", ["/etc/e2e-host"])?.stdout.trim()
    let url = "http://" + host + ":18686/"

    // dns_set: a name only this script put in the zone.
    let id = lan.dns_set("e2e-dyn.scripting.test", "10.86.0.77")?
    let d = vm.exec("/bin/sh", ["-c", "nslookup e2e-dyn.scripting.test 10.86.0.1 2>&1"])?
    lab.log("e2e-dns-set " + d.stdout)
    let cleared = lan.dns_clear(id)?
    let d2 = vm.exec("/bin/sh", ["-c", "nslookup e2e-dyn.scripting.test 10.86.0.1 2>&1"])?
    lab.log("e2e-dns-cleared " + fmt("{}", cleared) + " " + d2.stdout)

    // block / unblock: the host's listener through the gateway.
    lab.log("e2e-block-before " + probe(vm, url)?)
    let bid = lan.block(host)?
    lab.log("e2e-block-rules " + lan.rules()?)
    lab.log("e2e-block-during " + probe(vm, url)?)
    let gone = lan.unblock(bid)?
    lab.log("e2e-block-after " + probe(vm, url)?)

    // forward: publish a guest listener on the host.
    let serve = vm.exec("/bin/sh", ["-c", "setsid sh -c 'while true; do echo e2e-forwarded | nc -l -p 8686; done' </dev/null >/dev/null 2>&1 &"])?
    let fid = lan.forward(18687, "vm01", 8686)?
    lab.log("e2e-forward-id " + fmt("{}", fid))

    // route_to / unroute_to: the container on "back" reaches vm01 on "lan"
    // only while the pair is connected, and a pair is one thing -- it is
    // taken apart from the other side.
    let web = lab.container("web")?
    let back = lab.segment("back")?
    lab.log("e2e-route-before " + ping(web, "10.86.0.10")?)
    lan.route_to("back")?
    lab.log("e2e-route-during " + ping(web, "10.86.0.10")?)
    back.unroute_to("lan")?
    lab.log("e2e-route-after " + ping(web, "10.86.0.10")?)
    match lan.route_to("lan") {
        Ok(_) => lab.log("e2e-route-self ok"),
        Err(e) => lab.log("e2e-route-self err " + e),
    }
    Ok(())
}

fn main(lab: Lab) {
    match run(lab) {
        Ok(u) => u,
        Err(e) => {
            let failed: Result[unit, string] = Err(e)
            failed.expect("segment failed: " + e)
        },
    }
}
