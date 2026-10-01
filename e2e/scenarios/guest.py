"""Guest: the agent verbs, declared logins, and the screen — screenshot, OCR,
template matching, keyboard, mouse and the VNC console bridge."""

import json
import os
import pathlib
import pty
import re
import select
import signal
import socket
import struct
import subprocess
import time
import zlib

from harness import WORK, ScenarioFailed

VM = "g01"
OWNED = [
    "agent.exec", "agent.exec.timeout", "agent.shell", "agent.cp.push", "agent.cp.pull",
    "agent.tail", "agent.osinfo", "agent.stats", "agent.capabilities", "agent.clipboard",
    "agent.update", "agent.repair", "login.default", "login.user", "login.password", "vision.screenshot",
    "vision.ocr", "vision.find-image", "vision.sendkeys", "vision.mouse", "console.tcp",
]


# -- PNG, with the standard library only -----------------------------------------


def png_read(path) -> tuple[int, int, int, list[bytes]]:
    """Decode an 8-bit, non-interlaced grey/RGB/RGBA PNG into rows of raw
    pixel bytes: (width, height, bytes per pixel, rows)."""
    data = open(path, "rb").read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError(f"{path} is not a PNG")
    i, idat, w = 8, b"", 0
    while i < len(data):
        (n,) = struct.unpack(">I", data[i:i + 4])
        kind, body = data[i + 4:i + 8], data[i + 8:i + 8 + n]
        if kind == b"IHDR":
            w, hgt, depth, ctype, _, _, interlace = struct.unpack(">IIBBBBB", body)
            if depth != 8 or interlace:
                raise ValueError(f"unsupported PNG: depth {depth}, interlace {interlace}")
            bpp = {0: 1, 2: 3, 4: 2, 6: 4}[ctype]
        elif kind == b"IDAT":
            idat += body
        i += 12 + n
    raw = zlib.decompress(idat)
    stride = w * bpp
    rows, prev = [], bytearray(stride)
    for y in range(hgt):
        f = raw[y * (stride + 1)]
        line = bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        for x in range(stride):
            a = line[x - bpp] if x >= bpp else 0
            b = prev[x]
            c = prev[x - bpp] if x >= bpp else 0
            if f == 1:
                line[x] = (line[x] + a) & 0xFF
            elif f == 2:
                line[x] = (line[x] + b) & 0xFF
            elif f == 3:
                line[x] = (line[x] + (a + b) // 2) & 0xFF
            elif f == 4:
                pa, pb, pc = abs(b - c), abs(a - c), abs(a + b - 2 * c)
                pr = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                line[x] = (line[x] + pr) & 0xFF
        rows.append(bytes(line))
        prev = line
    return w, hgt, bpp, rows


def png_write_crop(src, dst, x: int, y: int, w: int, h: int) -> None:
    """Write the `w`x`h` region at (x, y) of `src` as an RGB PNG."""
    _, _, bpp, rows = png_read(src)
    out = bytearray()
    for row in rows[y:y + h]:
        px = row[x * bpp:(x + w) * bpp]
        if bpp != 3:
            px = b"".join(px[i:i + 3] if bpp >= 3 else px[i:i + 1] * 3 for i in range(0, len(px), bpp))
        out += b"\0" + px

    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))

    with open(dst, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)))
        f.write(chunk(b"IDAT", zlib.compress(bytes(out))))
        f.write(chunk(b"IEND", b""))


# -- local helpers ---------------------------------------------------------------


def pty_session(h, argv, steps, cwd, timeout=60) -> tuple[str, bool]:
    """Drive `argv` on a real pseudo-terminal: each step is (delay, bytes)
    written after the delay. Returns the transcript and whether the process
    exited on its own within `timeout` of the last step."""
    h.log.write(f"\n[{h.scenario}] $ (pty) {' '.join(argv)}\n")
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        os.execvp(argv[0], argv)
    out = bytearray()

    def pump(secs):
        end = time.monotonic() + secs
        while time.monotonic() < end:
            r, _, _ = select.select([fd], [], [], 0.1)
            if r:
                try:
                    chunk = os.read(fd, 4096)
                except OSError:
                    return True
                if not chunk:
                    return True
                out.extend(chunk)
            done, _ = os.waitpid(pid, os.WNOHANG)
            if done:
                return True
        return False

    exited = False
    for delay, data in steps:
        if pump(delay):
            exited = True
            break
        os.write(fd, data)
    if not exited:
        exited = pump(timeout)
    if not exited:
        os.kill(pid, signal.SIGKILL)
    try:
        os.waitpid(pid, 0)
    except ChildProcessError:
        pass
    os.close(fd)
    text = out.decode(errors="replace")
    h.log.write(text + f"\n[pty exited on its own: {exited}]\n")
    return text, exited


