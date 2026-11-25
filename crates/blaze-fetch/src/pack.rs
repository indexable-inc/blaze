//! The record of one fetched unit: its files in one byte string, written once and read in
//! place ([`PackView`]). Little-endian:
//!
//! ```text
//! 0..8     b"BLZFPK05"
//! 8        kind: 1 a registry crate, 2 a git commit, 3 a tree of blaze's toolchain
//! 9..12    zero
//! 12..16   files n
//! 16..24   bytes of paths
//! 24..32   bytes of stored data
//! 32..64   the source's digest: the `.crate`'s SHA-256, or the commit id and 12 zeros
//! 64..96   the BLAKE3 of the files' `blaze tree 3` listing (crate::tree): the tree's pin
//! 96..     the stored data, each file's bytes (raw or RFC 8878 Zstandard) in archive order
//! then     the paths (uncompressed UTF-8)
//! then     n rows of 64: path offset u32, path length u16, kind u8, codec u8,
//!          data offset u64, stored length u64, raw length u64, the file's BLAKE3;
//!          sorted by path bytes, each path once
//! last 32  the BLAKE3 of every byte before it
//! ```
//!
//! Header, paths, and the sorted 64-byte row table stay uncompressed so metadata queries
//! (`open_verified`, `len`, `entry`, `entries`, `find_entry`, `tree`, `source`) stay O(1)
//! zero-copy. Each file's payload is compressed independently with RFC 8878 Zstandard
//! (`blaze-zstd`) as it closes — using the `BLZFPK04` pinned Rust source dictionary for
//! files up to 32 KiB and plain Zstandard above — or kept raw (`Codec::Raw`) whenever
//! compression does not strictly shrink it, so random-access file reads decompress only the
//! requested file.
//!
//! A path is relative to the unit's folder (a crate's `name-version/` stripped), and holds to
//! the listing's rules: no empty, `.`, `..` or `.git` part, and no two paths (or the folders
//! they are in) one name on a case- and normalization-insensitive disk. A link's data is its
//! target, which stays inside the folder. Two builders of one key write equal bytes: the rows
//! are sorted, and the data is in the archive's order, which the key's digest fixes.

#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt;
use core::ops::Deref;

use blaze_own::{Alloc, AllocFailure, Bump, Vec};
use blaze_text::{Put, PutDisplay as _, Show, Writes as _};
use blaze_zstd::{CompressOpts, Level, ZstdDict, ZstdEncoder, ZstdLimits, decompress_in, decompress_with_dict_in};

use crate::digest::Blake3;
pub use crate::tree::Kind as Mode;
use crate::tree::{self, TreeRefused};

pub const MAGIC: &[u8; 8] = b"BLZFPK05";
pub const HEADER: usize = 96;
pub const ROW: usize = 64;
pub const FOOTER: usize = 32;

/// Minimum raw file size considered for Zstandard compression.
const MIN_COMPRESS_LEN: usize = 64;
/// Files up to this size compress with [`RUST_DICT`] (`Codec::ZstdRustDict`); larger files use
/// plain Zstandard (`Codec::Zstd`) so large payloads pay zero dictionary-prepend copy cost.
const MAX_DICT_FILE_LEN: usize = 32 << 10;

