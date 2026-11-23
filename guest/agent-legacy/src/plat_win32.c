/* Win32 adapter: NT4 through XP/2003 (mingw i686 or OpenWatcom `nt`), and
 * Windows 95/98/ME (OpenWatcom `win95`). One source: everything below is
 * ANSI, every API exists on both families, and the few that do not —
 * the SCM service entry points, InitiateSystemShutdown, RegisterServiceProcess
 * — are resolved with GetProcAddress so the binary loads on either.
 *
 * The channel is a COM port opened with zero read timeouts, so ReadFile
 * returns whatever the UART holds without blocking. Child stdin is the one
 * thing Win32 cannot write without blocking (anonymous pipes have no
 * overlapped mode), so a helper thread owns that pipe; everything else is
 * polled from the core loop.
 */
#define WIN32_LEAN_AND_MEAN
#define _WIN32_WINNT 0x0400
#include <windows.h>

#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "agent.h"
#include "plat.h"

/* The longest command line CreateProcess takes, terminator included. */
#define CMDLINE_CAP 32768

static HANDLE port = INVALID_HANDLE_VALUE;
static DWORD port_error; /* why the last port_open failed */
static int console_mode = 0;
static int is_win9x = 0;
static HANDLE log_file = INVALID_HANDLE_VALUE;

/* ---- text ----------------------------------------------------------- */

/* _snprintf leaves the buffer unterminated when the text does not fit (both
 * msvcrt's and OpenWatcom's); every formatted string here goes through this
 * instead, which always terminates. */
static void fmt(char *dst, size_t cap, const char *f, ...)
{
    va_list ap;
    if (cap == 0)
        return;
    va_start(ap, f);
    _vsnprintf(dst, cap, f, ap);
    va_end(ap);
    dst[cap - 1] = 0;
}

/* Why a wire string did not become an ANSI one. */
#define CONV_TOO_LONG (-1)
#define CONV_NOT_UTF8 (-2)
#define CONV_LOSSY (-3)

/* UTF-8 (the wire) -> the ANSI code page (every A API), by hand through
 * UTF-16 so no code page support is assumed of the guest. 0, or one of the
 * CONV_ codes with `dst` empty. Called from the core loop's thread only,
 * hence the static scratch.
 *
 * The conversion is exact or it is refused. WideCharToMultiByte reports
 * success when it puts the default character `?` — a wildcard to cmd.exe
 * and command.com — or a best-fit look-alike (a full-width quote becomes
 * `"`) in place of a character the code page lacks, so the result is
 * converted back and compared: only text that survives the round trip is
 * the text the host sent. That needs no flag NT4 or Windows 95 lack
 * (WC_NO_BEST_FIT_CHARS is newer than both). `*bad` is the first code point
 * that did not survive (CONV_LOSSY). */
static int utf8_to_ansi(const char *src, char *dst, int cap, unsigned long *bad)
{
    static WCHAR wide[CMDLINE_CAP];
    static WCHAR back[CMDLINE_CAP];
    int n = 0, r, m, i;
    const unsigned char *s = (const unsigned char *)src;
    *bad = 0;
    if (cap < 1)
        return CONV_TOO_LONG;
    dst[0] = 0;
    while (*s && n < CMDLINE_CAP - 2) {
        unsigned long cp;
        int extra;
        if (*s < 0x80) {
            cp = *s;
            extra = 0;
        } else if ((*s & 0xE0) == 0xC0) {
            cp = *s & 0x1F;
            extra = 1;
        } else if ((*s & 0xF0) == 0xE0) {
            cp = *s & 0x0F;
            extra = 2;
        } else if ((*s & 0xF8) == 0xF0) {
            cp = *s & 0x07;
            extra = 3;
        } else {
            return CONV_NOT_UTF8;
        }
        s++;
        while (extra > 0 && (*s & 0xC0) == 0x80) {
            cp = (cp << 6) | (*s++ & 0x3F);
            extra--;
        }
        if (extra || cp > 0x10FFFF)
            return CONV_NOT_UTF8; /* a cut sequence is not a shorter character */
        if (cp >= 0x10000) {
            cp -= 0x10000;
            wide[n++] = (WCHAR)(0xD800 | (cp >> 10));
            wide[n++] = (WCHAR)(0xDC00 | (cp & 0x3FF));
        } else {
            wide[n++] = (WCHAR)cp;
        }
    }
    wide[n] = 0;
    if (*s)
        return CONV_TOO_LONG; /* longer than the scratch */
    if (n == 0)
        return 0;
    r = WideCharToMultiByte(CP_ACP, 0, wide, n, dst, cap - 1, NULL, NULL);
    if (r <= 0) {
        dst[0] = 0; /* 0 is ERROR_INSUFFICIENT_BUFFER */
        return CONV_TOO_LONG;
    }
    dst[r] = 0;
    m = MultiByteToWideChar(CP_ACP, 0, dst, r, back, CMDLINE_CAP);
    for (i = 0; i < n && i < m && wide[i] == back[i]; i++)
        ;
    if (i < n || m != n) {
        /* `i == n` is text that grew on the way back: blame its last unit. */
        if (i == n)
            i = n - 1;
        /* Name a character outside the BMP whole, not one of its halves. */
        if (wide[i] >= 0xDC00 && wide[i] < 0xE000 && i > 0)
            i--;
        *bad = wide[i];
        if (*bad >= 0xD800 && *bad < 0xDC00 && i + 1 < n)
            *bad = 0x10000 + ((*bad - 0xD800) << 10) + (wide[i + 1] - 0xDC00);
        dst[0] = 0;
        return CONV_LOSSY;
    }
    return 0;
}

/* The refusal for a string that did not convert, naming it (`what`: which
 * argument, which environment entry, the cwd). ASCII only, so the message
 * itself always converts. */
static void conv_error(char *err, int errcap, const char *what, int rc, unsigned long bad)
{
    if (rc == CONV_LOSSY)
        fmt(err, (size_t)errcap,
            "exec: %s has the character U+%04lX, which the guest's code page %u "
            "cannot represent; nothing was run",
            what, bad, (unsigned)GetACP());
    else if (rc == CONV_NOT_UTF8)
        fmt(err, (size_t)errcap, "exec: %s is not valid UTF-8; nothing was run", what);
    else
        fmt(err, (size_t)errcap, "exec: %s too long", what);
}

/* ---- the channel ---------------------------------------------------- */

