use super::blit::{ROPS, rop};
use super::*;
use crate::Resettable;

fn device() -> CirrusVga {
    CirrusVga::new("vga", &[0x55, 0xaa, 1]).unwrap()
}

fn sr(dev: &mut CirrusVga, index: u8, value: u8) {
    dev.io_write(0x3c4, &[index, value]);
}

fn gr(dev: &mut CirrusVga, index: u8, value: u8) {
    dev.io_write(0x3ce, &[index, value]);
}

fn cr(dev: &mut CirrusVga, index: u8, value: u8) {
    dev.io_write(0x3d4, &[index, value]);
}

fn read(dev: &mut CirrusVga, port: u16) -> u8 {
    let mut value = [0];
    dev.io_read(port, &mut value);
    value[0]
}

fn indexed_read(dev: &mut CirrusVga, port: u16, index: u8) -> u8 {
    dev.io_write(port, &[index]);
    read(dev, port + 1)
}

fn dac(dev: &mut CirrusVga, rgb: [u8; 3]) {
    for component in rgb {
        dev.io_write(0x3c9, &[component]);
    }
}

fn vram(dev: &CirrusVga, addr: usize, len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    dev.vga
        .vram
        .read_slice(&mut bytes, GuestAddress(addr as u64))
        .unwrap();
    bytes
}

fn fill_vram(dev: &CirrusVga, addr: usize, bytes: &[u8]) {
    dev.vga
        .vram
        .write_slice(bytes, GuestAddress(addr as u64))
        .unwrap();
}

/// An extended mode: width x height at SR7 depth code `depth`, unblanked.
fn mode(dev: &mut CirrusVga, width: u32, height: u32, depth: u8, bytes: u32) {
    sr(dev, 6, 0x12);
    sr(dev, 7, 1 | depth << 1);
    cr(dev, 0x11, 0);
    cr(dev, 1, (width / 8 - 1) as u8);
    let lines = height - 1;
    cr(dev, 0x12, lines as u8);
    cr(
        dev,
        7,
        (((lines >> 8) & 1) << 1 | ((lines >> 9) & 1) << 6) as u8,
    );
    let pitch = width * bytes / 8;
    cr(dev, 0x13, pitch as u8);
    cr(dev, 0x1b, ((pitch >> 8) as u8 & 1) << 4);
    dev.io_write(0x3c0, &[0x20]);
}

/// Program a BitBLT through the graphics controller, as the capture does.
#[allow(clippy::too_many_arguments)]
fn blit(
    dev: &mut CirrusVga,
    mode: u8,
    rop: u8,
    width: usize,
    height: usize,
    dpitch: usize,
    spitch: usize,
    dst: usize,
    src: usize,
) {
    let w = width - 1;
    let h = height - 1;
    for (index, value) in [
        (0x20, w as u8),
        (0x21, (w >> 8) as u8),
        (0x22, h as u8),
        (0x23, (h >> 8) as u8),
        (0x24, dpitch as u8),
        (0x25, (dpitch >> 8) as u8),
        (0x26, spitch as u8),
        (0x27, (spitch >> 8) as u8),
        (0x28, dst as u8),
        (0x29, (dst >> 8) as u8),
        (0x2a, (dst >> 16) as u8),
        (0x2c, src as u8),
        (0x2d, (src >> 8) as u8),
        (0x2e, (src >> 16) as u8),
        (0x30, mode),
        (0x32, rop),
        (0x31, 0x02),
    ] {
        gr(dev, index, value);
    }
}

#[test]
fn pci_identity_bars_rom_and_config_masks_match_the_capture() {
    let mut dev = CirrusVga::new("vga", &[0x55, 0xaa, 1, 0xcb]).unwrap();
    assert_eq!(dev.config_read(0), 0x00b8_1013);
    assert_eq!(dev.config_read(2) >> 8, 0x030000);
    assert_eq!(dev.config_read(11), 0x1100_1af4);
    assert_eq!(dev.config_read(15) >> 8 & 0xff, 0, "no interrupt pin");
    assert_eq!(
        dev.bars()
            .iter()
            .map(|bar| (bar.index, bar.size))
            .collect::<Vec<_>>(),
        vec![(0, 32 << 20), (1, 4096), (ROM_BAR, 65536)]
    );
    dev.config_write(4, 0, &[0xff; 4]);
    dev.config_write(5, 0, &[0xff; 4]);
    dev.config_write(12, 0, &[0xff; 4]);
    assert_eq!(dev.config_read(4), 0xfe00_0008, "32 MiB prefetchable");
    assert_eq!(dev.config_read(5), 0xffff_f000);
    assert_eq!(dev.config_read(12), 0xffff_0001);
    dev.config_write(6, 0, &[0xff; 4]);
    assert_eq!(dev.config_read(6), 0, "no BAR2");
    let mut rom = [0; 4];
    dev.bar_read(ROM_BAR, 0, &mut rom);
    assert_eq!(rom, [0x55, 0xaa, 1, 0xcb]);
    dev.bar_read(ROM_BAR, 0xfffe, &mut rom[..2]);
    assert_eq!(rom[..2], [0xff, 0xff]);
    assert!(CirrusVga::new("vga", &[0x55; 65537]).is_err());
    assert_eq!(read(&mut dev, 0x1ce), 0xff, "no Bochs DISPI");
}

