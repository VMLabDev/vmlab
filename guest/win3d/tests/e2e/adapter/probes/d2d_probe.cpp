// P10, rendering is correct: the Direct2D coverage probe.
//
// Direct2D on feature level 11.1 fills antialiased shapes with
// target-independent rasterization: triangle fans drawn with a forced sample
// count of 16 whose coverage is XORed into a mask. Card #138 found pale gaps
// on the edges that two triangles of a fan share, in other places at every
// start (Word's ribbon).
//
// The probe fills the same scene of rounded rectangles, ellipses and path
// geometries (800x600, one colour on white) --runs times (3), each on a new
// Direct3D 11 device of the default adapter, and once on WARP. It reports
//   runs_differ   pixels that are not the same in every hardware run
//   holes         pixels WARP filled completely that a hardware run did not
//   spills        pixels WARP left white that a hardware run did not
//   edge_diffs    other pixels that differ from WARP by more than 64
// A gap of #138 is a hole, and a scene with gaps differs between runs.
// --bmp FILE keeps the first hardware picture.
#include "adapter.h"
#include <d2d1_1.h>

static const int W = 800, H = 600;
static const unsigned FILL = 0xff1a3c8c; // opaque, R 0x1a G 0x3c B 0x8c

static unsigned rng_state = 148;
static float rnd(float lo, float hi)
{
    rng_state = rng_state * 1664525u + 1013904223u;
    return lo + (hi - lo) * (float)((rng_state >> 8) & 0xffff) / 65535.0f;
}

static void scene(ID2D1Factory1 *factory, ID2D1DeviceContext *dc, ID2D1SolidColorBrush *brush)
{
    rng_state = 148;
    // Rounded rectangles and ellipses, as geometries: the fills Word makes.
    for (int i = 0; i < 24; i++) {
        float x = rnd(8, W - 200), y = rnd(8, H - 120), w = rnd(40, 180), h = rnd(16, 100);
        if (i & 1) {
            ID2D1RoundedRectangleGeometry *geo = NULL;
            D2D1_ROUNDED_RECT rr = {{x, y, x + w, y + h}, rnd(3, 14), rnd(3, 14)};
            if (SUCCEEDED(factory->CreateRoundedRectangleGeometry(&rr, &geo))) {
                dc->FillGeometry(geo, brush, NULL);
                geo->Release();
            }
        } else {
            ID2D1EllipseGeometry *geo = NULL;
            D2D1_ELLIPSE e = {{x + w / 2, y + h / 2}, w / 2, h / 2};
            if (SUCCEEDED(factory->CreateEllipseGeometry(&e, &geo))) {
                dc->FillGeometry(geo, brush, NULL);
                geo->Release();
            }
        }
    }
    // Path geometries: stars (many thin triangles that meet in one point)
    // and closed Bezier shapes.
    for (int i = 0; i < 16; i++) {
        ID2D1PathGeometry *geo = NULL;
        ID2D1GeometrySink *sink = NULL;
        if (FAILED(factory->CreatePathGeometry(&geo)))
            continue;
        if (FAILED(geo->Open(&sink))) {
            geo->Release();
            continue;
        }
        float cx = rnd(80, W - 80), cy = rnd(80, H - 80), rad = rnd(30, 75);
        sink->SetFillMode(D2D1_FILL_MODE_WINDING);
        if (i & 1) {
            int points = 5 + (i % 7);
            for (int k = 0; k < points * 2; k++) {
                float a = 3.14159265f * (float)k / (float)points, rr = (k & 1) ? rad * 0.45f : rad;
                D2D1_POINT_2F p = {cx + rr * __builtin_cosf(a), cy + rr * __builtin_sinf(a)};
                if (k == 0)
                    sink->BeginFigure(p, D2D1_FIGURE_BEGIN_FILLED);
                else
                    sink->AddLine(p);
            }
        } else {
            D2D1_POINT_2F p0 = {cx - rad, cy};
            sink->BeginFigure(p0, D2D1_FIGURE_BEGIN_FILLED);
            D2D1_BEZIER_SEGMENT b1 = {{cx - rad, cy - rad * 1.2f}, {cx + rad, cy - rad * 0.6f}, {cx + rad, cy}};
            D2D1_BEZIER_SEGMENT b2 = {{cx + rad, cy + rad * 1.3f}, {cx - rad * 0.3f, cy + rad * 0.4f}, {cx - rad, cy}};
            sink->AddBezier(&b1);
            sink->AddBezier(&b2);
        }
        sink->EndFigure(D2D1_FIGURE_END_CLOSED);
        sink->Close();
        sink->Release();
        dc->FillGeometry(geo, brush, NULL);
        geo->Release();
    }
}

