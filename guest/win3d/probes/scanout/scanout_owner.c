/* A fullscreen exclusive Direct3D 9 program that shows one colour (magenta,
 * as scanout_probe.exe). Windows gives such a program the scanout, and the
 * user-mode driver shows its frames with the VIOGPU_RES_SET_SCANOUT_BLOB
 * escape or a present. The check of card #755 must go on serving it:
 * tests/kmd/scanout-owner.sh takes a screenshot from the host meanwhile.
 * It must run in the session of the console. */
#define WIN32_LEAN_AND_MEAN
#define COBJMACROS
#include <windows.h>
#include <d3d9.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <wchar.h>

int wmain(int argc, wchar_t **argv)
{
    const wchar_t *out = NULL;
    unsigned seconds = 12;
    for (int i = 1; i < argc; ++i) {
        if (!wcscmp(argv[i], L"--out") && i + 1 < argc) out = argv[++i];
        else if (!wcscmp(argv[i], L"--seconds") && i + 1 < argc) seconds = (unsigned)wcstoul(argv[++i], NULL, 10);
    }
    const char *stage = "window";
    HRESULT hr = E_FAIL, first_failure = S_OK;
    unsigned presented = 0, failed = 0;
    DWORD session = 0;
    ProcessIdToSessionId(GetCurrentProcessId(), &session);
    IDirect3D9 *d3d = NULL;
    IDirect3DDevice9 *device = NULL;

    WNDCLASSW wc = {0};
    wc.lpfnWndProc = DefWindowProcW;
    wc.hInstance = GetModuleHandleW(NULL);
    wc.lpszClassName = L"c755-scanout-owner";
    RegisterClassW(&wc);
    HWND window = CreateWindowExW(WS_EX_TOPMOST, wc.lpszClassName, L"scanout owner", WS_POPUP | WS_VISIBLE,
                                  0, 0, 1024, 768, NULL, NULL, wc.hInstance, NULL);
    if (!window) { hr = HRESULT_FROM_WIN32(GetLastError()); goto done; }

    stage = "create_d3d9";
    d3d = Direct3DCreate9(D3D_SDK_VERSION);
    if (!d3d) goto done;

    D3DPRESENT_PARAMETERS pp;
    ZeroMemory(&pp, sizeof(pp));
    pp.BackBufferWidth = 1024;
    pp.BackBufferHeight = 768;
    pp.BackBufferFormat = D3DFMT_X8R8G8B8;
    pp.BackBufferCount = 1;
    pp.SwapEffect = D3DSWAPEFFECT_FLIP;
    pp.hDeviceWindow = window;
    pp.Windowed = FALSE;
    pp.PresentationInterval = D3DPRESENT_INTERVAL_ONE;
    stage = "create_device";
    hr = IDirect3D9_CreateDevice(d3d, D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, window,
                                 D3DCREATE_HARDWARE_VERTEXPROCESSING, &pp, &device);
    if (FAILED(hr)) goto done;

    stage = "present";
    const ULONGLONG end = GetTickCount64() + (ULONGLONG)seconds * 1000;
    do {
        MSG msg;
        while (PeekMessageW(&msg, NULL, 0, 0, PM_REMOVE)) DispatchMessageW(&msg);
        HRESULT frame = IDirect3DDevice9_Clear(device, 0, NULL, D3DCLEAR_TARGET, D3DCOLOR_XRGB(255, 0, 255), 1.0f, 0);
        if (SUCCEEDED(frame)) frame = IDirect3DDevice9_Present(device, NULL, NULL, NULL, NULL);
        if (SUCCEEDED(frame)) ++presented;
        else {
            if (!failed) first_failure = frame;
            ++failed;
            Sleep(50);
        }
    } while (GetTickCount64() < end);
    stage = "done";
    hr = S_OK;

done:
    if (device) IDirect3DDevice9_Release(device);
    if (d3d) IDirect3D9_Release(d3d);
    {
        const int pass = hr == S_OK && presented && !failed;
        FILE *files[2] = {stdout, out ? _wfopen(out, L"wb") : NULL};
        for (int i = 0; i < 2; ++i) {
            if (!files[i]) continue;
            fprintf(files[i], "{\"schema\":1,\"probe\":\"scanout-owner\",\"stage\":\"%s\",\"pass\":%s,\"hresult\":\"0x%08lx\","
                    "\"presented\":%u,\"failed\":%u,\"first_failure\":\"0x%08lx\",\"seconds\":%u,\"session\":%lu,"
                    "\"width\":1024,\"height\":768,\"colour_bgra\":\"ff00ffff\"}\n",
                    stage, pass ? "true" : "false", (unsigned long)hr, presented, failed,
                    (unsigned long)first_failure, seconds, (unsigned long)session);
            if (i) fclose(files[i]);
        }
        return pass ? 0 : 2;
    }
}
