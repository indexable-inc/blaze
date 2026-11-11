//! PNG's container and filters (ISO/IEC 15948, W3C PNG): the chunks, IHDR, the five filters,
//! and the zlib stream ([`super::zlib`]) across the IDAT chunks.

#![blaze::sans_io]

use std::alloc::Allocator;

use super::{
    ChunkName, Color, Depth, Image, ImageError, Limits, decode_bytes, row_bytes,
    zlib::{Crc, Inflate, compress, zeroed},
};

pub(crate) const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

const IHDR: ChunkName = ChunkName(*b"IHDR");
const PLTE: ChunkName = ChunkName(*b"PLTE");
const IDAT: ChunkName = ChunkName(*b"IDAT");
const IEND: ChunkName = ChunkName(*b"IEND");

/// The most a chunk's length may say (2^31 - 1).
const CHUNK_MOST: u32 = (1 << 31) - 1;
/// The most data an IDAT chunk [`encode`] writes holds.
const IDAT_MOST: usize = 1 << 20;

/// One chunk: its name, its data, and the byte its length field starts at.
struct Chunk<'a> {
    name: ChunkName,
    data: &'a [u8],
    at: usize,
}

/// The chunks after the signature, each checked (length, bounds, CRC) as it is read.
struct Chunks<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Chunks<'a> {
    /// The next chunk, or `None` once the input is used up.
    fn next(&mut self) -> Result<Option<Chunk<'a>>, ImageError> {
        let at = self.at;
        let Some(rest) = self.bytes.get(at..) else { return Ok(None) };
        if rest.is_empty() {
            return Ok(None);
        }
        let (Some(len), Some(name)) = (rest.get(0..4), rest.get(4..8)) else {
            return Err(ImageError::Truncated { what: "a chunk's length and name", at });
        };
        let len = u32::from_be_bytes([len[0], len[1], len[2], len[3]]);
        let name = ChunkName([name[0], name[1], name[2], name[3]]);
        if len > CHUNK_MOST {
            return Err(ImageError::ChunkTooLong { chunk: name, at, len });
        }
        let end = 8 + len as usize;
        let (Some(data), Some(crc)) = (rest.get(8..end), rest.get(end..end + 4)) else {
            return Err(ImageError::Truncated { what: "a chunk's data and CRC", at });
        };
        let mut sum = Crc::new();
        sum.update(&name.0);
        sum.update(data);
        if sum.finish() != u32::from_be_bytes([crc[0], crc[1], crc[2], crc[3]]) {
            return Err(ImageError::Crc { chunk: name, at });
        }
        self.at = at + end + 4;
        Ok(Some(Chunk { name, data, at }))
    }
}

/// What IHDR says, checked.
struct Header {
    width: u32,
    height: u32,
    color: Color,
    depth: Depth,
}

fn header(chunk: &Chunk<'_>) -> Result<Header, ImageError> {
    if chunk.name != IHDR {
        return Err(ImageError::Order { chunk: chunk.name, at: chunk.at, why: "the first chunk must be IHDR" });
    }
    let d = chunk.data;
    if d.len() != 13 {
        return Err(ImageError::Header { field: "length", value: d.len() as u32 });
    }
    let width = u32::from_be_bytes([d[0], d[1], d[2], d[3]]);
    let height = u32::from_be_bytes([d[4], d[5], d[6], d[7]]);
    let (bits, kind, compression, filter, interlace) = (d[8], d[9], d[10], d[11], d[12]);
    if !(1..=super::MAX_SIDE).contains(&width) {
        return Err(ImageError::Header { field: "width", value: width });
    }
    if !(1..=super::MAX_SIDE).contains(&height) {
        return Err(ImageError::Header { field: "height", value: height });
    }
    let color = match kind {
        0 => Color::Grey,
        2 => Color::Rgb,
        4 => Color::GreyAlpha,
        6 => Color::Rgba,
        3 => return Err(ImageError::Unsupported { what: "colour type 3 (a palette)", value: 3 }),
        other => return Err(ImageError::Header { field: "colour type", value: u32::from(other) }),
    };
    let depth = match (bits, kind) {
        (8, _) => Depth::Eight,
        (16, _) => Depth::Sixteen,
        (1 | 2 | 4, 0) => {
            return Err(ImageError::Unsupported { what: "a grey bit depth under 8", value: u32::from(bits) });
        }
        (other, _) => return Err(ImageError::Header { field: "bit depth", value: u32::from(other) }),
    };
    if compression != 0 {
        return Err(ImageError::Header { field: "compression method", value: u32::from(compression) });
    }
    if filter != 0 {
        return Err(ImageError::Header { field: "filter method", value: u32::from(filter) });
    }
    match interlace {
        0 => {}
        1 => return Err(ImageError::Unsupported { what: "Adam7 interlacing", value: 1 }),
        other => return Err(ImageError::Header { field: "interlace method", value: u32::from(other) }),
    }
    Ok(Header { width, height, color, depth })
}

