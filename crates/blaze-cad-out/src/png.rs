//! A PNG encoder and decoder (W3C, "Portable Network Graphics (PNG) Specification", third
//! edition, 2025; the same format as ISO/IEC 15948:2004): 8-bit RGBA (colour type 6), 8-bit
//! grey (colour type 0), and (on decode) 8-bit RGB (colour type 2, expanded to RGBA8), not
//! interlaced.
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

use blaze_deflate::{Crc32, DeflateError, Level, MAX_RATIO, unzlib, zlib};
use blaze_int::{Int, idx};
use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{Alloc, AllocFailure, Vec};

use crate::Limits;
use crate::plant::PngPlant;

pub(crate) const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

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

/// Pixels decoded from a PNG file, row by row from the top, left to right.
#[derive(PartialEq, Eq, Debug)]
pub struct DecodedPng<'a> {
    pub width: u32,
    pub height: u32,
    pub kind: PngKind,
    pub pixels: Vec<u8, Alloc<'a>>,
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
    /// Missing or wrong 8-byte PNG signature.
    BadSignature,
    /// Input ended inside a chunk header, data, or CRC.
    Truncated { at: usize },
    /// A chunk's stored CRC-32 did not match its type and data.
    CrcMismatch { chunk: [u8; 4], stored: u32, computed: u32 },
    /// A chunk appeared out of order (e.g. `IHDR` not first, `IDAT` split, or `IEND` non-empty).
    BadChunkOrder { chunk: [u8; 4] },
    /// Malformed `IHDR` chunk (wrong length, compression method, or filter method).
    BadIhdr,
    /// Unsupported bit depth, colour type, or interlacing.
    UnsupportedFormat { bit_depth: u8, colour_type: u8, interlace: u8 },
    /// An unrecognised critical chunk.
    UnsupportedChunk { chunk: [u8; 4] },
    /// File ended without `IHDR`.
    MissingIhdr,
    /// File had no `IDAT` chunk.
    MissingIdat,
    /// File ended without `IEND`.
    MissingIend,
    /// Bytes appeared after `IEND`.
    TrailingBytes { extra: usize },
    /// Zlib decompression of `IDAT` failed.
    Inflate(blaze_deflate::Error),
    /// Inflated `IDAT` payload length did not match `height · (1 + width · bpp)`.
    InflatedLength { want: usize, got: usize },
    /// Unknown PNG scanline filter type (must be 0..=4).
    BadFilter { row: u32, filter: u8 },
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

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// The Paeth predictor (section 9.4): left, then up, then upper-left on ties.
fn paeth_predict(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let (pa, pb, pc) = ((p - i16::from(a)).abs(), (p - i16::from(b)).abs(), (p - i16::from(c)).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

fn unfilter_predict(filter: u8, a: u8, b: u8, c: u8, row: u32) -> Result<u8, PngError> {
    match filter {
        0 => Ok(0),
        1 => Ok(a),
        2 => Ok(b),
        // The mean of two bytes fits in a byte.
        3 => Ok(((u16::from(a) + u16::from(b)) / 2).narrow_or_stop::<u8>()),
        4 => Ok(paeth_predict(a, b, c)),
        _ => Err(PngError::BadFilter { row, filter }),
    }
}

/// Decode a PNG image (non-interlaced 8-bit `Grey8`, `Rgba8`, or `Rgb8` expanded to `Rgba8`).
/// Verifies the signature, `IHDR`, every chunk's CRC-32, zlib framing and Adler-32, `limits.most_pixels`,
/// and exact scanline byte length.
pub fn decode_png<'a>(
    bytes: &[u8],
    limits: Limits,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<DecodedPng<'a>, PngError> {
    if bytes.len() < 8 || bytes[..8] != SIGNATURE {
        return Err(PngError::BadSignature);
    }
    let mut i = 8usize;
    let mut ihdr: Option<(u32, u32, u8)> = None;
    let mut idat: Vec<u8, Alloc<'_>> = Vec::new_in(scratch);
    let mut seen_idat = false;
    let mut after_idat = false;
    let mut ended = false;

    while i < bytes.len() {
        if ended {
            return Err(PngError::TrailingBytes { extra: bytes.len() - i });
        }
        if bytes.len() - i < 12 {
            return Err(PngError::Truncated { at: i });
        }
        let len_u32 = be32(bytes, i);
        if len_u32 > MOST_SIDE {
            return Err(PngError::Truncated { at: i });
        }
        let len = idx(len_u32);
        if bytes.len() - i - 12 < len {
            return Err(PngError::Truncated { at: i });
        }
        let kind = [bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]];
        let data = &bytes[i + 8..i + 8 + len];
        let stored_crc = be32(bytes, i + 8 + len);
        let mut crc = Crc32::new();
        crc.update(&kind);
        crc.update(data);
        let computed = crc.finish();
        if computed != stored_crc {
            return Err(PngError::CrcMismatch { chunk: kind, stored: stored_crc, computed });
        }

        match &kind {
            b"IHDR" => {
                if i != 8 || ihdr.is_some() {
                    return Err(PngError::BadChunkOrder { chunk: kind });
                }
                if len != 13 {
                    return Err(PngError::BadIhdr);
                }
                let width = be32(data, 0);
                let height = be32(data, 4);
                let bit_depth = data[8];
                let colour_type = data[9];
                let compression = data[10];
                let filter_method = data[11];
                let interlace = data[12];
                if width == 0 || height == 0 {
                    return Err(PngError::Empty { width, height });
                }
                if width > MOST_SIDE || height > MOST_SIDE {
                    return Err(PngError::TooWide { width, height });
                }
                let pixels = u64::from(width) * u64::from(height);
                if pixels > limits.most_pixels {
                    return Err(PngError::TooManyPixels { pixels, most: limits.most_pixels });
                }
                if compression != 0 || filter_method != 0 {
                    return Err(PngError::BadIhdr);
                }
                if bit_depth != 8 || interlace != 0 || !matches!(colour_type, 0 | 2 | 6) {
                    return Err(PngError::UnsupportedFormat { bit_depth, colour_type, interlace });
                }
                ihdr = Some((width, height, colour_type));
            }
            b"IDAT" => {
                if ihdr.is_none() || after_idat {
                    return Err(PngError::BadChunkOrder { chunk: kind });
                }
                seen_idat = true;
                idat.try_extend_from_slice(data)?;
            }
            b"IEND" => {
                if ihdr.is_none() || len != 0 {
                    return Err(PngError::BadChunkOrder { chunk: kind });
                }
                ended = true;
            }
            _ => {
                let Some((_, _, colour_type)) = ihdr else {
                    return Err(PngError::BadChunkOrder { chunk: kind });
                };
                if seen_idat {
                    after_idat = true;
                }
                // Bit 5 of the first byte is 0 for critical chunks, 1 for ancillary chunks.
                if (kind[0] & 0x20) == 0 {
                    // `PLTE` is an optional suggested palette for RGB/RGBA; reject all other unknown critical chunks.
                    if !(&kind == b"PLTE" && matches!(colour_type, 2 | 6)) {
                        return Err(PngError::UnsupportedChunk { chunk: kind });
                    }
                }
            }
        }
        i += 12 + len;
    }

    let Some((width, height, colour_type)) = ihdr else {
        return Err(PngError::MissingIhdr);
    };
    if !ended {
        return Err(PngError::MissingIend);
    }
    if !seen_idat {
        return Err(PngError::MissingIdat);
    }

    let src_bpp: usize = match colour_type {
        0 => 1,
        2 => 3,
        _ => 4,
    };
    let pixels_total = u64::from(width) * u64::from(height);
    let Some(stride) = idx(width).checked_mul(src_bpp) else {
        return Err(PngError::TooManyPixels { pixels: pixels_total, most: limits.most_pixels });
    };
    let Some(raw_row) = stride.checked_add(1) else {
        return Err(PngError::TooManyPixels { pixels: pixels_total, most: limits.most_pixels });
    };
    let Some(raw_len) = raw_row.checked_mul(idx(height)) else {
        return Err(PngError::TooManyPixels { pixels: pixels_total, most: limits.most_pixels });
    };

    let mut raw: Vec<u8, Alloc<'_>> = Vec::try_with_capacity_in(raw_len, scratch)?;
    let deflate_limits = blaze_deflate::Limits { most_out: raw_len.widen_to(), most_ratio: MAX_RATIO, most_header: 0 };
    unzlib(&idat, deflate_limits, scratch, &mut raw).map_err(PngError::Inflate)?;
    if raw.len() != raw_len {
        return Err(PngError::InflatedLength { want: raw_len, got: raw.len() });
    }

    if colour_type == 2 {
        // RGB8: unfilter into scratch rows, then expand each RGB pixel to RGBA8 with alpha = 255.
        let Some(out_len) = idx(width).checked_mul(idx(height)).and_then(|n| n.checked_mul(4)) else {
            return Err(PngError::TooManyPixels { pixels: pixels_total, most: limits.most_pixels });
        };
        let mut prev = zeroed(stride, scratch)?;
        let mut curr = zeroed(stride, scratch)?;
        let mut pixels: Vec<u8, Alloc<'a>> = Vec::try_with_capacity_in(out_len, alloc)?;
        for y in 0..height {
            let yi = idx(y);
            let filter = raw[yi * raw_row];
            let src = &raw[yi * raw_row + 1..yi * raw_row + 1 + stride];
            for x in 0..stride {
                let a = if x >= 3 { curr[x - 3] } else { 0 };
                let b = prev[x];
                let c = if x >= 3 { prev[x - 3] } else { 0 };
                let predicted = unfilter_predict(filter, a, b, c, y)?;
                curr[x] = src[x].wrapping_add(predicted);
            }
            for rgb in curr.chunks_exact(3) {
                pixels.try_extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255])?;
            }
            core::mem::swap(&mut prev, &mut curr);
        }
        return Ok(DecodedPng { width, height, kind: PngKind::Rgba8, pixels });
    }

    let kind = if colour_type == 0 { PngKind::Grey8 } else { PngKind::Rgba8 };
    let Some(out_len) = stride.checked_mul(idx(height)) else {
        return Err(PngError::TooManyPixels { pixels: pixels_total, most: limits.most_pixels });
    };
    let mut pixels = zeroed(out_len, alloc)?;
    for y in 0..height {
        let yi = idx(y);
        let filter = raw[yi * raw_row];
        let src = &raw[yi * raw_row + 1..yi * raw_row + 1 + stride];
        let row_start = yi * stride;
        for x in 0..stride {
            let a = if x >= src_bpp { pixels[row_start + x - src_bpp] } else { 0 };
            let b = if yi > 0 { pixels[row_start - stride + x] } else { 0 };
            let c = if yi > 0 && x >= src_bpp { pixels[row_start - stride + x - src_bpp] } else { 0 };
            let predicted = unfilter_predict(filter, a, b, c, y)?;
            pixels[row_start + x] = src[x].wrapping_add(predicted);
        }
    }
    Ok(DecodedPng { width, height, kind, pixels })
}
