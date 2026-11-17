//! The outputs record `bzbsout4`: [`crate::outputs`]'s record with every file over
//! [`BLOB_OVER`] kept as its own record, named here by content id and length
//! (NATIVE.md 3.8, "Large outputs": "a file over 1 MiB is stored as its own record (content id)
//! and the outputs record names it by id and length; the 64 MiB limit becomes 1 GiB in total,
//! 4096 files. bzbsout3 records are an old home: reported and rebuilt (rule 26)").
//!
//! aws-lc's and rocksdb's archives approach 64 MiB; inline they would make one outputs record
//! that large, read whole by every crate that only wants its cfgs. As blobs they are written
//! once, shared by content, and read only by the native link that wants them.
//!
//! The bytes are `bzbsout3`'s with two changes: the layout word, and each entry of the files
//! section. Every other section is byte for byte the encoding of [`Outputs::encode`] (one
//! encoder: `Outputs::encode_head` and `Outputs::encode_tail`):
//!
//! ```text
//! u64  LAYOUT4
//! out-dir and the ten sections before the files, as bzbsout3
//!   files       name, u8 form, then
//!                 form 0 (inline): u64 length, bytes          (length <= BLOB_OVER)
//!                 form 1 (blob):   32-byte content id, u64 length (length >  BLOB_OVER)
//!   c-requests  as bzbsout3
//! ```
//!
//! The form is a function of the length, so one [`Outputs`] has one encoding: a record whose
//! form disagrees with its length is refused ([`RecordError::FileForm`]), never read. The
//! blobs are handed to `put` in file-name order, and their ids are what `put` returns: the
//! content id of the bytes as the store names records.
//!
//! PRODUCER NOTE: a runner that switches from [`Outputs::encode`] to [`encode4`] changes what
//! it produces for an unchanged input; the layout word changes with it, and
//! [`crate::key::RunLimits::DEFAULT`]'s `out_dir_bytes` (in the run's key) moves to 1 GiB in
//! the same change.

#![blaze::sans_io]

use core::fmt;

use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc, String, Vec};

use crate::instr::SearchKind;
use crate::outputs::{self, Cursor, Entry, LinkArg, Note, NoteKind, OutFile, Outputs, RecordError};

/// The record's layout word: `bzbsout` and the layout's number.
pub const LAYOUT4: u64 = u64::from_le_bytes(*b"bzbsout4");

/// A file longer than this many bytes is a blob; one this long or shorter is inline.
pub const BLOB_OVER: u64 = 1 << 20;

/// The domain of [`Outputs4Ref::crate_digest`]: [`outputs::CRATE_VIEW`]'s view over this
/// layout's bytes (a blob covered by its id and length); a change to what it covers takes the
/// next number.
pub const CRATE_VIEW4: &str = "blaze v2 build-script crate view 2";

/// The files section's index among the twelve.
const FILES: usize = 10;
/// The form byte of an inline file.
const INLINE: u8 = 0;
/// The form byte of a blob.
const BLOB: u8 = 1;

/// One file of a `bzbsout4` record: its bytes in the record, or the blob holding them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileRef<'b> {
    Inline(&'b [u8]),
    Blob { id: blaze_ids::ContentId, len: u64 },
}

/// Where a record's blobs are read from: the store, by id.
pub trait Blobs {
    fn get(&self, id: blaze_ids::ContentId, len: u64) -> Result<&[u8], BlobError>;
}

/// Why a blob a record names cannot be read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlobError {
    /// No blob `id` is kept.
    Missing { id: blaze_ids::ContentId },
    /// The blob `id` is `got` bytes; the record names it as `want`.
    Length { id: blaze_ids::ContentId, want: u64, got: u64 },
}

impl fmt::Display for BlobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlobError::Missing { id } => {
                write!(f, "the build-script output file blob {id} is not in the store (an outputs record names it: NATIVE.md 3.8)")
            }
            BlobError::Length { id, want, got } => write!(
                f,
                "the build-script output file blob {id} is {got} bytes; its outputs record names it as {want} (NATIVE.md 3.8)"
            ),
        }
    }
}

impl core::error::Error for BlobError {}

impl<'b> FileRef<'b> {
    /// The file's length in bytes.
    pub fn len(self) -> u64 {
        match self {
            FileRef::Inline(bytes) => bytes.len() as u64,
            FileRef::Blob { len, .. } => len,
        }
    }

