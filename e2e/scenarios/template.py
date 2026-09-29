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
    h.check("template.registry.push", lambda: h.vmlab("template", "push", REF, REGISTRY, "--source", "https://example.invalid/e2e", timeout=900))
    h.check(
        "template.registry.config",
        lambda: h.vmlab("template", "registry", "add", "localhost:5000/e2e")
        and "localhost:5000/e2e" in h.vmlab("template", "registry", "list").text,
    )
    h.check(
        "template.registry.search",
        lambda: "e2e-alpine" in h.vmlab("template", "search", "--registry", "localhost:5000/e2e", "--json").text,
    )
    h.check("template.registry.list-remote", lambda: h.vmlab("template", "list", "--remote").code == 0)
    rm_all(h, "e2e-alpine")
    h.check(
        "template.registry.pull",
        lambda: h.vmlab("template", "pull", f"{REGISTRY}:1.0.0", "--arch", "x86_64", timeout=900) and has(h, "e2e-alpine"),
    )
    h.vmlab("template", "registry", "remove", "localhost:5000/e2e", check=False)

    # A lab that names the registry copy: `pull`, then `up`.
    with h.lab("registry-ref") as lab:
        rm_all(h, "e2e-alpine")
        h.check("lab.pull", lambda: h.vmlab("pull", cwd=lab, timeout=900) and has(h, "e2e-alpine"))
        h.check(
            "template.registry.lab-ref",
            lambda: h.vmlab("up", cwd=lab, timeout=600)
            and h.wait_ready(lab, "vm01") is None
            and "e2e" in h.vmlab("exec", "vm01", "--", "hostname", cwd=lab).out,
        )
