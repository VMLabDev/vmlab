/* Calls D3DKMTCreateAllocation on the viogpu3d kernel driver with the
 * requests of card #757 (docs/reviews/win3d-driver.md F10), as whatever
 * user runs it. One case per run: a driver without the fix can bugcheck the
 * guest. Run it in a disposable VM.
 *
 * The case "starve" is for a guest where Driver Verifier fails the pool
 * allocations of viogpu3d.sys in this program after a delay
 * (tests/kmd/alloc-probe.sh sets that up). It makes its device before the
 * delay is over, then creates and destroys one allocation again and again
 * until allocations fail, and goes on for a while to see that every failure
 * is the same clean status.
 *
 * Exit status: 0 pass, 2 fail, 4 nothing was injected (starve), 1 the probe
 * could not run, 3 usage. */
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
typedef struct { UINT handle; } close_adapter;

/* D3DDDI_ALLOCATIONINFO, the entry of D3DKMTCreateAllocation. */
typedef struct {
    UINT allocation;
    const void *system_mem;
    void *private_data;
    UINT private_data_size;
    UINT vidpn_source;
    UINT flags;
} allocation_info;

/* D3DDDI_ALLOCATIONINFO2, the entry of D3DKMTCreateAllocation2. */
typedef struct {
    UINT allocation;
    const void *system_mem;
    void *private_data;
    UINT private_data_size;
    UINT vidpn_source;
    UINT flags;
    uint64_t gpu_virtual_address;
    ULONG_PTR priority;
    ULONG_PTR reserved[5];
} allocation_info2;

/* D3DKMT_CREATEALLOCATION */
typedef struct {
    UINT device;
    UINT resource;
    UINT global_share;
    const void *runtime_data;
    UINT runtime_data_size;
    const void *private_data;
    UINT private_data_size;
    UINT num_allocations;
    void *allocation_info;
    UINT flags;
    HANDLE runtime_resource;
} create_allocation;

/* D3DKMT_DESTROYALLOCATION */
typedef struct {
    UINT device;
    UINT resource;
    const UINT *allocations;
    UINT count;
} destroy_allocation;

typedef LONG (WINAPI *enum_fn)(const enum_adapters *);
typedef LONG (WINAPI *query_fn)(const query_adapter *);
typedef LONG (WINAPI *create_fn)(create_device *);
typedef LONG (WINAPI *destroy_fn)(const destroy_device *);
typedef LONG (WINAPI *close_fn)(const close_adapter *);
typedef LONG (WINAPI *alloc_fn)(create_allocation *);
typedef LONG (WINAPI *free_fn)(const destroy_allocation *);

/* viogpu/shared/viogpum.h of the driver source. */
#pragma pack(push, 1)
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
#pragma pack(pop)

_Static_assert(sizeof(adapter_info) == 20, "D3DKMT_ADAPTERINFO ABI");
_Static_assert(sizeof(query_device_ids) == 28, "D3DKMT_QUERY_DEVICE_IDS ABI");
_Static_assert(sizeof(create_device) == (sizeof(void *) == 8 ? 64 : 36), "D3DKMT_CREATEDEVICE ABI");
_Static_assert(sizeof(allocation_info) == (sizeof(void *) == 8 ? 40 : 24), "D3DDDI_ALLOCATIONINFO ABI");
_Static_assert(sizeof(allocation_info2) == (sizeof(void *) == 8 ? 96 : 56), "D3DDDI_ALLOCATIONINFO2 ABI");
_Static_assert(sizeof(create_allocation) == (sizeof(void *) == 8 ? 72 : 44), "D3DKMT_CREATEALLOCATION ABI");
_Static_assert(offsetof(create_allocation, num_allocations) == (sizeof(void *) == 8 ? 44 : 28), "D3DKMT_CREATEALLOCATION ABI");
_Static_assert(sizeof(destroy_allocation) == (sizeof(void *) == 8 ? 24 : 16), "D3DKMT_DESTROYALLOCATION ABI");
_Static_assert(sizeof(viogpu_create_allocation_exchange) == 72, "VIOGPU_CREATE_ALLOCATION_EXCHANGE ABI");

