// P1, a hardware pointer: is the pointer part of the frame?
//
// Fills the whole primary display with one colour (255,0,255) for --seconds
// and records where Windows says the pointer is. The host moves the pointer
// over the window (`vmlab vm mouse-move`) and takes console captures
// (tests/e2e/adapter.sh, check "pointer"). With a hardware pointer the
// capture is the flat colour, because the frame does not hold the pointer.
// Without one Windows draws the pointer into the frame and the capture shows
// it. The samples prove that the pointer was visible and where the host put
// it, so an absent pointer cannot pass as a hardware pointer.
#include "adapter.h"

int main(int argc, char **argv)
{
    Report r("pointer");
    const char *out = arg_str(argc, argv, "--out", "pointer_probe.json");
    int seconds = arg_int(argc, argv, "--seconds", 20);

    int w = GetSystemMetrics(SM_CXSCREEN), h = GetSystemMetrics(SM_CYSCREEN);
    HWND hwnd = make_window(L"vmlab-win3d adapter: pointer", 0, 0, w, h, CreateSolidBrush(RGB(255, 0, 255)));
    if (!hwnd) {
        r.err("CreateWindowEx failed: %lu", GetLastError());
        return r.finish(out);
    }
    r.integer("screen_width", w);
    r.integer("screen_height", h);
    r.integer("max_pointer_width", GetSystemMetrics(SM_CXCURSOR));
    r.integer("max_pointer_height", GetSystemMetrics(SM_CYCURSOR));

    std::string samples = "[";
    double start = now_ms(), next = 0;
    int n = 0;
    while (now_ms() - start < seconds * 1000.0) {
        pump();
        double t = now_ms() - start;
        if (t >= next) {
            CURSORINFO ci = {};
            ci.cbSize = sizeof ci;
            char b[160];
            if (GetCursorInfo(&ci))
                snprintf(b, sizeof b, "%s{\"t\": %.2f, \"x\": %ld, \"y\": %ld, \"showing\": %s}", n ? ", " : "",
                         t / 1000.0, (long)ci.ptScreenPos.x, (long)ci.ptScreenPos.y,
                         (ci.flags & CURSOR_SHOWING) ? "true" : "false");
            else
                snprintf(b, sizeof b, "%s{\"t\": %.2f, \"error\": %lu}", n ? ", " : "", t / 1000.0, GetLastError());
            samples += b;
            n++;
            next += 200;
        }
        Sleep(10);
    }
    r.raw("samples", samples + "]");
    DestroyWindow(hwnd);
    return r.finish(out);
}
