/* Presents a blob allocation with a Blt to a window of the process, in a
 * Venus context of the viogpu3d kernel driver: card #756
 * (docs/reviews/win3d-driver.md F8), as whatever user runs it. Run it in the
 * desktop session, where the window has a surface. One case per run. For a
 * blob that exists on the host and has no mappable flag, a driver without
 * the fix accepts the present and bugchecks the guest with 0x119 when
 * Windows calls DxgkDdiPatch. Run it in a disposable VM. */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wchar.h>
#include <io.h>

/* Published d3dkmthk.h and d3dukmdt.h ABI subset. llvm-mingw does not ship
 * those SDK headers.
 * https://learn.microsoft.com/windows-hardware/drivers/ddi/d3dkmthk/ */
typedef struct {
    UINT handle;
    LUID luid;
    ULONG sources;
    BOOL precise_present_regions;
} adapter_info;

typedef struct {
    ULONG count;
    adapter_info *adapters;
} enum_adapters;

typedef struct {
    UINT handle;
    UINT type;
    void *data;
    UINT size;
} query_adapter;

typedef struct {
    UINT physical_index;
    UINT vendor_id;
    UINT device_id;
    UINT subvendor_id;
    UINT subsystem_id;
    UINT revision_id;
    UINT bus_type;
} query_device_ids;

typedef struct {
    union {
        UINT adapter;
        void *adapter_pointer;
    };
    UINT flags;
    UINT device;
    void *command_buffer;
    UINT command_buffer_size;
    void *allocation_list;
    UINT allocation_list_size;
    void *patch_location_list;
    UINT patch_location_list_size;
} create_device;

typedef struct { UINT device; } destroy_device;

typedef struct {
    UINT adapter;
    UINT device;
    UINT type;
    UINT flags;
    void *data;
    UINT size;
    UINT context;
} escape_args;

/* D3DKMT_CREATECONTEXT */
typedef struct {
    UINT device;
    UINT node_ordinal;
    UINT engine_affinity;
    UINT flags;
    void *private_data;
    UINT private_data_size;
    UINT client_hint;
    UINT context; /* out */
    void *command_buffer;
    UINT command_buffer_size;
    void *allocation_list;
    UINT allocation_list_size;
    void *patch_location_list;
    UINT patch_location_list_size;
    uint64_t command_buffer_va;
} create_context;

typedef struct { UINT context; } destroy_context;

/* D3DDDI_ALLOCATIONINFO2. Its first six members are D3DDDI_ALLOCATIONINFO,
 * so the kernel reads the same request whichever of the two it expects. */
typedef struct {
    UINT allocation; /* out */
    const void *system_mem;
    void *private_data;
    UINT private_data_size;
    UINT vidpn_source_id;
    UINT flags;
    uint64_t gpu_virtual_address;
    ULONG_PTR priority;
    ULONG_PTR reserved[5];
} allocation_info;

/* D3DKMT_CREATEALLOCATION */
typedef struct {
    UINT device;
    UINT resource;
    UINT global_share;
    const void *private_runtime_data;
    UINT private_runtime_data_size;
    const void *private_data;
    UINT private_data_size;
    UINT num_allocations;
    allocation_info *allocation_info;
    UINT flags;
    HANDLE private_runtime_resource_handle;
} create_allocation;

/* The start of D3DKMT_PRESENT, up to BroadcastContextCount. What follows
 * (the broadcast contexts, the present history token and the members
 * Windows added later) is zero: the request is a zeroed buffer larger than
 * any version of the structure. The request is the one of a windowed
 * Direct3D 9 present, as a trace of Microsoft-Windows-DxgKrnl shows it:
 * a window, a source, no destination, and the flags Blt, PresentCountValid
 * and RedirectedBlt. Windows then names the surface of the window as the
 * destination and calls DxgkDdiPresent. */
typedef struct {
    UINT context;
    HWND window;
    UINT vidpn_source_id;
    UINT source;
    UINT destination;
    UINT color;
    RECT dst_rect;
    RECT src_rect;
    UINT sub_rect_count;
    const RECT *src_sub_rects;
    UINT present_count;
    UINT flip_interval;
    UINT flags;
    ULONG broadcast_context_count;
} present_head;

