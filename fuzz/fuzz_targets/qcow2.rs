#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::Write;
use vmlab_disk::{DiskImage, qcow2::Qcow2Image};

fuzz_target!(|data: &[u8]| {
    // Never follow a fuzzer-controlled backing path outside the temporary image.
    if data.len() < 512 || data[8..20].iter().any(|&b| b != 0) {
        return;
    }
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(data).unwrap();
    if let Ok(mut image) = Qcow2Image::open(file.path(), true) {
        let mut out = vec![0; image.size().min(65536) as usize];
        let _ = image.read_at(0, &mut out);
        let _ = image.snapshots();
    }
});
