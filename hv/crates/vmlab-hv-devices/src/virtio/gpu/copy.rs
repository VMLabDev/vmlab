//! [`super::CMD_PRESENT_COPY`]: the copy of a present of the Blt kind, run
//! on the host in place of the guest driver's CPU copy
//! (`guest/win3d/docs/present-copy.md`).
//!
//! The command is the header, then the source and destination resource IDs,
//! the byte offset of each image in its resource's memory, the row pitch of
//! each, the pixel mode, the bytes per pixel, the box count and a reserved
//! word, then the boxes: source x and y, destination x and y, width and
//! height, all little-endian `u32`s. The guest driver clips the boxes to both
//! surfaces before it sends them, and the host checks each one again against
//! the memory it has.

/// The most boxes one command carries, the driver's limit of sub-rectangles.
pub(super) const MAX_BOXES: usize = 64;

/// How a pixel changes on the way, the driver's `VIOGPU_BLT_COPY_MODE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    /// Bytes as they are, any pixel size.
    Direct,
    /// 32-bit pixels with red and blue swapped.
    SwapRedBlue,
    /// R10G10B10A2 to R8G8B8A8.
    R10ToRgba8,
    /// R10G10B10A2 to B8G8R8A8.
    R10ToBgra8,
}

impl Mode {
    fn from_wire(mode: u32) -> Option<Self> {
        Some(match mode {
            1 => Self::Direct,
            2 => Self::SwapRedBlue,
            3 => Self::R10ToRgba8,
            4 => Self::R10ToBgra8,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CopyBox {
    pub src_x: u32,
    pub src_y: u32,
    pub dst_x: u32,
    pub dst_y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Request {
    pub src: u32,
    pub dst: u32,
    pub src_offset: u64,
    pub dst_offset: u64,
    pub src_stride: u32,
    pub dst_stride: u32,
    pub mode: Mode,
    pub bpp: u32,
    pub boxes: Vec<CopyBox>,
}

/// Decode the body that follows the 24-byte header.
pub(super) fn decode(body: &[u8]) -> Result<Request, u32> {
    let mut d = super::Decoder::new(body);
    let src = d.u32()?;
    let dst = d.u32()?;
    let src_offset = d.u64()?;
    let dst_offset = d.u64()?;
    let src_stride = d.u32()?;
    let dst_stride = d.u32()?;
    let mode = Mode::from_wire(d.u32()?).ok_or(super::INVALID)?;
    let bpp = d.u32()?;
    let count = d.u32()? as usize;
    d.u32()?;
    if src == 0
        || dst == 0
        || src == dst
        || !(1..=16).contains(&bpp)
        || (mode != Mode::Direct && bpp != 4)
        || !(1..=MAX_BOXES).contains(&count)
    {
        return Err(super::INVALID);
    }
    let mut boxes = Vec::with_capacity(count);
    for _ in 0..count {
        let b = CopyBox {
            src_x: d.u32()?,
            src_y: d.u32()?,
            dst_x: d.u32()?,
            dst_y: d.u32()?,
            width: d.u32()?,
            height: d.u32()?,
        };
        if b.width == 0 || b.height == 0 {
            return Err(super::INVALID);
        }
        boxes.push(b);
    }
    Ok(Request {
        src,
        dst,
        src_offset,
        dst_offset,
        src_stride,
        dst_stride,
        mode,
        bpp,
        boxes,
    })
}

/// Memory of one side of the copy: host segments in address order, as a
/// blob mapping (one segment) or a guest backing (one per guest range).
pub(super) trait Memory {
    fn len(&self) -> u64;
    fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), u32>;
    fn write(&mut self, offset: u64, data: &[u8]) -> Result<(), u32>;
}

/// Host memory the caller guarantees for the duration of the copy: a
/// renderer mapping, or guest RAM behind a resource's backing.
pub(super) struct Segments {
    pub parts: Vec<(usize, usize)>,
}

impl Segments {
    /// Visit the segments that hold `len` bytes from `offset`.
    fn each(
        &self,
        mut offset: u64,
        len: usize,
        mut f: impl FnMut(usize, usize, usize),
    ) -> Result<(), u32> {
        let mut done = 0;
        for &(base, part) in &self.parts {
            if done == len {
                break;
            }
            if offset >= part as u64 {
                offset -= part as u64;
                continue;
            }
            let n = (len - done).min(part - offset as usize);
            f(base + offset as usize, done, n);
            done += n;
            offset = 0;
        }
        if done == len { Ok(()) } else { Err(super::INVALID) }
    }
}

impl Memory for Segments {
    fn len(&self) -> u64 {
        self.parts.iter().map(|&(_, len)| len as u64).sum()
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), u32> {
        let target = out.as_mut_ptr();
        self.each(offset, out.len(), |address, at, n| {
            // SAFETY: the caller keeps every segment mapped and readable for
            // the copy, and `at + n` is within `out`.
            unsafe { std::ptr::copy_nonoverlapping(address as *const u8, target.add(at), n) }
        })
    }
    fn write(&mut self, offset: u64, data: &[u8]) -> Result<(), u32> {
        self.each(offset, data.len(), |address, at, n| {
            // SAFETY: the caller keeps every segment mapped and writable for
            // the copy, and `at + n` is within `data`.
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr().add(at), address as *mut u8, n) }
        })
    }
}

fn convert(mode: Mode, row: &mut [u8]) {
    if mode == Mode::Direct {
        return;
    }
    for pixel in row.as_chunks_mut::<4>().0 {
        let mut value = u32::from_le_bytes(*pixel);
        if matches!(mode, Mode::R10ToRgba8 | Mode::R10ToBgra8) {
            value = r10_to_rgba8(value);
        }
        if matches!(mode, Mode::SwapRedBlue | Mode::R10ToBgra8) {
            value = (value & 0xff00_ff00) | ((value >> 16) & 0xff) | ((value & 0xff) << 16);
        }
        *pixel = value.to_le_bytes();
    }
}

/// The driver's `VirglR10G10B10A2ToR8G8B8A8`: each colour channel scaled
/// from 10 bits to 8 with rounding, alpha from 2 bits to 8.
fn r10_to_rgba8(pixel: u32) -> u32 {
    let scale = |v: u32| ((v & 0x3ff) * 255 + 511) / 1023;
    let r = scale(pixel);
    let g = scale(pixel >> 10);
    let b = scale(pixel >> 20);
    let a = (pixel >> 30) * 85;
    r | (g << 8) | (b << 16) | (a << 24)
}

/// Check every box against both memories, then copy them in order.
/// Nothing is written when any box is out of bounds. Returns the bytes
/// copied.
pub(super) fn run(request: &Request, src: &dyn Memory, dst: &mut dyn Memory) -> Result<u64, u32> {
    let bpp = u64::from(request.bpp);
    let fits = |offset: u64, stride: u32, x: u32, y: u32, b: &CopyBox, len: u64| {
        let row = u64::from(b.width) * bpp;
        if row > u64::from(stride) {
            return false;
        }
        (|| {
            offset
                .checked_add(u64::from(y) * u64::from(stride))?
                .checked_add(u64::from(x) * bpp)?
                .checked_add(u64::from(b.height - 1) * u64::from(stride))?
                .checked_add(row)
        })()
        .is_some_and(|end| end <= len)
    };
    let (src_len, dst_len) = (src.len(), dst.len());
    for b in &request.boxes {
        if !fits(request.src_offset, request.src_stride, b.src_x, b.src_y, b, src_len)
            || !fits(request.dst_offset, request.dst_stride, b.dst_x, b.dst_y, b, dst_len)
        {
            return Err(super::INVALID);
        }
    }
    let mut copied = 0;
    let mut row = Vec::new();
    for b in &request.boxes {
        row.resize((u64::from(b.width) * bpp) as usize, 0);
        for y in 0..u64::from(b.height) {
            let from = request.src_offset
                + (u64::from(b.src_y) + y) * u64::from(request.src_stride)
                + u64::from(b.src_x) * bpp;
            let to = request.dst_offset
                + (u64::from(b.dst_y) + y) * u64::from(request.dst_stride)
                + u64::from(b.dst_x) * bpp;
            src.read(from, &mut row)?;
            convert(request.mode, &mut row);
            dst.write(to, &row)?;
            copied += row.len() as u64;
        }
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Plain(Vec<u8>);
    impl Memory for Plain {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
        fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), u32> {
            let at = offset as usize;
            out.copy_from_slice(self.0.get(at..at + out.len()).ok_or(super::super::INVALID)?);
            Ok(())
        }
        fn write(&mut self, offset: u64, data: &[u8]) -> Result<(), u32> {
            let at = offset as usize;
            self.0
                .get_mut(at..at + data.len())
                .ok_or(super::super::INVALID)?
                .copy_from_slice(data);
            Ok(())
        }
    }

    fn request(mode: Mode, boxes: Vec<CopyBox>) -> Request {
        Request {
            src: 1,
            dst: 2,
            src_offset: 0,
            dst_offset: 16,
            src_stride: 16,
            dst_stride: 32,
            mode,
            bpp: 4,
            boxes,
        }
    }

    fn whole(width: u32, height: u32) -> CopyBox {
        CopyBox { src_x: 0, src_y: 0, dst_x: 1, dst_y: 1, width, height }
    }

    #[test]
    fn boxes_land_at_their_offset_pitch_and_position() {
        let src = Plain((0..64).collect());
        let mut dst = Plain(vec![0xee; 16 + 32 * 4]);
        let copied = run(&request(Mode::Direct, vec![whole(2, 2)]), &src, &mut dst).unwrap();
        assert_eq!(copied, 16);
        // Row 1, pixel 1 of the destination, after the 16-byte image offset.
        assert_eq!(&dst.0[16 + 32 + 4..16 + 32 + 12], &src.0[0..8]);
        assert_eq!(&dst.0[16 + 64 + 4..16 + 64 + 12], &src.0[16..24]);
        assert_eq!(dst.0[16 + 32], 0xee);
        assert_eq!(dst.0[16 + 32 + 12], 0xee);
    }

    #[test]
    fn a_box_out_of_bounds_writes_nothing() {
        let src = Plain(vec![1; 64]);
        let mut dst = Plain(vec![0; 16 + 32 * 4]);
        let boxes = vec![whole(1, 1), whole(2, 5)];
        assert_eq!(run(&request(Mode::Direct, boxes), &src, &mut dst), Err(super::super::INVALID));
        assert!(dst.0.iter().all(|&b| b == 0));
        // A row wider than the pitch is refused too.
        let wide = vec![CopyBox { width: 5, ..whole(5, 1) }];
        assert!(run(&request(Mode::Direct, wide), &src, &mut dst).is_err());
    }

    #[test]
    fn pixels_convert_as_the_driver_converts_them() {
        let pixel = |v: u32| v.to_le_bytes().to_vec();
        let mut src = Plain([pixel(0x1122_3344), vec![0; 12]].concat());
        let mut dst = Plain(vec![0; 16 + 32 * 4]);
        run(&request(Mode::SwapRedBlue, vec![whole(1, 1)]), &src, &mut dst).unwrap();
        assert_eq!(dst.0[16 + 36..16 + 40], pixel(0x1144_3322)[..]);

        // R10G10B10A2 white with alpha 3, and red at full scale.
        src.0[..4].copy_from_slice(&(0xffff_ffffu32).to_le_bytes());
        run(&request(Mode::R10ToRgba8, vec![whole(1, 1)]), &src, &mut dst).unwrap();
        assert_eq!(dst.0[16 + 36..16 + 40], pixel(0xffff_ffff)[..]);
        src.0[..4].copy_from_slice(&(0x3ffu32 | (1 << 30)).to_le_bytes());
        run(&request(Mode::R10ToRgba8, vec![whole(1, 1)]), &src, &mut dst).unwrap();
        assert_eq!(dst.0[16 + 36..16 + 40], pixel(0x5500_00ff)[..]);
        run(&request(Mode::R10ToBgra8, vec![whole(1, 1)]), &src, &mut dst).unwrap();
        assert_eq!(dst.0[16 + 36..16 + 40], pixel(0x55ff_0000)[..]);
    }

    #[test]
    fn segments_split_a_row_across_guest_ranges() {
        let mut first = vec![0u8; 56];
        let mut second = vec![0u8; 104];
        let mut dst = Segments {
            parts: vec![
                (first.as_mut_ptr() as usize, first.len()),
                (second.as_mut_ptr() as usize, second.len()),
            ],
        };
        let src = Plain((0..64).collect());
        run(&request(Mode::Direct, vec![whole(2, 2)]), &src, &mut dst).unwrap();
        // The first row ends 4 bytes into the second range.
        let mut joined = first.clone();
        joined.extend_from_slice(&second);
        assert_eq!(&joined[16 + 36..16 + 44], &src.0[0..8]);
        assert_eq!(&joined[16 + 68..16 + 76], &src.0[16..24]);
        let mut back = vec![0; 8];
        dst.read(16 + 68, &mut back).unwrap();
        assert_eq!(back, src.0[16..24]);
    }

    #[test]
    fn decode_refuses_what_the_driver_never_sends() {
        let mut body = Vec::new();
        for word in [1u32, 2] {
            body.extend_from_slice(&word.to_le_bytes());
        }
        body.extend_from_slice(&[0; 16]);
        for word in [16u32, 16, 2, 4, 1, 0, 0, 0, 0, 0, 2, 2] {
            body.extend_from_slice(&word.to_le_bytes());
        }
        let request = decode(&body).unwrap();
        assert_eq!(request.mode, Mode::SwapRedBlue);
        assert_eq!(request.boxes, vec![CopyBox { src_x: 0, src_y: 0, dst_x: 0, dst_y: 0, width: 2, height: 2 }]);
        // A conversion of pixels that are not 32-bit.
        let mut bad = body.clone();
        bad[36..40].copy_from_slice(&2u32.to_le_bytes());
        assert!(decode(&bad).is_err());
        // An unknown mode, and a box of no width.
        let mut bad = body.clone();
        bad[32..36].copy_from_slice(&9u32.to_le_bytes());
        assert!(decode(&bad).is_err());
        let mut bad = body.clone();
        bad[64..68].copy_from_slice(&0u32.to_le_bytes());
        assert!(decode(&bad).is_err());
    }
}