typedef struct { UINT handle; } close_adapter;
typedef LONG (WINAPI *enum_fn)(const enum_adapters *);
typedef LONG (WINAPI *query_fn)(const query_adapter *);
typedef LONG (WINAPI *create_fn)(create_device *);
typedef LONG (WINAPI *destroy_fn)(const destroy_device *);
typedef LONG (WINAPI *escape_fn)(const escape_args *);
typedef LONG (WINAPI *create_context_fn)(create_context *);
typedef LONG (WINAPI *destroy_context_fn)(const destroy_context *);
typedef LONG (WINAPI *create_allocation_fn)(create_allocation *);
typedef LONG (WINAPI *present_fn)(void *);
typedef LONG (WINAPI *close_fn)(const close_adapter *);
/* dwmapi, export 100: has the compositor make the surface of a window on
 * the adapter that the caller names. d3d9.dll calls it before the first
 * windowed present: without it Windows answers the present with
 * STATUS_GRAPHICS_PRESENT_OCCLUDED. */
typedef HRESULT (WINAPI *dwm_surface_fn)(HWND, LUID, HMONITOR, DWORD, ULONG *, HANDLE *, UINT64 *);

/* viogpu/shared/viogpum.h of the driver source. VIOGPU_ADAPTERINFO is not
 * packed there: four bytes of padding follow the flags. */
typedef struct {
    uint64_t iam;
    uint32_t flags;
    uint64_t supported_capset_ids;
} viogpu_adapterinfo;

#pragma pack(push, 1)
typedef struct {
    uint16_t type;
    uint16_t data_length;
    /* The union of VIOGPU_ESCAPE: its largest member has 36 bytes. */
    union {
        uint32_t capset_id; /* VIOGPU_CTX_INIT_REQ */
        uint8_t largest[36];
    } u;
} viogpu_escape;

/* VIOGPU_SUBMIT_CMD_REQ. The command bytes follow it, and it follows the
 * whole VIOGPU_ESCAPE in the private data. */
typedef struct {
    uint32_t context; /* D3DKMT_HANDLE hContext, not read by the driver */
    uint32_t cmd_type;
    uint32_t cmd_size;
} viogpu_submit_cmd_req;

/* VIOGPU_CREATE_ALLOCATION_EXCHANGE: the private data of an allocation. */
typedef struct {
    struct {
        uint32_t target, format, bind, width, height, depth, array_size, last_level, nr_samples, flags;
    } options;
    uint64_t size;
    uint64_t blob_id;
    uint32_t blob_mem;
    uint32_t blob_flags;
    uint32_t stride;
    uint32_t scanout_offset;
} viogpu_allocation_exchange;
#pragma pack(pop)

_Static_assert(sizeof(adapter_info) == 20, "D3DKMT_ADAPTERINFO ABI");
_Static_assert(sizeof(query_device_ids) == 28, "D3DKMT_QUERY_DEVICE_IDS ABI");
_Static_assert(offsetof(escape_args, data) == 16, "D3DKMT_ESCAPE ABI");
_Static_assert(sizeof(create_device) == (sizeof(void *) == 8 ? 64 : 36), "D3DKMT_CREATEDEVICE ABI");
_Static_assert(sizeof(viogpu_adapterinfo) == 24 && offsetof(viogpu_adapterinfo, supported_capset_ids) == 16, "VIOGPU_ADAPTERINFO ABI");
_Static_assert(sizeof(viogpu_escape) == 40, "VIOGPU_ESCAPE ABI");
_Static_assert(sizeof(viogpu_allocation_exchange) == 72, "VIOGPU_CREATE_ALLOCATION_EXCHANGE ABI");
_Static_assert(sizeof(viogpu_submit_cmd_req) == 12, "VIOGPU_SUBMIT_CMD_REQ ABI");
#ifdef _WIN64
_Static_assert(sizeof(create_context) == 96 && offsetof(create_context, context) == 32, "D3DKMT_CREATECONTEXT ABI");
_Static_assert(sizeof(allocation_info) == 96 && offsetof(allocation_info, flags) == 32, "D3DDDI_ALLOCATIONINFO2 ABI");
_Static_assert(sizeof(create_allocation) == 72 && offsetof(create_allocation, allocation_info) == 48, "D3DKMT_CREATEALLOCATION ABI");
_Static_assert(offsetof(present_head, source) == 20 && offsetof(present_head, src_sub_rects) == 72 &&
               offsetof(present_head, flags) == 88, "D3DKMT_PRESENT ABI");
