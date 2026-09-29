"""Snapshots: a VM online and offline, the whole lab, list and delete, and a
container."""


LAB = "snapshots-main"
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
