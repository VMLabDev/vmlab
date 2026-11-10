#![no_main]
use libfuzzer_sys::fuzz_target;
use vmlab_agent_proto::{AgentMsg, FrameDecoder, HostMsg, encode_frame};

fuzz_target!(|data: &[u8]| {
    let mut decoder = FrameDecoder::new();
    for chunk in data.chunks(17) {
        decoder.push(chunk);
        while let Some(frame) = decoder.next_frame() {
            let _ = serde_json::from_slice::<HostMsg>(&frame.payload);
            let _ = serde_json::from_slice::<AgentMsg>(&frame.payload);
            let mut roundtrip = FrameDecoder::new();
            roundtrip.push(&encode_frame(frame.kind, frame.channel, &frame.payload));
            let decoded = roundtrip.next_frame().unwrap();
            assert_eq!(decoded.payload, frame.payload);
            assert_eq!(decoded.channel, frame.channel);
        }
    }
});
