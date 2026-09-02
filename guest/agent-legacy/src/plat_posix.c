/* POSIX adapter: the host-side conformance build (a Unix socket the lab
 * daemon's client connects to, exactly as it connects to QEMU's chardev
 * socket), and a serial transport for a Linux guest too old for
 * virtio-serial (`--port /dev/ttyS0`).
 */
#define _GNU_SOURCE
#include "plat.h"

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

#include "agent.h"

static int listen_fd = -1;
static int port_fd = -1;

static void set_nonblock(int fd)
{
    int fl = fcntl(fd, F_GETFL, 0);
    fcntl(fd, F_SETFL, fl | O_NONBLOCK);
}

static int open_serial(const char *path, char *err, int errcap)
{
    struct termios t;
    int fd = open(path, O_RDWR | O_NOCTTY | O_NONBLOCK);
    if (fd < 0) {
        snprintf(err, (size_t)errcap, "open %s: %s", path, strerror(errno));
        return -1;
    }
    if (tcgetattr(fd, &t) == 0) {
        cfmakeraw(&t);
        cfsetispeed(&t, B115200);
        cfsetospeed(&t, B115200);
        t.c_cflag |= CLOCAL | CREAD;
        t.c_cflag &= ~CRTSCTS;
        tcsetattr(fd, TCSANOW, &t);
    }
    port_fd = fd;
    return 0;
}

/* Listen on a Unix socket and wait for the one client. */
static int open_listener(const char *path, char *err, int errcap)
{
    struct sockaddr_un addr;
    if (listen_fd < 0) {
        listen_fd = socket(AF_UNIX, SOCK_STREAM, 0);
        if (listen_fd < 0) {
            snprintf(err, (size_t)errcap, "socket: %s", strerror(errno));
            return -1;
        }
        memset(&addr, 0, sizeof addr);
        addr.sun_family = AF_UNIX;
        strncpy(addr.sun_path, path, sizeof addr.sun_path - 1);
        unlink(path);
        if (bind(listen_fd, (struct sockaddr *)&addr, sizeof addr) < 0 ||
            listen(listen_fd, 1) < 0) {
            snprintf(err, (size_t)errcap, "listen %s: %s", path, strerror(errno));
            return -1;
        }
    }
    port_fd = accept(listen_fd, NULL, NULL);
    if (port_fd < 0) {
        snprintf(err, (size_t)errcap, "accept: %s", strerror(errno));
        return -1;
    }
    set_nonblock(port_fd);
    return 0;
}

int port_open(const char *spec, char *err, int errcap)
{
    if (strncmp(spec, "listen:", 7) == 0)
        return open_listener(spec + 7, err, errcap);
    return open_serial(spec, err, errcap);
}

long port_read(void *buf, long cap)
{
    ssize_t n = read(port_fd, buf, (size_t)cap);
    if (n > 0)
        return (long)n;
    if (n == 0)
        return -1;
    if (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR)
        return 0;
    return -1;
}

int port_write(const void *buf, long len)
{
    const char *p = buf;
    while (len > 0) {
        ssize_t n = write(port_fd, p, (size_t)len);
        if (n < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK) {
                struct pollfd pf;
                pf.fd = port_fd;
                pf.events = POLLOUT;
                poll(&pf, 1, 1000);
                continue;
            }
            if (errno == EINTR)
                continue;
            return -1;
        }
        p += n;
        len -= (long)n;
    }
    return 0;
}

void port_close(void)
{
    if (port_fd >= 0)
        close(port_fd);
    port_fd = -1;
}

void plat_sleep_ms(int ms)
{
    struct timespec ts;
    ts.tv_sec = ms / 1000;
    ts.tv_nsec = (long)(ms % 1000) * 1000000L;
    nanosleep(&ts, NULL);
}

/* ---- processes ------------------------------------------------------ */

struct plat_proc {
    pid_t pid;
    int in_fd;
    int out_fd;
    int err_fd;
    char *pending;
    long pending_len;
    long pending_cap;
    int in_eof; /* close once the queue drains */
    unsigned long consumed;
    int exited;
    long code;
};

