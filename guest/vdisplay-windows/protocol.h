#pragma once

#include <stddef.h>
#include <stdint.h>
#include <string.h>
#if defined(_MSC_VER) && !defined(__clang__)
#include <intrin.h>
#include <emmintrin.h>
#endif

namespace vdisplay {

constexpr uint32_t version = 1;
constexpr uint32_t header_size = 4096;
constexpr uint32_t cursor_bytes = 256 * 256 * 4;
constexpr uint64_t pixels_start = header_size + cursor_bytes;
constexpr uint32_t xrgb8888 = 0x34325258;

struct Mode {
    uint32_t width;
    uint32_t height;
};

inline bool equal(Mode a, Mode b) {
    return a.width == b.width && a.height == b.height;
}

inline uint32_t read32(const uint8_t* memory, size_t offset) {
#if defined(_MSC_VER) && !defined(__clang__)
    uint32_t value = uint32_t(__iso_volatile_load32(reinterpret_cast<const volatile int*>(memory + offset)));
    _ReadBarrier();
    return value;
#else
    return __atomic_load_n(reinterpret_cast<const uint32_t*>(memory + offset), __ATOMIC_ACQUIRE);
#endif
}

inline uint64_t read64(const uint8_t* memory, size_t offset) {
#if defined(_MSC_VER) && !defined(__clang__)
    uint64_t value = uint64_t(__iso_volatile_load64(reinterpret_cast<const volatile __int64*>(memory + offset)));
    _ReadBarrier();
    return value;
#else
    return __atomic_load_n(reinterpret_cast<const uint64_t*>(memory + offset), __ATOMIC_ACQUIRE);
#endif
}

inline void write32(uint8_t* memory, size_t offset, uint32_t value) {
#if defined(_MSC_VER) && !defined(__clang__)
    _WriteBarrier();
    __iso_volatile_store32(reinterpret_cast<volatile int*>(memory + offset), int(value));
#else
    __atomic_store_n(reinterpret_cast<uint32_t*>(memory + offset), value, __ATOMIC_RELEASE);
#endif
}

inline void write64(uint8_t* memory, size_t offset, uint64_t value) {
#if defined(_MSC_VER) && !defined(__clang__)
    _WriteBarrier();
    __iso_volatile_store64(reinterpret_cast<volatile __int64*>(memory + offset), __int64(value));
#else
    __atomic_store_n(reinterpret_cast<uint64_t*>(memory + offset), value, __ATOMIC_RELEASE);
#endif
}

inline void release_fence() {
#if defined(_MSC_VER) && !defined(__clang__)
    _mm_mfence();
#else
    __atomic_thread_fence(__ATOMIC_SEQ_CST);
#endif
}

struct Layout {
    uint64_t size;
    uint64_t buffers_offset;
    uint32_t buffer_stride;
    Mode maximum;

