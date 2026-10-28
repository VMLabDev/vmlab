"""Worktree labs (PRD §21): a main lab frozen at an offline snapshot, a
linked git worktree's lab cloned from it, and the isolation between them
(§21.4) — forwards on loopback, NAT refusing every host address, and global
segments that are the worktree's own."""

import json
import shutil
import subprocess

from harness import WORK, ScenarioFailed

from scenarios.network import curl, gsh, host_ip, resolve, start_host_http

LAB = "worktree-isolation"
MAIN_MARKER = "e2e-main-lab-marker"
HOST_PORT = 9003


def status(h, lab) -> dict:
    r = h.vmlab("status", "--json", cwd=lab, check=False)
    try:
        return json.loads(r.out)
    except json.JSONDecodeError:
        return {}


def git(h, cwd, *args: str):
    return h.run(["git", "-c", "user.name=e2e", "-c", "user.email=e2e@vmlab.test", *args], cwd=cwd)


def guest_get(h, lab, url: str):
    """`wget` from the guest: (fetched, what it said)."""
    r = gsh(h, lab, "web", f"wget -q -T 5 -O - {url} 2>&1")
    return r.code == 0, r.text.strip()[-200:]


def run(h):
    with h.lab(LAB) as main:
        wt = WORK / f"{LAB}-wt"
        try:
            scenario(h, main, wt)
        finally:
            if wt.exists():
                h.destroy(wt)
                subprocess.run(["git", "worktree", "remove", "--force", str(wt)], cwd=main, check=False)
                shutil.rmtree(wt, ignore_errors=True)


def scenario(h, main, wt):
    git(h, main, "init", "-q", "-b", "main")
    git(h, main, "add", "-A")
    git(h, main, "commit", "-q", "-m", "lab")

    # The main lab, provisioned once, snapshotted offline and frozen.
    h.vmlab("up", cwd=main, timeout=600)
    h.wait_ready(main, "web")
    h.vmlab("down", cwd=main, timeout=300)
    h.vmlab("snapshot", "create", "base", cwd=main, timeout=300)
    h.vmlab("worktree", "freeze", "base", cwd=main, timeout=600)
    h.vmlab("up", cwd=main, timeout=600)
    h.wait_ready(main, "web")
    gsh(
        h,
        main,
        "web",
        f"mkdir -p /tmp/www && echo {MAIN_MARKER} > /tmp/www/m && "
        "setsid nohup python3 -m http.server 8080 --directory /tmp/www >/dev/null 2>&1 </dev/null &",
    )

    # The worktree lab, cloned from the freeze.
    git(h, main, "worktree", "add", "-q", "-b", "wt", str(wt))
    h.vmlab("up", cwd=wt, timeout=600)
    h.wait_ready(wt, "web")

    main_st, wt_st = status(h, main), status(h, wt)
    main_iso, wt_iso = main_st.get("isolation") or {}, wt_st.get("isolation") or {}
    text = h.vmlab("status", cwd=wt, check=False).out
    h.ok(
        "worktree.isolation.status",
        main_iso.get("role") == "main"
        and wt_iso.get("role") == "worktree"
        and wt_iso.get("worktree") == wt.name
        and "worktree isolation" in text,
        f"main isolation={main_iso}; worktree isolation={wt_iso}",
    )

    # -- forwards bind loopback ------------------------------------------
    host = host_ip()
    forwards = [f for f in main_st.get("forwards", []) if f.get("machine") == "web"]
    port = forwards[0]["host_port"] if forwards else 0
    try:
        h.wait_until(
            lambda: MAIN_MARKER in curl(h, f"http://127.0.0.1:{port}/m"),
            timeout=60,
            what=f"the main lab's forward on 127.0.0.1:{port}",
        )
        on_loopback = True
    except ScenarioFailed:
        on_loopback = False
    on_lan = MAIN_MARKER in curl(h, f"http://{host}:{port}/m")
    h.ok(
        "worktree.isolation.forward",
        bool(port) and on_loopback and not on_lan and main_iso.get("forward_bind") == "127.0.0.1",
        f"main forward :{port}: 127.0.0.1 answered={on_loopback}, {host} answered={on_lan}",
    )

    # -- NAT refuses the host's addresses -----------------------------------
    servers, www = start_host_http([HOST_PORT])
    try:
        h.wait_until(
            lambda: curl(h, f"http://127.0.0.1:{HOST_PORT}/marker"),
            timeout=20,
            interval=0.5,
            what=f"the host-side server on :{HOST_PORT}",
        )
        # The main lab's forward through the host's own address: the path a
        # worktree guest would take to its clone in the main lab.
        to_main, said_main = guest_get(h, wt, f"http://{host}:{port}/m")
        to_host, said_host = guest_get(h, wt, f"http://{host}:{HOST_PORT}/marker")
        # A refused SYN is answered with RST at once, not left to time out.
        refused = all("refused" in s.lower() for s in (said_main, said_host))
        h.ok(
            "worktree.isolation.nat",
            not to_main and not to_host and refused,
            f"worktree guest -> {host}:{port} (main forward): {said_main!r}; "
            f"-> {host}:{HOST_PORT} (host server): {said_host!r}",
        )
    finally:
        for p in servers:
            p.terminate()
        shutil.rmtree(www, ignore_errors=True)

    # -- global segments are the worktree's own ------------------------------
    joined = {g["segment"]: g["joined"] for g in wt_iso.get("globals", [])}
    main_joined = {g["segment"]: g["joined"] for g in main_iso.get("globals", [])}
    gsh(h, wt, "web", "dhcpcd -1 -4 eth1 >/dev/null 2>&1; true")
    own = resolve(h, wt, "web", f"web.{wt_st.get('lab')}.vmlab.internal", "10.83.9.1")
    other = resolve(h, wt, "web", f"web.{main_st.get('lab')}.vmlab.internal", "10.83.9.1")
    h.ok(
        "worktree.isolation.global",
        joined.get("shared") == f"shared@{wt.name}"
        and main_joined.get("shared") == "shared"
        and own is not None
        and other is None,
        f"worktree joins {joined}, main joins {main_joined}; on the worktree's segment "
        f"its own web -> {own}, the main lab's web -> {other}",
    )

    h.vmlab("destroy", cwd=wt, timeout=300)

