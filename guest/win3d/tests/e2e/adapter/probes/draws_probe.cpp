// P10, a draw the driver once dropped leaves the device usable.
//
// Wine's d3d11 test makes three draws that the driver dropped, and it marked
// the device as removed at the next draw (card #328, docs/acceptance.md B11).
// The test releases those devices without another call, so its rows did not
// show it. This probe makes each draw on a device of its own and then uses
// the device:
//
//   bgrx-vertex          a quad whose colour element is B8G8R8X8_UNORM
//   null-index           DrawIndexed of a point list with no index buffer
//   null-index-indirect  DrawIndexedInstancedIndirect of it, no index buffer
//
// After each draw it reads the 4x4 target, draws an ordinary quad, reads
// that, and asks GetDeviceRemovedReason. Direct3D reads index 0 for every
// index when no index buffer is bound, so the point list draws its first
// vertex only.
#include "adapter.h"

static const char *HLSL = "struct V { float4 pos : SV_Position; float4 col : COLOR; };\n"
                          "V vs(float4 pos : POSITION, float4 col : COLOR) { V o; o.pos = pos; o.col = col; return o; }\n"
                          "float4 ps(V i) : SV_Target { return i.col; }\n";

struct Vertex {
    float x, y;
    unsigned char c[4];
};

struct Scene {
    Gpu g;
    ID3D11Texture2D *rt = NULL, *staging = NULL;
    ID3D11RenderTargetView *rtv = NULL;
    ID3D11VertexShader *vs = NULL;
    ID3D11PixelShader *ps = NULL;
    ID3D11InputLayout *rgba = NULL;
    ID3D11Buffer *quad = NULL;
    const void *vs_code = NULL;
    size_t vs_size = 0;
};

static const UINT SIDE = 4;
// The ordinary quad of the usability check, as R8G8B8A8_UNORM.
static const unsigned char LATER[4] = {40, 160, 220, 255};

static ID3D11Buffer *make_buffer(ID3D11Device *dev, UINT bind, UINT misc, const void *data, UINT size, HRESULT *hr)
{
    D3D11_BUFFER_DESC bd = {};
    bd.ByteWidth = size;
    bd.Usage = D3D11_USAGE_DEFAULT;
    bd.BindFlags = bind;
    bd.MiscFlags = misc;
    D3D11_SUBRESOURCE_DATA init = {};
    init.pSysMem = data;
    ID3D11Buffer *b = NULL;
    *hr = dev->CreateBuffer(&bd, &init, &b);
    return b;
}

static ID3D11InputLayout *make_layout(Scene &s, DXGI_FORMAT colour, HRESULT *hr)
{
    const D3D11_INPUT_ELEMENT_DESC desc[] = {
        {"POSITION", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 0, D3D11_INPUT_PER_VERTEX_DATA, 0},
        {"COLOR", 0, colour, 0, 8, D3D11_INPUT_PER_VERTEX_DATA, 0},
    };
    ID3D11InputLayout *l = NULL;
    *hr = s.g.dev->CreateInputLayout(desc, 2, s.vs_code, s.vs_size, &l);
    return l;
}

static void fill_quad(Vertex v[4], const unsigned char c[4])
{
    static const float xy[4][2] = {{-1, -1}, {-1, 1}, {1, -1}, {1, 1}};
    for (int i = 0; i < 4; i++) {
        v[i].x = xy[i][0];
        v[i].y = xy[i][1];
        memcpy(v[i].c, c, 4);
    }
}

