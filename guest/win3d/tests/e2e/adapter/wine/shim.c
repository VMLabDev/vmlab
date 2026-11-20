/* The four functions wine/debug.h imports from Wine's ntdll, for a build of
 * Wine's tests that runs on Windows (tests/e2e/adapter/build.sh). The tests
 * use them only to format strings for their messages. */
#include <stdio.h>
#include <string.h>

static __thread char ring[32][600];
static __thread unsigned ring_pos;

const char *__cdecl __wine_dbg_strdup(const char *str)
{
    char *b = ring[ring_pos++ % 32];
    strncpy(b, str, sizeof ring[0] - 1);
    b[sizeof ring[0] - 1] = 0;
    return b;
}

int __cdecl __wine_dbg_output(const char *str)
{
    return fputs(str, stderr);
}

int __cdecl __wine_dbg_header(int cls, void *channel, const char *function)
{
    return -1;
}

unsigned char __cdecl __wine_dbg_get_channel_flags(void *channel)
{
    return 0;
}
