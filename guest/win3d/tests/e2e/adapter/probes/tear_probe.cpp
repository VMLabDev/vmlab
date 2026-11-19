// P3 to P5, a flip shows a finished frame: the tearing probe.
//
// Draws a borderless window (800x480 at 32,32) in which every frame is one
// colour of an eight-colour palette, the next colour each frame. The frame
// is not cleared. It is drawn as --strips horizontal strips, top to bottom,
// by a pixel shader that loops --iters times per pixel, so that drawing a
// frame takes the GPU a while. A frame that reaches the screen before its
// rendering has finished shows rows of an earlier frame in a second colour:
// the stale bands of card #134.
//
// The probe animates for --animate-ms, then holds the last frame for
// --freeze-ms, and repeats for --seconds. A band that is on the screen during
// a hold stays until the next flip, which is how a user sees it. The host
// reads the window's rows from the console while this runs
// (tests/e2e/adapter/rfbgrab.py); this program measures nothing but its own
// frame rate.
//
// --mode flip   DXGI flip model: the frame goes to DWM or to the scanout.
// --mode blt    DXGI blt model: a present of the Blt kind, as windowed
//               Direct3D 9, WPF and DirectDraw make.
#include "adapter.h"

static const char *hlsl = R"(
cbuffer strip : register(b0) { float4 rect; float4 colour; float4 params; };
float4 vs(uint id : SV_VertexID) : SV_Position {
    float2 p = float2(id == 1 || id == 2 || id == 4 ? rect.z : rect.x, id == 2 || id == 4 || id == 5 ? rect.w : rect.y);
    return float4(p, 0, 1);
}
float4 ps(float4 pos : SV_Position) : SV_Target {
    float a = 0;
    int n = (int)params.x;
    [loop] for (int i = 0; i < n; i++)
        a += sin(pos.x * 0.001 * i + a);
    return float4(colour.rgb + a * 1e-9, 1);
}
)";

struct StripConstants {
    float rect[4];
    float colour[4];
    float params[4];
};

// The host has the same table (tests/e2e/adapter/rfbgrab.py).
static const float palette[8][3] = {{1, 0, 0}, {0, 1, 0}, {0, 0, 1}, {1, 1, 0}, {1, 0, 1}, {0, 1, 1}, {1, 1, 1}, {0.375f, 0.375f, 0.375f}};

