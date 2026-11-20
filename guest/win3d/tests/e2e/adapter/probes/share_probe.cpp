// P7, the surface Desktop Duplication uses stays an ordinary shared texture.
//
// The driver cannot tell a duplication surface from another shared BGRA
// render target of the size of the desktop, so it gives each of them linear,
// mappable storage (patches/mesa/0095). This probe shares such a texture
// between two Direct3D 11 devices, which is the ordinary use the pattern also
// catches, and a 256x256 one, which the pattern leaves alone. Each is shared
// twice: through a legacy handle (D3D11_RESOURCE_MISC_SHARED) and with a
// keyed mutex. In each case device A fills the texture and device B reads it,
// then B fills it and A reads it. This is the shared-texture test of card
// #139.
#include "adapter.h"

struct Rgb {
    int r, g, b;
};

static void fill(ID3D11Device *dev, ID3D11DeviceContext *ctx, ID3D11Texture2D *tex, Rgb c, HRESULT *hr)
{
    ID3D11RenderTargetView *rtv = NULL;
    *hr = dev->CreateRenderTargetView(tex, NULL, &rtv);
    if (FAILED(*hr))
        return;
    const float rgba[4] = {(float)c.r / 255.0f, (float)c.g / 255.0f, (float)c.b / 255.0f, 1.0f};
    ctx->ClearRenderTargetView(rtv, rgba);
    rtv->Release();
    ctx->Flush();
}

// Copies the texture to a staging texture of the same device and counts the
// pixels within 2 of `want`.
static HRESULT count(ID3D11Device *dev, ID3D11DeviceContext *ctx, ID3D11Texture2D *tex, Rgb want,
                     unsigned long long *same, Rgb *middle)
{
    D3D11_TEXTURE2D_DESC td = {};
    tex->GetDesc(&td);
    td.Usage = D3D11_USAGE_STAGING;
    td.BindFlags = 0;
    td.MiscFlags = 0;
    td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    ID3D11Texture2D *staging = NULL;
    HRESULT hr = dev->CreateTexture2D(&td, NULL, &staging);
    if (FAILED(hr))
        return hr;
    ctx->CopyResource(staging, tex);
    D3D11_MAPPED_SUBRESOURCE map = {};
    hr = ctx->Map(staging, 0, D3D11_MAP_READ, 0, &map);
    if (SUCCEEDED(hr)) {
        *same = 0;
        for (UINT y = 0; y < td.Height; y++) {
            const unsigned char *row = (const unsigned char *)map.pData + (size_t)y * map.RowPitch;
            for (UINT x = 0; x < td.Width; x++) {
                const unsigned char *p = row + x * 4;
                if (abs(p[2] - want.r) <= 2 && abs(p[1] - want.g) <= 2 && abs(p[0] - want.b) <= 2)
                    (*same)++;
            }
        }
        const unsigned char *p =
            (const unsigned char *)map.pData + (size_t)(td.Height / 2) * map.RowPitch + (size_t)(td.Width / 2) * 4;
        middle->r = p[2];
        middle->g = p[1];
        middle->b = p[0];
        ctx->Unmap(staging, 0);
    }
    staging->Release();
    return hr;
}

// One direction: `writer` fills, `reader` reads. Without a keyed mutex the
// two devices have no order but the writer's Flush, so the reader asks again
// for up to two seconds.
static std::string pass(const char *key, ID3D11Device *wdev, ID3D11DeviceContext *wctx, ID3D11Texture2D *wtex,
                        IDXGIKeyedMutex *wmutex, ID3D11Device *rdev, ID3D11DeviceContext *rctx, ID3D11Texture2D *rtex,
                        IDXGIKeyedMutex *rmutex, Rgb c, unsigned long long pixels, bool *ok)
{
    HRESULT hr = S_OK, sync = S_OK;
    unsigned long long same = 0;
    Rgb middle = {-1, -1, -1};
    int tries = 0;
    if (wmutex)
        sync = wmutex->AcquireSync(0, 5000);
    if (sync == S_OK) {
        fill(wdev, wctx, wtex, c, &hr);
        if (wmutex)
            wmutex->ReleaseSync(0);
    }
    if (sync == S_OK && SUCCEEDED(hr)) {
        const double end = now_ms() + 2000.0;
        do {
            tries++;
            if (rmutex)
                sync = rmutex->AcquireSync(0, 5000);
            if (sync != S_OK)
                break;
            hr = count(rdev, rctx, rtex, c, &same, &middle);
            if (rmutex)
                rmutex->ReleaseSync(0);
            if (FAILED(hr) || same == pixels)
                break;
            Sleep(50);
        } while (now_ms() < end);
    }
    bool good = sync == S_OK && SUCCEEDED(hr) && same == pixels;
    if (!good)
        *ok = false;
    char b[400];
    snprintf(b, sizeof b,
             "\"%s\": {\"hr\": \"0x%08lx\", \"sync\": \"0x%08lx\", \"expected\": [%d, %d, %d], \"middle\": [%d, %d, %d], "
             "\"same_pixels\": %llu, \"tries\": %d, \"match\": %s}",
             key, (unsigned long)hr, (unsigned long)sync, c.r, c.g, c.b, middle.r, middle.g, middle.b, same, tries,
             good ? "true" : "false");
    return b;
}

