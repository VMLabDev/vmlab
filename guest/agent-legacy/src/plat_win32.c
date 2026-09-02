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

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "agent.h"
#include "plat.h"

static HANDLE port = INVALID_HANDLE_VALUE;
static int console_mode = 0;
static int is_win9x = 0;
static HANDLE log_file = INVALID_HANDLE_VALUE;

/* ---- text ----------------------------------------------------------- */

/* UTF-8 (the wire) -> the ANSI code page (every A API), by hand through
 * UTF-16 so no code page support is assumed of the guest. */
static void utf8_to_ansi(const char *src, char *dst, int cap)
{
    WCHAR wide[2048];
    int n = 0;
    const unsigned char *s = (const unsigned char *)src;
    while (*s && n < 2046) {
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
            cp = '?';
            extra = 0;
        }
        s++;
        while (extra-- > 0 && (*s & 0xC0) == 0x80)
            cp = (cp << 6) | (*s++ & 0x3F);
        if (cp >= 0x10000) {
            cp -= 0x10000;
            wide[n++] = (WCHAR)(0xD800 | (cp >> 10));
            wide[n++] = (WCHAR)(0xDC00 | (cp & 0x3FF));
        } else {
            wide[n++] = (WCHAR)cp;
        }
    }
    wide[n] = 0;
    if (cap > 0) {
        int r = WideCharToMultiByte(CP_ACP, 0, wide, n, dst, cap - 1, NULL, NULL);
        dst[r < 0 ? 0 : r] = 0;
    }
}

/* ---- the channel ---------------------------------------------------- */

