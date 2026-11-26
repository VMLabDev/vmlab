// The entry of the flat aarch64 CoreMark image (bench/micro_a64.py).
//
// The image ends with the semihosting call SYS_EXIT: `hlt #0xf000` with
// W0 = 0x18 and X1 at a block of two words, the reason
// ADP_Stopped_ApplicationExit (0x20026) and the exit code. QEMU ends with
// that exit code (`-semihosting-config enable=on,target=native`). Our
// interpreter has no lifter for HLT and stops in front of it, and
// `a64_bench` reads the same block. X2 to X5 hold the three checksums of
// CoreMark and its error count at that moment.
//
// The image uses no system register, no FP and no vector register, and
// every access is aligned: the MMU is off, so memory is Device memory.
.section .text.start,"ax"
.global _start
_start:
    movz  x9, #0x40f0, lsl #16
    mov   sp, x9
    bl    main
    adrp  x9, bench_crc
    add   x9, x9, :lo12:bench_crc
    ldrh  w2, [x9]
    ldrh  w3, [x9, #2]
    ldrh  w4, [x9, #4]
    ldrh  w5, [x9, #6]
    movz  x10, #0x0026
    movk  x10, #0x2, lsl #16
    stp   x10, x0, [sp, #-16]!
    mov   x1, sp
    movz  w0, #0x18
    hlt   #0xf000
1:  b     1b
