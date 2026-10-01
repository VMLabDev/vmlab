"""Containers and shares: an OCI image as a micro-VM with env, volumes, a
published port and a healthcheck; an idle container; the container verbs; and
a VM carrying a virtiofs, an SMB and a read-only share."""

import json
import time

from harness import ScenarioFailed

LAB = "containers-main"
# The lab sits this deep so its `.vmlab/smb` is past 120 characters: smbd binds
# unix sockets (108-byte paths) in some of its directories, and a lab at a deep
# path once killed every SMB share at start (`messaging_dgm_ref failed: File
# name too long`). share.smb passing here is that regression's check.
DEEP = "a-lab-directory-deep-enough-to-overflow-a-unix-socket-path-" + "x" * 50


def sh(h, lab, machine, script, check=True, container=False, timeout=120):
    """Run a shell snippet in a machine; the result's stdout."""
    verb = ["container", "exec"] if container else ["exec"]
    return h.vmlab(*verb, machine, "--timeout", str(timeout), "--", "/bin/sh", "-c", script,
                   cwd=lab, check=check, timeout=timeout + 30)


def events(h, lab) -> list[dict]:
    r = h.vmlab("logs", "-o", "jsonl", "-n", "2000", cwd=lab, check=False)
    out = []
    for line in r.out.splitlines():
        try:
            out.append(json.loads(line))
        except ValueError:
            pass
    return out


def has_event(h, lab, name: str, machine: str) -> bool:
    return any(e.get("event") == name and machine in json.dumps(e.get("data", e)) for e in events(h, lab))


def http_get(h, port: int) -> str:
    r = h.run(["curl", "-fsS", "--max-time", "5", f"http://127.0.0.1:{port}/"], check=False)
    return r.out if r.code == 0 else ""


def run(h):
    with h.lab(LAB, under=DEEP) as lab:
        assert len(str(lab / ".vmlab" / "smb")) > 120, lab
        h.vmlab("up", cwd=lab, timeout=900)
        h.wait_ready(lab, "web", "idle", "vm01")

        # -- container.up: the workload runs, from a pulled image -----------
        line = h.machine_line(lab, "web")
        release = sh(h, lab, "web", "cat /etc/alpine-release", check=False, container=True)
        h.ok("container.up", "state=running" in line and "digest=sha256:" in line and release.out.startswith("3.22"),
             f"{line.strip()} release={release.out.strip()}")

        # -- container.exec: output and exit code ---------------------------
        r = sh(h, lab, "web", "echo exec-ok; exit 7", check=False, container=True)
        h.ok("container.exec", r.code == 7 and "exec-ok" in r.out, f"exit={r.code} out={r.out.strip()!r}")

        # -- container.env: the workload saw the variable -------------------
        try:
            page = h.wait_until(lambda: http_get(h, 18480), timeout=60, what="the published port")
        except ScenarioFailed:
            page = ""
        h.ok("container.env", "hello-e2e-value-42" in page, f"page={page.strip()!r}")

        # -- container.port: published port answers from the host -----------
        h.ok("container.port", page.strip() == "hello-e2e-value-42", f"curl 127.0.0.1:18480 -> {page.strip()!r}")

        # -- container.logs: the entrypoint's stdout in the console log -----
        logs = h.vmlab("container", "logs", "web", "-n", "500", cwd=lab, check=False)
        h.ok("container.logs", "E2E_VAR=e2e-value-42" in logs.out, f"exit={logs.code}, {len(logs.out)} bytes")

        # -- container.volume (host path): the workload's write on the host -
        from_c = lab / "hostvol" / "from-container.txt"
        host_seen = from_c.exists() and from_c.read_text().strip() == "written-by-web"
        in_guest = sh(h, lab, "web", "cat /hostvol/host.txt", check=False, container=True).out.strip()
        named = list(lab.glob(".vmlab/**/e2e-named/boots"))
        boots_before = named[0].read_text().count("boot") if named else 0
        vol_detail = f"host-bind: guest wrote={host_seen}, guest read={in_guest!r}; named={named[:1]} boots={boots_before}"

        # -- container.healthcheck: healthy, then failing -------------------
        healthy = "health=ok" in h.machine_line(lab, "web")
        sh(h, lab, "web", "touch /tmp/unhealthy", check=False, container=True)
        try:
            h.wait_until(lambda: has_event(h, lab, "container.unhealthy", "web"), timeout=60,
                         what="container.unhealthy")
            unhealthy = True
        except Exception:
            unhealthy = False
        failing = "health=failing" in h.machine_line(lab, "web")
        h.ok("container.healthcheck", healthy and unhealthy and failing,
             f"healthy first={healthy}, container.unhealthy event={unhealthy}, status failing={failing}")

        # -- container.power: ip, stop, start, restart ----------------------
        power = []
        ip = h.vmlab("container", "ip", "web", cwd=lab, check=False).out.strip()
        power.append(f"ip={ip}")
        ok = ip.startswith("10.84.0.")
        stopped = h.vmlab("container", "stop", "web", cwd=lab, check=False, timeout=180)
        st = h.machine_line(lab, "web")
        ok &= stopped.code == 0 and "state=stopped" in st
        power.append(f"stop->{st.split()[0] if st else '?'}")
        started = h.vmlab("container", "start", "web", cwd=lab, check=False, timeout=300)
        try:
            h.wait_ready(lab, "web", timeout=180)
            ok &= started.code == 0
            power.append("start->ready")
        except Exception as e:
            ok = False
            power.append(f"start failed: {e}")
        restarted = h.vmlab("container", "restart", "web", cwd=lab, check=False, timeout=300)
        try:
            h.wait_ready(lab, "web", timeout=180)
            ok &= restarted.code == 0
            power.append("restart->ready")
        except Exception as e:
            ok = False
            power.append(f"restart failed: {e}")

        # The named volume outlived two restarts of its container.
        named = list(lab.glob(".vmlab/**/e2e-named/boots"))
        boots_after = named[0].read_text().count("boot") if named else 0
        h.ok("container.volume",
             host_seen and in_guest == "from-host-vol" and boots_before >= 1 and boots_after >= boots_before + 2,
             f"{vol_detail}; boots after stop/start+restart={boots_after}")

        # -- container.idle: up with no workload, available for exec --------
        idle_line = h.machine_line(lab, "idle")
        ps = sh(h, lab, "idle", "ps -o comm", check=False, container=True)
        procs = [l.strip() for l in ps.out.splitlines()]
        h.ok("container.idle", "ready=yes" in idle_line and "nc" not in procs and ps.code == 0,
             f"{idle_line.strip()}; no workload among {len(procs) - 1} processes")

        # -- container.shell under a pty ------------------------------------
        # The session should end, and the CLI return, when the guest shell exits.
        r = h.pty("vmlab container shell idle", "echo SHELL_$((6*7))\nexit\n", cwd=lab, timeout=30)
        h.ok("container.shell", "SHELL_42" in r.out and r.code != 124,
             f"shell answered={'SHELL_42' in r.out}; " +
             ("CLI did not return within 30s of the guest shell exiting" if r.code == 124 else f"CLI exit={r.code}"))

        # -- shares on the VM -----------------------------------------------
        # Mount steps run after readiness, virtiofs first, in declaration order.
        def mounts_now():
            m = sh(h, lab, "vm01", "cat /proc/mounts", check=False).out
            return m if " /srv cifs " in m else None
        try:
            mounts = h.wait_until(mounts_now, timeout=120, interval=3, what="the SMB mount of /srv")
        except ScenarioFailed:
            mounts = sh(h, lab, "vm01", "cat /proc/mounts", check=False).out
        share_checks(h, lab, mounts)

        # container.destroy closes power.
        d = h.vmlab("container", "destroy", "idle", cwd=lab, check=False, timeout=180)
        gone = "state=running" not in h.machine_line(lab, "idle")
        ok &= d.code == 0 and "destroyed" in d.out and gone
        power.append(f"destroy: {d.out.strip()!r}")
        h.ok("container.power", ok, "; ".join(power))


