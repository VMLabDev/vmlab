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
        assert_eq!(frame.pixels.as_slice(), [42, 42, 42, 0]);
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

fn ordered_flip(scanout: u32, id: u32, frame: u64) -> Vec<u8> {
    let mut request = vec![0; 24];
    request[..4].copy_from_slice(&CMD_ORDERED_FLIP.to_le_bytes());
    for word in [scanout, id, 0, 0, 1, 1] {
        request.extend_from_slice(&word.to_le_bytes());
    }
    request.extend_from_slice(&frame.to_le_bytes());
    request
}

#[test]
fn the_ordered_flip_is_offered_and_refused_until_the_driver_acks_it() {
    let mut gpu = Gpu::new("gpu");
    let bit = 1u64 << ORDERED_FLIP_FEATURE;
    assert_ne!(gpu.features() & bit, 0);
    let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 4096)]).unwrap();

    // A driver that does not know the bit, as Linux's: the command is refused.
    gpu.ack_features(gpu.features() & !bit);
    assert_eq!(
        gpu.state.lock().command(&mem, 0, &ordered_flip(0, 1, 1)),
        Err(INVALID)
    );

    gpu.ack_features(gpu.features());
    assert_ne!(gpu.state.lock().acked & bit, 0);
    assert_eq!(
        gpu.state.lock().command(&mem, 0, &ordered_flip(1, 1, 1)),
        Err(SCANOUT)
    );
    // A short request is refused, not read past its end.
    let mut short = ordered_flip(0, 1, 1);
    short.truncate(50);
    assert_eq!(gpu.state.lock().command(&mem, 0, &short), Err(INVALID));

    gpu.reset();
    assert_eq!(gpu.state.lock().acked & bit, 0);
}

#[test]
fn an_ordered_flip_is_published_before_it_is_answered() {
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

    let mut gpu = Gpu::new("gpu");
    gpu.ack_features(gpu.features());
    let sink = Arc::new(Sink::default());
    let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 4096)]).unwrap();
    let rect = Rect {
        x: 0,
        y: 0,
        w: 1,
        h: 1,
    };
    let mut s = gpu.state.lock();
    s.display = Some(sink.clone());
    // A frame thread that never runs: a plain flush only waits for it.
    s.pacer = Some(Arc::new(pacer::Pacer::default()));
    s.native = true;
    s.scanout = Some((1, rect));
    s.resources.insert(
        1,
        Resource {
            width: 1,
            height: 1,
            format: 2,
            pixels: vec![1, 1, 1, 0],
            backing: vec![],
        },
    );

    let mut flush = vec![0; 24];
    flush[..4].copy_from_slice(&0x104u32.to_le_bytes());
    for word in [0u32, 0, 1, 1, 1, 0] {
        flush.extend_from_slice(&word.to_le_bytes());
    }
    assert!(s.command(&mem, 0, &flush).is_ok());
    s.schedule_flush(1);
    assert!(
        sink.0.lock().is_empty(),
        "a plain flush is left to the pacer"
    );

    s.resources.get_mut(&1).unwrap().pixels = vec![42, 42, 42, 0];
    assert_eq!(
        s.command(&mem, 0, &ordered_flip(0, 1, 7)),
        Ok((NODATA, Vec::new()))
    );
    let updates = sink.0.lock();
    let [update] = updates.as_slice() else {
        panic!("expected one published frame, got {}", updates.len());
    };
    let ScanoutPixels::Shm(frame) = &update.pixels else {
        panic!("expected a 2D scanout");
    };
    assert_eq!(frame.pixels.as_slice(), [42, 42, 42, 0]);
    // The flip's frame holds the earlier flush too: none is left pending.
    assert!(s.flush_pending.is_none());
}

fn present_copy(src: u32, dst: u32, boxes: &[[u32; 6]]) -> Vec<u8> {
    let mut request = vec![0; 24];
    request[..4].copy_from_slice(&CMD_PRESENT_COPY.to_le_bytes());
    for word in [src, dst] {
        request.extend_from_slice(&word.to_le_bytes());
    }
    request.extend_from_slice(&0u64.to_le_bytes());
    request.extend_from_slice(&0u64.to_le_bytes());
    for word in [64, 64, 1, 4, boxes.len() as u32, 0] {
        request.extend_from_slice(&word.to_le_bytes());
    }
    for b in boxes {
        for word in b {
            request.extend_from_slice(&word.to_le_bytes());
        }
    }
    request
}

