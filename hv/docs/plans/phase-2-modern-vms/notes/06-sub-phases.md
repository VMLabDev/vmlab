---
id: "06"
decision: D3
---

# Sub-phase order

## Question
Order within phase 2.

## Answer
2a q35 platform + SeaBIOS booting a Linux template; 2b frames, RFB and input; 2c OVMF, pflash, SMM, secure boot, TPM, Hyper-V and Windows templates; 2d GPU (virgl, Venus); 2e in-process virtio-fs and audio wiring; 2f USB host and vfio.

## Why
Linux on SeaBIOS proves the platform with the fewest moving parts; frames early because later sub-phases debug through a screen. Recommended option, accepted.

## Consequences
- Each sub-phase needs its own acceptance list in the spec.
- vmlab's refusal list shrinks per sub-phase.
