#![no_main]
use libfuzzer_sys::fuzz_target;
use vmlab_hv_proto::net::{Header, MAX_FRAME_LEN, METADATA_LEN, flow_hash};

// A vnet stream record as the switch reads it from a vmlab-hv port: the
// metadata carries the guest's own virtio-net header, and the switch
// completes its checksum or segments its GSO packet for a raw port.
fuzz_target!(|data: &[u8]| {
    let [a, b, body @ ..] = data else { return };
    // Network-layer MTU of the receiving port, from 68 (the IPv4 minimum) up.
    let mtu = u16::from_be_bytes([*a, *b]).max(68);
    if body.len() > MAX_FRAME_LEN + METADATA_LEN {
        return; // `read_packet` refuses a longer record before decoding it.
    }
    let Ok((header, queue, frame)) = Header::from_metadata(body) else {
        return;
    };
    assert_eq!(Header::decode(&header.encode()).unwrap(), header);
    assert_eq!(&header.metadata(queue)[..], &body[..METADATA_LEN]);
    let _ = flow_hash(frame);
    if let Ok(segments) = header.segments(frame, mtu) {
        for segment in &segments {
            // What leaves for a raw port carries no offload state, so it
            // must stand as a plain frame.
            assert!(segment.len() >= 14 && segment.len() <= MAX_FRAME_LEN);
            Header::default().validate(segment).unwrap();
            let _ = flow_hash(segment);
        }
    }
});
