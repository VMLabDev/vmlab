"""Console recording (§11.1): `vmlab record`, `--record` on `up` and
`script`, `m.record` from wscript, and what the files hold — probed with
ffprobe, frames pulled with ffmpeg, text read with tesseract."""

import collections
import contextlib
import json
import pathlib
import queue
import re
import subprocess
import threading
import time
import zlib

from harness import RESULTS, ScenarioFailed

LAB = "e2e-record"


def ffprobe(path: pathlib.Path) -> dict:
    """Codec, size and duration of the first video stream."""
    out = subprocess.run(
        [
            "ffprobe", "-v", "error", "-select_streams", "v:0",
            "-show_entries", "stream=codec_name,width,height:format=duration",
            "-of", "json", str(path),
        ],
        capture_output=True, text=True, check=True,
    ).stdout
    v = json.loads(out)
    s = v["streams"][0]
    return {
        "codec": s["codec_name"],
        "width": int(s["width"]),
        "height": int(s["height"]),
        "duration": float(v["format"]["duration"]),
    }


def frame_at(video: pathlib.Path, at: float, out: pathlib.Path) -> pathlib.Path:
    """The frame at `at` seconds, as a PNG."""
    subprocess.run(
        ["ffmpeg", "-v", "error", "-y", "-ss", f"{max(at, 0.0):.3f}", "-i", str(video),
         "-frames:v", "1", str(out)],
        check=True, capture_output=True,
    )
    return out


