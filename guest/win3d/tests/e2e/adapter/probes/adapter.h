// Shared by the adapter suite's probes (tests/e2e/adapter.sh): the JSON
// result, the command line, a window, a Direct3D 11 device and HLSL.
//
// Every probe takes `--out FILE` and writes one JSON object there:
//   {"probe": NAME, "result": "ok" | "error", "errors": [...], ...measurements}
// A probe reports what it measured. Whether a measurement passes is decided
// on the host, in tests/e2e/adapter/judge.py, so the thresholds are in one
// place. "error" means the probe could not measure (an API call failed).
#pragma once
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <string>
#include <vector>

struct Report {
    const char *probe;
    std::string body;
    std::vector<std::string> errors;

    explicit Report(const char *name) : probe(name) {}

    static std::string quote(const std::string &s)
    {
        std::string o = "\"";
        for (unsigned char c : s) {
            char b[8];
            if (c == '"' || c == '\\') {
                o += '\\';
                o += (char)c;
            } else if (c < 0x20) {
                snprintf(b, sizeof b, "\\u%04x", c);
                o += b;
            } else {
                o += (char)c;
            }
        }
        return o + "\"";
    }
    // `value` is JSON text already.
    void raw(const char *key, const std::string &value)
    {
        body += ",\n  " + quote(key) + ": " + value;
    }
    void num(const char *key, double v)
    {
        char b[64];
        snprintf(b, sizeof b, "%.6g", v);
        raw(key, b);
    }
    void integer(const char *key, long long v)
    {
        char b[32];
        snprintf(b, sizeof b, "%lld", v);
        raw(key, b);
    }
    void boolean(const char *key, bool v) { raw(key, v ? "true" : "false"); }
    void str(const char *key, const std::string &v) { raw(key, quote(v)); }
    void err(const char *fmt, ...)
    {
        char b[512];
        va_list ap;
        va_start(ap, fmt);
        vsnprintf(b, sizeof b, fmt, ap);
        va_end(ap);
        errors.push_back(b);
    }
    // True, and an error recorded, when hr is a failure.
    bool failed(HRESULT hr, const char *what)
    {
        if (SUCCEEDED(hr))
            return false;
        err("%s failed: 0x%08lx", what, (unsigned long)hr);
        return true;
    }
    // Writes the result and returns the exit code: 0 measured, 1 error.
    int finish(const char *path)
    {
        std::string e = "[";
        for (size_t i = 0; i < errors.size(); i++)
            e += (i ? ", " : "") + quote(errors[i]);
        e += "]";
        FILE *f = fopen(path, "wb");
        if (!f)
            return 1;
        fprintf(f, "{\n  \"probe\": %s,\n  \"result\": \"%s\",\n  \"errors\": %s%s\n}\n", quote(probe).c_str(),
                errors.empty() ? "ok" : "error", e.c_str(), body.c_str());
        fclose(f);
        return errors.empty() ? 0 : 1;
    }
};

static inline const char *arg_str(int argc, char **argv, const char *name, const char *def)
{
    for (int i = 1; i + 1 < argc; i++)
        if (!strcmp(argv[i], name))
            return argv[i + 1];
    return def;
}

static inline int arg_int(int argc, char **argv, const char *name, int def)
{
    const char *s = arg_str(argc, argv, name, NULL);
    return s ? atoi(s) : def;
}

static inline double now_ms(void)
{
    static LARGE_INTEGER freq;
    LARGE_INTEGER t;
    if (!freq.QuadPart)
        QueryPerformanceFrequency(&freq);
    QueryPerformanceCounter(&t);
    return (double)t.QuadPart * 1000.0 / (double)freq.QuadPart;
}

static inline std::string narrow(const wchar_t *w)
{
    char b[512];
    int n = WideCharToMultiByte(CP_UTF8, 0, w, -1, b, sizeof b, NULL, NULL);
    return n > 0 ? std::string(b) : std::string();
}

static inline LRESULT CALLBACK adapter_wndproc(HWND h, UINT m, WPARAM w, LPARAM l)
{
    if (m == WM_SETCURSOR && LOWORD(l) == HTCLIENT) {
        SetCursor(LoadCursorW(NULL, MAKEINTRESOURCEW(32512))); // IDC_ARROW
        return TRUE;
    }
    return DefWindowProcW(h, m, w, l);
}

