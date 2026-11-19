/* Target-independent rasterization probe (patches/mesa/0094,
 * docs/d2d-coverage.md).
 *
 * It draws what Direct2D draws on feature level 11.1 to antialias a shape:
 * triangles with ForcedSampleCount 16 into a single-sampled R16_UINT
 * texture, a pixel shader that returns SV_Coverage, and the output merger's
 * logic op XOR. The scene is 96 discs, each a fan of 24 triangles round its
 * centre, and 32 stars, each a fan from one corner. Every pixel on an edge
 * that two triangles of a fan share gets a fragment from each, with disjoint
 * coverage. A driver that loses one of the two XORs (GitHub #236) leaves
 * coverage bits clear there, in other places at every run.
 *
 * The scene is drawn --runs times (3) on a new device of the default adapter
 * each time, and once on WARP, whose rasterizer is the reference. The 16
 * coverage bits of every pixel are compared:
 *
 *   mismatch      pixels of a hardware run whose bits are not WARP's
 *   lost_bits     coverage bits WARP set that a hardware run left clear
 *   extra_bits    coverage bits a hardware run set that WARP left clear
 *   runs_differ   pixels that are not the same in every hardware run
 *
 *   tir_probe.exe [--runs N] [--samples 16] [--out FILE.json]
 *
 * Exit 0 when all four are 0, 1 when not, 2 when the scene could not be
 * drawn (no feature level 11.1, for example). It cannot see which path the
 * driver took; tests/e2e/tir.sh reads that from the driver's log. Built by
 * probes/d3d11-tir/build.sh. */
#include <windows.h>
#include <d3d11_1.h>
#include <d3dcommon.h>
#include <math.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <string>
#include <vector>

#define RT 512

static std::string g_errors;

static void say(const char *fmt, ...)
{
    char buf[1024];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf, sizeof(buf), fmt, ap);
    va_end(ap);
    printf("%s\n", buf);
    fflush(stdout);
}

static bool failed(HRESULT hr, const char *what)
{
    if (SUCCEEDED(hr))
        return false;
    char b[200];
    snprintf(b, sizeof(b), "%s\"%s: 0x%08lx\"", g_errors.empty() ? "" : ", ", what, (unsigned long)hr);
    g_errors += b;
    say("%s failed: 0x%08lx", what, (unsigned long)hr);
    return true;
}

static const char hlsl[] =
    "float4 vs(float2 p : POSITION) : SV_Position {\n"
    "  return float4(p.x / 256.0 - 1.0, 1.0 - p.y / 256.0, 0.5, 1.0); }\n"
    "uint ps(float4 p : SV_Position, uint cov : SV_Coverage) : SV_Target { return cov; }\n";

typedef HRESULT(WINAPI *d3dcompile_fn)(const void *, SIZE_T, const char *, const D3D_SHADER_MACRO *,
                                       ID3DInclude *, const char *, const char *, UINT, UINT,
                                       ID3DBlob **, ID3DBlob **);

static ID3DBlob *compile(const char *entry, const char *target)
{
    static d3dcompile_fn fn;
    if (!fn) {
        HMODULE m = LoadLibraryExW(L"d3dcompiler_47.dll", NULL, LOAD_LIBRARY_SEARCH_SYSTEM32);
        fn = m ? (d3dcompile_fn)(void *)GetProcAddress(m, "D3DCompile") : NULL;
        if (!fn) {
            failed(E_FAIL, "load D3DCompile from d3dcompiler_47.dll");
            return NULL;
        }
    }
    ID3DBlob *code = NULL, *err = NULL;
    HRESULT hr = fn(hlsl, sizeof(hlsl) - 1, "tir_probe", NULL, NULL, entry, target, 0, 0, &code, &err);
    if (FAILED(hr)) {
        say("%s", err ? (const char *)err->GetBufferPointer() : "");
        failed(hr, "D3DCompile");
    }
    if (err)
        err->Release();
    return code;
}

struct Vertex {
    float x, y;
};

static unsigned g_rng;
static float rnd(float lo, float hi)
{
    g_rng = g_rng * 1664525u + 1013904223u;
    return lo + (hi - lo) * (float)((g_rng >> 8) & 0xffff) / 65535.0f;
}

/* Triangle lists, one draw per shape as Direct2D makes one per geometry.
 * Coordinates are in pixels, on no particular grid. */
