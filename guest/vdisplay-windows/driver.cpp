// SPDX-License-Identifier: MS-PL
// IddCx and D3D lifecycle follows Microsoft's MS-PL-licensed IddSampleDriver.
// Copyright (c) Microsoft Corporation. See LICENSE-Microsoft.txt.
#define NOMINMAX
#define WIN32_NO_STATUS
#include <windows.h>
#undef WIN32_NO_STATUS
#ifdef _MSC_VER
#pragma warning(push)
#pragma warning(disable:4324)
#endif
#include <bugcodes.h>
#include <wudfwdm.h>
#include <wdf.h>
#include <IddCx.h>
#ifdef _MSC_VER
#pragma warning(pop)
#endif
#include <dxgi1_5.h>
#include <d3d11.h>
#include <avrt.h>
#include <stdio.h>
#include "protocol.h"
#include "ivshmem.h"

using vdisplay::Mode;

struct Device;
struct Monitor {
    Device* device;
    IDDCX_MONITOR object;
    IDDCX_SWAPCHAIN chain;
    LUID render_adapter;
    HANDLE available;
    HANDLE stop;
    HANDLE thread;
    HANDLE cursor_event;
    SRWLOCK worker_lock;
    uint32_t shape_id;
    LONG active;
    uint8_t cursor[vdisplay::cursor_bytes];
};

struct Device {
    WDFDEVICE object;
    IDDCX_ADAPTER adapter;
    IDDCX_MONITOR monitor;
    WDFTIMER timer;
    HANDLE transport;
    uint8_t* memory;
    vdisplay::Layout layout;
    vdisplay::Request requested;
    uint8_t edid[128];
    LONG powered;
    SRWLOCK publish_lock;
};

struct DeviceContext { Device* value; };
struct MonitorContext { Monitor* value; };
#ifdef __clang__
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wduplicate-decl-specifier"
#endif
WDF_DECLARE_CONTEXT_TYPE(DeviceContext);
WDF_DECLARE_CONTEXT_TYPE(MonitorContext);
#ifdef __clang__
#pragma clang diagnostic pop
#endif

EVT_WDF_DRIVER_DEVICE_ADD DeviceAdd;
EVT_WDF_DEVICE_D0_ENTRY PowerOn;
EVT_WDF_DEVICE_D0_EXIT PowerOff;
EVT_IDD_CX_ADAPTER_INIT_FINISHED AdapterReady;
EVT_IDD_CX_ADAPTER_COMMIT_MODES CommitModes;
EVT_IDD_CX_PARSE_MONITOR_DESCRIPTION ParseDescription;
EVT_IDD_CX_MONITOR_GET_DEFAULT_DESCRIPTION_MODES DefaultModes;
EVT_IDD_CX_MONITOR_QUERY_TARGET_MODES TargetModes;
EVT_IDD_CX_MONITOR_ASSIGN_SWAPCHAIN Assign;
EVT_IDD_CX_MONITOR_UNASSIGN_SWAPCHAIN Unassign;
EVT_WDF_TIMER Tick;
static NTSTATUS initialize_adapter(Device* device);

static void diagnostic(const wchar_t* stage, NTSTATUS status) {
    HANDLE source = RegisterEventSourceW(nullptr, L"VMLabDisplay");
    if (!source) return;
    wchar_t message[160];
    swprintf_s(message, L"%s: NTSTATUS 0x%08lx", stage, ULONG(status));
    const wchar_t* strings[] = {message};
    ReportEventW(source, NT_SUCCESS(status) ? EVENTLOG_INFORMATION_TYPE : EVENTLOG_ERROR_TYPE,
                 0, 1, nullptr, 1, 0, strings, nullptr);
    DeregisterEventSource(source);
}

template <typename T> static void release(T*& object) {
    if (object) { object->Release(); object = nullptr; }
}

