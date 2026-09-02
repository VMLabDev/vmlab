/* What the core loop (agent.c) needs from a platform, and nothing more.
 * Three adapters: plat_win32.c (NT4 through XP/2003, and Windows 9x/ME),
 * plat_dos.c (32-bit DOS under a bound extender) and plat_posix.c (the
 * host-side conformance build, and a serial transport for old Linux).
 *
 * Every call is non-blocking from the loop's point of view: the port is
 * polled, a child's pipes are polled, and stdin bytes are queued. A
 * platform with no way to write a pipe without blocking (Win32 anonymous
 * pipes) does that on a helper thread behind `proc_write_in`.
 */
#ifndef VMLAB_PLAT_H
#define VMLAB_PLAT_H

/* ---- the agent channel ---------------------------------------------- */

/* Open the channel named by `spec` (platform-shaped: "COM1", "/dev/ttyS0",
 * a socket path). Returns 0, or -1 with a message in `err`. */
int port_open(const char *spec, char *err, int errcap);
/* Bytes available now, 0 for none, -1 when the channel is gone. */
long port_read(void *buf, long cap);
/* Write everything or fail. */
int port_write(const void *buf, long len);
void port_close(void);

void plat_sleep_ms(int ms);

/* ---- a child process ------------------------------------------------ */

struct exec_spec {
    char **argv;
    int argc;
    /* Environment overrides as `KEY=VALUE` strings, applied over the
     * agent's own environment. */
    char **env;
    int nenv;
    const char *cwd; /* NULL: inherit */
};

struct plat_proc; /* opaque, platform-owned */

/* Spawn with piped stdio. NULL with a message in `err` on failure — which
 * on a single-tasking platform includes "one at a time". */
struct plat_proc *proc_start(const struct exec_spec *spec, char *err, int errcap);
/* Read from stdout (`which` 0) or stderr (1): bytes, 0 for none right now,
 * -1 once that pipe is closed for good. */
long proc_read(struct plat_proc *p, int which, void *buf, long cap);
/* Queue bytes for the child's stdin. Never blocks. */
void proc_write_in(struct plat_proc *p, const void *buf, long len);
/* Stdin bytes actually handed to the child since the last call — what the
 * host is granted credit back for. */
unsigned long proc_consumed(struct plat_proc *p);
void proc_close_in(struct plat_proc *p);
/* 1 once the child has exited, with its exit code. */
int proc_exited(struct plat_proc *p, long *code);
void proc_kill(struct plat_proc *p);
void proc_free(struct plat_proc *p);

/* ---- the machine ---------------------------------------------------- */

struct os_info {
    char id[32];
    char name[128];
    char version[64];
    char kernel[64];
    char arch[16];
    char hostname[128];
};

/* The `os` string in the hello: "windows", "dos" or "linux". */
const char *plat_os_tag(void);
void plat_os_info(struct os_info *info);
/* `mode` is "powerdown", "reboot" or "halt". 0 when initiated. */
int plat_shutdown(const char *mode);

/* Diagnostics: a service log, stderr, or nowhere. */
void plat_log(const char *msg);

#endif