#endif

enum {
    QUERY_UMDRIVERPRIVATE = 0,
    QUERY_PHYSICAL_ADAPTER_DEVICE_IDS = 31,
    ESCAPE_DRIVERPRIVATE = 0,
    VIOGPU_CTX_INIT = 0x200,
    VIOGPU_SUBMIT_CMD = 0x300,
    VIOGPU_CMD_SUBMIT = 1,
    CAPSET_VENUS = 4,
    BLOB_MEM_HOST3D = 2,
    BLOB_FLAG_MAPPABLE = 1,
    BLOB_FLAG_SHAREABLE = 2,
    PIPE_TEXTURE_2D = 2,
    VIRGL_FORMAT_B8G8R8A8_UNORM = 1,
    /* D3DKMT_PRESENTFLAGS */
    PRESENT_BLT = 1u << 0,
    PRESENT_COUNT_VALID = 1u << 12,
    PRESENT_REDIRECTED_BLT = 1u << 16,
    IMAGE_WIDTH = 256,
    IMAGE_HEIGHT = 256,
};
#define VIOGPU_IAM 0x56696f475055ull
/* A blob id that no Venus context of this process made. */
#define UNKNOWN_BLOB_ID 0x0000756c62373536ull
/* The size of the device memory that the Venus commands allocate. */
#define MEMORY_SIZE (1024u * 1024u)

/* Where the blob of a case comes from. */
enum { BLOB_UNKNOWN, BLOB_SHM, BLOB_DEVICE_MEMORY };
/* What a fixed driver does with a case. */
enum { CREATE_FAILS, PRESENT_FAILS, PRESENT_WORKS };

typedef struct {
    const char *name;
    int blob, outcome;
    uint32_t blob_flags;
    const char *expect;
} test_case;

static const test_case cases[] = {
    {"unknown-blob", BLOB_UNKNOWN, CREATE_FAILS, BLOB_FLAG_MAPPABLE,
     "D3DKMTCreateAllocation fails: the host does not know the blob id, and the driver creates the blob "
     "when the allocation is opened (the same before the fix)"},
    {"mappable", BLOB_SHM, PRESENT_WORKS, BLOB_FLAG_MAPPABLE,
     "every present is accepted and the guest stays up: shared memory of the Venus context is a mappable "
     "blob (the same before the fix)"},
    {"not-mappable", BLOB_DEVICE_MEMORY, PRESENT_FAILS, 0,
     "every present fails: a blob without VIRTGPU_BLOB_FLAG_USE_MAPPABLE cannot be the source of a present "
     "copy (before the fix: the present is accepted and DxgkDdiPatch fails, bugcheck 0x119)"},
    {"shareable", BLOB_DEVICE_MEMORY, PRESENT_FAILS, BLOB_FLAG_SHAREABLE,
     "every present fails: the blob flags of exported device memory that is not host visible "
     "(before the fix: the present is accepted and DxgkDdiPatch fails, bugcheck 0x119)"},
};

/* The Venus wire format: a stream of 32-bit words. A 64-bit value is two
 * words, low first. A pointer is a 64-bit count (0 for NULL), and a handle
 * is a 64-bit object id that the guest chooses. */
typedef struct {
    uint32_t words[128];
    uint32_t count;
} venus_stream;

static void v32(venus_stream *v, uint32_t value)
{
    if (v->count < sizeof(v->words) / sizeof(v->words[0])) v->words[v->count++] = value;
}

static void v64(venus_stream *v, uint64_t value)
{
    v32(v, (uint32_t)value);
    v32(v, (uint32_t)(value >> 32));
}

enum { VENUS_INSTANCE = 1, VENUS_PHYSICAL_DEVICE, VENUS_DEVICE, VENUS_MEMORY };

/* Four Venus commands, without a reply: vkCreateInstance,
 * vkEnumeratePhysicalDevices for the first device, vkCreateDevice with one
 * queue of family 0, and vkAllocateMemory of MEMORY_SIZE bytes of memory
 * type 0 that the host can export as a dma-buf. The memory then has the
 * object id VENUS_MEMORY, which is the blob id of the allocation. The
 * layout is the one of Mesa's vn_protocol_driver_*.h encoders. */
