"""Boot recordings must start at power-on and contain firmware, including
when it paints late."""

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

        def pixels(path):
            text = str(path).split(":", 1)[1]
            level = 255 if text else 0
            return 80, 80, [[(level, level, level)] * 80] * 80

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


if __name__ == "__main__":
    unittest.main()
