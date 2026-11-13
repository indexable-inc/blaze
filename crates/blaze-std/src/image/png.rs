//! PNG's container and filters (ISO/IEC 15948, W3C PNG): the chunks, IHDR, the five filters,
//! and the zlib stream ([`super::zlib`]) across the IDAT chunks.

#![blaze::sans_io]

use core::{alloc::Allocator, iter};

use super::{
    ChunkName, Color, Depth, Image, ImageError, Limits, decode_bytes, out_of_memory, row_bytes,
    zlib::{Crc, Inflate, compress, zeroed},
};
use crate::mem::{Named, Vec};

pub(crate) const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

const IHDR: ChunkName = ChunkName(*b"IHDR");
const PLTE: ChunkName = ChunkName(*b"PLTE");
const IDAT: ChunkName = ChunkName(*b"IDAT");
const IEND: ChunkName = ChunkName(*b"IEND");

/// The most a chunk's length may say (2^31 - 1).
const CHUNK_MOST: u32 = (1 << 31) - 1;
/// The most data an IDAT chunk [`encode`] writes holds.
const IDAT_MOST: usize = 1 << 20;

/// `$f::<BPP>(args)` with `BPP` the bytes a pixel of `$color` at `$depth` takes: each filter
/// loop is made for its pixel size, so it walks whole pixels (`[u8; BPP]`) with no index.
macro_rules! by_pixel {
    ($color:expr, $depth:expr, $f:ident($($arg:expr),* $(,)?)) => {
        match ($color, $depth) {
            (Color::Grey, Depth::Eight) => $f::<1>($($arg),*),
            (Color::GreyAlpha, Depth::Eight) | (Color::Grey, Depth::Sixteen) => $f::<2>($($arg),*),
            (Color::Rgb, Depth::Eight) => $f::<3>($($arg),*),
            (Color::Rgba, Depth::Eight) | (Color::GreyAlpha, Depth::Sixteen) => $f::<4>($($arg),*),
            (Color::Rgb, Depth::Sixteen) => $f::<6>($($arg),*),
            (Color::Rgba, Depth::Sixteen) => $f::<8>($($arg),*),
        }
    };
}

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
        let Some((&len, rest)) = rest.split_first_chunk::<4>() else {
            return Err(ImageError::Truncated { what: "a chunk's length and name", at });
        };
        let Some((&name, rest)) = rest.split_first_chunk::<4>() else {
            return Err(ImageError::Truncated { what: "a chunk's length and name", at });
        };
        let len = u32::from_be_bytes(len);
        let name = ChunkName(name);
        if len > CHUNK_MOST {
            return Err(ImageError::ChunkTooLong { chunk: name, at, len });
        }
        let data_len = len as usize;
        let Some((data, rest)) = rest.split_at_checked(data_len) else {
            return Err(ImageError::Truncated { what: "a chunk's data and CRC", at });
        };
        let Some((&crc, _)) = rest.split_first_chunk::<4>() else {
            return Err(ImageError::Truncated { what: "a chunk's data and CRC", at });
        };
        let mut sum = Crc::new();
        sum.update(&name.0)?;
        sum.update(data)?;
        if sum.finish() != u32::from_be_bytes(crc) {
            return Err(ImageError::Crc { chunk: name, at });
        }
        // Its length and name, its data and its CRC: all inside the input, so this is too.
        self.at = at + 8 + data_len + 4;
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
    let Ok(&[w0, w1, w2, w3, h0, h1, h2, h3, bits, kind, compression, filter, interlace]) =
        <&[u8; 13]>::try_from(chunk.data)
    else {
        return Err(ImageError::Header { field: "length", value: chunk.data.len() as u32 });
    };
    let width = u32::from_be_bytes([w0, w1, w2, w3]);
    let height = u32::from_be_bytes([h0, h1, h2, h3]);
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