static void venus_device_memory(venus_stream *v)
{
    /* vkCreateInstance */
    v32(v, 0); v32(v, 0);          /* command type, command flags */
    v64(v, 1);                     /* pCreateInfo */
    v32(v, 1); v64(v, 0);          /* VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, pNext */
    v32(v, 0);                     /* flags */
    v64(v, 1);                     /* pApplicationInfo */
    v32(v, 0); v64(v, 0);          /* VK_STRUCTURE_TYPE_APPLICATION_INFO, pNext */
    v64(v, 0); v32(v, 0);          /* pApplicationName, applicationVersion */
    v64(v, 0); v32(v, 0);          /* pEngineName, engineVersion */
    v32(v, 1u << 22 | 1u << 12);   /* apiVersion 1.1 */
    v32(v, 0); v64(v, 0);          /* enabledLayerCount, ppEnabledLayerNames */
    v32(v, 0); v64(v, 0);          /* enabledExtensionCount, ppEnabledExtensionNames */
    v64(v, 0);                     /* pAllocator */
    v64(v, 1); v64(v, VENUS_INSTANCE);

    /* vkEnumeratePhysicalDevices */
    v32(v, 2); v32(v, 0);
    v64(v, VENUS_INSTANCE);
    v64(v, 1); v32(v, 1);          /* pPhysicalDeviceCount: 1 */
    v64(v, 1); v64(v, VENUS_PHYSICAL_DEVICE);

    /* vkCreateDevice */
    v32(v, 11); v32(v, 0);
    v64(v, VENUS_PHYSICAL_DEVICE);
    v64(v, 1);                     /* pCreateInfo */
    v32(v, 3); v64(v, 0);          /* VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, pNext */
    v32(v, 0);                     /* flags */
    v32(v, 1); v64(v, 1);          /* queueCreateInfoCount, pQueueCreateInfos */
    v32(v, 2); v64(v, 0);          /* VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, pNext */
    v32(v, 0); v32(v, 0); v32(v, 1); /* flags, queueFamilyIndex, queueCount */
    v64(v, 1); v32(v, 0x3f800000); /* pQueuePriorities: 1.0f */
    v32(v, 0); v64(v, 0);          /* enabledLayerCount, ppEnabledLayerNames */
    v32(v, 0); v64(v, 0);          /* enabledExtensionCount, ppEnabledExtensionNames */
    v64(v, 0);                     /* pEnabledFeatures */
    v64(v, 0);                     /* pAllocator */
    v64(v, 1); v64(v, VENUS_DEVICE);

    /* vkAllocateMemory */
    v32(v, 21); v32(v, 0);
    v64(v, VENUS_DEVICE);
    v64(v, 1);                     /* pAllocateInfo */
    v32(v, 5);                     /* VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO */
    v64(v, 1);                     /* pNext */
    v32(v, 1000072002); v64(v, 0); /* VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO, pNext */
    v32(v, 0x200);                 /* handleTypes: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT */
    v64(v, MEMORY_SIZE); v32(v, 0); /* allocationSize, memoryTypeIndex */
    v64(v, 0);                     /* pAllocator */
    v64(v, 1); v64(v, VENUS_MEMORY);
}

typedef struct {
    const char *result, *stage, *name, *expect;
    int selected, pass;
    LONG last_status, ctx_init_status, venus_status, create_allocation_status, first_present_status, last_present_status;
    LONG window_surface_hresult;
    DWORD win32_error, session;
    uint64_t supported;
    uint32_t blob_flags, presents_asked, presents_done, presents_failed, presents_accepted;
    uint64_t blob_id;
    query_device_ids ids;
} result;