struct plat_proc *proc_start(const struct exec_spec *spec, char *err, int errcap)
{
    int in[2], out[2], errp[2];
    struct plat_proc *p;
    pid_t pid;
    if (pipe(in) < 0 || pipe(out) < 0 || pipe(errp) < 0) {
        snprintf(err, (size_t)errcap, "pipe: %s", strerror(errno));
        return NULL;
    }
    pid = fork();
    if (pid < 0) {
        snprintf(err, (size_t)errcap, "fork: %s", strerror(errno));
        return NULL;
    }
    if (pid == 0) {
        int i;
        dup2(in[0], 0);
        dup2(out[1], 1);
        dup2(errp[1], 2);
        close(in[0]);
        close(in[1]);
        close(out[0]);
        close(out[1]);
        close(errp[0]);
        close(errp[1]);
        for (i = 0; i < spec->nenv; i++)
            putenv(spec->env[i]);
        if (spec->cwd && chdir(spec->cwd) < 0)
            _exit(127);
        execvp(spec->argv[0], spec->argv);
        _exit(127);
    }
    close(in[0]);
    close(out[1]);
    close(errp[1]);
    p = calloc(1, sizeof *p);
    p->pid = pid;
    p->in_fd = in[1];
    p->out_fd = out[0];
    p->err_fd = errp[0];
    set_nonblock(p->in_fd);
    set_nonblock(p->out_fd);
    set_nonblock(p->err_fd);
    return p;
}

long proc_read(struct plat_proc *p, int which, void *buf, long cap)
{
    int fd = which ? p->err_fd : p->out_fd;
    ssize_t n;
    if (fd < 0)
        return -1;
    n = read(fd, buf, (size_t)cap);
    if (n > 0)
        return (long)n;
    if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR))
        return 0;
    close(fd);
    if (which)
        p->err_fd = -1;
    else
        p->out_fd = -1;
    return -1;
}

static void flush_in(struct plat_proc *p)
{
    while (p->in_fd >= 0 && p->pending_len > 0) {
        ssize_t n = write(p->in_fd, p->pending, (size_t)p->pending_len);
        if (n < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR)
                return;
            /* The child closed its end: keep draining so credit flows. */
            p->consumed += (unsigned long)p->pending_len;
            p->pending_len = 0;
            close(p->in_fd);
            p->in_fd = -1;
            return;
        }
        p->consumed += (unsigned long)n;
        memmove(p->pending, p->pending + n, (size_t)(p->pending_len - n));
        p->pending_len -= (long)n;
    }
    if (p->in_eof && p->pending_len == 0 && p->in_fd >= 0) {
        close(p->in_fd);
        p->in_fd = -1;
    }
}

void proc_write_in(struct plat_proc *p, const void *buf, long len)
{
    if (p->in_fd < 0) {
        p->consumed += (unsigned long)len;
        return;
    }
    if (p->pending_len + len > p->pending_cap) {
        long cap = p->pending_cap ? p->pending_cap * 2 : 8192;
        while (cap < p->pending_len + len)
            cap *= 2;
        p->pending = realloc(p->pending, (size_t)cap);
        p->pending_cap = cap;
    }
    memcpy(p->pending + p->pending_len, buf, (size_t)len);
    p->pending_len += len;
    flush_in(p);
}

unsigned long proc_consumed(struct plat_proc *p)
{
    unsigned long n;
    flush_in(p);
    n = p->consumed;
    p->consumed = 0;
    return n;
}

void proc_close_in(struct plat_proc *p)
{
    p->in_eof = 1;
    flush_in(p);
}

int proc_exited(struct plat_proc *p, long *code)
{
    int status;
    if (!p->exited) {
        pid_t r = waitpid(p->pid, &status, WNOHANG);
        if (r == p->pid) {
            p->exited = 1;
            if (WIFEXITED(status))
                p->code = WEXITSTATUS(status);
            else if (WIFSIGNALED(status))
                p->code = 128 + WTERMSIG(status);
            else
                p->code = 255;
        } else if (r < 0) {
            p->exited = 1;
            p->code = 255;
        }
    }
    if (p->exited)
        *code = p->code;
    return p->exited;
}

