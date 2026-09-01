# Machine: screen, image matching & OCR methods

| Method | Returns | Notes |
| --- | --- | --- |
| `m.screenshot(path: string)` | `Result[string, string]` | Returns saved path. `""` → auto-named in `.vmlab/screenshots/` |
| `m.wait_for_image(image: string, timeout_secs: int)` | `Result[Match, string]` | Threshold 0.9, polls every 1 s |
| `m.wait_for_image_opts(image, timeout_secs, threshold: float, region: List[int])` | `Result[Match, string]` | `region` = `[x, y, w, h]` or `[]` |
| `m.wait_for_any(images: List[string], timeout_secs: int)` | `Result[Match, string]` | First image to appear wins |
| `m.find_image(image: string)` | `Result[Option[Match], string]` | Single shot, no waiting |
| `m.ocr()` | `Result[string, string]` | Tesseract over the whole screen |
| `m.ocr_region(region: List[int])` | `Result[string, string]` | Cropped `[x, y, w, h]` |
| `m.wait_for_text(pattern: string, timeout_secs: int)` | `Result[Match, string]` | Regex over OCR text; only `m.text`/`m.score` meaningful (coords are 0) |

Reference images resolve relative to the **lab root**; convention is an `images/` folder beside `vmlab.wcl`. Matching is normalized cross-correlation on grayscale. Hits come back as a [Match](../references/entity_match_type.md).

These are the **Display** capability, and they are on every machine. A machine
that reports no framebuffer fails the call with \*"machine `web` has no
display"\* — naming what this machine does not offer, not what its kind could
never have. No container reports a display today; one running a display server
would, and this page would need no edit.


## Related

- [Machine](../references/entity_vm_api.md)

- [Match](../references/entity_match_type.md)

[← Back to SKILL.md](../SKILL.md)
