/*
 * VK_LAYER_VMLAB_pci_filter — the implicit Vulkan layer that leaves a
 * `vulkan` guest's renderer only the host GPU it was given (PRD §5.2).
 *
 * The loader's own filter (`VK_LOADER_DRIVERS_SELECT`) matches a driver, not
 * a device, so two identical cards both pass it and virglrenderer hands Venus
 * both. This layer sits between the loader and the drivers and answers
 * vkEnumeratePhysicalDevices and vkEnumeratePhysicalDeviceGroups with only
 * the device whose VK_EXT_pci_bus_info address is the one in
 * VMLAB_VK_PCI_ADDRESS (`dddd:bb:dd.f`, hex). Every other call passes through
 * untouched. Unset or unparsable, the layer hides nothing; set, a device
 * that cannot report its PCI address is hidden too, because it is not the
 * one asked for. VMLAB_VK_PCI_FILTER_DISABLE=1 (the manifest's
 * disable_environment) keeps the loader from loading it at all.
 *
 * The few Vulkan types the layer touches are declared here rather than taken
 * from <vulkan/vulkan.h>, so building vmlab needs a C compiler (which ring,
 * zstd-sys and aws-lc-sys already need) and no Vulkan SDK. They are the
 * stable ABI of Vulkan 1.1 and the loader's layer interface version 2.
 */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define EXPORT __attribute__((visibility("default")))

typedef int32_t VkResult;
#define VK_SUCCESS 0
#define VK_INCOMPLETE 5
#define VK_ERROR_OUT_OF_HOST_MEMORY (-1)
#define VK_ERROR_INITIALIZATION_FAILED (-3)

typedef int32_t VkStructureType;
#define VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO 47
#define VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO 48
#define VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2 1000059001
#define VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_GROUP_PROPERTIES 1000070000
#define VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PCI_BUS_INFO_PROPERTIES_EXT 1000212000

typedef struct VkInstance_T *VkInstance;
typedef struct VkPhysicalDevice_T *VkPhysicalDevice;
typedef struct VkDevice_T *VkDevice;
typedef uint32_t VkBool32;
typedef void (*PFN_vkVoidFunction)(void);
typedef PFN_vkVoidFunction (*PFN_vkGetInstanceProcAddr)(VkInstance, const char *);
typedef PFN_vkVoidFunction (*PFN_vkGetDeviceProcAddr)(VkDevice, const char *);

/* Every create-info and output struct opens with these two members. */
typedef struct {
    VkStructureType sType;
    const void *pNext;
} VkBaseInStructure;

/* vk_layer.h: the loader's chain links. Only `pLayerInfo`, the first member
 * of each union, is read. */
#define VK_LAYER_LINK_INFO 0
typedef struct VkLayerInstanceLink {
    struct VkLayerInstanceLink *pNext;
    PFN_vkGetInstanceProcAddr pfnNextGetInstanceProcAddr;
    PFN_vkVoidFunction pfnNextGetPhysicalDeviceProcAddr;
} VkLayerInstanceLink;
typedef struct {
    VkStructureType sType;
    const void *pNext;
    int32_t function;
    union {
        VkLayerInstanceLink *pLayerInfo;
        void *other[2];
    } u;
} VkLayerInstanceCreateInfo;
typedef struct VkLayerDeviceLink {
    struct VkLayerDeviceLink *pNext;
    PFN_vkGetInstanceProcAddr pfnNextGetInstanceProcAddr;
    PFN_vkGetDeviceProcAddr pfnNextGetDeviceProcAddr;
} VkLayerDeviceLink;
typedef struct {
    VkStructureType sType;
    const void *pNext;
    int32_t function;
    union {
        VkLayerDeviceLink *pLayerInfo;
        void *other;
    } u;
} VkLayerDeviceCreateInfo;
typedef struct {
    int32_t sType;
    void *pNext;
    uint32_t loaderLayerInterfaceVersion;
    PFN_vkGetInstanceProcAddr pfnGetInstanceProcAddr;
    PFN_vkGetDeviceProcAddr pfnGetDeviceProcAddr;
    PFN_vkVoidFunction pfnGetPhysicalDeviceProcAddr;
} VkNegotiateLayerInterface;

