// Interaction latency (card #447): how long after the pointer moves does the
// desktop show what a program drew in answer?
//
// A borderless window (800x480 at 32,32) of eight columns, each 100 pixels
// wide. When the pointer enters a column the whole window is filled with that
// column's colour of the eight-colour palette, with GDI, the way a program's
// ordinary user interface draws: the picture reaches the screen through DWM.
// The host moves the pointer from column to column and reads the window from
// the console (tests/e2e/adapter/latency.py); the time from its pointer event
// to the frame with the new colour is the measurement.
//
// This program reports its own part of that time: from the pointer message
// to the return of DwmFlush after the fill, which is the wait for the
// composition that shows it.
#include "adapter.h"
#include <dwmapi.h>
#include <algorithm>

// The host has the same table (tests/e2e/adapter/latency.py).
static const COLORREF palette[8] = {RGB(255, 0, 0),   RGB(0, 255, 0),   RGB(0, 0, 255),     RGB(255, 255, 0),
                                    RGB(255, 0, 255), RGB(0, 255, 255), RGB(255, 255, 255), RGB(96, 96, 96)};
static const int X = 32, Y = 32, W = 800, H = 480;
static int column = 7;
static std::vector<double> compose_ms;
static unsigned moves;

static void fill(HWND h)
{
    HDC dc = GetDC(h);
    RECT rc = {0, 0, W, H};
    HBRUSH b = CreateSolidBrush(palette[column]);
    FillRect(dc, &rc, b);
    DeleteObject(b);
    ReleaseDC(h, dc);
}

static LRESULT CALLBACK wndproc(HWND h, UINT m, WPARAM w, LPARAM l)
{
    if (m == WM_MOUSEMOVE) {
        moves++;
        int c = std::min(7, std::max(0, (int)(short)LOWORD(l) * 8 / W));
        if (c != column) {
            double t0 = now_ms();
            column = c;
            fill(h);
            GdiFlush();
            DwmFlush();
            compose_ms.push_back(now_ms() - t0);
        }
        return 0;
    }
    if (m == WM_PAINT) {
        PAINTSTRUCT ps;
        BeginPaint(h, &ps);
        EndPaint(h, &ps);
        fill(h);
        return 0;
    }
    if (m == WM_ERASEBKGND)
        return 1;
    return adapter_wndproc(h, m, w, l);
}

int main(int argc, char **argv)
{
    Report r("latency");
    const char *out = arg_str(argc, argv, "--out", "latency_probe.json");
    int seconds = arg_int(argc, argv, "--seconds", 60);

    WNDCLASSW wc = {};
    wc.lpfnWndProc = wndproc;
    wc.hInstance = GetModuleHandleW(NULL);
    wc.lpszClassName = L"vmlab-win3d-latency";
    RegisterClassW(&wc);
    HWND hwnd = CreateWindowExW(WS_EX_TOPMOST, wc.lpszClassName, L"vmlab-win3d adapter: latency", WS_POPUP, X, Y, W,
                                H, NULL, NULL, wc.hInstance, NULL);
    if (!hwnd) {
        r.err("CreateWindowEx failed: %lu", GetLastError());
        return r.finish(out);
    }
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    SetWindowPos(hwnd, HWND_TOPMOST, X, Y, W, H, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    fill(hwnd);

    double start = now_ms();
    while (now_ms() - start < seconds * 1000.0) {
        pump();
        MsgWaitForMultipleObjects(0, NULL, FALSE, 5, QS_ALLINPUT);
    }

    BOOL composed = FALSE;
    DwmIsCompositionEnabled(&composed);
    r.raw("window", "{\"x\": 32, \"y\": 32, \"width\": 800, \"height\": 480}");
    r.boolean("dwm_composition", composed);
    r.integer("pointer_messages", moves);
    r.integer("colour_changes", (long long)compose_ms.size());
    if (!compose_ms.empty()) {
        std::sort(compose_ms.begin(), compose_ms.end());
        size_t n = compose_ms.size();
        double sum = 0;
        for (double v : compose_ms)
            sum += v;
        char b[256];
        snprintf(b, sizeof b, "{\"n\": %u, \"mean\": %.3f, \"p50\": %.3f, \"p90\": %.3f, \"p99\": %.3f, \"max\": %.3f}",
                 (unsigned)n, sum / n, compose_ms[n / 2], compose_ms[std::min(n - 1, n * 9 / 10)],
                 compose_ms[std::min(n - 1, n * 99 / 100)], compose_ms[n - 1]);
        r.raw("compose_ms", b);
    }
    DestroyWindow(hwnd);
    return r.finish(out);
}