static void scene(std::vector<Vertex> &v, std::vector<unsigned> &first)
{
    g_rng = 145;
    for (int i = 0; i < 96; i++) {
        float cx = rnd(40, RT - 40), cy = rnd(40, RT - 40), rx = rnd(6, 38), ry = rnd(6, 38);
        first.push_back((unsigned)v.size());
        for (int k = 0; k < 24; k++) {
            float a = 6.2831853f * (float)k / 24.0f, b = 6.2831853f * (float)(k + 1) / 24.0f;
            v.push_back({cx, cy});
            v.push_back({cx + rx * cosf(a), cy + ry * sinf(a)});
            v.push_back({cx + rx * cosf(b), cy + ry * sinf(b)});
        }
    }
    for (int i = 0; i < 32; i++) {
        float cx = rnd(60, RT - 60), cy = rnd(60, RT - 60), rad = rnd(20, 55);
        int points = 5 + (i % 7);
        Vertex p[32];
        for (int k = 0; k < points * 2; k++) {
            float a = 3.14159265f * (float)k / (float)points, rr = (k & 1) ? rad * 0.45f : rad;
            p[k] = {cx + rr * cosf(a), cy + rr * sinf(a)};
        }
        first.push_back((unsigned)v.size());
        for (int k = 1; k + 1 < points * 2; k++) {
            v.push_back(p[0]);
            v.push_back(p[k]);
            v.push_back(p[k + 1]);
        }
    }
    first.push_back((unsigned)v.size());
}

struct Who {
    char name[128];
    unsigned vendor, level;
};