/// Pinned raw-content RFC 8878 dictionary for `BLZFPK04` (`Codec::ZstdRustDict`).
///
/// Immutable for `BLZFPK04`: changing these bytes requires bumping `MAGIC`. Holds high-frequency
/// Rust, Cargo.toml, doc-comment, and license boilerplate shared across small crate files.
const RUST_DICT: &[u8] = concat!(
    "// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or\n",
    "// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license\n",
    "// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your\n",
    "// option. This file may not be copied, modified, or distributed\n",
    "// except according to those terms.\n",
    "#![no_std]\n#![cfg_attr(not(feature = \"std\"), no_std)]\n",
    "#![allow(dead_code, unused_imports, missing_docs, clippy::all)]\n",
    "#![deny(unsafe_code, missing_docs)]\n",
    "#![feature(const_trait_impl, core_intrinsics, allocator_api, rustc_attrs)]\n",
    "extern crate alloc;\nextern crate core;\nextern crate std;\n",
    "use core::cell::{Cell, RefCell, UnsafeCell};\n",
    "use core::cmp::{Eq, Ord, Ordering, PartialEq, PartialOrd};\n",
    "use core::convert::{AsMut, AsRef, From, Into, TryFrom, TryInto};\n",
    "use core::default::Default;\n",
    "use core::fmt::{self, Debug, Display, Formatter, Write};\n",
    "use core::hash::{Hash, Hasher};\n",
    "use core::iter::{ExactSizeIterator, FromIterator, IntoIterator, Iterator};\n",
    "use core::marker::{Copy, PhantomData, Send, Sized, Sync, Unpin};\n",
    "use core::mem::{self, MaybeUninit, ManuallyDrop, align_of, size_of};\n",
    "use core::ops::{Add, BitAnd, BitOr, BitXor, Deref, DerefMut, Drop, Fn, FnMut, FnOnce, Index, IndexMut, Mul, Neg, Not, Range, Rem, Shl, Shr, Sub};\n",
    "use core::option::Option::{self, None, Some};\n",
    "use core::ptr::{self, NonNull};\n",
    "use core::result::Result::{self, Err, Ok};\n",
    "use core::slice;\nuse core::str;\n",
    "use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};\n",
    "use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, Write as IoWrite};\n",
    "use std::path::{Path, PathBuf};\n",
    "use std::string::{String, ToString};\n",
    "use std::sync::{Arc, Mutex, RwLock};\n",
    "use std::vec::Vec;\n",
    "#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]\n",
    "#[cfg(target_arch = \"aarch64\")]\n#[cfg(target_arch = \"x86_64\")]\n",
    "#[cfg(target_os = \"linux\")]\n#[cfg(target_os = \"macos\")]\n#[cfg(target_os = \"windows\")]\n",
    "#[cfg(unix)]\n#[cfg(windows)]\n#[cfg(test)]\n#[cfg(feature = \"std\")]\n#[cfg(feature = \"alloc\")]\n",
    "#[inline]\n#[inline(always)]\n#[must_use]\n#[cold]\n#[track_caller]\n#[repr(C)]\n#[repr(transparent)]\n",
    "#[stable(feature = \"rust1\", since = \"1.0.0\")]\n",
    "#[unstable(feature = \"core_intrinsics\", issue = \"none\")]\n",
    "#[rustc_const_stable(feature = \"const_fn\", since = \"1.32.0\")]\n",
    "pub mod error;\npub mod iter;\npub mod slice;\npub mod str;\npub mod sync;\n",
    "pub struct Error {\n    kind: ErrorKind,\n}\n",
    "impl fmt::Display for Error {\n    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {\n",
    "        write!(f, \"{}\", self.message())\n    }\n}\n",
    "impl core::error::Error for Error {}\n",
    "pub fn new() -> Self {\n    Self::default()\n}\n",
    "pub const fn len(&self) -> usize { self.len }\n",
    "pub const fn is_empty(&self) -> bool { self.len() == 0 }\n",
    "pub fn as_slice(&self) -> &[u8] { &self.buf }\n",
    "pub fn as_bytes(&self) -> &[u8] { self.as_str().as_bytes() }\n",
    "pub fn get(&self, index: usize) -> Option<&T> { self.slice.get(index) }\n",
    "pub unsafe fn from_raw_parts<'a, T>(data: *const T, len: usize) -> &'a [T] {\n",
    "    // SAFETY: caller guarantees valid pointer and length.\n",
    "}\n",
    "/// Creates a new instance.\n/// Returns `None` if the index is out of bounds.\n",
    "/// # Errors\n/// # Panics\n/// # Safety\n/// # Examples\n/// ```\n/// assert_eq!(\n/// ```\n",
    "#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn it_works() {\n",
    "        assert_eq!(result, expected);\n    }\n}\n",
    "[package]\nname = \"\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    "authors = []\ndescription = \"\"\nlicense = \"MIT OR Apache-2.0\"\n",
    "repository = \"https://github.com/\"\ndocumentation = \"https://docs.rs/\"\n",
    "keywords = []\ncategories = []\nreadme = \"README.md\"\nrust-version = \"1.70\"\n",
    "[features]\ndefault = [\"std\"]\nstd = [\"alloc\"]\nalloc = []\n",
    "[dependencies]\n[dev-dependencies]\n[build-dependencies]\n[lib]\npath = \"src/lib.rs\"\n",
    "pub trait From<T>: Sized { fn from(value: T) -> Self; }\n",
    "pub trait Into<T>: Sized { fn into(self) -> T; }\n",
    "pub trait TryFrom<T>: Sized { type Error; fn try_from(value: T) -> Result<Self, Self::Error>; }\n",
    "match value {\n    Ok(v) => Ok(v),\n    Err(e) => Err(e),\n}\n",
    "if let Some(value) = option {\n    return Some(value);\n}\n",
    "while let Some(item) = iter.next() {\n}\n",
    "for (index, item) in items.iter().enumerate() {\n}\n",
)
.as_bytes();

/// How one file's payload is stored in a `BLZFPK04` record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Codec {
    /// Stored uncompressed; read zero-copy straight from the mapped record.
    Raw,
    /// Stored as one RFC 8878 Zstandard frame.
    Zstd,
    /// Stored as one RFC 8878 Zstandard frame using the `BLZFPK04` pinned Rust dictionary.
    ZstdRustDict,
}

impl Codec {
    const fn byte(self) -> u8 {
        match self {
            Codec::Raw => 0,
            Codec::Zstd => 1,
            Codec::ZstdRustDict => 2,
        }
    }

    const fn from_byte(b: u8) -> Option<Codec> {
        match b {
            0 => Some(Codec::Raw),
            1 => Some(Codec::Zstd),
            2 => Some(Codec::ZstdRustDict),
            _ => None,
        }
    }
}

/// What kind of unit a record holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Crate,
    Git,
    /// A tree of blaze's toolchain (docs/v2/plan/design/toolchain.md): its source digest is
    /// its own listing's.
    Tree,
}

impl Kind {
    fn byte(self) -> u8 {
        match self {
            Kind::Crate => 1,
            Kind::Git => 2,
            Kind::Tree => 3,
        }
    }
}

/// What a record may hold (rule 18): the caller's choice.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    pub most_files: u32,
    /// Bytes of all files' data.
    pub most_bytes: u64,
    /// Bytes of one path.
    pub most_path: u16,
}

/// Why a record was not made, or bytes are not a record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PackRefused {
    FilesOver { most: u32 },
    BytesOver { most: u64 },
    PathOver { most: u16 },
    /// The listing's rules ([`crate::tree`]).
    Tree(TreeRefused),
    /// A link whose target leaves the folder.
    LinkOut { path: alloc::string::String },
    Alloc(AllocFailure),
    /// Bytes that are not a record: the part that is wrong.
    Form { why: &'static str },
    /// The record's BLAKE3 is not its bytes'.
    Digest,
    /// A record of other sources than the ones asked for (its kind, SHA-256 or commit).
    NotTheSource,
    /// A record past the room reserved for it ([`Fixed`]): the bound it was reserved at was
    /// wrong, which is blaze's bug, refused rather than written past.
    RoomOver { room: u64 },
    /// A read targeted a recorded git submodule (`0o160000` gitlink) or a path beneath it.
    /// Submodule trees are not materialized into the parent commit's pack; reads fail closed
    /// naming the submodule path and its pinned commit SHA-1.
    Submodule { path: crate::unpack::Cut, submodule: crate::unpack::Cut, commit: [u8; 20] },
}

