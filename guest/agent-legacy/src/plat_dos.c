/* DOS adapter: a 32-bit flat program under a bound extender (OpenWatcom,
 * `system dos32a`), so the core loop and its buffers need no memory model.
 *
 * The channel is the 16550 at COM1 polled directly (no interrupts). QEMU
 * stops feeding the UART while its FIFO is full, so polling loses nothing;
 * throughput is what the emulated baud rate allows.
 *
 * DOS runs one program at a time, so an exec is synchronous: stdout and
 * stderr go to temp files through DOS handle redirection, the child runs to
 * completion, and the files are then streamed back as the channel's bytes.
 * The agent answers nothing while the child runs — a fact the host sees as
 * latency, never as a lost agent, because the frames are simply late.
 * Stdin is not delivered (there is no one to deliver it to); the bytes are
 * acknowledged so the host's credit never stalls.
 */
#include <conio.h>
#include <ctype.h>
#include <direct.h>
#include <dos.h>
#include <errno.h>
#include <fcntl.h>
#include <i86.h>
#include <io.h>
#include <process.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

#include "agent.h"
#include "plat.h"

/* ---- the UART ------------------------------------------------------- */

static unsigned base = 0x3F8;

#define UART_RBR (base + 0)
#define UART_THR (base + 0)
#define UART_DLL (base + 0)
#define UART_DLM (base + 1)
#define UART_IER (base + 1)
#define UART_FCR (base + 2)
#define UART_LCR (base + 3)
#define UART_MCR (base + 4)
#define UART_LSR (base + 5)

int port_open(const char *spec, char *err, int errcap)
{
    if (strcmp(spec, "COM2") == 0)
        base = 0x2F8;
    else if (strcmp(spec, "COM3") == 0)
        base = 0x3E8;
    else if (strcmp(spec, "COM4") == 0)
        base = 0x2E8;
    else
        base = 0x3F8;
    (void)err;
    (void)errcap;
    outp(UART_IER, 0x00);       /* polling: no interrupts */
    outp(UART_LCR, 0x80);       /* divisor latch */
    outp(UART_DLL, 0x01);       /* 115200 */
    outp(UART_DLM, 0x00);
    outp(UART_LCR, 0x03);       /* 8N1 */
    outp(UART_FCR, 0xC7);       /* FIFO on, cleared, 14-byte trigger */
    outp(UART_MCR, 0x0B);       /* DTR | RTS | OUT2 */
    (void)inp(UART_RBR);        /* drain a stale byte */
    return 0;
}

long port_read(void *buf, long cap)
{
    unsigned char *p = buf;
    long n = 0;
    while (n < cap && (inp(UART_LSR) & 0x01))
        p[n++] = (unsigned char)inp(UART_RBR);
    return n;
}

int port_write(const void *buf, long len)
{
    const unsigned char *p = buf;
    while (len-- > 0) {
        while (!(inp(UART_LSR) & 0x20))
            ;
        outp(UART_THR, *p++);
    }
    return 0;
}

void port_close(void)
{
}

void plat_sleep_ms(int ms)
{
    union REGS r;
    /* Under a Windows DOS box, give the slice back; otherwise delay(). */
    r.w.ax = 0x1680;
    int386(0x2F, &r, &r);
    delay((unsigned)ms);
}

/* ---- processes ------------------------------------------------------ */

struct plat_proc {
    int out_fd; /* the captured stdout file, being streamed back */
    int err_fd;
    long code;
    unsigned long consumed;
    char out_path[128];
    char err_path[128];
};

static int busy = 0;

static void temp_path(char *dst, size_t cap, const char *name)
{
    const char *dir = getenv("TEMP");
    if (!dir)
        dir = getenv("TMP");
    if (!dir)
        dir = "C:\\";
    if (dir[strlen(dir) - 1] == '\\')
        _snprintf(dst, cap, "%s%s", dir, name);
    else
        _snprintf(dst, cap, "%s\\%s", dir, name);
}

/* The DOS command tail: arguments joined by spaces, one containing a space
 * wrapped in quotes (what the C runtime of a well-behaved child undoes;
 * COMMAND.COM's internals take the tail verbatim either way). */
