"""Scripting: `vmlab script`, provisions, every wscript handle a lab script
reaches for (identity, segment, vision, terminal, snapshots), the `on`
handlers bound to a lab's ordinary events, and every stream `vmlab logs`
reads."""

import http.server
import pathlib
import socket
import subprocess
import threading

from harness import WORK, ScenarioFailed

LAB = "e2e-scripting"
HTTP_PORT = 18686  # the host listener the guest reaches through the gateway
FWD_PORT = 18687  # the host port `Segment.forward` publishes
STATE = pathlib.Path.home() / ".local" / "state" / "vmlab"


def line(text: str, prefix: str) -> str:
    """The rest of the first output line starting with `prefix`, or ''."""
    for ln in text.splitlines():
        if prefix in ln:
            return ln.split(prefix, 1)[1].strip()
    return ""


def root(h, lab, *cmd: str) -> str:
    """Run a shell line in vm01 as the agent identity (vm01 declares a
    default login, so plain `exec` would be `dev`)."""
    return h.vmlab("exec", "--user", "root", "vm01", "--", "/bin/sh", "-c", " ".join(cmd), cwd=lab).out


def host_ip() -> str:
    """The container's own address on its default route: what the guest's
    NAT reaches the host by."""
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("192.0.2.1", 9))
        return s.getsockname()[0]
    finally:
        s.close()