/// Walks every chunk after IHDR once, checking their order, and counts the IDAT chunks.
fn idat_count(bytes: &[u8], from: usize, color: Color) -> Result<usize, ImageError> {
    let mut chunks = Chunks { bytes, at: from };
    let (mut idats, mut idat_ended, mut plte) = (0usize, false, false);
    loop {
        let Some(chunk) = chunks.next()? else {
            return Err(ImageError::Missing { chunk: IEND });
        };
        let (name, at) = (chunk.name, chunk.at);
        if name == IEND {
            break;
        }
        if name == IDAT {
            if idat_ended {
                return Err(ImageError::Order { chunk: name, at, why: "IDAT chunks must be consecutive" });
            }
            idats += 1;
            continue;
        }
        if idats > 0 {
            idat_ended = true;
        }
        if name == IHDR {
            return Err(ImageError::Order { chunk: name, at, why: "IHDR may appear only once, first" });
        }
        if name == PLTE {
            if matches!(color, Color::Grey | Color::GreyAlpha) {
                return Err(ImageError::Order { chunk: name, at, why: "a grey image may not have PLTE" });
            }
            if plte || idats > 0 {
                return Err(ImageError::Order { chunk: name, at, why: "PLTE comes once, before IDAT" });
            }
            // A suggested palette for an RGB image: skipped.
            plte = true;
            continue;
        }
        if name.0[0] & 0x20 == 0 {
            return Err(ImageError::UnknownCritical { chunk: name, at });
        }
    }
    if idats == 0 {
        return Err(ImageError::Missing { chunk: IDAT });
    }
    Ok(idats)
}

