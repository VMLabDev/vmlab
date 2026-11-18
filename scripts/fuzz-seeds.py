"""Generate small binary seeds from the qcow2 and agent framing specs."""

import json
from pathlib import Path
import struct
import sys


root = Path(sys.argv[1])
image = bytearray(4 * 512)
# v2, no backing, 512-byte clusters, 1 MiB virtual size, no encryption,
# 32 L1 entries at cluster 1, refcount table at cluster 2, no snapshots.
struct.pack_into(
    ">IIQIIQIIQQIIQ", image, 0,
    0x514649FB, 2, 0, 0, 9, 1 << 20, 0, 32, 512, 1024, 1, 0, 0,
)
struct.pack_into(">Q", image, 1024, 1536)
for cluster in range(4):
    struct.pack_into(">H", image, 1536 + cluster * 2, 1)
qcow = root / "qcow2" / "corpus"
qcow.mkdir(parents=True, exist_ok=True)
(qcow / "empty-v2").write_bytes(image)
# Exercise parsing beyond the empty snapshot fast path as well.
struct.pack_into(">I", image, 60, 1)
struct.pack_into(">Q", image, 64, 2048)
image.extend(bytearray(1024))
struct.pack_into(">QIHHIIQII", image, 2048, 2560, 32, 1, 4, 0, 0, 0, 0, 0)
image[2088:2093] = b"1seed"
for cluster in [4, 5]:
    struct.pack_into(">H", image, 1536 + cluster * 2, 1)
(qcow / "snapshot-v2").write_bytes(image)

agent = root / "agent_proto" / "corpus"
agent.mkdir(parents=True, exist_ok=True)
payload = json.dumps({"type": "hello", "proto_version": 1, "token": "seed"}).encode()
(agent / "hello").write_bytes(b"VMLB" + struct.pack("<IBI", len(payload), 0, 0) + payload)
(agent / "data").write_bytes(b"VMLB" + struct.pack("<IBI", 3, 1, 7) + b"abc")