class Hello(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"e2e-host-listener\n"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


def forwarded() -> str:
    with socket.create_connection(("127.0.0.1", FWD_PORT), timeout=5) as c:
        c.settimeout(5)
        return c.recv(100).decode(errors="replace")


def daemon_log() -> str:
    p = STATE / f"labd-{LAB}.log"
    return p.read_text(errors="replace") if p.exists() else ""


def run(h):
    with h.lab("scripting-lab") as lab:
        up = h.vmlab("up", cwd=lab, timeout=900)
        h.wait_ready(lab, "vm01")

        # -- provision: its streamed log line, and what it left in the guest.
        marker = root(h, lab, "cat /etc/e2e-provisioned")
        h.ok(
            "script.provision",
            "e2e-provision done on vm01" in up.text and "provisioned-by-setup" in marker,
            f"up streamed the provision's line; /etc/e2e-provisioned={marker.strip()!r}",
        )

        # -- vmlab script: a guest exec, streamed back.
        def script_run():
            r = h.vmlab("script", "scripts/hello.ws", cwd=lab)
            got = line(r.text, "e2e-script-out")
            assert got == "guest-42 from e2e", f"script printed {got!r}"
            return True

        h.check("script.run", script_run, "streamed `guest-42 from e2e`")

        # -- as_login / as_account: files land owned by that user.
        def identity():
            r = h.vmlab("script", "scripts/identity.ws", cwd=lab)
            who = (line(r.text, "e2e-login-whoami"), line(r.text, "e2e-account-whoami"))
            assert who == ("dev", "audit"), f"whoami under the handles: {who}"
            owners = root(
                h, lab,
                "stat -c '%U %n' /home/dev/from-login /home/dev/login-payload.txt"
                " /home/audit/from-account /home/audit/account-payload.txt",
            ).split()
            want = [
                "dev", "/home/dev/from-login", "dev", "/home/dev/login-payload.txt",
                "audit", "/home/audit/from-account", "audit", "/home/audit/account-payload.txt",
            ]
            assert owners == want, f"owners: {owners}"
            body = root(h, lab, "cat /home/dev/login-payload.txt")
            assert "payload-from-host" in body, f"copied payload: {body!r}"
            return True

        h.check("script.as_login", identity, "exec and copy_to as `dev` (login) and `audit` (account) own their files")

        # -- terminal: send_line / expect over a PTY.
        def terminal():
            r = h.vmlab("script", "scripts/terminal.ws", cwd=lab)
            got = r.text.split("e2e-term-saw", 1)[-1] if "e2e-term-saw" in r.text else ""
            assert "term-42-root" in got, f"expect returned {got.strip()[-200:]!r}"
            return True

        h.check("script.terminal", terminal, "expect matched `term-42-root`")

        # -- vision: put known text on the console, then read it from wscript.
        def vision():
            root(h, lab, "printf '\\033c\\n\\n  VMLAB VISION PROBE\\n\\n' > /dev/tty1")
            r = h.vmlab("script", "scripts/vision.ws", cwd=lab, timeout=300)
            shot = lab / "scripts" / "shots" / "vm01.png"
            assert shot.exists() and shot.read_bytes()[:8] == b"\x89PNG\r\n\x1a\n", f"no PNG at {shot}"
            waited = line(r.text, "e2e-wait-text")
            assert "VISION" in waited.upper(), f"wait_for_text matched {waited!r}"
            ocr = r.text.split("e2e-ocr-begin", 1)[-1].split("e2e-ocr-end", 1)[0]
            assert "PROBE" in ocr.upper(), f"ocr read {ocr.strip()[:200]!r}"
            return True

        h.check("script.vision-api", vision, "screenshot wrote a PNG; wait_for_text and ocr read the probe text")

        # -- segment API: dns_set, block/unblock, forward, route_to.
        def segment():
            ip = host_ip()
            srv = http.server.ThreadingHTTPServer(("0.0.0.0", HTTP_PORT), Hello)
            threading.Thread(target=srv.serve_forever, daemon=True).start()
            try:
                root(h, lab, f"echo {ip} > /etc/e2e-host")
                r = h.vmlab("script", "scripts/segment.ws", cwd=lab, timeout=300)
                t = r.text
            finally:
                srv.shutdown()
                srv.server_close()
            problems = []
            dns = t.split("e2e-dns-set", 1)[-1].split("e2e-dns-cleared", 1)[0]
            if "10.86.0.77" not in dns:
                problems.append(f"dns_set not resolved: {dns.strip()[:200]!r}")
            cleared = t.split("e2e-dns-cleared", 1)[-1].split("e2e-block-before", 1)[0]
            if "10.86.0.77" in cleared:
                problems.append(f"dns_clear left the record: {cleared.strip()[:200]!r}")
            before, during, after = (line(t, f"e2e-block-{k}") for k in ("before", "during", "after"))
            if "WGET-OK" not in before:
                problems.append(f"host listener unreachable before block: {before!r}")
            if "WGET-FAIL" not in during:
                problems.append(f"block did not stop the guest: {during!r}")
            if "WGET-OK" not in after:
                problems.append(f"unblock did not restore it: {after!r}")
            try:
                fw = h.wait_until(lambda: _try(forwarded), timeout=30, what="the script's forward to answer")
            except ScenarioFailed as e:
                fw = str(e)
            if "e2e-forwarded" not in fw:
                problems.append(f"forward 127.0.0.1:{FWD_PORT} answered {fw!r}")
            route = line(t, "e2e-route-to")
            # route_to is documented as not yet available from scripts: the
            # observable contract is its error, by name.
            if "not yet available" not in route:
                problems.append(f"route_to answered {route!r}")
            assert not problems, "; ".join(problems)
            return True

        h.check(
            "script.segment-api",
            segment,
            "dns_set resolved then cleared; block/unblock toggled the guest's reach to a host listener; "
            "forward served a guest port on the host; route_to gave its documented not-yet-available error",
        )

        # -- logs -f, started before the next events so it must pick them up.
        follow_out = WORK / "scripting-follow.jsonl"
        fh = open(follow_out, "w")
        follower = subprocess.Popen(
            ["vmlab", "logs", "-f", "-o", "jsonl", "-n", "1"], cwd=lab, stdout=fh, stderr=subprocess.STDOUT
        )

        # -- snapshots from wscript (also fires snapshot.created).
        def snapshots():
            r = h.vmlab("script", "scripts/snapshots.ws", cwd=lab, timeout=600)
            listed = line(r.text, "e2e-snap-listed")
            restored = line(r.text, "e2e-snap-restored")
            left = line(r.text, "e2e-snap-left")
            assert listed == "e2e-script-snap", f"snapshots() listed {listed!r}"
            assert restored == "captured", f"after restore the file read {restored!r}"
            assert left == "0", f"{left} snapshot(s) left after delete_snapshot"
            return True

        h.check("script.snapshot-api", snapshots, "snapshot, snapshots() listed it, restore rewound a file, delete_snapshot removed it")

        # -- vm.stopped: the last of the four bound events.
        h.vmlab("vm", "stop", "vm01", cwd=lab, timeout=300)

        def followed():
            got = h.wait_until(
                lambda: "vm.stopped" in follow_out.read_text() and follow_out.read_text(),
                timeout=60,
                what="`logs -f` to show vm.stopped",
            )
            return "snapshot.created" in got

        try:
            h.check("logs.follow", followed, "a running `vmlab logs -f` printed snapshot.created and vm.stopped as they happened")
        finally:
            follower.terminate()
            follower.wait(timeout=10)
            fh.close()

        def handlers():
            want = ["lab.up", "vm.ready vm=vm01", "snapshot.created vm=vm01", "vm.stopped vm=vm01"]

            def missing():
                log = daemon_log()
                return [w for w in want if f"e2e-handler {w}" not in log]

            try:
                h.wait_until(lambda: not missing(), timeout=60, what="every handler's line")
            except ScenarioFailed:
                raise AssertionError(f"no handler line for {missing()} in labd-{LAB}.log")
            return True

        h.check("events.handlers", handlers, f"handler lines for lab.up, vm.ready, snapshot.created, vm.stopped in labd-{LAB}.log")

        # -- every stream `vmlab logs` reads.
        def streams():
            problems = []
            ev = h.vmlab("logs", "-o", "jsonl", "-n", "50", cwd=lab, check=False).out
            if '"event":"lab.up"' not in ev.replace(" ", ""):
                problems.append("events: no lab.up in `vmlab logs`")
            vm = h.vmlab("logs", "vm01", "-n", "200", cwd=lab, check=False)
            sections = _sections(vm.out)
            serial = next((v for k, v in sections.items() if k.endswith("serial.log")), "")
            qemu = next((k for k in sections if k.endswith("qemu.log")), None)
            if not serial.strip():
                problems.append(f"serial: empty ({vm.text.strip()[:200]!r})")
            if qemu is None:
                problems.append("qemu: no qemu.log section")
            elif not sections[qemu].strip():
                problems.append("qemu: qemu.log is empty")
            con = h.vmlab("logs", f"{LAB}/web", "-n", "50", cwd=lab, check=False)
            if con.code != 0 or not con.out.strip():
                problems.append(f"console: `vmlab logs {LAB}/web` exited {con.code}: {con.text.strip()[:200]!r}")
            assert not problems, "; ".join(problems)
            return True

        h.check("logs.streams", streams, "events, serial, qemu and console streams each returned content")


def _try(fn):
    try:
        return fn()
    except OSError:
        return ""


def _sections(out: str) -> dict[str, str]:
    """Split `vmlab logs <vm>` output on its `==> path <==` headers."""
    sections: dict[str, str] = {}
    cur = None
    for ln in out.splitlines():
        if ln.startswith("==> ") and ln.endswith(" <=="):
            cur = ln[4:-4]
            sections[cur] = ""
        elif cur is not None:
            sections[cur] += ln + "\n"
    return sections
