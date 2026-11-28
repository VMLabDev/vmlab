/* ipstack: user-space stack sampler of one process, by perf_event_open.
 *
 * A variant of bench/ipsample.c for the stack profile of card C13b
 * (#1144). Each thread of the process gets one event (inherit forbids the
 * ring buffer) at 1000 samples a second, with the user call chain that the
 * kernel unwinds over frame pointers. Kernel frames are left out. Nothing
 * is resolved here: every sample is one line "<tid> <ip> <caller>...", in
 * hex, and the maps and the thread names of the process are copied beside
 * it, for ipstack.py.
 *
 * IPSTACK_WORDS=N (card #1212) also copies the first N words of the user
 * stack of each sample and writes them after a "|": a leaf that keeps no
 * frame (the memmove and memset of glibc) has its return address there, at
 * the stack pointer, where the frame-pointer chain does not see it. The
 * line then ends with "@<ns>", the time of the sample on CLOCK_MONOTONIC (the
 * kernel refuses CLOCK_REALTIME for a hardware event), and OUT.clock holds
 * "<realtime ns> <monotonic ns>" read together at the start, so that a
 * window of a few seconds (a bench workload) can be cut out.
 *
 * Build: cc -O2 -o ipstack ipstack.c
 * Use:   ipstack PID SECONDS OUT   (writes OUT.stacks, OUT.maps, OUT.threads)
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <inttypes.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
#include <linux/perf_event.h>

#define PAGES 64
#define EVENTS 512

static int copy_maps(pid_t pid, const char *out) {
    char path[64], name[600], buf[65536];
    snprintf(path, sizeof(path), "/proc/%d/maps", pid);
    snprintf(name, sizeof(name), "%s.maps", out);
    FILE *in = fopen(path, "r"), *to = fopen(name, "w");
    if (!in || !to) return -1;
    size_t n;
    while ((n = fread(buf, 1, sizeof(buf), in)) > 0) fwrite(buf, 1, n, to);
    fclose(in);
    fclose(to);
    return 0;
}

/* Copy `len` bytes at `offset` of the ring into `dst`, across its end. */
static void ring_copy(unsigned char *data, uint64_t mask, uint64_t offset,
                      void *dst, size_t len) {
    unsigned char *d = dst;
    for (size_t i = 0; i < len; i++) d[i] = data[(offset + i) & mask];
}