static uint64_t monotonic_ns() {
    LARGE_INTEGER counter, frequency;
    QueryPerformanceCounter(&counter);
    QueryPerformanceFrequency(&frequency);
    return (uint64_t(counter.QuadPart) / frequency.QuadPart) * 1000000000 +
           (uint64_t(counter.QuadPart) % frequency.QuadPart) * 1000000000 / frequency.QuadPart;
}

static void hide_cursor(Device* device) {
    vdisplay::write32(device->memory, 0x20c, 0);
    vdisplay::release_fence();
    vdisplay::write32(device->memory, 0x200, vdisplay::read32(device->memory, 0x200) + 1);
}

static bool open_transport(Device* device) {
    ULONG length = 0;
    if (CM_Get_Device_Interface_List_SizeW(&length, const_cast<GUID*>(&ivshmem::interface_id),
                                          nullptr, CM_GET_DEVICE_INTERFACE_LIST_PRESENT) != CR_SUCCESS || length < 2) return false;
    auto paths = static_cast<wchar_t*>(HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, SIZE_T(length) * sizeof(wchar_t)));
    if (!paths) return false;
    bool found = false;
    if (CM_Get_Device_Interface_ListW(const_cast<GUID*>(&ivshmem::interface_id), nullptr, paths,
                                      length, CM_GET_DEVICE_INTERFACE_LIST_PRESENT) == CR_SUCCESS) {
        for (const wchar_t* path = paths; *path; path += wcslen(path) + 1) {
            HANDLE handle = CreateFileW(path, GENERIC_READ | GENERIC_WRITE,
                                        FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr, OPEN_EXISTING, 0, nullptr);
            if (handle == INVALID_HANDLE_VALUE) continue;
            UINT64 size = 0;
            DWORD returned = 0;
            ivshmem::MapConfig config = {2}; // Write-combined, ordered with an explicit release fence.
            ivshmem::Mapping mapping = {};
            bool mapped = DeviceIoControl(handle, ivshmem::size_ioctl, nullptr, 0, &size, sizeof(size), &returned, nullptr) &&
                          returned == sizeof(size) &&
                          DeviceIoControl(handle, ivshmem::map_ioctl, &config, sizeof(config), &mapping, sizeof(mapping), &returned, nullptr) &&
                          returned == sizeof(mapping);
            if (mapped && mapping.memory && mapping.size == size &&
                vdisplay::validate(static_cast<uint8_t*>(mapping.memory), size, device->layout)) {
                device->transport = handle;
                device->memory = static_cast<uint8_t*>(mapping.memory);
                found = true;
                break;
            }
            // Closing the owner handle also releases a mapping rejected by validation.
            CloseHandle(handle);
        }
    }
    HeapFree(GetProcessHeap(), 0, paths);
    return found;
}

static DISPLAYCONFIG_VIDEO_SIGNAL_INFO signal(Mode mode, bool monitor) {
    DISPLAYCONFIG_VIDEO_SIGNAL_INFO result = {};
    result.activeSize = {mode.width, mode.height};
    result.totalSize = result.activeSize;
    result.AdditionalSignalInfo.vSyncFreqDivider = monitor ? 0 : 1;
    result.AdditionalSignalInfo.videoStandard = 255;
    result.vSyncFreq = {60, 1};
    result.hSyncFreq = {60 * mode.height, 1};
    result.pixelRate = uint64_t(mode.width) * mode.height * 60;
    result.scanLineOrdering = DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
    return result;
}

static NTSTATUS fill_monitor_modes(const vdisplay::Layout& layout, Mode requested,
                                   UINT capacity, IDDCX_MONITOR_MODE* output, UINT& count, UINT& preferred) {
    Mode list[12];
    count = vdisplay::modes(layout, requested, list);
    preferred = 0;
    if (!capacity) return STATUS_SUCCESS;
    if (capacity < count) return STATUS_BUFFER_TOO_SMALL;
    if (!output) return STATUS_INVALID_PARAMETER;
    for (UINT i = 0; i < count; ++i) {
        output[i] = {};
        output[i].Size = sizeof(output[i]);
        output[i].Origin = IDDCX_MONITOR_MODE_ORIGIN_DRIVER;
        output[i].MonitorVideoSignalInfo = signal(list[i], true);
    }
    return STATUS_SUCCESS;
}

