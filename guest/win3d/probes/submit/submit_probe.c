/* Sends the VIOGPU_SUBMIT_CMD escape of the viogpu3d kernel driver with the
 * command sizes of card #752 (docs/reviews/win3d-driver.md F4), as whatever
 * user runs it. One case per run: a driver without the fix can bugcheck the
 * guest on the case "wrap". Run it in a disposable VM. */
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
typedef LONG (WINAPI *enum_fn)(const enum_adapters *);
typedef LONG (WINAPI *query_fn)(const query_adapter *);
typedef LONG (WINAPI *create_fn)(create_device *);
typedef LONG (WINAPI *destroy_fn)(const destroy_device *);
typedef LONG (WINAPI *escape_fn)(const escape_args *);
typedef LONG (WINAPI *close_fn)(const close_adapter *);

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

typedef struct {
    uint32_t context; /* D3DKMT_HANDLE hContext, not read by the driver */
    uint32_t cmd_type;
    uint32_t cmd_size;
} viogpu_submit_cmd_req;

/* The submit request follows the whole VIOGPU_ESCAPE in the private data. */
typedef struct {
    viogpu_escape escape;
    viogpu_submit_cmd_req req;
    uint8_t cmd[16];
} submit_request;
#pragma pack(pop)

_Static_assert(sizeof(adapter_info) == 20, "D3DKMT_ADAPTERINFO ABI");
_Static_assert(sizeof(query_device_ids) == 28, "D3DKMT_QUERY_DEVICE_IDS ABI");
_Static_assert(offsetof(escape_args, data) == 16, "D3DKMT_ESCAPE ABI");
_Static_assert(offsetof(create_device, device) == sizeof(void *) + 4, "D3DKMT_CREATEDEVICE ABI");
_Static_assert(sizeof(create_device) == (sizeof(void *) == 8 ? 64 : 36), "D3DKMT_CREATEDEVICE ABI");
_Static_assert(sizeof(viogpu_adapterinfo) == 24 && offsetof(viogpu_adapterinfo, supported_capset_ids) == 16, "VIOGPU_ADAPTERINFO ABI");
_Static_assert(sizeof(viogpu_escape) == 40, "VIOGPU_ESCAPE ABI");
_Static_assert(sizeof(viogpu_submit_cmd_req) == 12, "VIOGPU_SUBMIT_CMD_REQ ABI");
_Static_assert(sizeof(submit_request) == 68, "escape buffer size");

enum {
    QUERY_UMDRIVERPRIVATE = 0,
    QUERY_PHYSICAL_ADAPTER_DEVICE_IDS = 31,
    ESCAPE_DRIVERPRIVATE = 0,
    VIOGPU_CTX_INIT = 0x200,
    VIOGPU_SUBMIT_CMD = 0x300,
    VIOGPU_CMD_NOP = 0,
    VIOGPU_CMD_SUBMIT = 1,
};
#define VIOGPU_IAM 0x56696f475055ull
#define STATUS_INVALID_PARAMETER ((LONG)0xc000000du)
#define STATUS_INVALID_BUFFER_SIZE ((LONG)0xc0000206u)

typedef struct {
    const char *name;
    uint16_t data_length;
    uint32_t cmd_type, cmd_size;
    LONG status; /* what a fixed driver returns */
    const char *expect;
} test_case;

/* The driver checks the sizes first, then the command type. A command type
 * of 0 is refused after the size check and nothing is allocated or sent, so
 * the status of a case with type 0 says what the size check did. */
static const test_case cases[] = {
    {"fits", 12 + 16, VIOGPU_CMD_NOP, 16, STATUS_INVALID_PARAMETER,
     "STATUS_INVALID_PARAMETER: the size check passes, command type 0 is refused"},
    {"too-long", 12 + 16, VIOGPU_CMD_NOP, 17, STATUS_INVALID_BUFFER_SIZE,
     "STATUS_INVALID_BUFFER_SIZE for a command one byte longer than the payload"},
    {"wrap-type0", 12, VIOGPU_CMD_NOP, 0xfffffff8u, STATUS_INVALID_BUFFER_SIZE,
     "STATUS_INVALID_BUFFER_SIZE for CmdSize 0xFFFFFFF8 (before the fix: STATUS_INVALID_PARAMETER, the size check passed)"},
    {"wrap", 12, VIOGPU_CMD_SUBMIT, 0xfffffff8u, STATUS_INVALID_BUFFER_SIZE,
     "STATUS_INVALID_BUFFER_SIZE for CmdSize 0xFFFFFFF8 (before the fix: bugcheck 0x50, or STATUS_INSUFFICIENT_RESOURCES)"},
    {"wrap-first", 12, VIOGPU_CMD_SUBMIT, 0xfffffff4u, STATUS_INVALID_BUFFER_SIZE,
     "STATUS_INVALID_BUFFER_SIZE for CmdSize 0xFFFFFFF4, the first size that wraps"},
};

typedef struct {
    const char *result, *stage, *name, *expect;
    int selected, completed, pass;
    LONG last_status, escape_status, expected_status, ctx_init_status;
    DWORD win32_error;
    uint64_t supported;
    uint32_t capset_id, cmd_type, cmd_size, data_length, private_size;
    query_device_ids ids;
} result;

