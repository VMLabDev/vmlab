/* ExecuteIndirect with vertex and index buffer view arguments
 * (patches/vkd3d-proton/0007, docs/ei.md).
 *
 * Venus does not forward VK_EXT_device_generated_commands, so vkd3d-proton
 * emulates command signatures that set vertex or index buffer views: it
 * copies what each sequence draws into scratch buffers on the GPU. This probe
 * draws a 5x5 grid of cells, one quad per cell, with five ExecuteIndirect
 * calls whose sequences take their buffers from two resources at different
 * offsets, and checks every cell against the colour it must have:
 *
 *   A  cells 0-7    VBV slot 0 + IBV + root constants + DRAW_INDEXED. Eight
 *                   sequences, the count buffer says seven. 16- and 32-bit
 *                   indices, StartIndexLocation, positive and negative
 *                   BaseVertexLocation, two vertex strides. Slot 1 (colour)
 *                   is bound on the CPU.
 *   B  cells 8-11   VBV slot 1, per instance + root constants + DRAW. Two
 *                   instances a sequence, StartInstanceLocation; slot 0 is
 *                   bound on the CPU and read from StartVertexLocation.
 *   C  cells 12-15  VBV slot 0 + IBV + DRAW_INDEXED, triangle strips with the
 *                   0xFFFF strip cut: two quads a sequence. No root argument.
 *   D  cells 16-17  VBV slot 0 + DRAW, not indexed, StartVertexLocation.
 *   E  cells 18-19  IBV + DRAW_INDEXED: only the index buffer comes from the
 *                   arguments.
 *
 * Cells 7 and 20-24 must stay the clear colour. The vertex shader also
 * checks SV_VertexID and SV_InstanceID and draws magenta when they are not
 * what Direct3D defines. The argument and count buffers are in a default
 * heap and are written by a copy in the same command list, so the arguments
 * exist only on the GPU when ExecuteIndirect is recorded.
 *
 * The scene is drawn on the default adapter and, in a child process, on
 * WARP. Where the package is installed the child runs from a temporary
 * directory holding Microsoft's d3d12.dll (the package keeps it as
 * System32\vmlab-win3d\d3d12-native.dll) and D3D12Core.dll, because the
 * package's d3d12.dll has no WARP. Both pictures are compared with the
 * expected cells and with each other, 1/255 a channel allowed.
 *
 *   ei_views_probe.exe [--out FILE.json] [--seconds N]
 *
 * --seconds is accepted and ignored (tests/guest/Run-Probe.ps1 passes it).
 * Exit 0 when both adapters draw every cell right and the pictures agree,
 * 1 when not, 2 when the scene could not be drawn. Beside FILE.json it
 * writes FILE.gpu.bmp, FILE.warp.bmp and FILE.vkd3d.log (vkd3d-proton's log
 * of the default-adapter run; "emulated_signatures" counts its lines about
 * signatures that fetch vertices). Built by probes/d3d12-ei/build.sh. */
#include <windows.h>
#include <d3d12.h>
#include <d3dcommon.h>
#include <dxgi1_4.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <string>
#include <vector>

#define GRID 5
#define CELL 64
#define RT (GRID * CELL)
#define CELLS (GRID * GRID)
#define TOLERANCE 1

typedef std::vector<uint8_t> Bytes;

static std::string g_errors;

static bool failed(HRESULT hr, const char *what)
{
    if (SUCCEEDED(hr))
        return false;
    char b[200];
    snprintf(b, sizeof(b), "%s\"%s: 0x%08lx\"", g_errors.empty() ? "" : ", ", what, (unsigned long)hr);
    g_errors += b;
    return true;
}

static void error(const char *fmt, ...)
{
    char b[400], q[420];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(b, sizeof(b), fmt, ap);
    va_end(ap);
    snprintf(q, sizeof(q), "%s\"%s\"", g_errors.empty() ? "" : ", ", b);
    g_errors += q;
}

/* ---- the scene, as bytes ------------------------------------------------ */

struct Rgb { uint8_t r, g, b; };

static const Rgb CLEAR = {51, 102, 153};
static const Rgb JUNK = {255, 255, 255};

static size_t put(Bytes &b, const void *p, size_t n)
{
    size_t at = b.size();
    b.insert(b.end(), (const uint8_t *)p, (const uint8_t *)p + n);
    return at;
}
static size_t put_u32(Bytes &b, uint32_t v) { return put(b, &v, 4); }
static size_t put_u16(Bytes &b, uint16_t v) { return put(b, &v, 2); }
static size_t put_f32(Bytes &b, float v) { return put(b, &v, 4); }
static size_t put_u64(Bytes &b, uint64_t v) { return put(b, &v, 8); }
static void pad_to(Bytes &b, size_t alignment, uint8_t fill)
{
    while (b.size() % alignment)
        b.push_back(fill);
}
static void pad(Bytes &b, size_t n, uint8_t fill) { b.insert(b.end(), n, fill); }

/* The corners of the quad in a cell: the middle half, on pixel boundaries. */
static void cell_quad(int cell, float xy[4][2])
{
    float x0 = -1.0f + (float)((cell % GRID) * CELL + CELL / 4) / (RT / 2);
    float y0 = 1.0f - (float)((cell / GRID) * CELL + CELL / 4) / (RT / 2);
    float d = (float)(CELL / 2) / (RT / 2);
    float q[4][2] = {{x0, y0}, {x0 + d, y0}, {x0, y0 - d}, {x0 + d, y0 - d}};
    memcpy(xy, q, sizeof(q));
}

/* A position that draws nothing: what a wrong address would fetch. */
static void put_junk_position(Bytes &b) { put_f32(b, 3.0f); put_f32(b, 3.0f); }

static void put_color(Bytes &b, Rgb c)
{
    uint8_t v[4] = {c.r, c.g, c.b, 255};
    put(b, v, 4);
}

static Rgb cell_color(int cell)
{
    static const uint8_t levels[4] = {255, 128, 64, 192};
    Rgb c = {levels[cell % 4], levels[(cell / 4) % 4], levels[(cell + cell / 4 + 1) % 4]};
    return c;
}

struct Scene
{
    /* Two resources hold the geometry: an upload heap and a default heap. */
    Bytes geo[2];
    /* Argument records of every call, then the count of call A. */
    Bytes args;
    size_t args_offset[5];
    size_t count_offset;
    UINT stride[5];
    UINT sequences[5];
    /* CPU-bound views, as offsets into geo[0]. */
    size_t a_colors, b_positions, c_colors, d_colors, e_positions, e_colors;
    UINT c_vid_base;
    Rgb expected[CELLS];
};

/* GPU addresses are known only once the resources exist: views are written
 * with the resource index in the top byte, and patched afterwards. */
static uint64_t view_va(int resource, size_t offset) { return ((uint64_t)(resource + 1) << 56) | offset; }