impl Show for PackRefused {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match self {
            PackRefused::FilesOver { most } => out.str("more than ").int(*most).str(" files"),
            PackRefused::BytesOver { most } => out.str("more than ").byte_count(*most).str(" of files"),
            PackRefused::PathOver { most } => out.str("a path longer than ").int(*most).str(" bytes"),
            PackRefused::Tree(why) => out.put_display(why),
            PackRefused::LinkOut { path } => out.str("the link ").str(path).str(" points outside its folder"),
            PackRefused::Alloc(failure) => out.put_display(failure),
            PackRefused::Form { why } => out.str("not a record: ").str(why),
            PackRefused::Digest => out.str("a record whose BLAKE3 is not its bytes'"),
            PackRefused::NotTheSource => out.str("a record of other sources than the lock's"),
            PackRefused::RoomOver { room } => {
                out.str("a record past the ").byte_count(*room).str(" reserved for it (blaze's bound is wrong)")
            }
            PackRefused::Submodule { path, submodule, commit } => out
                .str("the path ")
                .show(path)
                .str(" is inside unpopulated git submodule ")
                .show(submodule)
                .str(" (commit ")
                .hex_bytes(commit)
                .str(")"),
        };
    }
}

#[derive(Clone, Copy)]
struct Row {
    path_at: u32,
    path_len: u16,
    mode: Mode,
    codec: Codec,
    data_at: u64,
    stored_len: u64,
    raw_len: u64,
    /// The file's BLAKE3 over its uncompressed bytes, written when the file is sealed.
    blake3: [u8; 32],
}

/// Where a record's bytes go as it is made: the heap (a `Vec`, stored by `Put`) or a file's
/// writable mapping ([`Fixed`], a reservation written in place). The strategy is this type
/// parameter alone: a record's bytes are equal whichever made them (rule 25; the test
/// `a_record_is_the_same_bytes_in_the_heap_or_in_place`).
pub trait Sink {
    fn len(&self) -> usize;
    fn bytes(&self) -> &[u8];
    fn bytes_mut(&mut self) -> &mut [u8];
    /// Room for `more` bytes past the end, the first ask sized exactly: an allocation for the
    /// heap, a check against the reservation in place.
    fn room(&mut self, more: usize, exact: bool) -> Result<(), PackRefused>;
    /// `bytes` past the end, within the room asked.
    fn extend(&mut self, bytes: &[u8]);
    /// `n` zero bytes past the end, within the room asked.
    fn zeros(&mut self, n: usize);
    /// Shortens the written prefix to `len` (`len <= self.len()`), used after compressing a file
    /// in place into a smaller slice.
    fn truncate(&mut self, len: usize);
}

impl Sink for Vec<u8, Alloc<'_>> {
    fn len(&self) -> usize {
        Vec::len(self)
    }

    fn bytes(&self) -> &[u8] {
        self
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        self
    }

    fn room(&mut self, more: usize, exact: bool) -> Result<(), PackRefused> {
        match exact {
            true => self.try_reserve_exact(more),
            false => self.try_reserve(more),
        }
        .map_err(PackRefused::Alloc)
    }

    fn extend(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }

    fn zeros(&mut self, n: usize) {
        self.resize(Vec::len(self) + n, 0);
    }

    fn truncate(&mut self, len: usize) {
        Vec::truncate(self, len);
    }
}

/// A record made in place, in the bytes of a reservation (the store's writable mapping of the
/// file that will hold it): never past them, so nothing is allocated for its bytes.
pub struct Fixed<'m> {
    buf: &'m mut [u8],
    len: usize,
}

impl<'m> Fixed<'m> {
    pub fn new(buf: &'m mut [u8]) -> Fixed<'m> {
        Fixed { buf, len: 0 }
    }

    /// The record's length: what the reservation is cut to when it is finished.
    pub fn written(&self) -> u64 {
        self.len as u64
    }
}

impl Sink for Fixed<'_> {
    fn len(&self) -> usize {
        self.len
    }

    fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.buf[..self.len]
    }

    fn room(&mut self, more: usize, _exact: bool) -> Result<(), PackRefused> {
        match self.len.checked_add(more) {
            Some(end) if end <= self.buf.len() => Ok(()),
            _ => Err(PackRefused::RoomOver { room: self.buf.len() as u64 }),
        }
    }

    fn extend(&mut self, bytes: &[u8]) {
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }

    fn zeros(&mut self, n: usize) {
        // A reservation starts as zeros, but these bytes are written whatever they held.
        self.buf[self.len..self.len + n].fill(0);
        self.len += n;
    }

    fn truncate(&mut self, len: usize) {
        if len < self.len {
            self.len = len;
        }
    }
}

/// A record being made: files added in the archive's order, their bytes written straight into
/// the record, compressed in place as each file closes, then [`Builder::finish`].
pub struct Builder<'a, S: Sink = Vec<u8, Alloc<'a>>> {
    kind: Kind,
    source: [u8; 32],
    limits: Limits,
    rows: Vec<Row, Alloc<'a>>,
    paths: Vec<u8, Alloc<'a>>,
    /// Total uncompressed bytes across all files added so far (enforced against `limits.most_bytes`).
    raw_total: u64,
    /// Number of leading rows already hashed, validated, and compressed in place.
    sealed: usize,
    alloc: Alloc<'a>,
    encoder: Option<ZstdEncoder<Alloc<'a>>>,
    dict: Option<ZstdDict<Alloc<'a>>>,
    comp_buf: Vec<u8, Alloc<'a>>,
    /// The record: its header's room, then the stored data so far.
    out: S,
}