#[test]
fn the_present_copy_is_refused_until_the_driver_acks_the_bit() {
    let mut gpu = Gpu::new("gpu");
    let bit = 1u64 << ORDERED_FLIP_FEATURE;
    let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 4096)]).unwrap();
    let copy = present_copy(1, 2, &[[0, 0, 0, 0, 4, 4]]);

    gpu.ack_features(gpu.features() & !bit);
    assert_eq!(gpu.state.lock().command(&mem, 0, &copy), Err(INVALID));

    // Acked, but a 2D device has no renderer resources to copy between.
    gpu.ack_features(gpu.features());
    assert_eq!(gpu.state.lock().command(&mem, 0, &copy), Err(INVALID));
    // A request without boxes, or cut short, is refused before any lookup.
    assert_eq!(
        gpu.state.lock().command(&mem, 0, &present_copy(1, 2, &[])),
        Err(INVALID)
    );
    let mut short = copy.clone();
    short.truncate(80);
    assert_eq!(gpu.state.lock().command(&mem, 0, &short), Err(INVALID));
}

#[derive(Default)]
struct Interrupts {
    queues: parking_lot::Mutex<Vec<u16>>,
    config: std::sync::atomic::AtomicU32,
}

impl VirtioInterrupt for Interrupts {
    fn notify_queue(&self, queue: u16) {
        self.queues.lock().push(queue);
    }
    fn notify_config(&self) {
        self.config
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn events(gpu: &Gpu) -> u32 {
    let mut word = [0; 4];
    gpu.read_config(0, &mut word);
    u32::from_le_bytes(word)
}

fn command(kind: u32, words: &[u32]) -> Vec<u8> {
    let mut request = vec![0; 24];
    request[..4].copy_from_slice(&kind.to_le_bytes());
    for word in words {
        request.extend_from_slice(&word.to_le_bytes());
    }
    request
}

#[test]
fn a_lost_host_gpu_is_an_event_only_for_a_driver_that_acked_the_bit() {
    let bit = 1u64 << ORDERED_FLIP_FEATURE;
    let irq = Arc::new(Interrupts::default());

    // Linux's driver: no event, no configuration interrupt.
    let mut gpu = Gpu::new("gpu");
    gpu.ack_features(gpu.features() & !bit);
    gpu.state.lock().interrupt = Some(irq.clone());
    gpu.lose_host_gpu("test");
    assert_eq!(events(&gpu), 0);
    assert_eq!(irq.config.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(gpu.host_gpu_lost().as_deref(), Some("test"));

    // vmlab's driver: the event and one interrupt, and a second loss is
    // not a second event.
    let mut gpu = Gpu::new("gpu");
    gpu.ack_features(gpu.features());
    gpu.state.lock().interrupt = Some(irq.clone());
    gpu.lose_host_gpu("test");
    gpu.lose_host_gpu("again");
    assert_eq!(events(&gpu), EVENT_DEVICE_LOST);
    assert_eq!(irq.config.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(gpu.host_gpu_lost().as_deref(), Some("test"));

    // The driver clears it as it clears the display event.
    gpu.write_config(4, &EVENT_DEVICE_LOST.to_le_bytes());
    assert_eq!(events(&gpu), 0);

    // The loss outlives a reset, and the next negotiation of the bit
    // raises the event again.
    gpu.reset();
    assert_eq!(events(&gpu), 0);
    gpu.ack_features(gpu.features() & !bit);
    assert_eq!(events(&gpu), 0);
    gpu.ack_features(gpu.features());
    assert_eq!(events(&gpu), EVENT_DEVICE_LOST);
    assert!(gpu.host_gpu_lost().is_some());
}

#[test]
fn a_lost_host_gpu_refuses_work_but_serves_the_display_and_releases() {
    let mut gpu = Gpu::new("gpu");
    gpu.ack_features(gpu.features());
    let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 4096)]).unwrap();
    let mut s = gpu.state.lock();
    assert!(s.command(&mem, 0, &command(0x101, &[1, 2, 1, 1])).is_ok());
    drop(s);
    gpu.lose_host_gpu("test");

    let mut s = gpu.state.lock();
    for refused in [
        command(0x101, &[2, 2, 1, 1]),
        command(0x103, &[0, 0, 1, 1, 0, 1]),
        command(0x104, &[0, 0, 1, 1, 1, 0]),
        command(0x108, &[0, 0]),
        command(0x200, &[0; 18]),
        command(0x207, &[0, 0]),
        ordered_flip(0, 1, 1),
        present_copy(1, 2, &[[0, 0, 0, 0, 4, 4]]),
    ] {
        assert_eq!(s.command(&mem, 0, &refused), Err(UNSPEC));
    }
    assert_eq!(s.command(&mem, 0, &command(0x100, &[])).unwrap().0, 0x1101);
    assert_eq!(
        s.command(&mem, 0, &command(0x10a, &[0, 0])).unwrap().0,
        0x1104
    );
    // A release still releases.
    assert_eq!(
        s.command(&mem, 0, &command(0x102, &[1, 0])),
        Ok((NODATA, Vec::new()))
    );
    assert!(s.resources.is_empty());
    // The cursor queue has no host GPU to wait for.
    assert!(s.command(&mem, 1, &command(0x301, &[0; 8])).is_ok());
}

#[test]
fn a_lost_host_gpu_answers_every_fence_the_device_holds() {
    let gpu = Gpu::new("gpu");
    let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 4096)]).unwrap();
    let mut queue = Queue::new(16).unwrap();
    queue.try_set_size(16).unwrap();
    queue.try_set_desc_table_address(GuestAddress(0)).unwrap();
    queue
        .try_set_avail_ring_address(GuestAddress(0x400))
        .unwrap();
    queue
        .try_set_used_ring_address(GuestAddress(0x800))
        .unwrap();
    queue.set_ready(true);
    let irq = Arc::new(Interrupts::default());
    {
        let mut s = gpu.state.lock();
        s.mem = Some(mem.clone());
        s.interrupt = Some(irq.clone());
        s.queues = vec![queue, Queue::new(16).unwrap()];
        for head in [3, 5] {
            s.fences.push(PendingFence {
                queue: 0,
                head,
                len: 24,
                flags: 3,
                id: u64::from(head),
                ctx: 1,
                ring: 0,
            });
        }
    }
    gpu.lose_host_gpu("test");

    assert!(gpu.state.lock().fences.is_empty());
    // The used ring holds both heads, in order, and the guest was told once.
    let used = |at: u64| mem.read_obj::<u32>(GuestAddress(0x800 + at)).unwrap();
    assert_eq!(used(0) >> 16, 2);
    assert_eq!((used(4), used(12)), (3, 5));
    assert_eq!(irq.queues.lock().as_slice(), [0]);
}

