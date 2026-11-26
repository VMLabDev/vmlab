/*
 * a64lock: the QEMU TCG plugin of the aarch64 instruction lockstep
 * (phase 3d, DESIGN.md O3). It writes one binary record to `out` before each
 * instruction that the guest executes, one for each memory write of that
 * instruction, and one for each exception, interrupt and host call.
 *
 * This file is our own code. It includes QEMU's plugin API header
 * (qemu-plugin.h, the header for third-party plugins) and reads no other
 * QEMU source (DESIGN.md S6, X1). It began as measure/regtrace.c of the
 * design card.
 *
 * Arguments:
 *   out=<path>   where the records go: a FIFO that the follower reads.
 *   vectors=on   add V0 to V31, FPSR and FPCR to each instruction record.
 *
 * The stream, little-endian:
 *   header        "A64LOCK1", u32 flags (bit 0: vectors), u32 0
 *   instruction   u32 1, u32 encoding, u64 x[31], u64 sp, u64 pc, u64 cpsr
 *                 (pc is the address of the instruction, not the PC register)
 *                 with vectors: u8 v[32][16], u64 fpsr, u64 fpcr
 *   memory write  u32 2, u32 size in bytes, u64 vaddr, u64 low, u64 high
 *   discontinuity u32 3, u32 type (1 interrupt, 2 exception, 4 host call),
 *                 u64 from_pc, u64 to_pc
 *
 * Each record is one write(2) of less than PIPE_BUF bytes, so a reader of
 * the FIFO never sees a part of one, and a record is in the FIFO before the
 * guest goes on.
 */
#include <errno.h>
#include <fcntl.h>
#include <glib.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <qemu-plugin.h>

QEMU_PLUGIN_EXPORT int qemu_plugin_version = QEMU_PLUGIN_VERSION;

enum { REC_INSN = 1, REC_MEM = 2, REC_DISCON = 3 };

static int out_fd = -1;
static int vectors;
static struct qemu_plugin_register *reg_x[31], *reg_sp, *reg_cpsr;
static struct qemu_plugin_register *reg_v[32], *reg_fpsr, *reg_fpcr;
static GByteArray *scratch;
static int ready;
/* The address of the translation block that runs now. QEMU does not keep the
 * PC register current inside a block, so the PC of an instruction is this
 * plus its offset in the block. */
static uint64_t tb_base;

static void emit(const void *data, size_t len)
{
    const uint8_t *p = data;
    while (len) {
        ssize_t n = write(out_fd, p, len);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            /* The follower went away: there is nobody to trace for. */
            _exit(0);
        }
        p += n;
        len -= n;
    }
}

/* Append the low `want` bytes of a register, zero-extended. */
static void put_reg(GByteArray *rec, struct qemu_plugin_register *reg, guint want)
{
    uint8_t value[16] = { 0 };
    g_byte_array_set_size(scratch, 0);
    if (reg && qemu_plugin_read_register(reg, scratch)) {
        memcpy(value, scratch->data, MIN(scratch->len, want));
    }
    g_byte_array_append(rec, value, want);
}

static void insn_exec(unsigned int vcpu, void *udata)
{
    static GByteArray *rec;
    /* udata: the encoding in the low half, the offset in the block above. */
    uint64_t packed = (uint64_t)(uintptr_t)udata;
    uint32_t head[2] = { REC_INSN, (uint32_t)packed };
    uint64_t pc = tb_base + (packed >> 32);
    if (!ready || vcpu != 0) {
        return;
    }
    if (!rec) {
        rec = g_byte_array_new();
    }
    g_byte_array_set_size(rec, 0);
    g_byte_array_append(rec, (const uint8_t *)head, sizeof(head));
    for (int i = 0; i < 31; i++) {
        put_reg(rec, reg_x[i], 8);
    }
    put_reg(rec, reg_sp, 8);
    g_byte_array_append(rec, (const uint8_t *)&pc, 8);
    put_reg(rec, reg_cpsr, 8);
    if (vectors) {
        for (int i = 0; i < 32; i++) {
            put_reg(rec, reg_v[i], 16);
        }
        put_reg(rec, reg_fpsr, 8);
        put_reg(rec, reg_fpcr, 8);
    }
    emit(rec->data, rec->len);
}

static void mem_write(unsigned int vcpu, qemu_plugin_meminfo_t info, uint64_t vaddr, void *udata)
{
    struct {
        uint32_t tag, size;
        uint64_t vaddr, low, high;
    } rec = { REC_MEM, 1u << qemu_plugin_mem_size_shift(info), vaddr, 0, 0 };
    qemu_plugin_mem_value value = qemu_plugin_mem_get_value(info);
    if (!ready || vcpu != 0) {
        return;
    }
    switch (value.type) {
    case QEMU_PLUGIN_MEM_VALUE_U8:
        rec.low = value.data.u8;
        break;
    case QEMU_PLUGIN_MEM_VALUE_U16:
        rec.low = value.data.u16;
        break;
    case QEMU_PLUGIN_MEM_VALUE_U32:
        rec.low = value.data.u32;
        break;
    case QEMU_PLUGIN_MEM_VALUE_U64:
        rec.low = value.data.u64;
        break;
    case QEMU_PLUGIN_MEM_VALUE_U128:
        rec.low = value.data.u128.low;
        rec.high = value.data.u128.high;
        break;
    }
    emit(&rec, sizeof(rec));
}