// A borderless, topmost window at a fixed place, so that the host knows
// where its pixels are in a console capture. `brush` may be NULL (the window
// is then drawn by Direct3D only).
static inline HWND make_window(const wchar_t *title, int x, int y, int w, int h, HBRUSH brush)
{
    static bool registered;
    if (!registered) {
        WNDCLASSW wc = {};
        wc.lpfnWndProc = adapter_wndproc;
        wc.hInstance = GetModuleHandleW(NULL);
        wc.lpszClassName = L"vmlab-win3d-adapter";
        wc.hbrBackground = brush;
        RegisterClassW(&wc);
        registered = true;
    }
    HWND hwnd = CreateWindowExW(WS_EX_TOPMOST, L"vmlab-win3d-adapter", title, WS_POPUP, x, y, w, h, NULL, NULL,
                                GetModuleHandleW(NULL), NULL);
    if (hwnd) {
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        UpdateWindow(hwnd);
    }
    return hwnd;
}

static inline void pump(void)
{
    MSG msg;
    while (PeekMessageW(&msg, NULL, 0, 0, PM_REMOVE)) {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

struct Gpu {
    ID3D11Device *dev = NULL;
    ID3D11DeviceContext *ctx = NULL;
    IDXGIAdapter *adapter = NULL;
    IDXGIFactory2 *factory = NULL;
    D3D_FEATURE_LEVEL level = (D3D_FEATURE_LEVEL)0;
    std::string name;
};

// A Direct3D 11 device on the default hardware adapter (or WARP), with BGRA
// support for Direct2D. Records the adapter in the report under `key`.
static inline bool gpu_open(Report &r, Gpu &g, bool warp, const char *key)
{
    static const D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1,
                                               D3D_FEATURE_LEVEL_10_0};
    HRESULT hr = D3D11CreateDevice(NULL, warp ? D3D_DRIVER_TYPE_WARP : D3D_DRIVER_TYPE_HARDWARE, NULL,
                                   D3D11_CREATE_DEVICE_BGRA_SUPPORT, levels, 4, D3D11_SDK_VERSION, &g.dev, &g.level,
                                   &g.ctx);
    if (hr == E_INVALIDARG) // a runtime that does not know 11_1
        hr = D3D11CreateDevice(NULL, warp ? D3D_DRIVER_TYPE_WARP : D3D_DRIVER_TYPE_HARDWARE, NULL,
                               D3D11_CREATE_DEVICE_BGRA_SUPPORT, levels + 1, 3, D3D11_SDK_VERSION, &g.dev, &g.level,
                               &g.ctx);
    if (r.failed(hr, warp ? "D3D11CreateDevice(WARP)" : "D3D11CreateDevice"))
        return false;
    IDXGIDevice *dxgi = NULL;
    if (r.failed(g.dev->QueryInterface(__uuidof(IDXGIDevice), (void **)&dxgi), "QueryInterface(IDXGIDevice)"))
        return false;
    hr = dxgi->GetAdapter(&g.adapter);
    dxgi->Release();
    if (r.failed(hr, "IDXGIDevice::GetAdapter"))
        return false;
    if (r.failed(g.adapter->GetParent(__uuidof(IDXGIFactory2), (void **)&g.factory), "GetParent(IDXGIFactory2)"))
        return false;
    DXGI_ADAPTER_DESC d = {};
    g.adapter->GetDesc(&d);
    g.name = narrow(d.Description);
    if (key) {
        char b[160];
        snprintf(b, sizeof b, "{\"name\": %s, \"vendor_id\": \"0x%04x\", \"feature_level\": \"0x%04x\"}",
                 Report::quote(g.name).c_str(), d.VendorId, (unsigned)g.level);
        r.raw(key, b);
    }
    return true;
}

static inline void gpu_close(Gpu &g)
{
    if (g.ctx) {
        g.ctx->ClearState();
        g.ctx->Flush();
        g.ctx->Release();
    }
    if (g.factory)
        g.factory->Release();
    if (g.adapter)
        g.adapter->Release();
    if (g.dev)
        g.dev->Release();
    g = Gpu();
}

// flip: DXGI_SWAP_EFFECT_FLIP_DISCARD, a flip the kernel driver gets as
// SetVidPnSourceAddress or as a DWM composition. Otherwise the blt model
// (DXGI_SWAP_EFFECT_DISCARD): a present of the Blt kind.
static inline IDXGISwapChain1 *make_swap_chain(Report &r, Gpu &g, HWND hwnd, int w, int h, bool flip)
{
    DXGI_SWAP_CHAIN_DESC1 sd = {};
    sd.Width = (UINT)w;
    sd.Height = (UINT)h;
    sd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    sd.SampleDesc.Count = 1;
    sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    sd.BufferCount = flip ? 2 : 1;
    sd.SwapEffect = flip ? DXGI_SWAP_EFFECT_FLIP_DISCARD : DXGI_SWAP_EFFECT_DISCARD;
    sd.AlphaMode = DXGI_ALPHA_MODE_IGNORE;
    IDXGISwapChain1 *swap = NULL;
    if (r.failed(g.factory->CreateSwapChainForHwnd(g.dev, hwnd, &sd, NULL, NULL, &swap), "CreateSwapChainForHwnd"))
        return NULL;
    g.factory->MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
    return swap;
}

// Compiles HLSL with System32's d3dcompiler_47.dll. The blob is leaked: a
// probe compiles two shaders and exits.
static inline bool compile_hlsl(Report &r, const char *src, const char *entry, const char *target, const void **code,
                                size_t *size)
{
    typedef HRESULT(WINAPI * compile_fn)(LPCVOID, SIZE_T, LPCSTR, const void *, void *, LPCSTR, LPCSTR, UINT, UINT,
                                         ID3D10Blob **, ID3D10Blob **);
    static compile_fn compile;
    if (!compile) {
        HMODULE m = LoadLibraryExW(L"d3dcompiler_47.dll", NULL, LOAD_LIBRARY_SEARCH_SYSTEM32);
        if (m)
            compile = (compile_fn)(void *)GetProcAddress(m, "D3DCompile");
        if (!compile) {
            r.err("cannot load D3DCompile from System32\\d3dcompiler_47.dll: %lu", GetLastError());
            return false;
        }
    }
    ID3D10Blob *blob = NULL, *errors = NULL;
    HRESULT hr = compile(src, strlen(src), "adapter", NULL, NULL, entry, target, 1u << 15, 0, &blob, &errors);
    if (FAILED(hr)) {
        r.err("D3DCompile %s %s failed: 0x%08lx %s", entry, target, (unsigned long)hr,
              errors ? (const char *)errors->GetBufferPointer() : "");
        return false;
    }
    *code = blob->GetBufferPointer();
    *size = blob->GetBufferSize();
    return true;
}

struct Stats {
    double mean = 0, sd = 0, min = 0, max = 0, p01 = 0, p50 = 0, p99 = 0;
    size_t n = 0;
};

static inline int cmp_double(const void *a, const void *b)
{
    double x = *(const double *)a, y = *(const double *)b;
    return x < y ? -1 : x > y;
}

static inline Stats stats_of(std::vector<double> v)
{
    Stats s;
    s.n = v.size();
    if (!s.n)
        return s;
    qsort(v.data(), v.size(), sizeof(double), cmp_double);
    double sum = 0, sq = 0;
    for (double x : v)
        sum += x;
    s.mean = sum / (double)s.n;
    for (double x : v)
        sq += (x - s.mean) * (x - s.mean);
    s.sd = s.n > 1 ? __builtin_sqrt(sq / (double)(s.n - 1)) : 0;
    s.min = v.front();
    s.max = v.back();
    s.p01 = v[(size_t)((double)(s.n - 1) * 0.01)];
    s.p50 = v[(s.n - 1) / 2];
    s.p99 = v[(size_t)((double)(s.n - 1) * 0.99)];
    return s;
}

static inline void report_stats(Report &r, const char *key, const Stats &s)
{
    char b[256];
    snprintf(b, sizeof b,
             "{\"n\": %u, \"mean\": %.4f, \"sd\": %.4f, \"min\": %.4f, \"p01\": %.4f, \"p50\": %.4f, \"p99\": %.4f, "
             "\"max\": %.4f}",
             (unsigned)s.n, s.mean, s.sd, s.min, s.p01, s.p50, s.p99, s.max);
    r.raw(key, b);
}
