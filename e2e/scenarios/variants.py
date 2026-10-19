"""Template variants (PRD §6.6): a definition with two variants builds the base
and both, a lab selects each by reference and is refused the unsealed base, and
`--variant` rebuilds one variant while the other stays byte for byte as it was."""

import json
import re
import shutil

from harness import E2E, WORK, ScenarioFailed
from scenarios.template import ensure_base, rm_all

NAME = "e2e-variants"
OWNED = ["template.variant.build", "template.variant.select", "template.variant.rebuild"]


def version_row(h) -> dict | None:
    rows = json.loads(h.vmlab("template", "list", "--json").out)
    return next((r for r in rows if r.get("name") == NAME and r.get("arch") == "x86_64"), None)


def variants(row: dict | None) -> dict[str, dict]:
    return {v["name"]: v for v in (row or {}).get("variants", [])}


def marker(h, lab, vm) -> str:
    return h.vmlab("exec", vm, "--", "cat", "/etc/e2e-variant", cwd=lab, timeout=60).out.strip()


def run(h):
    try:
        _run(h)
    finally:
        for f in OWNED:
            if f not in h.results:
                h.ok(f, False, "not reached: the scenario stopped before this step")


def _run(h):
    ensure_base(h)
    src = WORK / "template-variants"
    shutil.rmtree(src, ignore_errors=True)
    shutil.copytree(E2E / "templates" / "variants", src)

    rm_all(h, NAME)
    built = h.vmlab("template", "build", "--version", "1.0.0", cwd=src, timeout=1200, check=False)
    row = version_row(h)
    vs = variants(row)
    progress = all(f"building variant x86_64/{NAME}@1.0.0#{v}" in built.text for v in ("standard", "extra"))
    if not h.ok(
        "template.variant.build",
        built.code == 0
        and set(vs) == {"standard", "extra"}
        and vs["standard"]["default"]
        and not vs["extra"]["default"]
        and progress,
        f"exit {built.code}; variants {sorted(vs)}; progress named each: {progress}; {built.text.strip()[-300:]}",
    ):
        raise ScenarioFailed("the variant template did not build")
    extra_sha = vs["extra"]["sha256"]

    with h.lab("variants-lab") as lab:
        # The `#base` machine refuses the whole lab, by name, before anything
        # boots.
        refused = h.vmlab("up", cwd=lab, timeout=300, check=False)
        # miette wraps the message, so look for its parts, not the sentence.
        base_refused = refused.code != 0 and all(w in refused.text for w in ('"base"', "#base", "refused"))
        wcl = lab / "vmlab.wcl"
        wcl.write_text(re.sub(r'\n  vm "base" \{[^}]*\}\n', "\n", wcl.read_text()))
        h.vmlab("up", cwd=lab, timeout=600)
        h.wait_ready(lab, "plain", "extra")
        got = {vm: marker(h, lab, vm) for vm in ("plain", "extra")}
        h.ok(
            "template.variant.select",
            base_refused and got == {"plain": "standard", "extra": "extra"},
            f"#base refused: {base_refused}; markers {got}",
        )

    # Rebuild `standard` alone with a changed marker; `extra` is untouched.
    script = src / "scripts" / "mark-standard.ws"
    script.write_text(script.read_text().replace("echo standard >", "echo standard-v2 >"))
    rebuilt = h.vmlab("template", "build", NAME, "--variant", "standard", cwd=src, timeout=900, check=False)
    vs = variants(version_row(h))
    with h.lab("variants-lab", under="rebuild") as lab:
        wcl = lab / "vmlab.wcl"
        wcl.write_text(re.sub(r'\n  vm "base" \{[^}]*\}\n', "\n", wcl.read_text()))
        h.vmlab("up", cwd=lab, timeout=600)
        h.wait_ready(lab, "plain", "extra")
        got = {vm: marker(h, lab, vm) for vm in ("plain", "extra")}
    h.ok(
        "template.variant.rebuild",
        rebuilt.code == 0
        and got == {"plain": "standard-v2", "extra": "extra"}
        and vs.get("extra", {}).get("sha256") == extra_sha,
        f"exit {rebuilt.code}; markers {got}; extra unchanged: {vs.get('extra', {}).get('sha256') == extra_sha}",
    )
    rm_all(h, NAME)