int port_open(const char *spec, char *err, int errcap)
{
    char path[64];
    DCB dcb;
    COMMTIMEOUTS to;
    /* "COM1" works through COM9; the \\.\ form works for any. */
    if (strncmp(spec, "\\\\.\\", 4) == 0)
        fmt(path, sizeof path, "%s", spec);
    else
        fmt(path, sizeof path, "\\\\.\\%s", spec);
    port = CreateFileA(path, GENERIC_READ | GENERIC_WRITE, 0, NULL, OPEN_EXISTING, 0, NULL);
    if (port == INVALID_HANDLE_VALUE) {
        port_error = GetLastError();
        fmt(err, (size_t)errcap, "open %s: error %lu", spec, (unsigned long)port_error);
        return -1;
    }
    SetupComm(port, 65536, 65536);
    memset(&dcb, 0, sizeof dcb);
    dcb.DCBlength = sizeof dcb;
    GetCommState(port, &dcb);
    dcb.BaudRate = CBR_115200;
    dcb.ByteSize = 8;
    dcb.Parity = NOPARITY;
    dcb.StopBits = ONESTOPBIT;
    dcb.fBinary = TRUE;
    dcb.fParity = FALSE;
    dcb.fOutxCtsFlow = FALSE;
    dcb.fOutxDsrFlow = FALSE;
    dcb.fDtrControl = DTR_CONTROL_ENABLE;
    dcb.fDsrSensitivity = FALSE;
    dcb.fOutX = FALSE;
    dcb.fInX = FALSE;
    dcb.fNull = FALSE;
    dcb.fRtsControl = RTS_CONTROL_ENABLE;
    dcb.fAbortOnError = FALSE;
    if (!SetCommState(port, &dcb)) {
        fmt(err, (size_t)errcap, "configure %s: error %lu", spec, (unsigned long)GetLastError());
        CloseHandle(port);
        port = INVALID_HANDLE_VALUE;
        return -1;
    }
    /* Return immediately with what is buffered: a poll, not a wait. */
    to.ReadIntervalTimeout = MAXDWORD;
    to.ReadTotalTimeoutMultiplier = 0;
    to.ReadTotalTimeoutConstant = 0;
    to.WriteTotalTimeoutMultiplier = 0;
    to.WriteTotalTimeoutConstant = 0;
    SetCommTimeouts(port, &to);
    PurgeComm(port, PURGE_RXCLEAR | PURGE_TXCLEAR);
    return 0;
}

long port_read(void *buf, long cap)
{
    DWORD got = 0;
    if (!ReadFile(port, buf, (DWORD)cap, &got, NULL)) {
        DWORD e = GetLastError();
        /* A line error is not a lost port: clear it and carry on. */
        if (e == ERROR_OPERATION_ABORTED || e == ERROR_IO_PENDING) {
            DWORD errs;
            ClearCommError(port, &errs, NULL);
            return 0;
        }
        {
            char msg[64];
            fmt(msg, sizeof msg, "read: error %lu", (unsigned long)e);
            plat_log(msg);
        }
        return -1;
    }
    return (long)got;
}

int port_write(const void *buf, long len)
{
    const char *p = buf;
    while (len > 0) {
        DWORD wrote = 0;
        if (!WriteFile(port, p, (DWORD)len, &wrote, NULL))
            return -1;
        p += wrote;
        len -= (long)wrote;
    }
    return 0;
}

void port_close(void)
{
    if (port != INVALID_HANDLE_VALUE)
        CloseHandle(port);
    port = INVALID_HANDLE_VALUE;
}

void plat_sleep_ms(int ms)
{
    Sleep((DWORD)ms);
}

/* ---- processes ------------------------------------------------------ */

struct plat_proc {
    HANDLE process;
    HANDLE in_w;  /* our end of the child's stdin */
    HANDLE out_r; /* our end of the child's stdout */
    HANDLE err_r;
    int out_closed;
    int err_closed;
    /* stdin writer thread */
    HANDLE writer;
    /* Set by the writer as it leaves; `detached` by proc_free when the
     * writer outlived its wait, handing the struct over for it to free. */
    int writer_done;
    int detached;
    CRITICAL_SECTION lock;
    HANDLE wake;
    char *pending;
    long pending_len;
    long pending_cap;
    int in_eof;
    int quit;
    unsigned long consumed;
    int exited;
    long code;
};

/* Quote one argument the way CommandLineToArgvW / the CRT undo it,
 * appending at `*len` and always leaving room for the terminator. -1 when
 * it does not fit: a truncated command line runs a different command. */
static int quote_arg(const char *arg, char *out, int cap, int *len)
{
    int need = strchr(arg, ' ') || strchr(arg, '\t') || strchr(arg, '"') || !*arg;
    int bs = 0;
    const char *p;
#define PUT(c) do { if (*len + 1 >= cap) return -1; out[(*len)++] = (c); } while (0)
    if (!need) {
        for (p = arg; *p; p++)
            PUT(*p);
        return 0;
    }
    PUT('"');
    for (p = arg; *p; p++) {
        if (*p == '\\') {
            bs++;
            continue;
        }
        if (*p == '"') {
            int i;
            for (i = 0; i < bs * 2 + 1; i++)
                PUT('\\');
            bs = 0;
            PUT('"');
            continue;
        }
        while (bs > 0) {
            PUT('\\');
            bs--;
        }
        PUT(*p);
    }
    while (bs > 0) {
        PUT('\\');
        PUT('\\');
        bs--;
    }
    PUT('"');
#undef PUT
    return 0;
}

/* The agent's environment with `KEY=VALUE` overrides applied (keys compared
 * case-insensitively, as Windows does). Caller frees. NULL with the reason
 * in `err`: no memory, or an override the code page cannot hold. */
static char *build_env_block(char **env, int nenv, char *err, int errcap)
{
    char *base = GetEnvironmentStringsA();
    char *out, *w;
    const char *e;
    size_t total = 1;
    int i;
    if (!base) {
        fmt(err, (size_t)errcap, "exec: out of memory");
        return NULL;
    }
    for (e = base; *e; e += strlen(e) + 1)
        total += strlen(e) + 1;
    for (i = 0; i < nenv; i++)
        total += strlen(env[i]) * 3 + 1;
    out = malloc(total);
    if (!out) {
        FreeEnvironmentStringsA(base);
        fmt(err, (size_t)errcap, "exec: out of memory");
        return NULL;
    }
    w = out;
    for (e = base; *e; e += strlen(e) + 1) {
        const char *eq = strchr(e + 1, '='); /* a leading '=' is a drive cwd entry */
        int overridden = 0;
        if (eq) {
            for (i = 0; i < nenv; i++) {
                const char *oeq = strchr(env[i], '=');
                size_t klen = (size_t)(eq - e);
                if (oeq && (size_t)(oeq - env[i]) == klen &&
                    _strnicmp(env[i], e, klen) == 0) {
                    overridden = 1;
                    break;
                }
            }
        }
        if (!overridden) {
            strcpy(w, e);
            w += strlen(e) + 1;
        }
    }
    for (i = 0; i < nenv; i++) {
        /* An entry that does not convert exactly fails the exec: the command
         * was written for the environment the host sent, not a part of it. */
        unsigned long bad;
        int rc = utf8_to_ansi(env[i], w, (int)(strlen(env[i]) * 3 + 1), &bad);
        if (rc < 0) {
            char what[32];
            fmt(what, sizeof what, "env entry %d", i);
            conv_error(err, errcap, what, rc, bad);
            FreeEnvironmentStringsA(base);
            free(out);
            return NULL;
        }
        if (*w)
            w += strlen(w) + 1;
    }
    *w = 0;
    FreeEnvironmentStringsA(base);
    return out;
}

/* What proc_free and a detached writer both release: everything but the
 * child's process and output handles, which proc_free closes itself. */
static void free_writer_side(struct plat_proc *p)
{
    if (p->wake)
        CloseHandle(p->wake);
    DeleteCriticalSection(&p->lock);
    if (p->in_w != INVALID_HANDLE_VALUE)
        CloseHandle(p->in_w);
    free(p->pending);
    free(p);
}