enum { QUERY_PHYSICAL_ADAPTER_DEVICE_IDS = 31, MAX_STATUSES = 8, CREATE_RESOURCE = 1 /* D3DKMT_CREATEALLOCATIONFLAGS.CreateResource */ };
#define NT_INSUFFICIENT_RESOURCES ((LONG)0xc000009au)
#define NT_NO_MEMORY ((LONG)0xc0000017u)

typedef struct {
    const char *name;
    UINT count;  /* NumAllocations */
    int version; /* 1: D3DKMTCreateAllocation, 2: D3DKMTCreateAllocation2 */
    const char *expect;
} test_case;

static const test_case cases[] = {
    {"one", 1, 2, "one allocation is created and destroyed"},
    {"one-v1", 1, 1, "one allocation is created and destroyed through D3DKMTCreateAllocation"},
    {"two", 2, 2, "NumAllocations = 2 with a resource fails, or both allocations exist and are destroyed"},
    {"two-v1", 2, 1, "the same through D3DKMTCreateAllocation"},
    {"none", 0, 2, "NumAllocations = 0 with a resource fails, or makes the resource alone"},
    {"starve", 1, 2, "every allocation that fails for want of pool memory fails with STATUS_INSUFFICIENT_RESOURCES or STATUS_NO_MEMORY"},
};

typedef struct {
    const char *result, *stage, *name, *expect;
    int selected, completed, pass;
    LONG last_status, create_status, destroy_status;
    DWORD win32_error;
    UINT count, handles[2], resource;
    /* starve */
    unsigned long attempts, succeeded, failed, failed_in_a_row, seconds;
    unsigned n_statuses;
    LONG statuses[MAX_STATUSES];
    unsigned long status_counts[MAX_STATUSES];
    query_device_ids ids;
} result;

