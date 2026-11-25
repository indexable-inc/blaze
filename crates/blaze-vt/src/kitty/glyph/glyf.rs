//! A TrueType `glyf` entry: simple glyphs only, no hinting (ghostty's font/opentype/glyf.zig,
//! the part the glyph protocol uses).

use blaze_own::{AVec, Alloc};

/// The 10-byte header every entry starts with (big-endian).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Header {
    /// At least 0 for a simple glyph; negative for a composite one.
    pub number_of_contours: i16,
    pub x_min: i16,
    pub y_min: i16,
    pub x_max: i16,
    pub y_max: i16,
}

impl Header {
    pub const SIZE: usize = 10;
}

/// Why an entry is refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GlyfError {
    /// The data ended before the entry did.
    EndOfStream,
    /// The entry has hinting instructions.
    InstructionsNotSupported,
    /// The entry is a composite glyph.
    CompositeNotSupported,
    /// The contour end points do not strictly increase.
    EndPointsOutOfOrder,
    /// The flags name more points than the last end point.
    TooManyPoints,
    /// A coordinate left `i32`.
    CoordinateOverflow,
    OutOfMemory,
}

/// A point of a contour, in font units.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Point {
    pub x: i32,
    pub y: i32,
    pub on_curve: bool,
}

/// A decoded simple glyph: the end point of each contour, and every point in order.
#[derive(Debug)]
pub struct Outline<'a> {
    pub contours: AVec<'a, u16>,
    pub points: AVec<'a, Point>,
}

impl Outline<'_> {
    /// The points of contour `index`.
    pub fn contour(&self, index: usize) -> &[Point] {
        let start = match index {
            0 => 0,
            i => usize::from(self.contours[i - 1]) + 1,
        };
        let end = usize::from(self.contours[index]) + 1;
        &self.points[start..end]
    }
}

/// One entry: its header and the bytes after it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry<'d> {
    pub header: Header,
    pub data: &'d [u8],
}

/// A simple-glyph flag byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Flags(u8);

impl Flags {
    const ON_CURVE: u8 = 0x01;
    const X_SHORT: u8 = 0x02;
    const Y_SHORT: u8 = 0x04;
    const REPEAT: u8 = 0x08;
    const X_SAME_OR_POSITIVE: u8 = 0x10;
    const Y_SAME_OR_POSITIVE: u8 = 0x20;

    fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }

    /// Bytes of the coordinate: 1 short, 0 the same as before, 2 a signed delta.
    fn coord_bytes(self, short: u8, same: u8) -> usize {
        if self.has(short) {
            1
        } else if self.has(same) {
            0
        } else {
            2
        }
    }
}

/// A cursor over big-endian bytes.
struct Reader<'d> {
    data: &'d [u8],
    pos: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, GlyfError> {
        let b = *self.data.get(self.pos).ok_or(GlyfError::EndOfStream)?;
        self.pos += 1;
        Ok(b)
    }

    fn u16(&mut self) -> Result<u16, GlyfError> {
        Ok(u16::from_be_bytes([self.byte()?, self.byte()?]))
    }

    fn i16(&mut self) -> Result<i16, GlyfError> {
        Ok(self.u16()? as i16)
    }

    fn skip(&mut self, n: usize) -> Result<(), GlyfError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.data.len())
            .ok_or(GlyfError::EndOfStream)?;
        self.pos = end;
        Ok(())
    }
}

