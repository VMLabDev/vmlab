#include "coremark.h"

/* The unchanged upstream iterate() and all three algorithms on the seeds of
 * a performance run (0, 0, 0x66) and 2000 bytes. CoreMark's own checksums for
 * that run are 0xe714 (list), 0x1fd7 (matrix) and 0x8e3a (state). The exit
 * code says which of them is wrong. This is a checked instruction mix, not a
 * reportable CoreMark score. bench_crc holds what the run computed, and
 * start.S puts it into X2 to X5. */
static ee_u8 memory[2000] __attribute__((aligned(16)));
volatile ee_s16 bench_seed1 = 0;
volatile ee_s16 bench_seed2 = 0;
volatile ee_s16 bench_seed3 = 0x66;
ee_u16 bench_crc[4];

int main(void) {
    core_results result = {0};
    result.seed1 = bench_seed1;
    result.seed2 = bench_seed2;
    result.seed3 = bench_seed3;
    result.size = sizeof(memory) / NUM_ALGORITHMS;
    result.iterations = ITERATIONS;
    result.execs = ALL_ALGORITHMS_MASK;
    result.memblock[0] = memory;
    for (unsigned i = 0; i < NUM_ALGORITHMS; ++i)
        result.memblock[i + 1] = memory + i * result.size;
    result.list = core_list_init(result.size, result.memblock[1], result.seed1);
    core_init_matrix(result.size, result.memblock[2],
                     (ee_s32)result.seed1 | ((ee_s32)result.seed2 << 16), &result.mat);
    core_init_state(result.size, result.seed1, result.memblock[3]);
    iterate(&result);
    bench_crc[0] = result.crclist;
    bench_crc[1] = result.crcmatrix;
    bench_crc[2] = result.crcstate;
    bench_crc[3] = (ee_u16)result.err;
    return (result.crclist != 0xe714) | (result.crcmatrix != 0x1fd7) << 1
        | (result.crcstate != 0x8e3a) << 2 | (result.err != 0) << 3;
}

void *memset(void *destination, int value, size_t count) {
    unsigned char *p = destination;
    for (size_t i = 0; i < count; ++i) p[i] = (unsigned char)value;
    return destination;
}
