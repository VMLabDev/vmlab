"""Templates: the fast Alpine build, a layered build, the store verbs, and a
round trip through a local OCI registry."""

import json
import shutil

from harness import E2E, WORK, ScenarioFailed

REF = "x86_64/e2e-alpine"
REGISTRY = "localhost:5000/e2e/e2e-alpine"
LAYERED = """import <vmlab.wcl>

template "e2e-layered" {
  arch    = "x86_64"
  version = "1.0"
  profile = "linux-modern"
  source "template" { from = "x86_64/e2e-alpine" }
  provision "scripts/mark.ws" { }
}
"""
MARK = """use vmlab

fn main(lab: Lab) {
    let vm = lab.vm("build").expect("no build vm")
    vm.wait_ready(300).expect("agent never answered")
    let r = vm.exec("/bin/sh", ["-c", "echo layered > /etc/e2e-layer"]).expect("mark failed")
    lab.log(fmt("marked, exit {}", r.exit_code))
}
"""


def listed(h) -> list[dict]:
    return json.loads(h.vmlab("template", "list", "--json").out)


def has(h, name: str) -> bool:
    return any(t.get("name") == name for t in listed(h))


def versions(h, name: str) -> list[str]:
    return [t["version"] for t in listed(h) if t.get("name") == name and t.get("arch") == "x86_64"]


def rm_all(h, name: str) -> bool:
    """Remove every x86_64 build of `name`; true when any existed."""
    found = versions(h, name)
    for v in found:
        h.vmlab("template", "rm", f"x86_64/{name}@{v}", "--force")
    return bool(found)


def built_event(h, name: str) -> bool:
    """The build lab's own event log carries `template.built`."""
    r = h.vmlab("logs", f"build-x86_64-{name}", "-n", "500", "-o", "jsonl", check=False)
    return "template.built" in r.out


def ensure_base(h) -> None:
    """Build the e2e template when the store lacks it, reporting nothing."""
    if has(h, "e2e-alpine"):
        return
    src = WORK / "template-alpine"
    shutil.rmtree(src, ignore_errors=True)
    shutil.copytree(E2E / "templates" / "alpine", src)
    h.vmlab("template", "build", "--version", "1.0.0", cwd=src, timeout=900)