/* The writer's last touch of `p`. Once it has set writer_done and seen no
 * hand-over, proc_free owns the struct; after a hand-over it is the
 * writer's to free. */
static void writer_exit(struct plat_proc *p)
{
    int detached;
    EnterCriticalSection(&p->lock);
    p->writer_done = 1;
    detached = p->detached;
    LeaveCriticalSection(&p->lock);
    if (detached)
        free_writer_side(p);
}

static DWORD WINAPI stdin_writer(LPVOID arg)
{
    struct plat_proc *p = arg;
    for (;;) {
        char *buf = NULL;
        long len = 0;
        int eof, quit;
        WaitForSingleObject(p->wake, 200);
        EnterCriticalSection(&p->lock);
        quit = p->quit;
        eof = p->in_eof;
        if (p->pending_len > 0) {
            buf = p->pending;
            len = p->pending_len;
            p->pending = NULL;
            p->pending_len = 0;
            p->pending_cap = 0;
        }
        LeaveCriticalSection(&p->lock);
        if (quit) {
            free(buf);
            break;
        }
        if (buf) {
            long off = 0;
            int broken = 0;
            while (off < len && !broken) {
                DWORD wrote = 0;
                if (!WriteFile(p->in_w, buf + off, (DWORD)(len - off), &wrote, NULL))
                    broken = 1;
                else
                    off += (long)wrote;
            }
            free(buf);
            EnterCriticalSection(&p->lock);
            /* Credit flows for what the host sent even if the child closed
             * its end, so the host never stalls on a child that stopped
             * reading. */
            p->consumed += (unsigned long)len;
            LeaveCriticalSection(&p->lock);
            if (broken)
                eof = 1;
        }
        if (eof) {
            EnterCriticalSection(&p->lock);
            if (p->pending_len == 0) {
                if (p->in_w != INVALID_HANDLE_VALUE)
                    CloseHandle(p->in_w);
                p->in_w = INVALID_HANDLE_VALUE;
                p->quit = 1;
            }
            LeaveCriticalSection(&p->lock);
        }
    }
    writer_exit(p);
    return 0;
}

static void close_if_open(HANDLE h)
{
    if (h != INVALID_HANDLE_VALUE)
        CloseHandle(h);
}

/* An inheritable pipe end for the child plus a private one for us. 9x has
 * no SetHandleInformation, so the private end is made by duplication. */

static int make_pipe(HANDLE *ours, HANDLE *theirs, int ours_is_read)
{
    SECURITY_ATTRIBUTES sa;
    HANDLE r, w, tmp;
    sa.nLength = sizeof sa;
    sa.lpSecurityDescriptor = NULL;
    sa.bInheritHandle = TRUE;
    if (!CreatePipe(&r, &w, &sa, 65536))
        return -1;
    tmp = ours_is_read ? r : w;
    if (!DuplicateHandle(GetCurrentProcess(), tmp, GetCurrentProcess(), ours, 0, FALSE,
                         DUPLICATE_SAME_ACCESS)) {
        CloseHandle(r);
        CloseHandle(w);
        return -1;
    }
    CloseHandle(tmp);
    *theirs = ours_is_read ? w : r;
    return 0;
}

struct plat_proc *proc_start(const struct exec_spec *spec, char *err, int errcap)
{
    static char cmdline[32768];
    static char cwd[1024];
    int len = 0, i;
    HANDLE in_r, out_w, err_w;
    STARTUPINFOA si;
    PROCESS_INFORMATION pi;
    struct plat_proc *p;
    char *envblock;
    DWORD flags;
    DWORD tid;

    for (i = 0; i < spec->argc; i++) {
        static char ansi[CMDLINE_CAP];
        unsigned long bad;
        int rc = utf8_to_ansi(spec->argv[i], ansi, (int)sizeof ansi, &bad);
        if (rc == CONV_LOSSY || rc == CONV_NOT_UTF8) {
            char what[32];
            fmt(what, sizeof what, "argument %d", i);
            conv_error(err, errcap, what, rc, bad);
            return NULL;
        }
        if (rc < 0 || (i && len + 1 >= (int)sizeof cmdline)) {
            fmt(err, (size_t)errcap, "exec: command line too long");
            return NULL;
        }
        if (i)
            cmdline[len++] = ' ';
        if (quote_arg(ansi, cmdline, (int)sizeof cmdline, &len) < 0) {
            fmt(err, (size_t)errcap, "exec: command line too long");
            return NULL;
        }
    }
    cmdline[len] = 0;
    if (spec->cwd) {
        unsigned long bad;
        int rc = utf8_to_ansi(spec->cwd, cwd, (int)sizeof cwd, &bad);
        if (rc < 0) {
            conv_error(err, errcap, "cwd", rc, bad);
            return NULL;
        }
    }

    p = calloc(1, sizeof *p);
    if (!p) {
        fmt(err, (size_t)errcap, "exec: out of memory");
        return NULL;
    }
    p->in_w = p->out_r = p->err_r = INVALID_HANDLE_VALUE;
    in_r = out_w = err_w = INVALID_HANDLE_VALUE;
    if (make_pipe(&p->in_w, &in_r, 0) < 0 || make_pipe(&p->out_r, &out_w, 1) < 0 ||
        make_pipe(&p->err_r, &err_w, 1) < 0) {
        fmt(err, (size_t)errcap, "pipe: error %lu", (unsigned long)GetLastError());
        close_if_open(in_r);
        close_if_open(out_w);
        close_if_open(err_w);
        close_if_open(p->in_w);
        close_if_open(p->out_r);
        close_if_open(p->err_r);
        free(p);
        return NULL;
    }

    memset(&si, 0, sizeof si);
    si.cb = sizeof si;
    si.dwFlags = STARTF_USESTDHANDLES | STARTF_USESHOWWINDOW;
    si.wShowWindow = SW_HIDE;
    si.hStdInput = in_r;
    si.hStdOutput = out_w;
    si.hStdError = err_w;
    /* NT hides a console outright; 9x has no such flag and needs a console
     * for command.com, so it gets a new hidden one. */
    flags = is_win9x ? CREATE_NEW_CONSOLE : CREATE_NO_WINDOW;
    envblock = build_env_block(spec->env, spec->nenv, err, errcap);
    if (!envblock || !CreateProcessA(NULL, cmdline, NULL, NULL, TRUE, flags, envblock,
                                     spec->cwd ? cwd : NULL, &si, &pi)) {
        if (envblock)
            fmt(err, (size_t)errcap, "exec %s: error %lu",
                spec->argv[0], (unsigned long)GetLastError());
        free(envblock);
        CloseHandle(in_r);
        CloseHandle(out_w);
        CloseHandle(err_w);
        CloseHandle(p->in_w);
        CloseHandle(p->out_r);
        CloseHandle(p->err_r);
        free(p);
        return NULL;
    }
    free(envblock);
    CloseHandle(pi.hThread);
    CloseHandle(in_r);
    CloseHandle(out_w);
    CloseHandle(err_w);
    p->process = pi.hProcess;

