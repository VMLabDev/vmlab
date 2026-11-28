/* card-1242 measurement plugin: what a riscv64 guest executes.
 * Counts executed instructions per mnemonic (QEMU's disassembly, a black-box
 * output), per SYSTEM encoding (opcode 0x73, keyed without rd and rs1), per
 * length (16 or 32 bits), and keeps every distinct encoding translated.
 * Dumps on exit to out=<path>. From measure/a64trace.c of phase 3d. */
#include <glib.h>
#include <inttypes.h>
#include <stdio.h>
#include <string.h>
#include <qemu-plugin.h>

QEMU_PLUGIN_EXPORT int qemu_plugin_version = QEMU_PLUGIN_VERSION;

#define NSLOT 16384
static struct qemu_plugin_scoreboard *score;
static struct qemu_plugin_scoreboard *len16;
static GHashTable *slots;      /* key string -> slot index + 1 */
static GPtrArray *slot_names;
static GHashTable *encodings;  /* distinct 32-bit encodings */
static GMutex lock;
static char *out_path;
static uint64_t discon[3];

static unsigned slot_for(const char *key)
{
    gpointer v = g_hash_table_lookup(slots, key);
    if (v) {
        return GPOINTER_TO_UINT(v) - 1;
    }
    unsigned idx = slot_names->len;
    if (idx >= NSLOT - 1) {
        return NSLOT - 1; /* overflow slot */
    }
    char *k = g_strdup(key);
    g_ptr_array_add(slot_names, k);
    g_hash_table_insert(slots, k, GUINT_TO_POINTER(idx + 1));
    return idx;
}

static void tb_trans(struct qemu_plugin_tb *tb, void *udata)
{
    size_t n = qemu_plugin_tb_n_insns(tb);
    for (size_t i = 0; i < n; i++) {
        struct qemu_plugin_insn *insn = qemu_plugin_tb_get_insn(tb, i);
        uint32_t e = 0;
        size_t len = qemu_plugin_insn_size(insn);
        qemu_plugin_insn_data(insn, &e, len < sizeof(e) ? len : sizeof(e));
        char key[96];
        if (len == 4 && (e & 0x7f) == 0x73) {
            /* SYSTEM: CSR accesses keyed by funct3 and the CSR number,
             * the rest (ecall, sret, mret, wfi, sfence.vma) by funct12 */
            snprintf(key, sizeof(key), "sys:%08x", e & ~0x000f8f80u);
        } else {
            char *d = qemu_plugin_insn_disas(insn);
            size_t l = strcspn(d ? d : "?", " \t");
            if (l >= sizeof(key)) {
                l = sizeof(key) - 1;
            }
            memcpy(key, d ? d : "?", l);
            key[l] = 0;
            g_free(d);
        }
        g_mutex_lock(&lock);
        unsigned s = slot_for(key);
        g_hash_table_add(encodings, GUINT_TO_POINTER(e));
        g_mutex_unlock(&lock);
        qemu_plugin_u64 entry = { score, s * sizeof(uint64_t) };
        qemu_plugin_register_vcpu_insn_exec_inline_per_vcpu(
            insn, QEMU_PLUGIN_INLINE_ADD_U64, entry, 1);
        if (len == 2) {
            qemu_plugin_u64 short_entry = { len16, 0 };
            qemu_plugin_register_vcpu_insn_exec_inline_per_vcpu(
                insn, QEMU_PLUGIN_INLINE_ADD_U64, short_entry, 1);
        }
    }
}

static void on_discon(unsigned int vcpu, enum qemu_plugin_discon_type type,
                      uint64_t from, uint64_t to, void *udata)
{
    int i = type == QEMU_PLUGIN_DISCON_INTERRUPT ? 0 :
            type == QEMU_PLUGIN_DISCON_EXCEPTION ? 1 : 2;
    __atomic_fetch_add(&discon[i], 1, __ATOMIC_RELAXED);
}

static void at_exit(void *udata)
{
    FILE *f = fopen(out_path, "w");
    if (!f) {
        return;
    }
    int nv = qemu_plugin_num_vcpus();
    uint64_t total = 0;
    fprintf(f, "discon interrupt=%" PRIu64 " exception=%" PRIu64
            " hostcall=%" PRIu64 "\n", discon[0], discon[1], discon[2]);
    for (guint s = 0; s < slot_names->len; s++) {
        uint64_t c = 0;
        qemu_plugin_u64 entry = { score, s * sizeof(uint64_t) };
        for (int v = 0; v < nv; v++) {
            c += qemu_plugin_u64_get(entry, v);
        }
        total += c;
        fprintf(f, "count %s %" PRIu64 "\n",
                (char *)g_ptr_array_index(slot_names, s), c);
    }
    uint64_t shorts = 0;
    qemu_plugin_u64 short_entry = { len16, 0 };
    for (int v = 0; v < nv; v++) {
        shorts += qemu_plugin_u64_get(short_entry, v);
    }
    fprintf(f, "total %" PRIu64 " vcpus %d slots %u len16 %" PRIu64 "\n",
            total, nv, slot_names->len, shorts);
    GHashTableIter it;
    gpointer k;
    g_hash_table_iter_init(&it, encodings);
    while (g_hash_table_iter_next(&it, &k, NULL)) {
        fprintf(f, "enc %08x\n", GPOINTER_TO_UINT(k));
    }
    fclose(f);
}

QEMU_PLUGIN_EXPORT int qemu_plugin_install(qemu_plugin_id_t id,
                                           const qemu_info_t *info,
                                           int argc, char **argv)
{
    for (int i = 0; i < argc; i++) {
        if (g_str_has_prefix(argv[i], "out=")) {
            out_path = g_strdup(argv[i] + 4);
        }
    }
    if (!out_path) {
        return -1;
    }
    score = qemu_plugin_scoreboard_new(NSLOT * sizeof(uint64_t));
    len16 = qemu_plugin_scoreboard_new(sizeof(uint64_t));
    slots = g_hash_table_new(g_str_hash, g_str_equal);
    slot_names = g_ptr_array_new();
    encodings = g_hash_table_new(NULL, NULL);
    qemu_plugin_register_vcpu_tb_trans_cb(id, tb_trans, NULL);
    qemu_plugin_register_vcpu_discon_cb(id, QEMU_PLUGIN_DISCON_ALL, on_discon, NULL);
    qemu_plugin_register_atexit_cb(id, at_exit, NULL);
    return 0;
}
