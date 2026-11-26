// The scalar loop as a flat image (bench/micro_a64.py): the three
// instructions of `a64_mips` (card C1), LOOP_ITERATIONS times.
//
// The stub is five instructions and the exit is nine up to the `hlt`, so an
// emulator that stops in front of the `hlt` has run 3 * N + 14
// instructions. The exit code is 0 when X0 counted to N. With
// LOOP_ITERATIONS=1 the image is the startup baseline of QEMU.
// LOOP_ITERATIONS is 1 to 2^32 - 1. How the image ends is in start.S.
.section .text.start,"ax"
.global _start
_start:
    movz  x9, #0x40f0, lsl #16
    mov   sp, x9
    movz  x1, #(LOOP_ITERATIONS & 0xffff)
    movk  x1, #((LOOP_ITERATIONS >> 16) & 0xffff), lsl #16
    movz  x0, #0
0:  .word 0x91000400          // add  x0, x0, #1
    .word 0xf1000421          // subs x1, x1, #1
    .word 0x54ffffc1          // b.ne 0b
    movz  x3, #(LOOP_ITERATIONS & 0xffff)
    movk  x3, #((LOOP_ITERATIONS >> 16) & 0xffff), lsl #16
    subs  x3, x0, x3
    cset  x3, ne
    movz  x10, #0x0026
    movk  x10, #0x2, lsl #16
    stp   x10, x3, [sp, #-16]!
    mov   x1, sp
    movz  w0, #0x18
    hlt   #0xf000
1:  b     1b