static void emit(FILE *f, const result *r)
{
    fprintf(f, "{\"schema\":1,\"probe\":\"submit\",\"case\":\"%s\",\"expect\":\"%s\","
            "\"result\":\"%s\",\"stage\":\"%s\",\"pass\":%s,\"request_completed\":%s,\"escape_status\":",
            r->name ? r->name : "", r->expect ? r->expect : "", r->result, r->stage,
            r->pass ? "true" : "false", r->completed ? "true" : "false");
    if (r->completed) fprintf(f, "\"0x%08lx\"", (unsigned long)r->escape_status);
    else fputs("null", f);
    fprintf(f, ",\"expected_status\":\"0x%08lx\",\"last_ntstatus\":\"0x%08lx\",\"win32_error\":%lu,"
            "\"supported_capset_ids\":\"0x%016llx\",\"context_capset_id\":%u,\"ctx_init_status\":\"0x%08lx\","
            "\"cmd_type\":%u,\"cmd_size\":\"0x%08x\",\"data_length\":%u,\"private_data_size\":%u,\"adapter\":",
            (unsigned long)r->expected_status, (unsigned long)r->last_status, (unsigned long)r->win32_error,
            (unsigned long long)r->supported, r->capset_id, (unsigned long)r->ctx_init_status,
            r->cmd_type, r->cmd_size, r->data_length, r->private_size);
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

int wmain(int argc, wchar_t **argv)
{
    result r = {0};
    r.result = "api_failure";
    r.stage = "arguments";
    const wchar_t *out = NULL;
    int which = -1, exit_code = 1;
    for (int i = 1; i < argc; ++i) {
        if (!wcscmp(argv[i], L"--out") && i + 1 < argc) out = argv[++i];
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
    if (which < 0) {
        fputs("usage: submit_probe.exe --case (fits | too-long | wrap-type0 | wrap | wrap-first) [--out FILE]\n", stderr);
        r.result = "usage"; exit_code = 3; goto done;
    }
    const test_case *tc = &cases[which];
    r.name = tc->name;
    r.expect = tc->expect;
    r.expected_status = tc->status;
    r.cmd_type = tc->cmd_type;
    r.cmd_size = tc->cmd_size;
    r.data_length = tc->data_length;

    HMODULE gdi = LoadLibraryExW(L"gdi32.dll", NULL, LOAD_LIBRARY_SEARCH_SYSTEM32);
    r.stage = "load_gdi32";
    if (!gdi) { r.win32_error = GetLastError(); goto done; }
    enum_fn enumerate = (enum_fn)GetProcAddress(gdi, "D3DKMTEnumAdapters2");
    query_fn query = (query_fn)GetProcAddress(gdi, "D3DKMTQueryAdapterInfo");
    create_fn create = (create_fn)GetProcAddress(gdi, "D3DKMTCreateDevice");
    destroy_fn destroy = (destroy_fn)GetProcAddress(gdi, "D3DKMTDestroyDevice");
    escape_fn escape = (escape_fn)GetProcAddress(gdi, "D3DKMTEscape");
    close_fn close = (close_fn)GetProcAddress(gdi, "D3DKMTCloseAdapter");
    if (!enumerate || !query || !create || !destroy || !escape || !close) {
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
    uint32_t k = 0;
    while (k < 64 && !(info.supported_capset_ids >> k & 1)) ++k;
    r.stage = "pick_capset";
    if (info.iam != VIOGPU_IAM || k == 64) { r.result = "no_capset"; goto close_adapters; }
    r.capset_id = k;

    /* The submit escape needs a device: the driver takes the host context id from it. */
    create_device device = {0};
    device.adapter = adapter;
    r.stage = "create_device";
    r.last_status = create(&device);
    if (r.last_status < 0) goto close_adapters;

    /* The context of the review's proof, as the user-mode driver makes it. */
    viogpu_escape init = {0};
    init.type = VIOGPU_CTX_INIT;
    init.data_length = sizeof(init.u.capset_id);
    init.u.capset_id = k;
    escape_args init_args = {adapter, device.device, ESCAPE_DRIVERPRIVATE, 0, &init, sizeof(init), 0};
    r.stage = "ctx_init";
    r.ctx_init_status = r.last_status = escape(&init_args);
    if (r.last_status < 0) goto destroy_device;

    submit_request request;
    memset(&request, 0, sizeof(request));
    request.escape.type = VIOGPU_SUBMIT_CMD;
    request.escape.data_length = tc->data_length;
    request.req.cmd_type = tc->cmd_type;
    request.req.cmd_size = tc->cmd_size;
    r.private_size = sizeof(request);
    escape_args args = {adapter, device.device, ESCAPE_DRIVERPRIVATE, 0, &request, sizeof(request), 0};

    /* A driver without the fix may not come back from the escape: the file
     * then says so. */
    r.stage = "escape";
    r.result = "request_did_not_return";
    if (!save(out, &r)) { r.result = "api_failure"; r.stage = "write_pending_result"; r.win32_error = ERROR_WRITE_FAULT; goto destroy_device; }
    r.escape_status = r.last_status = escape(&args);
    r.completed = 1;
    r.stage = "done";
    r.pass = r.escape_status == tc->status;
    r.result = r.pass ? "pass" : "wrong_status";
    exit_code = r.pass ? 0 : 2;

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
