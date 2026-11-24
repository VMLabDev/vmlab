/* Tries the two ways a program had to the scanout of the viogpu3d kernel
 * driver without Windows (card #755, docs/reviews/win3d-driver.md F7), as
 * whatever user runs it, from any session.
 *
 *   render-flip  a VIOGPU_CMD_PRESENT_FLIP command in the stream of
 *                D3DKMTRender, for a resource of this program
 *   escape       the VIOGPU_RES_SET_SCANOUT_BLOB escape, for a blob
 *                allocation of this program
 *
 * The resource is one colour (magenta, B=255 G=0 R=255). The program repeats
 * the request for --seconds, so that a screenshot from the host can be taken
 * meanwhile: a driver without the fix shows the colour in place of the
 * desktop. A fixed driver refuses the first request and the program ends.
 * Run it in a disposable VM. */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wchar.h>
#include <io.h>

/* Published d3dkmthk.h ABI subset. llvm-mingw does not ship that SDK header.
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

typedef struct { UINT handle; } close_adapter;

typedef struct {
    UINT device;
    UINT node_ordinal;
    UINT engine_affinity;
    UINT flags;
    void *private_data;
    UINT private_data_size;
    UINT client_hint;
    UINT context;
    void *command_buffer;
    UINT command_buffer_size;
    void *allocation_list;
    UINT allocation_list_size;
    void *patch_location_list;
    UINT patch_location_list_size;
    uint64_t command_buffer_va;
} create_context;

typedef struct { UINT context; } destroy_context;

typedef struct {
    UINT context;
    UINT command_offset;
    UINT command_length;
    UINT allocation_count;
    UINT patch_location_count;
    void *new_command_buffer;
    UINT new_command_buffer_size;
    void *new_allocation_list;
    UINT new_allocation_list_size;
    void *new_patch_location_list;
    UINT new_patch_location_list_size;
    UINT flags;
    uint64_t present_history_token;
    ULONG broadcast_context_count;
    UINT broadcast_context[64];
    ULONG queued_buffer_count;
    uint64_t new_command_buffer_va;
    void *private_data;
    UINT private_data_size;
} render_args;

typedef struct {
    UINT allocation;
    const void *system_mem;
    void *private_data;
    UINT private_data_size;
    UINT vidpn_source_id;
    UINT flags;
} allocation_info;

typedef struct {
    UINT device;
    UINT resource;
    UINT global_share;
    const void *private_runtime_data;
    UINT private_runtime_data_size;
    const void *private_data;
    UINT private_data_size;
    UINT num_allocations;
    allocation_info *allocations;
    UINT flags; /* bit 0: CreateResource */
    HANDLE private_runtime_resource;
} create_allocation;

typedef struct {
    UINT device;
    UINT resource;
    const UINT *allocations;
    UINT allocation_count;
} destroy_allocation;

typedef LONG (WINAPI *enum_fn)(const enum_adapters *);
typedef LONG (WINAPI *query_fn)(const query_adapter *);
typedef LONG (WINAPI *create_fn)(create_device *);
typedef LONG (WINAPI *destroy_fn)(const destroy_device *);
typedef LONG (WINAPI *escape_fn)(const escape_args *);
typedef LONG (WINAPI *close_fn)(const close_adapter *);
typedef LONG (WINAPI *create_context_fn)(create_context *);
typedef LONG (WINAPI *destroy_context_fn)(const destroy_context *);
typedef LONG (WINAPI *render_fn)(render_args *);
typedef LONG (WINAPI *create_allocation_fn)(create_allocation *);
typedef LONG (WINAPI *destroy_allocation_fn)(const destroy_allocation *);

/* viogpu/shared/viogpum.h of the driver source. VIOGPU_ADAPTERINFO is not
 * packed there: four bytes of padding follow the flags. */
typedef struct {
    uint64_t iam;
    uint32_t flags;
    uint64_t supported_capset_ids;
} viogpu_adapterinfo;

#pragma pack(push, 1)
typedef struct {
    uint32_t res_handle, scanout_id, width, height, x, y, format, stride, offset;
} viogpu_set_scanout_blob_req;

