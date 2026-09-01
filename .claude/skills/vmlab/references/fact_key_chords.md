# Keyboard chord names

`vm.send_keys(chord)` takes `-`-separated, case-insensitive QMP key names with aliases:

```text
ctrl alt shift win/super/meta enter/return esc del space tab backspace
up down left right home end pgup/pageup pgdn/pagedown insert menu print
pause caps_lock num_lock scroll_lock f1..f12 a-z 0-9
```

Examples: `"ctrl-alt-del"`, `"enter"`, `"shift-f5"`. `vm.type_text` is US-ASCII only (`\n`→enter, `\t`→tab).

## Related

- [Machine](../references/entity_vm_api.md)

- [Machine: keyboard & mouse methods](../references/fact_vm_input.md)

[← Back to SKILL.md](../SKILL.md)