    /// Whether the file is empty.
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// The file's bytes: in place, or the blob from `blobs`, its length checked against the
    /// record's.
    pub fn bytes<'s>(self, blobs: &'s (dyn Blobs + 's)) -> Result<&'s [u8], BlobError>
    where
        'b: 's,
    {
        match self {
            FileRef::Inline(bytes) => Ok(bytes),
            FileRef::Blob { id, len } => {
                let got = blobs.get(id, len)?;
                if got.len() as u64 != len {
                    return Err(BlobError::Length { id, want: len, got: got.len() as u64 });
                }
                Ok(got)
            }
        }
    }
}

/// Splits `files` into inline bytes and blobs; `put` receives each blob's bytes and returns its id.
pub fn encode4(outputs: &crate::outputs::Outputs<'_>, put: &mut dyn FnMut(&[u8]) -> blaze_ids::ContentId, out: &mut AVec<'_, u8>) {
    outputs.encode_head(LAYOUT4, out);
    outputs::count(out, outputs.files.len());
    for file in outputs.files.iter() {
        outputs::text(out, &file.name);
        let len = file.bytes.len() as u64;
        if len > BLOB_OVER {
            out.push(BLOB);
            let id = put(&file.bytes);
            out.extend_from_slice(&id.0);
            out.extend_from_slice(&len.to_le_bytes());
        } else {
            out.push(INLINE);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&file.bytes);
        }
    }
    outputs.encode_tail(out);
}

/// One entry of the files section, checked: the form agrees with the length.
fn file_entry<'b>(cur: &mut Cursor<'b>) -> Result<(&'b str, FileRef<'b>), RecordError> {
    let name = cur.text()?;
    let at = cur.at;
    match cur.u8()? {
        INLINE => {
            let len_at = cur.at;
            let len = cur.u64()?;
            if len > BLOB_OVER {
                return Err(RecordError::FileForm { at });
            }
            let len = usize::try_from(len).map_err(|_| RecordError::Short { at: len_at })?;
            Ok((name, FileRef::Inline(cur.take(len)?)))
        }
        BLOB => {
            let mut id = [0u8; 32];
            id.copy_from_slice(cur.take(32)?);
            let len = cur.u64()?;
            if len <= BLOB_OVER {
                return Err(RecordError::FileForm { at });
            }
            Ok((name, FileRef::Blob { id: ContentId(id), len }))
        }
        _ => Err(RecordError::Kind { at }),
    }
}

/// A `bzbsout4` outputs record, read in place: [`outputs::OutputsRef`] with its files as
/// [`FileRef`].
#[derive(Clone, Copy)]
pub struct Outputs4Ref<'b> {
    bytes: &'b [u8],
    out_dir: &'b str,
    starts: [(usize, u32); outputs::SECTIONS],
}

impl<'b> Outputs4Ref<'b> {
    /// Checks every entry of `bytes` once. A `bzbsout3` record is an old home
    /// ([`RecordError::OldHome`]: reported and rebuilt, rule 26), never read.
    pub fn open(bytes: &'b [u8]) -> Result<Outputs4Ref<'b>, crate::outputs::RecordError> {
        let mut cur = Cursor { bytes, at: 0 };
        let tag = cur.u64().map_err(|_| RecordError::Layout)?;
        if tag == outputs::LAYOUT {
            return Err(RecordError::OldHome { tag });
        }
        if tag != LAYOUT4 {
            return Err(RecordError::Layout);
        }
        let at = cur.at;
        let out_dir = cur.text()?;
        if !out_dir.starts_with('/') {
            return Err(RecordError::OutDir { at });
        }
        let mut starts = [(0, 0); outputs::SECTIONS];
        for (section, start) in starts.iter_mut().enumerate() {
            let n = cur.u32()?;
            *start = (cur.at, n);
            for _ in 0..n {
                if section == FILES {
                    file_entry(&mut cur)?;
                } else {
                    outputs::entry(section, &mut cur)?;
                }
            }
        }
        if cur.at != bytes.len() {
            return Err(RecordError::Trailing { at: cur.at });
        }
        Ok(Outputs4Ref { bytes, out_dir, starts })
    }

    /// The `OUT_DIR` the script ran with.
    pub fn out_dir(&self) -> &'b str {
        self.out_dir
    }

    /// [`outputs::OutputsRef::crate_digest`] for this layout: the `OUT_DIR`, the cfgs, the env
    /// pairs and the files section as kept (a blob by its content id and length, which name
    /// its bytes), under [`CRATE_VIEW4`].
    pub fn crate_digest(&self) -> blaze_ids::ContentId {
        let mut hasher = blaze_ids::Hasher::new(CRATE_VIEW4);
        hasher.part(self.out_dir.as_bytes());
        for section in [0, 2, FILES] {
            let (start, n) = self.starts[section];
            // A section's bytes run to the next section's count.
            let end = self.starts[section + 1].0 - 4;
            hasher.update(&n.to_le_bytes()).part(&self.bytes[start..end]);
        }
        hasher.finish()
    }

    /// The entries of a section other than the files, which `open` checked.
    fn entries(&self, section: usize) -> impl Iterator<Item = Entry<'b>> + use<'b> {
        let (at, n) = self.starts[section];
        let mut cur = Cursor { bytes: self.bytes, at };
        (0..n).map_while(move |_| outputs::entry(section, &mut cur).ok())
    }

    fn texts(&self, section: usize) -> impl Iterator<Item = &'b str> + use<'b> {
        self.entries(section).filter_map(|e| if let Entry::Text(t) = e { Some(t) } else { None })
    }