typedef struct {
    uint32_t res_handle, id;
} viogpu_res_info_req;

typedef struct {
    uint16_t type;
    uint16_t data_length;
    /* The union of VIOGPU_ESCAPE: its largest member has 36 bytes. */
    union {
        uint32_t capset_id; /* VIOGPU_CTX_INIT_REQ */
        viogpu_res_info_req res_info;
        viogpu_set_scanout_blob_req scanout;
        uint8_t largest[36];
    } u;
} viogpu_escape;

typedef struct {
    uint32_t context; /* D3DKMT_HANDLE hContext, not read by the driver */
    uint32_t cmd_type;
    uint32_t cmd_size;
} viogpu_submit_cmd_req;

/* The submit request follows the whole VIOGPU_ESCAPE in the private data. */
typedef struct {
    viogpu_escape escape;
    viogpu_submit_cmd_req req;
    uint32_t cmd[32];
} submit_request;

typedef struct {
    uint32_t target, format, bind, width, height, depth, array_size, last_level, nr_samples, flags;
} viogpu_resource_options;

typedef struct {
    viogpu_resource_options options;
    uint64_t size;
    uint64_t blob_id;
    uint32_t blob_mem;
    uint32_t blob_flags;
    uint32_t stride;
    uint32_t scanout_offset;
} viogpu_create_allocation_exchange;

typedef struct { uint32_t type, size; } viogpu_command_hdr;

typedef struct {
    uint32_t scan_id, res_id, width, height, x, y, is_blob, format, stride, offset;
} viogpu_present_flip_cmd;
#pragma pack(pop)

_Static_assert(sizeof(adapter_info) == 20, "D3DKMT_ADAPTERINFO ABI");
_Static_assert(sizeof(query_device_ids) == 28, "D3DKMT_QUERY_DEVICE_IDS ABI");
_Static_assert(offsetof(escape_args, data) == 16, "D3DKMT_ESCAPE ABI");
_Static_assert(sizeof(create_device) == (sizeof(void *) == 8 ? 64 : 36), "D3DKMT_CREATEDEVICE ABI");
_Static_assert(sizeof(create_context) == (sizeof(void *) == 8 ? 96 : 64), "D3DKMT_CREATECONTEXT ABI");
_Static_assert(offsetof(create_context, context) == (sizeof(void *) == 8 ? 32 : 28), "D3DKMT_CREATECONTEXT ABI");
_Static_assert(sizeof(render_args) == (sizeof(void *) == 8 ? 368 : 336), "D3DKMT_RENDER ABI");
_Static_assert(offsetof(render_args, broadcast_context) == (sizeof(void *) == 8 ? 84 : 60), "D3DKMT_RENDER ABI");
_Static_assert(sizeof(allocation_info) == (sizeof(void *) == 8 ? 40 : 24), "D3DDDI_ALLOCATIONINFO ABI");
_Static_assert(sizeof(create_allocation) == (sizeof(void *) == 8 ? 72 : 44), "D3DKMT_CREATEALLOCATION ABI");
_Static_assert(sizeof(viogpu_adapterinfo) == 24 && offsetof(viogpu_adapterinfo, supported_capset_ids) == 16, "VIOGPU_ADAPTERINFO ABI");
_Static_assert(sizeof(viogpu_escape) == 40, "VIOGPU_ESCAPE ABI");
_Static_assert(sizeof(viogpu_create_allocation_exchange) == 72, "VIOGPU_CREATE_ALLOCATION_EXCHANGE ABI");
_Static_assert(sizeof(viogpu_present_flip_cmd) == 40, "VIOGPU_PRESENT_FLIP_CMD ABI");