// One picture of the scene on a new device. BGRA, W*H*4 bytes.
static bool render(Report &r, bool warp, const char *key, std::vector<unsigned char> &pixels)
{
    Gpu g;
    if (!gpu_open(r, g, warp, key))
        return false;
    bool ok = false;
    ID2D1Factory1 *factory = NULL;
    IDXGIDevice *dxgi = NULL;
    ID2D1Device *device = NULL;
    ID2D1DeviceContext *dc = NULL;
    ID2D1Bitmap1 *target = NULL, *readable = NULL;
    ID2D1SolidColorBrush *brush = NULL;
    D2D1_FACTORY_OPTIONS fo = {};
    D2D1_BITMAP_PROPERTIES1 bp = {};
    bp.pixelFormat.format = DXGI_FORMAT_B8G8R8A8_UNORM;
    bp.pixelFormat.alphaMode = D2D1_ALPHA_MODE_PREMULTIPLIED;
    bp.dpiX = bp.dpiY = 96;
    D2D1_SIZE_U size = {(UINT32)W, (UINT32)H};
    D2D1_MAPPED_RECT map = {};
    D2D1_POINT_2U origin = {0, 0};
    HRESULT hr;
    do {
        if (r.failed(D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, __uuidof(ID2D1Factory1), &fo,
                                       (void **)&factory),
                     "D2D1CreateFactory") ||
            r.failed(g.dev->QueryInterface(__uuidof(IDXGIDevice), (void **)&dxgi), "QueryInterface(IDXGIDevice)") ||
            r.failed(factory->CreateDevice(dxgi, &device), "ID2D1Factory1::CreateDevice") ||
            r.failed(device->CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE, &dc), "CreateDeviceContext"))
            break;
        bp.bitmapOptions = D2D1_BITMAP_OPTIONS_TARGET;
        if (r.failed(dc->CreateBitmap(size, NULL, 0, &bp, &target), "CreateBitmap(target)"))
            break;
        bp.bitmapOptions = D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW;
        if (r.failed(dc->CreateBitmap(size, NULL, 0, &bp, &readable), "CreateBitmap(cpu read)"))
            break;
        D2D1_COLOR_F fill = {0x1a / 255.0f, 0x3c / 255.0f, 0x8c / 255.0f, 1.0f};
        if (r.failed(dc->CreateSolidColorBrush(&fill, NULL, &brush), "CreateSolidColorBrush"))
            break;
        dc->SetTarget(target);
        dc->SetDpi(96, 96);
        dc->SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        dc->BeginDraw();
        D2D1_COLOR_F white = {1, 1, 1, 1};
        dc->Clear(&white);
        scene(factory, dc, brush);
        hr = dc->EndDraw();
        if (r.failed(hr, "EndDraw"))
            break;
        dc->SetTarget(NULL);
        if (r.failed(readable->CopyFromBitmap(&origin, target, NULL), "CopyFromBitmap") ||
            r.failed(readable->Map(D2D1_MAP_OPTIONS_READ, &map), "ID2D1Bitmap1::Map"))
            break;
        pixels.resize((size_t)W * H * 4);
        for (int y = 0; y < H; y++)
            memcpy(&pixels[(size_t)y * W * 4], map.bits + (size_t)y * map.pitch, (size_t)W * 4);
        readable->Unmap();
        ok = true;
    } while (0);
    if (brush)
        brush->Release();
    if (readable)
        readable->Release();
    if (target)
        target->Release();
    if (dc)
        dc->Release();
    if (device)
        device->Release();
    if (dxgi)
        dxgi->Release();
    if (factory)
        factory->Release();
    gpu_close(g);
    return ok;
}

