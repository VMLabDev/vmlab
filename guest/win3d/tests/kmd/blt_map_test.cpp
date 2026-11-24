// Host test of where the kernel driver maps the surfaces of a Blt present
// copy (card #756, finding F8 of docs/reviews/win3d-driver.md).
//
// It compiles viogpu/viogpu3d/viogpu_blt_map.h, the header that
// patches/kmd/kmd-map-present-source-in-present.patch adds and that the
// driver uses for both decisions. scripts/test_kmd_blt_map.py takes the
// header out of the patch, so the test needs no driver source tree.
//
// With -DOLD_DRIVER the same checks run against the two expressions the
// driver had before the patch. They must fail there: that is the "before"
// of the proof. The exit status is the number of failed checks.
#include <cstdio>

#ifdef OLD_DRIVER
#define VIOGPU_BLT_BLOB_FLAG_MAPPABLE 0x0001u
#define VIOGPU_BLT_PRESENT_MAP_SOURCE 1
#define VIOGPU_BLT_PRESENT_MAP_BOTH 2
// viogpu_command.cpp:320-335 before the patch: VioGpuCpuCopyBltSupported
// took any blob source when the destination was not a blob.
static int VioGpuBltSurfacesSupported(int src_is_blob, unsigned int src_blob_flags, int dst_is_blob,
                                      unsigned int dst_blob_flags)
{
    if (!src_is_blob) return 0;
    if (!dst_is_blob) return 1;
    return (src_blob_flags & VIOGPU_BLT_BLOB_FLAG_MAPPABLE) && (dst_blob_flags & VIOGPU_BLT_BLOB_FLAG_MAPPABLE);
}
// viogpu_device.cpp:429-440 before the patch: Present mapped both surfaces
// under this condition and nothing otherwise (0), which left the mapping
// of the source blob to Patch.
static int VioGpuBltPresentMap(int dst_is_blob, int both_resident, int both_mapped)
{
    return (dst_is_blob || both_resident || both_mapped) ? VIOGPU_BLT_PRESENT_MAP_BOTH : 0;
}
#else
#include "viogpu_blt_map.h"
#endif

static int failed;

static void check(bool ok, const char *what)
{
    std::printf("%s %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) ++failed;
}

int main()
{
    const unsigned int M = VIOGPU_BLT_BLOB_FLAG_MAPPABLE;
    const int SOURCE = VIOGPU_BLT_PRESENT_MAP_SOURCE, BOTH = VIOGPU_BLT_PRESENT_MAP_BOTH;

    // What the user-mode drivers send.
    check(VioGpuBltSurfacesSupported(1, M, 0, 0), "a mappable Venus image is copied into a GDI surface");
    check(VioGpuBltSurfacesSupported(1, M | 2u, 0, 0), "other blob flags beside the mappable flag do not matter");
    check(VioGpuBltSurfacesSupported(1, M, 1, M), "Desktop Duplication: a mappable blob into a mappable blob");
    check(!VioGpuBltSurfacesSupported(0, 0, 0, 0), "a source that is no blob is refused");
    check(!VioGpuBltSurfacesSupported(0, M, 1, M), "a GDI surface into a blob is refused");
    check(!VioGpuBltSurfacesSupported(1, M, 1, 0), "a blob destination without the mappable flag is refused");
    check(!VioGpuBltSurfacesSupported(1, 0, 1, M), "a blob source without the mappable flag is refused for a blob destination");
    check(VioGpuBltPresentMap(0, 1, 0) == BOTH, "both allocations resident: Present maps both, Windows may skip Patch");
    check(VioGpuBltPresentMap(0, 0, 1) == BOTH, "both mapped already: Present maps both");
    check(VioGpuBltPresentMap(1, 0, 0) == BOTH, "a blob destination: Present maps both");

    // F8: the allocation of the review. BlobFlags = 0, a GDI destination.
    check(!VioGpuBltSurfacesSupported(1, 0, 0, 0), "F8: a blob source without the mappable flag is refused for a GDI destination");
    check(!VioGpuBltSurfacesSupported(1, 2u, 0, 0), "F8: a blob source with other flags only is refused");
    // F8: a mappable blob that the host refuses to create. The first
    // present of an allocation is not resident and has no mapping.
    check(VioGpuBltPresentMap(0, 0, 0) == SOURCE, "F8: a source that is not resident is mapped in Present, not in Patch");

    // For every staged copy, Present maps every blob of it: a blob's mapping
    // is what the calling program can make fail.
    bool held = true;
    for (int bits = 0; bits < 256; ++bits)
    {
        const int src_blob = bits & 1, dst_blob = bits >> 1 & 1, resident = bits >> 2 & 1, mapped = bits >> 3 & 1;
        const unsigned int src_flags = bits >> 4 & 3, dst_flags = bits >> 6 & 3;
        if (!VioGpuBltSurfacesSupported(src_blob, src_flags, dst_blob, dst_flags)) continue;
        const int map = VioGpuBltPresentMap(dst_blob, resident, mapped);
        const bool source_mapped = map == SOURCE || map == BOTH;
        const bool destination_mapped = map == BOTH;
        if (!src_blob || !(src_flags & M)) held = false;
        if (!source_mapped) held = false;
        if (dst_blob && !destination_mapped) held = false;
    }
    check(held, "F8: over every combination, a staged copy has a mappable blob source and Present maps each blob of it");

    return failed;
}
