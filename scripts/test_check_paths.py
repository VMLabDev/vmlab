import pathlib
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import check_paths  # noqa: E402

SCRIPT = pathlib.Path(__file__).resolve().parent / "check_paths.py"


class ProblemTest(unittest.TestCase):
    def test_windows_safe_paths_pass(self):
        for path in ("a/b.txt", "hv/pci-roms/0000-00-03.0.rom", ".github/x.yml",
                     "dir.d/file", "a b/c d.txt"):
            self.assertIsNone(check_paths.problem(path), path)

    def test_forbidden_characters_fail(self):
        for c in ':*?"<>|\\':
            self.assertIsNotNone(check_paths.problem(f"dir/a{c}b"), c)

    def test_trailing_dot_or_space_in_any_component_fails(self):
        for path in ("dir./file", "dir /file", "dir/file.", "dir/file "):
            self.assertIsNotNone(check_paths.problem(path), path)


class RepoTest(unittest.TestCase):
    def run_on(self, names):
        with tempfile.TemporaryDirectory(prefix="check-paths-") as tmp:
            git = ["git", "-C", tmp]
            subprocess.run(git + ["init", "-q"], check=True)
            for name in names:
                path = pathlib.Path(tmp, name)
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("x\n")
            subprocess.run(git + ["add", "--", *names], check=True)
            return subprocess.run([sys.executable, str(SCRIPT), tmp],
                                  capture_output=True, text=True)

    def test_clean_tree_passes(self):
        result = self.run_on(["pci-roms/0000-00-03.0.rom"])
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_colon_in_tracked_path_fails_and_names_it(self):
        result = self.run_on(["ok.txt", "pci-roms/0000:00:03.0.rom"])
        self.assertEqual(result.returncode, 1)
        self.assertIn("pci-roms/0000:00:03.0.rom", result.stderr)
        self.assertNotIn("ok.txt", result.stderr)


if __name__ == "__main__":
    unittest.main()