enum {
    QUERY_UMDRIVERPRIVATE = 0,
    QUERY_PHYSICAL_ADAPTER_DEVICE_IDS = 31,
    ESCAPE_DRIVERPRIVATE = 0,
    VIOGPU_RES_INFO = 0x100,
    VIOGPU_RES_SET_SCANOUT_BLOB = 0x105,
    VIOGPU_CTX_INIT = 0x200,
    VIOGPU_SUBMIT_CMD = 0x300,
    VIOGPU_CMD_SUBMIT = 1,
    VIOGPU_CMD_PRESENT_FLIP = 4,
    /* virgl_hw.h and virgl_protocol.h of virglrenderer */
    PIPE_TEXTURE_2D = 2,
    VIRGL_FORMAT_B8G8R8A8_UNORM = 1,
    VIRGL_BIND_RENDER_TARGET = 1 << 1,
    VIRGL_BIND_SAMPLER_VIEW = 1 << 3,
    VIRGL_CCMD_CREATE_OBJECT = 1,
    VIRGL_CCMD_SET_FRAMEBUFFER_STATE = 5,
    VIRGL_CCMD_CLEAR = 7,
    VIRGL_CCMD_PIPE_RESOURCE_CREATE = 48,
    VIRGL_OBJECT_SURFACE = 8,
    PIPE_CLEAR_COLOR0 = 1 << 2,
    BLOB_MEM_HOST3D = 2,
    BLOB_FLAG_USE_SHAREABLE = 2,
    WIDTH = 1024,
    HEIGHT = 768,
    BLOB_ID = 0x755,
};
#define VIOGPU_IAM 0x56696f475055ull
#define VIRGL_CMD0(cmd, obj, len) ((uint32_t)(cmd) | ((uint32_t)(obj) << 8) | ((uint32_t)(len) << 16))

typedef struct {
    const char *result, *stage, *name;
    int selected, completed, pass, blob;
    LONG last_status, first_status, last_request_status, ctx_init_status, create_status, clear_status, setup_status;
    DWORD win32_error;
    uint64_t supported;
    uint32_t capset_id, res_id, requests, accepted, seconds;
    query_device_ids ids;
} result;

