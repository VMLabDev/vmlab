# Machine: keyboard & mouse methods

| Method | Returns | Notes |
| --- | --- | --- |
| `m.send_keys(chord: string)` | `Result[unit, string]` | e.g. `"ctrl-alt-del"`, `"enter"`, `"shift-f5"` |
| `m.type_text(text: string)` | `Result[unit, string]` | ~35 ms/key; `\n`→enter, `\t`→tab; **US ASCII only** |
| `m.type_text_paced(text: string, delay_ms: int)` | `Result[unit, string]` | Custom inter-key delay |
| `m.mouse_move(x: int, y: int)` | `Result[unit, string]` | Absolute, scaled to current screen size |
| `m.mouse_click(button: string)` | `Result[unit, string]` | `"left"` / `"right"` / `"middle"` |
| `m.mouse_drag(x1, y1, x2, y2: int)` | `Result[unit, string]` | Human-ish 8-step drag |

See [the chord-key reference](../references/fact_key_chords.md) for the full key-name vocabulary.

These are the **Display** capability, and they are on every machine. A machine
that reports no framebuffer fails the call with \*"machine `web` has no
display"\* — naming what this machine does not offer, not what its kind could
never have. No container reports a display today; one running a display server
would, and this page would need no edit.


## Related

- [Machine](../references/entity_vm_api.md)

- [Keyboard chord names](../references/fact_key_chords.md)

[← Back to SKILL.md](../SKILL.md)