#[test]
fn unlock_identity_memory_size_and_read_only_registers() {
    let mut dev = device();
    assert_eq!(indexed_read(&mut dev, 0x3c4, 6), 0x0f);
    sr(&mut dev, 6, 0x12);
    assert_eq!(indexed_read(&mut dev, 0x3c4, 6), 0x12);
    sr(&mut dev, 6, 0);
    assert_eq!(indexed_read(&mut dev, 0x3c4, 6), 0x0f);
    sr(&mut dev, 6, 0x12);
    assert_eq!(indexed_read(&mut dev, 0x3c4, 0x0f), 0x98);
    sr(&mut dev, 0x0f, 0);
    assert_eq!(
        indexed_read(&mut dev, 0x3c4, 0x0f),
        0x98,
        "memory size fixed"
    );
    sr(&mut dev, 0x17, 0xff);
    assert_eq!(indexed_read(&mut dev, 0x3c4, 0x17), 0xe7, "bus type fixed");
    assert_eq!(indexed_read(&mut dev, 0x3d4, 0x27), 0xb8);
    cr(&mut dev, 0x27, 0);
    assert_eq!(indexed_read(&mut dev, 0x3d4, 0x27), 0xb8);
    dev.io_write(0x3c0, &[0x14]);
    assert_eq!(
        indexed_read(&mut dev, 0x3d4, 0x24),
        0x80,
        "attribute flip-flop"
    );
    assert_eq!(indexed_read(&mut dev, 0x3d4, 0x26), 0x14);
    gr(&mut dev, 0x0b, 0x2e);
    assert_eq!(indexed_read(&mut dev, 0x3ce, 0x0b), 0x2e);
}

#[test]
fn hidden_dac_register_needs_four_reads_of_the_pixel_mask() {
    let mut dev = device();
    for _ in 0..4 {
        assert_eq!(read(&mut dev, 0x3c6), 0xff);
    }
    dev.io_write(0x3c6, &[0xe1]);
    assert_eq!(dev.hdr, 0xe1);
    assert_eq!(
        read(&mut dev, 0x3c6),
        0xff,
        "the HDR write restarts the count"
    );
    for _ in 0..3 {
        read(&mut dev, 0x3c6);
    }
    assert_eq!(read(&mut dev, 0x3c6), 0xe1);
    for _ in 0..3 {
        read(&mut dev, 0x3c6);
    }
    read(&mut dev, 0x3c8);
    read(&mut dev, 0x3c6);
    dev.io_write(0x3c6, &[0x0f]);
    assert_eq!(dev.hdr, 0xe1, "another port resets the count");
    assert_eq!(dev.vga.dac_mask, 0x0f);
}

#[test]
fn banks_granularity_dual_banks_and_scaled_addressing() {
    let mut dev = device();
    mode(&mut dev, 640, 480, 0, 1);
    gr(&mut dev, 6, 0x05);
    gr(&mut dev, 9, 3);
    dev.legacy_write(0x10, &[0xa1]);
    assert_eq!(vram(&dev, 3 * 4096 + 0x10, 1), [0xa1], "4 KiB granularity");
    gr(&mut dev, 0x0b, 0x20);
    dev.legacy_write(0x10, &[0xa2]);
    assert_eq!(
        vram(&dev, 3 * 16384 + 0x10, 1),
        [0xa2],
        "16 KiB granularity"
    );
    gr(&mut dev, 0x0b, 0x21);
    gr(&mut dev, 0x0a, 8);
    dev.legacy_write(0x8004, &[0xa3]);
    assert_eq!(
        vram(&dev, 8 * 16384 + 4, 1),
        [0xa3],
        "GRA maps the upper half"
    );
    let mut back = [0];
    dev.legacy_read(0x8004, &mut back);
    assert_eq!(back, [0xa3]);
    dev.legacy_write(0x7fff, &[0xa4]);
    assert_eq!(vram(&dev, 3 * 16384 + 0x7fff, 1), [0xa4]);
    gr(&mut dev, 0x0b, 0x20);
    dev.legacy_write(0x8004, &[0xa5]);
    assert_eq!(
        vram(&dev, 3 * 16384 + 0x8004, 1),
        [0xa5],
        "single bank spans 64 KiB"
    );
    gr(&mut dev, 6, 0x0d);
    dev.legacy_write(0x8004, &[0xa6]);
    assert_eq!(
        vram(&dev, 3 * 16384 + 0x8004, 1),
        [0xa5],
        "outside the B0000 map"
    );
}