static void emit(FILE *f, const result *r)
{
    fprintf(f, "{\"schema\":1,\"probe\":\"scanout\",\"case\":\"%s\",\"result\":\"%s\",\"stage\":\"%s\",\"pass\":%s,"
            "\"request_completed\":%s,\"first_status\":",
            r->name ? r->name : "", r->result, r->stage, r->pass ? "true" : "false", r->completed ? "true" : "false");
    if (r->completed) fprintf(f, "\"0x%08lx\"", (unsigned long)r->first_status);
    else fputs("null", f);
    fprintf(f, ",\"last_request_status\":\"0x%08lx\",\"requests\":%u,\"accepted\":%u,\"seconds\":%u,"
            "\"last_ntstatus\":\"0x%08lx\",\"win32_error\":%lu,\"supported_capset_ids\":\"0x%016llx\","
            "\"context_capset_id\":%u,\"ctx_init_status\":\"0x%08lx\",\"blob\":%s,\"setup_submit_status\":\"0x%08lx\","
            "\"create_allocation_status\":\"0x%08lx\",\"resource_id\":%u,\"clear_submit_status\":\"0x%08lx\","
            "\"width\":%d,\"height\":%d,\"colour_bgra\":\"ff00ffff\",\"session\":",
            (unsigned long)r->last_request_status, r->requests, r->accepted, r->seconds,
            (unsigned long)r->last_status, (unsigned long)r->win32_error, (unsigned long long)r->supported,
            r->capset_id, (unsigned long)r->ctx_init_status, r->blob ? "true" : "false",
            (unsigned long)r->setup_status, (unsigned long)r->create_status, r->res_id,
            (unsigned long)r->clear_status, WIDTH, HEIGHT);
    DWORD session = 0;
    if (ProcessIdToSessionId(GetCurrentProcessId(), &session)) fprintf(f, "%lu", (unsigned long)session);
    else fputs("null", f);
    fputs(",\"adapter\":", f);
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

static escape_fn escape_call;
static UINT adapter, device;

/* Sends virgl commands to the host context of the device. */
static LONG submit(const uint32_t *words, unsigned count)
{
    submit_request request;
    memset(&request, 0, sizeof(request));
    request.escape.type = VIOGPU_SUBMIT_CMD;
    request.escape.data_length = (uint16_t)(sizeof(request.req) + count * 4);
    request.req.cmd_type = VIOGPU_CMD_SUBMIT;
    request.req.cmd_size = count * 4;
    memcpy(request.cmd, words, count * 4);
    escape_args args = {adapter, device, ESCAPE_DRIVERPRIVATE, 0, &request,
                        (UINT)(sizeof(request.escape) + sizeof(request.req) + count * 4), 0};
    return escape_call(&args);
}

int wmain(int argc, wchar_t **argv)
{
    result r = {0};
    r.result = "api_failure";
    r.stage = "arguments";
    r.seconds = 12;
    const wchar_t *out = NULL;
    int which = -1, exit_code = 1;
    for (int i = 1; i < argc; ++i) {
        if (!wcscmp(argv[i], L"--out") && i + 1 < argc) out = argv[++i];
        else if (!wcscmp(argv[i], L"--seconds") && i + 1 < argc) r.seconds = (uint32_t)wcstoul(argv[++i], NULL, 10);
        else if (!wcscmp(argv[i], L"--case") && i + 1 < argc) {
            ++i;
            if (!wcscmp(argv[i], L"render-flip")) which = 0;
            else if (!wcscmp(argv[i], L"escape")) which = 1;
            else which = -2;
        } else which = -2;
    }
    if (which < 0 || r.seconds > 600) {
        fputs("usage: scanout_probe.exe --case (render-flip | escape) [--seconds N] [--out FILE]\n", stderr);
        r.result = "usage"; exit_code = 3; goto done;
    }
    r.name = which ? "escape" : "render-flip";
    r.blob = which;

    HMODULE gdi = LoadLibraryExW(L"gdi32.dll", NULL, LOAD_LIBRARY_SEARCH_SYSTEM32);
    r.stage = "load_gdi32";
    if (!gdi) { r.win32_error = GetLastError(); goto done; }
    enum_fn enumerate = (enum_fn)GetProcAddress(gdi, "D3DKMTEnumAdapters2");
    query_fn query = (query_fn)GetProcAddress(gdi, "D3DKMTQueryAdapterInfo");
    create_fn create = (create_fn)GetProcAddress(gdi, "D3DKMTCreateDevice");
    destroy_fn destroy = (destroy_fn)GetProcAddress(gdi, "D3DKMTDestroyDevice");
    escape_call = (escape_fn)GetProcAddress(gdi, "D3DKMTEscape");
    close_fn close = (close_fn)GetProcAddress(gdi, "D3DKMTCloseAdapter");
    create_context_fn create_ctx = (create_context_fn)GetProcAddress(gdi, "D3DKMTCreateContext");
    destroy_context_fn destroy_ctx = (destroy_context_fn)GetProcAddress(gdi, "D3DKMTDestroyContext");
    render_fn render = (render_fn)GetProcAddress(gdi, "D3DKMTRender");
    create_allocation_fn create_alloc = (create_allocation_fn)GetProcAddress(gdi, "D3DKMTCreateAllocation");
    destroy_allocation_fn destroy_alloc = (destroy_allocation_fn)GetProcAddress(gdi, "D3DKMTDestroyAllocation");
    if (!enumerate || !query || !create || !destroy || !escape_call || !close || !create_ctx || !destroy_ctx ||
        !render || !create_alloc || !destroy_alloc) {
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
    UINT matches = 0;
    for (ULONG i = 0; i < adapters.count; ++i) {
        query_device_ids ids = {0};
        query_adapter q = {adapters.adapters[i].handle, QUERY_PHYSICAL_ADAPTER_DEVICE_IDS, &ids, sizeof(ids)};
        if (query(&q) >= 0 && ids.vendor_id == 0x1af4 && ids.device_id == 0x1050) {
            ++matches;
            adapter = adapters.adapters[i].handle;
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
    /* The colour is made with virgl commands: capset 2 (virgl2), else 1. */
    r.stage = "pick_capset";
    if (info.iam != VIOGPU_IAM || !(info.supported_capset_ids & 6)) { r.result = "no_virgl_capset"; goto close_adapters; }
    r.capset_id = info.supported_capset_ids & 4 ? 2 : 1;

    create_device dev = {0};
    dev.adapter = adapter;
    r.stage = "create_device";
    r.last_status = create(&dev);
    if (r.last_status < 0) goto close_adapters;
    device = dev.device;

    viogpu_escape init = {0};
    init.type = VIOGPU_CTX_INIT;
    init.data_length = sizeof(init.u.capset_id);
    init.u.capset_id = r.capset_id;
    escape_args init_args = {adapter, device, ESCAPE_DRIVERPRIVATE, 0, &init, sizeof(init), 0};
    r.stage = "ctx_init";
    r.ctx_init_status = r.last_status = escape_call(&init_args);
    if (r.last_status < 0) goto destroy_device;

    if (r.blob) {
        /* The host object that the blob resource of the allocation takes. */
        const uint32_t pipe_create[] = {
            VIRGL_CMD0(VIRGL_CCMD_PIPE_RESOURCE_CREATE, 0, 11),
            PIPE_TEXTURE_2D, VIRGL_FORMAT_B8G8R8A8_UNORM, VIRGL_BIND_RENDER_TARGET | VIRGL_BIND_SAMPLER_VIEW,
            WIDTH, HEIGHT, 1, 1, 0, 0, 0, BLOB_ID,
        };
        r.stage = "submit_pipe_resource_create";
        r.setup_status = r.last_status = submit(pipe_create, sizeof(pipe_create) / 4);
        if (r.last_status < 0) goto destroy_device;
    }

    viogpu_create_allocation_exchange exchange;
    memset(&exchange, 0, sizeof(exchange));
    exchange.options.target = PIPE_TEXTURE_2D;
    exchange.options.format = VIRGL_FORMAT_B8G8R8A8_UNORM;
    exchange.options.bind = VIRGL_BIND_RENDER_TARGET | VIRGL_BIND_SAMPLER_VIEW;
    exchange.options.width = WIDTH;
    exchange.options.height = HEIGHT;
    exchange.options.depth = 1;
    exchange.options.array_size = 1;
    exchange.size = (uint64_t)WIDTH * HEIGHT * 4;
    exchange.stride = WIDTH * 4;
    if (r.blob) {
        exchange.blob_id = BLOB_ID;
        exchange.blob_mem = BLOB_MEM_HOST3D;
        exchange.blob_flags = BLOB_FLAG_USE_SHAREABLE;
    }
    uint32_t resource_magic = 0;
    allocation_info alloc = {0};
    alloc.private_data = &exchange;
    alloc.private_data_size = sizeof(exchange);
    create_allocation ca = {0};
    ca.device = device;
    ca.private_data = &resource_magic;
    ca.private_data_size = sizeof(resource_magic);
    ca.num_allocations = 1;
    ca.allocations = &alloc;
    ca.flags = 1;
    r.stage = "create_allocation";
    r.create_status = r.last_status = create_alloc(&ca);
    if (r.last_status < 0) goto destroy_device;

    viogpu_escape res_info = {0};
    res_info.type = VIOGPU_RES_INFO;
    res_info.data_length = sizeof(res_info.u.res_info);
    res_info.u.res_info.res_handle = alloc.allocation;
    escape_args info_args = {adapter, device, ESCAPE_DRIVERPRIVATE, 0, &res_info, sizeof(res_info), 0};
    r.stage = "res_info";
    r.last_status = escape_call(&info_args);
    if (r.last_status < 0) goto destroy_allocation;
    r.res_id = res_info.u.res_info.id;

    /* One colour: a surface on the resource, the framebuffer, a clear. */
    {
        const float one = 1.0f, zero = 0.0f;
        uint32_t f1, f0;
        memcpy(&f1, &one, 4);
        memcpy(&f0, &zero, 4);
        const uint32_t clear[] = {
            VIRGL_CMD0(VIRGL_CCMD_CREATE_OBJECT, VIRGL_OBJECT_SURFACE, 5),
            1, r.res_id, VIRGL_FORMAT_B8G8R8A8_UNORM, 0, 0,
            VIRGL_CMD0(VIRGL_CCMD_SET_FRAMEBUFFER_STATE, 0, 3), 1, 0, 1,
            VIRGL_CMD0(VIRGL_CCMD_CLEAR, 0, 8), PIPE_CLEAR_COLOR0, f1, f0, f1, f1, 0, 0, 0,
        };
        r.stage = "submit_clear";
        r.clear_status = r.last_status = submit(clear, sizeof(clear) / 4);
        if (r.last_status < 0) goto destroy_allocation;
    }

    create_context ctx = {0};
    if (!r.blob) {
        ctx.device = device;
        r.stage = "create_context";
        r.last_status = create_ctx(&ctx);
        if (r.last_status < 0) goto destroy_allocation;
        r.stage = "command_buffer";
        if (!ctx.command_buffer || ctx.command_buffer_size < sizeof(viogpu_command_hdr) + sizeof(viogpu_present_flip_cmd)) {
            r.result = "no_command_buffer"; goto destroy_context;
        }
    }

    r.stage = "request";
    r.result = "request_did_not_return";
    if (!save(out, &r)) { r.result = "api_failure"; r.stage = "write_pending_result"; r.win32_error = ERROR_WRITE_FAULT; goto destroy_context; }
    void *command_buffer = ctx.command_buffer;
    const ULONGLONG end = GetTickCount64() + (ULONGLONG)r.seconds * 1000;
    do {
        LONG status;
        if (r.blob) {
            viogpu_escape e = {0};
            e.type = VIOGPU_RES_SET_SCANOUT_BLOB;
            e.data_length = sizeof(e.u.scanout);
            e.u.scanout.res_handle = alloc.allocation;
            e.u.scanout.width = WIDTH;
            e.u.scanout.height = HEIGHT;
            e.u.scanout.format = VIRGL_FORMAT_B8G8R8A8_UNORM;
            e.u.scanout.stride = WIDTH * 4;
            escape_args args = {adapter, device, ESCAPE_DRIVERPRIVATE, 0, &e, sizeof(e), 0};
            status = escape_call(&args);
        } else {
            viogpu_command_hdr hdr = {VIOGPU_CMD_PRESENT_FLIP, sizeof(viogpu_present_flip_cmd)};
            viogpu_present_flip_cmd flip = {0};
            flip.res_id = r.res_id;
            flip.width = WIDTH;
            flip.height = HEIGHT;
            flip.format = VIRGL_FORMAT_B8G8R8A8_UNORM;
            flip.stride = WIDTH * 4;
            memcpy(command_buffer, &hdr, sizeof(hdr));
            memcpy((char *)command_buffer + sizeof(hdr), &flip, sizeof(flip));
            render_args ra;
            memset(&ra, 0, sizeof(ra));
            ra.context = ctx.context;
            ra.command_length = sizeof(hdr) + sizeof(flip);
            status = render(&ra);
            if (ra.new_command_buffer) command_buffer = ra.new_command_buffer;
        }
        if (!r.requests) r.first_status = status;
        r.last_request_status = r.last_status = status;
        ++r.requests;
        if (status >= 0) ++r.accepted;
        r.completed = 1;
        if (status < 0) break;
        Sleep(15);
    } while (GetTickCount64() < end);
    r.stage = "done";
    /* The fixed driver refuses the first request, and accepts none. */
    r.pass = r.first_status < 0 && !r.accepted;
    r.result = r.pass ? "refused" : "accepted";
    exit_code = r.pass ? 0 : 2;

destroy_context:
    if (ctx.context) {
        destroy_context d = {ctx.context};
        destroy_ctx(&d);
    }
destroy_allocation:
    {
        destroy_allocation d = {device, ca.resource, NULL, 0};
        destroy_alloc(&d);
    }
destroy_device:
    {
        destroy_device d = {device};
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