static void build_scene(Scene &s)
{
    static const UINT deltas[8] = {0, 7, 100, 60000, 2, 1, 70000, 9};
    float q[4][2];
    int i, k;

    for (i = 0; i < CELLS; i++)
        s.expected[i] = CLEAR;
    pad(s.geo[0], 20, 0xcd);
    pad(s.geo[1], 36, 0xcd);

    /* ---- A: cells 0-7 ---- */
    s.a_colors = s.geo[0].size();
    for (i = 0; i < 3 + 5 * 8 + 4; i++)
        put_color(s.geo[0], JUNK);
    s.args_offset[0] = s.args.size();
    s.stride[0] = 72;
    s.sequences[0] = 8;
    for (i = 0; i < 8; i++)
    {
        int res = i & 1;
        Bytes &g = s.geo[res];
        UINT element = 3 + 5 * i, delta = deltas[i];
        UINT vstride = (i & 2) ? 16 : 8;
        bool wide = (i % 3) != 0 || delta > 0xfff0;
        UINT start_index = 2 + i;
        Rgb color = cell_color(i);
        float tint[3] = {(i & 1) ? 1.0f : 0.0f, 1.0f, (i & 4) ? 0.0f : 1.0f};
        size_t vb, ib, record;

        for (k = 0; k < 4; k++)
            memcpy(&s.geo[0][s.a_colors + (element + k) * 4], &color, 3);
        if (i < 7)
        {
            Rgb e = {(uint8_t)(color.r * tint[0]), color.g, (uint8_t)(color.b * tint[2])};
            s.expected[i] = e;
        }

        cell_quad(i, q);
        pad_to(g, 4, 0xcd);
        vb = g.size();
        for (k = 0; k < (int)element + 4; k++)
        {
            if (k >= (int)element)
                put(g, q[k - element], 8);
            else
                put_junk_position(g);
            if (vstride == 16)
                put_junk_position(g);
        }

        /* 16-bit index buffers start on odd and even multiples of 2 bytes. */
        pad_to(g, 4, 0xcd);
        if (!wide && (i & 1))
            pad(g, 2, 0xcd);
        ib = g.size();
        for (k = 0; k < (int)start_index + 6; k++)
        {
            static const UINT quad[6] = {0, 1, 2, 2, 1, 3};
            UINT index = k >= (int)start_index ? delta + quad[k - start_index] : delta + 3;
            if (wide)
                put_u32(g, index);
            else
                put_u16(g, (uint16_t)index);
        }

        record = s.args.size();
        put_u64(s.args, view_va(res, vb));
        put_u32(s.args, (element + 4) * vstride);
        put_u32(s.args, vstride);
        put_u64(s.args, view_va(res, ib));
        put_u32(s.args, (start_index + 6) * (wide ? 4 : 2));
        put_u32(s.args, wide ? DXGI_FORMAT_R32_UINT : DXGI_FORMAT_R16_UINT);
        put_f32(s.args, tint[0]);
        put_f32(s.args, tint[1]);
        put_f32(s.args, tint[2]);
        put_u32(s.args, delta);                          /* vid_base */
        put_u32(s.args, 6);                              /* IndexCountPerInstance */
        put_u32(s.args, 1);                              /* InstanceCount */
        put_u32(s.args, start_index);                    /* StartIndexLocation */
        put_u32(s.args, (uint32_t)((int)element - (int)delta)); /* BaseVertexLocation */
        put_u32(s.args, 0);                              /* StartInstanceLocation */
        pad(s.args, s.stride[0] - (s.args.size() - record), 0xee);
    }

    /* ---- B: cells 8-11, two instances a sequence ---- */
    pad_to(s.geo[0], 4, 0xcd);
    s.b_positions = s.geo[0].size();
    {
        float d = (float)(CELL / 2) / (RT / 2);
        float unit[4][2] = {{0, 0}, {d, 0}, {0, -d}, {d, -d}};
        put_junk_position(s.geo[0]);
        put_junk_position(s.geo[0]);
        put(s.geo[0], unit, sizeof(unit));
    }
    s.args_offset[1] = s.args.size();
    s.stride[1] = 48;
    s.sequences[1] = 2;
    for (i = 0; i < 2; i++)
    {
        int res = 1 - i;
        Bytes &g = s.geo[res];
        UINT start_instance = 1 + 2 * i;
        size_t vb, record;

        pad_to(g, 4, 0xcd);
        vb = g.size();
        for (k = 0; k < (int)start_instance + 2; k++)
        {
            if (k >= (int)start_instance)
            {
                int cell = 8 + 2 * i + (k - start_instance);
                cell_quad(cell, q);
                put(g, q[0], 8);
                put_color(g, cell_color(cell));
                s.expected[cell] = cell_color(cell);
            }
            else
            {
                put_junk_position(g);
                put_color(g, JUNK);
            }
        }

        record = s.args.size();
        put_u64(s.args, view_va(res, vb));
        put_u32(s.args, (start_instance + 2) * 12);
        put_u32(s.args, 12);
        put_f32(s.args, 1.0f);
        put_f32(s.args, 1.0f);
        put_f32(s.args, 1.0f);
        put_u32(s.args, 0);                              /* vid_base */
        put_u32(s.args, 4);                              /* VertexCountPerInstance */
        put_u32(s.args, 2);                              /* InstanceCount */
        put_u32(s.args, 2);                              /* StartVertexLocation */
        put_u32(s.args, start_instance);                 /* StartInstanceLocation */
        pad(s.args, s.stride[1] - (s.args.size() - record), 0xee);
    }

    /* ---- C: cells 12-15, strips with a cut ---- */
    s.c_vid_base = 21;
    pad_to(s.geo[0], 4, 0xcd);
    s.c_colors = s.geo[0].size();
    for (i = 0; i < 16; i++)
    {
        put_color(s.geo[0], cell_color(12 + i / 4));
        s.expected[12 + i / 4] = cell_color(12 + i / 4);
    }
    s.args_offset[2] = s.args.size();
    s.stride[2] = 52;
    s.sequences[2] = 2;
    for (i = 0; i < 2; i++)
    {
        int res = i;
        Bytes &g = s.geo[res];
        size_t vb, ib, record;

        /* Element 8 * i is the first vertex of this sequence: with the second
         * view starting 8 elements early, BaseVertexLocation is 8 - 21. */
        pad_to(g, 4, 0xcd);
        vb = g.size();
        for (k = 0; k < 8 * i; k++)
            put_junk_position(g);
        for (k = 0; k < 8; k++)
        {
            cell_quad(12 + 2 * i + k / 4, q);
            put(g, q[k % 4], 8);
        }

        pad_to(g, 4, 0xcd);
        pad(g, 2 * i, 0xcd);
        ib = g.size();
        put_u16(g, 0);
        for (k = 0; k < 9; k++)
            put_u16(g, k == 4 ? 0xffff : (uint16_t)(s.c_vid_base + k - (k > 4)));

        record = s.args.size();
        put_u64(s.args, view_va(res, vb));
        put_u32(s.args, (8 * i + 8) * 8);
        put_u32(s.args, 8);
        put_u64(s.args, view_va(res, ib));
        put_u32(s.args, 10 * 2);
        put_u32(s.args, DXGI_FORMAT_R16_UINT);
        put_u32(s.args, 9);                              /* IndexCountPerInstance */
        put_u32(s.args, 1);
        put_u32(s.args, 1);                              /* StartIndexLocation */
        put_u32(s.args, (uint32_t)(8 * i - (int)s.c_vid_base));
        put_u32(s.args, 0);
        pad(s.args, s.stride[2] - (s.args.size() - record), 0xee);
    }

    /* ---- D: cells 16-17, not indexed ---- */
    pad_to(s.geo[0], 4, 0xcd);
    s.d_colors = s.geo[0].size();
    for (i = 0; i < 17; i++)
        put_color(s.geo[0], i >= 5 && i < 11 ? cell_color(16) : i >= 11 ? cell_color(17) : JUNK);
    s.expected[16] = cell_color(16);
    s.expected[17] = cell_color(17);
    s.args_offset[3] = s.args.size();
    s.stride[3] = 32;
    s.sequences[3] = 2;
    for (i = 0; i < 2; i++)
    {
        static const int list[6] = {0, 1, 2, 2, 1, 3};
        int res = i;
        Bytes &g = s.geo[res];
        UINT start_vertex = 5 + 6 * i;
        size_t vb, record;

        cell_quad(16 + i, q);
        pad_to(g, 4, 0xcd);
        vb = g.size();
        for (k = 0; k < (int)start_vertex + 6; k++)
        {
            if (k >= (int)start_vertex)
                put(g, q[list[k - start_vertex]], 8);
            else
                put_junk_position(g);
        }

        record = s.args.size();
        put_u64(s.args, view_va(res, vb));
        put_u32(s.args, (start_vertex + 6) * 8);
        put_u32(s.args, 8);
        put_u32(s.args, 6);                              /* VertexCountPerInstance */
        put_u32(s.args, 1);
        put_u32(s.args, start_vertex);                   /* StartVertexLocation */
        put_u32(s.args, 0);
        pad(s.args, s.stride[3] - (s.args.size() - record), 0xee);
    }

    /* ---- E: cells 18-19, only the index buffer from the arguments ---- */
    pad_to(s.geo[0], 4, 0xcd);
    s.e_positions = s.geo[0].size();
    for (i = 0; i < 2; i++)
    {
        cell_quad(18 + i, q);
        put(s.geo[0], q, sizeof(q));
    }
    s.e_colors = s.geo[0].size();
    for (i = 0; i < 8; i++)
        put_color(s.geo[0], cell_color(18 + i / 4));
    s.expected[18] = cell_color(18);
    s.expected[19] = cell_color(19);
    s.args_offset[4] = s.args.size();
    s.stride[4] = 36;
    s.sequences[4] = 2;
    for (i = 0; i < 2; i++)
    {
        static const UINT quad[6] = {0, 1, 2, 2, 1, 3};
        int res = 1 - i;
        Bytes &g = s.geo[res];
        size_t ib, record;

        pad_to(g, 4, 0xcd);
        ib = g.size();
        for (k = 0; k < 6; k++)
        {
            if (i)
                put_u32(g, quad[k]);
            else
                put_u16(g, (uint16_t)quad[k]);
        }

        record = s.args.size();
        put_u64(s.args, view_va(res, ib));
        put_u32(s.args, 6 * (i ? 4 : 2));
        put_u32(s.args, i ? DXGI_FORMAT_R32_UINT : DXGI_FORMAT_R16_UINT);
        put_u32(s.args, 6);
        put_u32(s.args, 1);
        put_u32(s.args, 0);
        put_u32(s.args, (uint32_t)(4 * i));              /* BaseVertexLocation */
        put_u32(s.args, 0);
        pad(s.args, s.stride[4] - (s.args.size() - record), 0xee);
    }

    pad_to(s.args, 4, 0xee);
    s.count_offset = put_u32(s.args, 7);
    pad_to(s.geo[0], 16, 0xcd);
    pad_to(s.geo[1], 16, 0xcd);
}