impl<'a> Builder<'a> {
    /// A builder whose record has room for `data` bytes of files reserved at once (the caller's
    /// best knowledge, held to the limit: a git tree's exact sum, a gzip's stated size). More
    /// is reserved as it comes, never more than the limit.
    pub fn new(kind: Kind, source: [u8; 32], limits: Limits, data: u64, alloc: Alloc<'a>) -> Result<Builder<'a>, PackRefused> {
        Builder::with_sink(kind, source, limits, data, Vec::new_in(alloc), alloc)
    }
}

impl<'a, S: Sink> Builder<'a, S> {
    /// A builder writing into `out` (empty), with room for `data` bytes of files asked at once
    /// as [`Builder::new`] does; the rows and paths are kept in `alloc` until
    /// [`Builder::finish`] writes them after the data.
    pub fn with_sink(kind: Kind, source: [u8; 32], limits: Limits, data: u64, mut out: S, alloc: Alloc<'a>) -> Result<Builder<'a, S>, PackRefused> {
        let room = if crate::planted(crate::Plant::SizeFromWire) { data } else { data.min(limits.most_bytes) };
        let room = usize::try_from(room).map_err(|_| PackRefused::BytesOver { most: limits.most_bytes })?;
        out.room(HEADER + room, true)?;
        out.zeros(HEADER);
        Ok(Builder {
            kind,
            source,
            limits,
            rows: Vec::new_in(alloc),
            paths: Vec::new_in(alloc),
            raw_total: 0,
            sealed: 0,
            alloc,
            encoder: None,
            dict: None,
            comp_buf: Vec::new_in(alloc),
            out,
        })
    }

    fn stored_data_len(&self) -> u64 {
        u64::try_from(self.out.len().saturating_sub(HEADER)).unwrap_or(u64::MAX)
    }

    /// Hashes, validates symlink containment, and compresses the most recently added file in place
    /// if not already sealed. Compressed bytes are only kept when strictly smaller than raw bytes.
    fn seal_last_file(&mut self) -> Result<(), PackRefused> {
        if self.sealed >= self.rows.len() {
            return Ok(());
        }
        let idx = self.rows.len().saturating_sub(1);
        let (path_at, path_len, mode, data_at, raw_len) = {
            let Some(row) = self.rows.get(idx) else {
                return Ok(());
            };
            (row.path_at, row.path_len, row.mode, row.data_at, row.raw_len)
        };
        let start = HEADER
            .checked_add(usize::try_from(data_at).map_err(|_| PackRefused::Form { why: "data offset overflow" })?)
            .ok_or(PackRefused::Form { why: "data offset overflow" })?;
        let raw_usize = usize::try_from(raw_len).map_err(|_| PackRefused::Form { why: "file length overflow" })?;
        let end = start.checked_add(raw_usize).ok_or(PackRefused::Form { why: "data range overflow" })?;
        let blake3 = {
            let raw = self.out.bytes().get(start..end).ok_or(PackRefused::Form { why: "file range outside buffer" })?;
            if mode == Mode::Link {
                let path = path_at_len(&self.paths, path_at, path_len);
                if !link_inside(path, raw) {
                    return Err(PackRefused::LinkOut { path: path.into() });
                }
            }
            if mode == Mode::Submodule && raw_usize != 20 {
                return Err(PackRefused::Form { why: "a submodule entry must be a 20-byte commit id" });
            }
            Blake3::of(raw).0
        };

        let mut codec = Codec::Raw;
        let mut stored_len = raw_len;
        if mode != Mode::Submodule && raw_usize >= MIN_COMPRESS_LEN {
            let enc = match self.encoder.as_mut() {
                Some(enc) => enc,
                None => {
                    let built = ZstdEncoder::try_new_in(self.alloc)
                        .map_err(|_| PackRefused::Form { why: "pack encoder init failed" })?;
                    self.encoder.insert(built)
                }
            };
            let opts = CompressOpts { level: Level::Default, checksum: false };
            self.comp_buf.clear();
            let raw = self.out.bytes().get(start..end).ok_or(PackRefused::Form { why: "file range outside buffer" })?;
            let compressed_ok = if raw_usize <= MAX_DICT_FILE_LEN {
                let dict = match self.dict.as_ref() {
                    Some(d) => d,
                    None => {
                        let built = ZstdDict::try_new_in(RUST_DICT, self.alloc)
                            .map_err(|_| PackRefused::Form { why: "pack dictionary init failed" })?;
                        self.dict.insert(built)
                    }
                };
                enc.compress_with_dict_into(raw, dict, opts, &mut self.comp_buf).is_ok()
            } else {
                enc.compress_into(raw, opts, &mut self.comp_buf).is_ok()
            };
            if compressed_ok && self.comp_buf.len() < raw_usize {
                let comp_len = self.comp_buf.len();
                let dst_end = start.checked_add(comp_len).ok_or(PackRefused::Form { why: "compressed end overflow" })?;
                if let Some(dst) = self.out.bytes_mut().get_mut(start..dst_end) {
                    dst.copy_from_slice(self.comp_buf.as_slice());
                    self.out.truncate(dst_end);
                    codec = if raw_usize <= MAX_DICT_FILE_LEN { Codec::ZstdRustDict } else { Codec::Zstd };
                    stored_len = u64::try_from(comp_len).unwrap_or(raw_len);
                }
            }
        }

        if let Some(row) = self.rows.get_mut(idx) {
            row.blake3 = blake3;
            row.codec = codec;
            row.stored_len = stored_len;
        }
        self.sealed = self.rows.len();
        Ok(())
    }

    /// Starts a file at `path` of `size` bytes, as its archive declares it: the size is held to
    /// what is left of the limit before anything is reserved for it.
    pub fn begin(&mut self, path: &str, mode: Mode, size: u64) -> Result<(), PackRefused> {
        self.seal_last_file()?;
        if u64::try_from(self.rows.len()).unwrap_or(u64::MAX) >= u64::from(self.limits.most_files) {
            return Err(PackRefused::FilesOver { most: self.limits.most_files });
        }
        let Ok(path_len) = u16::try_from(path.len()) else {
            return Err(PackRefused::PathOver { most: self.limits.most_path });
        };
        if path_len > self.limits.most_path {
            return Err(PackRefused::PathOver { most: self.limits.most_path });
        }
        let left = self.limits.most_bytes.saturating_sub(self.raw_total);
        if size > left && !crate::planted(crate::Plant::SizeFromWire) {
            return Err(PackRefused::BytesOver { most: self.limits.most_bytes });
        }
        let size = usize::try_from(size).map_err(|_| PackRefused::BytesOver { most: self.limits.most_bytes })?;
        self.out.room(size, false)?;
        self.paths.try_reserve(path.len()).map_err(PackRefused::Alloc)?;
        self.rows.try_reserve(1).map_err(PackRefused::Alloc)?;
        let path_at = u32::try_from(self.paths.len()).map_err(|_| PackRefused::Form { why: "paths overflow" })?;
        let row = Row {
            path_at,
            path_len,
            mode,
            codec: Codec::Raw,
            data_at: self.stored_data_len(),
            stored_len: 0,
            raw_len: 0,
            blake3: [0; 32],
        };
        self.paths.extend_from_slice(path.as_bytes());
        self.rows.push(row);
        Ok(())
    }

    /// The next bytes of the file begun last.
    pub fn append(&mut self, bytes: &[u8]) -> Result<(), PackRefused> {
        let len_u64 = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let left = self.limits.most_bytes.saturating_sub(self.raw_total);
        if len_u64 > left {
            return Err(PackRefused::BytesOver { most: self.limits.most_bytes });
        }
        let Some(row) = self.rows.last_mut() else {
            return Err(PackRefused::Form { why: "file bytes before any file" });
        };
        self.out.room(bytes.len(), false)?;
        self.out.extend(bytes);
        row.raw_len = row.raw_len.saturating_add(len_u64);
        row.stored_len = row.raw_len;
        self.raw_total = self.raw_total.saturating_add(len_u64);
        Ok(())
    }

    /// A whole file.
    pub fn file(&mut self, path: &str, mode: Mode, bytes: &[u8]) -> Result<(), PackRefused> {
        self.begin(path, mode, u64::try_from(bytes.len()).unwrap_or(u64::MAX))?;
        self.append(bytes)
    }

    /// A file of `size` bytes, zeroed, for the caller to fill in place: its bytes are written
    /// once, into the record.
    pub fn slot(&mut self, path: &str, mode: Mode, size: u64) -> Result<&mut [u8], PackRefused> {
        self.begin(path, mode, size)?;
        let at = self.out.len();
        let size_usize = usize::try_from(size).map_err(|_| PackRefused::BytesOver { most: self.limits.most_bytes })?;
        self.out.zeros(size_usize);
        if let Some(row) = self.rows.last_mut() {
            row.raw_len = size;
            row.stored_len = size;
        }
        self.raw_total = self.raw_total.saturating_add(size);
        Ok(&mut self.out.bytes_mut()[at..])
    }

    /// Files added so far, and their uncompressed bytes.
    pub fn counts(&self) -> (u32, u64) {
        (u32::try_from(self.rows.len()).unwrap_or(u32::MAX), self.raw_total)
    }

    /// The record: every path checked by the listing's rules and every link's target held
    /// inside, every file compressed when smaller, the rows sorted, the listing's and the
    /// record's digests written.
    pub fn finish(mut self) -> Result<S, PackRefused> {
        self.seal_last_file()?;
        // The listing's rules and its digest, over the files as the tree module knows them.
        let mut entries: alloc::vec::Vec<tree::Entry> = alloc::vec::Vec::with_capacity(self.rows.len()); // alloc: the listing's own form, once a record
        for row in self.rows.iter() {
            let path = path_of(&self.paths, row);
            entries.push(tree::Entry { path: path.into(), kind: row.mode, blake3: Blake3(row.blake3), len: row.raw_len });
        }
        let listing = tree::listing(&mut entries).map_err(PackRefused::Tree)?;
        let tree_digest = Blake3::of(&listing).0;
        drop(entries);
        drop(listing);
        let paths = &self.paths;
        self.rows.sort_unstable_by(|a, b| path_of(paths, a).as_bytes().cmp(path_of(paths, b).as_bytes()));
        let data_len = self.stored_data_len();
        let tail = self
            .paths
            .len()
            .checked_add(self.rows.len().checked_mul(ROW).ok_or(PackRefused::Form { why: "rows overflow" })?)
            .and_then(|n| n.checked_add(FOOTER))
            .ok_or(PackRefused::Form { why: "record size overflow" })?;
        self.out.room(tail, true)?;
        self.out.extend(&self.paths);
        for row in self.rows.iter() {
            self.out.extend(&row.path_at.to_le_bytes());
            self.out.extend(&row.path_len.to_le_bytes());
            self.out.extend(&[mode_byte(row.mode), row.codec.byte()]);
            self.out.extend(&row.data_at.to_le_bytes());
            self.out.extend(&row.stored_len.to_le_bytes());
            self.out.extend(&row.raw_len.to_le_bytes());
            self.out.extend(&row.blake3);
        }
        let n_u32 = u32::try_from(self.rows.len()).map_err(|_| PackRefused::Form { why: "file count overflow" })?;
        let paths_u64 = u64::try_from(self.paths.len()).map_err(|_| PackRefused::Form { why: "paths length overflow" })?;
        let header = &mut self.out.bytes_mut()[..HEADER];
        header[..8].copy_from_slice(MAGIC);
        header[8..12].copy_from_slice(&[self.kind.byte(), 0, 0, 0]);
        header[12..16].copy_from_slice(&n_u32.to_le_bytes());
        header[16..24].copy_from_slice(&paths_u64.to_le_bytes());
        header[24..32].copy_from_slice(&data_len.to_le_bytes());
        header[32..64].copy_from_slice(&self.source);
        header[64..96].copy_from_slice(&tree_digest);
        let digest = Blake3::of(self.out.bytes()).0;
        self.out.extend(&digest);
        Ok(self.out)
    }
}

fn path_at_len(paths: &[u8], path_at: u32, path_len: u16) -> &str {
    let at = usize::try_from(path_at).unwrap_or(0);
    let end = at.saturating_add(usize::from(path_len));
    // Built from `&str`s, and cut only at their ends.
    core::str::from_utf8(paths.get(at..end).unwrap_or(&[])).expect("paths are joined `str`s")
}

fn path_of<'p>(paths: &'p [u8], row: &Row) -> &'p str {
    path_at_len(paths, row.path_at, row.path_len)
}