/* One picture of the scene on a new device: RT*RT coverage masks. */
static bool render(bool warp, unsigned samples, std::vector<unsigned short> &mask, Who *who)
{
    static const D3D_FEATURE_LEVEL want = D3D_FEATURE_LEVEL_11_1;
    ID3D11Device *dev = NULL;
    ID3D11Device1 *dev1 = NULL;
    ID3D11DeviceContext *ctx = NULL;
    ID3D11DeviceContext1 *ctx1 = NULL;
    IDXGIDevice *dxgi = NULL;
    IDXGIAdapter *adapter = NULL;
    ID3D11Texture2D *tex = NULL, *staging = NULL;
    ID3D11RenderTargetView *rtv = NULL;
    ID3D11RasterizerState1 *rs = NULL;
    ID3D11BlendState1 *bs = NULL;
    ID3D11DepthStencilState *dss = NULL;
    ID3D11VertexShader *vs = NULL;
    ID3D11PixelShader *ps = NULL;
    ID3D11InputLayout *layout = NULL;
    ID3D11Buffer *vb = NULL;
    ID3DBlob *vsb = NULL, *psb = NULL;
    D3D_FEATURE_LEVEL got = (D3D_FEATURE_LEVEL)0;
    bool ok = false;
    std::vector<Vertex> verts;
    std::vector<unsigned> first;
    scene(verts, first);
    do {
        if (failed(D3D11CreateDevice(NULL, warp ? D3D_DRIVER_TYPE_WARP : D3D_DRIVER_TYPE_HARDWARE, NULL, 0, &want, 1,
                                     D3D11_SDK_VERSION, &dev, &got, &ctx),
                   warp ? "D3D11CreateDevice(WARP, 11.1)" : "D3D11CreateDevice(11.1)") ||
            failed(dev->QueryInterface(__uuidof(ID3D11Device1), (void **)&dev1), "QueryInterface(ID3D11Device1)") ||
            failed(ctx->QueryInterface(__uuidof(ID3D11DeviceContext1), (void **)&ctx1),
                   "QueryInterface(ID3D11DeviceContext1)"))
            break;
        if (who && SUCCEEDED(dev->QueryInterface(__uuidof(IDXGIDevice), (void **)&dxgi)) &&
            SUCCEEDED(dxgi->GetAdapter(&adapter))) {
            DXGI_ADAPTER_DESC d = {};
            adapter->GetDesc(&d);
            WideCharToMultiByte(CP_UTF8, 0, d.Description, -1, who->name, sizeof(who->name), NULL, NULL);
            who->vendor = d.VendorId;
            who->level = (unsigned)got;
        }

        D3D11_TEXTURE2D_DESC td = {};
        td.Width = td.Height = RT;
        td.MipLevels = td.ArraySize = 1;
        td.Format = DXGI_FORMAT_R16_UINT;
        td.SampleDesc.Count = 1;
        td.Usage = D3D11_USAGE_DEFAULT;
        td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
        if (failed(dev->CreateTexture2D(&td, NULL, &tex), "CreateTexture2D(R16_UINT)") ||
            failed(dev->CreateRenderTargetView(tex, NULL, &rtv), "CreateRenderTargetView"))
            break;
        td.Usage = D3D11_USAGE_STAGING;
        td.BindFlags = 0;
        td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
        if (failed(dev->CreateTexture2D(&td, NULL, &staging), "CreateTexture2D(staging)"))
            break;

        D3D11_RASTERIZER_DESC1 rd = {};
        rd.FillMode = D3D11_FILL_SOLID;
        rd.CullMode = D3D11_CULL_NONE;
        rd.DepthClipEnable = TRUE;
        rd.ForcedSampleCount = samples;
        D3D11_BLEND_DESC1 bd = {};
        bd.RenderTarget[0].LogicOpEnable = TRUE;
        bd.RenderTarget[0].LogicOp = D3D11_LOGIC_OP_XOR;
        bd.RenderTarget[0].SrcBlend = bd.RenderTarget[0].SrcBlendAlpha = D3D11_BLEND_ONE;
        bd.RenderTarget[0].DestBlend = bd.RenderTarget[0].DestBlendAlpha = D3D11_BLEND_ZERO;
        bd.RenderTarget[0].BlendOp = bd.RenderTarget[0].BlendOpAlpha = D3D11_BLEND_OP_ADD;
        bd.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL;
        D3D11_DEPTH_STENCIL_DESC dd = {};
        dd.DepthWriteMask = D3D11_DEPTH_WRITE_MASK_ZERO;
        dd.DepthFunc = D3D11_COMPARISON_ALWAYS;
        if (failed(dev1->CreateRasterizerState1(&rd, &rs), "CreateRasterizerState1(ForcedSampleCount)") ||
            failed(dev1->CreateBlendState1(&bd, &bs), "CreateBlendState1(logic op XOR)") ||
            failed(dev->CreateDepthStencilState(&dd, &dss), "CreateDepthStencilState"))
            break;

        vsb = compile("vs", "vs_5_0");
        psb = compile("ps", "ps_5_0");
        if (!vsb || !psb)
            break;
        static const D3D11_INPUT_ELEMENT_DESC il[] = {
            {"POSITION", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 0, D3D11_INPUT_PER_VERTEX_DATA, 0}};
        D3D11_BUFFER_DESC vd = {};
        vd.ByteWidth = (UINT)(verts.size() * sizeof(Vertex));
        vd.Usage = D3D11_USAGE_IMMUTABLE;
        vd.BindFlags = D3D11_BIND_VERTEX_BUFFER;
        D3D11_SUBRESOURCE_DATA init = {};
        init.pSysMem = verts.data();
        if (failed(dev->CreateVertexShader(vsb->GetBufferPointer(), vsb->GetBufferSize(), NULL, &vs),
                   "CreateVertexShader") ||
            failed(dev->CreatePixelShader(psb->GetBufferPointer(), psb->GetBufferSize(), NULL, &ps),
                   "CreatePixelShader") ||
            failed(dev->CreateInputLayout(il, 1, vsb->GetBufferPointer(), vsb->GetBufferSize(), &layout),
                   "CreateInputLayout") ||
            failed(dev->CreateBuffer(&vd, &init, &vb), "CreateBuffer"))
            break;

        const FLOAT zero[4] = {0, 0, 0, 0};
        D3D11_VIEWPORT vp = {0, 0, RT, RT, 0, 1};
        UINT stride = sizeof(Vertex), offset = 0;
        ctx->ClearRenderTargetView(rtv, zero);
        ctx->OMSetRenderTargets(1, &rtv, NULL);
        ctx->OMSetBlendState(bs, NULL, 0xffffffff);
        ctx->OMSetDepthStencilState(dss, 0);
        ctx->RSSetState(rs);
        ctx->RSSetViewports(1, &vp);
        ctx->IASetInputLayout(layout);
        ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        ctx->IASetVertexBuffers(0, 1, &vb, &stride, &offset);
        ctx->VSSetShader(vs, NULL, 0);
        ctx->PSSetShader(ps, NULL, 0);
        for (size_t i = 0; i + 1 < first.size(); i++)
            ctx->Draw(first[i + 1] - first[i], first[i]);
        ctx->OMSetRenderTargets(0, NULL, NULL);
        ctx->CopyResource(staging, tex);
        D3D11_MAPPED_SUBRESOURCE map = {};
        if (failed(ctx->Map(staging, 0, D3D11_MAP_READ, 0, &map), "Map(staging)"))
            break;
        mask.resize((size_t)RT * RT);
        for (int y = 0; y < RT; y++)
            memcpy(&mask[(size_t)y * RT], (const char *)map.pData + (size_t)y * map.RowPitch, RT * sizeof(unsigned short));
        ctx->Unmap(staging, 0);
        if (failed(dev->GetDeviceRemovedReason(), "GetDeviceRemovedReason"))
            break;
        ok = true;
    } while (0);
#define DROP(p) \
    if (p)      \
        (p)->Release()
    DROP(vb);
    DROP(layout);
    DROP(ps);
    DROP(vs);
    DROP(psb);
    DROP(vsb);
    DROP(dss);
    DROP(bs);
    DROP(rs);
    DROP(rtv);
    DROP(staging);
    DROP(tex);
    DROP(adapter);
    DROP(dxgi);
    DROP(ctx1);
    DROP(dev1);
    DROP(ctx);
    DROP(dev);
    return ok;
}

