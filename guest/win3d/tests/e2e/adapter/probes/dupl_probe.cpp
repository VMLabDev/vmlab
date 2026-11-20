// P7, Desktop Duplication works.
//
// Duplicates the first output of the default adapter and takes --frames
// frames (8) while a window of its own (400x300 at 100,100) changes colour,
// because Desktop Duplication delivers a frame only when the desktop
// changed. Each frame is copied to a staging texture and read: how much of
// it is not black, and whether the pixel in the middle of the window has the
// colour the window had. This is the eight-frame test of card #139.
#include "adapter.h"
#include <dwmapi.h>
#include <dxgi1_2.h>

static const COLORREF colours[8] = {RGB(255, 0, 0),   RGB(0, 255, 0),   RGB(0, 0, 255),     RGB(255, 255, 0),
                                    RGB(255, 0, 255), RGB(0, 255, 255), RGB(255, 255, 255), RGB(255, 128, 0)};

static void paint(HWND hwnd, COLORREF c)
{
    HDC dc = GetDC(hwnd);
    RECT rc;
    GetClientRect(hwnd, &rc);
    HBRUSH b = CreateSolidBrush(c);
    FillRect(dc, &rc, b);
    DeleteObject(b);
    ReleaseDC(hwnd, dc);
    GdiFlush();
    DwmFlush();
}

int main(int argc, char **argv)
{
    Report r("duplication");
    const char *out = arg_str(argc, argv, "--out", "dupl_probe.json");
    int want = arg_int(argc, argv, "--frames", 8);
    const int X = 100, Y = 100, W = 400, H = 300;

    Gpu g;
    if (!gpu_open(r, g, false, "adapter"))
        return r.finish(out);
    HWND hwnd = make_window(L"vmlab-win3d adapter: duplication", X, Y, W, H, NULL);
    if (!hwnd) {
        r.err("CreateWindowEx failed: %lu", GetLastError());
        return r.finish(out);
    }
    paint(hwnd, RGB(32, 32, 32));
    pump();

    IDXGIOutput *output = NULL;
    IDXGIOutput1 *output1 = NULL;
    IDXGIOutputDuplication *dupl = NULL;
    if (r.failed(g.adapter->EnumOutputs(0, &output), "EnumOutputs(0)") ||
        r.failed(output->QueryInterface(__uuidof(IDXGIOutput1), (void **)&output1), "QueryInterface(IDXGIOutput1)"))
        return r.finish(out);
    HRESULT hr = output1->DuplicateOutput(g.dev, &dupl);
    char hx[32];
    snprintf(hx, sizeof hx, "0x%08lx", (unsigned long)hr);
    r.str("duplicate_output_hr", hx);
    if (FAILED(hr)) {
        // Not an error of the probe: this is the answer for an adapter
        // without Desktop Duplication.
        r.integer("frames_acquired", 0);
        r.raw("frames", "[]");
        return r.finish(out);
    }
    DXGI_OUTDUPL_DESC dd = {};
    dupl->GetDesc(&dd);
    r.integer("width", dd.ModeDesc.Width);
    r.integer("height", dd.ModeDesc.Height);
    r.boolean("desktop_image_in_system_memory", dd.DesktopImageInSystemMemory);

    D3D11_TEXTURE2D_DESC td = {};
    td.Width = dd.ModeDesc.Width;
    td.Height = dd.ModeDesc.Height;
    td.MipLevels = 1;
    td.ArraySize = 1;
    td.Format = dd.ModeDesc.Format;
    td.SampleDesc.Count = 1;
    td.Usage = D3D11_USAGE_STAGING;
    td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    ID3D11Texture2D *staging = NULL;
    if (r.failed(g.dev->CreateTexture2D(&td, NULL, &staging), "CreateTexture2D(staging)"))
        return r.finish(out);

    std::string frames = "[";
    int acquired = 0;
    for (int i = 0; i < want; i++) {
        COLORREF c = colours[i % 8];
        char line[400];
        HRESULT last = S_OK;
        bool got = false;
        // A frame that only moved the pointer has no image; ask again. A
        // timeout means nothing changed; paint again and ask again.
        for (int attempt = 0; attempt < 6 && !got; attempt++) {
            paint(hwnd, c);
            pump();
            DXGI_OUTDUPL_FRAME_INFO info = {};
            IDXGIResource *res = NULL;
            last = dupl->AcquireNextFrame(1000, &info, &res);
            if (last == DXGI_ERROR_WAIT_TIMEOUT)
                continue;
            if (FAILED(last))
                break;
            if (info.LastPresentTime.QuadPart == 0) {
                res->Release();
                dupl->ReleaseFrame();
                continue;
            }
            ID3D11Texture2D *tex = NULL;
            hr = res->QueryInterface(__uuidof(ID3D11Texture2D), (void **)&tex);
            res->Release();
            if (SUCCEEDED(hr)) {
                g.ctx->CopyResource(staging, tex);
                tex->Release();
            }
            dupl->ReleaseFrame();
            D3D11_MAPPED_SUBRESOURCE map = {};
            if (SUCCEEDED(hr))
                hr = g.ctx->Map(staging, 0, D3D11_MAP_READ, 0, &map);
            if (FAILED(hr)) {
                last = hr;
                break;
            }
            // BGRA rows. Count the pixels that are not black, and read the
            // middle of our window.
            unsigned long long lit = 0;
            for (UINT y = 0; y < td.Height; y++) {
                const unsigned char *row = (const unsigned char *)map.pData + (size_t)y * map.RowPitch;
                for (UINT x = 0; x < td.Width; x++)
                    if (row[x * 4] | row[x * 4 + 1] | row[x * 4 + 2])
                        lit++;
            }
            const unsigned char *p = (const unsigned char *)map.pData + (size_t)(Y + H / 2) * map.RowPitch + (X + W / 2) * 4;
            int pb = p[0], pg = p[1], pr = p[2];
            g.ctx->Unmap(staging, 0);
            bool match = abs(pr - GetRValue(c)) <= 8 && abs(pg - GetGValue(c)) <= 8 && abs(pb - GetBValue(c)) <= 8;
            if (!match && attempt < 5)
                continue; // a frame from before the paint; take the next one
            snprintf(line, sizeof line,
                     "%s{\"frame\": %d, \"hr\": \"0x00000000\", \"lit_pixels\": %llu, \"pixels\": %llu, "
                     "\"expected\": [%d, %d, %d], \"window_pixel\": [%d, %d, %d], \"match\": %s}",
                     i ? ", " : "", i, lit, (unsigned long long)td.Width * td.Height, GetRValue(c), GetGValue(c),
                     GetBValue(c), pr, pg, pb, match ? "true" : "false");
            got = true;
            acquired++;
        }
        if (!got)
            snprintf(line, sizeof line, "%s{\"frame\": %d, \"hr\": \"0x%08lx\"}", i ? ", " : "", i,
                     (unsigned long)last);
        frames += line;
    }
    r.integer("frames_acquired", acquired);
    r.raw("frames", frames + "]");
    staging->Release();
    dupl->Release();
    output1->Release();
    output->Release();
    DestroyWindow(hwnd);
    gpu_close(g);
    return r.finish(out);
}