static void emit(FILE *f, const result *r)
{
    fprintf(f, "{\"schema\":1,\"probe\":\"present\",\"case\":\"%s\",\"expect\":\"%s\","
            "\"result\":\"%s\",\"stage\":\"%s\",\"pass\":%s,\"session\":%lu,"
            "\"presents_asked\":%u,\"presents_done\":%u,\"presents_failed\":%u,\"presents_accepted\":%u,"
            "\"first_present_status\":\"0x%08lx\",\"last_present_status\":\"0x%08lx\","
            "\"create_allocation_status\":\"0x%08lx\",\"ctx_init_status\":\"0x%08lx\",\"venus_status\":\"0x%08lx\","
            "\"window_surface_hresult\":\"0x%08lx\","
            "\"last_ntstatus\":\"0x%08lx\",\"win32_error\":%lu,\"supported_capset_ids\":\"0x%016llx\","
            "\"blob_mem\":%u,\"blob_flags\":%u,\"blob_id\":\"0x%016llx\",\"adapter\":",
            r->name ? r->name : "", r->expect ? r->expect : "", r->result, r->stage,
            r->pass ? "true" : "false", (unsigned long)r->session,
            r->presents_asked, r->presents_done, r->presents_failed, r->presents_accepted,
            (unsigned long)r->first_present_status, (unsigned long)r->last_present_status,
            (unsigned long)r->create_allocation_status, (unsigned long)r->ctx_init_status,
            (unsigned long)r->venus_status, (unsigned long)r->window_surface_hresult,
            (unsigned long)r->last_status, (unsigned long)r->win32_error, (unsigned long long)r->supported,
            (unsigned)BLOB_MEM_HOST3D, r->blob_flags, (unsigned long long)r->blob_id);
    if (r->selected) fprintf(f, "{\"vendor_id\":\"0x%04x\",\"device_id\":\"0x%04x\"}", r->ids.vendor_id, r->ids.device_id);
    else fputs("null", f);
    fputs("}\n", f);
}

static int save(const wchar_t *path, const result *r)
{
    if (!path) return 1;
    FILE *f = _wfopen(path, L"wb");
    if (!f) return 0;
    emit(f, r);
    int ok = fflush(f) == 0 && _commit(_fileno(f)) == 0;
    if (fclose(f) != 0) ok = 0;
    return ok;
}