static void build_tail(char **argv, int argc, char *out, size_t cap)
{
    size_t n = 0;
    int i;
    out[0] = 0;
    for (i = 0; i < argc; i++) {
        int q = strchr(argv[i], ' ') != NULL;
        n += (size_t)_snprintf(out + n, cap - n, "%s%s%s%s", i ? " " : "",
                               q ? "\"" : "", argv[i], q ? "\"" : "");
        if (n >= cap - 1)
            break;
    }
}

struct plat_proc *proc_start(const struct exec_spec *spec, char *err, int errcap)
{
    static char tail[1024];
    struct plat_proc *p;
    int save0, save1, save2, in, out, errf, i, rc;
    char save_cwd[260];
    unsigned save_drive, drives;

    if (busy) {
        _snprintf(err, (size_t)errcap, "exec: DOS runs one command at a time");
        return NULL;
    }
    p = calloc(1, sizeof *p);
    temp_path(p->out_path, sizeof p->out_path, "VMLABOUT.TMP");
    temp_path(p->err_path, sizeof p->err_path, "VMLABERR.TMP");

    in = open("NUL", O_RDONLY);
    out = open(p->out_path, O_WRONLY | O_CREAT | O_TRUNC | O_BINARY, S_IREAD | S_IWRITE);
    errf = open(p->err_path, O_WRONLY | O_CREAT | O_TRUNC | O_BINARY, S_IREAD | S_IWRITE);
    if (in < 0 || out < 0 || errf < 0) {
        _snprintf(err, (size_t)errcap, "exec: cannot create %s", p->out_path);
        free(p);
        return NULL;
    }

    for (i = 0; i < spec->nenv; i++)
        putenv(spec->env[i]);
    getcwd(save_cwd, sizeof save_cwd);
    _dos_getdrive(&save_drive);
    if (spec->cwd) {
        if (strlen(spec->cwd) >= 2 && spec->cwd[1] == ':')
            _dos_setdrive((unsigned)(toupper(spec->cwd[0]) - 'A' + 1), &drives);
        chdir(spec->cwd);
    }

    save0 = dup(0);
    save1 = dup(1);
    save2 = dup(2);
    dup2(in, 0);
    dup2(out, 1);
    dup2(errf, 2);
    close(in);
    close(out);
    close(errf);

    busy = 1;
    /* A program on the path runs directly and reports its own exit code;
     * anything else (an internal command, a batch file) goes through
     * COMMAND.COM /C. */
    rc = spawnvp(P_WAIT, spec->argv[0], (const char *const *)spec->argv);
    if (rc < 0 && errno == ENOENT) {
        build_tail(spec->argv, spec->argc, tail, sizeof tail);
        rc = system(tail);
    }
    busy = 0;

    dup2(save0, 0);
    dup2(save1, 1);
    dup2(save2, 2);
    close(save0);
    close(save1);
    close(save2);

    _dos_setdrive(save_drive, &drives);
    chdir(save_cwd);
    for (i = 0; i < spec->nenv; i++) {
        char *eq = strchr(spec->env[i], '=');
        if (eq) {
            *eq = 0;
            /* Watcom: "NAME=" with an empty value removes the variable. */
            {
                static char del[256];
                _snprintf(del, sizeof del, "%s=", spec->env[i]);
                putenv(del);
            }
            *eq = '=';
        }
    }

    p->code = rc < 0 ? 127 : rc;
    p->out_fd = open(p->out_path, O_RDONLY | O_BINARY);
    p->err_fd = open(p->err_path, O_RDONLY | O_BINARY);
    return p;
}

long proc_read(struct plat_proc *p, int which, void *buf, long cap)
{
    int *fd = which ? &p->err_fd : &p->out_fd;
    int n;
    if (*fd < 0)
        return -1;
    n = read(*fd, buf, (unsigned)cap);
    if (n > 0)
        return n;
    close(*fd);
    *fd = -1;
    return -1;
}

void proc_write_in(struct plat_proc *p, const void *buf, long len)
{
    (void)buf;
    p->consumed += (unsigned long)len;
}

unsigned long proc_consumed(struct plat_proc *p)
{
    unsigned long n = p->consumed;
    p->consumed = 0;
    return n;
}

void proc_close_in(struct plat_proc *p)
{
    (void)p;
}

int proc_exited(struct plat_proc *p, long *code)
{
    *code = p->code;
    return 1;
}