static void discon(unsigned int vcpu, enum qemu_plugin_discon_type type,
                   uint64_t from_pc, uint64_t to_pc, void *udata)
{
    struct {
        uint32_t tag, type;
        uint64_t from_pc, to_pc;
    } rec = { REC_DISCON, (uint32_t)type, from_pc, to_pc };
    if (!ready || vcpu != 0) {
        return;
    }
    emit(&rec, sizeof(rec));
}

static void tb_exec(unsigned int vcpu, void *udata)
{
    if (vcpu == 0) {
        tb_base = (uint64_t)(uintptr_t)udata;
    }
}

static void tb_trans(struct qemu_plugin_tb *tb, void *udata)
{
    size_t n = qemu_plugin_tb_n_insns(tb);
    uint64_t base = qemu_plugin_tb_vaddr(tb);
    qemu_plugin_register_vcpu_tb_exec_cb(tb, tb_exec, QEMU_PLUGIN_CB_NO_REGS,
                                         (void *)(uintptr_t)base);
    for (size_t i = 0; i < n; i++) {
        struct qemu_plugin_insn *insn = qemu_plugin_tb_get_insn(tb, i);
        uint32_t encoding = 0;
        uint64_t offset = qemu_plugin_insn_vaddr(insn) - base;
        qemu_plugin_insn_data(insn, &encoding, sizeof(encoding));
        qemu_plugin_register_vcpu_insn_exec_cb(insn, insn_exec, QEMU_PLUGIN_CB_R_REGS,
                                               (void *)(uintptr_t)(encoding | offset << 32));
        qemu_plugin_register_vcpu_mem_cb(insn, mem_write, QEMU_PLUGIN_CB_NO_REGS,
                                         QEMU_PLUGIN_MEM_W, NULL);
    }
}

static void vcpu_init(unsigned int vcpu, void *udata)
{
    GArray *regs;
    int missing = 0;
    if (vcpu != 0 || ready) {
        return;
    }
    regs = qemu_plugin_get_registers();
    for (guint i = 0; regs && i < regs->len; i++) {
        qemu_plugin_reg_descriptor *r = &g_array_index(regs, qemu_plugin_reg_descriptor, i);
        const char *n = r->name;
        char *end;
        if (!strcmp(n, "sp")) {
            reg_sp = r->handle;
        } else if (!strcmp(n, "cpsr")) {
            reg_cpsr = r->handle;
        } else if (!strcmp(n, "fpsr")) {
            reg_fpsr = r->handle;
        } else if (!strcmp(n, "fpcr")) {
            reg_fpcr = r->handle;
        } else if ((n[0] == 'x' || n[0] == 'v') && g_ascii_isdigit(n[1])) {
            unsigned long k = strtoul(n + 1, &end, 10);
            if (*end == 0 && n[0] == 'x' && k < 31) {
                reg_x[k] = r->handle;
            } else if (*end == 0 && n[0] == 'v' && k < 32) {
                reg_v[k] = r->handle;
            }
        }
    }
    for (int i = 0; i < 31; i++) {
        missing += !reg_x[i];
    }
    missing += !reg_sp + !reg_cpsr;
    if (vectors) {
        for (int i = 0; i < 32; i++) {
            missing += !reg_v[i];
        }
        missing += !reg_fpsr + !reg_fpcr;
    }
    if (missing) {
        fprintf(stderr, "a64lock: the plugin API does not offer %d of the registers\n", missing);
        exit(1);
    }
    ready = 1;
}

QEMU_PLUGIN_EXPORT int qemu_plugin_install(qemu_plugin_id_t id, const qemu_info_t *info,
                                           int argc, char **argv)
{
    const char *path = NULL;
    struct {
        char magic[8];
        uint32_t flags, zero;
    } header = { { 'A', '6', '4', 'L', 'O', 'C', 'K', '1' }, 0, 0 };
    for (int i = 0; i < argc; i++) {
        if (g_str_has_prefix(argv[i], "out=")) {
            path = argv[i] + 4;
        } else if (!strcmp(argv[i], "vectors=on")) {
            vectors = 1;
        } else if (strcmp(argv[i], "vectors=off")) {
            fprintf(stderr, "a64lock: unknown argument %s\n", argv[i]);
            return -1;
        }
    }
    if (!path) {
        fprintf(stderr, "a64lock: give out=<path>\n");
        return -1;
    }
    out_fd = open(path, O_WRONLY);
    if (out_fd < 0) {
        fprintf(stderr, "a64lock: open %s: %s\n", path, strerror(errno));
        return -1;
    }
    scratch = g_byte_array_new();
    header.flags = vectors;
    emit(&header, sizeof(header));
    qemu_plugin_register_vcpu_init_cb(id, vcpu_init, NULL);
    qemu_plugin_register_vcpu_tb_trans_cb(id, tb_trans, NULL);
    qemu_plugin_register_vcpu_discon_cb(id, QEMU_PLUGIN_DISCON_ALL, discon, NULL);
    return 0;
}
