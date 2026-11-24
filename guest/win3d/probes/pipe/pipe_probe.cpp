/* Drives the pipe classes of the two helpers of the kernel driver package,
 * vgpusrv.exe (PipeServer) and viogpuap.exe (PipeClient), with the messages
 * of card #757 (docs/reviews/win3d-driver.md F9). It is linked with
 * viogpu/shared/pipe.cpp of the driver source: build/probes/build.sh pipe
 * makes pipe_probe_before.exe from the source without
 * patches/kmd/kmd-read-helper-pipe-message-inside-buffer.patch and
 * pipe_probe_after.exe from the source with it.
 *
 * One case per run. The probe talks to a pipe of its own name. It does not
 * touch the pipe of the installed service, except in the case "access",
 * which only tries to open it.
 *
 * In every case but "service-thread" the buffer of the object is replaced
 * by 2048 bytes at the end of a page, with two guard pages of a known
 * pattern behind them. So a write past the buffer is counted and harms
 * nothing, and a string that runs past the buffer is measured. In
 * "service-thread" the object runs as the service runs it, thread and heap
 * buffer included, and the probe asks Windows whether the heap is whole.
 *
 * Exit status: 0 pass, 2 fail, 1 the probe could not run, 3 usage. */
#include <windows.h>
#include <stdio.h>
#include <string.h>
#include <wchar.h>
#include <io.h>
#include <string>

/* The read and the buffer are private members. The probe needs both. */
#define private public
#include "pipe.h"
#undef private
#include "log.h"

void PrintMessage(LPCWSTR, ...) {}

static const unsigned BUFFER_BYTES = DATA_BUFFER_LENGTH * sizeof(wchar_t); /* 2048 */
static const unsigned GUARD_BYTES = 2 * 4096;
static const unsigned char GUARD_FILL = 0xA5;

struct Arena
{
    unsigned char *base;
    wchar_t *buffer;
    unsigned char *guard;

