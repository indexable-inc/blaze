//! Why a stream was refused: the framing it was read as, the byte of input it was found at,
//! and what was wrong (rule 19). Every refusal of the crate is one of these; none is a panic.

use core::fmt;

use blaze_own::AllocFailure;

/// The framing a stream was read or written as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// Raw DEFLATE (RFC 1951).
    Deflate,
    /// zlib (RFC 1950).
    Zlib,
    /// gzip (RFC 1952).
    Gzip,
}

/// One of a dynamic block's three codes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tree {
    /// The code-length code, which sends the other two's lengths.
    Precode,
    /// Literals, lengths and the end of the block.
    Literals,
    Distances,
}

/// What was wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// A block of type 3, which DEFLATE reserves.
    BlockType,
    /// A stored block whose length's complement is not its complement.
    StoredLength { len: u16, nlen: u16 },
    /// A dynamic block that says it has more than 286 literal/length codes or 30 distance codes.
    TooManyCodes { literals: u16, distances: u16 },
    /// Lengths that give more codes than the code space holds.
    Oversubscribed(Tree),
    /// Lengths that leave the code space part empty, other than the one short code zlib allows.
    Incomplete(Tree),
    /// A repeat of the previous length (code 16) before any length.
    RepeatFirst,
    /// A repeat that runs past the last length.
    RepeatPastEnd,
    /// A dynamic block with no code for its own end (symbol 256).
    NoEndOfBlock,
    /// Bits that are no code of the tree (an incomplete code's unused half, an empty distance code).
    BadCode(Tree),
    /// A symbol DEFLATE never sends: literal/length 286 or 287, distance 30 or 31.
    BadSymbol { tree: Tree, symbol: u16 },
    /// A match reaching back before the stream's first byte.
    TooFar { distance: u32, have: u64 },
    /// The output would pass the most the caller allows ([`crate::Limits::most_out`]).
    OutputOver { most: u64 },
    /// The output would pass the input times the most ratio the caller allows
    /// ([`crate::Limits::most_ratio`]): a decompression bomb.
    RatioOver { input: u64, output: u64, most_ratio: u64 },
    /// The input ended inside the stream.
    Truncated,
    /// More input after a one-shot call's stream ended.
    AfterEnd { extra: u64 },
    /// A buffer could not be had.
    Alloc(AllocFailure),
    /// A zlib header that is no zlib header: not method 8, a window over 32 KiB, or a bad check.
    ZlibHeader { cmf: u8, flg: u8 },
    /// A zlib stream that needs a preset dictionary, which nothing here has.
    ZlibDictionary,
    /// A zlib trailer whose Adler-32 is not the output's.
    Adler32 { stored: u32, computed: u32 },
    /// A gzip stream that does not start with 1f 8b.
    GzipMagic { id1: u8, id2: u8 },
    /// A gzip member of a method other than 8 (deflate).
    GzipMethod(u8),
    /// A gzip member with a reserved flag set.
    GzipFlags(u8),
    /// A gzip header (its extra field, name and comment) longer than the caller allows
    /// ([`crate::Limits::most_header`]).
    GzipHeaderOver { most: u32 },
    /// A gzip header whose CRC-16 is not its own.
    GzipHeaderCrc { stored: u16, computed: u16 },
    /// A gzip trailer whose CRC-32 is not the output's.
    Crc32 { stored: u32, computed: u32 },
    /// A gzip trailer whose size is not the output's (mod 2^32).
    Size { stored: u32, computed: u32 },
}

/// A refused stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Error {
    pub format: Format,
    /// The offset in the framed input at which it was found: for an inflate fault, at most eight
    /// bytes past the bit it is at (the bit buffer's read-ahead).
    pub at: u64,
    pub why: Why,
}

impl Error {
    pub(crate) const fn new(format: Format, at: u64, why: Why) -> Error {
        Error { format, at, why }
    }