#[test]
fn extended_write_modes_expand_bits_through_the_pixel_mask() {
    let mut dev = device();
    mode(&mut dev, 640, 480, 0, 1);
    gr(&mut dev, 6, 0x05);
    fill_vram(&dev, 0, &[0x11; 64]);
    // Write mode 5, BY8: set bits take GR1, clear bits GR0.
    gr(&mut dev, 0x0b, 0x2e);
    gr(&mut dev, 5, 0x45);
    gr(&mut dev, 0, 0x22);
    gr(&mut dev, 1, 0x33);
    sr(&mut dev, 2, 0xff);
    dev.legacy_write(1, &[0b1010_0001]);
    assert_eq!(
        vram(&dev, 8, 8),
        [0x33, 0x22, 0x33, 0x22, 0x22, 0x22, 0x22, 0x33]
    );
    // Write mode 4 leaves clear bits; SR2 masks pixels, bit 7 first.
    gr(&mut dev, 5, 0x44);
    sr(&mut dev, 2, 0x7f);
    gr(&mut dev, 1, 0x44);
    dev.legacy_write(2, &[0xff]);
    assert_eq!(
        vram(&dev, 16, 8),
        [0x11, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44, 0x44]
    );
    // BY16: sixteen-bit colours from GR0/GR10 and GR1/GR11.
    sr(&mut dev, 2, 0xff);
    gr(&mut dev, 0x0b, 0x3c);
    gr(&mut dev, 5, 0x45);
    gr(&mut dev, 0x10, 0x02);
    gr(&mut dev, 0x11, 0x04);
    dev.legacy_write(2, &[0x80]);
    assert_eq!(vram(&dev, 32, 4), [0x44, 0x04, 0x22, 0x02]);
    // Write mode 1 with the eight-byte latch copies eight pixels.
    gr(&mut dev, 0x0b, 0x2e);
    let mut latch = [0];
    dev.legacy_read(1, &mut latch);
    gr(&mut dev, 5, 0x41);
    dev.legacy_write(5, &[0]);
    assert_eq!(vram(&dev, 40, 8), vram(&dev, 8, 8));
}

#[test]
fn packed_pixel_scanout_depths_pitch_and_start_address() {
    let mut dev = device();
    mode(&mut dev, 640, 480, 0, 1);
    assert_eq!((dev.mode().width, dev.mode().height), (640, 480));
    dev.io_write(0x3c8, &[7]);
    dac(&mut dev, [63, 0, 32]);
    fill_vram(&dev, 0, &[7]);
    assert_eq!(&dev.frame().pixels[..4], &[0x82, 0, 0xff, 0]);
    // 800x600x16, 5:6:5 from the hidden DAC register, as the capture's 0xe1.
    mode(&mut dev, 800, 600, 3, 2);
    for _ in 0..4 {
        read(&mut dev, 0x3c6);
    }
    dev.io_write(0x3c6, &[0xe1]);
    assert_eq!(dev.pitch(), 1600);
    fill_vram(&dev, 1600 * 2 + 4, &0xf800u16.to_le_bytes());
    let frame = dev.frame();
    assert_eq!((frame.mode.width, frame.mode.height), (800, 600));
    let at = (2 * 800 + 2) * 4;
    assert_eq!(&frame.pixels[at..at + 4], &[0, 0, 0xff, 0]);
    // 5:5:5 with 0xe0.
    for _ in 0..4 {
        read(&mut dev, 0x3c6);
    }
    dev.io_write(0x3c6, &[0xe0]);
    fill_vram(&dev, 1600 * 2 + 4, &0x03e0u16.to_le_bytes());
    assert_eq!(&dev.frame().pixels[at..at + 4], &[0, 0xff, 0, 0]);
    // 24 and 32 bits per pixel.
    mode(&mut dev, 640, 480, 2, 3);
    fill_vram(&dev, 3, &[1, 2, 3]);
    assert_eq!(&dev.frame().pixels[4..8], &[1, 2, 3, 0]);
    mode(&mut dev, 640, 480, 4, 4);
    fill_vram(&dev, 4, &[4, 5, 6, 7]);
    assert_eq!(&dev.frame().pixels[4..8], &[4, 5, 6, 0]);
    // Start address in dwords, with CR1B bit 0 as bit 16.
    cr(&mut dev, 0x0d, 1);
    let overflow = dev.cr(0x1b) & 0x10;
    cr(&mut dev, 0x1b, 0x01 | overflow);
    fill_vram(&dev, (0x10001) * 4, &[8, 9, 10, 0]);
    assert_eq!(&dev.frame().pixels[..4], &[8, 9, 10, 0]);
    // SR7 bit 0 clear returns to the VGA core.
    sr(&mut dev, 7, 0);
    assert_eq!(dev.mode(), dev.vga.mode());
}

