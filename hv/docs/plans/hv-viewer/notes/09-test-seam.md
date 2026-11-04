---
id: "09"
decision: D8
---

# Test seam

## Question
How is the viewer proven?

## Answer
A headless viewer core (protocol frames in, input out) tested against a fake display server; an S1 test that drives a real guest and checks frames and input round-trip; a manual checklist for the window itself.

## Why
The window cannot be asserted headlessly; the core and the protocol path can.

## Consequences
None.
