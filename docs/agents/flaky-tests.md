# Flaky test quarantine

Quarantine is a report annotation, not a skip list. Run every test normally.
The owner card must fix or renew its entry by the end date. An expired entry
is not eligible for a flaky rerun exception.

| Test | Hypervisor | Owner card | End date (UTC) |
| --- | --- | --- | --- |
| template.variant.rebuild | QEMU | VML-127 | 2026-11-17 |
| lab.provision.parallel | QEMU | VML-128 | 2026-11-17 |
| vm.legacy.freedos | QEMU | VML-97 | 2026-11-17 |
| record.ocr | vmlab-hv | VML-129 | 2026-11-17 |

These failed the 0.45.2 full e2e run and passed their isolated reruns
(VML-111). `snapshot.vm.offline` is no longer quarantined: VML-130 fixed it.

## Merge gate

`just ci::check` never suppresses a failure or changes its exit status for
quarantine. Push the failing evidence as well as the isolated rerun. The
reviewer may accept a single failure only when the exact test and hypervisor
match an unexpired entry and the isolated rerun passes. A second failure
blocks the gate. Record the test, owner card, both results and the reason on
the card. Any rerun without a named flaky test needs a reason comment before
the rerun, not an unexplained retry until green.

## End-to-end report

Keep the original failed row in `results.json` and JUnit, including its
hypervisor and failure detail. In the release report, list the quarantined
failure with its owner and end date and link both the full run and isolated
rerun. Never rewrite a failed row as passed or skipped. A passing rerun is
separate evidence, not a replacement for the full run. Unlisted, expired or
repeated failures require a fix before acceptance.
