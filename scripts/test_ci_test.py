import contextlib
import io
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import ci_test


class EvidenceTests(unittest.TestCase):
    def test_raw_output_and_failure_status(self):
        for status in (0, 101):
            with self.subTest(status=status), tempfile.TemporaryDirectory() as root:
                with patch.dict(os.environ, VMLAB_TARGET_ROOT=root), \
                        patch("sys.argv", ["ci_test.py", "main", "--all-features"]), \
                        patch("subprocess.check_output", return_value="host: x86_64-unknown-linux-gnu\n"), \
                        patch("subprocess.run") as docs, \
                        patch("subprocess.Popen") as popen, \
                        contextlib.redirect_stdout(io.StringIO()) as console:
                    process = popen.return_value.__enter__.return_value
                    process.stdout = ['{"type":"suite","event":"started"}\n']
                    process.wait.return_value = status
                    docs.return_value.returncode = 0
                    self.assertEqual(ci_test.main(), status)
                    self.assertEqual(
                        (Path(root) / "test-results/main.json").read_text(),
                        '{"type":"suite","event":"started"}\n')
                    self.assertIn("main.json", console.getvalue())
                    self.assertIn("--all-features", popen.call_args.args[0])
                    self.assertNotIn("RUSTC_BOOTSTRAP", popen.call_args.kwargs["env"])
                    self.assertIn("--run", popen.call_args.kwargs["env"][
                        "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER"])
                    if status == 0:
                        self.assertIn("--doc", docs.call_args.args[0])
                        self.assertNotIn("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER",
                                         docs.call_args.kwargs["env"])
                    else:
                        docs.assert_not_called()

    def test_runner_sets_bootstrap_only_for_binary(self):
        with patch("sys.argv", ["ci_test.py", "--run", "/tmp/test", "arg"]), \
                patch("os.execvpe", side_effect=RuntimeError) as execute:
            with self.assertRaises(RuntimeError):
                ci_test.main()
            self.assertEqual(execute.call_args.args[:2], ("/tmp/test", [
                "/tmp/test", "arg", "-Z", "unstable-options", "--format", "json", "--report-time"]))
            self.assertEqual(execute.call_args.args[2]["RUSTC_BOOTSTRAP"], "1")

    def test_refuses_root(self):
        with patch.dict(os.environ, VMLAB_TARGET_ROOT="/"), \
                patch("sys.argv", ["ci_test.py", "hv"]):
            with self.assertRaises(SystemExit):
                ci_test.main()
