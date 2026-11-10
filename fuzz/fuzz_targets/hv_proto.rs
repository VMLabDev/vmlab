#![no_main]
use libfuzzer_sys::fuzz_target;
use vmlab_hv_proto::codec;

fuzz_target!(|data: &[u8]| {
    if let Ok(Some(msg)) = codec::read_client_msg(&mut &data[..]) {
        let mut out = Vec::new();
        codec::write_client_msg(&mut out, &msg).unwrap();
        assert!(codec::read_client_msg(&mut &out[..]).unwrap().is_some());
    }
    if let Ok(Some(msg)) = codec::read_server_msg(&mut &data[..]) {
        let mut out = Vec::new();
        codec::write_server_msg(&mut out, &msg).unwrap();
        assert!(codec::read_server_msg(&mut &out[..]).unwrap().is_some());
    }
});