int port_open(const char *spec, char *err, int errcap)
{
    char path[64];
    DCB dcb;
    COMMTIMEOUTS to;
    /* "COM1" works through COM9; the \\.\ form works for any. */
    if (strncmp(spec, "\\\\.\\", 4) == 0)
        _snprintf(path, sizeof path, "%s", spec);
    else
        _snprintf(path, sizeof path, "\\\\.\\%s", spec);
    port = CreateFileA(path, GENERIC_READ | GENERIC_WRITE, 0, NULL, OPEN_EXISTING, 0, NULL);
    if (port == INVALID_HANDLE_VALUE) {
        _snprintf(err, (size_t)errcap, "open %s: error %lu", spec, (unsigned long)GetLastError());
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
        _snprintf(err, (size_t)errcap, "configure %s: error %lu", spec, (unsigned long)GetLastError());
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

/* Quote one argument the way CommandLineToArgvW / the CRT undo it. */
static void quote_arg(const char *arg, char *out, int cap, int *len)
{
    int need = strchr(arg, ' ') || strchr(arg, '\t') || strchr(arg, '"') || !*arg;
    int bs = 0;
    const char *p;
#define PUT(c) do { if (*len + 1 < cap) out[(*len)++] = (c); } while (0)
    if (!need) {
        for (p = arg; *p; p++)
            PUT(*p);
        return;
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
}

/* The agent's environment with `KEY=VALUE` overrides applied (keys compared
 * case-insensitively, as Windows does). Caller frees. */
static char *build_env_block(char **env, int nenv)
{
    char *base = GetEnvironmentStringsA();
    char *out, *w;
    const char *e;
    size_t total = 1;
    int i;
    for (e = base; *e; e += strlen(e) + 1)
        total += strlen(e) + 1;
    for (i = 0; i < nenv; i++)
        total += strlen(env[i]) * 3 + 1;
    out = malloc(total);
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
        utf8_to_ansi(env[i], w, (int)(strlen(env[i]) * 3 + 1));
        w += strlen(w) + 1;
    }
    *w = 0;
    FreeEnvironmentStringsA(base);
    return out;
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
    return 0;
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
        char ansi[4096];
        utf8_to_ansi(spec->argv[i], ansi, sizeof ansi);
        if (i)
            cmdline[len++] = ' ';
        quote_arg(ansi, cmdline, (int)sizeof cmdline, &len);
    }
    cmdline[len] = 0;
    if (spec->cwd)
        utf8_to_ansi(spec->cwd, cwd, sizeof cwd);

    p = calloc(1, sizeof *p);
    p->in_w = p->out_r = p->err_r = INVALID_HANDLE_VALUE;
    if (make_pipe(&p->in_w, &in_r, 0) < 0 || make_pipe(&p->out_r, &out_w, 1) < 0 ||
        make_pipe(&p->err_r, &err_w, 1) < 0) {
        _snprintf(err, (size_t)errcap, "pipe: error %lu", (unsigned long)GetLastError());
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
    envblock = build_env_block(spec->env, spec->nenv);
    if (!CreateProcessA(NULL, cmdline, NULL, NULL, TRUE, flags, envblock,
                        spec->cwd ? cwd : NULL, &si, &pi)) {
        _snprintf(err, (size_t)errcap, "exec %s: error %lu",
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
    p->writer = CreateThread(NULL, 0, stdin_writer, p, 0, &tid);
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
    if (avail == 0)
        return 0;
    if ((long)avail < cap)
        cap = (long)avail;
    if (!ReadFile(h, buf, (DWORD)cap, &got, NULL)) {
        *closed = 1;
        return -1;
    }
    return (long)got;
}

void proc_write_in(struct plat_proc *p, const void *buf, long len)
{
    EnterCriticalSection(&p->lock);
    if (p->quit) {
        p->consumed += (unsigned long)len;
    } else {
        if (p->pending_len + len > p->pending_cap) {
            long cap = p->pending_cap ? p->pending_cap * 2 : 8192;
            while (cap < p->pending_len + len)
                cap *= 2;
            p->pending = realloc(p->pending, (size_t)cap);
            p->pending_cap = cap;
        }
        memcpy(p->pending + p->pending_len, buf, (size_t)len);
        p->pending_len += len;
    }
    LeaveCriticalSection(&p->lock);
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

void proc_free(struct plat_proc *p)
{
    EnterCriticalSection(&p->lock);
    p->quit = 1;
    LeaveCriticalSection(&p->lock);
    SetEvent(p->wake);
    WaitForSingleObject(p->writer, 2000);
    CloseHandle(p->writer);
    CloseHandle(p->wake);
    DeleteCriticalSection(&p->lock);
    if (p->in_w != INVALID_HANDLE_VALUE)
        CloseHandle(p->in_w);
    CloseHandle(p->out_r);
    CloseHandle(p->err_r);
    CloseHandle(p->process);
    free(p->pending);
    free(p);
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
    _snprintf(info->version, sizeof info->version, "%lu.%lu%s%s",
              (unsigned long)v.dwMajorVersion, (unsigned long)v.dwMinorVersion,
              v.szCSDVersion[0] ? " " : "", v.szCSDVersion);
    _snprintf(info->kernel, sizeof info->kernel, "%lu.%lu.%lu",
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

static void take_shutdown_privilege(void)
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
    if (lookup(NULL, "SeShutdownPrivilege", &tp.Privileges[0].Luid))
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
    take_shutdown_privilege();
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

void plat_log(const char *msg)
{
    char line[512];
    DWORD n;
    _snprintf(line, sizeof line, "vmlab-agent-legacy: %s\r\n", msg);
    OutputDebugStringA(line);
    if (console_mode)
        fprintf(stderr, "%s", line);
    if (log_file != INVALID_HANDLE_VALUE)
        WriteFile(log_file, line, (DWORD)strlen(line), &n, NULL);
}

/* ---- entry ---------------------------------------------------------- */

static char port_name[64] = "COM1";

/* Serve forever: reopen the port when it fails, exit on a shutdown. */
static void serve(void)
{
    char err[256];
    for (;;) {
        if (port_open(port_name, err, (int)sizeof err) < 0) {
            plat_log(err);
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

int main(int argc, char **argv)
{
    int i;
    is_win9x = (GetVersion() & 0x80000000UL) != 0;
    for (i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--console") == 0) {
            console_mode = 1;
        } else if (strcmp(argv[i], "--port") == 0 && i + 1 < argc) {
            _snprintf(port_name, sizeof port_name, "%s", argv[++i]);
        } else if (strcmp(argv[i], "--log") == 0 && i + 1 < argc) {
            log_file = CreateFileA(argv[++i], GENERIC_WRITE, FILE_SHARE_READ, NULL,
                                   OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
            if (log_file != INVALID_HANDLE_VALUE)
                SetFilePointer(log_file, 0, NULL, FILE_END);
        } else {
            fprintf(stderr, "usage: vmlab-agent-legacy [--console] [--port COMn] [--log file]\n");
            return 2;
        }
    }
    if (!console_mode) {
        if (is_win9x)
            register_9x_service();
        else if (run_as_nt_service() == 0)
            return 0;
    }
    serve();
    return 0;
}
