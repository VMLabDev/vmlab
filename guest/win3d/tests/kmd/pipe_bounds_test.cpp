// Host test of the message read of the pipe between the two helpers of the
// kernel driver package, vgpusrv.exe and viogpuap.exe (card #757, finding F9
// of docs/reviews/win3d-driver.md).
//
// It compiles viogpu/shared/pipe_bounds.h, the header that
// patches/kmd/kmd-read-helper-pipe-message-inside-buffer.patch adds and that
// PipeServer::Read and PipeClient::Read call. scripts/test_kmd_pipe_bounds.py
// takes the header out of the patch, so the test needs no driver source tree.
//
// With -DOLD_DRIVER the same checks run against the two read loops the
// helpers had before the patch. They must fail there: that is the "before"
// of the proof. The exit status is the number of failed checks.
//
// The buffer under test is the first 1024 units of a larger array. The rest
// is a guard with a known pattern, so a write past the buffer is counted and
// is no fault of the test itself.
#include <cstdio>
#include <cstring>
#include <vector>

#ifndef OLD_DRIVER
#include "pipe_bounds.h"
#endif

typedef unsigned short unit; // wchar_t of Windows
static const unsigned int LENGTH = 1024;                  // DATA_BUFFER_LENGTH
static const unsigned int BYTES = LENGTH * sizeof(unit);  // 2048
static const unsigned int GUARD_UNITS = 4096;
static const unit PATTERN = 0xA5A5;

// A pipe as ReadFile shows it. In message read mode a read that is smaller
// than the rest of the message returns what fits and "more data". In byte
// read mode it returns what fits and succeeds, and the next read gets the
// rest. A pipe with no message left fails the read (a closed pipe).
struct Pipe
{
    std::vector<std::vector<unsigned char>> messages;
    size_t at = 0, offset = 0;
    bool message_mode = true;
    unsigned int reads = 0;

    // -1 error, 0 more data, 1 the read is complete
    int read(void *destination, unsigned int room, unsigned int *got)
    {
        *got = 0;
        if (++reads > 100000 || at >= messages.size()) return -1;
        const std::vector<unsigned char> &m = messages[at];
        const size_t left = m.size() - offset;
        const size_t n = left < room ? left : room;
        if (n) std::memcpy(destination, m.data() + offset, n);
        *got = (unsigned int)n;
        offset += n;
        if (offset == m.size())
        {
            ++at;
            offset = 0;
            return 1;
        }
        return message_mode ? 0 : 1;
    }
};

enum { FAILED = 0, OK = 1, TOO_LONG = 2 };

#ifdef OLD_DRIVER
// viogpu/shared/pipe.cpp:126-156 before the patch. DATA_BUFFER_SIZE was
// "DATA_BUFFER_LENGTH * sizeof(wchar_t)" without parentheses, so
// "DATA_BUFFER_SIZE * sizeof(wchar_t)" is 1024 * 2 * 2.
static int ReadServer(unit *m_buffer, Pipe &pipe)
{
    unsigned int drBytes = 0;
    bool bFinishedRead = false;
    int read = 0;
    int left = LENGTH * sizeof(unit) * sizeof(unit);
    do
    {
        int piece = pipe.read(&m_buffer[read], (unsigned int)left, &drBytes);
        bFinishedRead = piece == 1;
        if (piece < 0) break;
        read += drBytes;
        left -= drBytes;
    } while (!bFinishedRead);
    return (!bFinishedRead || 0 == drBytes) ? FAILED : OK;
}

// viogpu/shared/pipe.cpp:292-318 before the patch.
static int ReadClient(unit *m_buffer, Pipe &pipe)
{
    unsigned int drBytes = 0;
    bool bFinishedRead = false;
    int read = 0;
    do
    {
        int piece = pipe.read(&m_buffer[read], BYTES, &drBytes);
        bFinishedRead = piece == 1;
        if (piece < 0) break;
        read += drBytes;
    } while (!bFinishedRead);
    return (!bFinishedRead || 0 == drBytes) ? FAILED : OK;
}
#else
static int ReadBoth(unit *m_buffer, Pipe &pipe)
{
    return VioGpuPipeReadMessage(m_buffer, LENGTH, [&pipe](void *destination, unsigned int room, unsigned int *got) {
        return pipe.read(destination, room, got);
    });
}
static int ReadServer(unit *m_buffer, Pipe &pipe) { return ReadBoth(m_buffer, pipe); }
static int ReadClient(unit *m_buffer, Pipe &pipe) { return ReadBoth(m_buffer, pipe); }
#endif