    InitializeCriticalSection(&p->lock);
    p->wake = CreateEventA(NULL, FALSE, FALSE, NULL);
    p->writer = p->wake ? CreateThread(NULL, 0, stdin_writer, p, 0, &tid) : NULL;
    if (!p->writer) {
        /* No writer, no stdin: refuse rather than leave a child nothing
         * can feed or free. */
        fmt(err, (size_t)errcap, "exec: cannot start the stdin writer: error %lu",
            (unsigned long)GetLastError());
        TerminateProcess(p->process, 137);
        CloseHandle(p->process);
        CloseHandle(p->out_r);
        CloseHandle(p->err_r);
        free_writer_side(p);
        return NULL;
    }
    return p;
}

long proc_read(struct plat_proc *p, int which, void *buf, long cap)
{
    HANDLE h = which ? p->err_r : p->out_r;
    int *closed = which ? &p->err_closed : &p->out_closed;
    DWORD avail = 0, got = 0;
    if (*closed)
        return -1;
    if (!PeekNamedPipe(h, NULL, 0, NULL, &avail, NULL)) {
        *closed = 1;
        return -1;
    }
    if (avail == 0) {
        /* A 9x anonymous pipe never reports a broken pipe: once the child has
         * gone, PeekNamedPipe keeps succeeding with nothing available, so the
         * end of output is never seen and the exec never finishes. What the
         * child wrote before it exited is already in the pipe, so an empty
         * pipe after exit is the end. NT reports the broken pipe itself. */
        if (is_win9x && WaitForSingleObject(p->process, 0) == WAIT_OBJECT_0) {
            *closed = 1;
            return -1;
        }
        return 0;
    }
    if ((long)avail < cap)
        cap = (long)avail;
    if (!ReadFile(h, buf, (DWORD)cap, &got, NULL)) {
        *closed = 1;
        return -1;
    }
    return (long)got;
}

long proc_output_pending(struct plat_proc *p, int which)
{
    HANDLE h = which ? p->err_r : p->out_r;
    DWORD avail = 0;
    return PeekNamedPipe(h, NULL, 0, NULL, &avail, NULL) ? (long)avail : 0;
}

void proc_write_in(struct plat_proc *p, const void *buf, long len)
{
    long keep = 0;
    int dropped = 0;
    if (len <= 0)
        return;
    EnterCriticalSection(&p->lock);
    if (p->quit) {
        p->consumed += (unsigned long)len;
    } else {
        /* The writer takes the queue whole, so pending_len is what waits. */
        keep = PLAT_STDIN_CAP - p->pending_len;
        if (keep > len)
            keep = len;
        if (keep < 0)
            keep = 0;
        if (keep && p->pending_len + keep > p->pending_cap) {
            long cap = p->pending_cap ? p->pending_cap * 2 : 8192;
            char *grown;
            while (cap < p->pending_len + keep)
                cap *= 2;
            grown = realloc(p->pending, (size_t)cap);
            if (grown) {
                p->pending = grown;
                p->pending_cap = cap;
            } else {
                keep = 0;
            }
        }
        if (keep) {
            memcpy(p->pending + p->pending_len, buf, (size_t)keep);
            p->pending_len += keep;
        }
        if (keep < len) {
            /* Credited back, so the host never stalls on what was dropped. */
            p->consumed += (unsigned long)(len - keep);
            dropped = 1;
        }
    }
    LeaveCriticalSection(&p->lock);
    if (dropped)
        plat_log("stdin queue full or out of memory, bytes dropped");
    SetEvent(p->wake);
}

unsigned long proc_consumed(struct plat_proc *p)
{
    unsigned long n;
    EnterCriticalSection(&p->lock);
    n = p->consumed;
    p->consumed = 0;
    LeaveCriticalSection(&p->lock);
    return n;
}

void proc_close_in(struct plat_proc *p)
{
    EnterCriticalSection(&p->lock);
    p->in_eof = 1;
    LeaveCriticalSection(&p->lock);
    SetEvent(p->wake);
}

int proc_exited(struct plat_proc *p, long *code)
{
    if (!p->exited) {
        DWORD c;
        if (WaitForSingleObject(p->process, 0) == WAIT_OBJECT_0) {
            p->exited = 1;
            p->code = GetExitCodeProcess(p->process, &c) ? (long)c : 255;
        }
    }
    if (p->exited)
        *code = p->code;
    return p->exited;
}

void proc_kill(struct plat_proc *p)
{
    if (!p->exited)
        TerminateProcess(p->process, 137);
}

/* The writer can be stuck in WriteFile past its wait — a grandchild that
 * inherited the child's stdin and never reads it keeps the pipe open after
 * the child is gone. Freeing under it would be a use after free, so the
 * struct is then handed to the writer, which frees it whenever its write
 * returns (or leaks it, if never). */
void proc_free(struct plat_proc *p)
{
    int handed_over = 0;
    EnterCriticalSection(&p->lock);
    p->quit = 1;
    LeaveCriticalSection(&p->lock);
    SetEvent(p->wake);
    CloseHandle(p->out_r);
    CloseHandle(p->err_r);
    CloseHandle(p->process);
    if (WaitForSingleObject(p->writer, 2000) != WAIT_OBJECT_0) {
        EnterCriticalSection(&p->lock);
        if (!p->writer_done) {
            p->detached = 1;
            handed_over = 1;
        }
        LeaveCriticalSection(&p->lock);
    }
    CloseHandle(p->writer);
    if (handed_over) {
        plat_log("stdin writer still blocked; it frees the exec when its write returns");
        return;
    }
    free_writer_side(p);
}

/* ---- the machine ---------------------------------------------------- */

const char *plat_os_tag(void)
{
    return "windows";
}

static void reg_string(HKEY root, const char *key, const char *value, char *dst, DWORD cap)
{
    HKEY h;
    DWORD type = 0, size = cap;
    dst[0] = 0;
    if (RegOpenKeyExA(root, key, 0, KEY_READ, &h) != ERROR_SUCCESS)
        return;
    if (RegQueryValueExA(h, value, NULL, &type, (LPBYTE)dst, &size) != ERROR_SUCCESS ||
        type != REG_SZ)
        dst[0] = 0;
    dst[cap - 1] = 0;
    RegCloseKey(h);
}

void plat_os_info(struct os_info *info)
{
    OSVERSIONINFOA v;
    SYSTEM_INFO si;
    DWORD n = sizeof info->hostname;
    memset(&v, 0, sizeof v);
    v.dwOSVersionInfoSize = sizeof v;
    GetVersionExA(&v);
    strcpy(info->id, "windows");
    reg_string(HKEY_LOCAL_MACHINE,
               is_win9x ? "SOFTWARE\\Microsoft\\Windows\\CurrentVersion"
                        : "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion",
               "ProductName", info->name, sizeof info->name);
    if (!info->name[0])
        strcpy(info->name, is_win9x ? "Windows" : "Windows NT");
    fmt(info->version, sizeof info->version, "%lu.%lu%s%s",
              (unsigned long)v.dwMajorVersion, (unsigned long)v.dwMinorVersion,
              v.szCSDVersion[0] ? " " : "", v.szCSDVersion);
    fmt(info->kernel, sizeof info->kernel, "%lu.%lu.%lu",
              (unsigned long)v.dwMajorVersion, (unsigned long)v.dwMinorVersion,
              (unsigned long)(is_win9x ? LOWORD(v.dwBuildNumber) : v.dwBuildNumber));
    GetSystemInfo(&si);
    strcpy(info->arch, si.wProcessorArchitecture == 9 ? "x86_64" : "x86");
    if (!GetComputerNameA(info->hostname, &n))
        info->hostname[0] = 0;
}

