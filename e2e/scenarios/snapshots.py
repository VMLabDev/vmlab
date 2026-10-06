"""Snapshots: a VM online and offline, the whole lab, list and delete, a
container, and a VM whose virtiofs share rides a virtiofsd too old to carry
its state through a snapshot."""

from harness import WORK

LAB = "snapshots-main"
OLD_LAB = "snapshots-old-virtiofsd"
# Ubuntu 24.04's virtiofsd, which has neither --migration-mode nor --readonly
# (installed by e2e/Dockerfile).
OLD_VIRTIOFSD = "/usr/local/lib/vmlab-e2e/virtiofsd-1.10.0"
MARK = "/root/e2e-mark"


def put(h, lab, machine, text, container=False):
    verb = ["container", "exec"] if container else ["exec"]
    h.vmlab(*verb, machine, "--", "/bin/sh", "-c", f"echo {text} > {MARK}; sync", cwd=lab)


def get(h, lab, machine, container=False) -> str:
    """The marker's content, once the machine answers again."""
    verb = ["container", "exec"] if container else ["exec"]

    def read():
        r = h.vmlab(*verb, machine, "--", "/bin/sh", "-c", f"cat {MARK} 2>/dev/null || echo absent",
                    cwd=lab, check=False)
        return r.out.strip() if r.code == 0 else None

    return h.wait_until(read, timeout=120, interval=2, what=f"{machine} to answer exec")


def snap(h, lab, *args, **kw):
    return h.vmlab("snapshot", *args, cwd=lab, timeout=300, **kw)


def state(h, lab, machine) -> str:
    line = h.machine_line(lab, machine)
    return line.split()[0] if line else ""


def run(h):
    with h.lab(LAB) as lab:
        h.vmlab("up", cwd=lab, timeout=900)
        h.wait_ready(lab, "vm01", "c01")

        # -- snapshot.vm.online ---------------------------------------------
        def online():
            put(h, lab, "vm01", "before")
            c = snap(h, lab, "create", "--vm", "vm01", "on1")
            put(h, lab, "vm01", "after")
            r = snap(h, lab, "restore", "--vm", "vm01", "on1")
            got = get(h, lab, "vm01")
            st = state(h, lab, "vm01")
            assert "created" in c.out and "restored" in r.out, c.text + r.text
            # No workspace on this machine, so no workspace-backup warning.
            assert "workspace backup" not in c.out + r.out, c.text + r.text
            assert got == "before" and st == "state=running", f"marker={got!r} {st}"
            return True

        h.check("snapshot.vm.online", online, "marker rewound to 'before', VM still running")

        # -- snapshot.container (online, on the idle container's scratch) ---
        def container():
            put(h, lab, "c01", "before", container=True)
            snap(h, lab, "create", "--vm", "c01", "con1")
            put(h, lab, "c01", "after", container=True)
            snap(h, lab, "restore", "--vm", "c01", "con1")
            got = get(h, lab, "c01", container=True)
            assert got == "before", f"marker={got!r}"
            listed = snap(h, lab, "list", "c01").out
            assert "con1" in listed and "online" in listed, listed
            return True

        h.check("snapshot.container", container, "container scratch rewound to 'before'; listed online")

        # -- snapshot.lab: every machine under one name ---------------------
        def lab_wide():
            put(h, lab, "vm01", "lab-before")
            put(h, lab, "c01", "lab-before", container=True)
            c = snap(h, lab, "create", "lab1")
            put(h, lab, "vm01", "lab-after")
            put(h, lab, "c01", "lab-after", container=True)
            r = snap(h, lab, "restore", "lab1")
            vm = get(h, lab, "vm01")
            ct = get(h, lab, "c01", container=True)
            assert vm == "lab-before" and ct == "lab-before", f"vm01={vm!r} c01={ct!r}"
            both = "lab1" in snap(h, lab, "list", "vm01").out and "lab1" in snap(h, lab, "list", "c01").out
            assert both, "lab1 not listed on both machines"
            return "created" in c.out and "restored" in r.out

        h.check("snapshot.lab", lab_wide, "both machines rewound to 'lab-before'")

        # -- snapshot.vm.offline --------------------------------------------
        def offline():
            put(h, lab, "vm01", "off-before")
            h.vmlab("vm", "stop", "vm01", cwd=lab, timeout=180)
            h.wait_until(lambda: state(h, lab, "vm01") == "state=stopped", timeout=120, what="vm01 stopped")
            snap(h, lab, "create", "--vm", "vm01", "off1")
            kind = [l for l in snap(h, lab, "list", "vm01").out.splitlines() if l.startswith("off1")]
            assert kind and "offline" in kind[0], f"list row {kind}"
            h.vmlab("vm", "start", "vm01", cwd=lab, timeout=300)
            h.wait_ready(lab, "vm01", timeout=180)
            put(h, lab, "vm01", "off-after")
            h.vmlab("vm", "stop", "vm01", cwd=lab, timeout=180)
            h.wait_until(lambda: state(h, lab, "vm01") == "state=stopped", timeout=120, what="vm01 stopped")
            snap(h, lab, "restore", "--vm", "vm01", "off1")
            st = state(h, lab, "vm01")
            assert st == "state=stopped", f"after offline restore {st}"
            h.vmlab("vm", "start", "vm01", cwd=lab, timeout=300)
            h.wait_ready(lab, "vm01", timeout=180)
            got = get(h, lab, "vm01")
            assert got == "off-before", f"marker={got!r}"
            return True

        h.check("snapshot.vm.offline", offline, "offline row, restore left it stopped, marker 'off-before' after boot")

        # -- snapshot.list-delete -------------------------------------------
        def list_delete():
            before = snap(h, lab, "list", "vm01").out
            names = {l.split()[0] for l in before.splitlines()[1:] if l.strip()}
            assert {"on1", "lab1", "off1"} <= names, before
            d = snap(h, lab, "delete", "vm01", "on1")
            after = snap(h, lab, "list", "vm01").out
            left = {l.split()[0] for l in after.splitlines()[1:] if l.strip()}
            assert "deleted" in d.out and "on1" not in left and {"lab1", "off1"} <= left, after
            gone = snap(h, lab, "restore", "--vm", "vm01", "on1", check=False)
            assert gone.code != 0, "restore of a deleted snapshot succeeded"
            return True

        h.check("snapshot.list-delete", list_delete, "on1 listed, deleted, then unrestorable")

    old_virtiofsd(h)