pub(crate) fn decode<A: Allocator>(bytes: &[u8], alloc: A, limits: Limits) -> Result<Image<A>, ImageError> {
    if bytes.get(..8) != Some(&SIGNATURE[..]) {
        return Err(ImageError::NotPng);
    }
    let mut chunks = Chunks { bytes, at: 8 };
    let first = chunks.next()?.ok_or(ImageError::Missing { chunk: IHDR })?;
    let Header { width, height, color, depth } = header(&first)?;
    if width > limits.width {
        return Err(ImageError::Limit { limit: "width", asked: u64::from(width), most: u64::from(limits.width) });
    }
    if height > limits.height {
        return Err(ImageError::Limit { limit: "height", asked: u64::from(height), most: u64::from(limits.height) });
    }
    let asked = decode_bytes(width, height, color, depth);
    if asked > limits.bytes as u64 {
        return Err(ImageError::Limit { limit: "bytes", asked, most: limits.bytes as u64 });
    }
    // Within `limits.bytes`, which is a usize: these fit.
    let stride = row_bytes(width, color, depth).ok_or(ImageError::Limit {
        limit: "bytes",
        asked,
        most: limits.bytes as u64,
    })?;
    let raw = (asked - 1) as usize;
    let after_ihdr = chunks.at;
    let mut idats = idat_count(bytes, after_ihdr, color)?;

    // The one allocation: the rows with their filter bytes, and one byte to prove the stream
    // ends where the header says.
    let mut buf: Vec<u8, A> = zeroed(raw + 1, alloc, "the image's rows")?;
    let mut inflate = Inflate::new();
    let mut chunks = Chunks { bytes, at: after_ihdr };
    while idats > 0 {
        let Some(chunk) = chunks.next()? else { break };
        if chunk.name != IDAT {
            continue;
        }
        idats -= 1;
        inflate.feed(chunk.data, idats > 0, &mut buf)?;
        if inflate.written() > raw {
            return Err(ImageError::DataSize { expected: raw as u64, got: inflate.written() as u64 });
        }
        if inflate.done() {
            break;
        }
    }
    if !inflate.done() {
        return Err(ImageError::Inflate { why: "the zlib stream does not end within the IDAT chunks" });
    }
    if inflate.written() != raw {
        return Err(ImageError::DataSize { expected: raw as u64, got: inflate.written() as u64 });
    }
    let bpp = (color.channels() * depth.bytes()) as usize;
    unfilter(&mut buf, height, stride, bpp)?;
    // Drop each row's filter byte, in place: row r moves from r * (stride + 1) + 1 to r * stride.
    for r in 0..height as usize {
        let from = r * (stride + 1) + 1;
        buf.copy_within(from..from + stride, r * stride);
    }
    buf.truncate(height as usize * stride);
    Image::new(width, height, color, depth, buf)
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (ia, ib, ic) = (i16::from(a), i16::from(b), i16::from(c));
    let p = ia + ib - ic;
    let (pa, pb, pc) = ((p - ia).abs(), (p - ib).abs(), (p - ic).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Undo each row's filter in place; each row is its filter byte, then `stride` bytes.
fn unfilter(buf: &mut [u8], height: u32, stride: usize, bpp: usize) -> Result<(), ImageError> {
    for r in 0..height as usize {
        let start = r * (stride + 1);
        let kind = buf[start];
        let (before, rest) = buf.split_at_mut(start + 1);
        let cur = &mut rest[..stride];
        // The row above, already unfiltered; none above the first.
        let up: Option<&[u8]> = if r == 0 { None } else { Some(&before[start - stride..start]) };
        match kind {
            0 => {}
            1 => {
                for i in bpp..stride {
                    cur[i] = cur[i].wrapping_add(cur[i - bpp]);
                }
            }
            2 => {
                if let Some(up) = up {
                    for i in 0..stride {
                        cur[i] = cur[i].wrapping_add(up[i]);
                    }
                }
            }
            3 => {
                for i in 0..stride {
                    let a = if i >= bpp { u16::from(cur[i - bpp]) } else { 0 };
                    let b = up.map_or(0, |up| u16::from(up[i]));
                    cur[i] = cur[i].wrapping_add(((a + b) / 2) as u8);
                }
            }
            4 => {
                for i in 0..stride {
                    let a = if i >= bpp { cur[i - bpp] } else { 0 };
                    let b = up.map_or(0, |up| up[i]);
                    let c = if i >= bpp { up.map_or(0, |up| up[i - bpp]) } else { 0 };
                    cur[i] = cur[i].wrapping_add(paeth(a, b, c));
                }
            }
            other => return Err(ImageError::Filter { row: r as u32, kind: other }),
        }
    }
    Ok(())
}

/// Byte `i` of a row filtered with `kind` (0 to 4).
fn filtered(kind: u8, row: &[u8], up: Option<&[u8]>, i: usize, bpp: usize) -> u8 {
    let x = row[i];
    let a = if i >= bpp { row[i - bpp] } else { 0 };
    let b = up.map_or(0, |up| up[i]);
    let c = if i >= bpp { up.map_or(0, |up| up[i - bpp]) } else { 0 };
    match kind {
        0 => x,
        1 => x.wrapping_sub(a),
        2 => x.wrapping_sub(b),
        3 => x.wrapping_sub(((u16::from(a) + u16::from(b)) / 2) as u8),
        _ => x.wrapping_sub(paeth(a, b, c)),
    }
}

/// The filter for a row: the one whose bytes, read as signed, sum smallest in magnitude (the
/// PNG spec's suggested heuristic); the lowest kind on a tie.
fn pick_filter(row: &[u8], up: Option<&[u8]>, bpp: usize) -> u8 {
    let mut best = (u64::MAX, 0u8);
    for kind in 0..5u8 {
        let cost: u64 =
            (0..row.len()).map(|i| u64::from((filtered(kind, row, up, i, bpp) as i8).unsigned_abs())).sum();
        if cost < best.0 {
            best = (cost, kind);
        }
    }
    best.1
}

fn color_type(color: Color) -> u8 {
    match color {
        Color::Grey => 0,
        Color::Rgb => 2,
        Color::GreyAlpha => 4,
        Color::Rgba => 6,
    }
}

/// A chunk: its length, name, data and CRC, onto `out` (whose room is reserved).
fn chunk<A: Allocator>(out: &mut Vec<u8, A>, name: ChunkName, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(&name.0);
    out.extend_from_slice(data);
    let mut sum = Crc::new();
    sum.update(&name.0);
    sum.update(data);
    out.extend_from_slice(&sum.finish().to_be_bytes());
}

pub(crate) fn encode<A: Allocator + Clone, B: Allocator>(image: &Image<B>, alloc: A) -> Result<Vec<u8, A>, ImageError> {
    let (width, height) = (image.width(), image.height());
    let stride = image.stride();
    let bpp = (image.color().channels() * image.depth().bytes()) as usize;
    let rows = height as usize;
    let raw_len = rows.checked_mul(stride + 1).ok_or(ImageError::OutOfMemory {
        what: "the filtered rows",
        asked: usize::MAX,
    })?;
    let mut raw: Vec<u8, A> = Vec::new_in(alloc.clone());
    raw.try_reserve_exact(raw_len)
        .map_err(|_| ImageError::OutOfMemory { what: "the filtered rows", asked: raw_len })?;
    let pixels = image.pixels();
    for r in 0..rows {
        let row = &pixels[r * stride..(r + 1) * stride];
        let up = if r == 0 { None } else { Some(&pixels[(r - 1) * stride..r * stride]) };
        let kind = pick_filter(row, up, bpp);
        raw.push(kind);
        for i in 0..stride {
            raw.push(filtered(kind, row, up, i, bpp));
        }
    }
    let z = compress(&raw, alloc.clone())?;
    drop(raw);

    let idats = z.len().div_ceil(IDAT_MOST).max(1);
    let most = 8 + 25 + z.len() + 12 * idats + 12;
    let mut out = Vec::new_in(alloc);
    out.try_reserve_exact(most).map_err(|_| ImageError::OutOfMemory { what: "the PNG", asked: most })?;
    out.extend_from_slice(&SIGNATURE);
    let mut ihdr = [0u8; 13];
    ihdr[0..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8] = match image.depth() {
        Depth::Eight => 8,
        Depth::Sixteen => 16,
    };
    ihdr[9] = color_type(image.color());
    // Compression 0 (deflate), filter method 0, no interlace.
    chunk(&mut out, IHDR, &ihdr);
    for part in z.chunks(IDAT_MOST) {
        chunk(&mut out, IDAT, part);
    }
    chunk(&mut out, IEND, &[]);
    Ok(out)
}