static void stop_worker(Monitor* monitor) {
    AcquireSRWLockExclusive(&monitor->worker_lock);
    if (monitor->stop) SetEvent(monitor->stop);
    if (monitor->thread) {
        WaitForSingleObject(monitor->thread, INFINITE);
        CloseHandle(monitor->thread);
        monitor->thread = nullptr;
    }
    if (monitor->stop) { CloseHandle(monitor->stop); monitor->stop = nullptr; }
    ReleaseSRWLockExclusive(&monitor->worker_lock);
}

static DWORD WINAPI process_frames(void* argument) {
    Monitor* monitor = static_cast<Monitor*>(argument);
    Device* owner = monitor->device;
    IDXGIFactory5* factory = nullptr;
    IDXGIAdapter1* adapter = nullptr;
    ID3D11Device* device = nullptr;
    ID3D11DeviceContext* context = nullptr;
    IDXGIDevice* dxgi = nullptr;
    ID3D11Texture2D* staging = nullptr;
    D3D11_TEXTURE2D_DESC staged = {};
    DWORD task_index = 0;
    HANDLE task = AvSetMmThreadCharacteristicsW(L"Distribution", &task_index);
    HRESULT result = CreateDXGIFactory2(0, IID_PPV_ARGS(&factory));
    if (SUCCEEDED(result)) result = factory->EnumAdapterByLuid(monitor->render_adapter, IID_PPV_ARGS(&adapter));
    if (SUCCEEDED(result)) result = D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0, D3D11_SDK_VERSION, &device, nullptr, &context);
    if (SUCCEEDED(result)) result = device->QueryInterface(IID_PPV_ARGS(&dxgi));
    if (SUCCEEDED(result)) {
        IDARG_IN_SWAPCHAINSETDEVICE input = {};
        input.pDevice = dxgi;
        result = IddCxSwapChainSetDevice(monitor->chain, &input);
    }
    while (SUCCEEDED(result) && WaitForSingleObject(monitor->stop, 0) == WAIT_TIMEOUT) {
        IDARG_OUT_RELEASEANDACQUIREBUFFER buffer = {};
        result = IddCxSwapChainReleaseAndAcquireBuffer(monitor->chain, &buffer);
        if (result == E_PENDING) {
            HANDLE events[] = {monitor->stop, monitor->available};
            DWORD waited = WaitForMultipleObjects(2, events, FALSE, 100);
            if (waited != WAIT_TIMEOUT && waited != WAIT_OBJECT_0 + 1) break;
            result = S_OK;
            continue;
        }
        if (FAILED(result)) break;
        IDXGIResource* surface = buffer.MetaData.pSurface;
        ID3D11Texture2D* texture = nullptr;
        result = surface->QueryInterface(IID_PPV_ARGS(&texture));
        if (SUCCEEDED(result)) {
            D3D11_TEXTURE2D_DESC description;
            texture->GetDesc(&description);
            if (description.Format != DXGI_FORMAT_B8G8R8A8_UNORM || description.SampleDesc.Count != 1 ||
                !owner->layout.supports({description.Width, description.Height})) result = E_INVALIDARG;
            if (SUCCEEDED(result) && (!staging || staged.Width != description.Width || staged.Height != description.Height)) {
                release(staging);
                staged = description;
                staged.Usage = D3D11_USAGE_STAGING;
                staged.BindFlags = 0;
                staged.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
                staged.MiscFlags = 0;
                result = device->CreateTexture2D(&staged, nullptr, &staging);
            }
            if (SUCCEEDED(result)) {
                context->CopyResource(staging, texture);
                D3D11_MAPPED_SUBRESOURCE mapped = {};
                result = context->Map(staging, 0, D3D11_MAP_READ, 0, &mapped);
                if (SUCCEEDED(result)) {
                    AcquireSRWLockExclusive(&owner->publish_lock);
                    if (InterlockedCompareExchange(&monitor->active, 0, 0) &&
                        vdisplay::publish(owner->memory, owner->layout, static_cast<uint8_t*>(mapped.pData),
                                          {description.Width, description.Height}, mapped.RowPitch)) {
                        vdisplay::write64(owner->memory, 0x108, monotonic_ns());
                        vdisplay::write32(owner->memory, 0x100, 1);
                        vdisplay::Request request;
                        if (vdisplay::request(owner->memory, owner->layout, request) &&
                            vdisplay::equal(vdisplay::preferred(owner->layout, request.mode), {description.Width, description.Height})) {
                            vdisplay::write32(owner->memory, 0x138, request.seq);
                        }
                        if (!ivshmem::ring(owner->transport)) result = HRESULT_FROM_WIN32(GetLastError());
                    }
                    ReleaseSRWLockExclusive(&owner->publish_lock);
                    context->Unmap(staging, 0);
                }
            }
        }
        release(texture);
        release(surface);
        if (FAILED(result)) break;
        result = IddCxSwapChainFinishedProcessingFrame(monitor->chain);
    }
    release(staging);
    release(dxgi);
    release(context);
    release(device);
    release(adapter);
    release(factory);
    if (task) AvRevertMmThreadCharacteristics(task);
    AcquireSRWLockExclusive(&owner->publish_lock);
    vdisplay::write32(owner->memory, 0x100, 2);
    ivshmem::ring(owner->transport);
    ReleaseSRWLockExclusive(&owner->publish_lock);
    WdfObjectDelete(reinterpret_cast<WDFOBJECT>(monitor->chain));
    monitor->chain = nullptr;
    return 0;
}

