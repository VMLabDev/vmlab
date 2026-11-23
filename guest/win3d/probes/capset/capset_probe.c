/* Sends the VIOGPU_GET_CAPS escape of the viogpu3d kernel driver with the
 * hostile arguments of card #751 (docs/reviews/win3d-driver.md F1 and F3),
 * as whatever user runs it. One case per run: a driver without the fix
 * bugchecks the guest on some of them. Run it in a disposable VM. */
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
typedef LONG (WINAPI *escape_fn)(const escape_args *);
typedef LONG (WINAPI *close_fn)(const close_adapter *);

/* viogpu/shared/viogpum.h of the driver source. VIOGPU_ADAPTERINFO is not
 * packed there: the pragma before it ends the packing. */
typedef struct {
    uint64_t iam;
    uint32_t flags;
    uint32_t padding;
    uint64_t supported_capset_ids;
} viogpu_adapterinfo;

#pragma pack(push, 1)
typedef struct {
    uint16_t type;
    uint16_t data_length;
    struct {
        uint32_t capset_id;
        uint32_t version;
        uint32_t size;
        uint64_t capset;
    } capset;
    /* VIOGPU_ESCAPE is a union: its largest member has 36 bytes. The driver
     * refuses private data that is shorter than the whole structure. */
    uint8_t rest[44];
} viogpu_escape;
#pragma pack(pop)

_Static_assert(sizeof(adapter_info) == 20, "D3DKMT_ADAPTERINFO ABI");
_Static_assert(sizeof(query_device_ids) == 28, "D3DKMT_QUERY_DEVICE_IDS ABI");
_Static_assert(offsetof(escape_args, data) == 16, "D3DKMT_ESCAPE ABI");
_Static_assert(sizeof(viogpu_adapterinfo) == 24, "VIOGPU_ADAPTERINFO ABI");
_Static_assert(offsetof(viogpu_adapterinfo, supported_capset_ids) == 16, "VIOGPU_ADAPTERINFO offset");
_Static_assert(offsetof(viogpu_escape, capset.capset) == 16, "VIOGPU_CAPSET_REQ ABI");
_Static_assert(sizeof(viogpu_escape) == 68, "escape buffer size");

enum {
    QUERY_UMDRIVERPRIVATE = 0,
    QUERY_PHYSICAL_ADAPTER_DEVICE_IDS = 31,
    ESCAPE_DRIVERPRIVATE = 0,
    VIOGPU_GET_CAPS = 2,
    USER_BUFFER = 65536,
    FILL = 0xa5,
};
#define VIOGPU_IAM 0x56696f475055ull
/* The lowest address of the kernel half on x64. Nothing is mapped there. */
#define KERNEL_ADDRESS 0xffff800000000000ull
#define STATUS_INVALID_PARAMETER_1 ((LONG)0xc00000efu)

typedef struct {
    const char *name;
    const char *expect; /* what a fixed driver does */
} test_case;

static const test_case cases[] = {
    {"read", "success, and capset bytes in the buffer"},
    {"bad-id", "STATUS_INVALID_PARAMETER_1 for capset id 0xFFFFFFC0 + k"},
    {"near-id", "STATUS_INVALID_PARAMETER_1 for capset id 64 + k"},
    {"kernel-address", "a failure status, no write"},
    {"unmapped-address", "a failure status"},
    {"read-only-address", "a failure status, the page unchanged"},
};

typedef struct {
    const char *result, *stage, *name, *expect;
    int selected, completed, pass;
    LONG last_status, escape_status;
    DWORD win32_error;
    uint64_t supported, address;
    uint32_t capset_id, size, written;
    query_device_ids ids;
} result;