def share_checks(h, lab, mounts: str):
    def probe(path, host_dir, expect, fstype):
        """Mounted with `fstype`, the host file visible, and a guest write
        landing on the host."""
        line = next((l for l in mounts.splitlines() if f" {path} " in l), "")
        read = sh(h, lab, "vm01", f"cat {path}/host.txt", check=False).out.strip()
        w = sh(h, lab, "vm01", f"echo from-guest > {path}/guest.txt", check=False)
        landed = lab / host_dir / "guest.txt"
        # Give an SMB write a moment to close.
        for _ in range(10):
            if landed.exists():
                break
            time.sleep(0.5)
        wrote = landed.exists() and landed.read_text().strip() == "from-guest"
        ok = f" {fstype} " in line and read == expect and w.code == 0 and wrote
        return ok, f"{path}: mount={line.split(' ')[2] if line else None} read={read!r} write exit={w.code} on host={wrote}"

    ok, detail = probe("/mnt/vfs", "share-vfs", "from-host-vfs", "virtiofs")
    h.ok("share.virtiofs", ok, detail)

    ok_srv, d_srv = probe("/srv", "share-smb-srv", "from-host-smb-srv", "cifs")
    ok_mnt, d_mnt = probe("/mnt/smb", "share-smb", "from-host-smb", "cifs")
    h.ok("share.smb", ok_srv and ok_mnt,
         f"lab path {len(str(lab))} chars; {d_srv}; {d_mnt}")

    line = next((l for l in mounts.splitlines() if " /mnt/ro " in l), "")
    read = sh(h, lab, "vm01", "cat /mnt/ro/host.txt", check=False).out.strip()
    w = sh(h, lab, "vm01", "echo nope > /mnt/ro/guest.txt", check=False)
    leaked = (lab / "share-ro" / "guest.txt").exists()
    h.ok("share.readonly", " ro," in line and read == "from-host-ro" and w.code != 0 and not leaked,
         f"mount={line!r} read={read!r} write exit={w.code} ({w.err.strip()[-80:]!r}) leaked={leaked}")
