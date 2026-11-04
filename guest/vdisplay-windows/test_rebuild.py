"""Portable checks for the Windows package transfer."""

from pathlib import Path
import tempfile
import unittest
import zipfile

from rebuild import extract_package


class PackageTransfer(unittest.TestCase):
    def test_windows_zip_directory_separators(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "package.zip"
            with zipfile.ZipFile(archive, "w") as package:
                package.writestr("ivshmem\\ivshmem.inf", b"driver input")
            extract_package(archive, root / "output")
            self.assertEqual((root / "output/ivshmem/ivshmem.inf").read_bytes(), b"driver input")

    def test_both_separator_styles_reject_parent_traversal(self):
        for name in ["../outside", "..\\outside", "/outside"]:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                archive = root / "package.zip"
                with zipfile.ZipFile(archive, "w") as package:
                    package.writestr(name, b"unexpected")
                with self.assertRaises(ValueError):
                    extract_package(archive, root / "output")


if __name__ == "__main__":
    unittest.main()
