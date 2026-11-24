"""build/dist.sh keeps `--locked` on a build whose version it stamps (F9 of
docs/reviews/install-release.md): a Cargo.lock that does not match Cargo.toml
must fail a release build, not be resolved again."""

import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest

DIST = pathlib.Path(__file__).resolve().parent / "dist.sh"
BPF_DIR = "src/net/fastpath/bpf"
BPF_OBJS = ("fastpath_sockmap.bpf.o", "xdp_switch.bpf.o")

# The recipes dist.sh calls after the binary build, as no-ops that leave what
# it copies.
JUST = """#!/usr/bin/env bash
set -euo pipefail
case "$1" in
  guest-package)
    mkdir -p target/guest-package
    : > "target/guest-package/vmlab-guest-$2.tar.gz"
    : > "target/guest-package/vmlab-guest-$2.tar.gz.sha256"
    ;;
  ci::hv-package | ebpf-build) ;;
  *) echo "just stub: unexpected recipe $1" >&2; exit 2 ;;
esac
"""

CARGO_TOML = """[package]
name = "vmlab"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "vmlab"
path = "src/main.rs"

[[bin]]
name = "vmlab-fastpath"
path = "src/fastpath.rs"

[dependencies]
lockdep = { path = "lockdep" }

[workspace]
"""

LOCKDEP_TOML = """[package]
name = "lockdep"
version = "0.3.0"
edition = "2021"
"""


def run(cmd, cwd, env):
    return subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True)


def without_package(lock, name):
    """`lock` without the `[[package]]` entry of `name`."""
    head, *entries = lock.split("[[package]]\n")
    kept = [e for e in entries if not e.startswith(f'name = "{name}"\n')]
    assert len(kept) == len(entries) - 1, f"no entry for {name} in the lock"
    return "[[package]]\n".join([head, *kept])


class DistLockedTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix="dist-locked-")
        self.addCleanup(tmp.cleanup)
        self.root = pathlib.Path(tmp.name) / "repo"
        stubs = pathlib.Path(tmp.name) / "stubs"
        stubs.mkdir()
        (stubs / "just").write_text(JUST)
        (stubs / "just").chmod(0o755)

        for rel, text in {
            "Cargo.toml": CARGO_TOML,
            "src/main.rs": "fn main() { lockdep::f(); }\n",
            "src/fastpath.rs": "fn main() { lockdep::f(); }\n",
            "lockdep/Cargo.toml": LOCKDEP_TOML,
            "lockdep/src/lib.rs": "pub fn f() {}\n",
            ".gitignore": "target/\n",
            **{f"{BPF_DIR}/{obj}": obj for obj in BPF_OBJS},
        }.items():
            path = self.root / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        (self.root / "build").mkdir()
        shutil.copy(DIST, self.root / "build/dist.sh")

        # The build must land in the scratch tree's own target/, where dist.sh
        # takes the binaries from, whatever the gate's environment redirects.
        self.env = {k: v for k, v in os.environ.items()
                    if k not in ("CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR",
                                 "CARGO_BUILD_TARGET", "RUSTFLAGS")}
        self.env.update(
            PATH=f"{stubs}{os.pathsep}{self.env['PATH']}",
            GIT_AUTHOR_NAME="ci", GIT_AUTHOR_EMAIL="ci@example.invalid",
            GIT_COMMITTER_NAME="ci", GIT_COMMITTER_EMAIL="ci@example.invalid",
        )
        self.must("cargo", "generate-lockfile", "--offline")
        self.must("git", "init", "-q")
        self.must("git", "add", "-A")
        self.must("git", "commit", "-q", "-m", "scratch")

    def must(self, *cmd):
        done = run(cmd, self.root, self.env)
        self.assertEqual(done.returncode, 0, f"{cmd}: {done.stderr}")
        return done

    def dist(self, version):
        return run(["bash", "build/dist.sh", version], self.root, self.env)

    def tracked(self):
        return {f: (self.root / f).read_text() for f in ("Cargo.toml", "Cargo.lock")}

    def test_a_stamped_build_passes_on_a_matching_lock(self):
        before = self.tracked()
        done = self.dist("9.9.9")
        self.assertEqual(done.returncode, 0, done.stderr)
        for name in ("vmlab", "vmlab-fastpath"):
            self.assertTrue((self.root / f"target/dist/{name}-9.9.9-linux-x86_64").is_file())
        self.assertEqual(self.tracked(), before, "the stamp was not put back")

    def test_a_stamped_build_refuses_a_lock_that_misses_a_package(self):
        lock = self.root / "Cargo.lock"
        lock.write_text(without_package(lock.read_text(), "lockdep"))
        before = self.tracked()
        # The same tree fails a plain locked build: the proof's control.
        control = run(["cargo", "build", "--release", "--locked", "--offline"],
                      self.root, self.env)
        self.assertNotEqual(control.returncode, 0, "the control build passed")
        self.assertEqual(self.tracked(), before)

        done = self.dist("9.9.9")
        self.assertNotEqual(done.returncode, 0,
                            f"dist.sh resolved the lock again:\n{done.stderr}")
        self.assertIn("--locked", done.stderr)
        self.assertFalse((self.root / "target/dist/vmlab-9.9.9-linux-x86_64").exists())
        self.assertEqual(self.tracked(), before, "the stamp was not put back")

    def test_an_unstamped_build_refuses_the_same_lock(self):
        lock = self.root / "Cargo.lock"
        lock.write_text(without_package(lock.read_text(), "lockdep"))
        done = self.dist("0.1.0")
        self.assertNotEqual(done.returncode, 0, done.stderr)
        self.assertIn("--locked", done.stderr)


if __name__ == "__main__":
    unittest.main()