pub(crate) fn decode<A: Allocator + Named>(bytes: &[u8], alloc: A, limits: Limits) -> Result<Image<A>, ImageError> {
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
    let most = limits.bytes.get();
    if asked > most {
        return Err(ImageError::Limit { limit: "bytes", asked, most });
    }
    // Within `limits.bytes`; where this target's `usize` does not hold them, the limit refuses.
    let past = || ImageError::Limit { limit: "bytes", asked, most };
    let stride = row_bytes(width, color, depth).ok_or_else(past)?;
    let rows = usize::try_from(height).map_err(|_| past())?;
    let line = stride.checked_add(1).ok_or_else(past)?;
    let raw = rows.checked_mul(line).ok_or_else(past)?;
    let room = raw.checked_add(1).ok_or_else(past)?;
    let after_ihdr = chunks.at;
    let mut idats = idat_count(bytes, after_ihdr, color)?;

    // The one allocation: the rows with their filter bytes, and one byte to prove the stream
    // ends where the header says.
    let mut buf: Vec<u8, A> = zeroed(room, alloc, "the image's rows")?;
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
    let len = buf.len();
    let Some(lines) = buf.get_mut(..raw) else {
        return Err(ImageError::Internal { what: "the image's rows", index: raw, len });
    };
    by_pixel!(color, depth, unfilter(lines, line))?;
    drop_filter_bytes(&mut buf, rows, stride)?;
    buf.truncate(rows * stride);
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

fn average(a: u8, b: u8) -> u8 {
    ((u16::from(a) + u16::from(b)) / 2) as u8
}

/// A row as whole pixels, or the error naming it when its length is not a whole number of them.
fn as_pixels<'r, const BPP: usize>(row: &'r [u8], what: &'static str) -> Result<&'r [[u8; BPP]], ImageError> {
    match row.as_chunks::<BPP>() {
        (whole, []) => Ok(whole),
        (_, rest) => Err(ImageError::Internal { what, index: row.len(), len: row.len() - rest.len() }),
    }
}

fn as_pixels_mut<'r, const BPP: usize>(
    row: &'r mut [u8],
    what: &'static str,
) -> Result<&'r mut [[u8; BPP]], ImageError> {
    let len = row.len();
    match row.as_chunks_mut::<BPP>() {
        (whole, []) => Ok(whole),
        (_, rest) => Err(ImageError::Internal { what, index: len, len: len - rest.len() }),
    }
}

/// Undo each row's filter in place; each row is `line` bytes: its filter byte, then its pixels.
fn unfilter<const BPP: usize>(rows: &mut [u8], line: usize) -> Result<(), ImageError> {
    // The row above, already unfiltered; none above the first (read as zeroes).
    let mut up: Option<&[[u8; BPP]]> = None;
    for (r, row) in (0u32..).zip(rows.chunks_exact_mut(line)) {
        let Some((&mut kind, cur)) = row.split_first_mut() else {
            return Err(ImageError::Internal { what: "a filtered row", index: 0, len: 0 });
        };
        let cur = as_pixels_mut::<BPP>(cur, "a filtered row")?;
        let above = up.unwrap_or(&[]);
        match kind {
            0 => {}
            1 => undo(cur, above, |a, _, _| a),
            2 => undo(cur, above, |_, b, _| b),
            3 => undo(cur, above, |a, b, _| average(a, b)),
            4 => undo(cur, above, paeth),
            other => return Err(ImageError::Filter { row: r, kind: other }),
        }
        up = Some(&*cur);
    }
    Ok(())
}

/// One row's filter undone: each byte plus `predict(a, b, c)` of the byte to its left (`a`,
/// already undone), above (`b`) and above-left (`c`), each 0 past the image's edge.
#[inline]
fn undo<const BPP: usize>(cur: &mut [[u8; BPP]], above: &[[u8; BPP]], predict: impl Fn(u8, u8, u8) -> u8) {
    let zero = [0u8; BPP];
    let above = above.iter().chain(iter::repeat(&zero));
    let (mut left, mut above_left) = ([0u8; BPP], [0u8; BPP]);
    for (px, up) in cur.iter_mut().zip(above) {
        for (((x, a), c), &b) in px.iter_mut().zip(&mut left).zip(&mut above_left).zip(up) {
            *x = x.wrapping_add(predict(*a, b, *c));
            *a = *x;
            *c = b;
        }
    }
}

/// Drops each row's filter byte, in place: row `r` moves from `r * (stride + 1) + 1` to
/// `r * stride`, which is never after it, so each move reads only bytes not yet written.
fn drop_filter_bytes(buf: &mut [u8], rows: usize, stride: usize) -> Result<(), ImageError> {
    let line = stride + 1;
    for r in 0..rows {
        let from = r * line + 1;
        let end = from + stride;
        if end > buf.len() {
            return Err(ImageError::Internal { what: "the image's rows", index: end, len: buf.len() });
        }
        // In range: `from..end` was just checked, and the target `r * stride..` ends before `end`.
        buf.copy_within(from..end, r * stride);
    }
    Ok(())
}

/// Byte `x` filtered with `kind` (0 to 4), given the bytes left (`a`), above (`b`) and
/// above-left (`c`) of it.
fn filter_byte(kind: u8, x: u8, a: u8, b: u8, c: u8) -> u8 {
    match kind {
        0 => x,
        1 => x.wrapping_sub(a),
        2 => x.wrapping_sub(b),
        3 => x.wrapping_sub(average(a, b)),
        _ => x.wrapping_sub(paeth(a, b, c)),
    }
}

/// The bytes of `row` filtered with `kind`, `above` the row above it (empty for the first).
fn filtered<'r, const BPP: usize>(
    kind: u8,
    row: &'r [[u8; BPP]],
    above: &'r [[u8; BPP]],
) -> impl Iterator<Item = u8> + 'r {
    let zero = [0u8; BPP];
    let left = iter::once(zero).chain(row.iter().copied());
    let up = above.iter().copied().chain(iter::repeat(zero));
    let up_left = iter::once(zero).chain(above.iter().copied()).chain(iter::repeat(zero));
    row.iter().zip(left).zip(up).zip(up_left).flat_map(move |(((x, a), b), c)| {
        x.iter().zip(a).zip(b).zip(c).map(move |(((&x, a), b), c)| filter_byte(kind, x, a, b, c))
    })
}

