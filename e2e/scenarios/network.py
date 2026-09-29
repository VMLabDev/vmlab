"""Networking: DHCP and reservations, segment DNS, NAT egress, isolation,
forwards, guest routes, L3 rules, MTU, the fast-path report, and a global
segment two labs share on one supervisor."""

import ipaddress
import json
import pathlib
import re
import shutil
import socket
import subprocess
import tempfile

from harness import ScenarioFailed

HOST_MARKER = "e2e-host-marker"
GUEST_MARKER = "e2e-guest-marker"
FORWARD_PORT = 18280


def host_ip() -> str:
    """The container's own address: what a guest's NAT'd flow lands on."""
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("192.0.2.1", 9))  # no packet is sent for a UDP connect
        return s.getsockname()[0]
    finally:
        s.close()


def start_host_http(ports) -> tuple[list[subprocess.Popen], pathlib.Path]:
    """Plain HTTP servers in the container serving one marker file."""
    root = pathlib.Path(tempfile.mkdtemp(prefix="e2e-network-www-"))
    (root / "marker").write_text(HOST_MARKER + "\n")
    procs = [
        subprocess.Popen(
            ["python3", "-m", "http.server", str(p), "--bind", "0.0.0.0", "--directory", str(root)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        for p in ports
    ]
    return procs, root


def curl(h, url: str) -> str:
    r = h.run(["curl", "-s", "-m", "5", url], check=False)
    return r.out if r.code == 0 else ""


def gexec(h, lab, machine: str, *argv: str, timeout: float = 60):
    return h.vmlab("exec", machine, "--", *argv, cwd=lab, check=False, timeout=timeout)


def gsh(h, lab, machine: str, script: str, timeout: float = 60):
    return gexec(h, lab, machine, "/bin/sh", "-c", script, timeout=timeout)


def ipv4(h, lab, machine: str, dev: str = "eth0") -> str:
    """The guest's first IPv4 address on `dev`, or ''."""
    out = gexec(h, lab, machine, "ip", "-4", "-o", "addr", "show", "dev", dev).out
    m = re.search(r"inet (\d+\.\d+\.\d+\.\d+)/", out)
    return m.group(1) if m else ""


def resolve(h, lab, machine: str, name: str, server: str) -> str | None:
    """Ask the segment gateway for `name` with busybox nslookup; the address
    after the `Name:` line, or None when the lookup failed."""
    r = gexec(h, lab, machine, "nslookup", name, server)
    m = re.search(r"Name:\s*\S+\s*\nAddress:\s*(\d+\.\d+\.\d+\.\d+)", r.out)
    return m.group(1) if (r.code == 0 and m) else None


def pings(h, lab, machine: str, target: str) -> tuple[bool, str]:
    r = gexec(h, lab, machine, "ping", "-c", "2", "-W", "2", target, timeout=30)
    return r.code == 0 and " 0% packet loss" in r.out, r.out


def wget(h, lab, machine: str, url: str, timeout: int = 5):
    return gexec(h, lab, machine, "wget", "-q", "-T", str(timeout), "-O", "-", url, timeout=timeout + 30)


def run(h):
    fastpath(h)
    host = host_ip()
    servers, www = start_host_http([9001, 9002])
    try:
        for port in (9001, 9002):
            h.wait_until(lambda port=port: HOST_MARKER in curl(h, f"http://127.0.0.1:{port}/marker"),
                         timeout=20, interval=0.5, what=f"the host-side server on :{port}")
        main_lab(h, host)
    finally:
        for p in servers:
            p.terminate()
        shutil.rmtree(www, ignore_errors=True)
    global_segment(h)


def fastpath(h):
    r = h.vmlab("fastpath", check=False)
    first = r.out.splitlines()[0] if r.out else ""
    m = re.match(r"network fast path: (afxdp|sockmap|userspace) \(mode (\w+)\)", first)
    reasons = [l.strip() for l in r.out.splitlines()[1:] if "unavailable" in l]
    ok = r.code == 0 and m is not None
    # Every tier not selected must say why it was skipped.
    if ok and m.group(1) == "userspace":
        ok = len(reasons) >= 2
    h.ok("net.fastpath", ok, f"{first}; {' | '.join(reasons)[:200]}" if ok else r.text.strip()[-300:])


def main_lab(h, host: str):
    with h.lab("network-main") as lab:
        wcl = lab / "vmlab.wcl"
        wcl.write_text(wcl.read_text().replace("@HOST@", host))
        h.vmlab("up", cwd=lab, timeout=600)
        h.wait_ready(lab, "vm01", "vm02", "router", "vm03")

        ip = {m: ipv4(h, lab, m) for m in ("vm01", "vm02", "router", "vm03")}
        lan = ipaddress.ip_network("10.82.0.0/24")

        # DHCP: a dynamic lease inside the subnet, carrying router and DNS.
        conf = gsh(h, lab, "vm01", "ip route; cat /etc/resolv.conf").out
        h.ok(
            "net.dhcp",
            bool(ip["vm01"])
            and ipaddress.ip_address(ip["vm01"]) in lan
            and ip["vm01"] not in ("10.82.0.1", "10.82.0.254")
            and "default via 10.82.0.1" in conf
            and "nameserver 10.82.0.1" in conf,
            f"vm01 leased {ip['vm01']} with gateway and DNS 10.82.0.1",
        )

        # A static ip is a DHCP reservation: the guest keeps plain DHCP and
        # lands on it anyway, on both of the router's NICs.
        eth1 = gsh(h, lab, "router", "dhcpcd -1 -4 eth1 >/dev/null 2>&1; ip -4 -o addr show dev eth1").out
        h.ok(
            "net.static-ip",
            ip["router"] == "10.82.0.254" and "10.82.1.254/" in eth1 and "dynamic" in eth1,
            f"router eth0={ip['router']}, eth1 leased {'10.82.1.254' if '10.82.1.254/' in eth1 else eth1.strip()}",
        )

        # MTU: 1400 declared on "back"; the jumbo default on the NAT segment.
        mtu_back = gexec(h, lab, "vm03", "cat", "/sys/class/net/eth0/mtu").out.strip()
        mtu_lan = gexec(h, lab, "vm01", "cat", "/sys/class/net/eth0/mtu").out.strip()
        h.ok("net.mtu", mtu_back == "1400" and mtu_lan == "9000", f"back eth0 mtu={mtu_back}, lan (nat default) mtu={mtu_lan}")

        dns(h, lab, ip)
        egress_and_rules(h, lab, host)
        isolation(h, lab, ip)
        forward(h, lab)
        routes(h, lab, ip)


def dns(h, lab, ip):
    gw = "10.82.0.1"
    want = {
        "vm02.vmlab.internal": ip["vm02"],
        "vm02.e2e-network.vmlab.internal": ip["vm02"],
        "router.vmlab.internal": "10.82.0.254",
        "db.e2e.test": "10.82.0.200",
        "any.wild.e2e.test": "10.82.0.202",
        "lab-wide.e2e.test": "10.82.0.201",
    }
    got = {n: resolve(h, lab, "vm01", n, gw) for n in want}
    # The lab-wide record answers on the other segment too.
    got_back = resolve(h, lab, "vm03", "lab-wide.e2e.test", "10.82.1.1")
    bad = {n: got[n] for n in want if got[n] != want[n]}
    h.ok(
        "net.dns.records",
        not bad and got_back == "10.82.0.201",
        f"all {len(want)} names resolved from vm01; lab-wide record on back={got_back}" if not bad else f"wrong answers: {bad}",
    )

    nx = gexec(h, lab, "vm01", "nslookup", "x.blocked.e2e.test", gw)
    zero = resolve(h, lab, "vm01", "zero.e2e.test", gw)
    h.ok(
        "net.dns.sinkhole",
        nx.code != 0 and "NXDOMAIN" in nx.out and zero == "0.0.0.0",
        f"*.blocked -> NXDOMAIN ({nx.code}), zero.e2e.test -> {zero}",
    )

    table = h.vmlab("dns", cwd=lab, check=False)
    js = h.vmlab("dns", "--json", cwd=lab, check=False)

    def verb_ok():
        segs = {s["segment"]: s["zone"] for s in json.loads(js.out)["segments"]}
        lan = segs["lan"]
        names = {r["name"] for r in lan["records"]}
        assert {"db.e2e.test", "vm01.e2e-network.vmlab.internal"} <= names, names
        assert "*.wild.e2e.test" in json.dumps(lan["wildcards"]), lan["wildcards"]
        assert "*.blocked.e2e.test" in json.dumps(lan["sinkholes"]), lan["sinkholes"]
        for s in ('segment "lan"', 'segment "back"', "db.e2e.test", "sinkhole/nxdomain", "sinkhole/zero", "dynamic", "wildcard"):
            assert s in table.out, f"table lacks {s!r}"
        return True

    h.check("net.dns.verb", verb_ok, "table and --json list records, wildcards and sinkholes per segment")


def egress_and_rules(h, lab, host: str):
    # NAT: a host-side service the guest reaches only off-segment, and the
    # internet when the container itself has it.
    to_host = wget(h, lab, "vm01", f"http://{host}:9001/marker")
    container_online = h.run(["curl", "-s", "-o", "/dev/null", "-m", "10", "http://dl-cdn.alpinelinux.org/alpine/"], check=False).code == 0
    inet = wget(h, lab, "vm01", "http://dl-cdn.alpinelinux.org/alpine/", timeout=15)
    inet_ok = inet.code == 0 and "<html" in inet.out.lower()
    if container_online:
        h.ok(
            "net.nat",
            HOST_MARKER in to_host.out and inet_ok,
            f"guest fetched {host}:9001 and dl-cdn.alpinelinux.org through the NAT"
            if inet_ok
            else f"host service: {HOST_MARKER in to_host.out}; internet: {inet.text.strip()[-200:]}",
        )
    else:
        h.ok(
            "net.nat",
            HOST_MARKER in to_host.out,
            f"guest fetched {host}:9001 through the NAT; the container has no internet, so the public fetch was not attempted",
        )

    # block: tcp/9002 to the host is refused at the switch, 9001 is not, and
    # the server on 9002 itself answers the container.
    blocked = wget(h, lab, "vm01", f"http://{host}:9002/marker")
    served = HOST_MARKER in curl(h, f"http://127.0.0.1:9002/marker")
    h.ok(
        "net.l3.block",
        blocked.code != 0 and "refused" in blocked.text.lower() and served and HOST_MARKER in to_host.out,
        f"guest -> {host}:9002: {blocked.text.strip()[-120:]}; container reaches it: {served}",
    )

    # redirect: 192.0.2.10:80 (TEST-NET, nothing there) is rewritten to the
    # host's 9001, and the reply comes back as from 192.0.2.10.
    red = wget(h, lab, "vm01", "http://192.0.2.10/marker")
    h.ok("net.l3.redirect", red.code == 0 and HOST_MARKER in red.out, f"192.0.2.10:80 answered: {red.text.strip()[-120:]}")


def isolation(h, lab, ip):
    blocked, _ = pings(h, lab, "vm01", ip["vm02"])
    back, _ = pings(h, lab, "vm02", ip["vm01"])
    gw, _ = pings(h, lab, "vm02", "10.82.0.1")
    neighbour, _ = pings(h, lab, "vm01", "10.82.0.254")
    h.ok(
        "net.isolated",
        not blocked and not back and gw and neighbour,
        f"vm01<->vm02(isolated) ping: {blocked}/{back}; vm02->gateway: {gw}; vm01->router (not isolated): {neighbour}",
    )


def forward(h, lab):
    started = gsh(
        h,
        lab,
        "vm01",
        f"mkdir -p /tmp/www && echo {GUEST_MARKER} > /tmp/www/g && "
        "setsid nohup python3 -m http.server 8080 --directory /tmp/www >/dev/null 2>&1 </dev/null &",
    )
    served = gsh(h, lab, "vm01", "for i in $(seq 1 20); do wget -q -O - http://127.0.0.1:8080/g && exit 0; sleep 0.5; done; exit 1")
    if not served.code == 0:
        h.ok("net.forward", False, f"guest web server did not start: {started.text.strip()[-200:]}")
        return
    url = f"http://127.0.0.1:{FORWARD_PORT}/g"
    # The forward is skipped at `up` (no lease yet) and installed when the
    # VM becomes ready, with no second `up`.
    try:
        h.wait_until(lambda: GUEST_MARKER in curl(h, url), timeout=30, what=f"host :{FORWARD_PORT}")
        h.ok("net.forward", True, f"host :{FORWARD_PORT} answered from vm01:8080")
    except ScenarioFailed:
        events = h.vmlab("logs", "-o", "jsonl", "-n", "200", cwd=lab, check=False).out
        skips = [l for l in events.splitlines() if "forward.skipped" in l]
        h.ok("net.forward", False, f"host :{FORWARD_PORT} refused after the VM became ready; skips: {skips[-3:]}")


def routes(h, lab, ip):
    # "back" pushes 10.82.0.0/24 via the router as DHCP option 121.
    table = gexec(h, lab, "vm03", "ip", "route").out
    pushed = "10.82.0.0/24 via 10.82.1.254" in table
    # RFC 3442: with option 121 present the guest ignores option 3, so the
    # default route has to ride option 121 as well.
    default = "default via 10.82.1.1" in table
    # Forward on the router, give vm01 the return route, and cross segments.
    gexec(h, lab, "router", "sysctl", "-w", "net.ipv4.ip_forward=1")
    gexec(h, lab, "vm01", "ip", "route", "add", "10.82.1.0/24", "via", "10.82.0.254")
    crossed, out = pings(h, lab, "vm03", ip["vm01"])
    hop = "ttl=63" in out
    h.ok(
        "net.routes",
        pushed and default and crossed and hop,
        f"option-121 route in vm03: {pushed}; default route kept: {default}; "
        f"vm03 -> vm01 through the router: {crossed} (ttl=63: {hop}). "
        "`routes_to` validates, but the daemon's own inter-segment forwarding is documented as not yet wired"
        + ("" if default else f"; vm03 routes: {table.strip()!r}"),
    )


def global_segment(h):
    """Two labs on one supervisor declaring the same global segment."""
    with h.lab("network-global-a") as a, h.lab("network-global-b") as b:
        h.vmlab("up", cwd=a, timeout=600)
        h.vmlab("up", cwd=b, timeout=600)
        h.wait_ready(a, "ga")
        h.wait_ready(b, "gb")
        ga, gb = ipv4(h, a, "ga"), ipv4(h, b, "gb")
        shared = ipaddress.ip_network("10.82.9.0/24")
        leased = bool(ga and gb) and ga != gb and all(ipaddress.ip_address(x) in shared for x in (ga, gb))
        ab, _ = pings(h, a, "ga", gb)
        ba, _ = pings(h, b, "gb", ga)
        # PRD §9.2: the supervisor runs the shared segment's DNS so names
        # span labs.
        name = resolve(h, a, "ga", "gb.vmlab.internal", "10.82.9.1")
        full = resolve(h, a, "ga", "gb.e2e-network-global-b.vmlab.internal", "10.82.9.1")
        reach = leased and ab and ba
        named = gb and gb in (name, full)
        h.ok(
            "net.global",
            reach and named,
            f"ga={ga} gb={gb} (one supervisor DHCP), ping a->b {ab}, b->a {ba}; "
            f"gb.vmlab.internal -> {name}, gb.e2e-network-global-b.vmlab.internal -> {full}"
            + ("" if named else " — the global segment's DNS zone has no registrations (PRD §9.2 promises cross-lab names)"),
        )