def run(h):
    src = WORK / "template-alpine"
    if src.exists():
        shutil.rmtree(src)
    shutil.copytree(E2E / "templates" / "alpine", src)

    # A clean store, so the build is exercised rather than found.
    rm_all(h, "e2e-alpine")
    built = h.vmlab("template", "build", "--version", "1.0.0", cwd=src, timeout=900, check=False)
    if not h.ok("template.build.qcow2", built.code == 0 and has(h, "e2e-alpine"), built.text.strip()[-300:]):
        raise ScenarioFailed("the e2e template did not build; every later scenario needs it")
    h.check("template.list", lambda: has(h, "e2e-alpine"))
    h.check("template.event", lambda: built_event(h, "e2e-alpine"), "the build lab logged template.built")

    # Layered on the first, with one provision.
    layered = WORK / "template-layered"
    shutil.rmtree(layered, ignore_errors=True)
    (layered / "scripts").mkdir(parents=True)
    (layered / "vmlab.wcl").write_text(LAYERED)
    (layered / "scripts" / "mark.ws").write_text(MARK)
    rm_all(h, "e2e-layered")
    h.check(
        "template.build.layered",
        lambda: h.vmlab("template", "build", "--version", "1.0.0", cwd=layered, timeout=900).code == 0
        and has(h, "e2e-layered"),
    )

    # Export, remove, import.
    archive = WORK / "e2e-layered.tar.zst"
    h.check("template.export", lambda: h.vmlab("template", "export", "x86_64/e2e-layered@1.0.0", str(archive)) and archive.stat().st_size > 0)
    h.check("template.rm", lambda: h.vmlab("template", "rm", "x86_64/e2e-layered@1.0.0", "--force") and not has(h, "e2e-layered"))
    h.check("template.import", lambda: h.vmlab("template", "import", str(archive)) and has(h, "e2e-layered"))

    # Clean: a second build of the same template, then keep only the newest.
    h.vmlab("template", "build", "--version", "1.0.1", cwd=layered, timeout=900)
    dry = h.vmlab("template", "clean", "e2e-layered", "--keep", "1")
    applied = h.vmlab("template", "clean", "e2e-layered", "--keep", "1", "-y", "--force")
    left = [t for t in listed(h) if t.get("name") == "e2e-layered"]
    h.ok("template.clean", "1.0.0" in dry.text and applied.code == 0 and len(left) == 1,
         f"dry run named 1.0.0; {len(left)} build(s) left")

    # Registry round trip against localhost:5000.
    h.check("template.registry.login", lambda: h.vmlab("template", "login", "localhost:5000", "-u", "e2e", "-p", "e2e"))
    # Two pushes at once: each must stage its own chunks, or they upload each
    # other's bytes and the registry refuses both.
    def push_two_at_once():
        other = h.background(["vmlab", "template", "push", "x86_64/e2e-layered", "localhost:5000/e2e/e2e-layered",
                              "--source", "https://example.invalid/e2e"])
        h.vmlab("template", "push", REF, REGISTRY, "--source", "https://example.invalid/e2e", timeout=900)
        assert other.wait(timeout=900) == 0, "the concurrent push of e2e-layered failed"
        for repo in ("e2e/e2e-alpine", "e2e/e2e-layered"):
            tags = h.run(["curl", "-sf", f"http://127.0.0.1:5000/v2/{repo}/tags/list"]).out
            assert "latest" in tags, f"{repo} has no latest tag: {tags}"
    h.check("template.registry.push", push_two_at_once, "two templates pushed concurrently")
    h.check(
        "template.registry.config",
        lambda: h.vmlab("template", "registry", "add", "localhost:5000/e2e")
        and "localhost:5000/e2e" in h.vmlab("template", "registry", "list").text,
    )
    h.check(
        "template.registry.search",
        lambda: "e2e-alpine" in h.vmlab("template", "search", "--registry", "localhost:5000/e2e", "--json").text,
    )
    # A tag from a retired versioning scheme that sorts above the version
    # `latest` names: search still shows what `latest` names.
    def search_follows_latest():
        repo = "http://127.0.0.1:5000/v2/e2e/e2e-alpine/manifests"
        index_type = "application/vnd.oci.image.index.v1+json"
        index = h.run(["curl", "-sf", "-H", f"Accept: {index_type}", f"{repo}/latest"]).out
        h.run(["curl", "-sf", "-X", "PUT", "-H", f"Content-Type: {index_type}",
               "--data-binary", "@-", f"{repo}/1.0.20260520"], input=index)
        rows = json.loads(h.vmlab("template", "search", "--registry", "localhost:5000/e2e", "e2e-alpine", "--json").out)
        row = next(r for r in rows if r["name"] == "e2e-alpine")
        assert row["version"] == "1.0.0", f"search showed {row['version']}, not latest's 1.0.0"
    h.check("template.registry.search-latest", search_follows_latest)
    h.check("template.registry.list-remote", lambda: h.vmlab("template", "list", "--remote").code == 0)
    rm_all(h, "e2e-alpine")
    h.check(
        "template.registry.pull",
        lambda: h.vmlab("template", "pull", f"{REGISTRY}:1.0.0", "--arch", "x86_64", timeout=900) and has(h, "e2e-alpine")
        # Again, with the same image already in the store: used, not refused.
        and h.vmlab("template", "pull", f"{REGISTRY}:1.0.0", "--arch", "x86_64", timeout=900),
    )
    h.vmlab("template", "registry", "remove", "localhost:5000/e2e", check=False)

    # A lab that names the registry copy: `pull`, then `up`.
    # The store still holds the version just pulled: the same image is used
    # as it stands rather than refused as already present.
    with h.lab("registry-ref") as lab:
        h.check("lab.pull", lambda: h.vmlab("pull", cwd=lab, timeout=900) and has(h, "e2e-alpine"))
        h.check(
            "template.registry.lab-ref",
            lambda: h.vmlab("up", cwd=lab, timeout=600)
            and h.wait_ready(lab, "vm01") is None
            and "e2e" in h.vmlab("exec", "vm01", "--", "hostname", cwd=lab).out,
        )