    /// The same fault, found inside a frame of `format` whose header was `header` bytes long.
    pub(crate) const fn framed(self, format: Format, header: u64) -> Error {
        Error { format, at: self.at + header, why: self.why }
    }
}

impl fmt::Display for Tree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Tree::Precode => "code-length code",
            Tree::Literals => "literal/length code",
            Tree::Distances => "distance code",
        })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let format = match self.format {
            Format::Deflate => "deflate",
            Format::Zlib => "zlib",
            Format::Gzip => "gzip",
        };
        write!(f, "blaze_deflate: {format} stream at byte {}: {}", self.at, self.why)
    }
}

/// What was wrong, without where: an [`Error`]'s message after its framing and offset, and
/// the words a test's oracle quotes for a refusal it holds only the `Why` of.
impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Why::BlockType => f.write_str("a block of the reserved type 3"),
            Why::StoredLength { len, nlen } => {
                write!(f, "a stored block of length {len} whose complement is {nlen}, not {}", !len)
            }
            Why::TooManyCodes { literals, distances } => write!(
                f,
                "a dynamic block of {literals} literal/length codes and {distances} distance codes (at most 286 and 30)"
            ),
            Why::Oversubscribed(tree) => write!(f, "the {tree}'s lengths give more codes than fit"),
            Why::Incomplete(tree) => write!(f, "the {tree}'s lengths leave part of the code unused"),
            Why::RepeatFirst => f.write_str("a repeat of the previous length before any length"),
            Why::RepeatPastEnd => f.write_str("a repeat of lengths past the last code"),
            Why::NoEndOfBlock => f.write_str("a dynamic block with no code for its end (symbol 256)"),
            Why::BadCode(tree) => write!(f, "bits that are no code of the {tree}"),
            Why::BadSymbol { tree, symbol } => write!(f, "the {tree}'s symbol {symbol}, which DEFLATE never sends"),
            Why::TooFar { distance, have } => {
                write!(f, "a match {distance} bytes back with only {have} bytes written")
            }
            Why::OutputOver { most } => write!(f, "output past the most allowed, {most} bytes"),
            Why::RatioOver { input, output, most_ratio } => write!(
                f,
                "{output} bytes out of {input} in, past the most ratio allowed ({most_ratio} to 1): a decompression bomb"
            ),
            Why::Truncated => f.write_str("the input ended inside the stream"),
            Why::AfterEnd { extra } => write!(f, "{extra} bytes of input after the stream's end"),
            Why::Alloc(failure) => write!(f, "{failure}"),
            Why::ZlibHeader { cmf, flg } => write!(f, "{cmf:#04x} {flg:#04x} is no zlib header"),
            Why::ZlibDictionary => f.write_str("the stream needs a preset dictionary"),
            Why::Adler32 { stored, computed } => {
                write!(f, "the trailer's Adler-32 is {stored:#010x}, the output's {computed:#010x}")
            }
            Why::GzipMagic { id1, id2 } => write!(f, "{id1:#04x} {id2:#04x} is not gzip's 1f 8b"),
            Why::GzipMethod(method) => write!(f, "method {method}, not 8 (deflate)"),
            Why::GzipFlags(flags) => write!(f, "flags {flags:#04x} set a reserved bit"),
            Why::GzipHeaderOver { most } => write!(f, "a header longer than the most allowed, {most} bytes"),
            Why::GzipHeaderCrc { stored, computed } => {
                write!(f, "the header's CRC-16 is {stored:#06x}, its bytes' {computed:#06x}")
            }
            Why::Crc32 { stored, computed } => {
                write!(f, "the trailer's CRC-32 is {stored:#010x}, the output's {computed:#010x}")
            }
            Why::Size { stored, computed } => {
                write!(f, "the trailer's size is {stored}, the output's {computed} (mod 2^32)")
            }
        }
    }
}

impl core::error::Error for Error {}