    bool make()
    {
        base = (unsigned char *)VirtualAlloc(NULL, 4096 + GUARD_BYTES, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if (!base) return false;
        buffer = (wchar_t *)(base + 4096 - BUFFER_BYTES);
        guard = base + 4096;
        /* A buffer from new wchar_t[] is not zeroed. */
        for (unsigned i = 0; i < DATA_BUFFER_LENGTH; ++i) buffer[i] = 0x4141;
        memset(guard, GUARD_FILL, GUARD_BYTES);
        /* The last unit of the guard ends a string that runs through all of it. */
        guard[GUARD_BYTES - 2] = guard[GUARD_BYTES - 1] = 0;
        return true;
    }
    unsigned changed() const
    {
        unsigned n = 0;
        for (unsigned i = 0; i < GUARD_BYTES - 2; ++i) n += guard[i] != GUARD_FILL;
        return n;
    }
    int first_changed() const
    {
        for (unsigned i = 0; i < GUARD_BYTES - 2; ++i)
            if (guard[i] != GUARD_FILL) return (int)i;
        return -1;
    }
};

struct Result
{
    const char *name = "", *expect = "", *result = "api_failure", *stage = "arguments";
    bool pass = false, measured = false;
    int read_first = -1, read_second = -1; /* what Read() returned: 1 true, 0 false, -1 not called */
    unsigned text_units = 0;               /* the length of the string GetData() gave */
    unsigned guard_changed = 0;
    int guard_first = -1;
    int heap_valid = -1;
    bool second_ok = false;
    DWORD win32_error = 0;
    std::string access;                    /* the case "access" */
};

static void emit(FILE *f, const Result &r)
{
    fprintf(f,
            "{\"schema\":1,\"probe\":\"pipe\",\"build\":\"%s\",\"case\":\"%s\",\"expect\":\"%s\",\"result\":\"%s\","
            "\"stage\":\"%s\",\"pass\":%s,\"read_returned\":%d,\"text_units\":%u,\"bytes_written_past_buffer\":%u,"
            "\"first_byte_past_buffer\":%d,\"second_read_returned\":%d,\"second_message_ok\":%s,\"heap_valid\":%d,"
            "\"win32_error\":%lu,\"pipes\":[%s]}\n",
            PIPE_PROBE_BUILD, r.name, r.expect, r.result, r.stage, r.pass ? "true" : "false", r.read_first,
            r.text_units, r.guard_changed, r.guard_first, r.read_second, r.second_ok ? "true" : "false",
            r.heap_valid, (unsigned long)r.win32_error, r.access.c_str());
}

static bool save(const wchar_t *path, const Result &r)
{
    if (!path) return true;
    FILE *f = _wfopen(path, L"wb");
    if (!f) return false;
    emit(f, r);
    bool ok = fflush(f) == 0 && _commit(_fileno(f)) == 0;
    if (fclose(f) != 0) ok = false;
    return ok;
}

/* n bytes, none of them zero. */
static std::string filled(size_t n)
{
    std::string m(n, 'a');
    for (size_t i = 0; i < n; ++i) m[i] = (char)('a' + i % 26);
    return m;
}

/* What PipeServer::Write and PipeClient::Write send for a text: its units
 * and one more byte. */
static std::string sent(const wchar_t *text)
{
    return std::string((const char *)text, wcslen(text) * sizeof(wchar_t) + 1);
}

static bool write_all(HANDLE pipe, const std::string &m)
{
    DWORD n = 0;
    return WriteFile(pipe, m.data(), (DWORD)m.size(), &n, NULL) && n == m.size();
}

/* The pipe as PipeServer::Init makes it (pipe.cpp:42-49). */
static HANDLE make_server_end(const std::wstring &name)
{
    return CreateNamedPipeW(name.c_str(), PIPE_ACCESS_DUPLEX, PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                            PIPE_UNLIMITED_INSTANCES, BUFFER_BYTES, BUFFER_BYTES, NMPWAIT_USE_DEFAULT_WAIT, NULL);
}

struct Case
{
    const char *name;
    bool server;       /* the object under test */
    bool message_mode; /* the read mode of the client handle when the client is under test */
    const char *expect;
};

static const Case cases[] = {
    {"server-command", true, false, "the command, as a terminated string, in a buffer that held other data"},
    {"server-long", true, false, "a message of 3000 bytes writes nothing past the buffer, is dropped, and the next message is read"},
    {"server-full", true, false, "a message of 2048 bytes leaves a string that ends inside the buffer"},
    {"service-thread", true, false, "the heap of the process is whole after the service thread read a message of 3000 bytes"},
    {"client-command", false, false, "the command, as a terminated string, in a buffer that held other data"},
    {"client-long", false, false, "3000 bytes on the handle CreateFile gives (byte read mode) leave a string that ends inside the buffer"},
    {"client-long-message-mode", false, true, "a message of 3000 bytes in message read mode writes nothing past the buffer"},
    {"access", false, false, "informational: who may open the pipes of the installed service"},
};

template <class Object> static void measure(Object &object, Arena &arena, Result &r)
{
    std::wstring text;
    object.GetData(text);
    r.text_units = (unsigned)text.size();
    r.guard_changed = arena.changed();
    r.guard_first = arena.first_changed();
    r.measured = true;
}

/* The pipes named viogpupipe<session>: can this user open one for writing? */
static void access_case(Result &r)
{
    WIN32_FIND_DATAW fd;
    HANDLE find = FindFirstFileW(L"\\\\.\\pipe\\*", &fd);
    r.stage = "list_pipes";
    if (find == INVALID_HANDLE_VALUE) { r.win32_error = GetLastError(); return; }
    do
    {
        if (_wcsnicmp(fd.cFileName, L"viogpupipe", 10)) continue;
        std::wstring path = L"\\\\.\\pipe\\";
        path += fd.cFileName;
        char entry[400];
        DWORD errors[2];
        const DWORD rights[2] = {GENERIC_READ | GENERIC_WRITE, GENERIC_READ};
        for (int i = 0; i < 2; ++i)
        {
            HANDLE h = CreateFileW(path.c_str(), rights[i], 0, NULL, OPEN_EXISTING, 0, NULL);
            errors[i] = h == INVALID_HANDLE_VALUE ? GetLastError() : 0;
            if (h != INVALID_HANDLE_VALUE) CloseHandle(h);
        }
        snprintf(entry, sizeof(entry), "%s{\"name\":\"%ls\",\"open_read_write_error\":%lu,\"open_read_error\":%lu}",
                 r.access.empty() ? "" : ",", fd.cFileName, (unsigned long)errors[0], (unsigned long)errors[1]);
        r.access += entry;
    } while (FindNextFileW(find, &fd));
    FindClose(find);
    r.stage = "done";
    r.result = "pass";
    r.pass = true;
}

int wmain(int argc, wchar_t **argv)
{
    Result r;
    const wchar_t *out = NULL;
    const Case *tc = NULL;
    int exit_code = 1;
    for (int i = 1; i < argc; ++i)
    {
        if (!wcscmp(argv[i], L"--out") && i + 1 < argc) out = argv[++i];
        else if (!wcscmp(argv[i], L"--case") && i + 1 < argc)
        {
            ++i;
            for (const Case &c : cases)
            {
                wchar_t wide[40];
                mbstowcs(wide, c.name, 40);
                if (!wcscmp(argv[i], wide)) tc = &c;
            }
            if (!tc) break;
        }
        else { tc = NULL; break; }
    }
    if (!tc)
    {
        fputs("usage: pipe_probe.exe --case (server-command | server-long | server-full | service-thread |"
              " client-command | client-long | client-long-message-mode | access) [--out FILE]\n", stderr);
        r.result = "usage";
        emit(stdout, r);
        return 3;
    }
    r.name = tc->name;
    r.expect = tc->expect;
    const std::string name = tc->name;

    if (name == "access")
    {
        access_case(r);
        emit(stdout, r);
        save(out, r);
        return r.pass ? 0 : 1;
    }

    std::wstring pipe_name = L"\\\\.\\pipe\\vmlab-c757-" + std::to_wstring(GetCurrentProcessId());
    const std::string first = name.find("command") != std::string::npos
                                  ? sent(tc->server ? CONNECTED : CLOSE)
                                  : filled(name == "server-full" ? BUFFER_BYTES : 3000);

    if (name == "service-thread")
    {
        /* The object as CSession::Init of the service uses it: Init() makes
         * the pipe and the thread that reads it into the heap buffer. */
        PipeServer *server = new PipeServer(pipe_name);
        r.stage = "init";
        if (!server->Init()) { r.win32_error = GetLastError(); goto done; }
        HANDLE peer = INVALID_HANDLE_VALUE;
        for (int i = 0; i < 50 && peer == INVALID_HANDLE_VALUE; ++i)
        {
            peer = CreateFileW(pipe_name.c_str(), GENERIC_READ | GENERIC_WRITE, 0, NULL, OPEN_EXISTING, 0, NULL);
            if (peer == INVALID_HANDLE_VALUE) Sleep(100);
        }
        r.stage = "connect";
        if (peer == INVALID_HANDLE_VALUE) { r.win32_error = GetLastError(); goto done; }
        /* The file then says "requesting" when the process dies of the overflow. */
        r.stage = "write";
        r.result = "requesting";
        save(out, r);
        if (!write_all(peer, first) || !write_all(peer, sent(CONNECTED))) { r.win32_error = GetLastError(); r.result = "api_failure"; goto done; }
        Sleep(1000); /* the thread reads both */
        r.heap_valid = HeapValidate(GetProcessHeap(), 0, NULL) ? 1 : 0;
        r.stage = "done";
        r.pass = r.heap_valid == 1;
        r.result = r.pass ? "pass" : "heap_corrupt";
        exit_code = r.pass ? 0 : 2;
        /* No Close() and no delete: with a damaged heap they would only hide the result. */
        emit(stdout, r);
        save(out, r);
        ExitProcess(exit_code);
    }

    {
        Arena arena;
        r.stage = "arena";
        if (!arena.make()) { r.win32_error = GetLastError(); goto done; }
        HANDLE server_end = make_server_end(pipe_name);
        r.stage = "create_pipe";
        if (server_end == INVALID_HANDLE_VALUE) { r.win32_error = GetLastError(); goto done; }
        r.result = "requesting";

        if (tc->server)
        {
            PipeServer server(pipe_name);
            wchar_t *own = server.m_buffer;
            server.m_buffer = arena.buffer;
            server.m_hPipe = server_end;
            HANDLE peer = CreateFileW(pipe_name.c_str(), GENERIC_READ | GENERIC_WRITE, 0, NULL, OPEN_EXISTING, 0, NULL);
            r.stage = "connect";
            if (peer == INVALID_HANDLE_VALUE || !server.WaitForClient()) { r.win32_error = GetLastError(); server.m_buffer = own; r.result = "api_failure"; goto done; }
            r.stage = "write";
            if (!write_all(peer, first) || !write_all(peer, sent(CONNECTED))) { r.win32_error = GetLastError(); server.m_buffer = own; r.result = "api_failure"; goto done; }
            r.stage = "read";
            save(out, r);
            r.read_first = server.Read() ? 1 : 0;
            measure(server, arena, r);
            if (name == "server-command")
            {
                r.pass = r.read_first == 1 && r.text_units == wcslen(CONNECTED) && !wcscmp(arena.buffer, CONNECTED) && r.guard_changed == 0;
            }
            else
            {
                /* The message behind the long one. A read that took the long
                 * message in pieces finds the rest of it here instead. */
                memset(arena.guard, GUARD_FILL, GUARD_BYTES - 2);
                r.read_second = server.Read() ? 1 : 0;
                std::wstring second;
                server.GetData(second);
                r.second_ok = r.read_second == 1 && second == CONNECTED;
                r.pass = r.guard_changed == 0 && r.text_units < DATA_BUFFER_LENGTH && r.second_ok &&
                         (name != "server-long" || (r.read_first == 1 && r.text_units == 0));
            }
            server.m_buffer = own;
            server.m_hPipe = INVALID_HANDLE_VALUE;
            CloseHandle(peer);
        }
        else
        {
            PipeClient client(pipe_name);
            wchar_t *own = client.m_buffer;
            client.m_buffer = arena.buffer;
            client.ConnectToServer();
            r.stage = "connect";
            if (client.m_hPipe == INVALID_HANDLE_VALUE || (!ConnectNamedPipe(server_end, NULL) && GetLastError() != ERROR_PIPE_CONNECTED))
            {
                r.win32_error = GetLastError(); client.m_buffer = own; r.result = "api_failure"; goto done;
            }
            if (tc->message_mode)
            {
                /* viogpuap.exe does not do this: its handle stays in byte
                 * read mode. The case shows what the loop does when it gets
                 * ERROR_MORE_DATA. */
                DWORD mode = PIPE_READMODE_MESSAGE;
                r.stage = "message_mode";
                if (!SetNamedPipeHandleState(client.m_hPipe, &mode, NULL, NULL)) { r.win32_error = GetLastError(); client.m_buffer = own; r.result = "api_failure"; goto done; }
            }
            r.stage = "write";
            if (!write_all(server_end, first)) { r.win32_error = GetLastError(); client.m_buffer = own; r.result = "api_failure"; goto done; }
            r.stage = "read";
            save(out, r);
            r.read_first = client.Read() ? 1 : 0;
            measure(client, arena, r);
            if (name == "client-command")
                r.pass = r.read_first == 1 && r.text_units == wcslen(CLOSE) && !wcscmp(arena.buffer, CLOSE) && r.guard_changed == 0;
            else
                r.pass = r.guard_changed == 0 && r.text_units < DATA_BUFFER_LENGTH;
            client.m_buffer = own;
            CloseHandle(client.m_hPipe);
            client.m_hPipe = INVALID_HANDLE_VALUE;
        }
        CloseHandle(server_end);
        r.stage = "done";
        r.result = r.pass ? "pass" : "fail";
        exit_code = r.pass ? 0 : 2;
    }

done:
    emit(stdout, r);
    if (!save(out, r) && exit_code == 0) exit_code = 1;
    return exit_code;
}
