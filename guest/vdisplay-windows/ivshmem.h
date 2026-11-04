#pragma once

#include <windows.h>
#include <winioctl.h>
#include <cfgmgr32.h>

// ABI from virtio-win Public.h, revision bd196cda83a50a5b2545dd4f85d25cc6238e10ae.
// https://github.com/virtio-win/kvm-guest-drivers-windows/tree/bd196cda83a50a5b2545dd4f85d25cc6238e10ae/ivshmem
namespace ivshmem {

const GUID interface_id = {0xdf576976, 0x569d, 0x4672, {0x95, 0xa0, 0xf5, 0x7e, 0x4e, 0xa0, 0xb2, 0x10}};
constexpr DWORD size_ioctl = CTL_CODE(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_ANY_ACCESS);
constexpr DWORD map_ioctl = CTL_CODE(FILE_DEVICE_UNKNOWN, 0x802, METHOD_BUFFERED, FILE_ANY_ACCESS);
constexpr DWORD unmap_ioctl = CTL_CODE(FILE_DEVICE_UNKNOWN, 0x803, METHOD_BUFFERED, FILE_ANY_ACCESS);
constexpr DWORD ring_ioctl = CTL_CODE(FILE_DEVICE_UNKNOWN, 0x804, METHOD_BUFFERED, FILE_ANY_ACCESS);

struct MapConfig { UINT8 cache_mode; };
struct Mapping {
    UINT16 peer_id;
    UINT64 size;
    void* memory;
    UINT16 vectors;
};
struct Ring { UINT16 peer_id; UINT16 vector; };
static_assert(sizeof(MapConfig) == 1 && sizeof(Mapping) == 32 && sizeof(Ring) == 4, "IVSHMEM x64 ABI");
static_assert(offsetof(Mapping, size) == 8 && offsetof(Mapping, memory) == 16 &&
              offsetof(Mapping, vectors) == 24, "IVSHMEM mapping ABI");

inline bool ring(HANDLE handle) {
    Ring request = {0, 0};
    DWORD returned = 0;
    return DeviceIoControl(handle, ring_ioctl, &request, sizeof(request), nullptr, 0, &returned, nullptr) != FALSE;
}

} // namespace ivshmem