static bool scene_open(Report &r, Scene &s, const char *adapter_key)
{
    if (!gpu_open(r, s.g, false, adapter_key))
        return false;
    ID3D11Device *dev = s.g.dev;
    D3D11_TEXTURE2D_DESC td = {};
    td.Width = td.Height = SIDE;
    td.MipLevels = td.ArraySize = 1;
    td.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    td.SampleDesc.Count = 1;
    td.BindFlags = D3D11_BIND_RENDER_TARGET;
    if (r.failed(dev->CreateTexture2D(&td, NULL, &s.rt), "CreateTexture2D(target)"))
        return false;
    td.Usage = D3D11_USAGE_STAGING;
    td.BindFlags = 0;
    td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    if (r.failed(dev->CreateTexture2D(&td, NULL, &s.staging), "CreateTexture2D(staging)"))
        return false;
    if (r.failed(dev->CreateRenderTargetView(s.rt, NULL, &s.rtv), "CreateRenderTargetView"))
        return false;
    const void *ps_code;
    size_t ps_size;
    if (!compile_hlsl(r, HLSL, "vs", "vs_4_0", &s.vs_code, &s.vs_size) ||
        !compile_hlsl(r, HLSL, "ps", "ps_4_0", &ps_code, &ps_size))
        return false;
    if (r.failed(dev->CreateVertexShader(s.vs_code, s.vs_size, NULL, &s.vs), "CreateVertexShader") ||
        r.failed(dev->CreatePixelShader(ps_code, ps_size, NULL, &s.ps), "CreatePixelShader"))
        return false;
    HRESULT hr;
    s.rgba = make_layout(s, DXGI_FORMAT_R8G8B8A8_UNORM, &hr);
    if (r.failed(hr, "CreateInputLayout(R8G8B8A8_UNORM)"))
        return false;
    Vertex v[4];
    fill_quad(v, LATER);
    s.quad = make_buffer(dev, D3D11_BIND_VERTEX_BUFFER, 0, v, sizeof v, &hr);
    if (r.failed(hr, "CreateBuffer(quad)"))
        return false;
    return true;
}

static void scene_close(Scene &s)
{
    IUnknown *all[] = {s.quad, s.rgba, s.ps, s.vs, s.rtv, s.staging, s.rt};
    for (IUnknown *u : all)
        if (u)
            u->Release();
    gpu_close(s.g);
    s = Scene();
}

// The state every draw of the probe starts from.
static void bind(Scene &s, ID3D11InputLayout *layout, ID3D11Buffer *vb, D3D11_PRIMITIVE_TOPOLOGY topology,
                 const float clear[4])
{
    ID3D11DeviceContext *ctx = s.g.ctx;
    const D3D11_VIEWPORT vp = {0, 0, (float)SIDE, (float)SIDE, 0, 1};
    const UINT stride = sizeof(Vertex), offset = 0;
    ctx->ClearRenderTargetView(s.rtv, clear);
    ctx->OMSetRenderTargets(1, &s.rtv, NULL);
    ctx->RSSetViewports(1, &vp);
    ctx->IASetInputLayout(layout);
    ctx->IASetPrimitiveTopology(topology);
    ctx->IASetVertexBuffers(0, 1, &vb, &stride, &offset);
    ctx->VSSetShader(s.vs, NULL, 0);
    ctx->PSSetShader(s.ps, NULL, 0);
}

// The target's 16 pixels, RGBA.
static HRESULT read_target(Scene &s, unsigned char px[SIDE * SIDE][4])
{
    s.g.ctx->CopyResource(s.staging, s.rt);
    D3D11_MAPPED_SUBRESOURCE map = {};
    HRESULT hr = s.g.ctx->Map(s.staging, 0, D3D11_MAP_READ, 0, &map);
    if (FAILED(hr))
        return hr;
    if (!map.pData) {
        s.g.ctx->Unmap(s.staging, 0);
        return E_POINTER;
    }
    for (UINT y = 0; y < SIDE; y++)
        memcpy(px[y * SIDE], (const unsigned char *)map.pData + (size_t)y * map.RowPitch, SIDE * 4);
    s.g.ctx->Unmap(s.staging, 0);
    return S_OK;
}

static bool close_to(const unsigned char *p, const unsigned char *want, int channels)
{
    for (int i = 0; i < channels; i++)
        if (abs((int)p[i] - (int)want[i]) > 2)
            return false;
    return true;
}