/* Replace the tagged addresses in the argument records with real ones. */
static void patch_views(Scene &s, const D3D12_GPU_VIRTUAL_ADDRESS va[2])
{
    for (size_t at = 0; at + 8 <= s.args.size(); at += 4)
    {
        uint64_t v;
        memcpy(&v, &s.args[at], 8);
        unsigned tag = (unsigned)(v >> 56);
        if ((tag == 1 || tag == 2) && (v & 0x00ffffffffffffffull) < s.geo[tag - 1].size())
        {
            v = va[tag - 1] + (v & 0x00ffffffffffffffull);
            memcpy(&s.args[at], &v, 8);
            at += 4;
        }
    }
}

/* ---- Direct3D 12 -------------------------------------------------------- */

static const char HLSL[] =
    "cbuffer K : register(b0) { float3 tint; uint vid_base; uint vid_limit; uint iid_limit; };\n"
    "struct VO { float4 pos : SV_Position; float4 col : COLOR; };\n"
    "VO vs_list(float2 p : POSITION, float4 c : COLOR, uint vid : SV_VertexID) {\n"
    "    VO o; o.pos = float4(p, 0, 1);\n"
    "    o.col = (vid - vid_base) < vid_limit ? float4(c.rgb * tint, 1) : float4(1, 0, 1, 1);\n"
    "    return o; }\n"
    "VO vs_inst(float2 p : POSITION, float2 off : OFFSET, float4 c : COLOR,\n"
    "        uint vid : SV_VertexID, uint iid : SV_InstanceID) {\n"
    "    VO o; o.pos = float4(p + off, 0, 1);\n"
    "    o.col = (vid - vid_base) < vid_limit && iid < iid_limit ? float4(c.rgb * tint, 1) : float4(1, 0, 1, 1);\n"
    "    return o; }\n"
    "float4 ps(VO i) : SV_Target { return i.col; }\n";

typedef HRESULT(WINAPI *D3DCompileFn)(LPCVOID, SIZE_T, LPCSTR, const void *, void *, LPCSTR, LPCSTR, UINT, UINT,
        ID3DBlob **, ID3DBlob **);

static ID3DBlob *compile(const char *entry, const char *target)
{
    static D3DCompileFn fn;
    ID3DBlob *code = NULL, *messages = NULL;
    if (!fn)
    {
        HMODULE m = LoadLibraryW(L"d3dcompiler_47.dll");
        fn = m ? (D3DCompileFn)(void *)GetProcAddress(m, "D3DCompile") : NULL;
        if (!fn)
        {
            error("d3dcompiler_47.dll has no D3DCompile");
            return NULL;
        }
    }
    if (failed(fn(HLSL, sizeof(HLSL) - 1, "ei_views", NULL, NULL, entry, target, 0, 0, &code, &messages), entry))
        return NULL;
    if (messages)
        messages->Release();
    return code;
}

