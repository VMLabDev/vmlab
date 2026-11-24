// Host test of the size check of the VIOGPU_SUBMIT_CMD escape of the kernel
// driver (card #752, finding F4 of docs/reviews/win3d-driver.md).
//
// It compiles viogpu/common/viogpu_submit_bounds.h, the header that
// patches/kmd/kmd-check-submit-escape-size.patch adds and that the driver
// uses for the check. scripts/test_kmd_submit_bounds.py takes the header out
// of the patch, so the test needs no driver source tree.
//
// With -DOLD_DRIVER the same checks run against the expression the driver
// had before the patch. They must fail there: that is the "before" of the
// proof. The exit status is the number of failed checks.
#include <cstdint>
#include <cstdio>

#ifdef OLD_DRIVER
// viogpu_adapter.cpp:1864 before the patch:
//   const UINT needed = (UINT)(sizeof(*req) + req->CmdSize);
//   if (pVioGpuEscape->DataLength < needed) return STATUS_INVALID_BUFFER_SIZE;
// The check before it has refused a DataLength below the header size.
static int VioGpuSubmitCmdFits(unsigned int data_length, unsigned int header_size, unsigned int cmd_size)
{
    if (data_length < header_size) return 0;
    const unsigned int needed = (unsigned int)(header_size + cmd_size);
    return !(data_length < needed);
}
#else
#include "viogpu_submit_bounds.h"
#endif

static int failed;

static void check(bool ok, const char *what)
{
    std::printf("%s %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) ++failed;
}

int main()
{
    const unsigned int header = 12;      // sizeof(VIOGPU_SUBMIT_CMD_REQ)
    const unsigned int longest = 0xFFFF; // DataLength is a USHORT

    // What a correct user sends.
    check(VioGpuSubmitCmdFits(12 + 16, header, 16), "a command of 16 bytes fits a payload of 28");
    check(VioGpuSubmitCmdFits(12 + 16, header, 8), "a command shorter than the payload fits");
    check(VioGpuSubmitCmdFits(12, header, 0), "the header alone fits (the driver refuses a size of 0 later)");
    check(VioGpuSubmitCmdFits(longest, header, longest - 12), "the longest command of a USHORT payload fits");
    check(!VioGpuSubmitCmdFits(12 + 16, header, 17), "a command one byte longer than the payload is refused");
    check(!VioGpuSubmitCmdFits(longest, header, longest - 11), "one byte more than the longest command is refused");
    check(!VioGpuSubmitCmdFits(11, header, 0), "a payload shorter than the header is refused");
    check(!VioGpuSubmitCmdFits(0, header, 0), "an empty payload is refused");

    // F4: the request of the review. 12 + 0xFFFFFFF8 is 4 in 32 bits.
    check(!VioGpuSubmitCmdFits(12, header, 0xFFFFFFF8u), "F4: CmdSize 0xFFFFFFF8 is refused with DataLength 12");
    check(!VioGpuSubmitCmdFits(12, header, 0xFFFFFFF4u), "F4: CmdSize 0xFFFFFFF4, the first that wraps, is refused");
    check(!VioGpuSubmitCmdFits(12, header, 0xFFFFFFFFu), "F4: CmdSize 0xFFFFFFFF is refused");
    check(!VioGpuSubmitCmdFits(longest, header, 0xFFFFFFF4u), "F4: CmdSize 0xFFFFFFF4 is refused with the longest payload");
    check(!VioGpuSubmitCmdFits(12, header, 0xFFFFFFF3u), "CmdSize 0xFFFFFFF3, the last that does not wrap, is refused");

    // For every payload length, no accepted command reaches past the payload.
    bool held = true;
    const unsigned int sizes[] = {0u, 1u, 16u, 0xFFF3u, 0xFFF4u, 0xFFFFu, 0x10000u, 0x7FFFFFFFu, 0x80000000u,
                                  0xFFFF0000u, 0xFFFFFFF3u, 0xFFFFFFF4u, 0xFFFFFFF8u, 0xFFFFFFFFu};
    for (unsigned int length = 0; length <= longest; ++length)
    {
        for (unsigned int size : sizes)
        {
            if (VioGpuSubmitCmdFits(length, header, size) && (unsigned long long)header + size > length) held = false;
        }
        // The commands that end in the last bytes of the payload, and the wrap of each length.
        for (unsigned int d = 0; d < 4; ++d)
        {
            const bool fits = length >= header + d;
            if (length >= header && (VioGpuSubmitCmdFits(length, header, length - header - d) != 0) != fits) held = false;
            if (VioGpuSubmitCmdFits(length, header, 0u - header + d)) held = false;
        }
    }
    check(held, "F4: for every payload length the accepted command ends inside the payload");

    std::printf("%d failed\n", failed);
    return failed > 100 ? 100 : failed;
}
