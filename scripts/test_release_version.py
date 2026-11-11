import os
import pathlib
import subprocess
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parent / "release-version.sh"
GIT_ENV = dict(
    os.environ,
    GIT_AUTHOR_NAME="ci", GIT_AUTHOR_EMAIL="ci@example.invalid",
    GIT_COMMITTER_NAME="ci", GIT_COMMITTER_EMAIL="ci@example.invalid",
)


def git(repo, *args):
    return subprocess.run(["git", "-C", repo, *args], check=True,
                          capture_output=True, text=True, env=GIT_ENV).stdout


def build(steps):
    """Make a repo from (subject, trailers, tag) steps; the last step is HEAD."""
    tmp = tempfile.TemporaryDirectory(prefix="release-version-")
    repo = tmp.name
    git(repo, "init", "-q")
    for subject, trailers, tag in steps:
        args = ["commit", "-q", "--allow-empty", "-m", subject]
        if trailers:
            args += ["-m", "\n".join(trailers)]
        git(repo, *args)
        if tag:
            git(repo, "tag", tag)
    return tmp, repo


def calc(repo, source=None):
    run = subprocess.run(["bash", str(SCRIPT)], cwd=repo, capture_output=True,
                         text=True, env=GIT_ENV)
    if run.returncode != 0:
        raise AssertionError(f"exit {run.returncode}: {run.stderr}")
    if source is not None and f"source={source}" not in run.stderr:
        raise AssertionError(f"missing source {source}: {run.stderr}")
    out = {}
    for line in run.stdout.splitlines():
        key, _, value = line.partition("=")
        out[key] = value
    return out


class ReleaseVersionTest(unittest.TestCase):
    def check(self, steps, source=None, **expected):
        tmp, repo = build(steps)
        self.addCleanup(tmp.cleanup)
        got = calc(repo, source)
        for key, value in expected.items():
            self.assertEqual(got.get(key), value, f"{key} in {got}")

    def test_head_bump_trailer_overrides_subjects(self):
        for bump, base, subject, version in [
            ("major", "1.2.3", "fix: a", "2.0.0-alpha"),
            ("major", "0.2.3", "fix: a", "0.3.0-alpha"),
            ("minor", "1.2.3", "feat!: a", "1.3.0-alpha"),
            ("patch", "1.2.3", "feat!: a", "1.2.4-alpha"),
        ]:
            with self.subTest(bump=bump, base=base):
                self.check(
                    [("chore: init", None, f"v{base}"),
                     (subject, ["pre-release: true", f"bump: {bump}"], None)],
                    version=version, bump=bump, source="HEAD bump trailer")

    def test_bump_trailer_does_not_change_promotion(self):
        for bump in ("major", "minor", "patch"):
            with self.subTest(bump=bump):
                self.check(
                    [("chore: init", None, "v1.2.3-alpha"),
                     ("feat!: a", ["release: true", f"bump: {bump}"], None)],
                    version="1.2.3", bump="promote", prerelease="false",
                    source="pre-release promotion")

    def test_pre_release_after_alpha_bumps_past_it(self):
        self.check(
            [("chore: init", None, "v0.45.0"),
             ("fix: a", None, "v0.45.1-alpha"),
             ("fix: b", ["pre-release: true"], None)],
            do_release="true", version="0.45.2-alpha", prerelease="true",
            bump="patch", last_tag="v0.45.1-alpha")

    def test_release_after_alpha_promotes_it(self):
        self.check(
            [("chore: init", None, "v0.45.0"),
             ("fix: a", None, "v0.45.1-alpha"),
             ("chore(release): cut", ["release: true"], None)],
            do_release="true", version="0.45.1", prerelease="false",
            bump="promote", last_tag="v0.45.1-alpha")

    def test_release_after_release_bumps(self):
        self.check(
            [("chore: init", None, "v0.45.1"),
             ("fix: a", ["release: true"], None)],
            do_release="true", version="0.45.2", prerelease="false",
            bump="patch", last_tag="v0.45.1")

    def test_breaking_change_in_0x_bumps_minor(self):
        self.check(
            [("chore: init", None, "v0.45.1"),
             ("feat!: drop the old api", ["release: true"], None)],
            do_release="true", version="0.46.0", prerelease="false",
            bump="major", last_tag="v0.45.1")

    def test_feat_after_release_bumps_minor(self):
        self.check(
            [("chore: init", None, "v0.45.1"),
             ("feat: new thing", ["release: true"], None)],
            do_release="true", version="0.46.0", prerelease="false",
            bump="minor", last_tag="v0.45.1", source="commit subjects")

    def test_release_without_tag_starts_from_zero(self):
        self.check(
            [("fix: a", ["release: true"], None)],
            do_release="true", version="0.0.1", prerelease="false",
            bump="patch", last_tag="")

    def test_no_trailer_is_tests_only(self):
        self.check(
            [("chore: init", None, "v0.45.0"),
             ("fix: a", None, None)],
            do_release="false")
        tmp, repo = build([("chore: init", None, "v0.45.0"), ("fix: a", None, None)])
        self.addCleanup(tmp.cleanup)
        self.assertNotIn("version", calc(repo))


if __name__ == "__main__":
    unittest.main()
