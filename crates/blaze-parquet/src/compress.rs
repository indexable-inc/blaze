//! Page compression, one of the codecs every Parquet reader knows: none, LZ4_RAW ([`crate::lz4`],
//! blaze-parquet's own) and ZSTD (the store's zstd, the workspace's `zstd` crate over the vendored zstd-sys, C zstd 1.5.7, as `blaze-cas` and
//! blaze-format-tiers reach it, so the tree holds one zstd). Which a file takes is the crate
//! doc's table.

use core::alloc::Allocator;

use blaze_own::Vec;

use crate::{ParquetError, column_name, lz4, meta::codec};

/// How a file's pages are compressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    /// Pages as their bytes are.
    None,
    /// Each page one LZ4 block (Parquet's LZ4_RAW).
    Lz4,
    /// Each page a zstd frame at `level`.
    Zstd { level: i32 },
}

impl Compression {
    /// A file a build writes: its encodings alone ([`Options::DEFAULT`](crate::Options::DEFAULT);
    /// LZ4_RAW cost more than 5% of the write, the crate doc's table).
    pub const HOT: Compression = Compression::None;
    /// A file merged or gone cold: zstd at its default level (level 1 was 1% larger and no
    /// faster to read, the crate doc's table).
    pub const COLD: Compression = Compression::Zstd { level: 3 };

    /// The footer's `CompressionCodec`.
    pub(crate) fn codec(self) -> i32 {
        match self {
            Compression::None => codec::UNCOMPRESSED,
            Compression::Lz4 => codec::LZ4_RAW,
            Compression::Zstd { .. } => codec::ZSTD,
        }
    }

    /// `body` compressed, appended to `out`; false when the page is written as it is.
    pub(crate) fn compress<A: Allocator>(self, body: &[u8], out: &mut Vec<u8, A>) -> Result<bool, ParquetError> {
        match self {
            Compression::None => Ok(false),
            Compression::Lz4 => {
                lz4::compress(body, out);
                Ok(true)
            }
            Compression::Zstd { level } => {
                let start = out.len();
                out.resize(start + zstd::zstd_safe::compress_bound(body.len()), 0);
                let len = zstd::bulk::compress_to_buffer(body, &mut out[start..], level)
                    .map_err(|_| ParquetError::TooLarge { column: column_name("a page"), what: "a page zstd refused" })?;
                out.truncate(start + len);
                Ok(true)
            }
        }
    }
}

/// The most bytes one page is read into: past it a page header is refused, never allocated.
pub(crate) const MOST_PAGE: usize = 1 << 30;

/// A page of codec `codec`, `size` bytes uncompressed, from `body`, appended to `out`: false
/// when uncompressed (`out` untouched; read `body`).
pub(crate) fn decompress<A: Allocator>(
    name: &str,
    codec: i32,
    body: &[u8],
    size: usize,
    out: &mut Vec<u8, A>,
) -> Result<bool, ParquetError> {
    if codec == codec::UNCOMPRESSED {
        return Ok(false);
    }
    if size > MOST_PAGE {
        return Err(ParquetError::TooLarge { column: column_name(name), what: "a page past 1 GiB uncompressed" });
    }
    let start = out.len();
    match codec {
        codec::LZ4_RAW => {
            out.resize(start + size, 0);
            lz4::decompress(body, &mut out[start..])
                .map_err(|lz4::Corrupt(what)| ParquetError::Corrupt { column: column_name(name), what })?;
            Ok(true)
        }
        codec::ZSTD => {
            out.resize(start + size, 0);
            let len = zstd::bulk::decompress_to_buffer(body, &mut out[start..])
                .map_err(|_| ParquetError::Corrupt { column: column_name(name), what: "a zstd page that does not decompress" })?;
            if len != size {
                return Err(ParquetError::Corrupt { column: column_name(name), what: "a zstd page of another size than its header's" });
            }
            Ok(true)
        }
        other => Err(ParquetError::Unsupported {
            column: column_name(name),
            what: "a column of a codec other than LZ4_RAW and zstd",
            code: other.into(),
        }),
    }
}