/// A reset whose render worker exits late, and one whose worker does not
/// exit: the status reads zero only once the worker has gone, so that the
/// guest frees nothing a worker can still use (`teardown.rs`).
#[test]
fn a_reset_is_acknowledged_only_after_the_render_workers_have_exited() {
    use crate::PciDevice;
    use crate::virtio::pci::VirtioPciDevice;
    use crate::virtio::pci::device_status::DEVICE_NEEDS_RESET;
    use std::time::{Duration, Instant};
    use teardown::tests::{proc_entry, scratch};

    let proc = scratch("gpu-reset");
    let gpu = Gpu::new("gpu");
    let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap();
    let mut pci = VirtioPciDevice::new("pci", Box::new(gpu.clone()), mem, None, None);
    let status = |pci: &mut VirtioPciDevice| {
        let mut byte = [0];
        pci.bar_read(4, 0x14, &mut byte);
        byte[0]
    };
    let worker = |patience| {
        // The render server 10 and its worker 20, of one guest context.
        proc_entry(&proc, 10, 'S', 1, 100);
        proc_entry(&proc, 20, 'S', 10, 200);
        let mut workers = teardown::Workers::at(&proc, patience);
        workers.note(&[10]);
        gpu.state.lock().workers = workers;
    };

    // The worker exits while the reset waits: the reset is acknowledged.
    worker(Duration::from_secs(5));
    pci.bar_write(4, 0x14, &[1]);
    let late = proc.clone();
    let exit = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        std::fs::remove_dir_all(late.join("20")).unwrap();
    });
    let start = Instant::now();
    pci.bar_write(4, 0x14, &[0]);
    assert!(start.elapsed() >= Duration::from_millis(50));
    assert_eq!(status(&mut pci), 0);
    exit.join().unwrap();

    // The worker does not exit in time: the status says the reset failed,
    // and the driver cannot start the device again.
    worker(Duration::from_millis(20));
    pci.bar_write(4, 0x14, &[1]);
    pci.bar_write(4, 0x14, &[0]);
    assert_eq!(status(&mut pci), DEVICE_NEEDS_RESET);
    pci.bar_write(4, 0x14, &[1]);
    assert_eq!(status(&mut pci), DEVICE_NEEDS_RESET);
    assert!(gpu.state.lock().unsettled);

    // It exits later: the next read finishes the reset.
    proc_entry(&proc, 20, 'Z', 10, 200);
    assert_eq!(status(&mut pci), 0);
    assert!(!gpu.state.lock().unsettled);
    pci.bar_write(4, 0x14, &[1]);
    assert_eq!(status(&mut pci), 1);
    std::fs::remove_dir_all(&proc).unwrap();
}
