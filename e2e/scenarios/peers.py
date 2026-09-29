"""Cross-host peering: two supervisors in one container, each under its own
XDG directories and trunk_port, bridge a global segment over a
PSK-authenticated TCP trunk, and a guest on each side reaches the other."""

import contextlib
import json
import re
import shutil
import socket
import threading

from harness import E2E, WORK, ScenarioFailed

PSK = "e2e-peers"
PORTS = {"a": 13957, "b": 13958}
# Both supervisors serve DHCP on the bridged segment, so leases can race;
# each guest also carries a fixed secondary address (as examples/peer-a does).
FIXED = {"a": "10.83.0.10", "b": "10.83.0.20"}
TEMPLATE = "x86_64/e2e-alpine@1.0.0"


def side_env(side: str) -> dict[str, str]:
    home = WORK / f"peers-home-{side}"
    env = {
        "XDG_RUNTIME_DIR": str(home / "run"),
        "XDG_CONFIG_HOME": str(home / "config"),
        "XDG_STATE_HOME": str(home / "state"),
    }
    if side == "b":
        # Side a shares the default template store; side b has its own and
        # gets the template by export/import.
        env["XDG_DATA_HOME"] = str(home / "data")
    return env


def prepare_home(side: str) -> None:
    home = WORK / f"peers-home-{side}"
    shutil.rmtree(home, ignore_errors=True)
    (home / "run").mkdir(parents=True, mode=0o700)
    (home / "config" / "vmlab").mkdir(parents=True)
    (home / "state").mkdir(parents=True)
    (home / "config" / "vmlab" / "config.wcl").write_text(
        f'import <vmlab-host.wcl>\n\nhost {{\n  psk        = "{PSK}"\n  trunk_port = {PORTS[side]}\n}}\n'
    )


class Subscriber:
    """Follow a supervisor's aggregate event stream (the wire's `subscribe`),
    since `segment.peer.up` is a host-scoped event no log file carries."""

    def __init__(self, sock_path: str):
        self.events: list[dict] = []
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(sock_path)
        self.sock.sendall(b'{"type":"req","id":1,"cmd":"subscribe","args":null}\n')
        self.thread = threading.Thread(target=self._read, daemon=True)
        self.thread.start()

    def _read(self):
        buf = b""
        try:
            while chunk := self.sock.recv(65536):
                buf += chunk
                while b"\n" in buf:
                    line, buf = buf.split(b"\n", 1)
                    with contextlib.suppress(ValueError):
                        msg = json.loads(line)
                        if msg.get("type") == "event":
                            self.events.append(msg)
        except OSError:
            pass

    def names(self) -> list[str]:
        return [e.get("event", "") for e in self.events]

    def close(self):
        with contextlib.suppress(OSError):
            self.sock.shutdown(socket.SHUT_RDWR)
        self.sock.close()


def vm(h, side: str, *args: str, **kw):
    return h.vmlab(*args, cwd=WORK / f"peers-{side}", env=side_env(side), **kw)


def ready(h, side: str, machine: str) -> bool:
    lines = vm(h, side, "status", "-v", check=False).out.splitlines()
    for i, line in enumerate(lines):
        cols = line.split()
        if cols and cols[0] == machine and i + 1 < len(lines):
            return "ready=yes" in lines[i + 1]
    return False


def run(h):
    sides = ("a", "b")
    archive = WORK / "peers-template.tar.zst"
    subs: dict[str, Subscriber] = {}
    try:
        for s in sides:
            prepare_home(s)
            lab = WORK / f"peers-{s}"
            shutil.rmtree(lab, ignore_errors=True)
            shutil.copytree(E2E / "labs" / f"peers-{s}", lab)
        archive.unlink(missing_ok=True)
        h.vmlab("template", "export", TEMPLATE, str(archive), timeout=600)
        vm(h, "b", "template", "import", str(archive), timeout=600)
        archive.unlink(missing_ok=True)

        # Start both supervisors and follow their events before anything dials.
        for s in sides:
            vm(h, s, "daemon", "start")
            subs[s] = Subscriber(side_env(s)["XDG_RUNTIME_DIR"] + "/vmlab/vmlabd.sock")

        # b listens; a declares `connect` and dials b's trunk_port.
        vm(h, "b", "up", timeout=600)
        vm(h, "a", "up", timeout=600)
        for s in sides:
            h.wait_until(lambda s=s: ready(h, s, f"p{s}"), timeout=300, interval=2, what=f"p{s} to report ready")

        for s in sides:
            vm(h, s, "exec", f"p{s}", "--", "ip", "addr", "add", f"{FIXED[s]}/24", "dev", "eth0", check=False)

        peer_col = {s: vm(h, s, "status", check=False).out for s in sides}
        ping = vm(h, "a", "exec", "pa", "--", "ping", "-c", "3", "-W", "2", FIXED["b"], check=False, timeout=30)
        pinged = ping.code == 0 and " 0% packet loss" in ping.out

        # TCP across the trunk: a web server on b, fetched from a.
        vm(
            h, "b", "exec", "pb", "--", "/bin/sh", "-c",
            "mkdir -p /tmp/www && echo e2e-peer-b > /tmp/www/m && "
            "setsid nohup python3 -m http.server 8000 --directory /tmp/www >/dev/null 2>&1 </dev/null &",
            check=False,
        )
        fetch = vm(
            h, "a", "exec", "pa", "--", "/bin/sh", "-c",
            f"for i in $(seq 1 20); do wget -q -T 3 -O - http://{FIXED['b']}:8000/m && exit 0; sleep 0.5; done; exit 1",
            check=False, timeout=90,
        )
        fetched = fetch.code == 0 and "e2e-peer-b" in fetch.out

        with contextlib.suppress(ScenarioFailed):
            h.wait_until(lambda: all("segment.peer.up" in subs[s].names() for s in sides), timeout=10,
                         interval=0.5, what="segment.peer.up on both supervisors")
        up_events = {s: [e["data"].get("data", e["data"]) for e in subs[s].events if e.get("event") == "segment.peer.up"] for s in sides}
        logs = {
            s: (WORK / f"peers-home-{s}" / "state" / "vmlab" / "vmlabd.log").read_text(errors="replace")
            for s in sides
        }
        logged = all(re.search(r'cross-host trunk \S+ for "e2e-wan" up', logs[s]) for s in sides)
        h.ok(
            "net.peer",
            pinged and fetched and all(up_events[s] for s in sides) and logged
            and "(up)" in peer_col["a"],
            f"pa -> pb ping {pinged}, TCP {fetched}; segment.peer.up a={up_events['a']} b={up_events['b']}; "
            f"vmlabd.log trunk-up lines on both: {bool(logged)}",
        )
    except ScenarioFailed as e:
        h.ok("net.peer", False, str(e))
    finally:
        for s in subs.values():
            s.close()
        for s in sides:
            if (WORK / f"peers-{s}").exists() and not h.keep:
                vm(h, s, "destroy", check=False, timeout=300)
            if not h.keep:
                vm(h, s, "daemon", "stop", check=False)
