# Match

_data type_

An image-match or OCR hit: bounding box, score, center point (cx/cy) and OCR text.

Returned by the image-matching and text-matching `Vm` methods (`wait_for_image`, `find_image`, `wait_for_text`, …).

```rust
struct Match { x: int, y: int, w: int, h: int, score: float,
               cx: int, cy: int,      // center — feed to mouse_move
               text: string }         // set by wait_for_text only
```

Feed `cx`/`cy` straight to `vm.mouse_move`. For `wait_for_text` hits only `text` and `score` are meaningful (the coordinates are 0).

## Related

- [Vm](../references/entity_vm_api.md)

- [Vm: screen, image matching & OCR methods](../references/fact_vm_vision.md)

[← Back to SKILL.md](../SKILL.md)
