// Host test of who may change the scanout in the kernel driver (card #755,
// finding F7 of docs/reviews/win3d-driver.md).
//
// It compiles viogpu/common/viogpu_scanout_owner.h, the header that
// patches/kmd/kmd-scanout-only-for-its-owner.patch adds and that the driver
// uses for both decisions. scripts/test_kmd_scanout_owner.py takes the
// header out of the patch, so the test needs no driver source tree.
//
// With -DOLD_DRIVER the same checks run against what the driver did before
// the patch: every command type passed Render, and the escape was served
// for every caller. They must fail there: that is the "before" of the
// proof. The exit status is the number of failed checks.
#include <cstdio>

#ifdef OLD_DRIVER
struct VIOGPU_PROCESS_IDENTITY
{
    unsigned long long pid;
    long long create_time;
};
enum
{
    VIOGPU_SCANOUT_ESCAPE_ALLOWED = 0,
    VIOGPU_SCANOUT_ESCAPE_NO_SUCH_SCANOUT,
    VIOGPU_SCANOUT_ESCAPE_NOT_THE_CREATOR,
    VIOGPU_SCANOUT_ESCAPE_NOT_THE_OWNER,
};
// viogpu_device.cpp:595 before the patch checked the size and the format of
// a flip command in the stream of a program, and let it through.
static int VioGpuUserStreamCommandAllowed(unsigned int) { return 1; }
// viogpu_adapter.cpp:1795 before the patch looked up the allocation and
// called EscapeResourceSetScanoutBlob for it.
static int VioGpuScanoutEscapeCheck(unsigned int, const VIOGPU_PROCESS_IDENTITY *, const VIOGPU_PROCESS_IDENTITY *,
                                    const VIOGPU_PROCESS_IDENTITY *)
{
    return VIOGPU_SCANOUT_ESCAPE_ALLOWED;
}
#else
#include "viogpu_scanout_owner.h"
#endif

static int failed;

static void check(bool ok, const char *what)
{
    std::printf("%s %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) ++failed;
}

int main()
{
    // viogpu/shared/viogpum.h
    enum { NOP = 0, SUBMIT = 1, TRANSFER_TO_HOST = 2, TRANSFER_FROM_HOST = 3, PRESENT_FLIP = 4, SUBMIT_TIMELINE = 5 };

    check(VioGpuUserStreamCommandAllowed(NOP) && VioGpuUserStreamCommandAllowed(SUBMIT) &&
              VioGpuUserStreamCommandAllowed(TRANSFER_TO_HOST) && VioGpuUserStreamCommandAllowed(TRANSFER_FROM_HOST) &&
              VioGpuUserStreamCommandAllowed(SUBMIT_TIMELINE),
          "the five commands of the user-mode driver pass Render");
    check(!VioGpuUserStreamCommandAllowed(PRESENT_FLIP), "F7: a flip command in the stream of a program is refused");

    const VIOGPU_PROCESS_IDENTITY none = {0, 0};
    const VIOGPU_PROCESS_IDENTITY dwm = {1200, 1000};
    const VIOGPU_PROCESS_IDENTITY game = {4321, 5000};
    const VIOGPU_PROCESS_IDENTITY probe = {7000, 9000};
    // The id of the game, given to a new process after the game ended.
    const VIOGPU_PROCESS_IDENTITY reused = {4321, 9500};

    check(VioGpuScanoutEscapeCheck(0, &game, &game, &game) == VIOGPU_SCANOUT_ESCAPE_ALLOWED,
          "the fullscreen application that Windows gave the scanout shows its own allocation");
    check(VioGpuScanoutEscapeCheck(0, &probe, &probe, &dwm) == VIOGPU_SCANOUT_ESCAPE_NOT_THE_OWNER,
          "F7: a program shows no allocation of its own while the desktop has the scanout");
    check(VioGpuScanoutEscapeCheck(0, &game, &game, &dwm) == VIOGPU_SCANOUT_ESCAPE_NOT_THE_OWNER,
          "F7: the application is refused after Windows took the scanout back");
    check(VioGpuScanoutEscapeCheck(0, &probe, &dwm, &dwm) == VIOGPU_SCANOUT_ESCAPE_NOT_THE_CREATOR,
          "F7: a program shows no allocation that it opened and another process made");
    check(VioGpuScanoutEscapeCheck(0, &probe, &game, &game) == VIOGPU_SCANOUT_ESCAPE_NOT_THE_CREATOR,
          "F7: a program shows no allocation of the application that has the scanout");
    check(VioGpuScanoutEscapeCheck(0, &probe, &probe, &none) == VIOGPU_SCANOUT_ESCAPE_NOT_THE_OWNER,
          "F7: a program is refused while Windows has set no source");
    check(VioGpuScanoutEscapeCheck(0, &none, &none, &none) != VIOGPU_SCANOUT_ESCAPE_ALLOWED,
          "F7: no process is not a process that equals itself");
    check(VioGpuScanoutEscapeCheck(0, &reused, &reused, &game) == VIOGPU_SCANOUT_ESCAPE_NOT_THE_OWNER,
          "F7: a new process with the id of the application is not the application");
    check(VioGpuScanoutEscapeCheck(1, &game, &game, &game) == VIOGPU_SCANOUT_ESCAPE_NO_SUCH_SCANOUT,
          "F7: a scanout id that is not 0 is refused for the owner too");
    check(VioGpuScanoutEscapeCheck(0xffffffffu, &game, &game, &game) == VIOGPU_SCANOUT_ESCAPE_NO_SUCH_SCANOUT,
          "F7: scanout id 0xFFFFFFFF is refused");

    std::printf("%d failed\n", failed);
    return failed;
}