int main(int argc, char **argv)
{
    Report r("tear");
    const char *out = arg_str(argc, argv, "--out", "tear_probe.json");
    const char *mode = arg_str(argc, argv, "--mode", "flip");
    int seconds = arg_int(argc, argv, "--seconds", 30);
    int strips = arg_int(argc, argv, "--strips", 48);
    int iters = arg_int(argc, argv, "--iters", 400);
    int animate_ms = arg_int(argc, argv, "--animate-ms", 3000);
    int freeze_ms = arg_int(argc, argv, "--freeze-ms", 3000);
    const int X = 32, Y = 32, W = 800, H = 480;
    bool flip = strcmp(mode, "blt") != 0;
    if (strips < 1)
        strips = 1;

    r.str("mode", flip ? "flip" : "blt");
    r.integer("strips", strips);
    r.integer("iters", iters);
    r.raw("window", "{\"x\": 32, \"y\": 32, \"width\": 800, \"height\": 480}");

    Gpu g;
    if (!gpu_open(r, g, false, "adapter"))
        return r.finish(out);
    HWND hwnd = make_window(L"vmlab-win3d adapter: tear", X, Y, W, H, NULL);
    IDXGISwapChain1 *swap = hwnd ? make_swap_chain(r, g, hwnd, W, H, flip) : NULL;
    if (!swap) {
        if (!hwnd)
            r.err("CreateWindowEx failed: %lu", GetLastError());
        return r.finish(out);
    }

    const void *vs_code, *ps_code;
    size_t vs_size, ps_size;
    ID3D11VertexShader *vs = NULL;
    ID3D11PixelShader *ps = NULL;
    ID3D11Buffer *cb = NULL;
    ID3D11RasterizerState *rs = NULL;
    D3D11_BUFFER_DESC bd = {};
    bd.ByteWidth = sizeof(StripConstants);
    bd.Usage = D3D11_USAGE_DEFAULT;
    bd.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
    D3D11_RASTERIZER_DESC rd = {};
    rd.FillMode = D3D11_FILL_SOLID;
    rd.CullMode = D3D11_CULL_NONE;
    rd.DepthClipEnable = TRUE;
    if (!compile_hlsl(r, hlsl, "vs", "vs_4_0", &vs_code, &vs_size) ||
        !compile_hlsl(r, hlsl, "ps", "ps_4_0", &ps_code, &ps_size) ||
        r.failed(g.dev->CreateVertexShader(vs_code, vs_size, NULL, &vs), "CreateVertexShader") ||
        r.failed(g.dev->CreatePixelShader(ps_code, ps_size, NULL, &ps), "CreatePixelShader") ||
        r.failed(g.dev->CreateBuffer(&bd, NULL, &cb), "CreateBuffer") ||
        r.failed(g.dev->CreateRasterizerState(&rd, &rs), "CreateRasterizerState"))
        return r.finish(out);

    D3D11_VIEWPORT vp = {0, 0, (float)W, (float)H, 0, 1};
    unsigned frame = 0, holds = 0;
    double start = now_ms(), drawing_ms = 0;
    while (now_ms() - start < seconds * 1000.0 && r.errors.empty()) {
        double phase = now_ms();
        while (now_ms() - phase < animate_ms && r.errors.empty()) {
            pump();
            ID3D11Texture2D *bb = NULL;
            ID3D11RenderTargetView *rtv = NULL;
            if (r.failed(swap->GetBuffer(0, __uuidof(ID3D11Texture2D), (void **)&bb), "GetBuffer") ||
                r.failed(g.dev->CreateRenderTargetView(bb, NULL, &rtv), "CreateRenderTargetView"))
                break;
            g.ctx->OMSetRenderTargets(1, &rtv, NULL);
            g.ctx->RSSetViewports(1, &vp);
            g.ctx->RSSetState(rs);
            g.ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            g.ctx->IASetInputLayout(NULL);
            g.ctx->VSSetShader(vs, NULL, 0);
            g.ctx->PSSetShader(ps, NULL, 0);
            g.ctx->VSSetConstantBuffers(0, 1, &cb);
            g.ctx->PSSetConstantBuffers(0, 1, &cb);
            const float *c = palette[frame % 8];
            double t0 = now_ms();
            for (int s = 0; s < strips; s++) {
                StripConstants k = {};
                k.rect[0] = -1;
                k.rect[2] = 1;
                k.rect[1] = 1.0f - 2.0f * (float)s / (float)strips;
                k.rect[3] = 1.0f - 2.0f * (float)(s + 1) / (float)strips;
                k.colour[0] = c[0];
                k.colour[1] = c[1];
                k.colour[2] = c[2];
                k.colour[3] = 1;
                k.params[0] = (float)iters;
                g.ctx->UpdateSubresource(cb, 0, NULL, &k, 0, 0);
                g.ctx->Draw(6, 0);
            }
            g.ctx->OMSetRenderTargets(0, NULL, NULL);
            rtv->Release();
            bb->Release();
            HRESULT hr = swap->Present(1, 0);
            drawing_ms += now_ms() - t0;
            if (r.failed(hr, "Present"))
                break;
            frame++;
        }
        holds++;
        phase = now_ms();
        while (now_ms() - phase < freeze_ms) {
            pump();
            Sleep(10);
        }
    }
    r.integer("frames_presented", frame);
    r.integer("holds", holds);
    if (frame) {
        r.num("draw_and_present_ms", drawing_ms / frame);
        r.num("animating_fps", frame * 1000.0 / drawing_ms);
    }
    swap->Release();
    DestroyWindow(hwnd);
    gpu_close(g);
    return r.finish(out);
}