def spawn(h, argv, cwd) -> subprocess.Popen:
    """A background command whose stdout the scenario reads."""
    h.log.write(f"\n[{h.scenario}] $ {' '.join(argv)} &\n")
    return subprocess.Popen(argv, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)


def stop(proc: subprocess.Popen, h) -> str:
    if proc.poll() is None:
        proc.send_signal(signal.SIGINT)
        try:
            proc.wait(10)
        except subprocess.TimeoutExpired:
            proc.kill()
    out = proc.stdout.read() if proc.stdout else ""
    h.log.write(out + f"[exit {proc.returncode}]\n")
    return out


def root(h, lab, script: str, **kw):
    return h.vmlab("exec", "--user", "root", VM, "--", "sh", "-c", script, cwd=lab, **kw)


def input_events(h, lab, action) -> dict[str, list[tuple[int, int, int]]]:
    """Capture every `/dev/input/event*` in the guest while `action` runs, and
    return the (type, code, value) of each event read, per device. Which
    device QEMU routes pointer input to varies from boot to boot, so all of
    them are watched."""
    cap = spawn(h, ["vmlab", "exec", "--user", "root", VM, "--", "sh", "-c",
                    "rm -f /tmp/ev-*.bin; for d in /dev/input/event*; do "
                    "timeout 8 cat $d > /tmp/ev-${d##*/}.bin & done; wait; true"], lab)
    time.sleep(2.5)
    action()
    cap.wait(30)
    stop(cap, h)
    dump = root(h, lab, "for f in /tmp/ev-*.bin; do echo \"== $f\"; od -An -tx1 -v $f; done").out
    out: dict[str, list[tuple[int, int, int]]] = {}
    for part in dump.split("== ")[1:]:
        name, _, hexdump = part.partition("\n")
        raw = bytes.fromhex("".join(hexdump.split()))
        evs = []
        for i in range(0, len(raw) - 23, 24):
            # struct input_event on x86_64: timeval (16), u16 type, u16 code, s32 value.
            evs.append(struct.unpack("<HHi", raw[i + 16:i + 24]))
        out[name.strip().removeprefix("/tmp/ev-").removesuffix(".bin")] = evs
    return out


# -- the scenario ----------------------------------------------------------------


def run(h):
    try:
        _run(h)
    finally:
        for f in OWNED:
            if f not in h.results:
                h.ok(f, False, "not reached: the scenario stopped before this step")


