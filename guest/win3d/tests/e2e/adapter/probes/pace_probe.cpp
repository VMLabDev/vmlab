// P2, a vblank that belongs to the display: the vblank interval, the present
// rate, the scan line and the flip statistics.
//
// Four measurements on the default adapter, each for --seconds:
//   vblank   the interval between returns of IDXGIOutput::WaitForVBlank
//   present  the interval between returns of Present(1) on a flip-model swap
//            chain, and the frame rate that gives
//   stats    what DXGI's frame statistics counted over the same presents
//            (SyncRefreshCount and SyncQPCTime: the flip timestamps)
//   scanline D3DKMTGetScanLine, sampled for one second: how many different
//            lines it answered and whether it ever said "in vertical blank"
// The refresh rate they are compared with is the display mode's.
#include "adapter.h"
#include <dwmapi.h>

struct KmtOpenAdapterFromHdc {
    HDC hDc;
    UINT hAdapter;
    LUID AdapterLuid;
    UINT VidPnSourceId;
};
struct KmtGetScanLine {
    UINT hAdapter;
    UINT VidPnSourceId;
    BOOLEAN InVerticalBlank;
    UINT ScanLine;
};
struct KmtCloseAdapter {
    UINT hAdapter;
};

static void scan_line(Report &r)
{
    typedef LONG(WINAPI * open_fn)(KmtOpenAdapterFromHdc *);
    typedef LONG(WINAPI * scan_fn)(KmtGetScanLine *);
    typedef LONG(WINAPI * close_fn)(const KmtCloseAdapter *);
    HMODULE gdi = GetModuleHandleW(L"gdi32.dll");
    open_fn open = (open_fn)(void *)GetProcAddress(gdi, "D3DKMTOpenAdapterFromHdc");
    scan_fn scan = (scan_fn)(void *)GetProcAddress(gdi, "D3DKMTGetScanLine");
    close_fn close = (close_fn)(void *)GetProcAddress(gdi, "D3DKMTCloseAdapter");
    if (!open || !scan || !close) {
        r.raw("scanline", "{\"available\": false, \"why\": \"gdi32 has no D3DKMTGetScanLine\"}");
        return;
    }
    KmtOpenAdapterFromHdc oa = {};
    oa.hDc = GetDC(NULL);
    LONG st = open(&oa);
    ReleaseDC(NULL, oa.hDc);
    if (st < 0) {
        char b[96];
        snprintf(b, sizeof b, "{\"available\": false, \"why\": \"D3DKMTOpenAdapterFromHdc 0x%08lx\"}",
                 (unsigned long)st);
        r.raw("scanline", b);
        return;
    }
    // One second of samples, 20 per millisecond at most: a real display walks
    // through every line in 16.7 ms.
    std::vector<UINT> seen;
    unsigned samples = 0, failures = 0, in_blank = 0, max_line = 0;
    LONG first_failure = 0;
    double start = now_ms(), last = -1;
    while (now_ms() - start < 1000.0) {
        double t = now_ms();
        if (t - last < 0.05)
            continue;
        last = t;
        KmtGetScanLine sl = {};
        sl.hAdapter = oa.hAdapter;
        sl.VidPnSourceId = oa.VidPnSourceId;
        st = scan(&sl);
        samples++;
        if (st < 0) {
            if (!failures)
                first_failure = st;
            failures++;
            continue;
        }
        if (sl.InVerticalBlank)
            in_blank++;
        if (sl.ScanLine > max_line)
            max_line = sl.ScanLine;
        bool found = false;
        for (UINT v : seen)
            if (v == sl.ScanLine) {
                found = true;
                break;
            }
        if (!found && seen.size() < 4096)
            seen.push_back(sl.ScanLine);
    }
    KmtCloseAdapter ca = {oa.hAdapter};
    close(&ca);
    char b[256];
    snprintf(b, sizeof b,
             "{\"available\": true, \"samples\": %u, \"failures\": %u, \"first_failure\": \"0x%08lx\", "
             "\"distinct_lines\": %u, \"max_line\": %u, \"in_vertical_blank\": %u}",
             samples, failures, (unsigned long)first_failure, (unsigned)seen.size(), max_line, in_blank);
    r.raw("scanline", b);
}