static ID3D12Resource *make_buffer(ID3D12Device *dev, D3D12_HEAP_TYPE heap, UINT64 size,
        D3D12_RESOURCE_STATES state, const char *what)
{
    D3D12_HEAP_PROPERTIES hp = {};
    D3D12_RESOURCE_DESC rd = {};
    ID3D12Resource *res = NULL;
    hp.Type = heap;
    rd.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
    rd.Width = size;
    rd.Height = 1;
    rd.DepthOrArraySize = 1;
    rd.MipLevels = 1;
    rd.SampleDesc.Count = 1;
    rd.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    if (failed(dev->CreateCommittedResource(&hp, D3D12_HEAP_FLAG_NONE, &rd, state, NULL,
            __uuidof(ID3D12Resource), (void **)&res), what))
        return NULL;
    return res;
}

static bool upload(ID3D12Resource *res, const Bytes &data)
{
    void *p = NULL;
    D3D12_RANGE none = {0, 0};
    if (failed(res->Map(0, &none, &p), "Map"))
        return false;
    memcpy(p, data.data(), data.size());
    res->Unmap(0, NULL);
    return true;
}

static void transition(ID3D12GraphicsCommandList *cl, ID3D12Resource *res, D3D12_RESOURCE_STATES before,
        D3D12_RESOURCE_STATES after)
{
    D3D12_RESOURCE_BARRIER b = {};
    b.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
    b.Transition.pResource = res;
    b.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
    b.Transition.StateBefore = before;
    b.Transition.StateAfter = after;
    cl->ResourceBarrier(1, &b);
}

static ID3D12PipelineState *make_pso(ID3D12Device *dev, ID3D12RootSignature *root, ID3DBlob *vs, ID3DBlob *ps,
        const D3D12_INPUT_ELEMENT_DESC *elems, UINT elem_count, bool strip_cut, const char *what)
{
    D3D12_GRAPHICS_PIPELINE_STATE_DESC pd = {};
    ID3D12PipelineState *pso = NULL;
    pd.pRootSignature = root;
    pd.VS.pShaderBytecode = vs->GetBufferPointer();
    pd.VS.BytecodeLength = vs->GetBufferSize();
    pd.PS.pShaderBytecode = ps->GetBufferPointer();
    pd.PS.BytecodeLength = ps->GetBufferSize();
    pd.BlendState.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL;
    pd.BlendState.RenderTarget[0].SrcBlend = D3D12_BLEND_ONE;
    pd.BlendState.RenderTarget[0].DestBlend = D3D12_BLEND_ZERO;
    pd.BlendState.RenderTarget[0].BlendOp = D3D12_BLEND_OP_ADD;
    pd.BlendState.RenderTarget[0].SrcBlendAlpha = D3D12_BLEND_ONE;
    pd.BlendState.RenderTarget[0].DestBlendAlpha = D3D12_BLEND_ZERO;
    pd.BlendState.RenderTarget[0].BlendOpAlpha = D3D12_BLEND_OP_ADD;
    pd.BlendState.RenderTarget[0].LogicOp = D3D12_LOGIC_OP_NOOP;
    pd.SampleMask = 0xffffffffu;
    pd.RasterizerState.FillMode = D3D12_FILL_MODE_SOLID;
    pd.RasterizerState.CullMode = D3D12_CULL_MODE_NONE;
    pd.RasterizerState.DepthClipEnable = TRUE;
    pd.InputLayout.pInputElementDescs = elems;
    pd.InputLayout.NumElements = elem_count;
    pd.IBStripCutValue = strip_cut ? D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_0xFFFF :
            D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_DISABLED;
    pd.PrimitiveTopologyType = D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE;
    pd.NumRenderTargets = 1;
    pd.RTVFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
    pd.SampleDesc.Count = 1;
    if (failed(dev->CreateGraphicsPipelineState(&pd, __uuidof(ID3D12PipelineState), (void **)&pso), what))
        return NULL;
    return pso;
}

static ID3D12CommandSignature *make_signature(ID3D12Device *dev, ID3D12RootSignature *root,
        const D3D12_INDIRECT_ARGUMENT_DESC *args, UINT count, UINT stride, const char *what)
{
    D3D12_COMMAND_SIGNATURE_DESC sd = {};
    ID3D12CommandSignature *sig = NULL;
    sd.ByteStride = stride;
    sd.NumArgumentDescs = count;
    sd.pArgumentDescs = args;
    if (failed(dev->CreateCommandSignature(&sd, root, __uuidof(ID3D12CommandSignature), (void **)&sig), what))
        return NULL;
    return sig;
}

struct Constants { float tint[3]; UINT vid_base, vid_limit, iid_limit; };

