//! HPACK (RFC 7541): the field compression of HTTP/2. A field block is a sequence of
//! representations (section 6): an indexed field (`1xxxxxxx`), a literal with incremental
//! indexing (`01xxxxxx`), a dynamic table size update (`001xxxxx`), a literal never indexed
//! (`0001xxxx`) and a literal without indexing (`0000xxxx`); integers with an N-bit prefix
//! (section 5.1); strings raw or Huffman-coded (section 5.2, Appendix B); one index space, the
//! 61 static entries then the dynamic table newest first (section 2.3.3).
//!
//! The decoder and the encoder are each a loop over their input; neither recurses. Both own their
//! dynamic table, in the allocator passed in, bounded by its maximum size (section 4.2).

#![blaze::sans_io]

use core::fmt;

use blaze_own::AllocFailure;

mod decoder;
mod encoder;
pub mod huffman;
mod int;
pub mod table;

pub use decoder::{Decoded, Decoder};
pub use encoder::{Encoder, Strings};
pub use int::{get_int, put_int};

/// Why a field block could not be decoded: a COMPRESSION_ERROR (RFC 9113 section 4.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HpackFault {
    /// Index 0, which names nothing (section 6.1).
    IndexZero,
    /// An index past the static table and the dynamic table's entries (section 2.3.3).
    IndexOutOfRange { index: u32, entries: u32 },
    /// An integer past 2^32 - 1, or with more continuation bytes than one that fits (section
    /// 5.1: "implementations MUST limit").
    IntegerOverflow,
    /// The block ended inside a representation.
    Truncated,
    /// A Huffman string holding the EOS symbol (section 5.2).
    HuffmanEos,
    /// A Huffman string whose padding is longer than 7 bits or not EOS's leading ones (section
    /// 5.2).
    HuffmanPadding,
    /// A dynamic table size update above the SETTINGS_HEADER_TABLE_SIZE we allow (section 6.3).
    SizeOverMax { size: u32, most: u32 },
    /// A dynamic table size update after the block's first field (section 4.2).
    SizeUpdateLate,
    /// No dynamic table size update at the start of the first block after we lowered the
    /// table's maximum (section 4.2).
    SizeUpdateMissing,
    /// More than two dynamic table size updates at the start of one block (section 4.2 allows
    /// the smallest and the final one).
    SizeUpdatesOverTwo,
}

impl fmt::Display for HpackFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            HpackFault::IndexZero => f.write_str("index 0 (RFC 7541 section 6.1)"),
            HpackFault::IndexOutOfRange { index, entries } => {
                write!(f, "index {index} past the {entries} entries (RFC 7541 section 2.3.3)")
            }
            HpackFault::IntegerOverflow => f.write_str("an integer past 2^32 - 1 (RFC 7541 section 5.1)"),
            HpackFault::Truncated => f.write_str("the block ends inside a representation (RFC 7541 section 6)"),
            HpackFault::HuffmanEos => f.write_str("a Huffman string holds EOS (RFC 7541 section 5.2)"),
            HpackFault::HuffmanPadding => f.write_str("a Huffman string's padding is not 0 to 7 one bits (RFC 7541 section 5.2)"),
            HpackFault::SizeOverMax { size, most } => {
                write!(f, "a table size update to {size}, above the {most} allowed (RFC 7541 section 6.3)")
            }
            HpackFault::SizeUpdateLate => f.write_str("a table size update after a field (RFC 7541 section 4.2)"),
            HpackFault::SizeUpdateMissing => {
                f.write_str("no table size update after the maximum was lowered (RFC 7541 section 4.2)")
            }
            HpackFault::SizeUpdatesOverTwo => f.write_str("more than two table size updates (RFC 7541 section 4.2)"),
        }
    }
}

/// Why decoding stopped: the block is bad, or a buffer could not be had.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Halt {
    Fault(HpackFault),
    Alloc(AllocFailure),
}

impl From<HpackFault> for Halt {
    fn from(fault: HpackFault) -> Halt {
        Halt::Fault(fault)
    }
}

impl From<AllocFailure> for Halt {
    fn from(failure: AllocFailure) -> Halt {
        Halt::Alloc(failure)
    }
}