static NTSTATUS arrive(Device* device) {
    vdisplay::edid(device->layout, device->requested.mode, device->edid);
    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, MonitorContext);
    attributes.EvtCleanupCallback = [](WDFOBJECT object) {
        Monitor* monitor = WdfObjectGet_MonitorContext(object)->value;
        if (!monitor) return;
        stop_worker(monitor);
        if (monitor->cursor_event) CloseHandle(monitor->cursor_event);
        HeapFree(GetProcessHeap(), 0, monitor);
    };
    IDDCX_MONITOR_INFO info = {};
    info.Size = sizeof(info);
    info.MonitorType = DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI;
    info.MonitorDescription.Size = sizeof(info.MonitorDescription);
    info.MonitorDescription.Type = IDDCX_MONITOR_DESCRIPTION_TYPE_EDID;
    info.MonitorDescription.DataSize = sizeof(device->edid);
    info.MonitorDescription.pData = device->edid;
    info.MonitorContainerId = {0xb8e77057, 0x8146, 0x495f, {0x83, 0x37, 0x91, 0x68, 0x50, 0x3c, 0x6a, 0x11}};
    IDARG_IN_MONITORCREATE input = {};
    input.ObjectAttributes = &attributes;
    input.pMonitorInfo = &info;
    IDARG_OUT_MONITORCREATE output = {};
    NTSTATUS status = IddCxMonitorCreate(device->adapter, &input, &output);
    if (!NT_SUCCESS(status)) return status;
    auto monitor = static_cast<Monitor*>(HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, sizeof(Monitor)));
    if (!monitor) { WdfObjectDelete(reinterpret_cast<WDFOBJECT>(output.MonitorObject)); return STATUS_INSUFFICIENT_RESOURCES; }
    monitor->device = device;
    monitor->object = output.MonitorObject;
    monitor->cursor_event = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    WdfObjectGet_MonitorContext(output.MonitorObject)->value = monitor;
    if (!monitor->cursor_event) { WdfObjectDelete(reinterpret_cast<WDFOBJECT>(output.MonitorObject)); return STATUS_INSUFFICIENT_RESOURCES; }
    IDARG_OUT_MONITORARRIVAL arrival = {};
    status = IddCxMonitorArrival(output.MonitorObject, &arrival);
    if (NT_SUCCESS(status)) device->monitor = output.MonitorObject;
    else WdfObjectDelete(reinterpret_cast<WDFOBJECT>(output.MonitorObject));
    return status;
}