static void pump(DWORD milliseconds)
{
    DWORD end = GetTickCount() + milliseconds;
    do {
        MSG msg;
        while (PeekMessageW(&msg, NULL, 0, 0, PM_REMOVE)) {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        Sleep(10);
    } while ((LONG)(end - GetTickCount()) > 0);
}

int wmain(int argc, wchar_t **argv)
{
    result r = {0};
    r.result = "api_failure";
    r.stage = "arguments";
    r.presents_asked = 1;
    const wchar_t *out = NULL;
    int which = -1, exit_code = 1, variant = 1;
    ProcessIdToSessionId(GetCurrentProcessId(), &r.session);
    for (int i = 1; i < argc; ++i) {
        if (!wcscmp(argv[i], L"--out") && i + 1 < argc) out = argv[++i];
        else if (!wcscmp(argv[i], L"--count") && i + 1 < argc) r.presents_asked = (uint32_t)wcstoul(argv[++i], NULL, 10);
        else if (!wcscmp(argv[i], L"--variant") && i + 1 < argc) variant = (int)wcstoul(argv[++i], NULL, 10);
        else if (!wcscmp(argv[i], L"--case") && i + 1 < argc) {
            ++i;
            for (int c = 0; c < (int)(sizeof(cases) / sizeof(cases[0])); ++c) {
                wchar_t wide[32];
                mbstowcs(wide, cases[c].name, 32);
                if (!wcscmp(argv[i], wide)) which = c;
            }
            if (which < 0) { r.result = "usage"; exit_code = 3; goto done; }
        } else which = -2;
    }
    if (which < 0 || r.presents_asked < 1 || r.presents_asked > 100000) {
        fputs("usage: present_probe.exe --case (unknown-blob | mappable | not-mappable | shareable) [--count N] [--out FILE]\n", stderr);
        r.result = "usage"; exit_code = 3; goto done;
    }
    const test_case *tc = &cases[which];
    r.name = tc->name;
    r.expect = tc->expect;
    r.blob_flags = tc->blob_flags;
    r.blob_id = tc->blob == BLOB_UNKNOWN ? UNKNOWN_BLOB_ID : tc->blob == BLOB_SHM ? 0 : VENUS_MEMORY;

    /* The destination of the present: a window of this process. */
    r.stage = "create_window";
    /* A window as the rendering probes make it. Its client area is the
     * size of the source. */
    WNDCLASSEXW wc = {0};
    wc.cbSize = sizeof(wc);
    wc.style = CS_OWNDC | CS_HREDRAW | CS_VREDRAW;
    wc.lpfnWndProc = DefWindowProcW;
    wc.hInstance = GetModuleHandleW(NULL);
    wc.hCursor = LoadCursorW(NULL, (LPCWSTR)IDC_ARROW);
    wc.hbrBackground = (HBRUSH)GetStockObject(WHITE_BRUSH);
    wc.lpszClassName = L"vmlab-win3d-present-probe";
    RegisterClassExW(&wc);
    const DWORD style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
    RECT frame = {0, 0, IMAGE_WIDTH, IMAGE_HEIGHT};
    AdjustWindowRect(&frame, style, FALSE);
    HWND window = CreateWindowExW(WS_EX_TOPMOST, wc.lpszClassName, L"present_probe (card 756)", style, 40, 40,
                                  frame.right - frame.left, frame.bottom - frame.top, NULL, NULL, wc.hInstance, NULL);
    if (!window) { r.win32_error = GetLastError(); goto done; }
    ShowWindow(window, SW_SHOWNORMAL);
    SetForegroundWindow(window);
    UpdateWindow(window);
    pump(1000);

    HMODULE gdi = LoadLibraryExW(L"gdi32.dll", NULL, LOAD_LIBRARY_SEARCH_SYSTEM32);
    r.stage = "load_gdi32";
    if (!gdi) { r.win32_error = GetLastError(); goto done; }
    enum_fn enumerate = (enum_fn)GetProcAddress(gdi, "D3DKMTEnumAdapters2");
    query_fn query = (query_fn)GetProcAddress(gdi, "D3DKMTQueryAdapterInfo");
    create_fn create = (create_fn)GetProcAddress(gdi, "D3DKMTCreateDevice");
    destroy_fn destroy = (destroy_fn)GetProcAddress(gdi, "D3DKMTDestroyDevice");
    escape_fn escape = (escape_fn)GetProcAddress(gdi, "D3DKMTEscape");
    create_context_fn context_create = (create_context_fn)GetProcAddress(gdi, "D3DKMTCreateContext");
    destroy_context_fn context_destroy = (destroy_context_fn)GetProcAddress(gdi, "D3DKMTDestroyContext");
    create_allocation_fn allocate = (create_allocation_fn)GetProcAddress(gdi, "D3DKMTCreateAllocation");
    present_fn present = (present_fn)GetProcAddress(gdi, "D3DKMTPresent");
    close_fn close = (close_fn)GetProcAddress(gdi, "D3DKMTCloseAdapter");
    if (!enumerate || !query || !create || !destroy || !escape || !context_create || !context_destroy ||
        !allocate || !present || !close) {
        r.win32_error = ERROR_PROC_NOT_FOUND; FreeLibrary(gdi); goto done;
    }
    enum_adapters adapters = {0};
    r.stage = "enumerate_count";
    r.last_status = enumerate(&adapters);
    if (r.last_status < 0 || !adapters.count || adapters.count > 1024) { FreeLibrary(gdi); goto done; }
    adapters.adapters = calloc(adapters.count, sizeof(*adapters.adapters));
    if (!adapters.adapters) { r.win32_error = ERROR_OUTOFMEMORY; FreeLibrary(gdi); goto done; }
    r.stage = "enumerate_adapters";
    r.last_status = enumerate(&adapters);
    if (r.last_status < 0) { free(adapters.adapters); FreeLibrary(gdi); goto done; }
    UINT matches = 0, adapter = 0;
    LUID adapter_luid = {0};
    for (ULONG i = 0; i < adapters.count; ++i) {
        query_device_ids ids = {0};
        query_adapter q = {adapters.adapters[i].handle, QUERY_PHYSICAL_ADAPTER_DEVICE_IDS, &ids, sizeof(ids)};
        if (query(&q) >= 0 && ids.vendor_id == 0x1af4 && ids.device_id == 0x1050) {
            ++matches;
            adapter = adapters.adapters[i].handle;
            adapter_luid = adapters.adapters[i].luid;
            r.ids = ids;
            r.selected = 1;
        }
    }
    r.stage = "select_virtio_gpu";
    if (matches != 1) { r.result = "adapter_selection_failed"; goto close_adapters; }

    viogpu_adapterinfo info = {0};
    query_adapter q = {adapter, QUERY_UMDRIVERPRIVATE, &info, sizeof(info)};
    r.stage = "query_adapter_info";
    r.last_status = query(&q);
    if (r.last_status < 0) goto close_adapters;
    r.supported = info.supported_capset_ids;
    r.stage = "need_venus";
    if (info.iam != VIOGPU_IAM || !(info.supported_capset_ids >> CAPSET_VENUS & 1)) { r.result = "no_venus_capset"; goto close_adapters; }

    create_device device = {0};
    device.adapter = adapter;
    r.stage = "create_device";
    r.last_status = create(&device);
    if (r.last_status < 0) goto close_adapters;

    /* A Venus context, as the Vulkan driver makes it: the driver copies a
     * Blt present itself only in such a context. */
    viogpu_escape init = {0};
    init.type = VIOGPU_CTX_INIT;
    init.data_length = sizeof(init.u.capset_id);
    init.u.capset_id = CAPSET_VENUS;
    escape_args init_args = {adapter, device.device, ESCAPE_DRIVERPRIVATE, 0, &init, sizeof(init), 0};
    r.stage = "ctx_init";
    r.ctx_init_status = r.last_status = escape(&init_args);
    if (r.last_status < 0) goto destroy_device;

    create_context context = {0};
    context.device = device.device;
    r.stage = "create_context";
    r.last_status = context_create(&context);
    if (r.last_status < 0) goto destroy_device;

    /* Device memory of the host's Vulkan driver, for a blob that the host
     * accepts. The commands go to the host context of this device, in
     * front of the request that creates the blob. */
    if (tc->blob == BLOB_DEVICE_MEMORY) {
        static struct {
            viogpu_escape escape;
            viogpu_submit_cmd_req req;
            venus_stream stream;
        } submit;
        venus_device_memory(&submit.stream);
        const uint32_t cmd_size = submit.stream.count * 4;
        submit.escape.type = VIOGPU_SUBMIT_CMD;
        submit.escape.data_length = (uint16_t)(sizeof(submit.req) + cmd_size);
        submit.req.cmd_type = VIOGPU_CMD_SUBMIT;
        submit.req.cmd_size = cmd_size;
        /* The words follow the request without padding. */
        memmove((uint8_t *)&submit.req + sizeof(submit.req), submit.stream.words, cmd_size);
        escape_args submit_args = {adapter, device.device, ESCAPE_DRIVERPRIVATE, 0, &submit,
                                   (UINT)(sizeof(submit.escape) + sizeof(submit.req) + cmd_size), 0};
        r.stage = "venus_commands";
        r.venus_status = r.last_status = escape(&submit_args);
        if (r.last_status < 0) goto destroy_context;
    }

    /* The allocation of the review's proof. The driver takes all of this
     * from the calling program. */
    viogpu_allocation_exchange exchange = {0};
    exchange.options.target = PIPE_TEXTURE_2D;
    exchange.options.format = VIRGL_FORMAT_B8G8R8A8_UNORM;
    exchange.options.width = IMAGE_WIDTH;
    exchange.options.height = IMAGE_HEIGHT;
    exchange.options.depth = 1;
    exchange.options.array_size = 1;
    exchange.size = IMAGE_WIDTH * IMAGE_HEIGHT * 4;
    exchange.blob_id = r.blob_id;
    exchange.blob_mem = BLOB_MEM_HOST3D;
    exchange.blob_flags = tc->blob_flags;
    exchange.stride = IMAGE_WIDTH * 4;
    allocation_info allocation = {0};
    allocation.private_data = &exchange;
    allocation.private_data_size = sizeof(exchange);
    create_allocation ca = {0};
    ca.device = device.device;
    ca.num_allocations = 1;
    ca.allocation_info = &allocation;
    r.stage = "create_allocation";
    r.create_allocation_status = r.last_status = allocate(&ca);
    if (tc->outcome == CREATE_FAILS) {
        r.stage = "done";
        r.pass = r.last_status < 0;
        r.result = r.pass ? "pass" : "allocation_created";
        exit_code = r.pass ? 0 : 2;
        goto destroy_context;
    }
    if (r.last_status < 0) goto destroy_context;

    /* The surface of the window, on this adapter. Windows uses it as the
     * destination of the present. */
    {
        HMODULE dwmapi = LoadLibraryExW(L"dwmapi.dll", NULL, LOAD_LIBRARY_SEARCH_SYSTEM32);
        dwm_surface_fn dwm_surface = dwmapi ? (dwm_surface_fn)GetProcAddress(dwmapi, "DwmpDxGetWindowSharedSurface") : NULL;
        ULONG format = 87; /* DXGI_FORMAT_B8G8R8A8_UNORM */
        HANDLE surface = NULL;
        UINT64 update_id = 0;
        r.stage = "window_surface";
        r.window_surface_hresult = (LONG)0x80004001; /* E_NOTIMPL: no such export */
        if (dwm_surface)
            r.window_surface_hresult = dwm_surface(window, adapter_luid, MonitorFromWindow(window, MONITOR_DEFAULTTOPRIMARY),
                                                   0, &format, &surface, &update_id);
        if (r.window_surface_hresult < 0) { r.last_status = r.window_surface_hresult; goto destroy_context; }
        pump(200);
    }

    /* A driver without the fix accepts the present, and the guest stops a
     * moment later: the file then says that a present was accepted. */
    r.stage = "present";
    r.result = "present_pending";
    if (!save(out, &r)) { r.result = "api_failure"; r.stage = "write_pending_result"; r.win32_error = ERROR_WRITE_FAULT; goto destroy_context; }
    for (uint32_t i = 0; i < r.presents_asked; ++i) {
        /* Larger than any D3DKMT_PRESENT, and 8-byte aligned. */
        static uint64_t request[1024];
        memset(request, 0, sizeof(request));
        present_head *p = (present_head *)request;
        p->context = context.context;
        p->window = window;
        p->source = allocation.allocation;
        p->dst_rect = (RECT){0, 0, IMAGE_WIDTH, IMAGE_HEIGHT};
        p->src_rect = (RECT){0, 0, IMAGE_WIDTH, IMAGE_HEIGHT};
        /* One sub-rectangle, the whole source: a present without one is refused. */
        static RECT sub_rect;
        sub_rect = p->src_rect;
        p->sub_rect_count = 1;
        p->src_sub_rects = &sub_rect;
        p->flags = PRESENT_BLT | PRESENT_REDIRECTED_BLT;
        /* The count is the one of the visible region of the screen, which
         * GDI gives (DdQueryVisRgnUniqueness). */
        typedef ULONG (WINAPI *uniqueness_fn)(void);
        uniqueness_fn uniqueness = (uniqueness_fn)GetProcAddress(gdi, "GdiEntry13");
        switch (variant) {
        case 0: break;
        case 1: p->flags |= PRESENT_COUNT_VALID; p->present_count = uniqueness ? uniqueness() : 0; break;
        case 2: p->flags |= PRESENT_COUNT_VALID; p->present_count = i + 1; break;
        default: p->flags |= PRESENT_COUNT_VALID; break;
        }
        LONG status = present(request);
        r.last_present_status = r.last_status = status;
        if (i == 0) r.first_present_status = status;
        ++r.presents_done;
        if (status < 0) ++r.presents_failed;
        else {
            ++r.presents_accepted;
            r.result = "present_accepted";
            if (r.presents_accepted == 1) save(out, &r);
        }
    }
    /* Windows calls DxgkDdiPatch for an accepted present when it schedules
     * it. Give that time before the process says it survived. */
    pump(r.presents_accepted ? 5000 : 500);
    r.stage = "done";
    r.pass = (tc->outcome == PRESENT_FAILS ? r.presents_failed : r.presents_accepted) == r.presents_asked;
    r.result = r.pass ? "pass" : r.presents_accepted ? "present_accepted" : "present_failed";
    exit_code = r.pass ? 0 : 2;

destroy_context:
    {
        destroy_context d = {context.context};
        if (context.context) context_destroy(&d);
    }
destroy_device:
    {
        destroy_device d = {device.device};
        destroy(&d);
    }
close_adapters:
    for (ULONG i = 0; i < adapters.count; ++i) {
        close_adapter c = {adapters.adapters[i].handle};
        close(&c);
    }
    free(adapters.adapters);
    FreeLibrary(gdi);
done:
    emit(stdout, &r);
    if (!save(out, &r) && exit_code == 0) exit_code = 1;
    return exit_code;
}
