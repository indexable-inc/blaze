//! A PNG encoder (W3C, "Portable Network Graphics (PNG) Specification", third edition, 2025;
//! the same format as ISO/IEC 15948:2004): 8-bit RGBA (colour type 6) or 8-bit grey (colour
//! type 0), not interlaced.
//!
//! The file is the signature, `IHDR`, one `IDAT` per 1 GiB of compressed data (one, for every
//! image under the shipped [`Limits`]), and `IEND`; each chunk is its data's length (big-endian),
//! its type, its data and the CRC-32 (ISO 3309, blaze-deflate's) of its type and data.
//!
//! Each row is filtered (section 7.3) with whichever of the five filters gives the least sum of
//! absolute values of its bytes read as signed (the heuristic of the specification's section
//! 12.7, libpng's default), ties to the lower filter type; the filtered rows are one zlib stream
//! (blaze-deflate, at the level asked). Every step is a function of the pixels alone, so the
//! bytes are too.

#![blaze::sans_io]

use blaze_deflate::{Crc32, DeflateError, Level, zlib};
use blaze_int::{Int, idx};
use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{Alloc, AllocFailure, Vec};

use crate::Limits;
use crate::plant::PngPlant;

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// The most bytes one `IDAT` carries (the format's own cap is `2^31 − 1`).
const MOST_IDAT: usize = 1 << 30;

/// The format's cap on a width or height.
const MOST_SIDE: u32 = (1 << 31) - 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PngKind {
    /// One byte per pixel, colour type 0.
    Grey8,
    /// Four bytes per pixel, red, green, blue, alpha, colour type 6 (alpha not premultiplied).
    Rgba8,
}

impl PngKind {
    pub fn channels(self) -> u32 {
        match self {
            PngKind::Grey8 => 1,
            PngKind::Rgba8 => 4,
        }
    }

    fn colour_type(self) -> u8 {
        match self {
            PngKind::Grey8 => 0,
            PngKind::Rgba8 => 6,
        }
    }
}

/// Pixels row by row from the top, each row left to right, `channels` bytes each.
#[derive(Clone, Copy, Debug)]
pub struct Image<'p> {
    pub width: u32,
    pub height: u32,
    pub kind: PngKind,
    pub pixels: &'p [u8],
}

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct PngOpts<'a, 's> {
    pub level: Level,
    pub limits: Limits,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PngError {
    /// A width or height of zero, which PNG does not allow.
    Empty { width: u32, height: u32 },
    /// A width or height past `2^31 − 1`.
    TooWide { width: u32, height: u32 },
    TooManyPixels { pixels: u64, most: u64 },
    /// `pixels` is not `width · height · channels` bytes long.
    Length { want: u64, got: usize },
    Deflate(DeflateError),
    Alloc(AllocFailure),
}

impl From<AllocFailure> for PngError {
    fn from(e: AllocFailure) -> PngError {
        PngError::Alloc(e)
    }
}

pub fn png<'a>(image: Image<'_>, opts: PngOpts<'a, '_>) -> Result<Vec<u8, Alloc<'a>>, PngError> {
    png_planted(image, opts, NoPlant)
}

/// One row and the row above it (zeros above the first), `bpp` bytes per pixel.
struct Rows<'r, P> {
    row: &'r [u8],
    prior: &'r [u8],
    bpp: usize,
    plant: P,
}

impl<P: Planted<PngPlant>> Rows<'_, P> {
    /// The Paeth predictor (section 9.4), its ties as the specification breaks them: left, then
    /// up, then upper-left.
    fn paeth(&self, a: u8, b: u8, c: u8) -> u8 {
        let p = i16::from(a) + i16::from(b) - i16::from(c);
        let (pa, pb, pc) = ((p - i16::from(a)).abs(), (p - i16::from(b)).abs(), (p - i16::from(c)).abs());
        if pa <= pb && pa <= pc {
            a
        } else if self.plant.has(PngPlant::PaethTie) {
            if pc <= pb { c } else { b }
        } else if pb <= pc {
            b
        } else {
            c
        }
    }

    /// The row filtered with filter `kind` (section 9.2) into `out`.
    fn filter(&self, kind: u8, out: &mut [u8]) {
        let (row, prior, bpp) = (self.row, self.prior, self.bpp);
        for i in 0..row.len() {
            let x = row[i];
            let a = if i >= bpp { row[i - bpp] } else { 0 };
            let b = prior[i];
            let c = if i >= bpp { prior[i - bpp] } else { 0 };
            out[i] = match kind {
                0 => x,
                1 => x.wrapping_sub(a),
                2 => x.wrapping_sub(b),
                // The mean of two bytes is a byte.
                3 => x.wrapping_sub(((u16::from(a) + u16::from(b)) / 2).narrow_or_stop::<u8>()),
                _ => x.wrapping_sub(self.paeth(a, b, c)),
            };
        }
    }
}