fn mode_byte(mode: Mode) -> u8 {
    match mode {
        Mode::File => 0,
        Mode::Exec => 1,
        Mode::Link => 2,
        Mode::Submodule => 3,
    }
}

/// Whether a link at `path` to `target` (relative, `/`-separated) stays inside the folder:
/// resolved part by part from the link's own folder, never above the top. An absolute target
/// or one with a `\` or a NUL is outside.
pub(crate) fn link_inside(path: &str, target: &[u8]) -> bool {
    let Ok(target) = core::str::from_utf8(target) else { return false };
    if target.is_empty() || target.starts_with('/') || target.contains('\\') || target.contains('\0') {
        return false;
    }
    // Depth below the top of the link's folder.
    let mut depth = path.matches('/').count() as i64;
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => depth += 1,
        }
    }
    true
}

/// A record read in place, its form and digest checked once ([`PackView::open`]).
#[derive(Clone, Copy)]
pub struct PackView<'b> {
    bytes: &'b [u8],
    n: usize,
    paths_at: usize,
    rows_at: usize,
    data_len: u64,
}

/// Zero-copy metadata and stored payload slice for one file of a record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileEntry<'b> {
    pub path: &'b str,
    pub mode: Mode,
    pub codec: Codec,
    pub raw_len: u64,
    pub stored: &'b [u8],
    /// The file's BLAKE3 over its uncompressed bytes (covered by the record's digest).
    pub blake3: Blake3,
}