/* Draws the scene into pixels (RT x RT, RGBA). Returns false with g_errors set. */
static bool draw_scene(bool warp, std::vector<uint8_t> &pixels, std::string &adapter_name)
{
    IDXGIFactory4 *factory = NULL;
    IDXGIAdapter *adapter = NULL;
    ID3D12Device *dev = NULL;
    Scene s;
    build_scene(s);

    if (failed(CreateDXGIFactory1(__uuidof(IDXGIFactory4), (void **)&factory), "CreateDXGIFactory1"))
        return false;
    if (warp && failed(factory->EnumWarpAdapter(__uuidof(IDXGIAdapter), (void **)&adapter), "EnumWarpAdapter"))
        return false;
    if (failed(D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_11_0, __uuidof(ID3D12Device), (void **)&dev),
            warp ? "D3D12CreateDevice(WARP)" : "D3D12CreateDevice(default adapter)"))
        return false;
    {
        /* The default adapter is the first one DXGI lists. */
        IDXGIAdapter *a = adapter;
        DXGI_ADAPTER_DESC d = {};
        if ((a || SUCCEEDED(factory->EnumAdapters(0, &a))) && SUCCEEDED(a->GetDesc(&d)))
        {
            char name[256];
            WideCharToMultiByte(CP_UTF8, 0, d.Description, -1, name, sizeof(name), NULL, NULL);
            adapter_name = name;
        }
    }

    D3D12_COMMAND_QUEUE_DESC qd = {};
    ID3D12CommandQueue *queue = NULL;
    ID3D12CommandAllocator *alloc = NULL;
    ID3D12GraphicsCommandList *cl = NULL;
    ID3D12Fence *fence = NULL;
    qd.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
    if (failed(dev->CreateCommandQueue(&qd, __uuidof(ID3D12CommandQueue), (void **)&queue), "CreateCommandQueue") ||
            failed(dev->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, __uuidof(ID3D12CommandAllocator),
                    (void **)&alloc), "CreateCommandAllocator") ||
            failed(dev->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, alloc, NULL,
                    __uuidof(ID3D12GraphicsCommandList), (void **)&cl), "CreateCommandList") ||
            failed(dev->CreateFence(0, D3D12_FENCE_FLAG_NONE, __uuidof(ID3D12Fence), (void **)&fence), "CreateFence"))
        return false;

    /* Root signature: six root constants at b0. */
    D3D12_ROOT_PARAMETER rp = {};
    D3D12_ROOT_SIGNATURE_DESC rsd = {};
    ID3DBlob *blob = NULL;
    ID3D12RootSignature *root = NULL;
    rp.ParameterType = D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS;
    rp.Constants.Num32BitValues = 6;
    rp.ShaderVisibility = D3D12_SHADER_VISIBILITY_ALL;
    rsd.NumParameters = 1;
    rsd.pParameters = &rp;
    rsd.Flags = D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT;
    if (failed(D3D12SerializeRootSignature(&rsd, D3D_ROOT_SIGNATURE_VERSION_1, &blob, NULL),
            "D3D12SerializeRootSignature") ||
            failed(dev->CreateRootSignature(0, blob->GetBufferPointer(), blob->GetBufferSize(),
                    __uuidof(ID3D12RootSignature), (void **)&root), "CreateRootSignature"))
        return false;
    blob->Release();

    ID3DBlob *vs_list = compile("vs_list", "vs_5_0");
    ID3DBlob *vs_inst = compile("vs_inst", "vs_5_0");
    ID3DBlob *ps = compile("ps", "ps_5_0");
    if (!vs_list || !vs_inst || !ps)
        return false;

    static const D3D12_INPUT_ELEMENT_DESC list_elems[] = {
        {"POSITION", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 0, D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA, 0},
        {"COLOR", 0, DXGI_FORMAT_R8G8B8A8_UNORM, 1, 0, D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA, 0},
    };
    static const D3D12_INPUT_ELEMENT_DESC inst_elems[] = {
        {"POSITION", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 0, D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA, 0},
        {"OFFSET", 0, DXGI_FORMAT_R32G32_FLOAT, 1, 0, D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA, 1},
        {"COLOR", 0, DXGI_FORMAT_R8G8B8A8_UNORM, 1, 8, D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA, 1},
    };
    ID3D12PipelineState *pso_list = make_pso(dev, root, vs_list, ps, list_elems, 2, false, "pipeline (list)");
    ID3D12PipelineState *pso_strip = make_pso(dev, root, vs_list, ps, list_elems, 2, true, "pipeline (strip cut)");
    ID3D12PipelineState *pso_inst = make_pso(dev, root, vs_inst, ps, inst_elems, 3, false, "pipeline (instances)");
    if (!pso_list || !pso_strip || !pso_inst)
        return false;

    /* The command signatures. */
    D3D12_INDIRECT_ARGUMENT_DESC a[4];
    ID3D12CommandSignature *sig[5];
    memset(a, 0, sizeof(a));
    a[0].Type = D3D12_INDIRECT_ARGUMENT_TYPE_VERTEX_BUFFER_VIEW;
    a[0].VertexBuffer.Slot = 0;
    a[1].Type = D3D12_INDIRECT_ARGUMENT_TYPE_INDEX_BUFFER_VIEW;
    a[2].Type = D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT;
    a[2].Constant.RootParameterIndex = 0;
    a[2].Constant.DestOffsetIn32BitValues = 0;
    a[2].Constant.Num32BitValuesToSet = 4;
    a[3].Type = D3D12_INDIRECT_ARGUMENT_TYPE_DRAW_INDEXED;
    sig[0] = make_signature(dev, root, a, 4, s.stride[0], "command signature A");

    memset(a, 0, sizeof(a));
    a[0].Type = D3D12_INDIRECT_ARGUMENT_TYPE_VERTEX_BUFFER_VIEW;
    a[0].VertexBuffer.Slot = 1;
    a[1].Type = D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT;
    a[1].Constant.RootParameterIndex = 0;
    a[1].Constant.Num32BitValuesToSet = 4;
    a[2].Type = D3D12_INDIRECT_ARGUMENT_TYPE_DRAW;
    sig[1] = make_signature(dev, root, a, 3, s.stride[1], "command signature B");

    memset(a, 0, sizeof(a));
    a[0].Type = D3D12_INDIRECT_ARGUMENT_TYPE_VERTEX_BUFFER_VIEW;
    a[0].VertexBuffer.Slot = 0;
    a[1].Type = D3D12_INDIRECT_ARGUMENT_TYPE_INDEX_BUFFER_VIEW;
    a[2].Type = D3D12_INDIRECT_ARGUMENT_TYPE_DRAW_INDEXED;
    sig[2] = make_signature(dev, NULL, a, 3, s.stride[2], "command signature C");

    memset(a, 0, sizeof(a));
    a[0].Type = D3D12_INDIRECT_ARGUMENT_TYPE_VERTEX_BUFFER_VIEW;
    a[0].VertexBuffer.Slot = 0;
    a[1].Type = D3D12_INDIRECT_ARGUMENT_TYPE_DRAW;
    sig[3] = make_signature(dev, NULL, a, 2, s.stride[3], "command signature D");

    memset(a, 0, sizeof(a));
    a[0].Type = D3D12_INDIRECT_ARGUMENT_TYPE_INDEX_BUFFER_VIEW;
    a[1].Type = D3D12_INDIRECT_ARGUMENT_TYPE_DRAW_INDEXED;
    sig[4] = make_signature(dev, NULL, a, 2, s.stride[4], "command signature E");
    for (int i = 0; i < 5; i++)
        if (!sig[i])
            return false;

    /* Geometry: geo[0] stays in the upload heap, geo[1] goes to a default heap. */
    ID3D12Resource *geo0 = make_buffer(dev, D3D12_HEAP_TYPE_UPLOAD, s.geo[0].size(),
            D3D12_RESOURCE_STATE_GENERIC_READ, "geometry (upload)");
    ID3D12Resource *geo1_up = make_buffer(dev, D3D12_HEAP_TYPE_UPLOAD, s.geo[1].size(),
            D3D12_RESOURCE_STATE_GENERIC_READ, "geometry staging");
    ID3D12Resource *geo1 = make_buffer(dev, D3D12_HEAP_TYPE_DEFAULT, s.geo[1].size(),
            D3D12_RESOURCE_STATE_COPY_DEST, "geometry (default)");
    ID3D12Resource *args_up = make_buffer(dev, D3D12_HEAP_TYPE_UPLOAD, s.args.size(),
            D3D12_RESOURCE_STATE_GENERIC_READ, "argument staging");
    ID3D12Resource *args = make_buffer(dev, D3D12_HEAP_TYPE_DEFAULT, s.args.size(),
            D3D12_RESOURCE_STATE_COPY_DEST, "arguments");
    /* What the signature must replace: a view of bytes that draw nothing. */
    Bytes bogus_bytes(4096, 0);
    ID3D12Resource *bogus = make_buffer(dev, D3D12_HEAP_TYPE_UPLOAD, bogus_bytes.size(),
            D3D12_RESOURCE_STATE_GENERIC_READ, "bogus buffer");
    if (!geo0 || !geo1_up || !geo1 || !args_up || !args || !bogus)
        return false;

    D3D12_GPU_VIRTUAL_ADDRESS va[2] = {geo0->GetGPUVirtualAddress(), geo1->GetGPUVirtualAddress()};
    patch_views(s, va);
    if (!upload(geo0, s.geo[0]) || !upload(geo1_up, s.geo[1]) || !upload(args_up, s.args) ||
            !upload(bogus, bogus_bytes))
        return false;

    /* Render target and readback. */
    D3D12_HEAP_PROPERTIES hp = {};
    D3D12_RESOURCE_DESC td = {};
    D3D12_CLEAR_VALUE cv = {};
    ID3D12Resource *target = NULL;
    ID3D12DescriptorHeap *rtv_heap = NULL;
    D3D12_DESCRIPTOR_HEAP_DESC hd = {};
    const float clear[4] = {CLEAR.r / 255.0f, CLEAR.g / 255.0f, CLEAR.b / 255.0f, 1.0f};
    hp.Type = D3D12_HEAP_TYPE_DEFAULT;
    td.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    td.Width = RT;
    td.Height = RT;
    td.DepthOrArraySize = 1;
    td.MipLevels = 1;
    td.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    td.SampleDesc.Count = 1;
    td.Flags = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;
    cv.Format = td.Format;
    memcpy(cv.Color, clear, sizeof(clear));
    hd.Type = D3D12_DESCRIPTOR_HEAP_TYPE_RTV;
    hd.NumDescriptors = 1;
    if (failed(dev->CreateCommittedResource(&hp, D3D12_HEAP_FLAG_NONE, &td, D3D12_RESOURCE_STATE_RENDER_TARGET, &cv,
            __uuidof(ID3D12Resource), (void **)&target), "render target") ||
            failed(dev->CreateDescriptorHeap(&hd, __uuidof(ID3D12DescriptorHeap), (void **)&rtv_heap),
                    "CreateDescriptorHeap"))
        return false;
    D3D12_CPU_DESCRIPTOR_HANDLE rtv = rtv_heap->GetCPUDescriptorHandleForHeapStart();
    dev->CreateRenderTargetView(target, NULL, rtv);

    D3D12_PLACED_SUBRESOURCE_FOOTPRINT fp = {};
    UINT64 readback_size = 0;
    dev->GetCopyableFootprints(&td, 0, 1, 0, &fp, NULL, NULL, &readback_size);
    ID3D12Resource *readback = make_buffer(dev, D3D12_HEAP_TYPE_READBACK, readback_size,
            D3D12_RESOURCE_STATE_COPY_DEST, "readback buffer");
    if (!readback)
        return false;

    /* ---- record ---- */
    cl->CopyBufferRegion(geo1, 0, geo1_up, 0, s.geo[1].size());
    cl->CopyBufferRegion(args, 0, args_up, 0, s.args.size());
    transition(cl, geo1, D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER | D3D12_RESOURCE_STATE_INDEX_BUFFER);
    transition(cl, args, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT);

    D3D12_VIEWPORT vp = {0, 0, RT, RT, 0, 1};
    D3D12_RECT sc = {0, 0, RT, RT};
    cl->OMSetRenderTargets(1, &rtv, FALSE, NULL);
    cl->RSSetViewports(1, &vp);
    cl->RSSetScissorRects(1, &sc);
    cl->ClearRenderTargetView(rtv, clear, 0, NULL);
    cl->SetGraphicsRootSignature(root);

    D3D12_GPU_VIRTUAL_ADDRESS g0 = va[0];
    D3D12_VERTEX_BUFFER_VIEW bogus_vb = {bogus->GetGPUVirtualAddress(), (UINT)bogus_bytes.size(), 8};
    D3D12_INDEX_BUFFER_VIEW bogus_ib = {bogus->GetGPUVirtualAddress(), (UINT)bogus_bytes.size(), DXGI_FORMAT_R16_UINT};
    D3D12_VERTEX_BUFFER_VIEW vb;
    /* What a signature with root constants must replace. */
    Constants wrong = {{0, 0, 0}, 9999, 0, 0};
    Constants k;

    /* A */
    cl->SetPipelineState(pso_list);
    cl->IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    k = wrong;
    k.vid_limit = 4;
    k.iid_limit = 1;
    cl->SetGraphicsRoot32BitConstants(0, 6, &k, 0);
    cl->IASetVertexBuffers(0, 1, &bogus_vb);
    vb = {g0 + s.a_colors, (3 + 5 * 8 + 4) * 4, 4};
    cl->IASetVertexBuffers(1, 1, &vb);
    cl->IASetIndexBuffer(&bogus_ib);
    cl->ExecuteIndirect(sig[0], s.sequences[0], args, s.args_offset[0], args, s.count_offset);

    /* B */
    cl->SetPipelineState(pso_inst);
    cl->IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
    k = wrong;
    k.vid_limit = 4;
    k.iid_limit = 2;
    cl->SetGraphicsRoot32BitConstants(0, 6, &k, 0);
    vb = {g0 + s.b_positions, 6 * 8, 8};
    cl->IASetVertexBuffers(0, 1, &vb);
    bogus_vb.StrideInBytes = 12;
    cl->IASetVertexBuffers(1, 1, &bogus_vb);
    cl->ExecuteIndirect(sig[1], s.sequences[1], args, s.args_offset[1], NULL, 0);

    /* C */
    cl->SetPipelineState(pso_strip);
    cl->IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
    k = {{1, 1, 1}, s.c_vid_base, 8, 1};
    cl->SetGraphicsRoot32BitConstants(0, 6, &k, 0);
    bogus_vb.StrideInBytes = 8;
    cl->IASetVertexBuffers(0, 1, &bogus_vb);
    vb = {g0 + s.c_colors, 16 * 4, 4};
    cl->IASetVertexBuffers(1, 1, &vb);
    cl->IASetIndexBuffer(&bogus_ib);
    cl->ExecuteIndirect(sig[2], s.sequences[2], args, s.args_offset[2], NULL, 0);

    /* D */
    cl->SetPipelineState(pso_list);
    cl->IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    k = {{1, 1, 1}, 0, 6, 1};
    cl->SetGraphicsRoot32BitConstants(0, 6, &k, 0);
    cl->IASetVertexBuffers(0, 1, &bogus_vb);
    vb = {g0 + s.d_colors, 17 * 4, 4};
    cl->IASetVertexBuffers(1, 1, &vb);
    cl->ExecuteIndirect(sig[3], s.sequences[3], args, s.args_offset[3], NULL, 0);

    /* E */
    k = {{1, 1, 1}, 0, 4, 1};
    cl->SetGraphicsRoot32BitConstants(0, 6, &k, 0);
    vb = {g0 + s.e_positions, 8 * 8, 8};
    cl->IASetVertexBuffers(0, 1, &vb);
    vb = {g0 + s.e_colors, 8 * 4, 4};
    cl->IASetVertexBuffers(1, 1, &vb);
    cl->IASetIndexBuffer(&bogus_ib);
    cl->ExecuteIndirect(sig[4], s.sequences[4], args, s.args_offset[4], NULL, 0);

    transition(cl, target, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE);
    D3D12_TEXTURE_COPY_LOCATION src = {}, dst = {};
    src.pResource = target;
    src.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
    dst.pResource = readback;
    dst.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
    dst.PlacedFootprint = fp;
    cl->CopyTextureRegion(&dst, 0, 0, 0, &src, NULL);
    if (failed(cl->Close(), "ID3D12GraphicsCommandList::Close"))
        return false;

    ID3D12CommandList *lists[] = {cl};
    queue->ExecuteCommandLists(1, lists);
    HANDLE event = CreateEventW(NULL, FALSE, FALSE, NULL);
    if (failed(queue->Signal(fence, 1), "Signal") ||
            failed(fence->SetEventOnCompletion(1, event), "SetEventOnCompletion"))
        return false;
    if (WaitForSingleObject(event, 120000) != WAIT_OBJECT_0)
    {
        error("the fence did not signal in 120 s");
        return false;
    }
    if (failed(dev->GetDeviceRemovedReason(), "GetDeviceRemovedReason"))
        return false;

    void *mapped = NULL;
    if (failed(readback->Map(0, NULL, &mapped), "Map(readback)"))
        return false;
    pixels.resize(RT * RT * 4);
    for (int y = 0; y < RT; y++)
        memcpy(&pixels[y * RT * 4], (const uint8_t *)mapped + fp.Offset + (size_t)y * fp.Footprint.RowPitch, RT * 4);
    readback->Unmap(0, NULL);

    /* The allocator reports dropped draws when it is reset: do it before the log is read. */
    alloc->Reset();
    return true;
}

