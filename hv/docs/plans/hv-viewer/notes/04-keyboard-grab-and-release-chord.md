---
id: "04"
decision: D3
---

# Keyboard, grab and release chord

## Question
How are keys mapped and the grab released?

## Answer
Map evdev keycodes to the protocol's QEMU key names. Click grabs keyboard and pointer; a configurable release chord, default left Ctrl+Alt (as virt-viewer), shown in the title bar.

## Why
Familiar default, configurable for people whose guests need that chord.

## Consequences
Where the chord is configured (viewer flag and vmlab host config) is a spec detail.