/// Uncompressed bytes of one file read from a [`PackView`]: borrowed zero-copy when stored raw,
/// or decompressed into the caller's allocator `A`.
pub enum FileBytes<'b, A: Allocator = Alloc<'b>> {
    Borrowed(&'b [u8]),
    Owned(Vec<u8, A>),
}

impl<'b, A: Allocator> FileBytes<'b, A> {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            FileBytes::Borrowed(s) => s,
            FileBytes::Owned(v) => v.as_slice(),
        }
    }
}

impl<A: Allocator> Deref for FileBytes<'_, A> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl<A: Allocator> AsRef<[u8]> for FileBytes<'_, A> {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl<A: Allocator> PartialEq for FileBytes<'_, A> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<A: Allocator> Eq for FileBytes<'_, A> {}

impl<A: Allocator> PartialEq<[u8]> for FileBytes<'_, A> {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_slice() == other
    }
}

impl<A: Allocator> PartialEq<&[u8]> for FileBytes<'_, A> {
    fn eq(&self, other: &&[u8]) -> bool {
        self.as_slice() == *other
    }
}

impl<A: Allocator> fmt::Debug for FileBytes<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_slice().fmt(f)
    }
}

/// One file of a record with its uncompressed bytes materialized.
#[derive(PartialEq, Eq, Debug)]
pub struct File<'b, A: Allocator = Alloc<'b>> {
    pub path: &'b str,
    pub mode: Mode,
    /// Uncompressed file bytes: borrowed zero-copy when stored raw, or decompressed into `A`.
    pub bytes: FileBytes<'b, A>,
    /// The file's BLAKE3, as the record states it (covered by the record's digest).
    pub blake3: Blake3,
}

impl<'b> FileEntry<'b> {
    /// If this entry is a recorded git submodule (`Mode::Submodule`), returns its 20-byte pinned
    /// commit SHA-1; otherwise `None`.
    pub fn submodule_commit(&self) -> Option<[u8; 20]> {
        if self.mode != Mode::Submodule {
            return None;
        }
        let oid = self.stored.get(..20)?;
        if self.stored.len() != 20 {
            return None;
        }
        let mut out = [0u8; 20];
        out.copy_from_slice(oid);
        Some(out)
    }

    fn submodule_err(&self, path: &str) -> PackRefused {
        let commit = self.submodule_commit().unwrap_or([0u8; 20]);
        PackRefused::Submodule {
            path: crate::unpack::Cut::of(path.as_bytes()),
            submodule: crate::unpack::Cut::of(self.path.as_bytes()),
            commit,
        }
    }

    /// Reads this file's uncompressed bytes into `alloc` (zero-copy when `self.codec == Codec::Raw`).
    /// Fails closed with [`PackRefused::Submodule`] when called on a gitlink submodule entry.
    pub fn read_in<A: Allocator + Copy>(&self, alloc: A) -> Result<FileBytes<'b, A>, PackRefused> {
        if self.mode == Mode::Submodule {
            return Err(self.submodule_err(self.path));
        }
        match self.codec {
            Codec::Raw => Ok(FileBytes::Borrowed(self.stored)),
            Codec::Zstd | Codec::ZstdRustDict => {
                let raw_usize = usize::try_from(self.raw_len).map_err(|_| PackRefused::Form { why: "decompressed length overflow" })?;
                let limits = ZstdLimits { most_output_bytes: raw_usize, most_window_bytes: 8 << 20, most_frames: 1 };
                let out = match self.codec {
                    Codec::Zstd => decompress_in(self.stored, limits, alloc),
                    Codec::ZstdRustDict => decompress_with_dict_in(self.stored, RUST_DICT, limits, alloc),
                    Codec::Raw => unreachable!(),
                }
                .map_err(|_| PackRefused::Form { why: "corrupt compressed file payload" })?;
                if out.len() != raw_usize {
                    return Err(PackRefused::Form { why: "decompressed length does not match row" });
                }
                Ok(FileBytes::Owned(out))
            }
        }
    }

    /// Reads this file's uncompressed bytes as a `&'f [u8]` slice: zero-copy `&'b [u8]` when
    /// stored raw, or decompressed directly into the bump allocator `bump` and frozen in place.
    pub fn read_slice<'f, A>(&self, bump: A) -> Result<&'f [u8], PackRefused>
    where
        'b: 'f,
        A: blaze_own::BumpAlloc + Copy + 'f,
    {
        match self.read_in(bump)? {
            FileBytes::Borrowed(s) => Ok(s),
            FileBytes::Owned(v) => Ok(v.into_bump_slice()),
        }
    }
}

impl<'b> PackView<'b> {
    /// `bytes` as a record: its length, its digest, every row's ranges and its order checked.
    pub fn open(bytes: &'b [u8]) -> Result<PackView<'b>, PackRefused> {
        let view = PackView::form(bytes)?;
        let body = bytes.len() - FOOTER;
        if Blake3::of(&bytes[..body]).0 != bytes[body..] {
            return Err(PackRefused::Digest);
        }
        view.rows_checked()?;
        Ok(view)
    }

    /// The reader of a stored source record: `bytes` (the mapped record found at `id.key()`)
    /// opened as [`PackView::open`] does, once, and refused unless it holds the sources `id`
    /// names (its kind and the `.crate`'s SHA-256 or the commit id). After it, every
    /// [`PackView::entries`] row (sorted, with its size and BLAKE3) is read in place, borrowed
    /// from `bytes`, with no check again.
    pub fn open_source(id: &crate::SourceId<'_>, bytes: &'b [u8]) -> Result<PackView<'b>, PackRefused> {
        PackView::open(bytes)?.of_source(id)
    }

