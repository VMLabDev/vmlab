---
id: "06"
decision: D5
---

# Clipboard route

## Question
How does clipboard reach the guest?

## Answer
Through the lab daemon's machine.clipboard_get/set: host to guest on focus or paste, guest to host polled about once a second while the window has focus. An agent clipboard change notification is a later improvement.

## Why
Works today without touching the agent protocol or the new agent spec.

## Consequences
The viewer needs to reach the lab daemon (socket path and machine identity), which vmlab passes when it launches the viewer.
