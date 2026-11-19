use super::*;

#[test]
fn partial_compose_reuses_released_pixels_and_preserves_held_frames() {
    let gpu = Gpu::new("gpu");
    let mut state = gpu.state.lock();
    state.native = true;
    state.scanout = Some((
        1,
        Rect {
            x: 0,
            y: 0,
            w: 35,
            h: 17,
        },
    ));
    state.resources.insert(
        1,
        Resource {
            width: 35,
            height: 17,
            format: 2,
            pixels: vec![0; 35 * 17 * 4],
            backing: Vec::new(),
        },
    );
    let initial = state.frame_2d().unwrap();

    let mut flush = |x, y, value| {
        let offset = ((y * 35 + x) * 4) as usize;
        state.resources.get_mut(&1).unwrap().pixels[offset] = value;
        state.flushes.push(Rect { x, y, w: 1, h: 1 });
        state.dirty = true;
        state.frame_2d().unwrap()
    };
    let first = flush(0, 0, 10);
    let second = flush(16, 0, 20);
    assert_eq!(initial.pixels[0], 0);
    assert_eq!(first.pixels[16 * 4], 0);
    assert_eq!(second.pixels[0], 10);
    assert_eq!(second.pixels[16 * 4], 20);
    let reusable = first.pixels.as_ptr();
    drop(first);

    let third = flush(34, 16, 30);
    assert_eq!(third.pixels.as_ptr(), reusable);
    assert_eq!(third.pixels[0], 10);
    assert_eq!(third.pixels[16 * 4], 20);
    assert_eq!(third.pixels[(16 * 35 + 34) * 4], 30);
    assert_eq!(
        third.damage,
        [crate::display::Damage {
            x: 32,
            y: 16,
            width: 3,
            height: 1
        }]
    );
    assert_eq!(second.pixels[(16 * 35 + 34) * 4], 0);
    let unchanged = flush(34, 16, 30);
    assert!(Arc::ptr_eq(&third.pixels, &unchanged.pixels));
    assert!(unchanged.damage.is_empty());
}

#[test]
fn backing_entries_are_bounded_across_resources() {
    let gpu = Gpu::new("gpu");
    let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 4096)]).unwrap();
    let mut state = gpu.state.lock();
    for id in 1..=2 {
        state.resources.insert(
            id,
            Resource {
                width: 1,
                height: 1,
                format: 1,
                pixels: vec![0; 4],
                backing: vec![],
            },
        );
    }
    let attach = |id: u32, count: usize| {
        let mut req = vec![0; 24];
        req[..4].copy_from_slice(&0x106u32.to_le_bytes());
        req.extend_from_slice(&id.to_le_bytes());
        req.extend_from_slice(&(count as u32).to_le_bytes());
        for _ in 0..count {
            req.extend_from_slice(&0u64.to_le_bytes());
            req.extend_from_slice(&1u32.to_le_bytes());
            req.extend_from_slice(&0u32.to_le_bytes());
        }
        req
    };
    assert!(state.command(&mem, 0, &attach(1, MAX_ENTRIES)).is_ok());
    for _ in 0..100 {
        assert_eq!(state.command(&mem, 0, &attach(2, 1)), Err(INVALID));
    }
    assert_eq!(
        state
            .resources
            .values()
            .map(|r| r.backing.len())
            .sum::<usize>(),
        MAX_ENTRIES
    );
    let mut detach = vec![0; 24];
    detach[..4].copy_from_slice(&0x107u32.to_le_bytes());
    detach.extend_from_slice(&1u32.to_le_bytes());
    detach.extend_from_slice(&0u32.to_le_bytes());
    assert!(state.command(&mem, 0, &detach).is_ok());
    assert!(state.command(&mem, 0, &attach(2, 1)).is_ok());
}

#[test]
fn edid_has_valid_checksum_and_preferred_timing() {
    for (w, h) in [(1024, 768), (1920, 1080), (3840, 2160)] {
        let e = edid(w, h);
        for block in e.as_chunks::<128>().0 {
            assert_eq!(block.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0);
        }
        assert_eq!(u32::from(e[56]) | (u32::from(e[58] >> 4) << 8), w);
        assert_eq!(u32::from(e[59]) | (u32::from(e[61] >> 4) << 8), h);
        assert_eq!(&e[..8], &[0, 255, 255, 255, 255, 255, 255, 0]);
    }
}

