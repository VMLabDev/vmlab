"""Boot recordings must contain firmware, including when it paints later."""

import unittest
from pathlib import Path
from unittest.mock import patch

from scenarios import record


class FirmwareFrameTests(unittest.TestCase):
    def scan(self, text, lit_at):
        seen = []

        def frame(video, at, out):
            seen.append(at)
            return out

        def pixels(path):
            level = 255 if seen[-1] >= lit_at else 0
            return 80, 80, [[(level, level, level)] * 80] * 80

        with patch.object(record, "frame_at", side_effect=frame), \
                patch.object(record, "png_read", side_effect=pixels), \
                patch.object(record, "ocr", return_value=text):
            return record.firmware_frame(Path("boot.mp4"), Path("boot.png"))

    def test_firmware_painting_after_the_old_deadline_passes(self):
        at, text = self.scan('BdsDxe: starting Boot0002 "UEFI Misc Device"', 0.8)
        self.assertEqual(at, 0.8)
        self.assertIn("BdsDxe", text)

    def test_bios_firmware_text_is_recognized(self):
        for text in ("SeaBIOS version 1.17.0-vmlab", "iPXE 2.0.0"):
            with self.subTest(text=text):
                self.assertEqual(self.scan(text, 1.2)[0], 1.2)

    def test_a_lit_linux_console_does_not_pass_for_firmware(self):
        with self.assertRaisesRegex(AssertionError, "no firmware text"):
            self.scan("Linux version 6.12\nalpine login:", 0.1)

    def test_an_empty_recording_fails(self):
        with self.assertRaisesRegex(AssertionError, "first lit at None"):
            self.scan("", 6.0)


if __name__ == "__main__":
    unittest.main()