impl<'d> Entry<'d> {
    /// The entry at the start of `data` (`Entry.init`): only the header is read.
    pub fn new(data: &'d [u8]) -> Result<Entry<'d>, GlyfError> {
        let mut r = Reader { data, pos: 0 };
        let header = Header {
            number_of_contours: r.i16()?,
            x_min: r.i16()?,
            y_min: r.i16()?,
            x_max: r.i16()?,
            y_max: r.i16()?,
        };
        Ok(Entry { header, data: &data[Header::SIZE..] })
    }

    pub fn is_composite(&self) -> bool {
        self.header.number_of_contours < 0
    }

    /// The bytes the entry takes, header included (`size`).
    pub fn size(&self) -> Result<usize, GlyfError> {
        if self.is_composite() {
            return Err(GlyfError::CompositeNotSupported);
        }
        let num_contours = self.header.number_of_contours as usize;
        if num_contours == 0 && self.data.len() < 2 {
            return Ok(Header::SIZE);
        }
        let mut r = Reader { data: self.data, pos: 0 };
        let mut max_point_index: i64 = -1;
        for _ in 0..num_contours {
            let index = i64::from(r.u16()?);
            if index <= max_point_index {
                return Err(GlyfError::EndPointsOutOfOrder);
            }
            max_point_index = index;
        }
        if r.u16()? > 0 {
            return Err(GlyfError::InstructionsNotSupported);
        }
        let (mut x_len, mut y_len) = (0usize, 0usize);
        let mut i: i64 = 0;
        while i <= max_point_index {
            let flag = Flags(r.byte()?);
            let xb = flag.coord_bytes(Flags::X_SHORT, Flags::X_SAME_OR_POSITIVE);
            let yb = flag.coord_bytes(Flags::Y_SHORT, Flags::Y_SAME_OR_POSITIVE);
            x_len += xb;
            y_len += yb;
            if flag.has(Flags::REPEAT) {
                let count = r.byte()?;
                i += i64::from(count);
                x_len += usize::from(count) * xb;
                y_len += usize::from(count) * yb;
                if i > max_point_index {
                    return Err(GlyfError::TooManyPoints);
                }
            }
            i += 1;
        }
        r.skip(x_len)?;
        r.skip(y_len)?;
        Ok(Header::SIZE + r.pos)
    }

    /// The outline (`decode`).
    pub fn decode<'a>(&self, alloc: Alloc<'a>) -> Result<Outline<'a>, GlyfError> {
        if self.is_composite() {
            return Err(GlyfError::CompositeNotSupported);
        }
        let oom = |_| GlyfError::OutOfMemory;
        let num_contours = self.header.number_of_contours as usize;
        let mut contours: AVec<'a, u16> = AVec::new_in(alloc);
        let mut points: AVec<'a, Point> = AVec::new_in(alloc);
        if num_contours == 0 && self.data.len() < 2 {
            return Ok(Outline { contours, points });
        }
        let mut r = Reader { data: self.data, pos: 0 };
        if num_contours == 0 {
            if r.u16()? > 0 {
                return Err(GlyfError::InstructionsNotSupported);
            }
            return Ok(Outline { contours, points });
        }
        contours.try_reserve(num_contours).map_err(oom)?;
        let mut prev: i64 = -1;
        for _ in 0..num_contours {
            let index = r.u16()?;
            if i64::from(index) <= prev {
                return Err(GlyfError::EndPointsOutOfOrder);
            }
            prev = i64::from(index);
            contours.try_push(index).map_err(|_| GlyfError::OutOfMemory)?;
        }
        let point_count = usize::from(contours[num_contours - 1]) + 1;
        if r.u16()? > 0 {
            return Err(GlyfError::InstructionsNotSupported);
        }
        let mut flags: AVec<'a, Flags> = AVec::new_in(alloc);
        flags.try_reserve(point_count).map_err(oom)?;
        while flags.len() < point_count {
            let flag = Flags(r.byte()?);
            let _ = flags.try_push(flag);
            if flag.has(Flags::REPEAT) {
                let count = usize::from(r.byte()?);
                if flags.len() + count > point_count {
                    return Err(GlyfError::TooManyPoints);
                }
                for _ in 0..count {
                    let _ = flags.try_push(flag);
                }
            }
        }
        points.try_reserve(point_count).map_err(oom)?;
        let mut x: i32 = 0;
        for &flag in flags.iter() {
            let dx = delta(&mut r, flag, Flags::X_SHORT, Flags::X_SAME_OR_POSITIVE)?;
            x = x.checked_add(dx).ok_or(GlyfError::CoordinateOverflow)?;
            let _ = points.try_push(Point { x, y: 0, on_curve: flag.has(Flags::ON_CURVE) });
        }
        let mut y: i32 = 0;
        for (&flag, point) in flags.iter().zip(points.iter_mut()) {
            let dy = delta(&mut r, flag, Flags::Y_SHORT, Flags::Y_SAME_OR_POSITIVE)?;
            y = y.checked_add(dy).ok_or(GlyfError::CoordinateOverflow)?;
            point.y = y;
        }
        Ok(Outline { contours, points })
    }
}

