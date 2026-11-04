#include "protocol.h"

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>

int main() {
    constexpr uint64_t size = 128 * 1024 * 1024;
    auto memory = static_cast<uint8_t*>(aligned_alloc(4096, size));
    assert(memory);
    memset(memory, 0, size);
    memcpy(memory, "VMLABDSP", 8);
    vdisplay::write32(memory, 8, 1);
    vdisplay::write32(memory, 12, 4096);
    vdisplay::write64(memory, 0x10, size);
    vdisplay::write32(memory, 0x18, 3840);
    vdisplay::write32(memory, 0x1c, 2160);
    vdisplay::write32(memory, 0x20, 3);
    vdisplay::write32(memory, 0x24, 3840 * 2160 * 4);
    vdisplay::write64(memory, 0x28, vdisplay::pixels_start);
    vdisplay::Layout layout;
    assert(vdisplay::validate(memory, size, layout));
    assert(!vdisplay::validate(memory, 1024, layout));
    vdisplay::write32(memory, 8, 2);
    assert(!vdisplay::validate(memory, size, layout));
    vdisplay::write32(memory, 8, 1);
    vdisplay::write64(memory, 0x28, UINT64_MAX - 4095);
    assert(!vdisplay::validate(memory, size, layout));
    vdisplay::write64(memory, 0x28, vdisplay::pixels_start);
    vdisplay::write32(memory, 0x24, 4096);
    assert(!vdisplay::validate(memory, size, layout));
    vdisplay::write32(memory, 0x24, 3840 * 2160 * 4);
    assert(vdisplay::validate(memory, size, layout));

    uint8_t source[8 * 2] = {1, 2, 3, 4, 99, 99, 99, 99, 5, 6, 7, 8, 99, 99, 99, 99};
    const uint8_t packed[] = {1, 2, 3, 4, 5, 6, 7, 8};
    for (unsigned frame = 0; frame < 8; ++frame) {
        uint32_t current = vdisplay::read32(memory, 0x114);
        uint8_t saved[8];
        memcpy(saved, memory + layout.buffers_offset + uint64_t(current) * layout.buffer_stride, sizeof(saved));
        assert(vdisplay::publish(memory, layout, source, {1, 2}, 8));
        assert(vdisplay::read32(memory, 0x110) == frame + 1);
        uint32_t next = vdisplay::read32(memory, 0x114);
        assert(next == (current + 1) % 3);
        assert(memcmp(memory + layout.buffers_offset + uint64_t(next) * layout.buffer_stride, packed, sizeof(packed)) == 0);
        assert(memcmp(memory + layout.buffers_offset + uint64_t(current) * layout.buffer_stride, saved, sizeof(saved)) == 0);
        assert(vdisplay::read32(memory, 0x120) == 4);
        assert(vdisplay::read32(memory, 0x124) == vdisplay::xrgb8888);
    }
    assert(!vdisplay::publish(memory, layout, source, {1, 2}, 3));
    assert(!vdisplay::publish(memory, layout, source, {0, 2}, 8));
    vdisplay::write32(memory, 0x110, UINT32_MAX);
    assert(vdisplay::publish(memory, layout, source, {1, 2}, 8));
    assert(vdisplay::read32(memory, 0x110) == 0);

    vdisplay::write32(memory, 0x34, 1366);
    vdisplay::write32(memory, 0x38, 768);
    vdisplay::write32(memory, 0x3c, 3);
    vdisplay::Request requested;
    assert(vdisplay::request(memory, layout, requested));
    assert(requested.seq == 3 && requested.mode.width == 1366);
    vdisplay::Mode list[12];
    unsigned count = vdisplay::modes(layout, requested.mode, list);
    assert(count == 12 && list[0].width == 1366);
    uint8_t descriptor[128];
    vdisplay::edid(layout, requested.mode, descriptor);
    vdisplay::Layout decoded;
    vdisplay::Mode decoded_request;
    assert(vdisplay::parse_edid(descriptor, 128, decoded, decoded_request));
    assert(vdisplay::equal(decoded.maximum, layout.maximum));
    assert(vdisplay::equal(decoded_request, requested.mode));
    descriptor[30] ^= 1;
    assert(!vdisplay::parse_edid(descriptor, 128, decoded, decoded_request));
    vdisplay::write32(memory, 0x34, 65535);
    assert(!vdisplay::request(memory, layout, requested));
    free(memory);
    puts("C2 validation, rotation, row pitch, publication, sequence wrap, modes, and EDID: passed");
}
