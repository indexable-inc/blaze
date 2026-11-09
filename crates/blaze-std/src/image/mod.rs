//! Images are functions (docs/v2/plan/design/std.md, "Gpu, video and images"):
//! [`decode`] takes bytes, an allocator and [`Limits`] and returns an [`Image`] in that
//! allocator; [`encode`] takes an image and an allocator and returns the bytes. No capability,
//! no IO, no clock, no global state: a pure core.
//!
//! PNG first, the whole format bstd needs: bit depths 8 and 16, grey, grey with alpha, RGB and
//! RGBA, every filter, the zlib stream across any number of IDAT chunks, and every chunk's
//! CRC checked. Refused with [`ImageError::Unsupported`], never guessed at: palettes (colour
//! type 3), bit depths 1, 2 and 4, and Adam7 interlacing. Ancillary chunks (gAMA, tRNS, text,
//! ...) are skipped; an unknown critical chunk is an error.
//!
//! Decode treats its input as hostile: it reads the header, checks the width, the height and
//! every byte it would allocate against [`Limits`] before allocating anything, allocates once,
//! and never panics on any input. [`encode`]'s output is a function of the image alone: the
//! same pixels give the same bytes.

#![blaze::sans_io]

pub(crate) mod png;
pub(crate) mod zlib;

use std::{alloc::Allocator, fmt};

/// The most a PNG's width or height may be (2^31 - 1, the PNG spec's).
pub const MAX_SIDE: u32 = (1 << 31) - 1;

/// What each pixel holds.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Color {
    Grey,
    GreyAlpha,
    Rgb,
    Rgba,
}

impl Color {
    pub const ALL: [Color; 4] = [Color::Grey, Color::GreyAlpha, Color::Rgb, Color::Rgba];

    /// Samples a pixel.
    pub const fn channels(self) -> u32 {
        match self {
            Color::Grey => 1,
            Color::GreyAlpha => 2,
            Color::Rgb => 3,
            Color::Rgba => 4,
        }
    }
}

/// Bits a sample. Sixteen-bit samples are big-endian in [`Image::pixels`], as PNG stores them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Depth {
    Eight,
    Sixteen,
}

impl Depth {
    pub const ALL: [Depth; 2] = [Depth::Eight, Depth::Sixteen];

    /// Bytes a sample.
    pub const fn bytes(self) -> u32 {
        match self {
            Depth::Eight => 1,
            Depth::Sixteen => 2,
        }
    }
}

/// Bytes a row of `width` pixels of `color` at `depth` takes, if it fits a `usize`.
pub fn row_bytes(width: u32, color: Color, depth: Depth) -> Option<usize> {
    let bytes = u64::from(width) * u64::from(color.channels()) * u64::from(depth.bytes());
    usize::try_from(bytes).ok()
}

/// Pixels in an allocator the caller handed in: `height` rows of [`row_bytes`] each, top row
/// first, no padding.
pub struct Image<A: Allocator> {
    width: u32,
    height: u32,
    color: Color,
    depth: Depth,
    pixels: Vec<u8, A>,
}

impl<A: Allocator> Image<A> {
    /// An image of `pixels`, which must be exactly `height` rows of [`row_bytes`]; the width and
    /// the height are each in `1..=`[`MAX_SIDE`].
    pub fn new(width: u32, height: u32, color: Color, depth: Depth, pixels: Vec<u8, A>) -> Result<Image<A>, ImageError> {
        if !(1..=MAX_SIDE).contains(&width) {
            return Err(ImageError::Header { field: "width", value: width });
        }
        if !(1..=MAX_SIDE).contains(&height) {
            return Err(ImageError::Header { field: "height", value: height });
        }
        let expected =
            u64::from(width) * u64::from(height) * u64::from(color.channels()) * u64::from(depth.bytes());
        if expected != pixels.len() as u64 {
            return Err(ImageError::Pixels { width, height, color, depth, expected, got: pixels.len() as u64 });
        }
        Ok(Image { width, height, color, depth, pixels })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn color(&self) -> Color {
        self.color
    }

    pub fn depth(&self) -> Depth {
        self.depth
    }

    /// Bytes a row.
    pub fn stride(&self) -> usize {
        self.pixels.len() / self.height as usize
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    pub fn into_pixels(self) -> Vec<u8, A> {
        self.pixels
    }
}

impl<A: Allocator, B: Allocator> PartialEq<Image<B>> for Image<A> {
    fn eq(&self, other: &Image<B>) -> bool {
        self.width == other.width
            && self.height == other.height
            && self.color == other.color
            && self.depth == other.depth
            && self.pixels[..] == other.pixels[..]
    }
}

impl<A: Allocator> fmt::Debug for Image<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Image({}x{} {:?} {:?}, {} bytes)", self.width, self.height, self.color, self.depth, self.pixels.len())
    }
}

/// What [`decode`] may accept: the most width, height and bytes asked of the allocator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    pub width: u32,
    pub height: u32,
    /// Every byte decode asks of the allocator, together: the inflated rows with their filter
    /// bytes, plus one (see [`decode_bytes`]). The pixels are made in place in that buffer.
    pub bytes: usize,
}

impl Limits {
    /// 16384 x 16384, and 1 GiB.
    pub const DEFAULT: Limits = Limits { width: 16_384, height: 16_384, bytes: 1 << 30 };
}

