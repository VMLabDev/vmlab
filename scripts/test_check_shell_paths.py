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

    # Round 2 (VML-88 review F4): each gap below used to pass the check.

    def _p1(self, body: str) -> list[str]:
        problems = checker.check_file(self._file("#!/bin/bash\nset -eu\n" + body))
        return [p for p in problems if ": P1: " in p]

    def test_array_element_needs_its_own_guard(self):
        # A guard on $srcdir is ${srcdir[0]}: it says nothing about ${srcdir[$name]}.
        bad = self._p1('[ -n "$srcdir" ] || exit 1\nrm -rf "${srcdir[$name]}"/*\n')
        self.assertEqual(len(bad), 1, bad)
        good = self._p1('[ -n "${srcdir[$name]}" ] && [ "${srcdir[$name]}" != / ] || exit 1\n'
                        'rm -rf "${srcdir[$name]}"/*\n')
        self.assertEqual(good, [])

    def test_comparison_with_another_path_is_not_a_guard(self):
        bad = self._p1('[ "$other" != /tmp ] && echo; cp -a "$d"/. x\n')
        self.assertEqual(len(bad), 1, bad)
        bad = self._p1('[ "$d" != /tmp ] || exit 1\ncp -a "$d"/. x\n')
        self.assertEqual(len(bad), 1, bad)
        # Only the variable compared with / is guarded, not the others on the line.
        bad = self._p1('[ "$d" != / ] || exit 1; cp -a "$e"/. x\n')
        self.assertEqual(len(bad), 1, bad)
        self.assertIn("$e", bad[0])
        self.assertEqual(self._p1('[ "$d" != "/" ] || exit 1\ncp -a "$d"/. x\n'), [])

    def test_reassignment_after_the_guard_voids_it(self):
        bad = self._p1('[ -n "$d" ] || exit 1\nd=$(ls -d a* | head -1)\nchown -R u "$d"/*\n')
        self.assertEqual(len(bad), 1, bad)
        self.assertIn(":5: P1", bad[0])
        for assign in ("local d", "read -r d", "for d in \"$@\"; do :; done", "export d=$1"):
            bad = self._p1(f'[ -n "$d" ] || exit 1\n{assign}\nrm -rf "$d"/*\n')
            self.assertEqual(len(bad), 1, (assign, bad))
        # A guard after the new assignment counts again.
        good = self._p1('[ -n "$d" ] || exit 1\nd=$(ls)\n[ -n "$d" ] || exit 1\nrm -rf "$d"/*\n')
        self.assertEqual(good, [])

    def test_assignment_in_the_same_line_before_the_use(self):
        self.assertEqual(len(self._p1('d=$(ls -d a* | head -1); cp -a "$d"/. x\n')), 1)

    def test_value_that_cannot_be_empty_or_root_is_safe(self):
        for value in ("/Library/x", "$work/src/$name", "${VMLAB_BIN_DIR:-$HOME/.local/bin}", "out"):
            self.assertEqual(self._p1(f'd={value}\nrm -rf "$d"/*\n'), [], value)
        for value in ("$EV/$name", "$(mktemp -d)", "/", "$1", "${X:-}"):
            self.assertEqual(len(self._p1(f'd={value}\nrm -rf "$d"/*\n')), 1, value)
        self.assertEqual(self._p1('for f in "$r"/probes/*.json; do cp "$f" x; done\n'), [])
        self.assertEqual(len(self._p1('for f in "$@"; do cp "$f" x; done\n')), 1)

    def test_unquoted_command_result_is_flagged(self):
        self.assertEqual(len(self._p1('cp -a $(ls -d a* | head -1)/. x\n')), 1)
        self.assertEqual(len(self._p1('cp -a `ls -d a* | head -1`/. x\n')), 1)
        self.assertEqual(len(self._p1('cp -a "$(ls -dt "$x"/v-* | head -1)"/. x\n')), 1)

    def test_prefix_commands_are_followed(self):
        for prefix in ("sudo -E", "sudo -u root", "command", "env X=1", "X=1",
                       "nice -n 19", "ionice -c3 nice -n19", "exec"):
            bad = self._p1(f'{prefix} cp -a "$d"/. x\n')
            self.assertEqual(len(bad), 1, (prefix, bad))

    def test_flags_of_other_commands_are_not_guards(self):
        # `rm -f "$x"` and `patch -d "$x"` used to count as -f and -d tests.
        bad = self._p1('patch -d "$w" -p1 <p\ncp "$s" "$w/.config"\n')
        self.assertTrue(any("$w" in p for p in bad), bad)
        self.assertEqual(len(self._p1('rm -f "$conf"\n')), 1)

    def test_test_forms_still_guard(self):
        for guard in ('[ -n "$d" ] || exit 1', '[[ -d $d ]] || exit 1', 'test -n "$d" || exit 1',
                      '[ ! -d "$d" ] && exit 1', ': "${d:?}"', '[ -z "${d}" ] && exit 1'):
            self.assertEqual(self._p1(f'{guard}\nrm -rf "$d"/*\n'), [], guard)

    def test_no_errexit_opt_out_keeps_set_u(self):
        text = ("#!/usr/bin/env bash\n"
                "# check-shell-paths: no-errexit probes record a failed rc and go on\n"
                "set -uo pipefail\nwait \"$pid\"; rc=$?\n")
        self.assertEqual(checker.check_file(self._file(text)), [])

    def _file(self, text: str, name: str = "script.sh") -> Path:
        return make_repo({name: text}) / name


if __name__ == "__main__":
    unittest.main()
