# D5 correction: QEMU serial transmission

Owner disposition, 2026-10-31: match measured QEMU behavior. The earlier
divisor-paced TX requirement came from an incorrect research premise.

## Evidence

The parent session ran `scripts/pc-uart-oracle.py` against QEMU 11.1.2 with
`pc,accel=qtest` and a connected UNIX socket chardev. The test used a 24-GiB
systemd scope with swap disabled and checked the shared host VM limit.
The committed observations are in `docs/hwref/pc-uart-socket.json` on
`wp/p4a-machine`. QEMU was used through qtest and its socket, without reading
implementation source.

With divisor 1 and 8N1, QEMU emitted all 20 bytes `A` through `T` before any
clock advancement after the writes. The initial virtual time was 1 ns.
LSR was 0x60. IIR reported 0xc2, then 0xc1 after acknowledgment. A file
backend produced the same immediate transmission in a separate observation.

When the socket blocked, LSR was 0x20 with one byte in the shift register.
After 17 more writes into a 16-byte FIFO, LSR was 0 and the first queued
marker was discarded. The last 16 markers arrived after the socket drained.
The observed 279 filler writes before blockage reflect host socket capacity,
not a UART constant.

RX preserved all 32 bytes across a full FIFO. Trigger levels 4, 8, and 14
raised IIR 0xc4 at the threshold. One byte below the trigger raised the timeout
interrupt 0xcc after four character times, 347224 ns at 115200 baud.
Non-FIFO input preserved all four bytes through host receive backpressure.

## Revised contract

Transmit immediately while the host backend accepts bytes. Preserve the shift
register when the backend blocks. Match QEMU FIFO overflow, LSR, THRE/TEMT,
interrupt acknowledgment, and receive backpressure. Use the programmed
divisor for receive timeout behavior. Preserve the socket connection during
reset and snapshot load.

For future UART changes, rerun the oracle and compare every observation above.
Treat host-dependent socket capacity separately from guest-visible FIFO state.