def png_read(path: pathlib.Path, step: int = 1):
    """Decode an 8-bit RGB/RGBA PNG with the standard library: (w, h, rows).

    With `step`, rows holds only every step-th row and pixel."""
    data = path.read_bytes()
    assert data[:8] == b"\x89PNG\r\n\x1a\n", f"{path} is not a PNG"
    pos, idat, w, h, ctype = 8, b"", 0, 0, 0
    while pos < len(data):
        n = int.from_bytes(data[pos:pos + 4], "big")
        kind = data[pos + 4:pos + 8]
        body = data[pos + 8:pos + 8 + n]
        if kind == b"IHDR":
            w = int.from_bytes(body[0:4], "big")
            h = int.from_bytes(body[4:8], "big")
            ctype = body[9]
        elif kind == b"IDAT":
            idat += body
        pos += 12 + n
    bpp = {2: 3, 6: 4}[ctype]
    raw = zlib.decompress(idat)
    stride = w * bpp
    rows, prev = [], bytearray(stride)
    for y in range(h):
        f = raw[y * (stride + 1)]
        line = bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        # An unfiltered row is already the pixels. early_frames asks ffmpeg
        # for those, as the per-byte loop costs about a second a frame.
        for i in range(stride if f else 0):
            a = line[i - bpp] if i >= bpp else 0
            b = prev[i]
            c = prev[i - bpp] if i >= bpp else 0
            if f == 1:
                line[i] = (line[i] + a) & 255
            elif f == 2:
                line[i] = (line[i] + b) & 255
            elif f == 3:
                line[i] = (line[i] + (a + b) // 2) & 255
            elif f == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pred = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                line[i] = (line[i] + pred) & 255
        rows.append(bytes(line))
        prev = line
    return w, h, [[tuple(r[x * bpp:x * bpp + 3]) for x in range(0, w, step)] for r in rows[::step]]


def ocr(png: pathlib.Path) -> str:
    return subprocess.run(
        ["tesseract", str(png), "stdout", "--psm", "6"],
        capture_output=True, text=True,
    ).stdout


def placement(fb_w: int, fb_h: int, out_w: int = 1920, out_h: int = 1080):
    """Where a framebuffer pixel lands in the letterboxed output."""
    scale = min(out_w / fb_w, out_h / fb_h)
    pw, ph = round(fb_w * scale), round(fb_h * scale)
    return scale, (out_w - pw) // 2, (out_h - ph) // 2


# Firmware text, or the vmlab logo vmlab's own OVMF draws at boot
# (guest/build-ovmf.sh puts guest/logo/Logo.bmp in place of edk2's).
# The text markers are (kind, pattern) and match anywhere in the upper-cased
# OCR text. The logo must be the whole text of the frame: the guest console
# prints " * Starting vmlab-agent" (src/template/bootstrap/install.sh), and a
# console frame must never pass for the logo. tesseract reads the logo as
# "vmlab" or "vmliab" (VML-36), so the pattern allows that.
FIRMWARE_MARKERS = (
    ("BdsDxe", re.compile(r"BDSDXE")),
    ("SeaBIOS", re.compile(r"SEABIOS")),
    ("iPXE", re.compile(r"IPXE")),
)
LOGO = re.compile(r"\W*VM[LI1|!]{1,2}AB\W*")
# QEMU's own text while the guest has not set a display mode yet.
NO_DISPLAY = re.compile(r"DISPLAY\s+OUTPUT\s+IS\s+NOT\s+ACTIVE")
GRUB = re.compile(r"GNU\s+GRUB")
# A recording from power-on opens on firmware, or on QEMU's no-display text
# within START_BY seconds. That rule, not the window size, proves "from boot".
# A firmware first frame has no time limit: vmlab-hv's native recorder leaves
# out its placeholder frames and starts at the guest's first painted frame,
# so OVMF's logo can come first at 0.53 s (VML-74).
# OVMF can draw and clear all of its output between two framebuffer reads,
# so a boot may record no firmware frame at all (VML-36): GRUB then counts.
# Under heavy host load OVMF painted its logo only at 11.5 s, so the search
# window is wide (FIRMWARE_WITHIN).
START_BY = 0.2
FIRMWARE_WITHIN = 30.0


def firmware_kind(upper: str):
    """The firmware marker in a frame's upper-cased OCR text, or None."""
    for kind, pattern in FIRMWARE_MARKERS:
        if pattern.search(upper):
            return kind
    if LOGO.fullmatch(upper.strip()):
        return "logo"
    return None


PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"
# One line of ffmpeg's showinfo filter per decoded frame, in output order.
SHOWINFO = re.compile(rb"\[Parsed_showinfo_\d+ @ [^]]*\] n:\s*(\d+)\s+pts:\s*\S+\s+pts_time:(\S+)")
# How long a piped frame may wait for its showinfo line.
SHOWINFO_WAIT = 60.0
_END = object()


def png_stream(stream):
    """Split a stream of PNG files written back to back (ffmpeg's
    image2pipe) into one bytes object per file."""
    def read(n):
        data = stream.read(n)
        if len(data) != n:
            raise ValueError(f"PNG stream cut short: wanted {n} bytes, got {len(data)}")
        return data

    while True:
        signature = stream.read(8)
        if not signature:
            return
        if signature != PNG_SIGNATURE:
            raise ValueError(f"not a PNG in the frame stream: {signature!r}")
        parts = [signature]
        while True:
            head = read(8)
            parts.append(head)
            parts.append(read(int.from_bytes(head[:4], "big") + 4))
            if head[4:8] == b"IEND":
                break
        yield b"".join(parts)


def _stop(proc, reader):
    """Stop ffmpeg if it still runs, reap it, and let the log reader end."""
    if proc.poll() is None:
        proc.kill()
    proc.wait()
    reader.join(timeout=10)
    proc.stdout.close()
    proc.stderr.close()


def _shown(times: queue.Queue, n: int, video: pathlib.Path):
    """The (frame number, pts_time) showinfo logged for piped frame n."""
    try:
        item = times.get(timeout=SHOWINFO_WAIT)
    except queue.Empty:
        raise RuntimeError(
            f"frame {n} of {video} was piped but ffmpeg logged no showinfo line for it "
            f"within {SHOWINFO_WAIT:g} s"
        ) from None
    if item is _END:
        raise RuntimeError(f"ffmpeg piped frame {n} of {video} but logged no time for it")
    return item


def _loop_failure(exc: BaseException, proc, reader, cmd, log):
    """What to raise when the frame loop fails. When ffmpeg itself failed
    (killed, or a non-zero exit) its status is the cause, and a cut stream
    is only its symptom. Otherwise the loop's own error stands, with
    ffmpeg's log attached. ffmpeg still running is stopped first: a
    desynced stream leaves it blocked on the pipe."""
    try:
        rc = proc.wait(timeout=2)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
        rc = None
    reader.join(timeout=10)
    if rc:
        return subprocess.CalledProcessError(rc, cmd, stderr="\n".join(log))
    exc.add_note("ffmpeg's last log lines:\n" + "\n".join(log))
    return exc


def early_frames(video: pathlib.Path, seconds: float, out: pathlib.Path):
    """Every distinct frame of the first `seconds`: (time, png), in order.

    One ffmpeg process decodes the window and pipes each frame as a PNG.
    Its showinfo filter logs each decoded frame's own timestamp, so a time
    always belongs to its frame, also for a variable frame rate and when
    a packet does not give one frame (packets before the first keyframe
    decode to nothing). ffmpeg counts the time from the start of the
    stream, as `-t` does. Frames are decoded only as far as the caller
    reads: when it stops (a match), ffmpeg is stopped.

    The showinfo frame number must equal the frame's index in the pipe;
    if it does not, the times and the frames are out of step and this
    raises. A failing ffmpeg raises CalledProcessError with its log."""
    out.mkdir(parents=True, exist_ok=True)
    for old in out.glob("f*.png"):
        old.unlink()
    cmd = [
        "ffmpeg", "-hide_banner", "-nostdin", "-nostats", "-loglevel", "info",
        "-i", str(video), "-t", f"{seconds:.3f}", "-vf", "showinfo",
        "-fps_mode", "passthrough", "-pix_fmt", "rgb24",
        "-f", "image2pipe", "-c:v", "png", "-pred", "none", "-",
    ]
    proc = subprocess.Popen(
        cmd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    times = queue.Queue()
    log = collections.deque(maxlen=20)

    def read_log():
        try:
            for line in proc.stderr:
                m = SHOWINFO.search(line)
                if m:
                    times.put((int(m.group(1)), m.group(2).decode()))
                else:
                    log.append(line.decode(errors="replace").rstrip())
        finally:
            times.put(_END)

    reader = threading.Thread(target=read_log, daemon=True)
    reader.start()
    try:
        try:
            previous = None
            for n, data in enumerate(png_stream(proc.stdout)):
                # showinfo runs before the PNG encoder, so frame n's line is
                # written before frame n reaches the pipe.
                shown_n, pts_time = _shown(times, n, video)
                if shown_n != n:
                    raise RuntimeError(
                        f"frame {n} of {video} is out of step with showinfo: "
                        f"ffmpeg logged frame {shown_n} for it"
                    )
                try:
                    at = float(pts_time)
                except ValueError:
                    raise RuntimeError(f"frame {n} of {video} has no timestamp ({pts_time})") from None
                digest = zlib.crc32(data)
                if digest == previous:
                    continue
                previous = digest
                png = out / f"f{n + 1:05d}.png"
                png.write_bytes(data)
                yield at, png
        except GeneratorExit:
            raise
        except BaseException as exc:
            failure = _loop_failure(exc, proc, reader, cmd, log)
            if failure is exc:
                raise
            raise failure from exc
        rc = proc.wait()
        reader.join(timeout=10)
        if rc != 0:
            raise subprocess.CalledProcessError(rc, cmd, stderr="\n".join(log))
    finally:
        _stop(proc, reader)


def firmware_frame(video: pathlib.Path, out: pathlib.Path, seconds: float = FIRMWARE_WITHIN):
    """Find the firmware's own output at the start of a recording.

    Every distinct frame is read, not a sample: OVMF's "BdsDxe: loading"
    line can be drawn and cleared again between two framebuffer reads, so a
    boot may never record it (VML-36). The vmlab logo OVMF shows before it
    is firmware output too and stays on screen for a fraction of a second.
    Decoding stops at the first match. A coarse sample first would miss
    those short frames, so a failing run still reads the whole window.

    Two rules. The first lit frame must be firmware, or QEMU's no-display
    text by START_BY, else the recording started late. Then a
    firmware marker, or GRUB when no firmware frame was recorded, must come
    within `seconds`.

    Returns (time, text, kind): kind is the marker that matched, one of
    BdsDxe, SeaBIOS, iPXE, logo, or no-display+GRUB.
    """
    seen = []
    lit_at = None
    with contextlib.closing(early_frames(video, seconds, out.with_suffix(".frames"))) as frames:
        for at, png in frames:
            _, _, rows = png_read(png, step=8)
            lit = sum(1 for row in rows for px in row if max(px) > 60)
            if lit <= 20:
                continue
            text = ocr(png)
            upper = text.upper()
            first = " | ".join(line for line in text.splitlines() if line.strip())[:100]
            seen.append(f"{at:.2f}s {first}")
            kind = firmware_kind(upper)
            if lit_at is None:
                lit_at = at
                if kind is None and (at > START_BY or not NO_DISPLAY.search(upper)):
                    raise AssertionError(
                        f"not recorded from power-on: the first lit frame, at {at:.2f} s, "
                        f"is not firmware or QEMU's no-display text by {START_BY:g} s: {first!r}"
                    )
            elif kind is None and GRUB.search(upper):
                kind = "no-display+GRUB"
            if kind is not None:
                out.write_bytes(png.read_bytes())
                return at, text, kind
    raise AssertionError(
        f"no firmware output in the first {seconds:g} s (first lit at {lit_at} s); "
        f"distinct lit frames read: {seen}"
    )


def run(h):
    with h.lab("record-lab") as lab:
        rec = lab / "rec"
        rec.mkdir()

        # -- up --record: the video starts at the firmware's first frame.
        boot = rec / "boot.mp4"
        # The container's no-display refusal does not require it to run.
        h.vmlab("up", "vm01", "--record", f"vm01={boot}", cwd=lab, timeout=600)
        h.wait_ready(lab, "vm01")
        status = h.vmlab("status", cwd=lab).text
        h.ok("record.status", "RECORDING" in status and str(boot) in status,
             "`vmlab status` names the live recording and its path")
        rs = h.vmlab("record", "status", cwd=lab).text
        h.ok("record.status", "vm01" in rs and str(boot) in rs, "`vmlab record status` lists it")

        def from_boot():
            h.vmlab("record", "stop", "vm01", cwd=lab)
            info = ffprobe(boot)
            assert info["codec"] == "h264" and (info["width"], info["height"]) == (1920, 1080), info
            # The whole boot, not a tail of it: an Alpine boot to the agent
            # handshake is well over ten seconds of video.
            assert info["duration"] >= 10.0, f"{info}: too short to hold the boot"
            # Firmware initialization time varies. Identify its text rather
            # than accepting any lit frame before a fixed subsecond deadline.
            try:
                at, text, kind = firmware_frame(boot, rec / "boot-early.png")
            except AssertionError:
                # Keep the recording outside the container for diagnosis.
                kept = RESULTS / f"record-up-from-boot-{time.strftime('%Y%m%dT%H%M%S')}.mp4"
                kept.write_bytes(boot.read_bytes())
                h.log.write(f"[record] kept the failing boot recording as {kept}\n")
                raise
            h.log.write(f"[record] firmware frame at {at:.1f}s ({kind}): {text.strip()[:200]}\n")
            return True

        h.check("record.up-from-boot", from_boot, "`up --record` started at power-on and reached firmware or GRUB; ffprobe reads 1920x1080 h264")

        # -- start/stop with a timeline, inputs sent by a script in between.
        shot = h.vmlab("vm", "screenshot", "vm01", str(rec / "fb.png"), cwd=lab)
        fb_w, fb_h, _ = png_read(rec / "fb.png")
        out = rec / "demo.mp4"
        t0 = time.monotonic()
        h.vmlab("record", "start", "vm01", "-o", str(out), "--timeline", cwd=lab)
        time.sleep(1.0)
        h.vmlab("script", "scripts/inputs.ws", cwd=lab, timeout=120)
        time.sleep(1.5)
        stopped = h.vmlab("record", "stop", "vm01", "--json", cwd=lab)
        wall = time.monotonic() - t0
        reply = json.loads(stopped.out)
        events = json.loads((rec / "demo.mp4.events.json").read_text())

        def probe():
            info = ffprobe(out)
            assert info["codec"] == "h264" and (info["width"], info["height"]) == (1920, 1080), info
            assert abs(info["duration"] - wall) < 2.5, f"duration {info['duration']:.1f}s vs wall {wall:.1f}s"
            assert abs(reply["duration_secs"] - info["duration"]) < 1.0, reply
            return True

        h.check("record.probe", probe, "h264, 1920x1080, duration within 2.5 s of wall time")

        def timeline():
            kinds = [e["kind"] for e in events["events"]]
            assert kinds == ["text", "move", "click"], kinds
            text, move, click = events["events"]
            assert text["text"] == "VMLAB RECORD PROBE", text
            assert (move["x"], move["y"]) == (300, 200), move
            assert click["button"] == "left" and (click["x"], click["y"]) == (300, 200), click
            ats = [e["at"] for e in events["events"]]
            assert ats == sorted(ats) and 0 < ats[0] and ats[-1] < events["duration"], ats
            assert events["fps"] == 30 and events["size"] == "1920x1080" and events["cuts"] == [], events
            return True

        h.check("record.timeline", timeline, "events.json lists text, move, click in video time")

        def typed_text():
            at = events["events"][0]["at"] + 2.0
            text = ocr(frame_at(out, at, rec / "typed.png"))
            assert "PROBE" in text.upper() or "RECORD" in text.upper(), f"OCR read {text.strip()[:200]!r}"
            return True

        h.check("record.ocr", typed_text, "OCR finds the typed text in a frame after typing")

        def ripple():
            click = events["events"][2]
            scale, ox, oy = placement(fb_w, fb_h)
            cx, cy = round(ox + 300 * scale), round(oy + 200 * scale)
            before = png_read(frame_at(out, click["at"] - 0.3, rec / "before.png"))[2]
            after = png_read(frame_at(out, click["at"] + 0.1, rec / "after.png"))[2]
            # The brightest red within a few pixels of the point, so a
            # one-pixel rounding of the placement does not decide it.
            near = [(dx, dy) for dx in range(-3, 4) for dy in range(-3, 4)]
            b = max(before[cy + dy][cx + dx] for dx, dy in near)
            a = max(after[cy + dy][cx + dx] for dx, dy in near)
            amber = a[0] > b[0] + 40 and a[0] > 110 and a[2] < 80
            assert amber, f"pixel at ({cx},{cy}) before {b} after {a}: no ripple"
            return True

        h.check("record.ripple", ripple, "amber ripple pixels at the click point in the next frame")

        # -- compress-idle: 20 s of an unchanging console becomes a short video.
        def compress():
            idle = rec / "idle.mp4"
            h.vmlab("record", "start", "vm01", "-o", str(idle), "--compress-idle=2", cwd=lab)
            time.sleep(20)
            h.vmlab("record", "stop", "vm01", cwd=lab)
            info = ffprobe(idle)
            assert info["duration"] < 6.0, f"20 s of idle became {info['duration']:.1f}s"
            return True

        h.check("record.compress-idle", compress, "a 20 s idle wait shrinks to a few seconds of video")

        # -- WebM.
        def webm():
            web = rec / "demo.webm"
            h.vmlab("record", "start", "vm01", "-o", str(web), "--size", "1280x720", "--fps", "15", cwd=lab)
            time.sleep(2)
            h.vmlab("record", "stop", "vm01", cwd=lab)
            info = ffprobe(web)
            assert info["codec"] == "vp9" and (info["width"], info["height"]) == (1280, 720), info
            return True

        h.check("record.webm", webm, "a .webm path yields VP9 at the requested size")

        # -- refusals: twice, a container, a GIF.
        def refusals():
            twice = rec / "twice.mp4"
            h.vmlab("record", "start", "vm01", "-o", str(twice), cwd=lab)
            try:
                again = h.vmlab("record", "start", "vm01", "-o", str(rec / "again.mp4"), cwd=lab, check=False)
                assert again.code != 0 and "already recording" in again.text and str(twice) in again.text, again.text
                web = h.vmlab("record", "start", "web", cwd=lab, check=False)
                assert web.code != 0 and "no display" in web.text, web.text
                gif = h.vmlab("record", "start", "vm01", "-o", str(rec / "x.gif"), cwd=lab, check=False)
                assert gif.code != 0 and ".gif" in gif.text and ".mp4" in gif.text, gif.text
            finally:
                h.vmlab("record", "stop", "vm01", cwd=lab)
            none = h.vmlab("record", "stop", "vm01", cwd=lab, check=False)
            assert none.code != 0 and "not recording" in none.text, none.text
            return True

        h.check("record.refusals", refusals, "second start is a conflict naming the file; a container has no display; .gif refused")

        # -- vm restart mid-recording: one continuous file.
        # The recorder's clock starts inside `record start` and ends inside
        # `record stop`, so the video is bracketed: no shorter than the time
        # between the two calls, no longer than the time across both. The
        # latency of the calls themselves grows with host load (VML-65), so
        # it bounds the check rather than eating a fixed tolerance.
        def restart():
            cont = rec / "restart.mp4"
            t0 = time.monotonic()
            h.vmlab("record", "start", "vm01", "-o", str(cont), cwd=lab)
            t1 = time.monotonic()
            time.sleep(1)
            h.vmlab("vm", "restart", "vm01", cwd=lab, timeout=300)
            h.wait_ready(lab, "vm01")
            time.sleep(1)
            t2 = time.monotonic()
            stopped = json.loads(h.vmlab("record", "stop", "vm01", "--json", cwd=lab).out)
            t3 = time.monotonic()
            duration = ffprobe(cont)["duration"]
            inner, outer = t2 - t1, t3 - t0
            h.log.write(
                f"[record] restart: video {duration:.2f}s, recorder {stopped['duration_secs']:.2f}s, "
                f"between the calls {inner:.2f}s, across them {outer:.2f}s "
                f"(start call {t1 - t0:.2f}s, stop call {t3 - t2:.2f}s)\n"
            )
            assert abs(duration - stopped["duration_secs"]) < 0.5, \
                f"file {duration:.2f}s but the recorder counted {stopped['duration_secs']:.2f}s"
            assert inner - 0.5 <= duration <= outer + 0.5, \
                f"duration {duration:.2f}s outside {inner:.2f}s..{outer:.2f}s across the restart"
            return True

        h.check("record.restart", restart, "a `vm restart` mid-recording yields one continuous file spanning the gap")

        # -- script --record finalises on a failing script.
        def script_record():
            out = rec / "script.mp4"
            r = h.vmlab("script", "scripts/failing.ws", "--record", f"vm01={out}", cwd=lab, check=False, timeout=120)
            assert r.code != 0, "the script was meant to fail"
            info = ffprobe(out)
            assert info["codec"] == "h264" and info["duration"] > 0, info
            assert not h.vmlab("record", "status", cwd=lab).text.count("vm01"), "nothing left recording"
            return True

        h.check("record.script", script_record, "`script --record` leaves a playable file after a failing script")

        # -- m.record from wscript.
        def wscript_record():
            r = h.vmlab("script", "scripts/record.ws", cwd=lab, timeout=120)
            stopped = [ln for ln in r.text.splitlines() if "e2e-stopped" in ln][0].split("e2e-stopped", 1)[1].strip()
            assert stopped.endswith("recordings/from-script.webm"), stopped
            info = ffprobe(pathlib.Path(stopped))
            assert info["codec"] == "vp9" and (info["width"], info["height"]) == (960, 540), info
            left = [ln for ln in r.text.splitlines() if "e2e-open" in ln][0].split("e2e-open", 1)[1].strip()
            assert pathlib.Path(left).exists(), f"the run did not finalise {left}"
            assert ffprobe(pathlib.Path(left))["codec"] == "h264"
            assert "no recordings" in h.vmlab("record", "status", cwd=lab).text
            return True

        h.check("record.wscript", wscript_record, "`m.record` records; `stop()` finalises; a handle left open is finalised with the run")