// Is the device still usable? Draws an ordinary quad over a black target and
// reads it. A driver that dropped the draw before this one reports it here,
// at the next draw, so this is also where the device would be lost.
static std::string later(Scene &s, bool *ok)
{
    static const float black[4] = {0, 0, 0, 1};
    unsigned char px[SIDE * SIDE][4] = {};
    bind(s, s.rgba, s.quad, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, black);
    s.g.ctx->Draw(4, 0);
    HRESULT hr = read_target(s, px);
    int same = 0;
    if (SUCCEEDED(hr))
        for (UINT i = 0; i < SIDE * SIDE; i++)
            same += close_to(px[i], LATER, 4);
    HRESULT reason = s.g.dev->GetDeviceRemovedReason();
    bool good = SUCCEEDED(hr) && same == (int)(SIDE * SIDE) && reason == S_OK;
    if (!good)
        *ok = false;
    char b[300];
    snprintf(b, sizeof b,
             "\"later_draw\": {\"read_hr\": \"0x%08lx\", \"pixel\": [%d, %d, %d, %d], \"same_pixels\": %d, "
             "\"match\": %s}, \"removed_reason\": \"0x%08lx\", \"device_usable\": %s",
             (unsigned long)hr, px[0][0], px[0][1], px[0][2], px[0][3], same,
             SUCCEEDED(hr) && same == (int)(SIDE * SIDE) ? "true" : "false", (unsigned long)reason,
             good ? "true" : "false");
    return b;
}

static std::string fail_case(const char *name, const char *step, HRESULT hr)
{
    char b[200];
    snprintf(b, sizeof b, "{\"case\": \"%s\", \"failed\": \"%s\", \"hr\": \"0x%08lx\", \"pass\": false}", name, step,
             (unsigned long)hr);
    return b;
}

static std::string case_bgrx(Report &r, int *passed, const char *adapter_key)
{
    const char *name = "bgrx-vertex";
    Scene s;
    if (!scene_open(r, s, adapter_key)) {
        scene_close(s);
        return fail_case(name, "setup", E_FAIL);
    }
    std::string out = "{\"case\": \"bgrx-vertex\", ";
    bool ok = true;
    UINT support = 0;
    HRESULT hr = s.g.dev->CheckFormatSupport(DXGI_FORMAT_B8G8R8X8_UNORM, &support);
    const bool supported = SUCCEEDED(hr) && (support & D3D11_FORMAT_SUPPORT_IA_VERTEX_BUFFER);
    out += supported ? "\"vertex_format_supported\": true, " : "\"vertex_format_supported\": false, ";
    if (supported) {
        // The bytes are B, G, R, X in memory. X is not read: Direct3D leaves the fourth component undefined.
        static const unsigned char bgrx[4] = {0x10, 0x80, 0xf0, 0x55};
        static const unsigned char want[3] = {0xf0, 0x80, 0x10};
        static const float white[4] = {1, 1, 1, 1};
        Vertex v[4];
        fill_quad(v, bgrx);
        ID3D11Buffer *vb = make_buffer(s.g.dev, D3D11_BIND_VERTEX_BUFFER, 0, v, sizeof v, &hr);
        ID3D11InputLayout *layout = SUCCEEDED(hr) ? make_layout(s, DXGI_FORMAT_B8G8R8X8_UNORM, &hr) : NULL;
        if (FAILED(hr)) {
            if (vb)
                vb->Release();
            scene_close(s);
            return fail_case(name, "CreateInputLayout(B8G8R8X8_UNORM)", hr);
        }
        unsigned char px[SIDE * SIDE][4] = {};
        bind(s, layout, vb, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, white);
        s.g.ctx->Draw(4, 0);
        hr = read_target(s, px);
        int same = 0;
        if (SUCCEEDED(hr))
            for (UINT i = 0; i < SIDE * SIDE; i++)
                same += close_to(px[i], want, 3);
        if (FAILED(hr) || same != (int)(SIDE * SIDE))
            ok = false;
        char b[240];
        snprintf(b, sizeof b,
                 "\"read_hr\": \"0x%08lx\", \"expected_rgb\": [%d, %d, %d], \"pixel\": [%d, %d, %d, %d], "
                 "\"same_pixels\": %d, ",
                 (unsigned long)hr, want[0], want[1], want[2], px[0][0], px[0][1], px[0][2], px[0][3], same);
        out += b;
        layout->Release();
        vb->Release();
    }
    out += later(s, &ok);
    out += ok ? ", \"pass\": true}" : ", \"pass\": false}";
    if (ok)
        (*passed)++;
    scene_close(s);
    return out;
}