/// The Windows 98 SE driver leaves SR1 = 0x21 at its 640x480 desktop.
#[test]
fn screen_off_does_not_blank_packed_pixel_modes() {
    let mut dev = device();
    mode(&mut dev, 640, 480, 0, 1);
    dac(&mut dev, [63, 63, 63]);
    fill_vram(&dev, 0, &[0]);
    sr(&mut dev, 1, 0x21);
    assert!(dev.scanout_active());
    dev.io_write(0x3c8, &[0]);
    dac(&mut dev, [63, 63, 63]);
    assert_eq!(&dev.frame().pixels[..4], &[0xff, 0xff, 0xff, 0]);
    // The attribute index's palette bit still blanks.
    read(&mut dev, 0x3da);
    dev.io_write(0x3c0, &[0x00]);
    assert!(!dev.scanout_active());
    assert_eq!(&dev.frame().pixels[..4], &[0; 4]);
    // VGA modes keep the core's rule.
    read(&mut dev, 0x3da);
    dev.io_write(0x3c0, &[0x20]);
    sr(&mut dev, 7, 0);
    assert!(!dev.scanout_active());
}

#[test]
fn hardware_cursor_is_composited_with_its_dac_colours() {
    let mut dev = device();
    mode(&mut dev, 640, 480, 4, 4);
    fill_vram(&dev, 0, &[0x40; 640 * 4]);
    sr(&mut dev, 0x12, 2);
    dev.io_write(0x3c8, &[0x00]);
    dac(&mut dev, [63, 0, 0]);
    dev.io_write(0x3c8, &[0x0f]);
    dac(&mut dev, [0, 0, 63]);
    dev.io_write(0x3c7, &[0x0f]);
    let back: Vec<u8> = (0..3).map(|_| read(&mut dev, 0x3c9)).collect();
    assert_eq!(back, [0, 0, 63]);
    // Cursor 1 of the 32x32 set, row 0: invert, background, foreground.
    let base = VRAM_SIZE - 16384 + 256;
    fill_vram(&dev, base, &[0b1010_0000]);
    fill_vram(&dev, base + 128, &[0b0110_0000]);
    sr(&mut dev, 0x13, 1);
    sr(&mut dev, 0x12, 1);
    // X = 2: SR10 = 0, index bits 7:5 = 2.
    sr(&mut dev, 0x50, 0);
    sr(&mut dev, 0x11, 0);
    let frame = dev.frame();
    let px = |x: usize| frame.pixels[x * 4..x * 4 + 4].to_vec();
    assert_eq!(px(1), [0x40, 0x40, 0x40, 0]);
    assert_eq!(px(2), [0xbf, 0xbf, 0xbf, 0], "inverted");
    assert_eq!(px(3), [0, 0, 0xff, 0], "background");
    assert_eq!(px(4), [0xff, 0, 0, 0], "foreground");
    assert_eq!(px(5), [0x40, 0x40, 0x40, 0], "transparent");
    sr(&mut dev, 0x12, 0);
    assert_eq!(&dev.frame().pixels[8..12], &[0x40, 0x40, 0x40, 0]);
}

#[test]
fn rop_table_covers_the_sixteen_codes() {
    let (s, d) = (0b1100u8, 0b1010u8);
    let mut seen = std::collections::BTreeSet::new();
    for code in ROPS {
        seen.insert(rop(code, s, d) & 0x0f);
    }
    assert_eq!(seen.len(), 16, "each code is a distinct boolean function");
    assert_eq!(rop(0x0d, s, d), s);
    assert_eq!(rop(0x05, s, d), s & d);
    assert_eq!(rop(0x59, s, d), s ^ d);
    assert_eq!(rop(0xd6, s, d), !s | d);
    assert_eq!(rop(0x42, s, d), d, "undefined codes keep the destination");
}

