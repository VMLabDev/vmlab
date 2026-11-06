"""Boot recordings must contain firmware, including when it paints later."""

import unittest
from pathlib import Path
from unittest.mock import patch

from scenarios import record


class FirmwareFrameTests(unittest.TestCase):
    def scan(self, frames):
        """Run firmware_frame over distinct frames given as (time, text);
        a frame with text is lit, one without is dark."""
        def early(video, seconds, out):
            for at, text in frames:
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

    def test_firmware_painting_after_the_old_deadline_passes(self):
        at, text = self.scan([(0.1, ""), (6.2, 'BdsDxe: starting Boot0002 "UEFI Misc Device"')])
        self.assertEqual(at, 6.2)
        self.assertIn("BdsDxe", text)

    def test_bios_firmware_text_is_recognized(self):
        for text in ("SeaBIOS version 1.17.0-vmlab", "iPXE 2.0.0"):
            with self.subTest(text=text):
                self.assertEqual(self.scan([(1.2, text)])[0], 1.2)

    def test_the_vmlab_logo_is_firmware_when_bdsdxe_was_never_recorded(self):
        # VML-36's failing recordings: the logo, then GRUB, no BdsDxe frame.
        frames = [(0.4, ""), (1.67, "vmlab"), (2.07, "GNU GRUB version 2.12")]
        self.assertEqual(self.scan(frames)[0], 1.67)

    def test_a_lit_bootloader_or_linux_console_does_not_pass_for_firmware(self):
        frames = [(0.1, "GNU GRUB version 2.12"), (3.0, "Linux version 6.12\nalpine login:")]
        with self.assertRaisesRegex(AssertionError, "no firmware output") as caught:
            self.scan(frames)
        self.assertIn("3.00s Linux version 6.12", str(caught.exception))

    def test_an_empty_recording_fails(self):
        with self.assertRaisesRegex(AssertionError, "first lit at None"):
            self.scan([(0.0, ""), (6.0, "")])


if __name__ == "__main__":
    unittest.main()
