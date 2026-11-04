#!/usr/bin/env python3
"""Initialize or inspect an explicitly named C2 shared-memory file on an x86 host."""

import argparse
import hashlib
import json
import mmap
import os
from pathlib import Path
import struct


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=["init", "read", "request"])
    parser.add_argument("file", type=Path)
    parser.add_argument("--width", type=int, default=1920)
    parser.add_argument("--height", type=int, default=1080)
    arguments = parser.parse_args()
    with arguments.file.open("r+b" if arguments.operation != "read" else "rb") as file:
        size = os.fstat(file.fileno()).st_size
        access = mmap.ACCESS_READ if arguments.operation == "read" else mmap.ACCESS_WRITE
        with mmap.mmap(file.fileno(), 0, access=access) as memory:
            if arguments.operation == "init":
                if size < 100 * 1024 * 1024 or size & (size - 1):
                    raise ValueError("Expected a power-of-two region of at least 128 MiB")
                if any(memory[:4096]):
                    raise ValueError("Refusing to replace a nonempty shared-memory header")
                struct.pack_into("<8sIIQIIIIQ", memory, 0, b"VMLABDSP", 1, 4096, size,
                                 3840, 2160, 3, 3840 * 2160 * 4, 4096 + 256 * 256 * 4)
                struct.pack_into("<IIII", memory, 0x30, 1, arguments.width, arguments.height, 1)
            if memory[:8] != b"VMLABDSP" or struct.unpack_from("<I", memory, 8)[0] != 1:
                raise ValueError("Not a C2 version-1 region")
            if arguments.operation == "request":
                maximum = struct.unpack_from("<II", memory, 0x18)
                if not 0 < arguments.width <= maximum[0] or not 0 < arguments.height <= maximum[1]:
                    raise ValueError("Mode exceeds the region's maximum dimensions")
                sequence = struct.unpack_from("<I", memory, 0x3c)[0]
                struct.pack_into("<II", memory, 0x34, arguments.width, arguments.height)
                struct.pack_into("<I", memory, 0x3c, (sequence + 1) & 0xffffffff)
            for _ in range(20):
                before = struct.unpack_from("<I", memory, 0x110)[0]
                header = memory[:4096]
                state, build, heartbeat, sequence, index, width, height, pitch, pixel_format = struct.unpack_from("<IIQIIIIII", header, 0x100)
                buffer_stride = struct.unpack_from("<I", header, 0x24)[0]
                base = struct.unpack_from("<Q", header, 0x28)[0] + index * buffer_stride
                frame = b""
                if state == 1 and index < 3 and 0 < width <= 3840 and 0 < height <= 2160 and pitch >= width * 4:
                    if pitch * height > buffer_stride or base + pitch * height > size:
                        raise ValueError("Frame is outside the mapped region")
                    frame = memory[base:base + pitch * height]
                if before == struct.unpack_from("<I", memory, 0x110)[0]:
                    break
            else:
                raise RuntimeError("Frame changed during every read")
            cursor = struct.unpack_from("<IiiIIIIII", header, 0x200)
            print(json.dumps({"state": state, "build": build, "heartbeat_ns": heartbeat,
                              "frame_seq": sequence, "buffer": index, "width": width,
                              "height": height, "pitch": pitch, "format": pixel_format,
                              "request_ack": struct.unpack_from("<I", header, 0x138)[0],
                              "cursor": cursor, "frame_sha256": hashlib.sha256(frame).hexdigest() if frame else None}))


if __name__ == "__main__":
    main()