/// mode=0xc0 rop=0x0d w=640 h=480 dpitch=640 daddr=0 saddr=0x60c08: the
/// capture's full-screen fill from an 8x8 monochrome pattern.
#[test]
fn capture_pattern_fill_with_colour_expansion() {
    let mut dev = device();
    mode(&mut dev, 640, 480, 0, 1);
    fill_vram(
        &dev,
        0x60c08,
        &[0xff, 0x00, 0xaa, 0xff, 0xff, 0xff, 0xff, 0xff],
    );
    gr(&mut dev, 1, 0x07);
    gr(&mut dev, 0, 0x01);
    blit(&mut dev, 0xc0, 0x0d, 640, 480, 640, 0, 0, 0x60c08);
    assert_eq!(read(&mut dev, 0x3cf) & 1, 0, "complete");
    assert_eq!(vram(&dev, 0, 4), [7; 4]);
    assert_eq!(vram(&dev, 640, 4), [1; 4]);
    assert_eq!(vram(&dev, 1280, 4), [7, 1, 7, 1]);
    assert_eq!(vram(&dev, 640 * 479 + 636, 4), [7; 4]);
    assert_eq!(vram(&dev, 640 * 480, 1), [0], "nothing past the rectangle");
    // The low three source bits preset the pattern row.
    blit(&mut dev, 0xc0, 0x0d, 4, 1, 640, 0, 0, 0x60c09);
    assert_eq!(vram(&dev, 0, 4), [1; 4]);
}

#[test]
fn solid_extension_without_expansion_still_reads_source() {
    let mut dev = device();
    mode(&mut dev, 640, 480, 0, 1);
    fill_vram(&dev, 0x60000, &[1, 2, 3, 4]);
    gr(&mut dev, 0x33, blit::EXT_SOLID);
    blit(&mut dev, 0, 0x0d, 4, 1, 640, 640, 0, 0x60000);
    assert_eq!(vram(&dev, 0, 4), [1, 2, 3, 4]);
}

/// mode=0xd0: the same fill at 16 bits per pixel, from GR1/GR11.
#[test]
fn capture_pattern_fill_at_sixteen_bits() {
    let mut dev = device();
    fill_vram(&dev, 0x60c08, &[0xf0; 8]);
    gr(&mut dev, 1, 0x34);
    gr(&mut dev, 0x11, 0x12);
    gr(&mut dev, 0, 0x78);
    gr(&mut dev, 0x10, 0x56);
    blit(&mut dev, 0xd0, 0x0d, 16, 2, 1280, 0, 0x100, 0x60c08);
    assert_eq!(
        vram(&dev, 0x100, 16),
        [
            0x34, 0x12, 0x34, 0x12, 0x34, 0x12, 0x34, 0x12, 0x78, 0x56, 0x78, 0x56, 0x78, 0x56,
            0x78, 0x56
        ]
    );
    assert_eq!(vram(&dev, 0x100 + 1280, 2), [0x34, 0x12]);
}

/// mode=0x80 rop=0xd6 w=16 h=16 spitch=1: colour expansion from packed
/// monochrome rows in video memory, two bytes per row with no source pitch.
#[test]
fn capture_colour_expansion_from_video_memory_uses_packed_rows() {
    let mut dev = device();
    let source: Vec<u8> = (0..32)
        .map(|i| if i % 2 == 0 { 0xff } else { 0x00 })
        .collect();
    fill_vram(&dev, 0x60c08, &source);
    fill_vram(&dev, 0x47bc8, &[0x0f; 16]);
    fill_vram(&dev, 0x47bc8 + 640 * 15, &[0x0f; 16]);
    gr(&mut dev, 1, 0xff);
    gr(&mut dev, 0, 0x00);
    blit(&mut dev, 0x80, 0xd6, 16, 16, 640, 1, 0x47bc8, 0x60c08);
    // ~src | dst: foreground 0xff gives dst, background 0 gives 0xff.
    let row = vram(&dev, 0x47bc8, 16);
    assert_eq!(row[..8], [0x0f; 8]);
    assert_eq!(row[8..], [0xff; 8]);
    assert_eq!(vram(&dev, 0x47bc8 + 640 * 15, 16)[..8], [0x0f; 8]);
}