typedef int (*Reader)(unit *, Pipe &);

// The buffer and the guard behind it.
struct Arena
{
    std::vector<unit> units;
    // The helpers allocate the buffer with new wchar_t[]: its content is not defined.
    explicit Arena(unit fill = 0x4141) : units(LENGTH + GUARD_UNITS, PATTERN)
    {
        for (unsigned int i = 0; i < LENGTH; ++i) units[i] = fill;
    }
    unit *buffer() { return units.data(); }
    unsigned int overrun() const // the bytes of the guard that changed
    {
        unsigned int n = 0;
        const unsigned char *p = (const unsigned char *)(units.data() + LENGTH);
        for (unsigned int i = 0; i < GUARD_UNITS * sizeof(unit); ++i) n += p[i] != 0xA5;
        return n;
    }
    // The units GetData() takes: up to the first zero unit, also past the buffer.
    unsigned int text_units() const
    {
        unsigned int n = 0;
        while (n < units.size() && units[n]) ++n;
        return n;
    }
};

static int failed;

static void check(bool ok, const char *what)
{
    std::printf("%s %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) ++failed;
}

// A message of n bytes, none of them zero.
static std::vector<unsigned char> filled(size_t n)
{
    std::vector<unsigned char> m(n);
    for (size_t i = 0; i < n; ++i) m[i] = (unsigned char)('a' + i % 26);
    return m;
}

// What PipeServer::Write and PipeClient::Write send for a text: its units
// and one more byte, the low half of the terminator.
static std::vector<unsigned char> sent(const char *text)
{
    std::vector<unsigned char> m;
    for (const char *p = text; *p; ++p)
    {
        m.push_back((unsigned char)*p);
        m.push_back(0);
    }
    m.push_back(0);
    return m;
}

static bool holds(Arena &a, const char *text)
{
    const size_t n = std::strlen(text);
    if (a.text_units() != n) return false;
    for (size_t i = 0; i < n; ++i)
        if (a.buffer()[i] != (unit)text[i]) return false;
    return true;
}

int main()
{
    const Reader readers[] = {ReadServer, ReadClient};
    const char *const names[] = {"server", "client"};
    char what[200];

    for (int r = 0; r < 2; ++r)
    {
        const Reader read = readers[r];

        // What a correct peer sends, into a buffer that happens to be zero.
        {
            Arena a(0);
            Pipe p;
            p.messages = {sent("\\CLOSE")};
            const int result = read(a.buffer(), p);
            std::snprintf(what, sizeof(what), "%s: a command arrives whole in a zeroed buffer", names[r]);
            check(result == OK && holds(a, "\\CLOSE") && a.overrun() == 0, what);
        }
        {
            Arena a;
            Pipe p; // no message: the read fails
            std::snprintf(what, sizeof(what), "%s: a failed read is a failure", names[r]);
            check(read(a.buffer(), p) == FAILED && a.overrun() == 0, what);
        }
        {
            Arena a;
            Pipe p;
            p.messages = {std::vector<unsigned char>()};
            std::snprintf(what, sizeof(what), "%s: an empty message is a failure, as before", names[r]);
            check(read(a.buffer(), p) == FAILED && a.overrun() == 0, what);
        }

        // F9. The buffer of the helpers is not zeroed and the sender's
        // terminator is half a unit.
        {
            Arena a;
            Pipe p;
            p.messages = {sent("\\CLOSE")};
            const int result = read(a.buffer(), p);
            std::snprintf(what, sizeof(what), "F9: %s: a command is a terminated string in a buffer that held other data", names[r]);
            check(result == OK && holds(a, "\\CLOSE"), what);
        }
        // F9. The message of the review: 3000 bytes in one write.
        {
            Arena a;
            Pipe p;
            p.messages = {filled(3000), sent("\\CONNECTED")};
            const int result = read(a.buffer(), p);
            std::snprintf(what, sizeof(what), "F9: %s: a message of 3000 bytes writes nothing past the buffer", names[r]);
            check(a.overrun() == 0, what);
            std::snprintf(what, sizeof(what), "F9: %s: a message of 3000 bytes is refused and leaves an empty string", names[r]);
            check(result == TOO_LONG && a.text_units() == 0, what);
            const int next = read(a.buffer(), p);
            std::snprintf(what, sizeof(what), "F9: %s: the message after a refused one is read from its first byte", names[r]);
            check(next == OK && holds(a, "\\CONNECTED") && a.overrun() == 0, what);
        }
        // F9. A message that fills the buffer leaves no terminator.
        {
            Arena a;
            Pipe p;
            p.messages = {filled(BYTES)};
            read(a.buffer(), p);
            std::snprintf(what, sizeof(what), "F9: %s: a message of 2048 bytes leaves a string that ends inside the buffer", names[r]);
            check(a.text_units() < LENGTH && a.overrun() == 0, what);
        }
        // The longest message that fits.
        {
            Arena a;
            Pipe p;
            p.messages = {filled(BYTES - sizeof(unit))};
            const int result = read(a.buffer(), p);
            std::snprintf(what, sizeof(what), "F9: %s: a message of 2046 bytes is taken whole and terminated", names[r]);
            check(result == OK && a.text_units() == LENGTH - 1 && a.overrun() == 0, what);
        }
        // A handle in byte read mode (what CreateFile gives the client).
        {
            Arena a;
            Pipe p;
            p.message_mode = false;
            p.messages = {filled(3000)};
            read(a.buffer(), p);
            std::snprintf(what, sizeof(what), "F9: %s: 3000 bytes in byte read mode leave a string that ends inside the buffer", names[r]);
            check(a.text_units() < LENGTH && a.overrun() == 0, what);
        }
        // Every length the loops of the old helpers end on.
        {
            bool inside = true, terminated = true, verdict = true;
            for (unsigned int n = 1; n <= 2 * BYTES; ++n)
            {
                Arena a;
                Pipe p;
                p.messages = {filled(n)};
                const int result = read(a.buffer(), p);
                if (a.overrun()) inside = false;
                if (a.text_units() >= LENGTH) terminated = false;
                if (result != (n <= BYTES - sizeof(unit) ? OK : TOO_LONG)) verdict = false;
            }
            std::snprintf(what, sizeof(what), "F9: %s: no message of 1 to 4096 bytes writes past the buffer", names[r]);
            check(inside, what);
            std::snprintf(what, sizeof(what), "F9: %s: every message of 1 to 4096 bytes leaves a terminated string", names[r]);
            check(terminated, what);
            std::snprintf(what, sizeof(what), "F9: %s: a message is taken up to 2046 bytes and refused above", names[r]);
            check(verdict, what);
        }
#ifndef OLD_DRIVER
        // The old server loop does not end on a message above 4096 bytes: it
        // asks for 0 bytes again and again. So this runs on the patch only.
        {
            Arena a;
            Pipe p;
            p.messages = {filled(1000000), sent("\\CLOSED")};
            const int result = read(a.buffer(), p);
            const int next = read(a.buffer(), p);
            std::snprintf(what, sizeof(what), "%s: a message of 1000000 bytes is refused and the next one is read", names[r]);
            check(result == TOO_LONG && next == OK && holds(a, "\\CLOSED") && a.overrun() == 0, what);
        }
        {
            Arena a;
            Pipe p;
            p.messages = {filled(3000)}; // the pipe closes in the middle of the rest
            p.reads = 100000 - 1;
            std::snprintf(what, sizeof(what), "%s: a read that fails while the rest is thrown away is a failure", names[r]);
            check(read(a.buffer(), p) == FAILED && a.text_units() == 0 && a.overrun() == 0, what);
        }
#endif
    }

#ifndef OLD_DRIVER
    // A reader that reports more bytes than it was given room for.
    {
        Arena a;
        const int result = VioGpuPipeReadMessage(a.buffer(), LENGTH, [](void *, unsigned int room, unsigned int *got) {
            *got = room + 1;
            return (int)VIOGPU_PIPE_PIECE_MORE;
        });
        check(result == FAILED && a.text_units() == 0 && a.overrun() == 0, "a piece longer than the room given is a failure");
    }
    check(VioGpuPipeMessageRoom<unit>(LENGTH) == 2046 && VioGpuPipeMessageRoom<unit>(1) == 0 && VioGpuPipeMessageRoom<unit>(0) == 0,
          "the room of a buffer is its length less one unit");
    {
        unit one[2] = {PATTERN, PATTERN};
        Pipe p;
        p.messages = {filled(10)};
        const int result = VioGpuPipeReadMessage(one, 1u, [&p](void *d, unsigned int room, unsigned int *got) { return p.read(d, room, got); });
        check(result == TOO_LONG && one[0] == 0 && one[1] == PATTERN, "a buffer of one unit takes no message");
    }
#endif

    std::printf("%d failed\n", failed);
    return failed > 100 ? 100 : failed;
}