void proc_kill(struct plat_proc *p)
{
    (void)p; /* already finished by construction */
}

void proc_free(struct plat_proc *p)
{
    if (p->out_fd >= 0)
        close(p->out_fd);
    if (p->err_fd >= 0)
        close(p->err_fd);
    unlink(p->out_path);
    unlink(p->err_path);
    free(p);
}

/* ---- the machine ---------------------------------------------------- */

const char *plat_os_tag(void)
{
    return "dos";
}

void plat_os_info(struct os_info *info)
{
    union REGS r;
    unsigned major, minor, oem;
    r.h.ah = 0x30;
    r.h.al = 0x00;
    int386(0x21, &r, &r);
    major = r.h.al;
    minor = r.h.ah;
    oem = r.h.bh;
    /* OEM 0xFD is FreeDOS; 0xFF (or 0) is MS-DOS/PC DOS. */
    if (oem == 0xFD) {
        strcpy(info->id, "freedos");
        strcpy(info->name, "FreeDOS");
    } else {
        strcpy(info->id, "msdos");
        strcpy(info->name, "MS-DOS");
    }
    _snprintf(info->version, sizeof info->version, "%u.%02u", major, minor);
    strcpy(info->kernel, info->version);
    strcpy(info->arch, "i386");
    strcpy(info->hostname, "dos");
}

/* Simulate a real-mode interrupt through DPMI 0300h. */
struct rmregs {
    unsigned long edi, esi, ebp, reserved, ebx, edx, ecx, eax;
    unsigned short flags, es, ds, fs, gs, ip, cs, sp, ss;
};

static int real_int(int num, struct rmregs *rm)
{
    union REGS r;
    struct SREGS s;
    memset(&r, 0, sizeof r);
    segread(&s);
    r.w.ax = 0x0300;
    r.h.bl = (unsigned char)num;
    r.h.bh = 0;
    r.w.cx = 0;
    s.es = FP_SEG(rm);
    r.x.edi = FP_OFF(rm);
    int386x(0x31, &r, &r, &s);
    return r.w.cflag ? -1 : 0;
}

static int apm_power_off(void)
{
    struct rmregs rm;
    memset(&rm, 0, sizeof rm);
    rm.eax = 0x5300; /* installation check */
    rm.ebx = 0;
    if (real_int(0x15, &rm) < 0 || (rm.flags & 1))
        return -1;
    memset(&rm, 0, sizeof rm);
    rm.eax = 0x5301; /* connect real-mode interface */
    rm.ebx = 0;
    real_int(0x15, &rm);
    memset(&rm, 0, sizeof rm);
    rm.eax = 0x530E; /* driver version 1.1 */
    rm.ebx = 0;
    rm.ecx = 0x0101;
    real_int(0x15, &rm);
    memset(&rm, 0, sizeof rm);
    rm.eax = 0x5307; /* set power state: all devices off */
    rm.ebx = 0x0001;
    rm.ecx = 0x0003;
    if (real_int(0x15, &rm) < 0 || (rm.flags & 1))
        return -1;
    return 0;
}

int plat_shutdown(const char *mode)
{
    if (strcmp(mode, "reboot") == 0) {
        /* Pulse the keyboard controller's reset line. */
        outp(0x64, 0xFE);
        return 0;
    }
    /* "halt" has no distinct meaning here: power off. */
    return apm_power_off();
}

void plat_log(const char *msg)
{
    printf("vmlab-agent-legacy: %s\n", msg);
}

/* ---- entry ---------------------------------------------------------- */

int main(int argc, char **argv)
{
    char err[128];
    const char *port_name = "COM1";
    int i;
    for (i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--port") == 0 && i + 1 < argc) {
            port_name = argv[++i];
        } else {
            printf("usage: VMLABAGT [--port COMn]\n");
            return 2;
        }
    }
    printf("vmlab-agent-legacy %s on %s (Ctrl-Break to stop)\n", AGENT_VERSION, port_name);
    for (;;) {
        if (port_open(port_name, err, (int)sizeof err) < 0) {
            plat_log(err);
            return 1;
        }
        if (agent_run() == 0)
            return 0; /* shutdown initiated */
        plat_sleep_ms(500);
    }
}