_Use_decl_annotations_
void Tick(WDFTIMER timer) {
    Device* device = WdfObjectGet_DeviceContext(WdfTimerGetParentObject(timer))->value;
    if (!InterlockedCompareExchange(&device->powered, 0, 0)) return;
    WdfTimerStart(timer, WDF_REL_TIMEOUT_IN_MS(100));
    if (!device->memory) {
        if (!open_transport(device)) return;
        vdisplay::write32(device->memory, 0x104, 1);
        vdisplay::request(device->memory, device->layout, device->requested);
    }
    if (!device->adapter) {
        initialize_adapter(device);
        return;
    }
    vdisplay::write64(device->memory, 0x108, monotonic_ns());
    vdisplay::Request request;
    if (vdisplay::request(device->memory, device->layout, request) && request.seq != device->requested.seq) {
        if (device->monitor) {
            Monitor* monitor = WdfObjectGet_MonitorContext(device->monitor)->value;
            stop_worker(monitor);
            IddCxMonitorDeparture(device->monitor);
            device->monitor = nullptr;
        }
        device->requested = request;
        arrive(device);
    }
    if (!device->monitor) return;
    Monitor* monitor = WdfObjectGet_MonitorContext(device->monitor)->value;
    if (!InterlockedCompareExchange(&monitor->active, 0, 0) ||
        WaitForSingleObject(monitor->cursor_event, 0) != WAIT_OBJECT_0) return;
    IDARG_IN_QUERY_HWCURSOR input = {};
    input.LastShapeId = monitor->shape_id;
    input.ShapeBufferSizeInBytes = sizeof(monitor->cursor);
    input.pShapeBuffer = monitor->cursor;
    IDARG_OUT_QUERY_HWCURSOR output = {};
    if (!NT_SUCCESS(IddCxMonitorQueryHardwareCursor(device->monitor, &input, &output))) return;
    const auto& shape = output.CursorShapeInfo;
    if (output.IsCursorShapeUpdated) {
        if (shape.CursorType != IDDCX_CURSOR_SHAPE_TYPE_ALPHA || shape.Width > 256 || shape.Height > 256 ||
            !shape.Width || !shape.Height || shape.Pitch < shape.Width * 4 ||
            uint64_t(shape.Pitch) * shape.Height > sizeof(monitor->cursor) ||
            shape.XHot >= shape.Width || shape.YHot >= shape.Height) return;
        for (UINT y = 0; y < shape.Height; ++y) {
            memcpy(device->memory + 0x1000 + y * shape.Width * 4, monitor->cursor + y * shape.Pitch, shape.Width * 4);
        }
        vdisplay::write32(device->memory, 0x214, shape.Width);
        vdisplay::write32(device->memory, 0x218, shape.Height);
        vdisplay::write32(device->memory, 0x21c, shape.XHot);
        vdisplay::write32(device->memory, 0x220, shape.YHot);
        monitor->shape_id = shape.ShapeId;
        vdisplay::write32(device->memory, 0x210, vdisplay::read32(device->memory, 0x210) + 1);
    }
    // IddCx returns top-left coordinates. C2 requires hotspot coordinates.
    vdisplay::write32(device->memory, 0x204, uint32_t(int64_t(output.X) + vdisplay::read32(device->memory, 0x21c)));
    vdisplay::write32(device->memory, 0x208, uint32_t(int64_t(output.Y) + vdisplay::read32(device->memory, 0x220)));
    vdisplay::write32(device->memory, 0x20c, output.IsCursorVisible ? 1 : 0);
    vdisplay::release_fence();
    vdisplay::write32(device->memory, 0x200, vdisplay::read32(device->memory, 0x200) + 1);
    ivshmem::ring(device->transport);
}