int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "usage: ipstack PID SECONDS OUT\n");
        return 2;
    }
    pid_t pid = (pid_t)atoi(argv[1]);
    long secs = atol(argv[2]);
    const char *out = argv[3];
    if (copy_maps(pid, out) < 0) {
        perror("maps");
        return 2;
    }
    char name[600];
    snprintf(name, sizeof(name), "%s.stacks", out);
    FILE *stacks = fopen(name, "w");
    if (!stacks) {
        perror("stacks");
        return 2;
    }

    struct {
        int fd;
        struct perf_event_mmap_page *mp;
        uint64_t prev;
    } ev[EVENTS];
    int nev = 0;
    char taskdir[64];
    snprintf(taskdir, sizeof(taskdir), "/proc/%d/task", pid);
    DIR *d = opendir(taskdir);
    if (!d) {
        perror("opendir tasks");
        return 2;
    }
    size_t page = sysconf(_SC_PAGESIZE);
    long words = getenv("IPSTACK_WORDS") ? atol(getenv("IPSTACK_WORDS")) : 0;
    if (words < 0 || words > 64) words = 0;
    struct dirent *de;
    while ((de = readdir(d)) && nev < EVENTS) {
        if (de->d_name[0] == '.') continue;
        pid_t tid = atoi(de->d_name);
        struct perf_event_attr attr;
        memset(&attr, 0, sizeof(attr));
        attr.size = sizeof(attr);
        attr.type = PERF_TYPE_HARDWARE;
        attr.config = PERF_COUNT_HW_CPU_CYCLES;
        attr.exclude_kernel = 1;
        int probe = syscall(__NR_perf_event_open, &attr, tid, -1, -1, 0);
        if (probe < 0) {
            attr.type = PERF_TYPE_SOFTWARE;
            attr.config = PERF_COUNT_SW_CPU_CLOCK;
        } else {
            close(probe);
        }
        attr.freq = 1;
        attr.sample_freq = 1000;
        attr.wakeup_events = 1;
        attr.sample_type = PERF_SAMPLE_IP | PERF_SAMPLE_TID | PERF_SAMPLE_CALLCHAIN;
        attr.exclude_kernel = 1;
        attr.exclude_guest = 1;
        attr.exclude_callchain_kernel = 1;
        if (words) {
            attr.sample_type |= PERF_SAMPLE_TIME | PERF_SAMPLE_STACK_USER;
            attr.use_clockid = 1;
            attr.clockid = CLOCK_MONOTONIC;
            attr.sample_stack_user = (uint32_t)(words * 8);
        }
        int fd = (int)syscall(__NR_perf_event_open, &attr, tid, -1, -1, 0);
        if (fd < 0) continue;
        void *ring = mmap(NULL, (PAGES + 1) * page, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (ring == MAP_FAILED) {
            close(fd);
            continue;
        }
        ev[nev].fd = fd;
        ev[nev].mp = ring;
        ev[nev].prev = 0;
        nev++;
    }
    closedir(d);
    if (words) {
        struct timespec real, mono;
        clock_gettime(CLOCK_REALTIME, &real);
        clock_gettime(CLOCK_MONOTONIC, &mono);
        snprintf(name, sizeof(name), "%s.clock", out);
        FILE *clock = fopen(name, "w");
        if (clock) {
            fprintf(clock, "%lld %lld\n", (long long)real.tv_sec * 1000000000LL + real.tv_nsec,
                    (long long)mono.tv_sec * 1000000000LL + mono.tv_nsec);
            fclose(clock);
        }
    }
    snprintf(name, sizeof(name), "%s.threads", out);
    FILE *threads = fopen(name, "w");
    if (threads) {
        d = opendir(taskdir);
        while (d && (de = readdir(d))) {
            if (de->d_name[0] == '.') continue;
            char comm[600], line[64] = "";
            snprintf(comm, sizeof(comm), "%s/%s/comm", taskdir, de->d_name);
            FILE *c = fopen(comm, "r");
            if (c) {
                if (!fgets(line, sizeof(line), c)) line[0] = 0;
                fclose(c);
            }
            line[strcspn(line, "\n")] = 0;
            fprintf(threads, "%x %s\n", atoi(de->d_name), line);
        }
        if (d) closedir(d);
        fclose(threads);
    }
    fprintf(stderr, "sampling %d threads\n", nev);
    if (nev == 0) {
        fprintf(stderr, "no events opened: %s\n", strerror(errno));
        return 2;
    }

    struct pollfd pfds[EVENTS];
    for (int i = 0; i < nev; i++) {
        pfds[i].fd = ev[i].fd;
        pfds[i].events = POLLIN;
    }
    uint64_t samples = 0;
    uint64_t chain[512];
    long end = time(NULL) + secs;
    while (time(NULL) < end) {
        poll(pfds, nev, 200);
        for (int i = 0; i < nev; i++) {
            struct perf_event_mmap_page *mp = ev[i].mp;
            uint64_t head = __atomic_load_n(&mp->data_head, __ATOMIC_ACQUIRE);
            unsigned char *data = (unsigned char *)mp + page;
            uint64_t mask = (PAGES * page) - 1;
            while (ev[i].prev < head) {
                struct perf_event_header hdr;
                ring_copy(data, mask, ev[i].prev, &hdr, sizeof(hdr));
                if (hdr.size == 0) break;
                if (hdr.type == PERF_RECORD_SAMPLE) {
                    uint64_t at = ev[i].prev + sizeof(hdr);
                    uint64_t ip, nr;
                    uint32_t ids[2];
                    ring_copy(data, mask, at, &ip, 8);
                    ring_copy(data, mask, at + 8, ids, 8);
                    /* PERF_SAMPLE_TIME comes after the ids, when it is on. */
                    uint64_t when = 0, skip = words ? 8 : 0;
                    if (words) ring_copy(data, mask, at + 16, &when, 8);
                    ring_copy(data, mask, at + 16 + skip, &nr, 8);
                    uint64_t stack_at = at + 24 + skip + nr * 8;
                    if (nr > 512) nr = 512;
                    ring_copy(data, mask, at + 24 + skip, chain, nr * 8);
                    fprintf(stacks, "%x %" PRIx64, ids[1], ip);
                    for (uint64_t k = 0; k < nr; k++) {
                        /* The context markers (PERF_CONTEXT_USER and the
                         * others) are the top 4095 values. */
                        if (chain[k] >= (uint64_t)-4095) continue;
                        if (k == 0 || chain[k] != ip) fprintf(stacks, " %" PRIx64, chain[k]);
                    }
                    if (words) {
                        /* PERF_SAMPLE_STACK_USER: the size, the bytes, then
                         * the size that was copied (dyn_size). */
                        uint64_t size, top[64], dyn = 0;
                        ring_copy(data, mask, stack_at, &size, 8);
                        if (size) ring_copy(data, mask, stack_at + 8 + size, &dyn, 8);
                        if (dyn > size) dyn = size;
                        if (dyn > sizeof(top)) dyn = sizeof(top);
                        ring_copy(data, mask, stack_at + 8, top, dyn);
                        fputs(" |", stacks);
                        for (uint64_t k = 0; k < dyn / 8; k++) fprintf(stacks, " %" PRIx64, top[k]);
                        fprintf(stacks, " @%" PRIu64, when);
                    }
                    fputc('\n', stacks);
                    samples++;
                }
                ev[i].prev += hdr.size;
            }
            __atomic_store_n(&mp->data_tail, head, __ATOMIC_RELEASE);
        }
    }
    for (int i = 0; i < nev; i++) ioctl(ev[i].fd, PERF_EVENT_IOC_DISABLE, 0);
    fclose(stacks);
    fprintf(stderr, "%" PRIu64 " samples\n", samples);
    return 0;
}