/// mode=0x01 w=79 dpitch=640 daddr=0x478fc saddr=0x478fa and mode=0x00 w=1
/// h=19: overlapping copies in both directions.
#[test]
fn capture_screen_copies_overlap_in_both_directions() {
    let mut dev = device();
    let row: Vec<u8> = (0..100).collect();
    fill_vram(&dev, 0x478fa - 78, &row);
    // Backwards: the addresses name the last byte; a right shift by 2.
    blit(&mut dev, 0x01, 0x0d, 79, 1, 640, 640, 0x478fc, 0x478fa);
    let copied = vram(&dev, 0x478fa - 78, 100);
    assert_eq!(copied[..2], row[..2]);
    assert_eq!(copied[2..81], row[..79]);
    assert_eq!(copied[81..], row[81..]);
    // Forwards: a left shift by 2 of a 19-row column.
    for y in 0..19 {
        fill_vram(&dev, 0x47b2c + y * 640, &[y as u8 + 1]);
    }
    blit(&mut dev, 0x00, 0x0d, 1, 19, 640, 640, 0x47b2e, 0x47b2c);
    for y in 0..19 {
        assert_eq!(vram(&dev, 0x47b2e + y * 640, 1), [y as u8 + 1]);
    }
    // A forward copy over its own source repeats, as the sequential hardware.
    fill_vram(&dev, 0x1000, &[1, 2, 3, 4, 5, 6]);
    blit(&mut dev, 0x00, 0x0d, 4, 1, 640, 640, 0x1002, 0x1000);
    assert_eq!(vram(&dev, 0x1000, 6), [1, 2, 1, 2, 1, 2]);
}

#[test]
fn every_capture_rop_combines_source_and_destination() {
    let mut dev = device();
    for code in [0x05, 0x0d, 0x59, 0xd6] {
        fill_vram(&dev, 0x2000, &[0b1100]);
        fill_vram(&dev, 0x3000, &[0b1010]);
        blit(&mut dev, 0x00, code, 1, 1, 0, 0, 0x3000, 0x2000);
        assert_eq!(
            vram(&dev, 0x3000, 1),
            [rop(code, 0b1100, 0b1010)],
            "{code:#x}"
        );
    }
}

#[test]
fn transparency_skips_clear_bits_and_key_colours() {
    let mut dev = device();
    fill_vram(&dev, 0x100, &[0b1000_0001]);
    fill_vram(&dev, 0x200, &[9; 8]);
    gr(&mut dev, 1, 5);
    blit(&mut dev, 0x88, 0x0d, 8, 1, 0, 0, 0x200, 0x100);
    assert_eq!(vram(&dev, 0x200, 8), [5, 9, 9, 9, 9, 9, 9, 5]);
    // Inverted expansion through GR33 bit 1.
    gr(&mut dev, 0x33, 0x02);
    blit(&mut dev, 0x88, 0x0d, 8, 1, 0, 0, 0x200, 0x100);
    assert_eq!(vram(&dev, 0x200, 8), [5; 8]);
    // Solid fill through GR33 bit 2 ignores the source.
    gr(&mut dev, 0x33, 0x04);
    gr(&mut dev, 1, 6);
    blit(&mut dev, 0x80, 0x0d, 8, 1, 0, 0, 0x200, 0x100);
    assert_eq!(vram(&dev, 0x200, 8), [6; 8]);
    gr(&mut dev, 0x33, 0);
    // Key transparency for a 16-bit copy: GR34/35 key, GR36/37 don't-care bits.
    fill_vram(&dev, 0x300, &[0x34, 0x12, 0x35, 0x12, 0x00, 0x56]);
    fill_vram(&dev, 0x400, &[0xee; 6]);
    gr(&mut dev, 0x34, 0x34);
    gr(&mut dev, 0x35, 0x12);
    gr(&mut dev, 0x36, 0x01);
    blit(&mut dev, 0x18, 0x0d, 6, 1, 0, 0, 0x400, 0x300);
    assert_eq!(vram(&dev, 0x400, 6), [0xee, 0xee, 0xee, 0xee, 0x00, 0x56]);
}

#[test]
fn colour_expansion_at_twenty_four_and_thirty_two_bits_and_left_clipping() {
    let mut dev = device();
    fill_vram(&dev, 0x100, &[0b1100_0000]);
    for (index, value) in [(1, 1), (0x11, 2), (0x13, 3), (0x15, 4)] {
        gr(&mut dev, index, value);
    }
    blit(&mut dev, 0xa0, 0x0d, 9, 1, 0, 0, 0x200, 0x100);
    assert_eq!(vram(&dev, 0x200, 9), [1, 2, 3, 1, 2, 3, 0, 0, 0]);
    blit(&mut dev, 0xb0, 0x0d, 12, 1, 0, 0, 0x300, 0x100);
    assert_eq!(vram(&dev, 0x300, 12), [1, 2, 3, 4, 1, 2, 3, 4, 0, 0, 0, 0]);
    gr(&mut dev, 0x2f, 1);
    fill_vram(&dev, 0x400, &[9; 4]);
    blit(&mut dev, 0x80, 0x0d, 4, 1, 0, 0, 0x400, 0x100);
    assert_eq!(vram(&dev, 0x400, 4), [9, 1, 0, 0], "first pixel clipped");
}