// Four points, one in each pixel of the top row, each of its own colour.
// With no index buffer every index reads 0, so only the first is drawn.
static std::string case_null_index(Report &r, bool indirect, int *passed)
{
    const char *name = indirect ? "null-index-indirect" : "null-index";
    Scene s;
    if (!scene_open(r, s, NULL)) {
        scene_close(s);
        return fail_case(name, "setup", E_FAIL);
    }
    if (indirect && s.g.level < D3D_FEATURE_LEVEL_11_0) {
        scene_close(s);
        (*passed)++;
        return std::string("{\"case\": \"") + name + "\", \"skipped\": \"feature level below 11.0\", \"pass\": true}";
    }
    static const unsigned char colours[4][4] = {
        {255, 0, 0, 255}, {0, 255, 0, 255}, {0, 0, 255, 255}, {255, 255, 0, 255}};
    static const float black[4] = {0, 0, 0, 1};
    Vertex v[4];
    for (int i = 0; i < 4; i++) {
        v[i].x = ((float)i + 0.5f) / (float)SIDE * 2.0f - 1.0f;
        v[i].y = 1.0f - 0.5f / (float)SIDE * 2.0f;
        memcpy(v[i].c, colours[i], 4);
    }
    HRESULT hr;
    ID3D11Buffer *vb = make_buffer(s.g.dev, D3D11_BIND_VERTEX_BUFFER, 0, v, sizeof v, &hr);
    if (FAILED(hr)) {
        scene_close(s);
        return fail_case(name, "CreateBuffer(points)", hr);
    }
    ID3D11Buffer *args = NULL;
    if (indirect) {
        static const D3D11_DRAW_INDEXED_INSTANCED_INDIRECT_ARGS a = {4, 1, 0, 0, 0};
        args = make_buffer(s.g.dev, 0, D3D11_RESOURCE_MISC_DRAWINDIRECT_ARGS, &a, sizeof a, &hr);
        if (FAILED(hr)) {
            vb->Release();
            scene_close(s);
            return fail_case(name, "CreateBuffer(arguments)", hr);
        }
    }
    bool ok = true;
    unsigned char px[SIDE * SIDE][4] = {};
    bind(s, s.rgba, vb, D3D11_PRIMITIVE_TOPOLOGY_POINTLIST, black);
    s.g.ctx->IASetIndexBuffer(NULL, DXGI_FORMAT_R32_UINT, 0);
    if (indirect)
        s.g.ctx->DrawIndexedInstancedIndirect(args, 0);
    else
        s.g.ctx->DrawIndexed(4, 0, 0);
    hr = read_target(s, px);
    // Which of the four points is on the target, and how many other pixels are not black.
    int lit[4] = {}, stray = 0;
    static const unsigned char none[3] = {0, 0, 0};
    if (SUCCEEDED(hr))
        for (UINT i = 0; i < SIDE * SIDE; i++) {
            if (i < 4)
                lit[i] = close_to(px[i], colours[i], 3);
            if (!close_to(px[i], none, 3) && !(i < 4 && lit[i]))
                stray++;
        }
    const bool first_only = SUCCEEDED(hr) && lit[0] && !lit[1] && !lit[2] && !lit[3] && !stray;
    if (!first_only)
        ok = false;
    char b[300];
    snprintf(b, sizeof b,
             "{\"case\": \"%s\", \"read_hr\": \"0x%08lx\", \"points_drawn\": [%d, %d, %d, %d], \"other_pixels\": %d, "
             "\"first_vertex_only\": %s, ",
             name, (unsigned long)hr, lit[0], lit[1], lit[2], lit[3], stray, first_only ? "true" : "false");
    std::string out = b;
    out += later(s, &ok);
    out += ok ? ", \"pass\": true}" : ", \"pass\": false}";
    if (ok)
        (*passed)++;
    if (args)
        args->Release();
    vb->Release();
    scene_close(s);
    return out;
}

int main(int argc, char **argv)
{
    Report r("draws");
    const char *out = arg_str(argc, argv, "--out", "draws_probe.json");
    int passed = 0;
    std::string cases = "[";
    cases += case_bgrx(r, &passed, "adapter") + ",\n    ";
    cases += case_null_index(r, false, &passed) + ",\n    ";
    cases += case_null_index(r, true, &passed) + "]";
    r.raw("cases", cases);
    r.integer("cases_passed", passed);
    return r.finish(out);
}
