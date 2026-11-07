"""Tests for check_shell_paths.py: each bad pattern must fail, each guarded form must pass."""

import contextlib
import io
import tempfile
import unittest
from pathlib import Path

import check_shell_paths as checker


def make_repo(files: dict[str, str]) -> Path:
    """Write the given files into a fresh directory and return its path. No git: the
    tests name the files, so the check never lists them."""
    root = Path(tempfile.mkdtemp(prefix="check-shell-paths-"))
    for name, text in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
    return root


def run_main(root: Path, *names: str) -> tuple[int, str]:
    err = io.StringIO()
    with contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()):
        code = checker.main(["check_shell_paths.py", str(root), *names])
    return code, err.getvalue()


GOOD_SCRIPT = """#!/usr/bin/env bash
set -euo pipefail
d="$(ls -dt target/dist/vmlab-* | head -1)"
[ -n "$d" ] && [ -d "$d" ] || { echo "d missing" >&2; exit 1; }
base=/tmp/x
[ -n "$base" ] && [ "$base" != / ] || exit 1
cp -a "$d"/. "$base/hv/"
"""

BAD_SCRIPT = """#!/usr/bin/env bash
d=$(ls -dt target/dist/vmlab-hv-* | head -1)
cp -a "$d"/. "$base/hv/"
"""


class ShellPathsTest(unittest.TestCase):
    def test_bad_script_fails_the_gate(self):
        # The VML-81 pattern: an empty command result becomes `/.`.
        root = make_repo({"build.sh": BAD_SCRIPT})
        code, err = run_main(root, "build.sh")
        self.assertEqual(code, 1, err)
        self.assertIn("build.sh:3: P1", err)
        self.assertIn("build.sh:2: S1", err)

    def test_good_script_passes(self):
        root = make_repo({"build.sh": GOOD_SCRIPT})
        code, err = run_main(root, "build.sh")
        self.assertEqual(code, 0, err)

    def test_rm_with_trailing_slash_and_glob_is_flagged(self):
        text = "#!/bin/sh\nset -eu\nrm -rf \"$OUT\"/*\n"
        problems = checker.check_file(self._file(text))
        self.assertTrue(any("P1" in p and "$OUT" in p for p in problems), problems)

    def test_find_exec_cp_is_flagged(self):
        text = '#!/bin/sh\nset -eu\nfind "$src" -exec cp -a {} "$dst/" \\;\n'
        problems = checker.check_file(self._file(text))
        self.assertTrue(any(":3: P1" in p and "$dst" in p for p in problems), problems)

    def test_chown_recursive_on_variable_path_is_flagged(self):
        text = '#!/bin/sh\nset -eu\nchown -R "$u:g" "$home/Library"\n'
        problems = checker.check_file(self._file(text))
        self.assertTrue(any("P1" in p and "$home" in p for p in problems), problems)

    def test_command_result_only_counts_with_a_slash(self):
        text = '#!/bin/sh\nset -eu\ncp "$(fetch x)" dst/\ncp -a "$(fetch x)"/. dst/\n'
        problems = checker.check_file(self._file(text))
        self.assertEqual(len([p for p in problems if "P1" in p]), 1, problems)
        self.assertTrue(any(":4: P1" in p for p in problems), problems)

    def test_file_test_on_a_child_is_not_a_guard(self):
        # `-f "$SAVE/$f"` checks a file, not $SAVE. It must not count as a guard.
        text = ('#!/bin/sh\nset -eu\n[ -f "$SAVE/$f" ] && echo yes\n'
                'cp "$SAVE/x" "$dst"\ncp -a "$SAVE"/. "$dst/"\n')
        problems = checker.check_file(self._file(text))
        self.assertTrue(any(":5: P1" in p and "$SAVE" in p for p in problems), problems)

    def test_guard_after_the_use_does_not_count(self):
        text = ('#!/bin/sh\nset -eu\ncp -a "$d"/. "$dst/"\n'
                '[ -n "$d" ] && [ "$d" != / ] || exit 1\n')
        problems = checker.check_file(self._file(text))
        self.assertTrue(any(":3: P1" in p for p in problems), problems)

    def test_missing_errexit_fails_unless_opted_out(self):
        bad = self._file("#!/bin/sh\nset -u\necho hi\n")
        self.assertTrue(any("S1" in p for p in checker.check_file(bad)))
        opted = self._file("#!/bin/sh\n# check-shell-paths: no-errexit finish must run\nset -u\necho hi\n")
        self.assertEqual(checker.check_file(opted), [])

    def test_sourced_library_gets_only_the_path_rule(self):
        # No shebang: a sourced file must not turn on errexit for its caller.
        text = 'rm -rf "$W"/*\n'
        problems = checker.check_file(self._file(text))
        self.assertEqual(len(problems), 1, problems)
        self.assertIn(": P1: ", problems[0])

    def test_just_recipe_with_unguarded_path_fails(self):
        root = make_repo({"justfile": "clean dir:\n\trm -rf {{dir}}/*\n"})
        code, err = run_main(root, "justfile")
        self.assertEqual(code, 1, err)
        self.assertIn("justfile:2: P1", err)

    def test_just_recipe_guarded_passes(self):
        text = ("clean dir:\n"
                "\t[ -n {{dir}} ] && [ {{dir}} != / ] || exit 1\n"
                "\trm -rf {{dir}}/*\n")
        root = make_repo({"justfile": text})
        code, err = run_main(root, "justfile")
        self.assertEqual(code, 0, err)

    def test_just_expression_guard_is_for_the_same_expression_only(self):
        text = ("ship:\n"
                "\t[ -n {{quote(a)}} ] || exit 1\n"
                "\tcp x {{quote(b)}}/y\n")
        problems = checker.check_file(self._file(text, name="justfile"))
        self.assertTrue(any("P1" in p for p in problems), problems)

    def _file(self, text: str, name: str = "script.sh") -> Path:
        return make_repo({name: text}) / name


if __name__ == "__main__":
    unittest.main()