#[test]
fn edid_external_decoder_matches_qemu_reference_when_available() {
    use std::process::Command;
    if Command::new("edid-decode").arg("--help").output().is_err() {
        eprintln!("SKIP EDID conformance: edid-decode missing");
        return;
    }
    let decode = |bytes: &[u8]| {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        Command::new("edid-decode")
            .arg("--check")
            .arg(file.path())
            .output()
            .unwrap()
    };
    let reference = decode(&edid(1280, 800));
    let reference_text = String::from_utf8(reference.stdout).unwrap();
    // The captured QEMU CTA block lacks a VCDB. D30 preserves that byte-for-byte;
    // resized timings must not add decoder failures to the reference's failures.
    eprintln!("QEMU reference EDID decoder status: {}", reference.status);
    let failures = |text: &str| {
        text.split_once("\nFailures:\n")
            .map(|(_, errors)| errors.to_owned())
    };
    for (w, h) in [(1024, 768), (1920, 1080), (3840, 2160)] {
        let result = decode(&edid(w, h));
        let output = String::from_utf8(result.stdout).unwrap();
        assert_eq!(result.status, reference.status, "{output}");
        assert_eq!(failures(&output), failures(&reference_text), "{output}");
    }
}

#[test]
fn all_formats_convert_to_bgra() {
    for (format, pixels) in [
        (1, [1, 2, 3, 4]),
        (2, [1, 2, 3, 0]),
        (3, [4, 3, 2, 1]),
        (4, [0, 3, 2, 1]),
        (67, [3, 2, 1, 4]),
        (68, [0, 1, 2, 3]),
        (121, [4, 1, 2, 3]),
        (134, [3, 2, 1, 0]),
    ] {
        let r = Resource {
            width: 1,
            height: 1,
            format,
            pixels: pixels.to_vec(),
            backing: vec![],
        };
        let alpha = if matches!(format, 2 | 4 | 68 | 134) {
            255
        } else {
            4
        };
        assert_eq!(r.pixel(0), [1, 2, 3, alpha]);
    }
}

#[test]
fn snapshot_publishes_pending_flush_without_guest_activity() {
    #[derive(Default)]
    struct Sink(parking_lot::Mutex<Vec<ScanoutUpdate>>);

    impl DisplaySink for Sink {
        fn needs_cpu(&self) -> bool {
            true
        }

        fn update(&self, frame: ScanoutUpdate) {
            self.0.lock().push(frame);
        }
    }

    let gpu = Gpu::new("gpu");
    let original = Arc::new(Sink::default());
    let rect = Rect {
        x: 0,
        y: 0,
        w: 1,
        h: 1,
    };
    {
        let mut s = gpu.state.lock();
        s.display = Some(original.clone());
        // Record pacing requests without a thread that could publish before save.
        s.pacer = Some(Arc::new(pacer::Pacer::default()));
        s.native = true;
        s.scanout = Some((1, rect));
        s.resources.insert(
            1,
            Resource {
                width: 1,
                height: 1,
                format: 2,
                pixels: vec![0; 4],
                backing: vec![],
            },
        );
        s.publish_scanout(1);
        s.resources.get_mut(&1).unwrap().pixels = vec![42, 42, 42, 0];

        let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 4096)]).unwrap();
        let mut flush = vec![0; 24];
        flush[..4].copy_from_slice(&0x104u32.to_le_bytes());
        for word in [0u32, 0, 1, 1, 1, 0] {
            flush.extend_from_slice(&word.to_le_bytes());
        }
        assert!(s.command(&mem, 0, &flush).is_ok());
        s.schedule_flush(2);
    }
    let saved = gpu.save_state().unwrap();
    assert_eq!(original.0.lock().len(), 1);

    for attach_before_restore in [true, false] {
        let mut restored = Gpu::new("restored");
        let sink = Arc::new(Sink::default());
        if attach_before_restore {
            restored.set_display_sink(sink.clone());
        }
        restored.restore_state(&saved).unwrap();
        if !attach_before_restore {
            restored.set_display_sink(sink.clone());
        }

        let updates = sink.0.lock();
        let [update] = updates.as_slice() else {
            panic!("expected one restored frame, got {}", updates.len());
        };
        assert_eq!(update.timestamp_ns, 2);
        let ScanoutPixels::Shm(frame) = &update.pixels else {
            panic!("expected a restored 2D scanout");
        };
        assert_eq!(frame.pixels, [42, 42, 42, 0]);
        assert!(restored.state.lock().flush_pending.is_none());
    }
}

#[test]
fn restores_gpu_snapshot_without_flush_state() {
    let mut gpu = Gpu::new("gpu");
    gpu.request_size(1920, 1080).unwrap();
    let mut old = gpu.save_state().unwrap();
    old[4] = 1;
    assert_eq!(old.pop(), Some(0));
    gpu.request_size(1024, 768).unwrap();
    gpu.restore_state(&old).unwrap();
    let restored = gpu.state.lock();
    assert_eq!(restored.preferred, (1920, 1080));
    assert!(restored.flush_pending.is_none());
}

#[test]
fn invalid_snapshots_do_not_change_gpu() {
    let mut gpu = Gpu::new("gpu");
    gpu.request_size(1920, 1080).unwrap();
    let state = gpu.save_state().unwrap();
    for n in 0..state.len() {
        assert!(gpu.restore_state(&state[..n]).is_err());
        assert_eq!(gpu.save_state().unwrap(), state);
    }
    let mut state = state;
    state[0] = 0;
    assert!(gpu.restore_state(&state).is_err());
}
