---
id: "05"
decision: D4
---

# Resize

## Question
How does the guest follow the window size?

## Answer
Send RequestSize after a 200 ms debounce while the window is dragged; std VGA guests, which ignore RequestSize, are scaled into the window with letterboxing.

## Why
Avoids flooding the guest with mode changes; std VGA cannot resize.

## Consequences
None.