void proc_kill(struct plat_proc *p)
{
    if (!p->exited)
        kill(p->pid, SIGKILL);
}

void proc_free(struct plat_proc *p)
{
    long code;
    if (p->in_fd >= 0)
        close(p->in_fd);
    if (p->out_fd >= 0)
        close(p->out_fd);
    if (p->err_fd >= 0)
        close(p->err_fd);
    if (!p->exited) {
        int status;
        waitpid(p->pid, &status, 0);
    }
    (void)code;
    free(p->pending);
    free(p);
}

/* ---- the machine ---------------------------------------------------- */

const char *plat_os_tag(void)
{
    return "linux";
}

static void copy_field(char *dst, size_t cap, const char *src)
{
    strncpy(dst, src, cap - 1);
    dst[cap - 1] = 0;
}

/* One os-release value, unquoted. */
static void os_release(const char *key, char *dst, size_t cap)
{
    FILE *f = fopen("/etc/os-release", "r");
    char line[512];
    size_t klen = strlen(key);
    dst[0] = 0;
    if (!f)
        return;
    while (fgets(line, sizeof line, f)) {
        if (strncmp(line, key, klen) == 0 && line[klen] == '=') {
            char *v = line + klen + 1;
            size_t n = strlen(v);
            while (n && (v[n - 1] == '\n' || v[n - 1] == '\r'))
                v[--n] = 0;
            if (n >= 2 && v[0] == '"' && v[n - 1] == '"') {
                v[n - 1] = 0;
                v++;
            }
            copy_field(dst, cap, v);
            break;
        }
    }
    fclose(f);
}

void plat_os_info(struct os_info *info)
{
    struct utsname u;
    os_release("ID", info->id, sizeof info->id);
    os_release("PRETTY_NAME", info->name, sizeof info->name);
    os_release("VERSION_ID", info->version, sizeof info->version);
    if (!info->id[0])
        copy_field(info->id, sizeof info->id, "linux");
    if (!info->name[0])
        copy_field(info->name, sizeof info->name, "Linux");
    if (uname(&u) == 0) {
        copy_field(info->kernel, sizeof info->kernel, u.release);
        copy_field(info->arch, sizeof info->arch, u.machine);
        copy_field(info->hostname, sizeof info->hostname, u.nodename);
    }
}

int plat_shutdown(const char *mode)
{
    const char *argv[3];
    pid_t pid;
    argv[0] = "/sbin/shutdown";
    argv[2] = NULL;
    if (strcmp(mode, "reboot") == 0)
        argv[1] = "-r";
    else if (strcmp(mode, "halt") == 0)
        argv[1] = "-H";
    else
        argv[1] = "-P";
    pid = fork();
    if (pid < 0)
        return -1;
    if (pid == 0) {
        execl(argv[0], argv[0], argv[1], "now", (char *)NULL);
        _exit(127);
    }
    return 0;
}

void plat_log(const char *msg)
{
    fprintf(stderr, "%s\n", msg);
}

/* ---- entry ---------------------------------------------------------- */

static void usage(void)
{
    fprintf(stderr,
            "usage: vmlab-agent-legacy --port <serial device>\n"
            "       vmlab-agent-legacy --listen <unix socket path>\n");
}

int main(int argc, char **argv)
{
    char spec[512];
    char err[256];
    if (argc != 3) {
        usage();
        return 2;
    }
    if (strcmp(argv[1], "--port") == 0) {
        snprintf(spec, sizeof spec, "%s", argv[2]);
    } else if (strcmp(argv[1], "--listen") == 0) {
        snprintf(spec, sizeof spec, "listen:%s", argv[2]);
    } else {
        usage();
        return 2;
    }
    signal(SIGPIPE, SIG_IGN);
    for (;;) {
        if (port_open(spec, err, (int)sizeof err) < 0) {
            plat_log(err);
            plat_sleep_ms(1000);
            continue;
        }
        if (agent_run() == 0) {
            port_close();
            return 0; /* shutdown initiated */
        }
        port_close();
    }
}