static void emit(FILE *f, const result *r)
{
    fprintf(f, "{\"schema\":1,\"probe\":\"alloc\",\"case\":\"%s\",\"expect\":\"%s\",\"result\":\"%s\",\"stage\":\"%s\","
            "\"pass\":%s,\"request_completed\":%s,\"num_allocations\":%u,\"create_status\":\"0x%08lx\","
            "\"allocation_handles\":[%u,%u],\"resource_handle\":%u,\"destroy_status\":\"0x%08lx\",\"last_ntstatus\":\"0x%08lx\",\"win32_error\":%lu,"
            "\"attempts\":%lu,\"succeeded\":%lu,\"failed\":%lu,\"seconds\":%lu,\"failure_statuses\":{",
            r->name ? r->name : "", r->expect ? r->expect : "", r->result, r->stage,
            r->pass ? "true" : "false", r->completed ? "true" : "false", r->count, (unsigned long)r->create_status,
            r->handles[0], r->handles[1], r->resource, (unsigned long)r->destroy_status, (unsigned long)r->last_status,
            (unsigned long)r->win32_error, r->attempts, r->succeeded, r->failed, r->seconds);
    for (unsigned i = 0; i < r->n_statuses; ++i)
        fprintf(f, "%s\"0x%08lx\":%lu", i ? "," : "", (unsigned long)r->statuses[i], r->status_counts[i]);
    fputs("},\"adapter\":", f);
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

/* A 64 x 64 B8G8R8A8 texture in guest memory, as the user-mode driver asks
 * for one: no blob, so the driver needs no host context for it. */
static void fill_exchange(viogpu_create_allocation_exchange *x)
{
    memset(x, 0, sizeof(*x));
    x->options.target = 2; /* PIPE_TEXTURE_2D */
    x->options.format = 1; /* VIRGL_FORMAT_B8G8R8A8_UNORM */
    x->options.bind = 2;   /* VIRGL_BIND_RENDER_TARGET */
    x->options.width = 64;
    x->options.height = 64;
    x->options.depth = 1;
    x->options.array_size = 1;
    x->size = 64 * 64 * 4;
    x->stride = 64 * 4;
}

typedef struct {
    alloc_fn create[3]; /* by version */
    free_fn destroy[3];
    UINT device;
} kmt;

/* One D3DKMTCreateAllocation call with `count` entries. */
static LONG create(const kmt *k, int version, UINT count, UINT handles[2], UINT *resource)
{
    viogpu_create_allocation_exchange x[2];
    allocation_info v1[2];
    allocation_info2 v2[2];
    create_allocation args;
    memset(v1, 0, sizeof(v1));
    memset(v2, 0, sizeof(v2));
    memset(&args, 0, sizeof(args));
    for (UINT i = 0; i < 2; ++i) {
        fill_exchange(&x[i]);
        v1[i].private_data = v2[i].private_data = &x[i];
        v1[i].private_data_size = v2[i].private_data_size = sizeof(x[i]);
    }
    args.device = k->device;
    args.num_allocations = count;
    args.allocation_info = version == 1 ? (void *)v1 : (void *)v2;
    /* More than one allocation, or none, belongs to a resource. */
    if (count != 1) args.flags = CREATE_RESOURCE;
    LONG status = k->create[version](&args);
    *resource = args.resource;
    for (UINT i = 0; i < 2; ++i) handles[i] = version == 1 ? v1[i].allocation : v2[i].allocation;
    return status;
}

/* With a resource, the resource and every allocation of it. */
static LONG destroy(const kmt *k, int version, const UINT *handles, UINT count, UINT resource)
{
    destroy_allocation args = {k->device, resource, resource ? NULL : handles, resource ? 0 : count};
    return k->destroy[version](&args);
}

static void count_status(result *r, LONG status)
{
    unsigned i = 0;
    while (i < r->n_statuses && r->statuses[i] != status) ++i;
    if (i == MAX_STATUSES) i = MAX_STATUSES - 1;
    if (i == r->n_statuses) { r->statuses[i] = status; r->status_counts[i] = 0; ++r->n_statuses; }
    ++r->status_counts[i];
}

int wmain(int argc, wchar_t **argv)
{
    result r = {0};
    r.result = "api_failure";
    r.stage = "arguments";
    const wchar_t *out = NULL;
    int which = -1, exit_code = 1;
    unsigned long seconds = 600, after_failures = 200;
    for (int i = 1; i < argc; ++i) {
        if (!wcscmp(argv[i], L"--out") && i + 1 < argc) out = argv[++i];
        else if (!wcscmp(argv[i], L"--seconds") && i + 1 < argc) seconds = wcstoul(argv[++i], NULL, 10);
        else if (!wcscmp(argv[i], L"--case") && i + 1 < argc) {
            ++i;
            for (int c = 0; c < (int)(sizeof(cases) / sizeof(cases[0])); ++c) {
                wchar_t wide[32];
                mbstowcs(wide, cases[c].name, 32);
                if (!wcscmp(argv[i], wide)) which = c;
            }
            if (which < 0) break;
        } else { which = -1; break; }
    }
    if (which < 0) {
        fputs("usage: alloc_probe.exe --case (one | one-v1 | two | two-v1 | none | starve) [--seconds N] [--out FILE]\n", stderr);
        r.result = "usage"; exit_code = 3; goto done;
    }
    const test_case *tc = &cases[which];
    r.name = tc->name;
    r.expect = tc->expect;
    r.count = tc->count;

    HMODULE gdi = LoadLibraryExW(L"gdi32.dll", NULL, LOAD_LIBRARY_SEARCH_SYSTEM32);
    r.stage = "load_gdi32";
    if (!gdi) { r.win32_error = GetLastError(); goto done; }
    enum_fn enumerate = (enum_fn)GetProcAddress(gdi, "D3DKMTEnumAdapters2");
    query_fn query = (query_fn)GetProcAddress(gdi, "D3DKMTQueryAdapterInfo");
    create_fn create_dev = (create_fn)GetProcAddress(gdi, "D3DKMTCreateDevice");
    destroy_fn destroy_dev = (destroy_fn)GetProcAddress(gdi, "D3DKMTDestroyDevice");
    close_fn close = (close_fn)GetProcAddress(gdi, "D3DKMTCloseAdapter");
    kmt k = {0};
    k.create[1] = (alloc_fn)GetProcAddress(gdi, "D3DKMTCreateAllocation");
    k.create[2] = (alloc_fn)GetProcAddress(gdi, "D3DKMTCreateAllocation2");
    k.destroy[1] = (free_fn)GetProcAddress(gdi, "D3DKMTDestroyAllocation");
    k.destroy[2] = (free_fn)GetProcAddress(gdi, "D3DKMTDestroyAllocation");
    if (!enumerate || !query || !create_dev || !destroy_dev || !close || !k.create[1] || !k.create[2] || !k.destroy[1]) {
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

    create_device device = {0};
    device.adapter = adapter;
    r.stage = "create_device";
    r.last_status = create_dev(&device);
    if (r.last_status < 0) goto close_adapters;
    k.device = device.device;

    /* A driver without the fix may not come back: the file then says so. */
    r.stage = "create_allocation";
    r.result = "request_did_not_return";
    if (!save(out, &r)) { r.result = "api_failure"; r.stage = "write_pending_result"; r.win32_error = ERROR_WRITE_FAULT; goto destroy_device; }

    if (strcmp(tc->name, "starve")) {
        r.create_status = r.last_status = create(&k, tc->version, tc->count, r.handles, &r.resource);
        r.completed = 1;
        if (r.create_status >= 0) {
            /* The call succeeded: every entry has a handle, and the handles can be destroyed. */
            UINT made = 0;
            for (UINT i = 0; i < tc->count && i < 2; ++i) made += r.handles[i] != 0;
            r.stage = "destroy_allocation";
            save(out, &r);
            if (made || r.resource) r.destroy_status = r.last_status = destroy(&k, tc->version, r.handles, made, r.resource);
            r.pass = made == tc->count && r.destroy_status >= 0;
            r.result = r.pass ? "pass" : "allocations_missing";
        } else {
            /* A clean failure passes where the case allows one. */
            r.pass = tc->count != 1;
            r.result = r.pass ? "pass" : "create_failed";
        }
        r.stage = "done";
        exit_code = r.pass ? 0 : 2;
    } else {
        const ULONGLONG start = GetTickCount64();
        ULONGLONG saved = start;
        int bad = 0;
        r.result = "starving";
        while (r.failed < after_failures) {
            UINT handles[2] = {0, 0}, resource = 0;
            const ULONGLONG now = GetTickCount64();
            r.seconds = (unsigned long)((now - start) / 1000);
            if (r.seconds >= seconds) break;
            if (now - saved >= 2000 || r.failed == 1) { save(out, &r); saved = now; }
            ++r.attempts;
            r.create_status = r.last_status = create(&k, 2, 1, handles, &resource);
            if (r.create_status >= 0) {
                ++r.succeeded;
                r.failed_in_a_row = 0;
                r.destroy_status = destroy(&k, 2, handles, 1, 0);
                if (r.destroy_status < 0) { bad = 1; break; }
                Sleep(r.failed ? 5 : 100);
            } else {
                ++r.failed;
                ++r.failed_in_a_row;
                count_status(&r, r.create_status);
                if (r.create_status != NT_INSUFFICIENT_RESOURCES && r.create_status != NT_NO_MEMORY) bad = 1;
                Sleep(5);
            }
        }
        r.completed = 1;
        r.stage = "done";
        if (bad) { r.result = r.destroy_status < 0 ? "destroy_failed" : "wrong_status"; exit_code = 2; }
        else if (!r.failed) { r.result = "nothing_injected"; exit_code = 4; }
        else { r.pass = 1; r.result = "pass"; exit_code = 0; }
    }

destroy_device:
    {
        destroy_device d = {device.device};
        destroy_dev(&d);
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