typedef struct {
    char extensionName[256];
    uint32_t specVersion;
} VkExtensionProperties;

typedef struct {
    VkStructureType sType;
    void *pNext;
    uint32_t pciDomain, pciBus, pciDevice, pciFunction;
} VkPhysicalDevicePCIBusInfoPropertiesEXT;

/* VkPhysicalDeviceProperties is 824 bytes on LP64; only deviceName, at byte
 * 20 after four uint32s and the device type, is read. */
typedef struct {
    VkStructureType sType;
    void *pNext;
    union {
        struct {
            uint32_t head[5];
            char deviceName[256];
        } p;
        uint64_t space[128];
    } properties;
} VkPhysicalDeviceProperties2;

typedef struct {
    VkStructureType sType;
    void *pNext;
    uint32_t physicalDeviceCount;
    VkPhysicalDevice physicalDevices[32];
    VkBool32 subsetAllocation;
} VkPhysicalDeviceGroupProperties;

typedef VkResult (*PFN_vkCreateInstance)(const void *, const void *, VkInstance *);
typedef void (*PFN_vkDestroyInstance)(VkInstance, const void *);
typedef VkResult (*PFN_vkCreateDevice)(VkPhysicalDevice, const void *, const void *, VkDevice *);
typedef void (*PFN_vkDestroyDevice)(VkDevice, const void *);
typedef VkResult (*PFN_vkEnumeratePhysicalDevices)(VkInstance, uint32_t *, VkPhysicalDevice *);
typedef VkResult (*PFN_vkEnumeratePhysicalDeviceGroups)(VkInstance, uint32_t *,
                                                        VkPhysicalDeviceGroupProperties *);
typedef VkResult (*PFN_vkEnumerateDeviceExtensionProperties)(VkPhysicalDevice, const char *,
                                                             uint32_t *, VkExtensionProperties *);
typedef void (*PFN_vkGetPhysicalDeviceProperties2)(VkPhysicalDevice, VkPhysicalDeviceProperties2 *);

/* Dispatchable handles start with the loader's dispatch table pointer, the
 * key a layer finds its own per-object state by. */
static void *key_of(const void *handle) { return *(void *const *)handle; }

/* One entry per live instance or device: its key and the next link down. */
struct entry {
    void *key;
    VkInstance instance;
    PFN_vkGetInstanceProcAddr gipa;
    PFN_vkGetDeviceProcAddr gdpa;
};
static struct entry *entries;
static size_t n_entries;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;

static int remember(struct entry e) {
    pthread_mutex_lock(&lock);
    struct entry *grown = realloc(entries, (n_entries + 1) * sizeof *entries);
    if (grown) {
        entries = grown;
        entries[n_entries++] = e;
    }
    pthread_mutex_unlock(&lock);
    return grown != NULL;
}

static struct entry find(const void *handle) {
    void *key = key_of(handle);
    struct entry found = {0};
    pthread_mutex_lock(&lock);
    for (size_t i = 0; i < n_entries; i++)
        if (entries[i].key == key) {
            found = entries[i];
            break;
        }
    pthread_mutex_unlock(&lock);
    return found;
}

static void forget(const void *handle) {
    void *key = key_of(handle);
    pthread_mutex_lock(&lock);
    for (size_t i = 0; i < n_entries; i++)
        if (entries[i].key == key) {
            entries[i] = entries[--n_entries];
            break;
        }
    pthread_mutex_unlock(&lock);
}

/* The PCI address asked for; 0 when the variable is unset or unparsable,
 * which filters nothing. */
static int wanted(uint32_t addr[4]) {
    const char *s = getenv("VMLAB_VK_PCI_ADDRESS");
    unsigned d, b, v, f;
    if (!s || sscanf(s, "%x:%x:%x.%x", &d, &b, &v, &f) != 4)
        return 0;
    addr[0] = d, addr[1] = b, addr[2] = v, addr[3] = f;
    return 1;
}