    fn pairs(&self, section: usize) -> impl Iterator<Item = (&'b str, &'b str)> + use<'b> {
        self.entries(section).filter_map(|e| if let Entry::Pair(a, b) = e { Some((a, b)) } else { None })
    }

    pub fn cfgs(&self) -> impl Iterator<Item = (&'b str, Option<&'b str>)> + use<'b> {
        self.entries(0).filter_map(|e| if let Entry::Cfg(n, v) = e { Some((n, v)) } else { None })
    }

    pub fn check_cfgs(&self) -> impl Iterator<Item = &'b str> + use<'b> {
        self.texts(1)
    }

    pub fn env(&self) -> impl Iterator<Item = (&'b str, &'b str)> + use<'b> {
        self.pairs(2)
    }

    pub fn link_libs(&self) -> impl Iterator<Item = &'b str> + use<'b> {
        self.texts(3)
    }

    pub fn link_search(&self) -> impl Iterator<Item = (SearchKind, &'b str)> + use<'b> {
        self.entries(4).filter_map(|e| if let Entry::Search(k, p) = e { Some((k, p)) } else { None })
    }

    /// (target code, bin, arg).
    pub fn link_args(&self) -> impl Iterator<Item = (u8, &'b str, &'b str)> + use<'b> {
        self.entries(5).filter_map(|e| if let Entry::Arg(t, b, a) = e { Some((t, b, a)) } else { None })
    }

    pub fn metadata(&self) -> impl Iterator<Item = (&'b str, &'b str)> + use<'b> {
        self.pairs(6)
    }

    pub fn warnings(&self) -> impl Iterator<Item = &'b str> + use<'b> {
        self.texts(7)
    }

    pub fn errors(&self) -> impl Iterator<Item = &'b str> + use<'b> {
        self.texts(8)
    }

    pub fn notes(&self) -> impl Iterator<Item = (u32, NoteKind, &'b str)> + use<'b> {
        self.entries(9).filter_map(|e| if let Entry::Note(l, k, t) = e { Some((l, k, t)) } else { None })
    }

    /// (name, file), sorted by name.
    pub fn files(&self) -> impl Iterator<Item = (&'b str, FileRef<'b>)> + use<'b> {
        let (at, n) = self.starts[FILES];
        let mut cur = Cursor { bytes: self.bytes, at };
        (0..n).map_while(move |_| file_entry(&mut cur).ok())
    }

    /// The deferred C requests, each argv joined by NUL.
    pub fn c_requests(&self) -> impl Iterator<Item = &'b str> + use<'b> {
        self.texts(11)
    }

    /// The file `name` (relative to `OUT_DIR`), by a scan of the sorted names.
    pub fn file_ref(&self, name: &str) -> Option<FileRef<'b>> {
        self.files().find(|(n, _)| *n == name).map(|(_, f)| f)
    }

    /// The file a crate names by `path` when it is under the script's `OUT_DIR`: `Some` of the
    /// file, or `Some(None)` when the record has no such file; `None` when `path` is not under
    /// `OUT_DIR` at all.
    pub fn file_at_ref(&self, path: &str) -> Option<Option<FileRef<'b>>> {
        let name = path.strip_prefix(self.out_dir)?.strip_prefix('/')?;
        Some(self.file_ref(name))
    }

    /// The record owned in `alloc`, every blob read from `blobs`: [`encode4`] of it with the
    /// same blob ids is the same bytes.
    pub fn to_owned<'a>(&self, blobs: &dyn Blobs, alloc: Alloc<'a>) -> Result<Outputs<'a>, BlobError> {
        let own = |text: &str| String::from_str_in(text, alloc);
        let mut out = Outputs::new_in(self.out_dir, alloc);
        for (name, value) in self.cfgs() {
            out.cfgs.push((own(name), value.map(own)));
        }
        self.check_cfgs().for_each(|it| out.check_cfgs.push(own(it)));
        self.env().for_each(|(a, b)| out.env.push((own(a), own(b))));
        self.link_libs().for_each(|it| out.link_libs.push(own(it)));
        self.link_search().for_each(|(k, p)| out.link_search.push((k, own(p))));
        self.link_args().for_each(|(to, bin, arg)| out.link_args.push(LinkArg { to, bin: own(bin), arg: own(arg) }));
        self.metadata().for_each(|(a, b)| out.metadata.push((own(a), own(b))));
        self.warnings().for_each(|it| out.warnings.push(own(it)));
        self.errors().for_each(|it| out.errors.push(own(it)));
        self.notes().for_each(|(line, kind, key)| out.notes.push(Note { line, kind, key: own(key) }));
        for (name, file) in self.files() {
            let bytes = file.bytes(blobs)?;
            let mut owned = Vec::with_capacity_in(bytes.len(), alloc);
            owned.extend_from_slice(bytes);
            out.files.push(OutFile { name: own(name), bytes: owned });
        }
        self.c_requests().for_each(|it| out.c_requests.push(own(it)));
        Ok(out)
    }
}