/* ---- judging ------------------------------------------------------------ */

static bool near_rgb(const uint8_t *p, Rgb c)
{
    return abs(p[0] - c.r) <= TOLERANCE && abs(p[1] - c.g) <= TOLERANCE && abs(p[2] - c.b) <= TOLERANCE;
}

/* Cells whose centre is not the expected colour or whose corner is not the clear colour. */
static int wrong_cells(const std::vector<uint8_t> &px, std::string &list)
{
    Scene s;
    int wrong = 0;
    build_scene(s);
    for (int c = 0; c < CELLS; c++)
    {
        int x = (c % GRID) * CELL, y = (c / GRID) * CELL;
        const uint8_t *centre = &px[((y + CELL / 2) * RT + x + CELL / 2) * 4];
        const uint8_t *corner = &px[((y + 4) * RT + x + 4) * 4];
        if (!near_rgb(centre, s.expected[c]) || !near_rgb(corner, CLEAR))
        {
            char b[160];
            snprintf(b, sizeof(b), "%s{\"cell\": %d, \"expected\": [%d, %d, %d], \"centre\": [%d, %d, %d], "
                    "\"corner\": [%d, %d, %d]}", wrong ? ", " : "", c, s.expected[c].r, s.expected[c].g,
                    s.expected[c].b, centre[0], centre[1], centre[2], corner[0], corner[1], corner[2]);
            list += b;
            wrong++;
        }
    }
    return wrong;
}