typedef BOOL(WINAPI *InitiateShutdownFn)(LPSTR, LPSTR, DWORD, BOOL, BOOL);
typedef BOOL(WINAPI *OpenProcessTokenFn)(HANDLE, DWORD, PHANDLE);
typedef BOOL(WINAPI *LookupPrivilegeFn)(LPCSTR, LPCSTR, PLUID);
typedef BOOL(WINAPI *AdjustPrivilegesFn)(HANDLE, BOOL, PTOKEN_PRIVILEGES, DWORD,
                                         PTOKEN_PRIVILEGES, PDWORD);

/* Enable one privilege on the agent's own token. NT only: 9x has no
 * tokens, and lacks these advapi32 entry points, hence the run-time
 * lookup. Best effort; the call it guards reports the failure. */
static void take_privilege(const char *name)
{
    HMODULE adv = GetModuleHandleA("advapi32.dll");
    OpenProcessTokenFn open_tok;
    LookupPrivilegeFn lookup;
    AdjustPrivilegesFn adjust;
    HANDLE tok;
    TOKEN_PRIVILEGES tp;
    if (!adv)
        return;
    open_tok = (OpenProcessTokenFn)GetProcAddress(adv, "OpenProcessToken");
    lookup = (LookupPrivilegeFn)GetProcAddress(adv, "LookupPrivilegeValueA");
    adjust = (AdjustPrivilegesFn)GetProcAddress(adv, "AdjustTokenPrivileges");
    if (!open_tok || !lookup || !adjust)
        return;
    if (!open_tok(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &tok))
        return;
    tp.PrivilegeCount = 1;
    tp.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
    if (lookup(NULL, name, &tp.Privileges[0].Luid))
        adjust(tok, FALSE, &tp, 0, NULL, NULL);
    CloseHandle(tok);
}

int plat_shutdown(const char *mode)
{
    int reboot = strcmp(mode, "reboot") == 0;
    if (is_win9x) {
        UINT flags = reboot ? EWX_REBOOT : EWX_SHUTDOWN;
        if (strcmp(mode, "powerdown") == 0)
            flags = EWX_POWEROFF;
        return ExitWindowsEx(flags | EWX_FORCE, 0) ? 0 : -1;
    }
    take_privilege("SeShutdownPrivilege");
    {
        HMODULE adv = GetModuleHandleA("advapi32.dll");
        InitiateShutdownFn init =
            adv ? (InitiateShutdownFn)GetProcAddress(adv, "InitiateSystemShutdownA") : NULL;
        /* InitiateSystemShutdown is what a service is meant to call; it
         * powers ACPI machines off. ExitWindowsEx is the fallback. */
        if (init && init(NULL, NULL, 0, TRUE, reboot ? TRUE : FALSE))
            return 0;
    }
    return ExitWindowsEx((reboot ? EWX_REBOOT : EWX_POWEROFF) | EWX_FORCE, 0) ? 0 : -1;
}

/* SetSystemTime takes UTC on 9x and NT alike; NT wants the privilege
 * enabled first, and LocalSystem holds it disabled. */
int plat_set_time(const struct plat_time *t, char *err, int errcap)
{
    SYSTEMTIME st;
    memset(&st, 0, sizeof st);
    st.wYear = (WORD)t->year;
    st.wMonth = (WORD)t->month;
    st.wDayOfWeek = (WORD)t->weekday;
    st.wDay = (WORD)t->day;
    st.wHour = (WORD)t->hour;
    st.wMinute = (WORD)t->minute;
    st.wSecond = (WORD)t->second;
    if (!is_win9x)
        take_privilege("SeSystemtimePrivilege");
    if (!SetSystemTime(&st)) {
        fmt(err, (size_t)errcap, "set_time: SetSystemTime: error %lu",
                  (unsigned long)GetLastError());
        return -1;
    }
    return 0;
}

void plat_log(const char *msg)
{
    char line[512];
    DWORD n;
    fmt(line, sizeof line, "vmlab-agent-legacy: %s\r\n", msg);
    OutputDebugStringA(line);
    if (console_mode)
        fprintf(stderr, "%s", line);
    if (log_file != INVALID_HANDLE_VALUE) {
        WriteFile(log_file, line, (DWORD)strlen(line), &n, NULL);
        /* The lines that matter most precede a forced restart, or the hang
         * Windows 95 can make of one, which the write-behind cache does not
         * survive. */
        FlushFileBuffers(log_file);
    }
}

/* ---- entry ---------------------------------------------------------- */

static char port_name[64] = "COM1";

/* 9x: Windows 95 Setup's hardware detection probes the serial ports for a
 * mouse, and the agent channel answering on COM1 can pass for one. Setup then
 * installs a serial mouse there, whose driver holds the port for good, so
 * every open is refused ("access denied") and the agent never serves. Remove
 * that legacy-detected device and restart once: nothing claims the port on
 * the next boot. A marker stops a second restart if that did not help.
 * Returns 1 when a restart is under way. */
static int reclaim_port_from_serial_mouse(void)
{
    static const char mouse[] = "Enum\\Root\\*PNP0F0C";
    HKEY key;
    DWORD done = 0, one = 1, type, len = sizeof done;
    if (RegOpenKeyExA(HKEY_LOCAL_MACHINE, "Software\\vmlab", 0, KEY_READ, &key) == ERROR_SUCCESS) {
        if (RegQueryValueExA(key, "PortReclaimed", NULL, &type, (BYTE *)&done, &len) != ERROR_SUCCESS)
            done = 0;
        RegCloseKey(key);
        if (done)
            return 0;
    }
    if (RegOpenKeyExA(HKEY_LOCAL_MACHINE, mouse, 0, KEY_READ, &key) != ERROR_SUCCESS)
        return 0;
    RegCloseKey(key);
    /* On 9x RegDeleteKey removes the whole subtree. */
    if (RegDeleteKeyA(HKEY_LOCAL_MACHINE, mouse) != ERROR_SUCCESS)
        return 0;
    if (RegCreateKeyExA(HKEY_LOCAL_MACHINE, "Software\\vmlab", 0, NULL, 0, KEY_WRITE, NULL, &key,
                        NULL) == ERROR_SUCCESS) {
        RegSetValueExA(key, "PortReclaimed", 0, REG_DWORD, (const BYTE *)&one, sizeof one);
        RegCloseKey(key);
    }
    plat_log("the port is held by a serial mouse Setup detected on the agent channel; "
             "removed it, restarting");
    /* Windows 95 can hang for good on its way down; the removal must already
     * be on disk when it does, so a forced reset still finds the port free. */
    RegFlushKey(HKEY_LOCAL_MACHINE);
    ExitWindowsEx(EWX_REBOOT | EWX_FORCE, 0);
    return 1;
}

/* 9x (OSR2 and later): serenum.vxd probes each COM port at boot for a Plug
 * and Play serial device, and the host's hellos queued on the agent channel
 * can answer that probe. The enumerator then keeps the port, and every open
 * is refused for the whole boot. The agent's port is not a place a device can
 * be plugged in, so its enumerator is removed from the port's driver key.
 * That takes effect on the next boot; the template build boots enough times
 * that a sealed image carries it. */