    bool supports(Mode mode) const {
        return mode.width != 0 && mode.height != 0 &&
               mode.width <= maximum.width && mode.height <= maximum.height &&
               uint64_t(mode.width) * mode.height * 4 <= buffer_stride;
    }
};

inline bool validate(const uint8_t* memory, uint64_t mapped_size, Layout& out) {
    if (mapped_size < pixels_start || memcmp(memory, "VMLABDSP", 8) != 0 ||
        read32(memory, 8) != version || read32(memory, 12) != header_size ||
        read32(memory, 0x20) != 3) {
        return false;
    }
    Layout candidate = {read64(memory, 0x10), read64(memory, 0x28),
                        read32(memory, 0x24), {read32(memory, 0x18), read32(memory, 0x1c)}};
    if (candidate.size > mapped_size || candidate.size < pixels_start ||
        candidate.buffers_offset < pixels_start || candidate.buffers_offset > candidate.size ||
        (candidate.buffers_offset & 4095) || (candidate.buffer_stride & 4095) ||
        candidate.buffer_stride == 0 ||
        uint64_t(candidate.buffer_stride) * 3 > candidate.size - candidate.buffers_offset ||
        candidate.maximum.width > 16384 || candidate.maximum.height > 16384 ||
        !candidate.supports(candidate.maximum)) {
        return false;
    }
    out = candidate;
    return true;
}

struct Request {
    uint32_t seq;
    Mode mode;
};

inline bool request(const uint8_t* memory, const Layout& layout, Request& out) {
    uint32_t seq = read32(memory, 0x3c);
    Mode mode = {read32(memory, 0x34), read32(memory, 0x38)};
    if (read32(memory, 0x3c) != seq ||
        ((!mode.width || !mode.height) && (mode.width || mode.height)) ||
        (mode.width && !layout.supports(mode))) {
        return false;
    }
    out = {seq, mode};
    return true;
}

inline Mode preferred(const Layout& layout, Mode requested) {
    if (layout.supports(requested)) return requested;
    if (layout.supports({1920, 1080})) return {1920, 1080};
    return layout.maximum;
}

inline unsigned modes(const Layout& layout, Mode requested, Mode* output) {
    const Mode common[] = {{640, 480}, {800, 600}, {1024, 768}, {1280, 720},
                           {1280, 800}, {1600, 900}, {1920, 1080}, {1920, 1200},
                           {2560, 1440}, {2560, 1600}, {3840, 2160}};
    unsigned count = 1;
    output[0] = preferred(layout, requested);
    for (Mode mode : common) {
        if (layout.supports(mode) && !equal(mode, output[0])) output[count++] = mode;
    }
    return count;
}

inline bool publish(uint8_t* memory, const Layout& layout, const uint8_t* source,
                    Mode mode, uint32_t source_pitch) {
    if (!layout.supports(mode) || source_pitch < uint64_t(mode.width) * 4) return false;
    uint32_t current = read32(memory, 0x114);
    if (current >= 3) return false;
    // Following 0,1,2 rotation leaves both the current and previous buffers untouched.
    uint32_t next = (current + 1) % 3;
    uint32_t pitch = mode.width * 4;
    uint8_t* destination = memory + layout.buffers_offset + uint64_t(next) * layout.buffer_stride;
    for (uint32_t y = 0; y < mode.height; ++y) {
        memcpy(destination + uint64_t(y) * pitch, source + uint64_t(y) * source_pitch, pitch);
    }
    write32(memory, 0x118, mode.width);
    write32(memory, 0x11c, mode.height);
    write32(memory, 0x120, pitch);
    write32(memory, 0x124, xrgb8888);
    for (size_t offset = 0x128; offset <= 0x134; offset += 4) write32(memory, offset, 0);
    write32(memory, 0x114, next);
    // A full hardware fence also drains write-combined BAR2 stores on x86.
    release_fence();
    write32(memory, 0x110, read32(memory, 0x110) + 1);
    return true;
}

inline uint16_t read16(const uint8_t* data) {
    return uint16_t(data[0]) | uint16_t(data[1]) << 8;
}

inline void put16(uint8_t* data, uint32_t value) {
    data[0] = uint8_t(value);
    data[1] = uint8_t(value >> 8);
}

inline void edid(const Layout& layout, Mode requested, uint8_t* output) {
    memset(output, 0, 128);
    const uint8_t signature[] = {0, 255, 255, 255, 255, 255, 255, 0};
    memcpy(output, signature, sizeof(signature));
    output[8] = 0x59;
    output[9] = 0xac; // EISA manufacturer VML.
    output[10] = 1;
    output[12] = 1;
    output[16] = 1;
    output[17] = 36;
    output[18] = 1;
    output[19] = 4;
    output[20] = 0x80;
    output[21] = 60;
    output[22] = 34;
    output[23] = 120;
    output[24] = 0x06;
    for (unsigned i = 38; i < 54; i += 2) { output[i] = 1; output[i + 1] = 1; }
    Mode mode = preferred(layout, requested);
    // EDID base-block timings have 12-bit active dimensions. Larger modes remain in the DDI list.
    if (mode.width > 4095 || mode.height > 4095) mode = {1920, 1080};
    uint8_t* timing = output + 54;
    put16(timing, ((mode.width + 160) * (mode.height + 30) * 60) / 10000);
    timing[2] = uint8_t(mode.width);
    timing[3] = 160;
    timing[4] = uint8_t((mode.width >> 8) << 4);
    timing[5] = uint8_t(mode.height);
    timing[6] = 30;
    timing[7] = uint8_t((mode.height >> 8) << 4);
    timing[8] = 48;
    timing[9] = 32;
    timing[10] = 0x35;
    timing[12] = 0x58;
    timing[13] = 0x54;
    timing[14] = 0x21;
    timing[17] = 0x1a;
    output[75] = 0xfc;
    memcpy(output + 77, "VMLab display\n", 13);
    output[93] = 0xff;
    memcpy(output + 95, "000000000001\n", 13);
    output[111] = 0x0f; // Manufacturer-specific descriptor used by the parse callback.
    memcpy(output + 113, "VMLD", 4);
    put16(output + 117, layout.maximum.width);
    put16(output + 119, layout.maximum.height);
    put16(output + 121, requested.width);
    put16(output + 123, requested.height);
    uint8_t sum = 0;
    for (unsigned i = 0; i < 127; ++i) sum += output[i];
    output[127] = uint8_t(0 - sum);
}

inline bool parse_edid(const uint8_t* data, size_t size, Layout& layout, Mode& requested) {
    if (size != 128 || data[8] != 0x59 || data[9] != 0xac ||
        memcmp(data + 113, "VMLD", 4) != 0) return false;
    uint8_t sum = 0;
    for (unsigned i = 0; i < 128; ++i) sum += data[i];
    if (sum) return false;
    layout = {};
    layout.maximum = {read16(data + 117), read16(data + 119)};
    layout.buffer_stride = 0xffffffff;
    requested = {read16(data + 121), read16(data + 123)};
    return layout.supports(layout.maximum) && layout.maximum.width <= 16384 &&
           layout.maximum.height <= 16384;
}

} // namespace vdisplay