#[test]
fn pattern_copy_repeats_an_eight_by_eight_colour_tile() {
    let mut dev = device();
    let tile: Vec<u8> = (0..64).collect();
    fill_vram(&dev, 0x1000, &tile);
    blit(&mut dev, 0x40, 0x0d, 10, 9, 16, 0, 0x2000, 0x1000);
    assert_eq!(vram(&dev, 0x2000, 10), [0, 1, 2, 3, 4, 5, 6, 7, 0, 1]);
    assert_eq!(
        vram(&dev, 0x2000 + 16 * 8, 2),
        [0, 1],
        "row 8 wraps to row 0"
    );
    assert_eq!(vram(&dev, 0x2000 + 16 * 3, 1), [24]);
    let tile16: Vec<u8> = (0..128).collect();
    fill_vram(&dev, 0x4000, &tile16);
    blit(&mut dev, 0x50, 0x0d, 18, 1, 0, 0, 0x5000, 0x4002);
    assert_eq!(vram(&dev, 0x5000, 4), [32, 33, 34, 35], "preset row 2");
    assert_eq!(vram(&dev, 0x5000 + 16, 2), [32, 33]);
}

#[test]
fn system_source_rows_come_from_the_aperture_and_the_bar0_port() {
    let mut dev = device();
    gr(&mut dev, 1, 0xaa);
    gr(&mut dev, 0, 0x55);
    // Colour expansion of 12 pixels x 2 rows: 2 bytes, padded to a dword.
    blit(&mut dev, 0x84, 0x0d, 12, 2, 640, 0, 0x100, 0);
    assert_eq!(read(&mut dev, 0x3cf) & 0x09, 0x09, "busy while waiting");
    dev.legacy_write(0x1234, &[0xf0, 0x0f, 0xde, 0xad]);
    assert_eq!(
        vram(&dev, 0x100, 12),
        [
            0xaa, 0xaa, 0xaa, 0xaa, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55
        ]
    );
    let mut ignored = [0];
    dev.legacy_read(0, &mut ignored);
    assert_eq!(ignored, [0xff]);
    dev.bar_write(0, 16 << 20, &[0xff, 0xff]);
    assert!(dev.blit.is_some());
    dev.bar_write(0, (16 << 20) + 2, &[0, 0]);
    assert!(dev.blit.is_none());
    assert_eq!(read(&mut dev, 0x3cf) & 0x09, 0);
    assert_eq!(vram(&dev, 0x100 + 640, 12), [0xaa; 12]);
    // A copy from the CPU: five bytes per row, padded to eight.
    blit(&mut dev, 0x04, 0x59, 5, 1, 0, 0, 0x300, 0);
    fill_vram(&dev, 0x300, &[0xff; 5]);
    dev.legacy_write(0, &[1, 2, 3, 4, 5, 6, 7]);
    assert!(dev.blit.is_some());
    dev.legacy_write(0, &[8]);
    assert_eq!(vram(&dev, 0x300, 6), [0xfe, 0xfd, 0xfc, 0xfb, 0xfa, 0]);
    // GR31 bit 2 aborts a waiting BitBLT.
    blit(&mut dev, 0x04, 0x0d, 4, 4, 0, 0, 0x400, 0);
    gr(&mut dev, 0x31, 0x04);
    assert!(dev.blit.is_none());
    gr(&mut dev, 0x31, 0);
    sr(&mut dev, 4, 0x0e);
    gr(&mut dev, 6, 0x05);
    dev.legacy_write(0, &[7]);
    assert_eq!(vram(&dev, 0, 1), [7], "aperture writes resume as memory");
}