/// One coordinate delta as its flag says.
fn delta(
    r: &mut Reader<'_>,
    flag: Flags,
    short: u8,
    same_or_positive: u8,
) -> Result<i32, GlyfError> {
    Ok(if flag.has(short) {
        let b = i32::from(r.byte()?);
        if flag.has(same_or_positive) { b } else { -b }
    } else if !flag.has(same_or_positive) {
        i32::from(r.i16()?)
    } else {
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kitty::storage::tests::alloc;
    use std::vec::Vec;

    fn header(buf: &mut Vec<u8>, contours: i16, x_min: i16, y_min: i16, x_max: i16, y_max: i16) {
        for v in [contours, x_min, y_min, x_max, y_max] {
            buf.extend_from_slice(&v.to_be_bytes());
        }
    }

    fn u16s(buf: &mut Vec<u8>, vs: &[u16]) {
        for v in vs {
            buf.extend_from_slice(&v.to_be_bytes());
        }
    }

    fn i16s(buf: &mut Vec<u8>, vs: &[i16]) {
        for v in vs {
            buf.extend_from_slice(&v.to_be_bytes());
        }
    }

    const fn pt(x: i32, y: i32, on_curve: bool) -> Point {
        Point { x, y, on_curve }
    }

    /// A hint-free simple glyph standing in for Cozette's `\0` (the font is not in the tree):
    /// two contours of four points with short, long and repeated flags.
    fn simple_fixture() -> Vec<u8> {
        let mut b = Vec::new();
        header(&mut b, 2, 0, 0, 20, 20);
        u16s(&mut b, &[3, 7, 0]);
        // Points 0..=3: on, x/y short positive, repeat 3. Points 4..=7: long deltas.
        b.extend_from_slice(&[0x01 | 0x02 | 0x04 | 0x10 | 0x20 | 0x08, 3, 0x01, 0x01, 0x01, 0x01]);
        b.extend_from_slice(&[1, 2, 3, 4]);
        i16s(&mut b, &[5, -5, 5, -5]);
        b.extend_from_slice(&[1, 2, 3, 4]);
        i16s(&mut b, &[6, -6, 6, -6]);
        b
    }

    // ghostty glyf.zig:674 "glyf" (a synthetic glyph in place of the Cozette font fixture)
    #[test]
    fn glyf() {
        let data = simple_fixture();
        let entry = Entry::new(&data).unwrap();
        assert!(!entry.is_composite());
        assert_eq!(entry.size(), Ok(data.len()));
        let outline = entry.decode(alloc()).unwrap();
        assert_eq!(outline.contours.len(), entry.header.number_of_contours as usize);
        assert_eq!(outline.points.len(), 8);
        // Extra bytes after an entry are allowed: its size is still its own.
        let mut longer = data.clone();
        longer.extend_from_slice(&[0, 0, 0]);
        assert_eq!(Entry::new(&longer).unwrap().size(), Ok(data.len()));
    }

    // ghostty glyf.zig:712 "glyf: decode triangle"
    #[test]
    fn glyf_decode_triangle() {
        let mut b = Vec::new();
        header(&mut b, 1, 100, 100, 900, 900);
        u16s(&mut b, &[2, 0]);
        b.extend_from_slice(&[0x01, 0x01, 0x01]);
        i16s(&mut b, &[500, -400, 800, 900, -800, 0]);
        let glyph = Entry::new(&b).unwrap();
        let outline = glyph.decode(alloc()).unwrap();
        assert_eq!((glyph.header.x_min, glyph.header.x_max), (100, 900));
        assert_eq!((outline.contours.len(), outline.points.len()), (1, 3));
        assert_eq!(
            outline.contour(0),
            [pt(500, 900, true), pt(100, 100, true), pt(900, 100, true)]
        );
    }

    // ghostty glyf.zig:747 "glyf: decode multiple contours"
    #[test]
    fn glyf_decode_multiple_contours() {
        let mut b = Vec::new();
        header(&mut b, 2, 0, 0, 30, 10);
        u16s(&mut b, &[1, 3, 0]);
        b.extend_from_slice(&[0x01; 4]);
        i16s(&mut b, &[0, 10, 10, 10, 0, 0, 10, 0]);
        let outline = Entry::new(&b).unwrap().decode(alloc()).unwrap();
        assert_eq!(outline.contours.as_slice(), [1, 3]);
        assert_eq!(outline.points.len(), 4);
        assert_eq!(outline.contour(0), &outline.points[0..2]);
        assert_eq!(outline.contour(1), &outline.points[2..4]);
    }

    // ghostty glyf.zig:776 "glyf: decode repeat and short vector flags"
    #[test]
    fn glyf_decode_repeat_and_short_vector_flags() {
        let mut b = Vec::new();
        header(&mut b, 1, 0, -16, 16, 0);
        u16s(&mut b, &[3, 0]);
        b.extend_from_slice(&[0x01 | 0x02 | 0x04 | 0x08 | 0x10, 3, 1, 2, 4, 8, 1, 2, 4, 8]);
        let outline = Entry::new(&b).unwrap().decode(alloc()).unwrap();
        assert_eq!(
            outline.points.as_slice(),
            [pt(1, -1, true), pt(3, -3, true), pt(7, -7, true), pt(15, -15, true)]
        );
    }

    // ghostty glyf.zig:802 "glyf: decode off curve and same coordinate flags"
    #[test]
    fn glyf_decode_off_curve_and_same_coordinate_flags() {
        let mut b = Vec::new();
        header(&mut b, 1, 0, 0, 7, 9);
        u16s(&mut b, &[1, 0]);
        b.extend_from_slice(&[0x10 | 0x20, 0x01 | 0x02 | 0x04 | 0x10 | 0x20, 7, 9]);
        let outline = Entry::new(&b).unwrap().decode(alloc()).unwrap();
        assert_eq!(outline.points.as_slice(), [pt(0, 0, false), pt(7, 9, true)]);
    }

    // ghostty glyf.zig:825 "glyf: decode one-point contour"
    #[test]
    fn glyf_decode_one_point_contour() {
        let mut b = Vec::new();
        header(&mut b, 1, 0, 0, 0, 0);
        u16s(&mut b, &[0, 0]);
        b.push(0x01 | 0x10 | 0x20);
        let outline = Entry::new(&b).unwrap().decode(alloc()).unwrap();
        assert_eq!((outline.points.len(), outline.contours.as_slice()), (1, &[0u16][..]));
        assert_eq!(outline.contour(0)[0], pt(0, 0, true));
    }

    // ghostty glyf.zig:847 "glyf: decode contour ending at max point index"
    #[test]
    fn glyf_decode_contour_ending_at_max_point_index() {
        let mut b = Vec::new();
        header(&mut b, 1, 0, 0, 0, 0);
        u16s(&mut b, &[u16::MAX, 0]);
        let flag = 0x01 | 0x10 | 0x20;
        let mut remaining = usize::from(u16::MAX) + 1;
        while remaining > 0 {
            let run = remaining.min(256);
            if run == 1 {
                b.push(flag);
            } else {
                b.extend_from_slice(&[flag | 0x08, (run - 1) as u8]);
            }
            remaining -= run;
        }
        let outline = Entry::new(&b).unwrap().decode(alloc()).unwrap();
        assert_eq!((outline.points.len(), outline.contour(0).len()), (65536, 65536));
    }

    // ghostty glyf.zig:879 "glyf: reject glyphs with instructions and composite glyphs"
    // (synthetic entries in place of the JetBrains Mono fixture)
    #[test]
    fn glyf_reject_glyphs_with_instructions_and_composite_glyphs() {
        let mut b = Vec::new();
        header(&mut b, 1, 0, 0, 0, 0);
        u16s(&mut b, &[0, 2]);
        b.extend_from_slice(&[0xB0, 0x00, 0x01]);
        let hinted = Entry::new(&b).unwrap();
        assert_eq!(hinted.size(), Err(GlyfError::InstructionsNotSupported));
        assert_eq!(hinted.decode(alloc()).unwrap_err(), GlyfError::InstructionsNotSupported);
        let mut b = Vec::new();
        header(&mut b, -1, 0, 0, 0, 0);
        u16s(&mut b, &[0, 1, 0, 0, 0, 0, 0]);
        let composite = Entry::new(&b).unwrap();
        assert!(composite.is_composite());
        assert_eq!(composite.size(), Err(GlyfError::CompositeNotSupported));
        assert_eq!(composite.decode(alloc()).unwrap_err(), GlyfError::CompositeNotSupported);
    }

    // ghostty glyf.zig:916 "glyf: reject truncated"
    #[test]
    fn glyf_reject_truncated() {
        let data = simple_fixture();
        let short = &data[..data.len() - 1];
        let entry = Entry::new(short).unwrap();
        assert_eq!(entry.size(), Err(GlyfError::EndOfStream));
        assert_eq!(entry.decode(alloc()).unwrap_err(), GlyfError::EndOfStream);
        assert_eq!(Entry::new(&data[..9]).unwrap_err(), GlyfError::EndOfStream);
    }

    // ghostty glyf.zig:937 "glyf: reject endpoints out of order"
    #[test]
    fn glyf_reject_endpoints_out_of_order() {
        let mut data = simple_fixture();
        // The second end point (bytes 12..14) set to 0.
        data[12] = 0;
        data[13] = 0;
        let entry = Entry::new(&data).unwrap();
        assert_eq!(entry.size(), Err(GlyfError::EndPointsOutOfOrder));
        assert_eq!(entry.decode(alloc()).unwrap_err(), GlyfError::EndPointsOutOfOrder);
    }

    // ghostty glyf.zig:966 "glyf: reject too many points"
    #[test]
    fn glyf_reject_too_many_points() {
        let mut data = simple_fixture();
        // The last flag (byte 21) repeats 255 times.
        data[21] |= 0x08;
        data[22] = 0xFF;
        let entry = Entry::new(&data).unwrap();
        assert_eq!(entry.size(), Err(GlyfError::TooManyPoints));
        assert_eq!(entry.decode(alloc()).unwrap_err(), GlyfError::TooManyPoints);
    }

    // ghostty glyf.zig:994 "glyf: zero-contour glyph can be header-only"
    #[test]
    fn glyf_zero_contour_glyph_can_be_header_only() {
        let glyph = Entry::new(&[0; 10]).unwrap();
        assert_eq!(glyph.size(), Ok(Header::SIZE));
        let outline = glyph.decode(alloc()).unwrap();
        assert_eq!((outline.points.len(), outline.contours.len()), (0, 0));
    }

    // ghostty glyf.zig:1014 "glyf: zero-contour glyph can include instruction length"
    #[test]
    fn glyf_zero_contour_glyph_can_include_instruction_length() {
        let glyph = Entry::new(&[0; 12]).unwrap();
        assert_eq!(glyph.size(), Ok(Header::SIZE + 2));
        let outline = glyph.decode(alloc()).unwrap();
        assert_eq!((outline.points.len(), outline.contours.len()), (0, 0));
    }

    // ghostty glyf.zig:1033 "glyf: zero-contour glyph rejects instructions"
    #[test]
    fn glyf_zero_contour_glyph_rejects_instructions() {
        let mut b = std::vec![0u8; 10];
        u16s(&mut b, &[1]);
        let glyph = Entry::new(&b).unwrap();
        assert_eq!(glyph.size(), Err(GlyfError::InstructionsNotSupported));
        assert_eq!(glyph.decode(alloc()).unwrap_err(), GlyfError::InstructionsNotSupported);
    }

    /// The widest sum a glyph can reach (65536 deltas of -32768) is exactly `i32::MIN`: it fits.
    #[test]
    fn glyf_coordinate_extreme_fits() {
        let mut b = Vec::new();
        header(&mut b, 1, 0, 0, 0, 0);
        u16s(&mut b, &[u16::MAX, 0]);
        let mut remaining = usize::from(u16::MAX) + 1;
        while remaining > 0 {
            let run = remaining.min(256);
            b.extend_from_slice(&[0x01 | 0x20 | 0x08, (run - 1) as u8]);
            remaining -= run;
        }
        for _ in 0..65536 {
            b.extend_from_slice(&i16::MIN.to_be_bytes());
        }
        let outline = Entry::new(&b).unwrap().decode(alloc()).unwrap();
        assert_eq!(outline.points[65535].x, i32::MIN);
    }
}
