//! Opening a toolchain from its shelf (toolchain.md, section 2): the per-user folder where
//! every record is one file named by its 64-hex digest. The driver implements [`Shelf`] (a
//! read-only folder: the cli's is bstd's `Fs::ancestor`); this module decides what is read and
//! checks it. A size is held to its limit before a byte is read, a digest is checked once, and
//! anything absent or other is a [`ToolchainError`] naming the digest and the folder. Never a
//! fallback to another version, a path or rustup (rules 3, 8).

use alloc::{string::String, string::ToString};
use core::fmt;

use blaze_fetch::{Blake3, PackView};

use crate::record::{CrateRow, RecordRefused, ToolchainView};
use crate::{Hex, ToolchainId};

/// The most bytes a toolchain manifest may be (rule 18).
pub const MOST_MANIFEST: u64 = 4 << 20;
/// The most bytes one crate's pack may be: core's, with stdarch's `core_arch` it reaches, is
/// the largest, about 30 MiB.
pub const MOST_PACK: u64 = 256 << 20;

/// A read-only folder of records named by their hex digests. `name` is always 64 lower-case
/// hex digits: an implementation reads nothing else (a name of another form is a bug here).
pub trait Shelf {
    /// What the driver said, when a read failed for another reason than absence.
    type Error: fmt::Display;
    /// The bytes of a file, in what the driver reads into (a frame's, a mapping).
    type Bytes: AsRef<[u8]>;
    /// Where it is, as a person would look for it (for errors only; never in a key).
    fn place(&self) -> &str;
    /// The size of the file `name`, or `None` when there is none.
    fn size(&mut self, name: &str) -> Result<Option<u64>, Self::Error>;
    /// The whole file `name`, whose size was just asked: at most `size` bytes are read.
    fn read(&mut self, name: &str, size: u64) -> Result<Self::Bytes, Self::Error>;
}

/// What is being opened: the manifest, or one crate's pack.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum What {
    Manifest,
    Pack { name: String, version: String },
}

impl fmt::Display for What {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            What::Manifest => f.write_str("the toolchain manifest"),
            What::Pack { name, version } => write!(f, "the sources of `{name}` {version}"),
        }
    }
}

/// Why a toolchain or a crate of it could not be opened: each names the toolchain, the digest
/// and the folder looked in (rule 19).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ToolchainError {
    pub toolchain: ToolchainId,
    pub what: What,
    pub digest: [u8; 32],
    pub place: String,
    pub why: Why,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Why {
    /// No file of that digest.
    Absent,
    /// A size other than the manifest states, or past the limit.
    Size { found: u64, expected: u64 },
    /// The bytes' BLAKE3 is not the digest they are named by.
    Digest { found: [u8; 32] },
    /// The manifest's form.
    Record(RecordRefused),
    /// The pack's form.
    Pack(String),
    /// The driver could not read it.
    Read(String),
    /// The manifest holds no sysroot graph for the target.
    NoTarget { triple: String },
}

impl fmt::Display for ToolchainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let file = Hex::of(&self.digest);
        write!(f, "toolchain {}: {} (file {file} in {}): ", self.toolchain, self.what, self.place)?;
        match &self.why {
            Why::Absent => write!(f, "not there; this blaze needs its own toolchain installed, run `blaze-toolchain` from its tree or reinstall"),
            Why::Size { found, expected } => write!(f, "{found} bytes, not the {expected} its manifest states"),
            Why::Digest { found } => write!(f, "its BLAKE3 is {}, not its name", Hex::of(found)),
            Why::Record(why) => write!(f, "{why}"),
            Why::Pack(why) => write!(f, "{why}"),
            Why::Read(said) => write!(f, "{said}"),
            Why::NoTarget { triple } => write!(f, "it holds no sysroot for the target `{triple}`"),
        }
    }
}

impl core::error::Error for ToolchainError {}

/// Reads `name` whole when its size is `expected`, or at most `most` when no size is stated.
fn read_sized<S: Shelf>(shelf: &mut S, digest: &[u8; 32], expected: Option<u64>, most: u64) -> Result<S::Bytes, Why> {
    let name = Hex::of(digest);
    let size = shelf.size(name.as_str()).map_err(|said| Why::Read(said.to_string()))?.ok_or(Why::Absent)?;
    if let Some(expected) = expected
        && size != expected
    {
        return Err(Why::Size { found: size, expected });
    }
    if size > most {
        return Err(Why::Size { found: size, expected: most });
    }
    let bytes = shelf.read(name.as_str(), size).map_err(|said| Why::Read(said.to_string()))?;
    let read = bytes.as_ref().len() as u64;
    if read != size {
        return Err(Why::Size { found: read, expected: size });
    }
    Ok(bytes)
}

/// The manifest of toolchain `id` from `shelf`: open it with [`ToolchainView::open`] (done here
/// once too, so the bytes returned are a checked record).
pub fn open_manifest<S: Shelf>(shelf: &mut S, id: ToolchainId) -> Result<S::Bytes, ToolchainError> {
    let error = |shelf: &S, why| ToolchainError { toolchain: id, what: What::Manifest, digest: id.0, place: shelf.place().into(), why };
    let bytes = read_sized(shelf, &id.0, None, MOST_MANIFEST).map_err(|why| error(shelf, why))?;
    ToolchainView::open(bytes.as_ref(), id).map_err(|why| error(shelf, Why::Record(why)))?;
    Ok(bytes)
}

/// The pack of `krate` from `shelf`, its length checked against the manifest's before it is
/// read, then its BLAKE3 (`PackView::open`) and its name against its footer: read it after with
/// `PackView::open_verified`.
pub fn open_pack<S: Shelf>(shelf: &mut S, id: ToolchainId, krate: &CrateRow<'_>) -> Result<S::Bytes, ToolchainError> {
    let error = |shelf: &S, why| ToolchainError {
        toolchain: id,
        what: What::Pack { name: krate.name.into(), version: krate.version.into() },
        digest: krate.pack,
        place: shelf.place().into(),
        why,
    };
    let bytes = read_sized(shelf, &krate.pack, Some(krate.pack_len), MOST_PACK).map_err(|why| error(shelf, why))?;
    let held = bytes.as_ref();
    PackView::open(held).map_err(|why| error(shelf, Why::Pack(blaze_text::Shown(&why).to_string())))?;
    let footer: [u8; 32] = held[held.len() - 32..].try_into().expect("an opened pack ends in its 32-byte footer");
    if footer != krate.pack {
        return Err(error(shelf, Why::Digest { found: footer }));
    }
    Ok(bytes)
}

/// The BLAKE3 a pack is named by: its footer, which `PackView::open` checks is its bytes'.
pub fn pack_digest(pack: &[u8]) -> Option<[u8; 32]> {
    let at = pack.len().checked_sub(32)?;
    let footer: [u8; 32] = pack[at..].try_into().ok()?;
    (Blake3::of(&pack[..at]).0 == footer).then_some(footer)
}