static void stop_enumerating_port(void)
{
    static const char *const buses[] = {"Enum\\Root", "Enum\\BIOS"};
    char dev[128], inst[128], path[320], name[64], driver[128];
    HKEY bus, devkey, key;
    DWORD i, j, len, type;
    int b;
    for (b = 0; b < 2; b++) {
        if (RegOpenKeyExA(HKEY_LOCAL_MACHINE, buses[b], 0, KEY_READ, &bus) != ERROR_SUCCESS)
            continue;
        for (i = 0; RegEnumKeyA(bus, i, dev, sizeof dev) == ERROR_SUCCESS; i++) {
            if (RegOpenKeyExA(bus, dev, 0, KEY_READ, &devkey) != ERROR_SUCCESS)
                continue;
            for (j = 0; RegEnumKeyA(devkey, j, inst, sizeof inst) == ERROR_SUCCESS; j++) {
                if (RegOpenKeyExA(devkey, inst, 0, KEY_READ, &key) != ERROR_SUCCESS)
                    continue;
                len = sizeof name;
                if (RegQueryValueExA(key, "PortName", NULL, &type, (BYTE *)name, &len) != ERROR_SUCCESS ||
                    _stricmp(name, port_name) != 0) {
                    RegCloseKey(key);
                    continue;
                }
                len = sizeof driver;
                if (RegQueryValueExA(key, "Driver", NULL, &type, (BYTE *)driver, &len) != ERROR_SUCCESS) {
                    RegCloseKey(key);
                    continue;
                }
                RegCloseKey(key);
                fmt(path, sizeof path, "System\\CurrentControlSet\\Services\\Class\\%s", driver);
                if (RegOpenKeyExA(HKEY_LOCAL_MACHINE, path, 0, KEY_READ | KEY_WRITE, &key) == ERROR_SUCCESS) {
                    if (RegDeleteValueA(key, "Enumerator") == ERROR_SUCCESS) {
                        plat_log("stopped serial Plug and Play enumeration of the agent's port");
                        RegFlushKey(HKEY_LOCAL_MACHINE);
                    }
                    RegCloseKey(key);
                }
            }
            RegCloseKey(devkey);
        }
        RegCloseKey(bus);
    }
}

/* Serve forever: reopen the port when it fails, exit on a shutdown. */
static void serve(void)
{
    char err[256];
    for (;;) {
        if (port_open(port_name, err, (int)sizeof err) < 0) {
            plat_log(err);
            if (is_win9x && port_error == ERROR_ACCESS_DENIED && reclaim_port_from_serial_mouse())
                return;
            Sleep(2000);
            continue;
        }
        plat_log("serving");
        if (agent_run() == 0) {
            port_close();
            return;
        }
        port_close();
        Sleep(500);
    }
}

/* NT service plumbing, resolved at run time (absent on 9x). */
typedef SERVICE_STATUS_HANDLE(WINAPI *RegisterHandlerFn)(LPCSTR, LPHANDLER_FUNCTION);
typedef BOOL(WINAPI *SetStatusFn)(SERVICE_STATUS_HANDLE, LPSERVICE_STATUS);
typedef BOOL(WINAPI *StartDispatcherFn)(const SERVICE_TABLE_ENTRYA *);

static SERVICE_STATUS_HANDLE status_handle;
static SetStatusFn set_status;

static void report(DWORD state)
{
    SERVICE_STATUS st;
    memset(&st, 0, sizeof st);
    st.dwServiceType = SERVICE_WIN32_OWN_PROCESS;
    st.dwCurrentState = state;
    st.dwControlsAccepted = state == SERVICE_RUNNING ? SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN : 0;
    set_status(status_handle, &st);
}

static void WINAPI handler(DWORD ctl)
{
    if (ctl == SERVICE_CONTROL_STOP || ctl == SERVICE_CONTROL_SHUTDOWN) {
        report(SERVICE_STOP_PENDING);
        port_close();
        report(SERVICE_STOPPED);
        ExitProcess(0);
    }
}

static void WINAPI service_main(DWORD argc, LPSTR *argv)
{
    HMODULE adv = GetModuleHandleA("advapi32.dll");
    RegisterHandlerFn reg = (RegisterHandlerFn)GetProcAddress(adv, "RegisterServiceCtrlHandlerA");
    (void)argc;
    (void)argv;
    status_handle = reg("vmlab-agent", handler);
    report(SERVICE_RUNNING);
    serve();
    report(SERVICE_STOPPED);
}

static int run_as_nt_service(void)
{
    HMODULE adv = LoadLibraryA("advapi32.dll");
    StartDispatcherFn start;
    SERVICE_TABLE_ENTRYA table[2];
    if (!adv)
        return -1;
    start = (StartDispatcherFn)GetProcAddress(adv, "StartServiceCtrlDispatcherA");
    set_status = (SetStatusFn)GetProcAddress(adv, "SetServiceStatus");
    if (!start || !set_status)
        return -1;
    table[0].lpServiceName = "vmlab-agent";
    table[0].lpServiceProc = service_main;
    table[1].lpServiceName = NULL;
    table[1].lpServiceProc = NULL;
    if (!start(table))
        return -1; /* not launched by the SCM: run in the foreground */
    return 0;
}

/* 9x: hide from the task list and survive logoff (RunServices). */
static void register_9x_service(void)
{
    typedef DWORD(WINAPI *RegisterServiceProcessFn)(DWORD, DWORD);
    RegisterServiceProcessFn rsp =
        (RegisterServiceProcessFn)GetProcAddress(GetModuleHandleA("kernel32.dll"),
                                                 "RegisterServiceProcess");
    if (rsp)
        rsp(0, 1);
}

/* `--install`: register this binary, at its current path, to start with
 * the machine — an SCM service on NT (no `sc.exe` needed, which NT4 and
 * 2000 lack), the RunServices key on 9x — and start it now. */
typedef SC_HANDLE(WINAPI *OpenSCManagerFn)(LPCSTR, LPCSTR, DWORD);
typedef SC_HANDLE(WINAPI *CreateServiceFn)(SC_HANDLE, LPCSTR, LPCSTR, DWORD, DWORD, DWORD,
                                           DWORD, LPCSTR, LPCSTR, LPDWORD, LPCSTR, LPCSTR,
                                           LPCSTR);
typedef SC_HANDLE(WINAPI *OpenServiceFn)(SC_HANDLE, LPCSTR, DWORD);
typedef BOOL(WINAPI *ChangeServiceConfigFn)(SC_HANDLE, DWORD, DWORD, DWORD, LPCSTR, LPCSTR,
                                            LPDWORD, LPCSTR, LPCSTR, LPCSTR, LPCSTR);
typedef BOOL(WINAPI *StartServiceFn)(SC_HANDLE, DWORD, LPCSTR *);
typedef BOOL(WINAPI *CloseServiceHandleFn)(SC_HANDLE);

