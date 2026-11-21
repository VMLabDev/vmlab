"""Exercise the tracked merge rule with two independent series appends."""

from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import unittest


ROOT = Path(__file__).resolve().parents[3]


class SeriesMergeTest(unittest.TestCase):
    def test_concurrent_appends_merge(self):
        with TemporaryDirectory() as temp:
            repo = Path(temp)

            def git(*args):
                result = subprocess.run(
                    ["git", "-c", "user.name=Series Merge Test",
                     "-c", "user.email=series-merge@example.invalid",
                     "-c", "commit.gpgsign=false", *args],
                    cwd=repo, capture_output=True, text=True,
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

            git("init", "--initial-branch=first")
            (repo / ".gitattributes").write_bytes((ROOT / ".gitattributes").read_bytes())
            series = repo / "guest/win3d/patches/mesa/series"
            series.parent.mkdir(parents=True)
            series.write_text("base.patch\n")
            git("add", ".")
            git("commit", "-m", "Base series")
            git("branch", "second")

            for branch in ("first", "second"):
                git("switch", branch)
                with series.open("a") as entries:
                    entries.write(f"{branch}.patch\n")
                git("commit", "-am", f"Append {branch} patch")

            git("merge", "--no-edit", "first")
            base, *appends = series.read_text().splitlines()
            self.assertEqual(base, "base.patch")
            self.assertCountEqual(appends, ["first.patch", "second.patch"])
