/* libcsyms: the names of the glibc functions that glibc does not export
 * (card #1212, P3).
 *
 * glibc picks the memmove, memset and the other string functions of a CPU
 * at load (IFUNC), and exports only the selector: the function that runs,
 * `__memmove_avx_unaligned_erms` for example, has no symbol in a stripped
 * libc. dlsym of the exported name runs the selector and returns the
 * function that this CPU gets, the one a process on the same host calls.
 * Each line is "<address in the file, hex> <exported name>", the address
 * as ipstack.py resolves a sample in libc (the virtual address in the
 * ELF file), for `ipstack.py --libc-names`.
 *
 * Build: cc -O2 -o libcsyms libcsyms.c
 * Use:   libcsyms > libc-names.txt
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <link.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static const char *const NAMES[] = {
    "memmove", "memcpy", "mempcpy", "memset", "memcmp", "bcmp", "memchr",
    "memrchr", "rawmemchr", "strlen", "strnlen", "strcmp", "strncmp",
    "strchr", "strchrnul", "strrchr", "strcpy", "stpcpy", "strncpy",
    "strcat", "strstr", "strcasecmp", "wmemset", "wcslen", "wcsnlen",
    "__memcpy_chk", "__memmove_chk", "__memset_chk",
};

int main(void) {
    for (size_t i = 0; i < sizeof(NAMES) / sizeof(NAMES[0]); i++) {
        void *at = dlsym(RTLD_DEFAULT, NAMES[i]);
        Dl_info info;
        struct link_map *map = NULL;
        if (!at || !dladdr1(at, &info, (void **)&map, RTLD_DL_LINKMAP) || !map) continue;
        if (!strstr(info.dli_fname, "libc.so")) continue;
        /* l_addr is the load bias: the address in the file is the address
         * less the bias. */
        printf("%lx %s\n", (unsigned long)((uintptr_t)at - map->l_addr), NAMES[i]);
    }
    return 0;
}