_Use_decl_annotations_
NTSTATUS ParseDescription(const IDARG_IN_PARSEMONITORDESCRIPTION* input, IDARG_OUT_PARSEMONITORDESCRIPTION* output) {
    vdisplay::Layout layout;
    Mode requested;
    if (!input->MonitorDescription.pData || !vdisplay::parse_edid(static_cast<const uint8_t*>(input->MonitorDescription.pData),
        input->MonitorDescription.DataSize, layout, requested)) return STATUS_INVALID_PARAMETER;
    return fill_monitor_modes(layout, requested, input->MonitorModeBufferInputCount, input->pMonitorModes,
                               output->MonitorModeBufferOutputCount, output->PreferredMonitorModeIdx);
}

_Use_decl_annotations_
NTSTATUS DefaultModes(IDDCX_MONITOR object, const IDARG_IN_GETDEFAULTDESCRIPTIONMODES* input, IDARG_OUT_GETDEFAULTDESCRIPTIONMODES* output) {
    Device* device = WdfObjectGet_MonitorContext(object)->value->device;
    return fill_monitor_modes(device->layout, device->requested.mode, input->DefaultMonitorModeBufferInputCount,
        input->pDefaultMonitorModes, output->DefaultMonitorModeBufferOutputCount, output->PreferredMonitorModeIdx);
}