/// The heuristic's cost: each byte read as signed, its magnitude summed.
fn cost(bytes: &[u8]) -> u64 {
    bytes.iter().map(|&x| u64::from(if x < 128 { x } else { x.wrapping_neg() })).sum()
}

/// The file as its chunks are appended.
struct Chunks<'o, 'a, P> {
    out: &'o mut Vec<u8, Alloc<'a>>,
    plant: P,
}

impl<P: Planted<PngPlant>> Chunks<'_, '_, P> {
    /// A chunk: length, type, data, CRC of type and data.
    fn put(&mut self, kind: &[u8; 4], data: &[u8]) -> Result<(), PngError> {
        // At most `MOST_IDAT` bytes (13 for IHDR): below `2^31`.
        let len: u32 = data.len().narrow_or_stop();
        self.out.try_extend_from_slice(&len.to_be_bytes())?;
        self.out.try_extend_from_slice(kind)?;
        self.out.try_extend_from_slice(data)?;
        let mut crc = Crc32::new();
        if !self.plant.has(PngPlant::CrcWithoutType) {
            crc.update(kind);
        }
        crc.update(data);
        self.out.try_extend_from_slice(&crc.finish().to_be_bytes())?;
        Ok(())
    }
}

/// `len` zero bytes in `alloc`.
fn zeroed(len: usize, alloc: Alloc<'_>) -> Result<Vec<u8, Alloc<'_>>, AllocFailure> {
    let mut v = Vec::try_with_capacity_in(len, alloc)?;
    for _ in 0..len {
        v.try_push(0).map_err(|r| r.why)?;
    }
    Ok(v)
}

/// [`png`] with a planted bug, for the tests' controls.
pub fn png_planted<'a>(
    image: Image<'_>,
    opts: PngOpts<'a, '_>,
    plant: impl Planted<PngPlant>,
) -> Result<Vec<u8, Alloc<'a>>, PngError> {
    let (width, height) = (image.width, image.height);
    if width == 0 || height == 0 {
        return Err(PngError::Empty { width, height });
    }
    if width > MOST_SIDE || height > MOST_SIDE {
        return Err(PngError::TooWide { width, height });
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > opts.limits.most_pixels {
        return Err(PngError::TooManyPixels { pixels, most: opts.limits.most_pixels });
    }
    let channels = image.kind.channels();
    let want = pixels * u64::from(channels);
    if want != image.pixels.len().widen_to::<u64>() {
        return Err(PngError::Length { want, got: image.pixels.len() });
    }
    // `want` equals a slice's length, so a row and the whole fit in `usize`.
    let stride = idx(width) * idx(channels);
    let bpp = idx(channels);
    let s = opts.scratch;

    // The filtered rows: a filter byte, then the row.
    let mut raw: Vec<u8, Alloc<'_>> = Vec::try_with_capacity_in(image.pixels.len() + idx(height), s)?;
    let zeros = zeroed(stride, s)?;
    let mut trial = zeroed(stride * 5, s)?;
    for y in 0..idx(height) {
        let row = &image.pixels[y * stride..(y + 1) * stride];
        let prior = if y == 0 { &zeros[..] } else { &image.pixels[(y - 1) * stride..y * stride] };
        let rows = Rows { row, prior, bpp, plant };
        let mut best = (u64::MAX, 0u8);
        for kind in 0..5u8 {
            let at = usize::from(kind) * stride;
            let out = &mut trial[at..at + stride];
            rows.filter(kind, out);
            let c = cost(out);
            if c < best.0 {
                best = (c, kind);
            }
        }
        let at = usize::from(best.1) * stride;
        raw.try_extend_from_slice(&[best.1])?;
        raw.try_extend_from_slice(&trial[at..at + stride])?;
    }
    let mut compressed: Vec<u8, Alloc<'_>> = Vec::new_in(s);
    zlib(&raw, opts.level, s, &mut compressed).map_err(PngError::Deflate)?;

    let mut out: Vec<u8, Alloc<'a>> = Vec::try_with_capacity_in(8 + 25 + compressed.len() + 12 + 12, opts.alloc)?;
    out.try_extend_from_slice(&SIGNATURE)?;
    let mut ihdr = [0u8; 13];
    ihdr[0..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    // Bit depth 8, the colour type, compression 0 (zlib), filter method 0, no interlace.
    ihdr[8] = 8;
    ihdr[9] = image.kind.colour_type();
    let mut chunks = Chunks { out: &mut out, plant };
    chunks.put(b"IHDR", &ihdr)?;
    for piece in compressed.chunks(MOST_IDAT) {
        chunks.put(b"IDAT", piece)?;
    }
    chunks.put(b"IEND", &[])?;
    Ok(out)
}
