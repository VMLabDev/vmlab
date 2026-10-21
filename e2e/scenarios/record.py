"""Console recording (§11.1): `vmlab record`, `--record` on `up` and
`script`, `m.record` from wscript, and what the files hold — probed with
ffprobe, frames pulled with ffmpeg, text read with tesseract."""

import json
import pathlib
import subprocess
import time
import zlib

from harness import ScenarioFailed

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


def png_read(path: pathlib.Path):
    """Decode an 8-bit RGB/RGBA PNG with the standard library: (w, h, rows)."""
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
        for i in range(stride):
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
    return w, h, [[tuple(r[x * bpp:x * bpp + 3]) for x in range(w)] for r in rows]


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


def run(h):
    with h.lab("record-lab") as lab:
        rec = lab / "rec"
        rec.mkdir()

        # -- up --record: the video starts at the firmware's first frame.
        boot = rec / "boot.mp4"
        t0 = time.monotonic()
        h.vmlab("up", "--record", f"vm01={boot}", cwd=lab, timeout=600)
        h.wait_ready(lab, "vm01")
        boot_wall = time.monotonic() - t0
        status = h.vmlab("status", cwd=lab).text
        h.ok("record.status", "RECORDING" in status and str(boot) in status,
             "`vmlab status` names the live recording and its path")
        rs = h.vmlab("record", "status", cwd=lab).text
        h.ok("record.status", "vm01" in rs and str(boot) in rs, "`vmlab record status` lists it")

        def from_boot():
            h.vmlab("record", "stop", "vm01", cwd=lab)
            info = ffprobe(boot)
            assert info["codec"] == "h264" and (info["width"], info["height"]) == (1920, 1080), info
            assert info["duration"] >= boot_wall * 0.8, f"{info} vs {boot_wall:.1f}s of wall time"
            # The firmware draws within the first second; a video that
            # started late would open on the login prompt instead.
            w, hh, rows = png_read(frame_at(boot, 0.5, rec / "boot-early.png"))
            lit = sum(1 for row in rows[::8] for px in row[::8] if max(px) > 60)
            assert lit > 20, f"an early frame is black ({lit} lit samples): the recording missed the boot"
            return True

        h.check("record.up-from-boot", from_boot, "`up --record` captured the firmware screen; ffprobe reads 1920x1080 h264")

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
            assert click["button"] == "left" and click.get("x") is None, click
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
            b, a = before[cy][cx], after[cy][cx]
            amber = a[0] > b[0] + 40 and a[0] > 120 and a[2] < 80
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
        def restart():
            cont = rec / "restart.mp4"
            t0 = time.monotonic()
            h.vmlab("record", "start", "vm01", "-o", str(cont), cwd=lab)
            time.sleep(1)
            h.vmlab("vm", "restart", "vm01", cwd=lab, timeout=300)
            h.wait_ready(lab, "vm01")
            time.sleep(1)
            h.vmlab("record", "stop", "vm01", cwd=lab)
            wall = time.monotonic() - t0
            info = ffprobe(cont)
            assert abs(info["duration"] - wall) < 3.0, f"duration {info['duration']:.1f}s vs wall {wall:.1f}s across the restart"
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