static void emit(FILE *f, const result *r)
{
    fprintf(f, "{\"schema\":1,\"probe\":\"capset\",\"case\":\"%s\",\"expect\":\"%s\","
            "\"result\":\"%s\",\"stage\":\"%s\",\"pass\":%s,\"request_completed\":%s,\"escape_status\":",
            r->name ? r->name : "", r->expect ? r->expect : "", r->result, r->stage,
            r->pass ? "true" : "false", r->completed ? "true" : "false");
    if (r->completed) fprintf(f, "\"0x%08lx\"", (unsigned long)r->escape_status);
    else fputs("null", f);
    fprintf(f, ",\"last_ntstatus\":\"0x%08lx\",\"win32_error\":%lu,"
            "\"supported_capset_ids\":\"0x%016llx\",\"capset_id\":\"0x%08x\","
            "\"capset_address\":\"0x%016llx\",\"capset_size\":%u,\"bytes_written\":%u,\"adapter\":",
            (unsigned long)r->last_status, (unsigned long)r->win32_error,
            (unsigned long long)r->supported, r->capset_id,
            (unsigned long long)r->address, r->size, r->written);
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

/* The bytes of the buffer that are not the fill pattern any more, counted to the last one. */
static uint32_t written(const uint8_t *buffer, uint32_t size)
{
    while (size && buffer[size - 1] == FILL) --size;
    return size;
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
        fputs("usage: capset_probe.exe --case (read | bad-id | near-id | kernel-address | "
              "unmapped-address | read-only-address) [--out FILE]\n", stderr);
        r.result = "usage"; exit_code = 3; goto done;
    }
    r.name = cases[which].name;
    r.expect = cases[which].expect;

    HMODULE gdi = LoadLibraryExW(L"gdi32.dll", NULL, LOAD_LIBRARY_SEARCH_SYSTEM32);
    r.stage = "load_gdi32";
    if (!gdi) { r.win32_error = GetLastError(); goto done; }
    enum_fn enumerate = (enum_fn)GetProcAddress(gdi, "D3DKMTEnumAdapters2");
    query_fn query = (query_fn)GetProcAddress(gdi, "D3DKMTQueryAdapterInfo");
    escape_fn escape = (escape_fn)GetProcAddress(gdi, "D3DKMTEscape");
    close_fn close = (close_fn)GetProcAddress(gdi, "D3DKMTCloseAdapter");
    if (!enumerate || !query || !escape || !close) { r.win32_error = ERROR_PROC_NOT_FOUND; FreeLibrary(gdi); goto done; }
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

    uint8_t *buffer = VirtualAlloc(NULL, USER_BUFFER, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    r.stage = "allocate";
    if (!buffer) { r.win32_error = GetLastError(); goto close_adapters; }
    memset(buffer, FILL, USER_BUFFER);

    viogpu_escape request = {0};
    request.type = VIOGPU_GET_CAPS;
    request.data_length = sizeof(request.capset);
    request.capset.capset_id = k;
    request.capset.version = 0;
    request.capset.size = USER_BUFFER;
    request.capset.capset = (uint64_t)(uintptr_t)buffer;
    if (!strcmp(r.name, "bad-id")) request.capset.capset_id = 0xffffffc0u + k;
    else if (!strcmp(r.name, "near-id")) request.capset.capset_id = 64 + k;
    else if (!strcmp(r.name, "kernel-address")) { request.capset.capset = KERNEL_ADDRESS; request.capset.size = 16; }
    else if (!strcmp(r.name, "unmapped-address")) request.capset.capset = 0x10000;
    else if (!strcmp(r.name, "read-only-address")) {
        DWORD old;
        if (!VirtualProtect(buffer, USER_BUFFER, PAGE_READONLY, &old)) { r.win32_error = GetLastError(); goto close_adapters; }
    }
    r.capset_id = request.capset.capset_id;
    r.address = request.capset.capset;
    r.size = request.capset.size;

    /* A driver without the fix can bugcheck here: leave the request on disk first. */
    r.stage = "escape";
    r.result = "requesting";
    if (!save(out, &r)) { r.win32_error = ERROR_WRITE_FAULT; r.result = "api_failure"; goto close_adapters; }
    escape_args e = {0};
    e.adapter = adapter;
    e.type = ESCAPE_DRIVERPRIVATE;
    e.data = &request;
    e.size = sizeof(request);
    r.escape_status = escape(&e);
    r.completed = 1;
    r.last_status = r.escape_status;
    r.written = written(buffer, USER_BUFFER);
    r.result = "completed";
    if (!strcmp(r.name, "read")) r.pass = r.escape_status >= 0 && r.written > 0;
    else if (!strcmp(r.name, "bad-id") || !strcmp(r.name, "near-id"))
        r.pass = r.escape_status == STATUS_INVALID_PARAMETER_1 && r.written == 0;
    else r.pass = r.escape_status < 0 && r.written == 0;
    exit_code = r.pass ? 0 : 2;

close_adapters:
    for (ULONG i = 0; i < adapters.count; ++i) {
        close_adapter c = {adapters.adapters[i].handle};
        close(&c);
    }
    free(adapters.adapters);
    FreeLibrary(gdi);
done:
    if (!save(out, &r)) { r.win32_error = ERROR_WRITE_FAULT; exit_code = 1; }
    emit(stdout, &r);
    return exit_code;
}