#[test]
fn mmio_registers_at_bar1_and_b8000_with_autostart() {
    let mut dev = device();
    fill_vram(&dev, 0x500, &[1, 2, 3, 4]);
    let program = |dev: &mut CirrusVga, write: &dyn Fn(&mut CirrusVga, usize, u8)| {
        for (offset, value) in [
            (0x08, 3),
            (0x09, 0),
            (0x0a, 0),
            (0x0b, 0),
            (0x14, 0x00),
            (0x15, 0x05),
            (0x16, 0x00),
            (0x18, 0x00),
            (0x1a, 0x0d),
            (0x10, 0x00),
            (0x11, 0x06),
        ] {
            write(dev, offset, value);
        }
        write(dev, 0x12, 0);
    };
    // BAR1: BitBLT registers at 0x100, GR31 at 0x140 with autostart.
    dev.bar_write(1, 0x140, &[0x80]);
    program(&mut dev, &|dev, offset, value| {
        dev.bar_write(1, 0x100 + offset as u64, &[value])
    });
    assert_eq!(vram(&dev, 0x600, 4), [1, 2, 3, 4]);
    let mut value = [0];
    dev.bar_read(1, 0x118, &mut value);
    assert_eq!(value, [0]);
    dev.bar_read(1, 0x10a, &mut value);
    assert_eq!(value, [0]);
    // BAR1 low half reaches the VGA ports: 0x3c4/0x3c5 at offsets 4 and 5.
    dev.bar_write(1, 4, &[6, 0x12]);
    assert_eq!(indexed_read(&mut dev, 0x3c4, 6), 0x12);
    // SR17 bit 2 maps the same registers at 0xb8000; GR31 start bit at 0x40.
    sr(&mut dev, 0x17, 0x04);
    dev.bar_write(1, 0x140, &[0]);
    program(&mut dev, &|dev, offset, value| {
        let target = if offset == 0x11 { 0x07 } else { value };
        dev.legacy_write(0x18000 + offset as u64, &[target])
    });
    assert_eq!(vram(&dev, 0x700, 4), [0; 4], "no autostart");
    dev.legacy_write(0x18040, &[0x02]);
    assert_eq!(vram(&dev, 0x700, 4), [1, 2, 3, 4]);
    dev.legacy_read(0x18015, &mut value);
    assert_eq!(value, [0x05]);
    sr(&mut dev, 0x17, 0x44);
    dev.legacy_read(0x18015, &mut value);
    assert_ne!(value, [0x05], "SR17 bit 6 removes the 0xb8000 window");
}

#[test]
fn snapshot_restores_extended_state_and_a_waiting_blit() {
    let mut dev = device();
    mode(&mut dev, 640, 480, 3, 2);
    for _ in 0..4 {
        read(&mut dev, 0x3c6);
    }
    dev.io_write(0x3c6, &[0xc1]);
    sr(&mut dev, 0x30, 4);
    gr(&mut dev, 1, 0xaa);
    blit(&mut dev, 0x84, 0x0d, 8, 2, 640, 0, 0x100, 0);
    dev.legacy_write(0, &[0xff, 0xff]);
    fill_vram(&dev, 0x9000, &[0x5a]);
    let saved = dev.save_state().unwrap();
    let frame = dev.frame().pixels;
    let mut other = device();
    other.restore_state(&saved).unwrap();
    assert_eq!(other.hdr, 0xc1);
    assert_eq!(other.cursor_x, 33);
    assert_eq!(other.frame().pixels, frame);
    // Two bytes complete row 0; row 1 is all foreground.
    other.legacy_write(0, &[0xff, 0xff, 0xff, 0, 0, 0]);
    assert!(other.blit.is_none());
    assert_eq!(vram(&other, 0x100 + 640, 8), [0xaa; 8]);
    assert_eq!(vram(&other, 0x9000, 1), [0x5a]);
    // Reset returns to the power-on state and keeps the storage mapping.
    let mapping = other.linear_framebuffer();
    other.reset().unwrap();
    assert_eq!(indexed_read(&mut other, 0x3c4, 6), 0x0f);
    assert!(other.blit.is_none());
    assert_eq!(vram(&other, 0x9000, 1), [0]);
    use vm_memory::GuestMemoryBackend;
    assert_eq!(
        mapping.get_host_address(GuestAddress(0)).unwrap(),
        other
            .linear_framebuffer()
            .get_host_address(GuestAddress(0))
            .unwrap()
    );
    // Bad snapshots are refused.
    let mut bad = saved.clone();
    bad[0] = b'X';
    assert!(other.restore_state(&bad).is_err());
    let std_vga = StandardVga::new("vga", &[0x55, 0xaa, 1]).unwrap();
    assert!(other.restore_state(&std_vga.save_state().unwrap()).is_err());
    let mut truncated = saved.clone();
    truncated.truncate(saved.len() - 4);
    assert!(other.restore_state(&truncated).is_err());
}

#[test]
fn vga_modes_still_use_the_core() {
    let mut dev = device();
    // Mode 13h: chain-4 through the VGA core.
    sr(&mut dev, 4, 0x0e);
    gr(&mut dev, 5, 0x40);
    gr(&mut dev, 6, 0x05);
    dev.legacy_write(5, &[0x3c]);
    assert_eq!(vram(&dev, 5, 1), [0x3c]);
    let mut text = device();
    cr(&mut text, 1, 79);
    assert_eq!(text.frame().mode.width, 720);
}