static int has_pci_bus_info(struct entry *in, VkPhysicalDevice pd) {
    PFN_vkEnumerateDeviceExtensionProperties list = (PFN_vkEnumerateDeviceExtensionProperties)
        in->gipa(in->instance, "vkEnumerateDeviceExtensionProperties");
    uint32_t n = 0;
    if (!list || list(pd, NULL, &n, NULL) != VK_SUCCESS)
        return 0;
    VkExtensionProperties *ext = calloc(n ? n : 1, sizeof *ext);
    int found = 0;
    if (ext && list(pd, NULL, &n, ext) >= 0)
        for (uint32_t i = 0; i < n && !found; i++)
            found = !strcmp(ext[i].extensionName, "VK_EXT_pci_bus_info");
    free(ext);
    return found;
}

/* Said once per process, on stderr — QEMU's, so qemu.log — when the filter
 * first decides: which device it kept and how many it hid. */
static void say_once(const char *addr, const char *kept, uint32_t hidden) {
    static int said;
    pthread_mutex_lock(&lock);
    int first = !said;
    said = 1;
    pthread_mutex_unlock(&lock);
    if (!first)
        return;
    if (kept)
        fprintf(stderr, "vmlab-pci-filter: %s: kept %s, hid %u\n", addr, kept, hidden);
    else
        fprintf(stderr, "vmlab-pci-filter: %s: no Vulkan device at this address, hid %u\n", addr,
                hidden);
}

/* Whether a physical device is the one asked for, writing its name into
 * `name` when it is. */
static int keep(struct entry *in, const uint32_t addr[4], VkPhysicalDevice pd, char name[256]) {
    if (!has_pci_bus_info(in, pd))
        return 0;
    PFN_vkGetPhysicalDeviceProperties2 props =
        (PFN_vkGetPhysicalDeviceProperties2)in->gipa(in->instance, "vkGetPhysicalDeviceProperties2");
    if (!props)
        props = (PFN_vkGetPhysicalDeviceProperties2)in->gipa(in->instance,
                                                            "vkGetPhysicalDeviceProperties2KHR");
    if (!props)
        return 0;
    VkPhysicalDevicePCIBusInfoPropertiesEXT pci = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PCI_BUS_INFO_PROPERTIES_EXT};
    VkPhysicalDeviceProperties2 p = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
                                     .pNext = &pci};
    props(pd, &p);
    if (pci.pciDomain != addr[0] || pci.pciBus != addr[1] || pci.pciDevice != addr[2] ||
        pci.pciFunction != addr[3])
        return 0;
    memcpy(name, p.properties.p.deviceName, 256);
    name[255] = 0;
    return 1;
}

static VkResult enumerate_devices(VkInstance instance, uint32_t *count, VkPhysicalDevice *out) {
    struct entry in = find(instance);
    PFN_vkEnumeratePhysicalDevices next =
        (PFN_vkEnumeratePhysicalDevices)in.gipa(instance, "vkEnumeratePhysicalDevices");
    uint32_t addr[4];
    if (!wanted(addr))
        return next(instance, count, out);
    uint32_t n = 0;
    VkResult r = next(instance, &n, NULL);
    if (r < 0)
        return r;
    VkPhysicalDevice *all = calloc(n ? n : 1, sizeof *all);
    if (!all)
        return VK_ERROR_OUT_OF_HOST_MEMORY;
    r = next(instance, &n, all);
    if (r < 0) {
        free(all);
        return r;
    }
    uint32_t m = 0;
    char name[256] = "";
    for (uint32_t i = 0; i < n; i++)
        if (keep(&in, addr, all[i], name))
            all[m++] = all[i];
    say_once(getenv("VMLAB_VK_PCI_ADDRESS"), m ? name : NULL, n - m);
    /* Vulkan's two-call idiom: a count, or as many as fit. */
    uint32_t c = out ? (*count < m ? *count : m) : m;
    if (out)
        memcpy(out, all, c * sizeof *all);
    *count = c;
    free(all);
    return (out && c < m) ? VK_INCOMPLETE : VK_SUCCESS;
}