/// The bytes [`decode`] asks of its allocator for an image of this size and kind: every row
/// with its filter byte, and one byte more (the inflater's proof that the stream ends where
/// the header says).
pub fn decode_bytes(width: u32, height: u32, color: Color, depth: Depth) -> u64 {
    let stride = u64::from(width) * u64::from(color.channels()) * u64::from(depth.bytes());
    u64::from(height).saturating_mul(stride.saturating_add(1)).saturating_add(1)
}

/// A PNG's pixels, in `alloc`. Width, height and bytes are checked against `limits` before
/// anything is allocated; any input ends in an image or an [`ImageError`], never a panic.
pub fn decode<A: Allocator>(bytes: &[u8], alloc: A, limits: Limits) -> Result<Image<A>, ImageError> {
    png::decode(bytes, alloc, limits)
}

/// `image` as a PNG, in `alloc`. The same image always gives the same bytes.
pub fn encode<A: Allocator + Clone, B: Allocator>(image: &Image<B>, alloc: A) -> Result<Vec<u8, A>, ImageError> {
    png::encode(image, alloc)
}

/// A PNG chunk's four-letter name.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkName(pub [u8; 4]);

impl fmt::Display for ChunkName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &b in &self.0 {
            if b.is_ascii_graphic() {
                write!(f, "{}", b as char)?;
            } else {
                write!(f, "\\x{b:02x}")?;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for ChunkName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChunkName({self})")
    }
}

/// Why an image could not be decoded or encoded: each names the chunk, the field, the row or
/// the limit, and where in the input it is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ImageError {
    /// The input does not start with PNG's eight-byte signature.
    NotPng,
    /// The input ends inside `what`, which starts at byte `at`.
    Truncated { what: &'static str, at: usize },
    /// Chunk `chunk` at byte `at` fails its CRC.
    Crc { chunk: ChunkName, at: usize },
    /// Chunk `chunk` at byte `at` says it is `len` bytes, past PNG's 2^31 - 1.
    ChunkTooLong { chunk: ChunkName, at: usize, len: u32 },
    /// Chunk `chunk` at byte `at` is where PNG's rule `why` forbids it.
    Order { chunk: ChunkName, at: usize, why: &'static str },
    /// Chunk `chunk` at byte `at` is critical (its first letter is upper case) and unknown.
    UnknownCritical { chunk: ChunkName, at: usize },
    /// The image has no chunk `chunk`.
    Missing { chunk: ChunkName },
    /// IHDR's `field` is `value`, which PNG does not define (or a width or height out of range).
    Header { field: &'static str, value: u32 },
    /// IHDR asks for what bstd's PNG does not do: `what`, as `value`.
    Unsupported { what: &'static str, value: u32 },
    /// The image passes the decode limit `limit`: it asks `asked`, the most is `most`.
    Limit { limit: &'static str, asked: u64, most: u64 },
    /// The zlib stream in the IDAT chunks: `why`.
    Inflate { why: &'static str },
    /// The IDAT chunks inflate to `got` bytes; the header asks for `expected`.
    DataSize { expected: u64, got: u64 },
    /// Row `row` (from the top, from 0) has filter type `kind`; PNG defines 0 to 4.
    Filter { row: u32, kind: u8 },
    /// A `width` x `height` image of `color` at `depth` is `expected` bytes; it was given `got`.
    Pixels { width: u32, height: u32, color: Color, depth: Depth, expected: u64, got: u64 },
    /// The allocator handed in could not give `asked` bytes for `what`.
    OutOfMemory { what: &'static str, asked: usize },
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::NotPng => write!(f, "png: the input does not start with PNG's signature"),
            ImageError::Truncated { what, at } => write!(f, "png: the input ends inside {what}, at byte {at}"),
            ImageError::Crc { chunk, at } => write!(f, "png: chunk {chunk} at byte {at} fails its CRC"),
            ImageError::ChunkTooLong { chunk, at, len } => {
                write!(f, "png: chunk {chunk} at byte {at} says it is {len} bytes, past PNG's 2^31 - 1")
            }
            ImageError::Order { chunk, at, why } => write!(f, "png: chunk {chunk} at byte {at}: {why}"),
            ImageError::UnknownCritical { chunk, at } => {
                write!(f, "png: chunk {chunk} at byte {at} is critical and not one PNG defines")
            }
            ImageError::Missing { chunk } => write!(f, "png: the image has no {chunk} chunk"),
            ImageError::Header { field, value } => write!(f, "png: IHDR's {field} is {value}, which PNG does not allow"),
            ImageError::Unsupported { what, value } => {
                write!(f, "png: IHDR asks for {what} ({value}), which bstd::image does not decode")
            }
            ImageError::Limit { limit, asked, most } => {
                write!(f, "png: the image's {limit} is {asked}, past the decode limit {most}")
            }
            ImageError::Inflate { why } => write!(f, "png: IDAT: {why}"),
            ImageError::DataSize { expected, got } => {
                write!(f, "png: IDAT inflates to {got} bytes; IHDR asks for {expected}")
            }
            ImageError::Filter { row, kind } => {
                write!(f, "png: row {row} has filter type {kind}; PNG defines 0 to 4")
            }
            ImageError::Pixels { width, height, color, depth, expected, got } => write!(
                f,
                "image: a {width}x{height} {color:?} {depth:?} image is {expected} bytes of pixels; it was given {got}"
            ),
            ImageError::OutOfMemory { what, asked } => {
                write!(f, "image: the allocator could not give {asked} bytes for {what}")
            }
        }
    }
}

impl std::error::Error for ImageError {}