    /// [`PackView::open_source`] of a record whose digest was checked when this machine
    /// fetched and put it (a record from elsewhere, the shared cache, takes `open_source`):
    /// its form, rows and source checked, its bytes not hashed again (the memory rule: bytes
    /// verified once per fetch, never on each read).
    pub fn open_source_put(id: &crate::SourceId<'_>, bytes: &'b [u8]) -> Result<PackView<'b>, PackRefused> {
        PackView::open_verified(bytes)?.of_source(id)
    }

    fn of_source(self, id: &crate::SourceId<'_>) -> Result<PackView<'b>, PackRefused> {
        let view = self;
        let (kind, source) = match id {
            crate::SourceId::Crate { sha256, .. } => (Kind::Crate, *sha256),
            crate::SourceId::Git { commit } => {
                let mut source = [0; 32];
                source[..20].copy_from_slice(&commit.0);
                (Kind::Git, source)
            }
        };
        if view.kind() != kind || *view.source() != source {
            return Err(PackRefused::NotTheSource);
        }
        Ok(view)
    }

    /// `bytes` as a record whose digest was checked when it was fetched or put (the store's
    /// key binds it): its lengths, rows and order checked, its digest not computed again (the
    /// memory-and-data rule: bytes verified once per fetch, never on each read).
    pub fn open_verified(bytes: &'b [u8]) -> Result<PackView<'b>, PackRefused> {
        let view = PackView::form(bytes)?;
        view.rows_checked()?;
        Ok(view)
    }

    fn form(bytes: &'b [u8]) -> Result<PackView<'b>, PackRefused> {
        let form = |why| PackRefused::Form { why };
        if bytes.len() < HEADER + FOOTER || bytes.get(..8) != Some(MAGIC.as_slice()) {
            return Err(form("no header"));
        }
        if !matches!(bytes[8], 1..=3) || bytes[9..12] != [0, 0, 0] {
            return Err(form("an unknown kind"));
        }
        let n = u64::from(u32::from_le_bytes(bytes[12..16].try_into().expect("4 bytes")));
        let paths = u64::from_le_bytes(bytes[16..24].try_into().expect("8 bytes"));
        let data = u64::from_le_bytes(bytes[24..32].try_into().expect("8 bytes"));
        let row_u64 = u64::try_from(ROW).unwrap_or(0);
        let hdr_ftr = u64::try_from(HEADER + FOOTER).unwrap_or(0);
        let total = hdr_ftr
            .checked_add(n.checked_mul(row_u64).ok_or(form("lengths overflow"))?)
            .and_then(|it| it.checked_add(paths))
            .and_then(|it| it.checked_add(data))
            .ok_or(form("lengths overflow"))?;
        if total != u64::try_from(bytes.len()).unwrap_or(u64::MAX) {
            return Err(form("lengths that do not add up to its size"));
        }
        let data_usize = usize::try_from(data).map_err(|_| form("lengths overflow"))?;
        let paths_usize = usize::try_from(paths).map_err(|_| form("lengths overflow"))?;
        let n_usize = usize::try_from(n).map_err(|_| form("lengths overflow"))?;
        let paths_at = HEADER.checked_add(data_usize).ok_or(form("lengths overflow"))?;
        let rows_at = paths_at.checked_add(paths_usize).ok_or(form("lengths overflow"))?;
        Ok(PackView { bytes, n: n_usize, paths_at, rows_at, data_len: data })
    }

    fn rows_checked(&self) -> Result<(), PackRefused> {
        let form = |why| PackRefused::Form { why };
        let paths = u64::try_from(self.rows_at.saturating_sub(self.paths_at)).unwrap_or(0);
        let mut last: Option<&str> = None;
        for at in 0..self.n {
            let r_at = self.rows_at + at * ROW;
            let Some(r) = self.bytes.get(r_at..r_at + ROW) else {
                return Err(form("a row outside its record"));
            };
            if r[6] > 3 {
                return Err(form("an unknown mode"));
            }
            let Some(codec) = Codec::from_byte(r[7]) else {
                return Err(form("an unknown codec"));
            };
            let row = self.row(at);
            if u64::from(row.path_at) + u64::from(row.path_len) > paths
                || row.data_at.checked_add(row.stored_len).is_none_or(|end| end > self.data_len)
            {
                return Err(form("a row outside its record"));
            }
            if codec == Codec::Raw && row.stored_len != row.raw_len {
                return Err(form("raw row stored and raw lengths differ"));
            }
            if row.mode == Mode::Submodule && (codec != Codec::Raw || row.raw_len != 20) {
                return Err(form("a submodule row must be 20 raw bytes"));
            }
            let start = self.paths_at + usize::try_from(row.path_at).map_err(|_| form("path offset overflow"))?;
            let end = start.checked_add(usize::from(row.path_len)).ok_or(form("path range overflow"))?;
            let path = core::str::from_utf8(self.bytes.get(start..end).ok_or(form("a row outside its record"))?)
                .map_err(|_| form("a path that is not UTF-8"))?;
            if last.is_some_and(|last| last.as_bytes() >= path.as_bytes()) {
                return Err(form("rows out of order or twice"));
            }
            last = Some(path);
        }
        Ok(())
    }

    fn row(&self, at: usize) -> Row {
        let r = &self.bytes[self.rows_at + at * ROW..self.rows_at + (at + 1) * ROW];
        Row {
            path_at: u32::from_le_bytes(r[0..4].try_into().expect("4 bytes")),
            path_len: u16::from_le_bytes(r[4..6].try_into().expect("2 bytes")),
            mode: match r[6] {
                0 => Mode::File,
                1 => Mode::Exec,
                2 => Mode::Link,
                _ => Mode::Submodule,
            },
            codec: Codec::from_byte(r[7]).unwrap_or(Codec::Raw),
            data_at: u64::from_le_bytes(r[8..16].try_into().expect("8 bytes")),
            stored_len: u64::from_le_bytes(r[16..24].try_into().expect("8 bytes")),
            raw_len: u64::from_le_bytes(r[24..32].try_into().expect("8 bytes")),
            blake3: r[32..64].try_into().expect("32 bytes"),
        }
    }

    pub fn kind(&self) -> Kind {
        match self.bytes[8] {
            1 => Kind::Crate,
            2 => Kind::Git,
            _ => Kind::Tree,
        }
    }

    /// The `.crate`'s SHA-256, or the commit id followed by 12 zeros.
    pub fn source(&self) -> &'b [u8; 32] {
        self.bytes[32..64].try_into().expect("32 bytes")
    }

    /// The BLAKE3 of the files' listing (crate::tree): what a source pin is over.
    pub fn tree(&self) -> Blake3 {
        Blake3(self.bytes[64..96].try_into().expect("32 bytes"))
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Zero-copy metadata entry for file `at`.
    pub fn entry(&self, at: usize) -> FileEntry<'b> {
        let row = self.row(at);
        let start = self.paths_at + usize::try_from(row.path_at).unwrap_or(0);
        let path = core::str::from_utf8(&self.bytes[start..start + usize::from(row.path_len)]).expect("checked at open");
        let data = HEADER + usize::try_from(row.data_at).unwrap_or(0);
        let stored_len = usize::try_from(row.stored_len).unwrap_or(0);
        FileEntry {
            path,
            mode: row.mode,
            codec: row.codec,
            raw_len: row.raw_len,
            stored: &self.bytes[data..data + stored_len],
            blake3: Blake3(row.blake3),
        }
    }

    /// Zero-copy metadata iterator over every file in path order (never decompresses payloads).
    pub fn entries(&self) -> impl Iterator<Item = FileEntry<'b>> + '_ {
        (0..self.n).map(|at| self.entry(at))
    }

    /// Zero-copy index lookup of `path` by binary search (never decompresses payload).
    pub fn find_index(&self, path: &str) -> Option<usize> {
        let (mut low, mut high) = (0, self.n);
        while low < high {
            let mid = low + (high - low) / 2;
            let entry = self.entry(mid);
            match entry.path.as_bytes().cmp(path.as_bytes()) {
                core::cmp::Ordering::Less => low = mid + 1,
                core::cmp::Ordering::Greater => high = mid,
                core::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// Zero-copy metadata lookup of `path`, by binary search (never decompresses payload).
    pub fn find_entry(&self, path: &str) -> Option<FileEntry<'b>> {
        self.find_index(path).map(|at| self.entry(at))
    }

    /// If `path` is itself a recorded git submodule or lies beneath one (`<subpath>/...`),
    /// returns that submodule's entry.
    pub fn submodule_covering(&self, path: &str) -> Option<FileEntry<'b>> {
        if let Some(entry) = self.find_entry(path)
            && entry.mode == Mode::Submodule
        {
            return Some(entry);
        }
        for (slash, _) in path.match_indices('/') {
            let Some(prefix) = path.get(..slash) else { continue };
            if let Some(entry) = self.find_entry(prefix)
                && entry.mode == Mode::Submodule
            {
                return Some(entry);
            }
        }
        None
    }

    /// Reads file `at`, decompressing into `alloc` if compressed (zero-copy if stored raw).
    pub fn read_at<A: Allocator + Copy>(&self, at: usize, alloc: A) -> Result<File<'b, A>, PackRefused> {
        let entry = self.entry(at);
        let bytes = entry.read_in(alloc)?;
        Ok(File { path: entry.path, mode: entry.mode, bytes, blake3: entry.blake3 })
    }

    /// Finds and reads the file at `path`, decompressing into `alloc` if compressed.
    /// Fails closed with [`PackRefused::Submodule`] if `path` is a recorded submodule or lies
    /// beneath one.
    pub fn read<A: Allocator + Copy>(&self, path: &str, alloc: A) -> Result<Option<File<'b, A>>, PackRefused> {
        let Some(entry) = self.find_entry(path) else {
            if let Some(sub) = self.submodule_covering(path) {
                return Err(sub.submodule_err(path));
            }
            return Ok(None);
        };
        let bytes = entry.read_in(alloc)?;
        Ok(Some(File { path: entry.path, mode: entry.mode, bytes, blake3: entry.blake3 }))
    }

    /// Finds and reads the file at `path` into a bump slice `&'f [u8]` (zero-copy when raw).
    /// Fails closed with [`PackRefused::Submodule`] if `path` is a recorded submodule or lies
    /// beneath one.
    pub fn read_slice<'f, A>(&self, path: &str, bump: A) -> Result<Option<&'f [u8]>, PackRefused>
    where
        'b: 'f,
        A: blaze_own::BumpAlloc + Copy + 'f,
    {
        let Some(entry) = self.find_entry(path) else {
            if let Some(sub) = self.submodule_covering(path) {
                return Err(sub.submodule_err(path));
            }
            return Ok(None);
        };
        entry.read_slice(bump).map(Some)
    }

    /// Finds and reads the file at `path` into a bump `&'f str` slice (zero-copy when raw).
    pub fn read_str_slice<'f, A>(&self, path: &str, bump: A) -> Result<Option<&'f str>, PackRefused>
    where
        'b: 'f,
        A: blaze_own::BumpAlloc + Copy + 'f,
    {
        let Some(bytes) = self.read_slice(path, bump)? else {
            return Ok(None);
        };
        Ok(core::str::from_utf8(bytes).ok())
    }

    /// Reads the file at `path` into a [`Bump<'f>`] slice (convenience wrapper over [`Self::read_slice`]).
    pub fn read_bump<'f>(&self, path: &str, bump: Bump<'f>) -> Result<Option<&'f [u8]>, PackRefused>
    where
        'b: 'f,
    {
        self.read_slice(path, bump)
    }

    /// Stored payload bytes across all files (compressed + raw).
    pub fn data_len(&self) -> u64 {
        self.data_len
    }

    /// Sum of uncompressed lengths across all files.
    pub fn raw_data_len(&self) -> u64 {
        let mut total = 0u64;
        for at in 0..self.n {
            total = total.saturating_add(self.row(at).raw_len);
        }
        total
    }
}