static VkResult enumerate_groups(VkInstance instance, uint32_t *count,
                                 VkPhysicalDeviceGroupProperties *out, const char *entry_point) {
    struct entry in = find(instance);
    PFN_vkEnumeratePhysicalDeviceGroups next =
        (PFN_vkEnumeratePhysicalDeviceGroups)in.gipa(instance, entry_point);
    uint32_t addr[4];
    if (!wanted(addr))
        return next(instance, count, out);
    uint32_t n = 0;
    VkResult r = next(instance, &n, NULL);
    if (r < 0)
        return r;
    VkPhysicalDeviceGroupProperties *all = calloc(n ? n : 1, sizeof *all);
    if (!all)
        return VK_ERROR_OUT_OF_HOST_MEMORY;
    for (uint32_t i = 0; i < n; i++)
        all[i].sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_GROUP_PROPERTIES;
    r = next(instance, &n, all);
    if (r < 0) {
        free(all);
        return r;
    }
    uint32_t m = 0;
    char name[256];
    for (uint32_t i = 0; i < n; i++) {
        VkPhysicalDeviceGroupProperties g = all[i];
        uint32_t k = 0;
        for (uint32_t x = 0; x < g.physicalDeviceCount; x++)
            if (keep(&in, addr, g.physicalDevices[x], name))
                g.physicalDevices[k++] = g.physicalDevices[x];
        if (!k)
            continue;
        if (k != g.physicalDeviceCount)
            g.subsetAllocation = 0;
        g.physicalDeviceCount = k;
        all[m++] = g;
    }
    uint32_t c = out ? (*count < m ? *count : m) : m;
    if (out)
        for (uint32_t i = 0; i < c; i++) {
            /* The caller's pNext chain is its own; only the members move. */
            out[i].physicalDeviceCount = all[i].physicalDeviceCount;
            memcpy(out[i].physicalDevices, all[i].physicalDevices,
                   sizeof all[i].physicalDevices);
            out[i].subsetAllocation = all[i].subsetAllocation;
        }
    *count = c;
    free(all);
    return (out && c < m) ? VK_INCOMPLETE : VK_SUCCESS;
}

static VkResult EnumeratePhysicalDevices(VkInstance i, uint32_t *c, VkPhysicalDevice *o) {
    return enumerate_devices(i, c, o);
}
static VkResult EnumeratePhysicalDeviceGroups(VkInstance i, uint32_t *c,
                                              VkPhysicalDeviceGroupProperties *o) {
    return enumerate_groups(i, c, o, "vkEnumeratePhysicalDeviceGroups");
}
static VkResult EnumeratePhysicalDeviceGroupsKHR(VkInstance i, uint32_t *c,
                                                 VkPhysicalDeviceGroupProperties *o) {
    return enumerate_groups(i, c, o, "vkEnumeratePhysicalDeviceGroupsKHR");
}

/* The loader's link for this layer in a create-info's pNext chain. */
static const VkBaseInStructure *link_info(const void *create_info, VkStructureType type) {
    const VkBaseInStructure *s = ((const VkBaseInStructure *)create_info)->pNext;
    while (s && !(s->sType == type && ((const VkLayerInstanceCreateInfo *)s)->function ==
                                          VK_LAYER_LINK_INFO))
        s = s->pNext;
    return s;
}

