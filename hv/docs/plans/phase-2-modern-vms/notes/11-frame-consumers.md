---
id: "11"
decision: D8
---

# vmlab's frame consumers

## Question
What do vmlab's screenshot, vision, recorder and input use on vmlab-hv machines?

## Answer
A native frame client in vmlab through new Frames, streaming and Input capabilities on the seam; the hypervisor's RFB server serves human viewers only.

## Why
Removes the GL-screenshot fallback and the RFB hop (HV-D21). Recommended option, accepted.

## Consequences
- A vmlab PR adds the streaming frame capability and a native client, with the recorder consuming it.
