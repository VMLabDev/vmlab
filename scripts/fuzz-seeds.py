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


def checksum(data):
    if len(data) % 2:
        data += b"\0"
    total = sum(struct.unpack(f">{len(data) // 2}H", data))
    while total > 0xFFFF:
        total = (total & 0xFFFF) + (total >> 16)
    return ~total & 0xFFFF


def ipv4(src, dst, proto, payload):
    head = struct.pack(">BBHHHBBH4s4s", 0x45, 0, 20 + len(payload), 1, 0, 64, proto, 0, src, dst)
    return head[:10] + struct.pack(">H", checksum(head)) + head[12:] + payload


def udp(src, dst, sport, dport, payload):
    head = struct.pack(">HHHH", sport, dport, 8 + len(payload), 0)
    pseudo = src + dst + struct.pack(">BBH", 0, 17, 8 + len(payload))
    return head[:6] + struct.pack(">H", checksum(pseudo + head + payload) or 0xFFFF) + payload


def ethernet(dst, src, payload):
    return dst + src + b"\x08\x00" + payload


def records(*frames):
    """The net_frame input: each frame behind a 2-byte big-endian length."""
    return b"".join(struct.pack(">H", len(frame)) + frame for frame in frames)


GATEWAY = bytes([10, 213, 1, 1])
GATEWAY_MAC = bytes([0x52, 0x56, 0, 0, 0, 1])
CLIENT = bytes([10, 213, 1, 2])
CLIENT_MAC = bytes([0x02, 0, 0, 0, 0, 9])
ANY, EVERYONE = bytes(4), b"\xff" * 4


def dhcp(kind, options=b""):
    bootp = bytearray(240)
    bootp[0:4] = bytes([1, 1, 6, 0])
    bootp[4:8] = b"seed"
    bootp[28:34] = CLIENT_MAC
    bootp[236:240] = bytes([99, 130, 83, 99])
    bootp += bytes([53, 1, kind]) + options + b"\xff"
    return ethernet(b"\xff" * 6, CLIENT_MAC, ipv4(ANY, EVERYONE, 17, udp(ANY, EVERYONE, 68, 67, bytes(bootp))))


def dns(name, qtype=1):
    labels = b"".join(bytes([len(part)]) + part.encode() for part in name.split("."))
    query = struct.pack(">HHHHHH", 7, 0x0100, 1, 0, 0, 0) + labels + b"\0" + struct.pack(">HH", qtype, 1)
    return ethernet(GATEWAY_MAC, CLIENT_MAC, ipv4(CLIENT, GATEWAY, 17, udp(CLIENT, GATEWAY, 40000, 53, query)))


net = root / "net_frame" / "corpus"
net.mkdir(parents=True, exist_ok=True)
requested = bytes([50, 4]) + CLIENT + bytes([54, 4]) + GATEWAY
(net / "dhcp-lease").write_bytes(
    records(dhcp(1), dhcp(3, requested), dhcp(4, requested), dhcp(1), dhcp(7))
)
(net / "dns-queries").write_bytes(
    records(
        dns("web.vmlab.internal"),
        dns("a.apps.vmlab.internal"),
        dns("x.blocked.test"),
        dns("example.org", 28),
    )
)
arp = struct.pack(">HHBBH", 1, 0x0800, 6, 4, 1) + CLIENT_MAC + CLIENT + bytes(6) + GATEWAY
echo = bytearray(struct.pack(">BBHHH", 8, 0, 0, 1, 1) + b"ping")
struct.pack_into(">H", echo, 2, checksum(bytes(echo)))
tcp = struct.pack(">HHIIBBHHH", 40000, 80, 1, 0, 0x60, 2, 65535, 0, 0) + bytes([2, 4, 5, 180])
(net / "arp-icmp-tcp").write_bytes(
    records(
        b"\xff" * 6 + CLIENT_MAC + b"\x08\x06" + arp,
        ethernet(GATEWAY_MAC, CLIENT_MAC, ipv4(CLIENT, GATEWAY, 1, bytes(echo))),
        ethernet(GATEWAY_MAC, CLIENT_MAC, ipv4(CLIENT, GATEWAY, 6, tcp)),
    )
)


def vnet(mtu, flags, gso_type, gso_size, csum_start, csum_offset, frame, queue=0):
    """The vnet_offload input: the port MTU, then one vnet stream record body."""
    header = struct.pack("<BBHHHH", flags, gso_type, 0, gso_size, csum_start, csum_offset) + bytes(2)
    return struct.pack(">HHH", mtu, 1, queue) + header + frame


offload = root / "vnet_offload" / "corpus"
offload.mkdir(parents=True, exist_ok=True)
segment = struct.pack(">HHIIBBHHH", 40000, 80, 1, 1, 0x50, 0x18, 65535, 0, 0) + b"x" * 4000
big_tcp4 = ethernet(GATEWAY_MAC, CLIENT_MAC, ipv4(CLIENT, GATEWAY, 6, segment))
(offload / "tso4").write_bytes(vnet(1500, 1, 1, 1460, 34, 16, big_tcp4))
tagged = big_tcp4[:12] + b"\x81\x00\x00\x05" + big_tcp4[12:]
(offload / "tso4-vlan").write_bytes(vnet(1500, 1, 1, 1460, 38, 16, tagged))
datagram = struct.pack(">HHHH", 40000, 53, 8 + 3000, 0) + b"y" * 3000
big_udp4 = ethernet(GATEWAY_MAC, CLIENT_MAC, ipv4(CLIENT, GATEWAY, 17, datagram))
(offload / "uso4").write_bytes(vnet(1500, 1, 5, 1000, 34, 6, big_udp4))
ipv6 = struct.pack(">IHBB", 6 << 28, len(segment), 6, 64) + bytes(15) + b"\1" + bytes(15) + b"\2"
big_tcp6 = GATEWAY_MAC + CLIENT_MAC + b"\x86\xdd" + ipv6 + segment
(offload / "tso6").write_bytes(vnet(1500, 1, 4, 1440, 54, 16, big_tcp6))
small = ethernet(GATEWAY_MAC, CLIENT_MAC, ipv4(CLIENT, GATEWAY, 17, datagram[:8] + b"abc"))
(offload / "partial-checksum").write_bytes(vnet(1500, 1, 0, 0, 34, 6, small))
(offload / "plain").write_bytes(vnet(1500, 0, 0, 0, 0, 0, small))