static VkResult CreateInstance(const void *info, const void *alloc, VkInstance *instance) {
    VkLayerInstanceCreateInfo *chain = (VkLayerInstanceCreateInfo *)link_info(
        info, VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO);
    if (!chain)
        return VK_ERROR_INITIALIZATION_FAILED;
    PFN_vkGetInstanceProcAddr gipa = chain->u.pLayerInfo->pfnNextGetInstanceProcAddr;
    chain->u.pLayerInfo = chain->u.pLayerInfo->pNext;
    PFN_vkCreateInstance create = (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance");
    VkResult r = create(info, alloc, instance);
    if (r != VK_SUCCESS)
        return r;
    if (!remember((struct entry){key_of(*instance), *instance, gipa, NULL})) {
        ((PFN_vkDestroyInstance)gipa(*instance, "vkDestroyInstance"))(*instance, alloc);
        return VK_ERROR_OUT_OF_HOST_MEMORY;
    }
    return r;
}

static void DestroyInstance(VkInstance instance, const void *alloc) {
    struct entry in = find(instance);
    forget(instance);
    ((PFN_vkDestroyInstance)in.gipa(instance, "vkDestroyInstance"))(instance, alloc);
}

static VkResult CreateDevice(VkPhysicalDevice pd, const void *info, const void *alloc,
                             VkDevice *device) {
    VkLayerDeviceCreateInfo *chain =
        (VkLayerDeviceCreateInfo *)link_info(info, VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO);
    if (!chain)
        return VK_ERROR_INITIALIZATION_FAILED;
    PFN_vkGetInstanceProcAddr gipa = chain->u.pLayerInfo->pfnNextGetInstanceProcAddr;
    PFN_vkGetDeviceProcAddr gdpa = chain->u.pLayerInfo->pfnNextGetDeviceProcAddr;
    chain->u.pLayerInfo = chain->u.pLayerInfo->pNext;
    PFN_vkCreateDevice create = (PFN_vkCreateDevice)gipa(NULL, "vkCreateDevice");
    VkResult r = create(pd, info, alloc, device);
    if (r != VK_SUCCESS)
        return r;
    if (!remember((struct entry){key_of(*device), NULL, NULL, gdpa})) {
        ((PFN_vkDestroyDevice)gdpa(*device, "vkDestroyDevice"))(*device, alloc);
        return VK_ERROR_OUT_OF_HOST_MEMORY;
    }
    return r;
}

static void DestroyDevice(VkDevice device, const void *alloc) {
    struct entry dev = find(device);
    forget(device);
    ((PFN_vkDestroyDevice)dev.gdpa(device, "vkDestroyDevice"))(device, alloc);
}

EXPORT PFN_vkVoidFunction vmlab_pci_filter_GetDeviceProcAddr(VkDevice device, const char *name) {
    if (!strcmp(name, "vkGetDeviceProcAddr"))
        return (PFN_vkVoidFunction)vmlab_pci_filter_GetDeviceProcAddr;
    if (!strcmp(name, "vkDestroyDevice"))
        return (PFN_vkVoidFunction)DestroyDevice;
    struct entry dev = find(device);
    return dev.gdpa ? dev.gdpa(device, name) : NULL;
}

EXPORT PFN_vkVoidFunction vmlab_pci_filter_GetInstanceProcAddr(VkInstance instance,
                                                               const char *name) {
    static const struct {
        const char *name;
        PFN_vkVoidFunction fn;
    } ours[] = {
        {"vkGetInstanceProcAddr", (PFN_vkVoidFunction)vmlab_pci_filter_GetInstanceProcAddr},
        {"vkGetDeviceProcAddr", (PFN_vkVoidFunction)vmlab_pci_filter_GetDeviceProcAddr},
        {"vkCreateInstance", (PFN_vkVoidFunction)CreateInstance},
        {"vkDestroyInstance", (PFN_vkVoidFunction)DestroyInstance},
        {"vkCreateDevice", (PFN_vkVoidFunction)CreateDevice},
        {"vkDestroyDevice", (PFN_vkVoidFunction)DestroyDevice},
        {"vkEnumeratePhysicalDevices", (PFN_vkVoidFunction)EnumeratePhysicalDevices},
        {"vkEnumeratePhysicalDeviceGroups", (PFN_vkVoidFunction)EnumeratePhysicalDeviceGroups},
        {"vkEnumeratePhysicalDeviceGroupsKHR",
         (PFN_vkVoidFunction)EnumeratePhysicalDeviceGroupsKHR},
    };
    for (size_t i = 0; i < sizeof ours / sizeof ours[0]; i++)
        if (!strcmp(name, ours[i].name))
            return ours[i].fn;
    if (!instance)
        return NULL;
    struct entry in = find(instance);
    return in.gipa ? in.gipa(instance, name) : NULL;
}

EXPORT VkResult vkNegotiateLoaderLayerInterfaceVersion(VkNegotiateLayerInterface *v) {
    if (v->loaderLayerInterfaceVersion < 2)
        return VK_ERROR_INITIALIZATION_FAILED;
    v->loaderLayerInterfaceVersion = 2;
    v->pfnGetInstanceProcAddr = vmlab_pci_filter_GetInstanceProcAddr;
    v->pfnGetDeviceProcAddr = vmlab_pci_filter_GetDeviceProcAddr;
    v->pfnGetPhysicalDeviceProcAddr = NULL;
    return VK_SUCCESS;
}