static const char *arg(int argc, char **argv, const char *name, const char *dfault)
{
    for (int i = 1; i + 1 < argc; i++)
        if (!strcmp(argv[i], name))
            return argv[i + 1];
    return dfault;
}

int main(int argc, char **argv)
{
    const char *out = arg(argc, argv, "--out", NULL);
    int runs = atoi(arg(argc, argv, "--runs", "3"));
    unsigned samples = (unsigned)atoi(arg(argc, argv, "--samples", "16"));
    if (runs < 1)
        runs = 1;
    if (runs > 16)
        runs = 16;

    Who ref_who = {}, who = {};
    std::vector<unsigned short> ref;
    std::vector<std::vector<unsigned short>> hw((size_t)runs);
    bool ok = render(true, samples, ref, &ref_who);
    for (int i = 0; ok && i < runs; i++)
        ok = render(false, samples, hw[(size_t)i], i ? NULL : &who);

    unsigned long long covered = 0, mismatch = 0, lost = 0, extra = 0, runs_differ = 0;
    int fx = -1, fy = -1;
    for (int y = 0; ok && y < RT; y++)
        for (int x = 0; x < RT; x++) {
            size_t o = (size_t)y * RT + x;
            bool differ = false, wrong = false;
            if (ref[o])
                covered++;
            for (int i = 0; i < runs; i++) {
                unsigned short h = hw[(size_t)i][o];
                if (h != hw[0][o])
                    differ = true;
                if (h != ref[o])
                    wrong = true;
                lost += (unsigned)__builtin_popcount((unsigned)(ref[o] & ~h) & 0xffff);
                extra += (unsigned)__builtin_popcount((unsigned)(h & ~ref[o]) & 0xffff);
            }
            if (differ)
                runs_differ++;
            if (wrong) {
                if (!mismatch) {
                    fx = x;
                    fy = y;
                }
                mismatch++;
            }
        }
    bool clean = ok && !mismatch && !runs_differ;
    say("tir_probe: %s on %s (feature level 0x%04x), %d runs, %u samples", ok ? "drew" : "could not draw", who.name,
        who.level, runs, samples);
    if (ok)
        say("covered_pixels=%llu mismatch=%llu lost_bits=%llu extra_bits=%llu runs_differ=%llu first_mismatch=%d,%d",
            covered, mismatch, lost, extra, runs_differ, fx, fy);
    if (out) {
        FILE *f = fopen(out, "wb");
        if (f) {
            fprintf(f, "{\n  \"probe\": \"tir\",\n  \"result\": \"%s\",\n  \"errors\": [%s],\n", ok ? "ok" : "error",
                    g_errors.c_str());
            fprintf(f, "  \"reference\": {\"name\": \"%s\", \"vendor_id\": \"0x%04x\", \"feature_level\": \"0x%04x\"},\n",
                    ref_who.name, ref_who.vendor, ref_who.level);
            fprintf(f, "  \"adapter\": {\"name\": \"%s\", \"vendor_id\": \"0x%04x\", \"feature_level\": \"0x%04x\"},\n",
                    who.name, who.vendor, who.level);
            fprintf(f, "  \"runs\": %d,\n  \"samples\": %u,\n  \"width\": %d,\n  \"height\": %d,\n", runs, samples, RT, RT);
            fprintf(f, "  \"covered_pixels\": %llu,\n  \"mismatch\": %llu,\n  \"lost_bits\": %llu,\n", covered, mismatch,
                    lost);
            fprintf(f, "  \"extra_bits\": %llu,\n  \"runs_differ\": %llu,\n  \"first_mismatch\": [%d, %d]\n}\n", extra,
                    runs_differ, fx, fy);
            fclose(f);
        }
    }
    return !ok ? 2 : clean ? 0 : 1;
}