static void write_bmp(const std::wstring &path, const std::vector<uint8_t> &px)
{
    BITMAPFILEHEADER fh = {};
    BITMAPINFOHEADER ih = {};
    std::vector<uint8_t> bgr(RT * RT * 3);
    FILE *f = _wfopen(path.c_str(), L"wb");
    if (!f)
        return;
    for (int y = 0; y < RT; y++)
        for (int x = 0; x < RT; x++)
        {
            const uint8_t *p = &px[((RT - 1 - y) * RT + x) * 4];
            uint8_t *q = &bgr[(y * RT + x) * 3];
            q[0] = p[2];
            q[1] = p[1];
            q[2] = p[0];
        }
    fh.bfType = 0x4d42;
    fh.bfOffBits = sizeof(fh) + sizeof(ih);
    fh.bfSize = fh.bfOffBits + (DWORD)bgr.size();
    ih.biSize = sizeof(ih);
    ih.biWidth = RT;
    ih.biHeight = RT;
    ih.biPlanes = 1;
    ih.biBitCount = 24;
    fwrite(&fh, sizeof(fh), 1, f);
    fwrite(&ih, sizeof(ih), 1, f);
    fwrite(bgr.data(), 1, bgr.size(), f);
    fclose(f);
}

static bool read_file(const std::wstring &path, std::vector<uint8_t> &data)
{
    FILE *f = _wfopen(path.c_str(), L"rb");
    uint8_t buf[65536];
    size_t n;
    if (!f)
        return false;
    data.clear();
    while ((n = fread(buf, 1, sizeof(buf), f)) > 0)
        data.insert(data.end(), buf, buf + n);
    fclose(f);
    return true;
}

static std::string json_string(const std::string &s)
{
    std::string o;
    for (char c : s)
    {
        if (c == '"' || c == '\\')
            o += '\\';
        if ((unsigned char)c >= 0x20)
            o += c;
    }
    return o;
}

static bool file_exists(const std::wstring &path)
{
    return GetFileAttributesW(path.c_str()) != INVALID_FILE_ATTRIBUTES;
}

/* The child: draw on WARP and write the pixels, then the adapter name. */
static int child_main(const wchar_t *out)
{
    std::vector<uint8_t> px;
    std::string name;
    FILE *f;
    if (!draw_scene(true, px, name))
    {
        fprintf(stderr, "[%s]\n", g_errors.c_str());
        f = _wfopen((std::wstring(out) + L".err").c_str(), L"wb");
        if (f)
        {
            fwrite(g_errors.data(), 1, g_errors.size(), f);
            fclose(f);
        }
        return 2;
    }
    f = _wfopen(out, L"wb");
    if (!f)
        return 2;
    fwrite(px.data(), 1, px.size(), f);
    fwrite(name.data(), 1, name.size(), f);
    fclose(f);
    return 0;
}

/* Run this program again for WARP. Where the package replaced d3d12.dll, the
 * child runs from a directory with Microsoft's d3d12.dll and its core. */