static int channel_diff(const unsigned char *a, const unsigned char *b)
{
    int d = 0;
    for (int c = 0; c < 3; c++) {
        int v = abs((int)a[c] - (int)b[c]);
        if (v > d)
            d = v;
    }
    return d;
}

static void write_bmp(const char *path, const std::vector<unsigned char> &px)
{
    FILE *f = fopen(path, "wb");
    if (!f)
        return;
    BITMAPFILEHEADER fh = {};
    BITMAPINFOHEADER ih = {};
    fh.bfType = 0x4d42;
    fh.bfOffBits = sizeof fh + sizeof ih;
    fh.bfSize = fh.bfOffBits + (DWORD)px.size();
    ih.biSize = sizeof ih;
    ih.biWidth = W;
    ih.biHeight = -H;
    ih.biPlanes = 1;
    ih.biBitCount = 32;
    fwrite(&fh, sizeof fh, 1, f);
    fwrite(&ih, sizeof ih, 1, f);
    fwrite(px.data(), 1, px.size(), f);
    fclose(f);
}

int main(int argc, char **argv)
{
    Report r("d2d");
    const char *out = arg_str(argc, argv, "--out", "d2d_probe.json");
    const char *bmp = arg_str(argc, argv, "--bmp", NULL);
    int runs = arg_int(argc, argv, "--runs", 3);
    if (runs < 1)
        runs = 1;
    if (runs > 8)
        runs = 8;

    std::vector<unsigned char> ref;
    std::vector<std::vector<unsigned char>> hw((size_t)runs);
    if (!render(r, true, "reference", ref))
        return r.finish(out);
    for (int i = 0; i < runs; i++)
        if (!render(r, false, i ? NULL : "adapter", hw[(size_t)i]))
            return r.finish(out);
    if (bmp)
        write_bmp(bmp, hw[0]);

    const unsigned char fill[3] = {(unsigned char)(FILL & 0xff), (unsigned char)((FILL >> 8) & 0xff),
                                   (unsigned char)((FILL >> 16) & 0xff)};
    const unsigned char white[3] = {255, 255, 255};
    unsigned long long runs_differ = 0, holes = 0, spills = 0, edge_diffs = 0, filled = 0;
    int first_hole_x = -1, first_hole_y = -1;
    for (int y = 0; y < H; y++)
        for (int x = 0; x < W; x++) {
            size_t o = ((size_t)y * W + x) * 4;
            const unsigned char *p = &ref[o];
            bool differ = false;
            int worst = 0;
            for (int i = 0; i < runs; i++) {
                if (i && memcmp(&hw[(size_t)i][o], &hw[0][o], 3))
                    differ = true;
                int d = channel_diff(&hw[(size_t)i][o], p);
                if (d > worst)
                    worst = d;
            }
            if (differ)
                runs_differ++;
            bool inside = !channel_diff(p, fill), outside = !channel_diff(p, white);
            if (inside)
                filled++;
            if (inside && worst > 24) {
                if (!holes) {
                    first_hole_x = x;
                    first_hole_y = y;
                }
                holes++;
            } else if (outside && worst > 24) {
                spills++;
            } else if (worst > 64) {
                edge_diffs++;
            }
        }
    r.integer("runs", runs);
    r.integer("width", W);
    r.integer("height", H);
    r.integer("filled_pixels", (long long)filled);
    r.integer("runs_differ", (long long)runs_differ);
    r.integer("holes", (long long)holes);
    r.integer("spills", (long long)spills);
    r.integer("edge_diffs", (long long)edge_diffs);
    if (holes) {
        char b[64];
        snprintf(b, sizeof b, "[%d, %d]", first_hole_x, first_hole_y);
        r.raw("first_hole", b);
    }
    return r.finish(out);
}
