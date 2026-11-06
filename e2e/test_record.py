"""Boot recordings must start at power-on and contain firmware, including
when it paints late."""

import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from scenarios import record

NO_DISPLAY = "Display output is not active."
GRUB = "GNU GRUB version 2.12"


class FirmwareFrameTests(unittest.TestCase):
    def scan(self, frames):
        """Run firmware_frame over distinct frames given as (time, text);
        a frame with text is lit, one without is dark. Like ffmpeg -t, only
        frames before the window's end are read."""
        def early(video, seconds, out):
            for at, text in frames:
                if at < seconds:
                    yield at, Path(f"{at}:{text}")

        def pixels(path, step=1):
            text = str(path).split(":", 1)[1]
            level = 255 if text else 0
            return 80, 80, [[(level, level, level)] * (80 // step)] * (80 // step)

        def ocr(path):
            return str(path).split(":", 1)[1]

        with patch.object(record, "early_frames", side_effect=early), \
                patch.object(record, "png_read", side_effect=pixels), \
                patch.object(record, "ocr", side_effect=ocr), \
                patch.object(Path, "write_bytes"), patch.object(Path, "read_bytes"):
            return record.firmware_frame(Path("boot.mp4"), Path("boot.png"))

    def test_firmware_painting_late_passes(self):
        frames = [(0.0, NO_DISPLAY), (6.2, 'BdsDxe: starting Boot0002 "UEFI Misc Device"')]
        at, text, kind = self.scan(frames)
        self.assertEqual((at, kind), (6.2, "BdsDxe"))
        self.assertIn("BdsDxe", text)

    def test_firmware_painting_under_heavy_load_passes(self):
        # VML-36 vml36-runs2 run 1: load 93 on 32 cores, logo at 11.47 s.
        frames = [(0.0, NO_DISPLAY), (3.77, NO_DISPLAY + " "), (11.47, "vmliab")]
        self.assertEqual(self.scan(frames)[::2], (11.47, "logo"))

    def test_bios_firmware_text_is_recognized(self):
        for text in ("SeaBIOS version 1.17.0-vmlab", "iPXE 2.0.0"):
            with self.subTest(text=text):
                at, _, kind = self.scan([(0.0, NO_DISPLAY), (1.2, text)])
                self.assertEqual((at, kind), (1.2, text.split()[0]))

    def test_firmware_as_the_first_lit_frame_passes(self):
        self.assertEqual(self.scan([(0.0, ""), (0.1, "SeaBIOS version 1.17.0")])[2], "SeaBIOS")

    def test_the_vmlab_logo_is_firmware_when_bdsdxe_was_never_recorded(self):
        # VML-36's first failing recordings: the logo, then GRUB, no BdsDxe.
        frames = [(0.0, NO_DISPLAY), (1.67, "vmlab"), (2.07, GRUB)]
        self.assertEqual(self.scan(frames)[::2], (1.67, "logo"))

    def test_an_ocr_misread_of_the_logo_still_counts(self):
        # VML-36 runs 5 and 7: tesseract read the logo as "vmliab".
        for text in ("vmliab", "VMLAB", "vm1ab"):
            with self.subTest(text=text):
                at, _, kind = self.scan([(0.0, NO_DISPLAY), (2.6, text)])
                self.assertEqual((at, kind), (2.6, "logo"))

    def test_grub_counts_after_a_no_display_start_when_no_firmware_was_recorded(self):
        # VML-36 run 4: no logo and no BdsDxe frame, QEMU's text, then GRUB.
        frames = [(0.0, ""), (0.03, NO_DISPLAY), (0.13, NO_DISPLAY + " "), (2.17, GRUB)]
        at, _, kind = self.scan(frames)
        self.assertEqual((at, kind), (2.17, "no-display+GRUB"))

    def test_firmware_wins_over_grub_and_names_its_kind(self):
        frames = [(0.0, NO_DISPLAY), (1.9, "BdsDxe: loading Boot0002"), (2.5, GRUB)]
        self.assertEqual(self.scan(frames)[2], "BdsDxe")

    def test_a_late_start_fails_whatever_follows(self):
        # The first lit frame is not firmware, or is QEMU's text after
        # START_BY: the recording missed power-on.
        for frames in (
            [(0.0, GRUB), (1.0, "vmlab")],
            [(0.1, "Linux version 6.12\nalpine login:")],
            [(0.0, ""), (0.5, NO_DISPLAY), (2.0, "vmlab")],
            [(0.0, ""), (0.53, " * Starting vmlab-agent ... [ ok ]"), (1.0, "vmlab")],
        ):
            with self.subTest(frames=frames), \
                    self.assertRaisesRegex(AssertionError, "not recorded from power-on"):
                self.scan(frames)

    def test_firmware_first_after_start_by_passes_on_vmlab_hv(self):
        # VML-74: vmlab-hv's native recorder starts at the guest's first
        # painted frame, OVMF's black screen, and its logo comes at 0.53 s.
        for text in ("vmliab", "vmlab", "BdsDxe: loading Boot0002"):
            with self.subTest(text=text):
                at, _, kind = self.scan([(0.0, ""), (0.5, ""), (0.53, text), (2.0, GRUB)])
                self.assertEqual(at, 0.53)
                self.assertIn(kind, ("logo", "BdsDxe"))

    def test_the_logo_is_the_whole_text_of_a_frame(self):
        for text in ("VMLAB", "VMLIAB", " vmlab \n", "VM1AB\x0c", "|vmlab|"):
            with self.subTest(text=text):
                self.assertEqual(record.firmware_kind(text.upper()), "logo")
        for text in (
            "VMLAB-AGENT",
            " * STARTING VMLAB-AGENT ... [ OK ]",
            "MOUNT LABEL=VMLAB /MEDIA/VMLAB",
            "VMLAB BOOTSTRAP: AGENT INSTALLED (OPENRC, STARTED)",
            "VMLAB\nE2E LOGIN:",
        ):
            with self.subTest(text=text):
                self.assertIsNone(record.firmware_kind(text))

    def test_a_late_start_on_the_guest_console_fails(self):
        # Review of VML-36: the console line names vmlab-agent, and it stays
        # on tty1 up to the login prompt.
        console = " * Starting vmlab-agent ... [ ok ]\nWelcome to Alpine Linux 3.22\ne2e login:"
        for frames in ([(0.0, console)], [(0.0, "vmlab-agent"), (1.0, "vmlab")]):
            with self.subTest(frames=frames), \
                    self.assertRaisesRegex(AssertionError, "not recorded from power-on"):
                self.scan(frames)

    def test_the_guest_console_after_a_no_display_start_is_not_the_logo(self):
        # QEMU can show its no-display text again when the kernel's DRM driver
        # takes over, so the console can follow it with no GRUB in between.
        frames = [(0.0, NO_DISPLAY), (8.5, NO_DISPLAY + " "),
                  (9.0, " * Starting vmlab-agent ... [ ok ]\ne2e login:"),
                  (10.0, "mount LABEL=VMLAB /media/vmlab")]
        with self.assertRaisesRegex(AssertionError, "no firmware output"):
            self.scan(frames)

    def test_a_linux_console_does_not_pass_for_firmware(self):
        frames = [(0.0, NO_DISPLAY), (3.0, "Linux version 6.12\nalpine login:")]
        with self.assertRaisesRegex(AssertionError, "no firmware output") as caught:
            self.scan(frames)
        self.assertIn("3.00s Linux version 6.12", str(caught.exception))

    def test_nothing_within_the_window_fails(self):
        for late in ("vmlab", GRUB):
            with self.subTest(late=late), self.assertRaisesRegex(AssertionError, "no firmware output"):
                self.scan([(0.0, NO_DISPLAY), (record.FIRMWARE_WITHIN + 0.5, late)])

    def test_an_empty_recording_fails(self):
        with self.assertRaisesRegex(AssertionError, "first lit at None"):
            self.scan([(0.0, ""), (6.0, "")])


def make_video(path: Path, frames: int, options=()):
    """A gray video whose frame n is at n*n/10 s (a variable frame rate)
    and has brightness 40*(n//2): every frame shows twice."""
    subprocess.run(
        ["ffmpeg", "-v", "error", "-y", "-f", "lavfi",
         "-i", f"color=c=black:s=64x48:r=10:d={frames / 10}",
         "-vf", "format=yuv444p,geq=lum='40*floor(N/2)':cb=128:cr=128,setpts='N*N/10/TB'",
         "-fps_mode", "passthrough", *options, str(path)],
        check=True, capture_output=True,
    )
    return path


H264 = ("-c:v", "libx264", "-qp", "0", "-bf", "2", "-pix_fmt", "yuv444p")


@unittest.skipUnless(shutil.which("ffmpeg"), "needs ffmpeg")
class EarlyFramesTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)

    def frames(self, video, seconds):
        return [(round(at, 3), record.png_read(png)[2][0][0][0])
                for at, png in record.early_frames(video, seconds, self.dir / "frames")]

    def test_each_time_belongs_to_its_frame(self):
        # Frames at 0, .1, .4, .9, 1.6, 2.5, 3.6 s; pairs repeat, so the
        # distinct ones are frames 0, 2, 4, 6 at 0, .4, 1.6, 3.6 s.
        # H.264 with B-frames stores packets out of display order.
        for name, options in (("h264.mp4", H264), ("ffv1.mkv", ("-c:v", "ffv1"))):
            with self.subTest(name=name):
                got = self.frames(make_video(self.dir / name, 7, options), 10.0)
                self.assertEqual([at for at, _ in got], [0.0, 0.4, 1.6, 3.6])
                levels = [level for _, level in got]
                self.assertEqual(levels, sorted(set(levels)), got)

    def test_the_window_ends_before_seconds(self):
        got = self.frames(make_video(self.dir / "v.mkv", 7, ("-c:v", "ffv1")), 2.0)
        self.assertEqual([at for at, _ in got], [0.0, 0.4, 1.6])

    def test_time_counts_from_the_start_of_the_stream(self):
        video = make_video(self.dir / "late.mkv", 7, ("-c:v", "ffv1", "-output_ts_offset", "5"))
        self.assertEqual([at for at, _ in self.frames(video, 10.0)], [0.0, 0.4, 1.6, 3.6])

    def test_packets_that_decode_to_no_frame_take_no_frame_time(self):
        # A stream cut between keyframes: its first 7 packets, from 0.05 s,
        # decode to nothing, and the first frame is the keyframe at 0.75 s,
        # 0.7 s into the stream. One time per packet gave it 0.0 s.
        full = self.dir / "full.mp4"
        subprocess.run(
            ["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", "color=c=black:s=64x48:r=10:d=3",
             "-vf", "format=yuv444p,geq=lum='8*N':cb=128:cr=128", "-c:v", "libx264", "-qp", "0",
             "-pix_fmt", "yuv444p", "-g", "10", "-bf", "0", str(full)],
            check=True, capture_output=True,
        )
        cut = self.dir / "cut.mp4"
        subprocess.run(
            ["ffmpeg", "-v", "error", "-y", "-i", str(full), "-ss", "1.25", "-c", "copy",
             "-copyinkf", str(cut)],
            check=True, capture_output=True,
        )
        got = self.frames(cut, 10.0)
        self.assertEqual([at for at, _ in got], [round(0.7 + n / 10, 3) for n in range(10)])

    def test_closing_early_stops_ffmpeg(self):
        video = make_video(self.dir / "long.mkv", 60, ("-c:v", "ffv1"))
        started = []
        popen = subprocess.Popen

        def spy(*args, **kwargs):
            started.append(popen(*args, **kwargs))
            return started[-1]

        with patch.object(record.subprocess, "Popen", side_effect=spy):
            frames = record.early_frames(video, 400.0, self.dir / "frames")
            next(frames)
            frames.close()
        self.assertIsNotNone(started[0].poll())
        self.assertLess(len(list((self.dir / "frames").glob("f*.png"))), 3)

    def test_firmware_frame_stops_decoding_at_the_match(self):
        video = make_video(self.dir / "boot.mkv", 60, ("-c:v", "ffv1"))
        read = []

        def ocr(png):
            read.append(png)
            return "BdsDxe: loading Boot0002"

        with patch.object(record, "ocr", side_effect=ocr):
            at, _, kind = record.firmware_frame(video, self.dir / "boot.png", seconds=400.0)
        # Frames 0 and 2 are below the lit level; frame 4 at 1.6 s matches.
        self.assertEqual((at, kind, len(read)), (1.6, "BdsDxe", 1))
        self.assertLess(len(list((self.dir / "boot.frames").glob("f*.png"))), 5)

    def test_png_read_decodes_unfiltered_and_filtered_rows_alike(self):
        pixels = {}
        for pred in ("none", "paeth", "mixed"):
            png = self.dir / f"{pred}.png"
            subprocess.run(
                ["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=96x64",
                 "-frames:v", "1", "-pix_fmt", "rgb24", "-pred", pred, str(png)],
                check=True, capture_output=True,
            )
            pixels[pred] = record.png_read(png)
        self.assertEqual(pixels["none"], pixels["paeth"])
        self.assertEqual(pixels["none"], pixels["mixed"])
        self.assertGreater(len({px for row in pixels["none"][2] for px in row}), 50)
        w, h, sampled = record.png_read(self.dir / "none.png", step=8)
        self.assertEqual((w, h), (96, 64))
        self.assertEqual(sampled, [row[::8] for row in pixels["none"][2][::8]])

    def test_a_broken_video_fails(self):
        bad = self.dir / "bad.mp4"
        bad.write_bytes(b"not a video")
        with self.assertRaises(subprocess.CalledProcessError):
            list(record.early_frames(bad, 1.0, self.dir / "frames"))


if __name__ == "__main__":
    unittest.main()