static int install_nt_service(const char *exe)
{
    HMODULE adv = LoadLibraryA("advapi32.dll");
    OpenSCManagerFn open_scm = (OpenSCManagerFn)GetProcAddress(adv, "OpenSCManagerA");
    CreateServiceFn create = (CreateServiceFn)GetProcAddress(adv, "CreateServiceA");
    OpenServiceFn open_svc = (OpenServiceFn)GetProcAddress(adv, "OpenServiceA");
    ChangeServiceConfigFn change = (ChangeServiceConfigFn)GetProcAddress(adv, "ChangeServiceConfigA");
    StartServiceFn start = (StartServiceFn)GetProcAddress(adv, "StartServiceA");
    CloseServiceHandleFn close_h = (CloseServiceHandleFn)GetProcAddress(adv, "CloseServiceHandle");
    SC_HANDLE scm, svc;
    if (!open_scm || !create || !open_svc || !change || !start || !close_h) {
        fprintf(stderr, "install: service APIs unavailable\n");
        return 1;
    }
    scm = open_scm(NULL, NULL, SC_MANAGER_ALL_ACCESS);
    if (!scm) {
        fprintf(stderr, "install: OpenSCManager failed: %lu\n", (unsigned long)GetLastError());
        return 1;
    }
    svc = create(scm, "vmlab-agent", "vmlab guest agent (legacy, COM1)", SERVICE_ALL_ACCESS,
                 SERVICE_WIN32_OWN_PROCESS, SERVICE_AUTO_START, SERVICE_ERROR_NORMAL, exe,
                 NULL, NULL, NULL, NULL, NULL);
    if (!svc && GetLastError() == ERROR_SERVICE_EXISTS) {
        /* A rebuild: point the existing service at this binary. */
        svc = open_svc(scm, "vmlab-agent", SERVICE_ALL_ACCESS);
        if (svc)
            change(svc, SERVICE_WIN32_OWN_PROCESS, SERVICE_AUTO_START, SERVICE_ERROR_NORMAL,
                   exe, NULL, NULL, NULL, NULL, NULL, NULL);
    }
    if (!svc) {
        fprintf(stderr, "install: CreateService failed: %lu\n", (unsigned long)GetLastError());
        close_h(scm);
        return 1;
    }
    if (!start(svc, 0, NULL) && GetLastError() != ERROR_SERVICE_ALREADY_RUNNING)
        fprintf(stderr, "install: StartService failed: %lu\n", (unsigned long)GetLastError());
    close_h(svc);
    close_h(scm);
    printf("vmlab-agent service installed: %s\n", exe);
    return 0;
}