static std::string share(Gpu &a, Gpu &b, const char *size, UINT w, UINT h, bool keyed, int *passed)
{
    char head[200];
    snprintf(head, sizeof head, "{\"size\": \"%s\", \"width\": %u, \"height\": %u, \"sharing\": \"%s\", ", size, w, h,
             keyed ? "keyed-mutex" : "legacy-handle");
    std::string out = head;
    const char *step = "CreateTexture2D";
    ID3D11Texture2D *ta = NULL, *tb = NULL;
    IDXGIResource *res = NULL;
    IDXGIKeyedMutex *ma = NULL, *mb = NULL;
    HANDLE handle = NULL;
    bool ok = true;

    D3D11_TEXTURE2D_DESC td = {};
    td.Width = w;
    td.Height = h;
    td.MipLevels = 1;
    td.ArraySize = 1;
    td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    td.SampleDesc.Count = 1;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
    td.MiscFlags = keyed ? D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX : D3D11_RESOURCE_MISC_SHARED;
    HRESULT hr = a.dev->CreateTexture2D(&td, NULL, &ta);
    if (SUCCEEDED(hr)) {
        step = "GetSharedHandle";
        hr = ta->QueryInterface(__uuidof(IDXGIResource), (void **)&res);
        if (SUCCEEDED(hr))
            hr = res->GetSharedHandle(&handle);
    }
    if (SUCCEEDED(hr)) {
        step = "OpenSharedResource";
        hr = b.dev->OpenSharedResource(handle, __uuidof(ID3D11Texture2D), (void **)&tb);
    }
    if (SUCCEEDED(hr) && keyed) {
        step = "QueryInterface(IDXGIKeyedMutex)";
        hr = ta->QueryInterface(__uuidof(IDXGIKeyedMutex), (void **)&ma);
        if (SUCCEEDED(hr))
            hr = tb->QueryInterface(__uuidof(IDXGIKeyedMutex), (void **)&mb);
    }
    char b2[120];
    if (FAILED(hr)) {
        snprintf(b2, sizeof b2, "\"failed\": \"%s\", \"hr\": \"0x%08lx\", \"match\": false}", step, (unsigned long)hr);
        out += b2;
    } else {
        const unsigned long long pixels = (unsigned long long)w * h;
        const Rgb first = {200, 40, 90}, second = {20, 160, 230};
        out += pass("a_to_b", a.dev, a.ctx, ta, ma, b.dev, b.ctx, tb, mb, first, pixels, &ok) + ", ";
        out += pass("b_to_a", b.dev, b.ctx, tb, mb, a.dev, a.ctx, ta, ma, second, pixels, &ok) + ", ";
        snprintf(b2, sizeof b2, "\"pixels\": %llu, \"match\": %s}", pixels, ok ? "true" : "false");
        out += b2;
        if (ok)
            (*passed)++;
    }
    if (ma)
        ma->Release();
    if (mb)
        mb->Release();
    if (res)
        res->Release();
    if (tb)
        tb->Release();
    if (ta)
        ta->Release();
    return out;
}

int main(int argc, char **argv)
{
    Report r("shared-texture");
    const char *out = arg_str(argc, argv, "--out", "share_probe.json");

    Gpu a, b;
    if (!gpu_open(r, a, false, "adapter") || !gpu_open(r, b, false, NULL))
        return r.finish(out);

    // The size the driver takes for a duplication surface: the display mode.
    DEVMODEW mode = {};
    mode.dmSize = sizeof mode;
    if (!EnumDisplaySettingsW(NULL, ENUM_CURRENT_SETTINGS, &mode)) {
        r.err("EnumDisplaySettings failed: %lu", GetLastError());
        return r.finish(out);
    }
    r.integer("desktop_width", mode.dmPelsWidth);
    r.integer("desktop_height", mode.dmPelsHeight);

    int passed = 0;
    std::string cases = "[";
    cases += share(a, b, "desktop", mode.dmPelsWidth, mode.dmPelsHeight, false, &passed) + ",\n    ";
    cases += share(a, b, "desktop", mode.dmPelsWidth, mode.dmPelsHeight, true, &passed) + ",\n    ";
    cases += share(a, b, "small", 256, 256, false, &passed) + ",\n    ";
    cases += share(a, b, "small", 256, 256, true, &passed) + "]";
    r.raw("cases", cases);
    r.integer("cases_passed", passed);

    gpu_close(b);
    gpu_close(a);
    return r.finish(out);
}