/// The filter for a row: the one whose bytes, read as signed, sum smallest in magnitude (the
/// PNG spec's suggested heuristic); the lowest kind on a tie.
fn pick_filter<const BPP: usize>(row: &[[u8; BPP]], above: &[[u8; BPP]]) -> u8 {
    let mut best = (u64::MAX, 0u8);
    for kind in 0..5u8 {
        let cost: u64 = filtered(kind, row, above).map(|b| u64::from((b as i8).unsigned_abs())).sum();
        if cost < best.0 {
            best = (cost, kind);
        }
    }
    best.1
}

/// Each row of `pixels` (rows of `stride` bytes) filtered into `out` (rows of `stride + 1`: the
/// filter's kind, then the row).
fn filter<const BPP: usize>(out: &mut [u8], pixels: &[u8], stride: usize) -> Result<(), ImageError> {
    if stride == 0 {
        return Err(ImageError::Internal { what: "the image's rows", index: 0, len: 0 });
    }
    let mut up: Option<&[[u8; BPP]]> = None;
    for (dst, row) in out.chunks_exact_mut(stride + 1).zip(pixels.chunks_exact(stride)) {
        let Some((kind_at, dst)) = dst.split_first_mut() else {
            return Err(ImageError::Internal { what: "the filtered rows", index: 0, len: 0 });
        };
        let row = as_pixels::<BPP>(row, "the image's rows")?;
        let above = up.unwrap_or(&[]);
        let kind = pick_filter(row, above);
        *kind_at = kind;
        for (o, b) in dst.iter_mut().zip(filtered(kind, row, above)) {
            *o = b;
        }
        up = Some(row);
    }
    Ok(())
}

fn color_type(color: Color) -> u8 {
    match color {
        Color::Grey => 0,
        Color::Rgb => 2,
        Color::GreyAlpha => 4,
        Color::Rgba => 6,
    }
}

/// What the PNG's bytes are, when the allocator has no room for them.
const PNG: &str = "the PNG";

/// A chunk: its length, name, data and CRC, onto `out`.
fn chunk<A: Allocator + Named>(out: &mut Vec<u8, A>, name: ChunkName, data: &[u8]) -> Result<(), ImageError> {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes()).map_err(out_of_memory(PNG))?;
    out.extend_from_slice(&name.0).map_err(out_of_memory(PNG))?;
    out.extend_from_slice(data).map_err(out_of_memory(PNG))?;
    let mut sum = Crc::new();
    sum.update(&name.0)?;
    sum.update(data)?;
    out.extend_from_slice(&sum.finish().to_be_bytes()).map_err(out_of_memory(PNG))
}

pub(crate) fn encode<A: Allocator + Clone + Named, B: Allocator>(
    image: &Image<B>,
    alloc: A,
) -> Result<Vec<u8, A>, ImageError> {
    let (width, height, color, depth) = (image.width(), image.height(), image.color(), image.depth());
    let stride = image.stride();
    let too_large = ImageError::TooLarge { width, height, color, depth };
    let raw_len = usize::try_from(height)
        .ok()
        .zip(stride.checked_add(1))
        .and_then(|(rows, line)| rows.checked_mul(line))
        .ok_or(too_large)?;
    let mut raw: Vec<u8, A> = Vec::with_capacity_in(raw_len, alloc.clone()).map_err(out_of_memory("the filtered rows"))?;
    let rows = raw.extend_filled(raw_len, 0).map_err(out_of_memory("the filtered rows"))?;
    by_pixel!(color, depth, filter(rows, image.pixels(), stride))?;
    let z = compress(&raw, alloc.clone())?;
    drop(raw);

    let idats = z.len().div_ceil(IDAT_MOST).max(1);
    let most = 8 + 25 + z.len() + 12 * idats + 12;
    let mut out = Vec::with_capacity_in(most, alloc).map_err(out_of_memory(PNG))?;
    out.extend_from_slice(&SIGNATURE).map_err(out_of_memory(PNG))?;
    let [w0, w1, w2, w3] = width.to_be_bytes();
    let [h0, h1, h2, h3] = height.to_be_bytes();
    let bits = match depth {
        Depth::Eight => 8,
        Depth::Sixteen => 16,
    };
    // Compression 0 (deflate), filter method 0, no interlace.
    let ihdr = [w0, w1, w2, w3, h0, h1, h2, h3, bits, color_type(color), 0, 0, 0];
    chunk(&mut out, IHDR, &ihdr)?;
    for part in z.chunks(IDAT_MOST) {
        chunk(&mut out, IDAT, part)?;
    }
    chunk(&mut out, IEND, &[])?;
    Ok(out)
}