static int install_9x(const char *exe)
{
    HKEY key;
    STARTUPINFOA si;
    PROCESS_INFORMATION pi;
    static char cmd[1024];
    if (RegCreateKeyExA(HKEY_LOCAL_MACHINE,
                        "Software\\Microsoft\\Windows\\CurrentVersion\\RunServices", 0, NULL,
                        0, KEY_WRITE, NULL, &key, NULL) != ERROR_SUCCESS) {
        fprintf(stderr, "install: cannot open RunServices\n");
        return 1;
    }
    RegSetValueExA(key, "vmlab-agent", 0, REG_SZ, (const BYTE *)exe, (DWORD)strlen(exe) + 1);
    RegCloseKey(key);
    /* Start it now, detached, so the build's handshake needs no reboot. */
    memset(&si, 0, sizeof si);
    si.cb = sizeof si;
    fmt(cmd, sizeof cmd, "\"%s\"", exe);
    if (CreateProcessA(NULL, cmd, NULL, NULL, FALSE, DETACHED_PROCESS, NULL, NULL, &si, &pi)) {
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
    printf("vmlab-agent registered in RunServices: %s\n", exe);
    return 0;
}

/* `--secure-dir <dir>`: the NT service runs as LocalSystem from <dir>, and
 * a directory made under the root of C: inherits that root's permissions —
 * full control for Everyone on NT4 and Windows 2000 — so any local user
 * could replace the binary, or put a DLL beside it, and be SYSTEM at the
 * next boot. This gives the directory and what is already in it to
 * Administrators (owner) with full control for Administrators and SYSTEM
 * and nobody else; what is copied in afterwards inherits that. The install
 * script runs it from the ISO before the copy.
 *
 * The SIDs are written out rather than looked up by name, because the
 * group names are localised. The DACL is marked protected where the system
 * knows the bit (Windows 2000 on), so a later change to the root of the
 * drive does not flow back in. */
#ifndef SE_DACL_PROTECTED
#define SE_DACL_PROTECTED 0x1000
#endif
#ifndef FS_PERSISTENT_ACLS
#define FS_PERSISTENT_ACLS 0x00000008
#endif

/* A SID with room for two sub-authorities, laid out as winnt.h's SID. */
typedef struct {
    BYTE Revision;
    BYTE SubAuthorityCount;
    BYTE IdentifierAuthority[6];
    DWORD SubAuthority[2];
} FixedSid;
static FixedSid sid_system = {1, 1, {0, 0, 0, 0, 0, 5}, {18, 0}};  /* S-1-5-18 */
static FixedSid sid_admins = {1, 2, {0, 0, 0, 0, 0, 5}, {32, 544}}; /* S-1-5-32-544 */

typedef BOOL(WINAPI *InitializeAclFn)(PACL, DWORD, DWORD);
typedef BOOL(WINAPI *AddAccessAllowedAceFn)(PACL, DWORD, DWORD, PSID);
typedef BOOL(WINAPI *GetAceFn)(PACL, DWORD, LPVOID *);
typedef BOOL(WINAPI *InitializeSecurityDescriptorFn)(PSECURITY_DESCRIPTOR, DWORD);
typedef BOOL(WINAPI *SetSecurityDescriptorDaclFn)(PSECURITY_DESCRIPTOR, BOOL, PACL, BOOL);
typedef BOOL(WINAPI *SetSecurityDescriptorOwnerFn)(PSECURITY_DESCRIPTOR, PSID, BOOL);
typedef BOOL(WINAPI *SetSecurityDescriptorControlFn)(PSECURITY_DESCRIPTOR, WORD, WORD);
typedef BOOL(WINAPI *SetFileSecurityFn)(LPCSTR, SECURITY_INFORMATION, PSECURITY_DESCRIPTOR);

static struct {
    InitializeAclFn init_acl;
    AddAccessAllowedAceFn add_ace;
    GetAceFn get_ace;
    InitializeSecurityDescriptorFn init_sd;
    SetSecurityDescriptorDaclFn set_dacl;
    SetSecurityDescriptorOwnerFn set_owner;
    SetSecurityDescriptorControlFn set_control; /* NULL on NT4 */
    SetFileSecurityFn set_file;
} sec;

/* Owner, then DACL: the owner may always write the DACL, so the second
 * call does not depend on what the old DACL granted. 0, or -1 with the
 * failing call's error left in GetLastError. */
static int secure_path(const char *path, int is_dir)
{
    static DWORD acl_buf[32]; /* DWORD-aligned, 128 bytes; the ACL needs 52 */
    PACL acl = (PACL)acl_buf;
    SECURITY_DESCRIPTOR sd;
    DWORD i;
    if (!sec.init_acl(acl, sizeof acl_buf, ACL_REVISION) ||
        !sec.add_ace(acl, ACL_REVISION, FILE_ALL_ACCESS, (PSID)&sid_admins) ||
        !sec.add_ace(acl, ACL_REVISION, FILE_ALL_ACCESS, (PSID)&sid_system))
        return -1;
    if (is_dir) {
        /* AddAccessAllowedAceEx is Windows 2000 on; the flags are set by
         * hand so NT4 gets inheriting entries too. */
        for (i = 0; i < 2; i++) {
            LPVOID ace;
            if (!sec.get_ace(acl, i, &ace))
                return -1;
            ((ACE_HEADER *)ace)->AceFlags = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;
        }
    }
    if (!sec.init_sd(&sd, SECURITY_DESCRIPTOR_REVISION) ||
        !sec.set_owner(&sd, (PSID)&sid_admins, FALSE) ||
        !sec.set_file(path, OWNER_SECURITY_INFORMATION, &sd))
        return -1;
    if (!sec.init_sd(&sd, SECURITY_DESCRIPTOR_REVISION) ||
        !sec.set_dacl(&sd, TRUE, acl, FALSE))
        return -1;
    if (sec.set_control && !sec.set_control(&sd, SE_DACL_PROTECTED, SE_DACL_PROTECTED))
        return -1;
    if (!sec.set_file(path, DACL_SECURITY_INFORMATION, &sd))
        return -1;
    return 0;
}

static int secure_dir(const char *dir)
{
    static char pattern[MAX_PATH], child[MAX_PATH];
    char root[4];
    const char *root_arg = NULL;
    DWORD attrs, fs_flags = 0;
    HMODULE adv;
    HANDLE find;
    WIN32_FIND_DATAA fd;
    int failed = 0;
    size_t len = strlen(dir);

    if (is_win9x) {
        printf("secure-dir: Windows 9x has no file permissions, nothing to do\n");
        return 0;
    }
    attrs = GetFileAttributesA(dir);
    if (attrs == 0xFFFFFFFFUL || !(attrs & FILE_ATTRIBUTE_DIRECTORY)) {
        fprintf(stderr, "secure-dir: %s is not a directory\n", dir);
        return 1;
    }
    /* Never a drive root: this takes access away from every other user. */
    if (len < 4 || dir[1] != ':' || dir[2] != '\\' || len + 3 > sizeof pattern) {
        fprintf(stderr, "secure-dir: %s: give a directory as X:\\name\n", dir);
        return 1;
    }
    root[0] = dir[0];
    root[1] = ':';
    root[2] = '\\';
    root[3] = 0;
    root_arg = root;
    if (!GetVolumeInformationA(root_arg, NULL, 0, NULL, NULL, &fs_flags, NULL, 0)) {
        fprintf(stderr, "secure-dir: cannot read the volume of %s: %lu\n", dir,
                (unsigned long)GetLastError());
        return 1;
    }
    if (!(fs_flags & FS_PERSISTENT_ACLS)) {
        /* FAT: nothing can be set, and nothing protects any other file of
         * this guest either. Said, not failed. */
        printf("secure-dir: warning: the volume of %s keeps no permissions (FAT); "
               "any user of this guest can replace the agent\n", dir);
        return 0;
    }
    adv = LoadLibraryA("advapi32.dll");
    sec.init_acl = (InitializeAclFn)GetProcAddress(adv, "InitializeAcl");
    sec.add_ace = (AddAccessAllowedAceFn)GetProcAddress(adv, "AddAccessAllowedAce");
    sec.get_ace = (GetAceFn)GetProcAddress(adv, "GetAce");
    sec.init_sd = (InitializeSecurityDescriptorFn)GetProcAddress(adv, "InitializeSecurityDescriptor");
    sec.set_dacl = (SetSecurityDescriptorDaclFn)GetProcAddress(adv, "SetSecurityDescriptorDacl");
    sec.set_owner = (SetSecurityDescriptorOwnerFn)GetProcAddress(adv, "SetSecurityDescriptorOwner");
    sec.set_control =
        (SetSecurityDescriptorControlFn)GetProcAddress(adv, "SetSecurityDescriptorControl");
    sec.set_file = (SetFileSecurityFn)GetProcAddress(adv, "SetFileSecurityA");
    if (!sec.init_acl || !sec.add_ace || !sec.get_ace || !sec.init_sd || !sec.set_dacl ||
        !sec.set_owner || !sec.set_file) {
        fprintf(stderr, "secure-dir: security APIs unavailable\n");
        return 1;
    }
    /* The directory first, so nothing new gets in under the old ACL. */
    if (secure_path(dir, 1) != 0) {
        fprintf(stderr, "secure-dir: cannot secure %s: %lu\n", dir,
                (unsigned long)GetLastError());
        return 1;
    }
    /* Then what is already there (a rebuild's binary keeps its own ACL when
     * `copy /y` writes over it). One level: the install makes no more. */
    fmt(pattern, sizeof pattern, "%s\\*", dir);
    find = FindFirstFileA(pattern, &fd);
    if (find != INVALID_HANDLE_VALUE) {
        do {
            if (strcmp(fd.cFileName, ".") == 0 || strcmp(fd.cFileName, "..") == 0)
                continue;
            if (len + 1 + strlen(fd.cFileName) >= sizeof child) {
                fprintf(stderr, "secure-dir: name too long under %s\n", dir);
                failed = 1;
                continue;
            }
            fmt(child, sizeof child, "%s\\%s", dir, fd.cFileName);
            if (secure_path(child, (fd.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) != 0) != 0) {
                fprintf(stderr, "secure-dir: cannot secure %s: %lu\n", child,
                        (unsigned long)GetLastError());
                failed = 1;
            }
        } while (FindNextFileA(find, &fd));
        FindClose(find);
    }
    if (failed)
        return 1;
    printf("secured %s: Administrators and SYSTEM only\n", dir);
    return 0;
}

int main(int argc, char **argv)
{
    int i;
    is_win9x = (GetVersion() & 0x80000000UL) != 0;
    if (argc == 3 && strcmp(argv[1], "--secure-dir") == 0)
        return secure_dir(argv[2]);
    if (argc == 2 && strcmp(argv[1], "--install") == 0) {
        static char exe[MAX_PATH];
        GetModuleFileNameA(NULL, exe, sizeof exe);
        return is_win9x ? install_9x(exe) : install_nt_service(exe);
    }
    for (i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--console") == 0) {
            console_mode = 1;
        } else if (strcmp(argv[i], "--port") == 0 && i + 1 < argc) {
            fmt(port_name, sizeof port_name, "%s", argv[++i]);
        } else if (strcmp(argv[i], "--log") == 0 && i + 1 < argc) {
            log_file = CreateFileA(argv[++i], GENERIC_WRITE, FILE_SHARE_READ, NULL,
                                   OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
            if (log_file != INVALID_HANDLE_VALUE)
                SetFilePointer(log_file, 0, NULL, FILE_END);
        } else {
            fprintf(stderr, "usage: vmlab-agent-legacy [--console] [--port COMn] [--log file]\n"
                            "       vmlab-agent-legacy --install\n"
                            "       vmlab-agent-legacy --secure-dir X:\\dir\n");
            return 2;
        }
    }
    if (!console_mode) {
        if (is_win9x)
            register_9x_service();
        else if (run_as_nt_service() == 0)
            return 0;
    }
    if (is_win9x)
        stop_enumerating_port();
    serve();
    return 0;
}
