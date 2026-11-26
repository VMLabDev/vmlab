/* card-1032 measurement plugin: the cost and the determinism of a
 * per-instruction register trace out of QEMU.
 * args: mode=pchash|gpr|all  limit=<insns>  ck=<insns between checkpoints>
 *       out=<file for raw records, optional>  */
#include <glib.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <qemu-plugin.h>

QEMU_PLUGIN_EXPORT int qemu_plugin_version = QEMU_PLUGIN_VERSION;

static int mode;                 /* 0 pchash, 1 gpr, 2 all */
static uint64_t limit = 100000000, ck = 10000000, next_ck;
static uint64_t count, hash = 1469598103934665603ull;
static FILE *out;
static GPtrArray *handles;
static GByteArray *buf;
static struct timespec t0;
static int started;
static uint64_t bytes;

static double now(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (t.tv_sec - t0.tv_sec) + (t.tv_nsec - t0.tv_nsec) / 1e9;
}

static inline void mix(uint64_t v)
{
    hash = (hash ^ v) * 1099511628211ull;
}

static void check(void)
{
    if (count >= next_ck) {
        fprintf(stderr, "ck %" PRIu64 " %016" PRIx64 " t=%.3f\n", count, hash, now());
        next_ck += ck;
    }
    if (count >= limit) {
        double t = now();
        fprintf(stderr, "done insns=%" PRIu64 " seconds=%.3f rate=%.0f/s bytes=%" PRIu64 " hash=%016" PRIx64 "\n",
                count, t, count / t, bytes, hash);
        if (out) {
            fclose(out);
        }
        fflush(stderr);
        _exit(0);
    }
}

static void tb_exec(unsigned int vcpu, void *udata)
{
    uint64_t v = (uint64_t)(uintptr_t)udata; /* pc ^ n<<56 packed below */
    if (!started) {
        clock_gettime(CLOCK_MONOTONIC, &t0);
        started = 1;
    }
    mix(v & 0x00ffffffffffffffull);
    count += v >> 56;
    check();
}

static void insn_exec(unsigned int vcpu, void *udata)
{
    if (!started) {
        clock_gettime(CLOCK_MONOTONIC, &t0);
        started = 1;
    }
    g_byte_array_set_size(buf, 0);
    for (guint i = 0; i < handles->len; i++) {
        qemu_plugin_read_register(g_ptr_array_index(handles, i), buf);
    }
    for (guint i = 0; i + 8 <= buf->len; i += 8) {
        uint64_t v;
        memcpy(&v, buf->data + i, 8);
        mix(v);
    }
    bytes += buf->len;
    if (out) {
        fwrite(buf->data, 1, buf->len, out);
    }
    count++;
    check();
}

static void tb_trans(struct qemu_plugin_tb *tb, void *udata)
{
    size_t n = qemu_plugin_tb_n_insns(tb);
    if (mode == 0) {
        uint64_t v = (qemu_plugin_tb_vaddr(tb) & 0x00ffffffffffffffull) | ((uint64_t)n << 56);
        qemu_plugin_register_vcpu_tb_exec_cb(tb, tb_exec, QEMU_PLUGIN_CB_NO_REGS,
                                             (void *)(uintptr_t)v);
        return;
    }
    for (size_t i = 0; i < n; i++) {
        qemu_plugin_register_vcpu_insn_exec_cb(qemu_plugin_tb_get_insn(tb, i), insn_exec,
                                               QEMU_PLUGIN_CB_R_REGS, NULL);
    }
}

static int wanted(const char *n)
{
    if (!strcmp(n, "sp") || !strcmp(n, "pc") || !strcmp(n, "cpsr")) {
        return 1;
    }
    if (n[0] == 'x' && g_ascii_isdigit(n[1])) {
        return 1;
    }
    if (mode == 2) {
        if (!strcmp(n, "fpsr") || !strcmp(n, "fpcr")) {
            return 1;
        }
        if (n[0] == 'v' && g_ascii_isdigit(n[1])) {
            return 1;
        }
    }
    return 0;
}

static void vcpu_init(unsigned int vcpu, void *udata)
{
    if (vcpu != 0 || handles->len) {
        return;
    }
    GArray *regs = qemu_plugin_get_registers();
    GString *all = g_string_new("");
    for (guint i = 0; regs && i < regs->len; i++) {
        qemu_plugin_reg_descriptor *r = &g_array_index(regs, qemu_plugin_reg_descriptor, i);
        g_string_append_printf(all, "%s ", r->name);
        if (wanted(r->name)) {
            g_ptr_array_add(handles, r->handle);
        }
    }
    fprintf(stderr, "registers the plugin API offers: %u; traced: %u\n", regs ? regs->len : 0, handles->len);
    if (getenv("REGTRACE_LIST")) {
        fprintf(stderr, "names: %s\n", all->str);
    }
    g_string_free(all, TRUE);
}

QEMU_PLUGIN_EXPORT int qemu_plugin_install(qemu_plugin_id_t id, const qemu_info_t *info,
                                           int argc, char **argv)
{
    for (int i = 0; i < argc; i++) {
        if (g_str_has_prefix(argv[i], "mode=")) {
            const char *m = argv[i] + 5;
            mode = !strcmp(m, "gpr") ? 1 : !strcmp(m, "all") ? 2 : 0;
        } else if (g_str_has_prefix(argv[i], "limit=")) {
            limit = g_ascii_strtoull(argv[i] + 6, NULL, 10);
        } else if (g_str_has_prefix(argv[i], "ck=")) {
            ck = g_ascii_strtoull(argv[i] + 3, NULL, 10);
        } else if (g_str_has_prefix(argv[i], "out=")) {
            out = fopen(argv[i] + 4, "wb");
        }
    }
    next_ck = ck;
    handles = g_ptr_array_new();
    buf = g_byte_array_new();
    qemu_plugin_register_vcpu_init_cb(id, vcpu_init, NULL);
    qemu_plugin_register_vcpu_tb_trans_cb(id, tb_trans, NULL);
    return 0;
}