static bool run_warp_child(const std::wstring &raw, std::string &how)
{
    wchar_t self[MAX_PATH], sys[MAX_PATH], tmp[MAX_PATH];
    std::wstring exe, dir, native, core;
    PROCESS_INFORMATION pi = {};
    STARTUPINFOW si = {};
    DWORD code = 1;

    GetModuleFileNameW(NULL, self, MAX_PATH);
    GetSystemDirectoryW(sys, MAX_PATH);
    GetTempPathW(MAX_PATH, tmp);
    exe = self;
    native = std::wstring(sys) + L"\\vmlab-win3d\\d3d12-native.dll";
    core = std::wstring(sys) + L"\\D3D12Core.dll";
    how = "the system d3d12.dll";

    if (file_exists(native))
    {
        wchar_t sub[64];
        swprintf(sub, 64, L"ei_views_warp_%lu", (unsigned long)GetCurrentProcessId());
        dir = std::wstring(tmp) + sub;
        CreateDirectoryW(dir.c_str(), NULL);
        exe = dir + L"\\ei_views_probe.exe";
        if (!CopyFileW(self, exe.c_str(), FALSE) || !CopyFileW(native.c_str(), (dir + L"\\d3d12.dll").c_str(), FALSE) ||
                !CopyFileW(core.c_str(), (dir + L"\\D3D12Core.dll").c_str(), FALSE))
        {
            error("could not stage the WARP run in the temporary directory: error %lu", (unsigned long)GetLastError());
            return false;
        }
        how = "vmlab-win3d\\\\d3d12-native.dll and D3D12Core.dll, copied beside the child";
    }

    std::wstring cmd = L"\"" + exe + L"\" --child-warp \"" + raw + L"\"";
    si.cb = sizeof(si);
    if (!CreateProcessW(exe.c_str(), &cmd[0], NULL, NULL, FALSE, 0, NULL, dir.empty() ? NULL : dir.c_str(), &si, &pi))
    {
        error("CreateProcess for the WARP run: error %lu", (unsigned long)GetLastError());
        return false;
    }
    if (WaitForSingleObject(pi.hProcess, 300000) != WAIT_OBJECT_0)
    {
        TerminateProcess(pi.hProcess, 124);
        error("the WARP run did not end in 300 s");
    }
    GetExitCodeProcess(pi.hProcess, &code);
    CloseHandle(pi.hProcess);
    CloseHandle(pi.hThread);
    if (!dir.empty())
    {
        DeleteFileW(exe.c_str());
        DeleteFileW((dir + L"\\d3d12.dll").c_str());
        DeleteFileW((dir + L"\\D3D12Core.dll").c_str());
        RemoveDirectoryW(dir.c_str());
    }
    if (code != 0)
    {
        std::vector<uint8_t> err;
        if (read_file(raw + L".err", err))
        {
            if (!g_errors.empty())
                g_errors += ", ";
            g_errors += "\"WARP run:\", " + std::string(err.begin(), err.end());
            DeleteFileW((raw + L".err").c_str());
        }
        else
        {
            error("the WARP run exited with %lu", (unsigned long)code);
        }
        return false;
    }
    return true;
}

int wmain(int argc, wchar_t **argv)
{
    std::wstring out = L"ei_views_probe.json";
    for (int i = 1; i < argc; i++)
    {
        if (!wcscmp(argv[i], L"--child-warp") && i + 1 < argc)
            return child_main(argv[i + 1]);
        else if (!wcscmp(argv[i], L"--out") && i + 1 < argc)
            out = argv[++i];
        else if (!wcscmp(argv[i], L"--seconds") && i + 1 < argc)
            i++;
        else
        {
            fprintf(stderr, "usage: ei_views_probe.exe [--out FILE.json] [--seconds N]\n");
            return 2;
        }
    }

    std::wstring base = out.size() > 5 && !_wcsicmp(out.c_str() + out.size() - 5, L".json") ?
            out.substr(0, out.size() - 5) : out;
    std::wstring log = base + L".vkd3d.log", raw = base + L".warp.raw";
    std::vector<uint8_t> gpu, warp, text;
    std::string gpu_name, warp_name, warp_how, gpu_wrong, warp_wrong;
    int gpu_wrong_cells = -1, warp_wrong_cells = -1, differ = -1, emulated = 0, dropped = 0;
    bool drew = false;

    /* vkd3d-proton reads these when its core is loaded, at D3D12CreateDevice. */
    DeleteFileW(log.c_str());
    SetEnvironmentVariableW(L"VKD3D_LOG_FILE", log.c_str());

    if (draw_scene(false, gpu, gpu_name))
    {
        write_bmp(base + L".gpu.bmp", gpu);
        gpu_wrong_cells = wrong_cells(gpu, gpu_wrong);
        DeleteFileW(raw.c_str());
        if (run_warp_child(raw, warp_how) && read_file(raw, warp) && warp.size() >= RT * RT * 4)
        {
            warp_name.assign(warp.begin() + RT * RT * 4, warp.end());
            warp.resize(RT * RT * 4);
            write_bmp(base + L".warp.bmp", warp);
            warp_wrong_cells = wrong_cells(warp, warp_wrong);
            differ = 0;
            for (size_t i = 0; i < gpu.size(); i += 4)
                if (abs(gpu[i] - warp[i]) > TOLERANCE || abs(gpu[i + 1] - warp[i + 1]) > TOLERANCE ||
                        abs(gpu[i + 2] - warp[i + 2]) > TOLERANCE)
                    differ++;
            drew = true;
        }
        DeleteFileW(raw.c_str());
    }

    if (read_file(log, text))
    {
        std::string t(text.begin(), text.end());
        for (size_t at = 0; (at = t.find("fetching vertices", at)) != std::string::npos; at++)
            emulated++;
        for (size_t at = 0; (at = t.find("dropped draws", at)) != std::string::npos; at++)
            dropped++;
    }

    bool ok = drew && !gpu_wrong_cells && !warp_wrong_cells && !differ && !dropped;
    int code = !drew ? 2 : ok ? 0 : 1;
    FILE *f = _wfopen(out.c_str(), L"wb");
    if (f)
    {
        fprintf(f, "{\n  \"schema\": 1,\n  \"probe\": \"d3d12-ei-views\",\n  \"arch\": \"%s\",\n",
                sizeof(void *) == 8 ? "x64" : "x86");
        fprintf(f, "  \"result\": \"%s\",\n  \"exit_code\": %d,\n", !drew ? "error" : ok ? "pass" : "fail", code);
        fprintf(f, "  \"adapter\": \"%s\",\n  \"warp_adapter\": \"%s\",\n  \"warp_d3d12\": \"%s\",\n",
                json_string(gpu_name).c_str(), json_string(warp_name).c_str(), warp_how.c_str());
        fprintf(f, "  \"cells\": %d,\n  \"pixels\": %d,\n  \"tolerance\": %d,\n", CELLS, RT * RT, TOLERANCE);
        fprintf(f, "  \"wrong_cells\": %d,\n  \"wrong_cells_warp\": %d,\n  \"pixels_differ_from_warp\": %d,\n",
                gpu_wrong_cells, warp_wrong_cells, differ);
        fprintf(f, "  \"emulated_signatures\": %d,\n  \"dropped_draw_reports\": %d,\n", emulated, dropped);
        fprintf(f, "  \"wrong\": [%s],\n  \"wrong_warp\": [%s],\n  \"errors\": [%s]\n}\n", gpu_wrong.c_str(),
                warp_wrong.c_str(), g_errors.c_str());
        fclose(f);
    }
    printf("%s: %d wrong cells, %d on WARP, %d of %d pixels differ from WARP, %d emulated signatures%s%s\n",
            !drew ? "error" : ok ? "pass" : "fail", gpu_wrong_cells, warp_wrong_cells, differ, RT * RT, emulated,
            g_errors.empty() ? "" : "; ", g_errors.c_str());
    return code;
}