def _run(h):
    with h.lab("guest-lab") as lab:
        up = h.vmlab("up", cwd=lab, timeout=600, check=False)
        if up.code != 0:
            raise ScenarioFailed(f"`vmlab up` failed: {up.text.strip()[-400:]}")
        h.wait_ready(lab, VM)
        h.wait_until(lambda: root(h, lab, "id dev && id ops && id temp", check=False).code == 0,
                     timeout=120, what="the provision's accounts")

        # -- exec -----------------------------------------------------------------
        r = root(h, lab, "echo to-out; echo to-err >&2; exit 3", check=False)
        h.ok("agent.exec", r.code == 3 and r.out.strip() == "to-out" and "to-err" in r.err,
             f"exit {r.code}, stdout {r.out.strip()!r}, stderr {r.err.strip()!r}")

        t = time.monotonic()
        r = h.vmlab("exec", "--timeout", "3", VM, "--", "sleep", "60", cwd=lab, check=False, timeout=60)
        took = time.monotonic() - t
        h.ok("agent.exec.timeout", r.code != 0 and took < 20 and "timed out" in r.text,
             f"exit {r.code} after {took:.1f}s: {r.text.strip()[-100:]}")

        # -- logins ---------------------------------------------------------------
        who = lambda *a: h.vmlab("exec", *a, VM, "--", "id", "-un", cwd=lab, check=False)  # noqa: E731
        d = who()
        h.ok("login.default", d.code == 0 and d.out.strip() == "dev", f"`exec -- id -un` -> {d.out.strip()!r}")
        o, rt = who("--user", "ops"), who("--user", "root")
        h.ok("login.user", o.out.strip() == "ops" and rt.out.strip() == "root",
             f"--user ops -> {o.out.strip()!r}, --user root -> {rt.out.strip()!r}")
        p = who("--user", "temp", "--password", "anything")
        ghost = who("--user", "ghost", "--password", "anything")
        h.ok("login.password", p.code == 0 and p.out.strip() == "temp" and ghost.code != 0 and "ghost" in ghost.text,
             f"undeclared temp -> {p.out.strip()!r}; missing account refused by name: {ghost.text.strip()[-90:]!r}")

        # -- files ----------------------------------------------------------------
        src = WORK / "guest-push"
        (src / "sub").mkdir(parents=True, exist_ok=True)
        (src / "one.txt").write_text("pushed one\n")
        (src / "sub" / "two.sh").write_text("#!/bin/sh\necho pushed two\n")
        os.chmod(src / "sub" / "two.sh", 0o755)
        f1 = h.vmlab("cp", str(src / "one.txt"), f"{VM}:/tmp/push/deep/one.txt", cwd=lab, check=False)
        f2 = h.vmlab("cp", str(src), f"{VM}:/tmp/tree", cwd=lab, check=False)
        seen = root(h, lab, "cat /tmp/push/deep/one.txt; /tmp/tree/sub/two.sh", check=False)
        h.ok("agent.cp.push", f1.code == 0 and f2.code == 0 and seen.out == "pushed one\npushed two\n",
             f"{f1.out.strip()}; {f2.out.strip()}; guest ran the pushed script with its mode kept")

        root(h, lab, "head -c 200000 /dev/urandom > /tmp/pull.bin; md5sum /tmp/pull.bin > /tmp/pull.md5")
        want = root(h, lab, "cat /tmp/pull.md5").out.split()[0]
        dst = WORK / "guest-pull"
        dst.mkdir(exist_ok=True)
        pl = h.vmlab("cp", f"{VM}:/tmp/pull.bin", str(dst), cwd=lab, check=False)
        got = h.run(["md5sum", str(dst / "pull.bin")], check=False).out.split()
        h.ok("agent.cp.pull", pl.code == 0 and got and got[0] == want,
             f"{pl.out.strip()}; md5 {got[0] if got else '-'} == guest {want}")

        # -- tail -----------------------------------------------------------------
        root(h, lab, "echo tail-line-one > /tmp/e2e-tail.log")
        tail = spawn(h, ["vmlab", "tail", VM, "/tmp/e2e-tail.log"], lab)
        time.sleep(3)
        root(h, lab, "echo tail-line-two >> /tmp/e2e-tail.log")
        time.sleep(3)
        out = stop(tail, h)
        h.ok("agent.tail", "tail-line-one" in out and "tail-line-two" in out,
             "followed the file and printed the appended line")

        # -- reports --------------------------------------------------------------
        oi = h.vmlab("osinfo", VM, cwd=lab, check=False)
        try:
            info = json.loads(oi.out)
        except ValueError:
            info = {}
        h.ok("agent.osinfo", info.get("id") == "alpine" and info.get("arch") == "x86_64" and info.get("hostname"),
             oi.out.strip())

        stj = h.vmlab("machine", "stats", "--json", VM, cwd=lab, check=False)
        stp = h.vmlab("machine", "stats", VM, cwd=lab, check=False)
        try:
            stats = json.loads(stj.out)
        except ValueError:
            stats = {}
        h.ok("agent.stats",
             stats.get("mem_total", 0) > 256 << 20 and any(d.get("mount") == "/" for d in stats.get("disks", []))
             and "memory" in stp.out,
             f"mem_total={stats.get('mem_total')} disks={[d.get('mount') for d in stats.get('disks', [])]}")

        cap = h.vmlab("machine", "capabilities", VM, cwd=lab, check=False).out
        agent_line = next((l for l in cap.splitlines() if l.startswith("agent")), "")
        features = {f.strip() for f in agent_line.split(None, 1)[-1].split(",")}
        h.ok("agent.capabilities",
             {"terminal", "exec", "fileops", "tail", "metrics"} <= features and re.search(r"display\s+yes", cap),
             agent_line)

        # -- clipboard ------------------------------------------------------------
        if "clipboard" in features:
            s = h.vmlab("clipboard", "set", VM, "e2e-clip-42", cwd=lab, check=False)
            g = h.vmlab("clipboard", "get", VM, cwd=lab, check=False)
            h.ok("agent.clipboard", s.code == 0 and g.out == "e2e-clip-42", f"round trip -> {g.out!r}")
        else:
            # A headless Linux agent has no display server to hold a clipboard
            # and never advertises it, so both verbs must refuse by name
            # (`unsupported`, exit 6). The real round trip needs a guest with a
            # display server the agent can reach; this template has none.
            s = h.vmlab("clipboard", "set", VM, "e2e-clip-42", cwd=lab, check=False)
            g = h.vmlab("clipboard", "get", VM, cwd=lab, check=False, timeout=60)
            refused = all(r.code == 6 and "no clipboard" in r.text for r in (s, g))
            h.ok("agent.clipboard", refused,
                 f"agent lacks `clipboard`, both verbs refuse by name: set exit {s.code} "
                 f"({s.text.strip()[:80]!r}), get exit {g.code} ({g.text.strip()[:80]!r})")

        # -- shell ----------------------------------------------------------------
        text, _ = pty_session(h, ["vmlab", "shell", VM], [(4, b"echo SHELL_$((6*7)) $(id -un)\r"), (3, b"\x1d")], lab, timeout=15)
        detached = "SHELL_42 dev" in text
        text2, exited = pty_session(h, ["vmlab", "shell", "--user", "root", VM], [(4, b"exit\r")], lab, timeout=20)
        h.ok("agent.shell", detached and exited,
             f"command ran as the default login and Ctrl-] detached: {detached}; "
             f"`exit` in the guest shell ended the client: {exited}"
             + ("" if exited else " — vmlab bug: the guest shell is gone (no process left) but the "
                "daemon never closes the session socket, so the client stays attached"))

        # -- screen ---------------------------------------------------------------
        shot = WORK / "g01.png"
        sc = h.vmlab("vm", "screenshot", VM, str(shot), cwd=lab, check=False)
        try:
            w, hgt, _, _ = png_read(shot)
        except (OSError, ValueError, KeyError) as e:
            w, hgt = 0, 0
            h.log.write(f"screenshot unreadable: {e}\n")
        h.ok("vision.screenshot", sc.code == 0 and w >= 640 and hgt >= 400, f"{shot.name}: {w}x{hgt} PNG")

        full = h.wait_until(
            lambda: (lambda t: t if "login" in t else None)(h.vmlab("vm", "ocr", VM, cwd=lab, check=False).out),
            timeout=60, interval=3, what="the login prompt on the console")
        # The banner's first text line only, located from the screenshot: the
        # login line below it is outside the region.
        try:
            lit = [y for y, row in enumerate(png_read(shot)[3]) if any(row)]
        except (OSError, ValueError, KeyError):
            lit = []
        top = lit[0] if lit else 0
        bottom = next((a for a, b in zip(lit, lit[1:]) if b != a + 1), top + 16) if lit else 20
        ry = max(0, top - 8)
        rh = bottom - ry + 3
        region = h.vmlab("vm", "ocr", "--region", "0", str(ry), str(w or 1280), str(rh), VM, cwd=lab, check=False)
        h.ok("vision.ocr", "login" in full and region.code == 0 and "Alpine" in region.out
             and "login" not in region.out,
             f"screen: {full.strip().splitlines()[-1][:40]!r}; region 0,{ry},{w},{rh}: {region.out.strip()[:50]!r}")

        # A tight crop of the top of the screen, found where it was cut from.
        ref = WORK / "g01-crop.png"
        png_write_crop(shot, ref, 0, 0, 320, 48)
        fi = h.vmlab("vm", "find-image", VM, str(ref), cwd=lab, check=False)
        m = re.search(r"x=(\d+) y=(\d+) .*score=([\d.]+)", fi.out)
        h.ok("vision.find-image", fi.code == 0 and m and m.group(1) == "0" and m.group(2) == "0"
             and float(m.group(3)) >= 0.9, fi.out.strip() or fi.text.strip())

        # Keys typed at the login prompt appear on screen and reach the guest keyboard.
        def typed():
            for k in "xyzzy":
                h.vmlab("vm", "sendkeys", VM, k, cwd=lab)
        kev = [e for evs in input_events(h, lab, typed).values() for e in evs]
        screen = h.vmlab("vm", "ocr", VM, cwd=lab, check=False).out
        h.vmlab("vm", "sendkeys", VM, "ctrl-u", cwd=lab, check=False)
        presses = sum(1 for t_, _, v in kev if t_ == 1 and v == 1)
        h.ok("vision.sendkeys", "xyzzy" in screen and presses >= 5,
             f"OCR after typing: {next((l for l in screen.splitlines() if 'xyzzy' in l), '-')!r}; "
             f"{presses} key presses on the guest keyboard")

        def mouse():
            h.vmlab("vm", "mouse-move", VM, "100", "100", cwd=lab)
            h.vmlab("vm", "click", VM, "200", "150", cwd=lab)
            h.vmlab("vm", "click", "--button", "right", VM, cwd=lab)
            h.vmlab("vm", "drag", VM, "10", "10", "300", "300", cwd=lab)
        per = input_events(h, lab, mouse)
        mev = [e for evs in per.values() for e in evs]
        devs = sorted(d for d, evs in per.items() if any(t_ in (2, 3) for t_, _, _ in evs))
        motion = sum(1 for t_, _, _ in mev if t_ in (2, 3))
        left = sum(1 for t_, c, v in mev if t_ == 1 and c == 0x110 and v == 1)
        right = sum(1 for t_, c, v in mev if t_ == 1 and c == 0x111 and v == 1)
        h.ok("vision.mouse", motion > 0 and left >= 2 and right >= 1,
             f"guest {'+'.join(devs) or 'no device'}: {motion} motion events, {left} left presses (click + drag), {right} right")

        # -- console --------------------------------------------------------------
        con = spawn(h, ["vmlab", "console", "--tcp", VM], lab)
        port, banner = None, b""
        deadline = time.monotonic() + 15
        line = ""
        while time.monotonic() < deadline and port is None:
            line = con.stdout.readline()
            mm = re.search(r"127\.0\.0\.1:(\d+)", line)
            if mm:
                port = int(mm.group(1))
        if port:
            with socket.create_connection(("127.0.0.1", port), timeout=5) as s:
                banner = s.recv(12)
        stop(con, h)
        h.ok("console.tcp", banner.startswith(b"RFB ") and con.returncode is not None,
             f"{line.strip()}: server said {banner!r}; bridge ended on Ctrl-C")

        # -- update: `up` refreshes a stale agent (it replaces the agent too) -----
        # The image carries a second agent stamped `agent=e2e-stale`. Put it in
        # the guest and restart the service, so the guest really runs a stale
        # agent; `up` must then push the shipped one, and the agent answering
        # afterwards must carry the shipped stamp, not just answer.
        stale_bin = "/usr/share/vmlab/e2e/stale-agent/linux-x86_64/vmlab-agent"
        shipped = pathlib.Path("/usr/share/vmlab/guest/agent/linux-x86_64/VERSION").read_text().strip()
        h.vmlab("cp", stale_bin, f"{VM}:/usr/local/lib/vmlab/vmlab-agent.stale", cwd=lab)
        h.vmlab("exec", VM, "--user", "root", "--", "/bin/sh", "-c",
                "chmod 755 /usr/local/lib/vmlab/vmlab-agent.stale && "
                "mv -f /usr/local/lib/vmlab/vmlab-agent.stale /usr/local/lib/vmlab/vmlab-agent",
                cwd=lab)
        # The next boot starts the stale agent; this `up` must replace it.
        h.vmlab("down", VM, cwd=lab, timeout=180)
        upd = h.vmlab("up", cwd=lab, timeout=600, check=False)
        h.wait_ready(lab, VM)
        line = h.machine_line(lab, VM)
        after = h.vmlab("exec", VM, "--", "id", "-un", cwd=lab, check=False)
        said = [ln.strip() for ln in upd.text.splitlines() if ln.strip().startswith(("agent:", "warning: agent:"))]
        h.ok("agent.update",
             upd.code == 0
             and any(s.startswith(f'agent: updated "{VM}" (agent=e2e-stale') and s.endswith(f"→ {shipped})") for s in said)
             and "diverged=yes" in line and after.out.strip() == "dev",
             f"guest ran the stale agent; up exit {upd.code}: {' | '.join(said) or 'no agent line'}; "
             f"status -v: diverged=yes {'present' if 'diverged=yes' in line else 'absent'}; "
             f"exec after update -> {after.out.strip()!r}")

        # -- repair (last: it replaces the agent under everything above) ----------
        rp = h.vmlab("machine", "repair-agent", VM, cwd=lab, check=False, timeout=180)
        line = h.machine_line(lab, VM)
        after = h.vmlab("exec", VM, "--", "id", "-un", cwd=lab, check=False)
        h.ok("agent.repair",
             rp.code == 0 and "now diverged" in rp.out and "diverged=yes" in line and after.out.strip() == "dev",
             f"{rp.out.strip().splitlines()[0] if rp.out.strip() else rp.text.strip()[-100:]}; status -v: diverged=yes "
             f"{'present' if 'diverged=yes' in line else 'absent'}; exec after repair -> {after.out.strip()!r}")