int main(int argc, char **argv)
{
    Report r("pace");
    const char *out = arg_str(argc, argv, "--out", "pace_probe.json");
    int seconds = arg_int(argc, argv, "--seconds", 10);
    const int W = 640, H = 360;

    DEVMODEW dm = {};
    dm.dmSize = sizeof dm;
    if (EnumDisplaySettingsW(NULL, ENUM_CURRENT_SETTINGS, &dm)) {
        r.integer("mode_width", dm.dmPelsWidth);
        r.integer("mode_height", dm.dmPelsHeight);
        r.integer("mode_refresh_hz", dm.dmDisplayFrequency);
    }
    DWM_TIMING_INFO ti = {};
    ti.cbSize = sizeof ti;
    if (SUCCEEDED(DwmGetCompositionTimingInfo(NULL, &ti)) && ti.rateRefresh.uiDenominator) {
        LARGE_INTEGER f;
        QueryPerformanceFrequency(&f);
        r.num("dwm_refresh_hz", (double)ti.rateRefresh.uiNumerator / ti.rateRefresh.uiDenominator);
        r.num("dwm_refresh_period_ms", (double)ti.qpcRefreshPeriod * 1000.0 / (double)f.QuadPart);
    }

    Gpu g;
    if (!gpu_open(r, g, false, "adapter"))
        return r.finish(out);
    HWND hwnd = make_window(L"vmlab-win3d adapter: pace", 32, 32, W, H, NULL);
    IDXGISwapChain1 *swap = hwnd ? make_swap_chain(r, g, hwnd, W, H, true) : NULL;
    if (!swap) {
        if (!hwnd)
            r.err("CreateWindowEx failed: %lu", GetLastError());
        return r.finish(out);
    }

    // vblank: the waits themselves, nothing presented.
    IDXGIOutput *output = NULL;
    HRESULT hr = swap->GetContainingOutput(&output);
    if (FAILED(hr))
        hr = g.adapter->EnumOutputs(0, &output);
    if (!r.failed(hr, "GetContainingOutput")) {
        std::vector<double> iv;
        double start = now_ms(), prev = 0;
        unsigned failures = 0;
        while (now_ms() - start < seconds * 1000.0) {
            pump();
            hr = output->WaitForVBlank();
            double t = now_ms();
            if (FAILED(hr)) {
                if (!failures++)
                    r.err("WaitForVBlank failed: 0x%08lx", (unsigned long)hr);
                Sleep(5);
                continue;
            }
            if (prev)
                iv.push_back(t - prev);
            prev = t;
        }
        report_stats(r, "vblank_interval_ms", stats_of(iv));
        output->Release();
    }

    // present: Present(1) on the flip-model swap chain, a new colour a frame.
    {
        std::vector<double> iv;
        DXGI_FRAME_STATISTICS fs0 = {}, fs1 = {};
        bool have0 = false, have1 = false;
        double start = now_ms(), prev = 0;
        unsigned frame = 0;
        while (now_ms() - start < seconds * 1000.0) {
            pump();
            ID3D11Texture2D *bb = NULL;
            ID3D11RenderTargetView *rtv = NULL;
            if (r.failed(swap->GetBuffer(0, __uuidof(ID3D11Texture2D), (void **)&bb), "GetBuffer") ||
                r.failed(g.dev->CreateRenderTargetView(bb, NULL, &rtv), "CreateRenderTargetView"))
                break;
            float c = (frame & 1) ? 0.25f : 0.75f;
            const float colour[4] = {c, 0.5f, 1.0f - c, 1.0f};
            g.ctx->ClearRenderTargetView(rtv, colour);
            rtv->Release();
            bb->Release();
            hr = swap->Present(1, 0);
            double t = now_ms();
            if (r.failed(hr, "Present"))
                break;
            if (prev)
                iv.push_back(t - prev);
            prev = t;
            frame++;
            DXGI_FRAME_STATISTICS fs = {};
            if (SUCCEEDED(swap->GetFrameStatistics(&fs)) && fs.SyncQPCTime.QuadPart) {
                if (!have0) {
                    fs0 = fs;
                    have0 = true;
                } else {
                    fs1 = fs;
                    have1 = true;
                }
            }
        }
        Stats s = stats_of(iv);
        report_stats(r, "present_interval_ms", s);
        r.integer("frames_presented", frame);
        if (s.n && s.mean > 0)
            r.num("present_fps", 1000.0 / s.mean);
        if (have0 && have1) {
            LARGE_INTEGER f;
            QueryPerformanceFrequency(&f);
            double ms = (double)(fs1.SyncQPCTime.QuadPart - fs0.SyncQPCTime.QuadPart) * 1000.0 / (double)f.QuadPart;
            unsigned refreshes = fs1.SyncRefreshCount - fs0.SyncRefreshCount;
            unsigned presents = fs1.PresentCount - fs0.PresentCount;
            char b[200];
            snprintf(b, sizeof b,
                     "{\"available\": true, \"sync_refreshes\": %u, \"presents\": %u, \"sync_ms\": %.3f, "
                     "\"refresh_hz\": %.4f}",
                     refreshes, presents, ms, ms > 0 ? refreshes * 1000.0 / ms : 0.0);
            r.raw("frame_statistics", b);
        } else {
            r.raw("frame_statistics", "{\"available\": false}");
        }
    }

    scan_line(r);
    swap->Release();
    DestroyWindow(hwnd);
    gpu_close(g);
    return r.finish(out);
}