_Use_decl_annotations_
NTSTATUS TargetModes(IDDCX_MONITOR object, const IDARG_IN_QUERYTARGETMODES* input, IDARG_OUT_QUERYTARGETMODES* output) {
    Device* device = WdfObjectGet_MonitorContext(object)->value->device;
    Mode list[12];
    UINT count = vdisplay::modes(device->layout, device->requested.mode, list);
    output->TargetModeBufferOutputCount = count;
    if (!input->TargetModeBufferInputCount) return STATUS_SUCCESS;
    if (input->TargetModeBufferInputCount < count) return STATUS_BUFFER_TOO_SMALL;
    if (!input->pTargetModes) return STATUS_INVALID_PARAMETER;
    for (UINT i = 0; i < count; ++i) {
        input->pTargetModes[i] = {};
        input->pTargetModes[i].Size = sizeof(IDDCX_TARGET_MODE);
        input->pTargetModes[i].TargetVideoSignalInfo.targetVideoSignalInfo = signal(list[i], false);
    }
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS CommitModes(IDDCX_ADAPTER object, const IDARG_IN_COMMITMODES* input) {
    Device* device = WdfObjectGet_DeviceContext(object)->value;
    for (UINT i = 0; i < input->PathCount; ++i) {
        const auto& path = input->pPaths[i];
        Monitor* monitor = WdfObjectGet_MonitorContext(path.MonitorObject)->value;
        bool active = (path.Flags & IDDCX_PATH_FLAGS_ACTIVE) != 0;
        InterlockedExchange(&monitor->active, active ? 1 : 0);
        if (active) {
            IDARG_IN_SETUP_HWCURSOR cursor = {};
            cursor.CursorInfo.Size = sizeof(cursor.CursorInfo);
            cursor.CursorInfo.AlphaCursorSupport = TRUE;
            cursor.CursorInfo.ColorXorCursorSupport = IDDCX_XOR_CURSOR_SUPPORT_EMULATION;
            cursor.CursorInfo.MaxX = 256;
            cursor.CursorInfo.MaxY = 256;
            cursor.hNewCursorDataAvailable = monitor->cursor_event;
            NTSTATUS status = IddCxMonitorSetupHardwareCursor(path.MonitorObject, &cursor);
            if (!NT_SUCCESS(status)) return status;
            SetEvent(monitor->cursor_event);
        } else {
            AcquireSRWLockExclusive(&device->publish_lock);
            vdisplay::write32(device->memory, 0x100, 2);
            hide_cursor(device);
            ivshmem::ring(device->transport);
            ReleaseSRWLockExclusive(&device->publish_lock);
        }
    }
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS Assign(IDDCX_MONITOR object, const IDARG_IN_SETSWAPCHAIN* input) {
    Monitor* monitor = WdfObjectGet_MonitorContext(object)->value;
    stop_worker(monitor);
    AcquireSRWLockExclusive(&monitor->worker_lock);
    monitor->chain = input->hSwapChain;
    monitor->render_adapter = input->RenderAdapterLuid;
    monitor->available = input->hNextSurfaceAvailable;
    monitor->stop = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    if (monitor->stop) monitor->thread = CreateThread(nullptr, 0, process_frames, monitor, 0, nullptr);
    if (!monitor->thread) {
        if (monitor->stop) { CloseHandle(monitor->stop); monitor->stop = nullptr; }
        WdfObjectDelete(reinterpret_cast<WDFOBJECT>(monitor->chain));
        monitor->chain = nullptr;
    }
    ReleaseSRWLockExclusive(&monitor->worker_lock);
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS Unassign(IDDCX_MONITOR object) {
    stop_worker(WdfObjectGet_MonitorContext(object)->value);
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS AdapterReady(IDDCX_ADAPTER object, const IDARG_IN_ADAPTER_INIT_FINISHED* input) {
    if (!NT_SUCCESS(input->AdapterInitStatus)) return input->AdapterInitStatus;
    Device* device = WdfObjectGet_DeviceContext(object)->value;
    NTSTATUS status = arrive(device);
    if (NT_SUCCESS(status)) WdfTimerStart(device->timer, WDF_REL_TIMEOUT_IN_MS(100));
    return status;
}

_Use_decl_annotations_
NTSTATUS PowerOn(WDFDEVICE object, WDF_POWER_DEVICE_STATE) {
    Device* device = WdfObjectGet_DeviceContext(object)->value;
    InterlockedExchange(&device->powered, 1);
    if (!device->memory && !open_transport(device)) {
        // The independent PCI driver may start later during boot.
        WdfTimerStart(device->timer, WDF_REL_TIMEOUT_IN_MS(100));
        return STATUS_SUCCESS;
    }
    vdisplay::write32(device->memory, 0x104, 1);
    vdisplay::request(device->memory, device->layout, device->requested);
    if (device->adapter) {
        WdfTimerStart(device->timer, WDF_REL_TIMEOUT_IN_MS(100));
        return STATUS_SUCCESS;
    }
    return initialize_adapter(device);
}

static NTSTATUS initialize_adapter(Device* device) {
    IDDCX_ADAPTER_CAPS caps = {};
    caps.Size = sizeof(caps);
    caps.MaxMonitorsSupported = 1;
    caps.EndPointDiagnostics.Size = sizeof(caps.EndPointDiagnostics);
    caps.EndPointDiagnostics.GammaSupport = IDDCX_FEATURE_IMPLEMENTATION_NONE;
    caps.EndPointDiagnostics.TransmissionType = IDDCX_TRANSMISSION_TYPE_WIRED_OTHER;
    caps.EndPointDiagnostics.pEndPointFriendlyName = L"VMLab shared-memory display";
    caps.EndPointDiagnostics.pEndPointManufacturerName = L"VMLab";
    caps.EndPointDiagnostics.pEndPointModelName = L"VMLABDSP v1";
    IDDCX_ENDPOINT_VERSION version = {};
    version.Size = sizeof(version);
    version.MajorVer = 1;
    caps.EndPointDiagnostics.pFirmwareVersion = &version;
    caps.EndPointDiagnostics.pHardwareVersion = &version;
    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, DeviceContext);
    IDARG_IN_ADAPTER_INIT input = {};
    input.WdfDevice = device->object;
    input.pCaps = &caps;
    input.ObjectAttributes = &attributes;
    IDARG_OUT_ADAPTER_INIT output = {};
    NTSTATUS status = IddCxAdapterInitAsync(&input, &output);
    if (NT_SUCCESS(status)) {
        device->adapter = output.AdapterObject;
        WdfObjectGet_DeviceContext(output.AdapterObject)->value = device;
    }
    return status;
}

_Use_decl_annotations_
NTSTATUS PowerOff(WDFDEVICE object, WDF_POWER_DEVICE_STATE) {
    Device* device = WdfObjectGet_DeviceContext(object)->value;
    InterlockedExchange(&device->powered, 0);
    WdfTimerStop(device->timer, TRUE);
    if (device->monitor) stop_worker(WdfObjectGet_MonitorContext(device->monitor)->value);
    if (device->memory) {
        AcquireSRWLockExclusive(&device->publish_lock);
        vdisplay::write32(device->memory, 0x100, 0);
        hide_cursor(device);
        ivshmem::ring(device->transport);
        ReleaseSRWLockExclusive(&device->publish_lock);
    }
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS DeviceAdd(WDFDRIVER, PWDFDEVICE_INIT init) {
    WDF_PNPPOWER_EVENT_CALLBACKS power;
    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&power);
    power.EvtDeviceD0Entry = PowerOn;
    power.EvtDeviceD0Exit = PowerOff;
    WdfDeviceInitSetPnpPowerEventCallbacks(init, &power);
    IDD_CX_CLIENT_CONFIG config;
    IDD_CX_CLIENT_CONFIG_INIT(&config);
    config.EvtIddCxAdapterInitFinished = AdapterReady;
    config.EvtIddCxAdapterCommitModes = CommitModes;
    config.EvtIddCxParseMonitorDescription = ParseDescription;
    config.EvtIddCxMonitorGetDefaultDescriptionModes = DefaultModes;
    config.EvtIddCxMonitorQueryTargetModes = TargetModes;
    config.EvtIddCxMonitorAssignSwapChain = Assign;
    config.EvtIddCxMonitorUnassignSwapChain = Unassign;
    NTSTATUS status = IddCxDeviceInitConfig(init, &config);
    diagnostic(L"IddCxDeviceInitConfig", status);
    if (!NT_SUCCESS(status)) return status;
    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, DeviceContext);
    attributes.EvtCleanupCallback = [](WDFOBJECT object) {
        Device* device = WdfObjectGet_DeviceContext(object)->value;
        if (!device) return;
        if (device->timer) WdfTimerStop(device->timer, TRUE);
        if (device->transport) CloseHandle(device->transport);
        HeapFree(GetProcessHeap(), 0, device);
    };
    attributes.ExecutionLevel = WdfExecutionLevelPassive;
    WDFDEVICE object = nullptr;
    status = WdfDeviceCreate(&init, &attributes, &object);
    diagnostic(L"WdfDeviceCreate", status);
    if (!NT_SUCCESS(status)) return status;
    auto device = static_cast<Device*>(HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, sizeof(Device)));
    if (!device) return STATUS_INSUFFICIENT_RESOURCES;
    device->object = object;
    WdfObjectGet_DeviceContext(object)->value = device;
    WDF_TIMER_CONFIG timer_config;
    WDF_TIMER_CONFIG_INIT(&timer_config, Tick);
    timer_config.AutomaticSerialization = TRUE;
    WDF_OBJECT_ATTRIBUTES timer_attributes;
    WDF_OBJECT_ATTRIBUTES_INIT(&timer_attributes);
    timer_attributes.ParentObject = object;
    timer_attributes.ExecutionLevel = WdfExecutionLevelPassive;
    status = WdfTimerCreate(&timer_config, &timer_attributes, &device->timer);
    diagnostic(L"WdfTimerCreate", status);
    if (!NT_SUCCESS(status)) return status;
    status = IddCxDeviceInitialize(object);
    diagnostic(L"IddCxDeviceInitialize", status);
    return status;
}

extern "C" BOOL WINAPI DllMain(HINSTANCE, DWORD, void*) { return TRUE; }

extern "C" DRIVER_INITIALIZE DriverEntry;
_Use_decl_annotations_
extern "C" NTSTATUS DriverEntry(PDRIVER_OBJECT object, PUNICODE_STRING registry) {
    diagnostic(L"DriverEntry", STATUS_SUCCESS);
    WDF_DRIVER_CONFIG config;
    WDF_DRIVER_CONFIG_INIT(&config, DeviceAdd);
    return WdfDriverCreate(object, registry, WDF_NO_OBJECT_ATTRIBUTES, &config, WDF_NO_HANDLE);
}
