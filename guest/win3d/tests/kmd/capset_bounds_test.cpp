// Host test of the capset bounds of the kernel driver (card #751, findings
// F1, F3 and F6 of docs/reviews/win3d-driver.md).
//
// It compiles viogpu/common/viogpu_capset_bounds.h, the header that
// patches/kmd/kmd-check-capset-escape.patch adds and that the driver uses for
// every bound of the capset path. scripts/test_kmd_capset_bounds.py takes the
// header out of the patch, so the test needs no driver source tree.
//
// With -DOLD_DRIVER the same checks run against the expressions the driver
// had before the patch. They must fail there: that is the "before" of the
// proof. The exit status is the number of failed checks.
#include <cstdint>
#include <cstdio>

#ifdef OLD_DRIVER
// viogpu_adapter.cpp:1572 before the patch. The shift count is taken modulo
// 64, as the x64 shift instruction does, to keep this program defined.
static int VioGpuCapsetIdSupported(unsigned long long supported, unsigned int id)
{
    return (supported & (1ull << (id & 63))) != 0;
}
// viogpu_queue.cpp:777 before the patch: `int resp_size = sizeof(GPU_RESP_CAPSET) + capset_size`.
static unsigned long long VioGpuCapsetResponseSize(unsigned int header_size, unsigned int capset_size)
{
    return (unsigned int)(header_size + capset_size);
}
// viogpu_adapter.cpp:1598 before the patch: `min(Capset.Size, max_size)`.
static unsigned int VioGpuCapsetCopySize(unsigned int user_size, unsigned int capset_size)
{
    return user_size < capset_size ? user_size : capset_size;
}
// viogpu_adapter.cpp:4181 before the patch stored every size.
static int VioGpuCapsetSizeAccepted(unsigned int) { return 1; }
#define VIOGPU_CAPSET_ID_LIMIT 64u
#else
#include "viogpu_capset_bounds.h"
#endif

static int failed;

static void check(bool ok, const char *what)
{
    std::printf("%s %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) ++failed;
}

int main()
{
    const unsigned int header = 25; // sizeof(GPU_RESP_CAPSET): a 24-byte header and capset_data[1]
    const unsigned long long supported = (1ull << 1) | (1ull << 2) | (1ull << 4); // virgl, virgl2, venus

    // The ids a correct user sends.
    check(VioGpuCapsetIdSupported(supported, 1), "id 1 is supported");
    check(VioGpuCapsetIdSupported(supported, 4), "id 4 is supported");
    check(!VioGpuCapsetIdSupported(supported, 3), "id 3 is not in the mask");
    check(!VioGpuCapsetIdSupported(supported, 0), "id 0 is never supported");
    check(VioGpuCapsetIdSupported(~0ull, 63), "id 63 is the last id");

    // F3: an id of 64 + k must not pass as capset k.
    check(!VioGpuCapsetIdSupported(supported, 64 + 4), "F3: id 68 is refused");
    check(!VioGpuCapsetIdSupported(supported, 0xFFFFFFC0u + 4), "F3: id 0xFFFFFFC4 is refused");
    check(!VioGpuCapsetIdSupported(~0ull, 64), "F3: id 64 is refused with every capset supported");
    check(!VioGpuCapsetIdSupported(~0ull, 0xFFFFFFFFu), "F3: id 0xFFFFFFFF is refused");
    for (unsigned int id = 0; id < 4096; ++id)
    {
        if (VioGpuCapsetIdSupported(~0ull, id) && id >= VIOGPU_CAPSET_ID_LIMIT)
        {
            check(false, "F3: an accepted id indexes past the 64 entries of m_capsetInfos");
            break;
        }
    }

    // F6: the forged host answer of the review, capset_max_size = 0xFFFFFFF0.
    const unsigned int forged = 0xFFFFFFF0u;
    check(!VioGpuCapsetSizeAccepted(forged), "F6: a capset of 0xFFFFFFF0 bytes is refused at negotiation");
    const unsigned long long response = VioGpuCapsetResponseSize(header, forged);
    check(response == 0 || response >= (unsigned long long)header + forged,
          "F6: the response buffer is refused or holds the header and the capset");
    const unsigned int copied = VioGpuCapsetCopySize(65536, forged);
    check(copied == 0 || header + (unsigned long long)copied <= response,
          "F6: the escape copies no byte from behind the response buffer");
    check(!VioGpuCapsetSizeAccepted(0xFFFFFFFFu), "F6: a capset of 0xFFFFFFFF bytes is refused");
    check(VioGpuCapsetCopySize(65536, 0xFFFFFFFFu) == 0, "F6: nothing is copied for a refused size");

    // For every size the response holds what the escape copies.
    const unsigned int sizes[] = {0u, 1u, 308u, 1400u, 65536u, 1u << 20, (1u << 20) + 1, 0x7FFFFFFFu,
                                  0x80000000u, 0xFFFFFFE6u, 0xFFFFFFE7u, 0xFFFFFFF0u, 0xFFFFFFFFu};
    bool held = true;
    for (unsigned int size : sizes)
    {
        const unsigned long long r = VioGpuCapsetResponseSize(header, size);
        const unsigned int users[] = {0u, 1u, 65536u, 0xFFFFFFFFu};
        for (unsigned int user : users)
        {
            const unsigned int c = VioGpuCapsetCopySize(user, size);
            if (c != 0 && header + (unsigned long long)c > r) held = false;
            if (c > user) held = false;
        }
    }
    check(held, "F6: for every host size the copy stays inside the response buffer");

    // The sizes a correct host sends still work.
    check(VioGpuCapsetSizeAccepted(1400), "a capset of 1400 bytes is accepted");
    check(VioGpuCapsetResponseSize(header, 1400) == 1425, "its response is the header and 1400 bytes");
    check(VioGpuCapsetCopySize(65536, 1400) == 1400, "a large user buffer gets the whole capset");
    check(VioGpuCapsetCopySize(100, 1400) == 100, "a small user buffer gets its own size");
    check(VioGpuCapsetCopySize(0, 1400) == 0, "an empty user buffer gets nothing");

    std::printf("%d failed\n", failed);
    return failed > 100 ? 100 : failed;
}