def old_virtiofsd(h):
    """virtiofsd 1.10.0 serves the share without the migration flags, online
    snapshots refuse naming 1.11.0, offline ones work, and a read-only
    virtiofs share fails validation naming 1.13.0."""
    old = {"VMLAB_VIRTIOFSD": OLD_VIRTIOFSD}
    # labd inherits the supervisor's environment, so the override needs a
    # supervisor of its own.
    h.vmlab("daemon", "stop", check=False)
    h.run(["vmlab", "daemon", "start"], env=old)
    try:
        with h.lab(OLD_LAB) as lab:
            h.vmlab("up", cwd=lab, timeout=600, env=old)
            h.wait_ready(lab, "vm01")

            def share():
                argv = h.run(["ps", "-eo", "args"]).out
                daemons = [l for l in argv.splitlines() if l.startswith(OLD_VIRTIOFSD)]
                assert daemons, f"no {OLD_VIRTIOFSD} running:\n{argv}"
                assert all("--migration-mode" not in d for d in daemons), daemons
                mounts = h.vmlab("exec", "vm01", "--", "cat", "/proc/mounts", cwd=lab).out
                assert any(" /mnt/vfs virtiofs " in l for l in mounts.splitlines()), mounts
                read = h.vmlab("exec", "vm01", "--", "cat", "/mnt/vfs/host.txt", cwd=lab).out.strip()
                assert read == "from-host-vfs", read
                return True

            h.check("share.virtiofs.old-virtiofsd", share,
                    "virtiofsd 1.10.0 ran with no --migration-mode and the guest read the host file over virtiofs")

            def snapshots():
                on = snap(h, lab, "create", "--vm", "vm01", "on1", check=False)
                assert on.code != 0 and "virtiofsd 1.11.0 or later" in on.text, on.text
                assert state(h, lab, "vm01") == "state=running", "a refused snapshot stopped the VM"
                h.vmlab("vm", "stop", "vm01", cwd=lab, timeout=180)
                h.wait_until(lambda: state(h, lab, "vm01") == "state=stopped", timeout=120, what="vm01 stopped")
                off = snap(h, lab, "create", "--vm", "vm01", "off1")
                assert "created" in off.out, off.text
                ro = WORK / "old-virtiofsd-readonly"
                ro.mkdir(exist_ok=True)
                (ro / "share").mkdir(exist_ok=True)
                (ro / "vmlab.wcl").write_text(
                    'import <vmlab.wcl>\n\nlab "ro" {\n  vm "vm01" {\n    template = "x86_64/e2e-alpine"\n'
                    '    share { host = "./share" guest = "/mnt/ro" transport = "virtiofs" readonly = true }\n'
                    '  }\n}\n')
                v = h.run(["vmlab", "validate"], cwd=ro, env=old, check=False)
                assert v.code != 0 and "virtiofsd 1.13.0" in v.text, v.text
                return True

            h.check("snapshot.virtiofs.old-virtiofsd", snapshots,
                    "online create refused naming virtiofsd 1.11.0 with the VM left running; offline create "
                    "taken; a read-only virtiofs share failed validate naming 1.13.0")
    finally:
        # The next scenario's first command starts a supervisor with the
        # default environment.
        h.vmlab("daemon", "stop", check=False)
