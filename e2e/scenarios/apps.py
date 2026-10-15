"""Shared apps (PRD §20) over X11: an X11-only host session, which Xvfb is
here, makes `vmlab app run` pick X11 by itself, and the guest program's window
appears on that server. Waypipe needs a Wayland compositor and RDP a Windows
guest, neither of which this container has.
"""

import os
import pathlib
import signal
import subprocess
import time

from harness import ScenarioFailed

LAB = "apps-x11"
M = "app01"
DISPLAY = ":99"
TITLE = "vmlab-e2e-app"


def host_env() -> dict[str, str]:
    """This process's environment as an X11-only desktop session."""
    env = {k: v for k, v in os.environ.items() if k not in ("WAYLAND_DISPLAY", "XAUTHORITY")}
    env["DISPLAY"] = DISPLAY
    return env


def has_window(h, title: str) -> bool:
    r = h.run(["xwininfo", "-root", "-tree", "-display", DISPLAY], check=False)
    return f'"{title}"' in r.out


def guest_has(h, lab, pattern: str) -> bool:
    r = h.vmlab("exec", M, "--", "pgrep", "-f", pattern, cwd=lab, check=False)
    return r.code == 0


def run(h):
    h.skip("app.waypipe", "no Wayland compositor in the e2e container")
    h.skip("app.rdp", "no Windows guest in the e2e suite")

    socket = pathlib.Path(f"/tmp/.X11-unix/X{DISPLAY[1:]}")
    xvfb = h.background(["Xvfb", DISPLAY, "-screen", "0", "1024x768x24", "-nolisten", "tcp"])
    try:
        h.wait_until(socket.exists, timeout=30, what="Xvfb to listen")
        with h.lab(LAB) as lab:
            h.vmlab("up", cwd=lab, timeout=900)
            h.wait_ready(lab, M)
            h.vmlab("cp", str(lab / "xclient.py"), f"{M}:/tmp/xclient.py", cwd=lab)

            # -- app.x11: the window appears on the host X server ------------
            argv = ["vmlab", "app", "run", M, "--", "python3", "/tmp/xclient.py", TITLE]
            h.log.write(f"\n[{h.scenario}] $ {' '.join(argv)} &  (DISPLAY={DISPLAY})\n")
            app = subprocess.Popen(argv, cwd=lab, env=host_env(), stdout=h.log,
                                   stderr=subprocess.STDOUT, text=True)
            try:
                shown = h.check("app.x11", lambda: h.wait_until(
                    lambda: has_window(h, TITLE) or app.poll() is not None,
                    timeout=120, what="the guest window on Xvfb") and has_window(h, TITLE),
                    f"`{TITLE}` on {DISPLAY}, no --via")

                # -- app.interrupt: Ctrl-C ends the session and the program --
                def interrupt():
                    if not shown:
                        raise ScenarioFailed("the session never showed its window")
                    assert guest_has(h, lab, "xclient.py"), "the program is not running in the guest"
                    app.send_signal(signal.SIGINT)
                    code = app.wait(timeout=60)
                    assert code == 130, f"app run exited {code} on SIGINT, not 130"
                    h.wait_until(lambda: not guest_has(h, lab, "xclient.py"), timeout=30,
                                 what="the guest program to be killed")
                    h.wait_until(lambda: not has_window(h, TITLE), timeout=30,
                                 what="the window to go")
                    return True

                h.check("app.interrupt", interrupt, "exit 130, program and window gone")
            finally:
                if app.poll() is None:
                    app.kill()
                    app.wait()

            # -- app.exit-code: the program's code is app run's --------------
            r = h.run(["vmlab", "app", "run", M, "--", "python3", "/tmp/xclient.py", TITLE, "3"],
                      cwd=lab, env=host_env(), check=False, timeout=120)
            h.ok("app.exit-code", r.code == 3, f"exit={r.code}")
    finally:
        xvfb.send_signal(signal.SIGTERM)
        try:
            xvfb.wait(timeout=10)
        except subprocess.TimeoutExpired:
            xvfb.kill()
