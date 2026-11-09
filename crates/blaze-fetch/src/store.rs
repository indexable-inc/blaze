//! Objects and the side index, both in blaze's store ([`blaze_cas`]), so both are persisted,
//! verified on every read and made durable by the store's own commit; nothing here keeps a
//! file of its own.
//!
//! - An **object** is some bytes, filed under the key of their plain BLAKE3 alone
//!   ([`Object::key`]): whatever made them, two equal byte strings are one object. Its value is
//!   the bytes themselves when they fit in one chunk ([`CHUNK_LEN`]), else a list of chunks,
//!   each a value of its own under the key of its BLAKE3, so no object is ever held whole in
//!   memory to be put or read. Every read hashes what it gives and compares the object's
//!   BLAKE3 ([`Objects::read`]): the store's own check covers a value against the hash it was
//!   put with, this one covers the bytes against the name they are filed under.
//! - The **index** maps an [`Origin`] to the object it gave: a pin (`sha256:…` and the rest)
//!   with the form asked for to the object whose bytes have that digest, and, for the fetch
//!   graph, a pure step over its inputs to its output. Each record is a value of its own under
//!   the origin's key: 41 bytes, the object's form, BLAKE3 and length. Two objects for one
//!   origin is the store's [`blaze_cas::Mismatch`], a typed refusal, never an overwrite
//!   (AGENTS.md rule 21).
//! - Only this crate files: `check_and_file` hashes what a producer made itself, and the
//!   caller never hands in a hash to be believed. Outside it, [`Objects::keep_pinned`] and
//!   [`Objects::keep_step`] are the ways in.
//!
//! Bazel's repository cache files a download under `content_addressable/sha256/<hex>` and so
//! knows one algorithm; here the bytes are filed once by BLAKE3 and every algorithm a pin is
//! in is an index record onto them. A record is put only after its object (its chunks before
//! it, a tree's files before its listing), by the same writer, so a commit never makes one
//! durable without the other; a crash before the commit loses both, and the next fetch
//! downloads again. A record whose object, chunk or tree file the store does not have (lost,
//! or dropped by a later collector of store values: the store has none today) is a miss
//! ([`Objects::lookup`]), so the fetch makes it again. A lookup asks only whether each value
//! is there ([`Handle::has`]) and reads no file: every read verifies what it gives.

use std::{io::Read as _, path::Path};

use blaze_cas::{Handle, Key, Kind};
use blaze_ids::{ContentId, Hasher};

use crate::{
    decisions::{Verdict, verdict},
    digest::{Blake3, Digest, Hashing},
    error::{FetchError, Producer},
    graph::Tree,
    tree::{self, Entry},
};

/// The schema of an object: its bytes inline, or the list of its chunks. [`CHUNK_LEN`] is in
/// it, as which objects are chunked and where their chunks begin depend on it.
const OBJECT: blaze_format::SchemaHash = blaze_format::schema(
    "blaze-fetch object: bytes, or chunks of 4194304 bytes, by their BLAKE3 3",
);
const _: () = assert!(
    CHUNK_LEN == 4_194_304,
    "CHUNK_LEN is written in OBJECT's schema: change both together"
);
/// The schema of a chunk: a piece of an object, under the key of its own BLAKE3.
const CHUNK: blaze_format::SchemaHash =
    blaze_format::schema("blaze-fetch chunk: a piece of an object by its BLAKE3 1");
/// The schema of an index record: 41 bytes, an object's form, BLAKE3 and length.
const RECORD: blaze_format::SchemaHash =
    blaze_format::schema("blaze-fetch index record: form, BLAKE3, length 2");
const RECORD_LEN: usize = 41;

/// The most bytes of an object held at once, to put or to read: an object longer than this
/// is kept as chunks of it.
pub const CHUNK_LEN: usize = 4 << 20;

/// Whether an object of `len` bytes is kept as chunks.
fn chunked(len: u64) -> bool {
    len > CHUNK_LEN as u64
}

/// An object's value: the tag, then the bytes.
pub(crate) const INLINE: u8 = 0;
/// An object's value: the tag, then each chunk's BLAKE3 (32 bytes) and length (8, little
/// endian), in order.
pub(crate) const CHUNKED: u8 = 1;
const CHUNK_ENTRY: usize = 40;

fn producer() -> ContentId {
    ContentId::of("blaze-fetch 2", &[])
}

/// What an object's bytes are.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Form {
    /// The bytes themselves.
    Blob,
    /// A tree's listing ([`crate::tree`]); each listed file an object of its own.
    Tree,
}

impl Form {
    fn code(self) -> u8 {
        match self {
            Form::Blob => 0,
            Form::Tree => 1,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Form::Blob => "file",
            Form::Tree => "tree",
        }
    }
}

/// Bytes in the store: what [`crate::Fetcher::get`] gives, and what a later step of a fetch
/// graph takes as its input (by [`Object::id`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Object {
    pub blake3: Blake3,
    pub len: u64,
    pub form: Form,
}

impl Object {
    /// The store's key of its bytes (or of the list of its chunks).
    pub fn key(&self) -> Key {
        object_key(self.blake3)
    }

    /// As an input of a step ([`Origin::Step`]): its BLAKE3 and its form.
    pub fn id(&self) -> ContentId {
        let mut hasher = Hasher::new("blaze-fetch object as an input 1");
        hasher.update(&[self.form.code()]).id(&self.blake3.id());
        hasher.finish()
    }

    fn record(&self) -> [u8; RECORD_LEN] {
        let mut out = [0; RECORD_LEN];
        out[0] = self.form.code();
        out[1..33].copy_from_slice(&self.blake3.0);
        out[33..].copy_from_slice(&self.len.to_le_bytes());
        out
    }

    fn from_record(bytes: &[u8]) -> Option<Object> {
        if bytes.len() != RECORD_LEN {
            return None;
        }
        let form = match bytes[0] {
            0 => Form::Blob,
            1 => Form::Tree,
            _ => return None,
        };
        let blake3 = Blake3(bytes[1..33].try_into().ok()?);
        let len = u64::from_le_bytes(bytes[33..].try_into().ok()?);
        Some(Object { blake3, len, form })
    }
}

pub(crate) fn object_key(blake3: Blake3) -> Key {
    Key::of(OBJECT, producer(), &[blake3.id()])
}

pub(crate) fn chunk_key(blake3: Blake3) -> Key {
    Key::of(CHUNK, producer(), &[blake3.id()])
}

/// What an index record is of.
#[derive(Clone, Copy, Debug)]
pub enum Origin<'a> {
    /// Bytes whose digest is `pin`, as `form`: a file's bytes and a tree's listing pinned by
    /// the same digest are two records, so a URL's body never answers a git source's pin.
    Pinned { pin: &'a Digest, form: Form },
    /// The output of a pure step: `code` the step's code identity (the kernel's per-step
    /// code ID), `inputs` each input object's [`Object::id`], in order.
    Step { code: ContentId, inputs: &'a [ContentId] },
}

impl Origin<'_> {
    /// The store's key of its record; also what the single-flight table keys a fetch by
    /// ([`crate::flight`]).
    pub fn key(&self) -> Key {
        let mut hasher = Hasher::new("blaze-fetch origin 3");
        // The tree format a tree's listing is written in: a step keyed by its code and inputs
        // alone would, after a format change, make other listing bytes under the same key (the
        // store's `Mismatch`, for good). A pinned tree's pin is over its listing, so it changes
        // with the format anyway; every origin takes it, for one rule.
        hasher.part(crate::tree::HEADER.as_bytes());
        match self {
            Origin::Pinned { pin, form } => {
                hasher.update(&[0, form.code(), pin.algo().code()]).part(pin.bytes());
            }
            Origin::Step { code, inputs } => {
                hasher.update(&[1]).id(code).update(&(inputs.len() as u64).to_le_bytes());
                for input in *inputs {
                    hasher.id(input);
                }
            }
        }
        Key::of(RECORD, producer(), &[hasher.finish()])
    }

    fn pin(&self) -> Option<Digest> {
        match self {
            Origin::Pinned { pin, .. } => Some(**pin),
            Origin::Step { .. } => None,
        }
    }
}

/// Where a producer's bytes are, to be checked and filed.
#[derive(Clone, Copy)]
pub enum Made<'b> {
    /// A file's bytes, in memory.
    Bytes(&'b [u8]),
    /// A file's bytes, in a file of the producer's own (a download's temporary file), read a
    /// chunk at a time and never whole.
    File(&'b Path),
    /// A tree: its files are filed each as an object, and its listing as the tree's.
    Tree(&'b Tree),
}

impl Made<'_> {
    pub fn form(&self) -> Form {
        match self {
            Made::Bytes(_) | Made::File(_) => Form::Blob,
            Made::Tree(_) => Form::Tree,
        }
    }
}

/// What a producer made, to be checked against a pin and kept: the one place a pin is checked
/// and an object filed, which [`crate::Fetcher`] and a fetch graph's last step share.
pub struct Produced<'b> {
    pub made: Made<'b>,
    pub from: Producer,
}

/// A blob's bytes as filing reads them.
#[derive(Clone, Copy)]
enum Bytes<'b> {
    Slice(&'b [u8]),
    File(&'b Path),
}

impl Bytes<'_> {
    fn io(&self, what: &str) -> impl FnOnce(std::io::Error) -> FetchError {
        let what = match self {
            Bytes::Slice(_) => what.to_owned(), // alloc: error path
            Bytes::File(path) => format!("{what} {}", path.display()), // alloc: error path
        };
        move |error| FetchError::Io { what, error }
    }

    fn len(&self) -> Result<u64, FetchError> {
        match self {
            Bytes::Slice(bytes) => Ok(bytes.len() as u64),
            Bytes::File(path) => {
                Ok(std::fs::metadata(path).map_err(self.io("reading the length of"))?.len())
            }
        }
    }

    /// Each piece of at most [`CHUNK_LEN`] bytes, in order; every piece but the last is
    /// exactly [`CHUNK_LEN`].
    fn each(
        &self,
        mut piece: impl FnMut(&[u8]) -> Result<(), FetchError>,
    ) -> Result<(), FetchError> {
        match self {
            Bytes::Slice(bytes) => bytes.chunks(CHUNK_LEN).try_for_each(piece),
            Bytes::File(path) => {
                let mut file = std::fs::File::open(path).map_err(self.io("opening"))?;
                let mut buffer = vec![0; CHUNK_LEN]; // alloc: one chunk, once a pass over a file
                loop {
                    let mut filled = 0;
                    while filled < CHUNK_LEN {
                        match file.read(&mut buffer[filled..]) {
                            Ok(0) => break,
                            Ok(n) => filled += n,
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(e) => return Err(self.io("reading")(e)),
                        }
                    }
                    if filled > 0 {
                        piece(&buffer[..filled])?;
                    }
                    if filled < CHUNK_LEN {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Objects and the index over one store.
#[derive(Clone, Copy)]
pub struct Objects<'s> {
    store: &'s Handle,
}

impl<'s> Objects<'s> {
    pub fn new(store: &'s Handle) -> Objects<'s> {
        Objects { store }
    }

    /// The store this files into, by its address: what the single-flight table tells two
    /// stores of one process apart by. Only ever compared in the process, never written out.
    pub(crate) fn store_id(&self) -> usize {
        std::ptr::from_ref(self.store).addr()
    }

    /// The object `origin` gave, when the index has it and the store still has all of it: its
    /// value, each of its chunks and, for a tree, each listed file. A record with anything of
    /// its object gone is a miss, so the caller makes the object again and puts the same
    /// record, which the store takes as already kept.
    pub fn lookup(&self, origin: Origin<'_>) -> Result<Option<Object>, FetchError> {
        let failed = |what: String| FetchError::Store { pin: origin.pin(), what };
        let record = self
            .store
            .read(&origin.key(), |entry| Object::from_record(entry.bytes))
            .map_err(|e| failed(format!("its index record failed its check: {e:?}")))?; // alloc: error path
        let object = match record {
            None => return Ok(None),
            Some(None) => {
                return Err(failed(
                    "its index record is not 41 bytes of a form, a BLAKE3 and a length".to_owned(),
                ));
            } // alloc: error path
            Some(Some(object)) => object,
        };
        Ok(self.whole(&object)?.then_some(object))
    }

    /// Whether the store has every part of `object`, by presence alone: a blob's value and,
    /// when it is chunked, each chunk; a tree's listing, read and verified (it is a line a
    /// file), and each listed file the same way, by the length the listing gives it.
    fn whole(&self, object: &Object) -> Result<bool, FetchError> {
        if !self.blob_whole(object.blake3, object.len)? {
            return Ok(false);
        }
        if object.form == Form::Blob {
            return Ok(true);
        }
        let listing = self.read(object)?;
        let entries = tree::parse(&listing).map_err(|refused| FetchError::Tree {
            from: Producer::Given(object.blake3.to_string()),
            refused,
        })?; // alloc: error path
        for entry in &entries {
            if !self.blob_whole(entry.blake3, entry.len)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether the store has the `len` bytes filed under `blake3`: their value, and, for an
    /// object kept as chunks, every chunk its list names (the list is the one value read).
    fn blob_whole(&self, blake3: Blake3, len: u64) -> Result<bool, FetchError> {
        let key = object_key(blake3);
        if !chunked(len) {
            return Ok(self.store.has(&key));
        }
        let store = self.store;
        let whole = store
            .read(&key, |entry| -> Result<bool, ()> {
                match entry.bytes.split_first() {
                    Some((&CHUNKED, list)) => {
                        Ok(chunk_list(list)?.all(|(chunk, _)| store.has(&chunk_key(chunk))))
                    }
                    _ => Err(()),
                }
            })
            .map_err(|e| FetchError::Store {
                pin: None,
                what: format!("{blake3} failed its check: {e:?}"),
            })?; // alloc: error path
        match whole {
            None => Ok(false),
            Some(Err(())) => Err(not_an_object(blake3)),
            Some(Ok(whole)) => Ok(whole),
        }
    }

    /// The bytes of `object`, hashed as they are read and compared with its BLAKE3 and length.
    pub fn read(&self, object: &Object) -> Result<Vec<u8>, FetchError> {
        let mut out = Vec::with_capacity(object.len.min(CHUNK_LEN as u64) as usize); // alloc: the bytes the caller asked for
        let len = self.read_into(object.blake3, &mut |piece| {
            out.extend_from_slice(piece);
            Ok(())
        })?;
        if len != object.len {
            return Err(FetchError::Length {
                object: object.blake3,
                expected: object.len,
                actual: len,
            });
        }
        Ok(out)
    }

    /// The bytes filed under `blake3`, a piece at a time into `out`, hashed as they go and
    /// compared with `blake3` at the end: their length, which a caller that knows the length
    /// (the object's, or a tree entry's in the listing) compares too. `out` may have been given bytes of a failed read; its caller throws them
    /// away (a materialize's temporary file).
    pub fn read_into(
        &self,
        blake3: Blake3,
        out: &mut dyn FnMut(&[u8]) -> Result<(), FetchError>,
    ) -> Result<u64, FetchError> {
        let failed = |what: String| FetchError::Store { pin: None, what };
        let check = |e: blaze_cas::Corrupt| failed(format!("{blake3} failed its check: {e:?}")); // alloc: error path
        let value = self
            .store
            .pinned(&object_key(blake3))
            .map_err(check)?
            .ok_or_else(|| failed(format!("it has no {blake3}")))?; // alloc: error path
        let mut hashing = blaze_ids::b3::Hasher::new();
        let mut len = 0u64;
        match value.split_first() {
            Some((&INLINE, bytes)) => {
                hashing.update(bytes);
                len = bytes.len() as u64;
                out(bytes)?;
            }
            Some((&CHUNKED, list)) => {
                for (chunk, chunk_len) in chunk_list(list).map_err(|()| not_an_object(blake3))? {
                    let bytes = self
                        .store
                        .pinned(&chunk_key(chunk))
                        .map_err(|e| {
                            failed(format!("{chunk}, a chunk of {blake3}, failed its check: {e:?}"))
                        })? // alloc: error path
                        .ok_or_else(|| failed(format!("it has no {chunk}, a chunk of {blake3}")))?; // alloc: error path
                    if bytes.len() as u64 != chunk_len {
                        return Err(failed(format!(
                            "{chunk}, a chunk of {blake3}, is {} bytes, and the list says {chunk_len}",
                            bytes.len()
                        ))); // alloc: error path
                    }
                    hashing.update(&bytes[..]);
                    len += chunk_len;
                    out(&bytes[..])?;
                }
            }
            _ => return Err(not_an_object(blake3)),
        }
        let got = Blake3(hashing.finalize());
        if got != blake3 {
            return Err(failed(format!(
                "the bytes filed as {blake3} are {got}: kept wrong, and not given"
            ))); // alloc: error path
        }
        Ok(len)
    }

    /// `bytes`, checked against `pin`, kept as a file object with `pin` recorded onto it, as
    /// `check_and_file` does: for bytes a caller already has (a `.crate` in cargo's cache).
    pub fn keep_pinned(
        &self,
        pin: &Digest,
        from: Producer,
        bytes: &[u8],
    ) -> Result<Object, FetchError> {
        check_and_file(*self, pin, Produced { made: Made::Bytes(bytes), from })
    }

    /// `tree`, the output of the pure step whose code identity is `code` over `inputs` (each
    /// an [`Object::id`]), kept with the step recorded onto it: a later run of the same step on
    /// the same inputs is [`Objects::lookup`] of [`Origin::Step`]. `step` names it in errors.
    pub fn keep_step(
        &self,
        code: ContentId,
        inputs: &[ContentId],
        step: &str,
        tree: &Tree,
    ) -> Result<Object, FetchError> {
        let from = Producer::Step(step.to_owned()); // alloc: once a step
        let listing = listing_of(tree, &from)?;
        self.put_files(tree)?;
        let object = self.file(Bytes::Slice(&listing), Blake3::of(&listing), Form::Tree)?;
        self.record(&object, Origin::Step { code, inputs })?;
        Ok(object)
    }

    /// Puts `bytes` as an object of `form` whose BLAKE3 is `blake3`, which this crate hashed
    /// from them in an earlier pass: the pass that puts them hashes them again, and a file that
    /// changed in between is refused before the object is.
    fn file(&self, bytes: Bytes<'_>, blake3: Blake3, form: Form) -> Result<Object, FetchError> {
        let len = bytes.len()?;
        let mut whole = blaze_ids::b3::Hasher::new();
        let mut put = 0u64;
        let value = if !chunked(len) {
            let mut value = Vec::with_capacity(1 + len as usize); // alloc: one chunk at most, once an object
            value.push(INLINE);
            bytes.each(|piece| {
                whole.update(piece);
                put += piece.len() as u64;
                value.extend_from_slice(piece);
                Ok(())
            })?;
            value
        } else {
            let mut value =
                Vec::with_capacity(1 + CHUNK_ENTRY * len.div_ceil(CHUNK_LEN as u64) as usize); // alloc: the list, once an object
            value.push(CHUNKED);
            bytes.each(|piece| {
                whole.update(piece);
                put += piece.len() as u64;
                let chunk = Blake3::of(piece);
                self.store.put(chunk_key(chunk), Kind::Output, piece).map_err(|it| FetchError::Store {
                    pin: None,
                    what: format!("it keeps other bytes ({:x}) under the key of {chunk}, a chunk of {blake3}", it.kept), // alloc: error path
                })?;
                value.extend_from_slice(&chunk.0);
                value.extend_from_slice(&(piece.len() as u64).to_le_bytes());
                Ok(())
            })?;
            value
        };
        let got = Blake3(whole.finalize());
        if got != blake3 || put != len {
            return Err(bytes.io("filing")(std::io::Error::other(format!(
                // alloc: error path
                "its bytes changed while they were filed: hashed as {blake3}, then read as {got}"
            ))));
        }
        self.store.put(object_key(blake3), Kind::Output, &value).map_err(|it| {
            FetchError::Store {
                pin: None,
                what: format!("it keeps another value ({:x}) under the key of {blake3}", it.kept), // alloc: error path
            }
        })?;
        Ok(Object { blake3, len, form })
    }

    /// Each file of `tree` as an object of its own, before its listing is filed.
    fn put_files(&self, tree: &Tree) -> Result<(), FetchError> {
        for file in &tree.files {
            self.file(Bytes::Slice(&file.bytes), Blake3::of(&file.bytes), Form::Blob)?;
        }
        Ok(())
    }

    /// Records `origin` onto `object`, which is kept.
    pub(crate) fn record(&self, object: &Object, origin: Origin<'_>) -> Result<(), FetchError> {
        self.store.put(origin.key(), Kind::Run, &object.record()).map(drop).map_err(|it| FetchError::Store {
            pin: origin.pin(),
            what: format!( // alloc: error path
                "its index already records another object ({:x}) for it, and this is {}: two objects for one origin",
                it.kept, object.blake3
            ),
        })
    }
}

fn not_an_object(blake3: Blake3) -> FetchError {
    FetchError::Store {
        pin: None,
        what: format!("its value under the key of {blake3} is neither bytes nor a list of chunks"),
    } // alloc: error path
}

/// A chunked object's list: each chunk's BLAKE3 and length.
fn chunk_list(list: &[u8]) -> Result<impl Iterator<Item = (Blake3, u64)> + '_, ()> {
    if list.is_empty() || !list.len().is_multiple_of(CHUNK_ENTRY) {
        return Err(());
    }
    Ok(list.chunks_exact(CHUNK_ENTRY).map(|it| {
        let blake3 = Blake3(it[..32].try_into().expect("32 bytes of a 40-byte entry"));
        (blake3, u64::from_le_bytes(it[32..].try_into().expect("8 bytes of a 40-byte entry")))
    }))
}

/// `tree`'s listing ([`crate::tree`]), its files hashed.
pub(crate) fn listing_of(tree: &Tree, from: &Producer) -> Result<Vec<u8>, FetchError> {
    let mut entries: Vec<Entry> = tree // alloc: once a tree
        .files
        .iter()
        .map(|it| Entry {
            path: it.path.clone(),
            kind: it.kind,
            blake3: Blake3::of(&it.bytes),
            len: it.bytes.len() as u64,
        }) // clone: once a file
        .collect();
    tree::listing(&mut entries).map_err(|refused| FetchError::Tree { from: from.clone(), refused }) // clone: error path
}

/// `pin`'s object, as `form`: from the index, else `produce`d, checked against `pin` and
/// filed. A mismatch keeps nothing and names the producer and both digests.
///
/// The fetch graph's short cut: a final pin the store has skips every step before it, as
/// `produce` is not called.
pub fn ensure<'b>(
    objects: Objects<'_>,
    pin: &Digest,
    form: Form,
    produce: impl FnOnce() -> Result<Produced<'b>, FetchError>,
) -> Result<Object, FetchError> {
    if let Some(object) = objects.lookup(Origin::Pinned { pin, form })? {
        return Ok(object);
    }
    let produced = produce()?;
    if produced.made.form() != form {
        return Err(FetchError::Store {
            pin: Some(*pin),
            what: format!(
                // alloc: error path
                "{} made a {} where a {} was asked for, so nothing of it was kept",
                produced.from,
                produced.made.form().name(),
                form.name()
            ),
        });
    }
    check_and_file(objects, pin, produced)
}

/// `produced` hashed here, in the pin's algorithm and BLAKE3, and checked against `pin`; on a
/// match filed with `pin` recorded onto it, on a mismatch nothing kept. No caller hands in a
/// hash: the bytes filed are the bytes hashed, read again as they are put.
pub(crate) fn check_and_file(
    objects: Objects<'_>,
    pin: &Digest,
    produced: Produced<'_>,
) -> Result<Object, FetchError> {
    let Produced { made, from } = produced;
    let listing;
    let (bytes, form) = match made {
        Made::Bytes(bytes) => (Bytes::Slice(bytes), Form::Blob),
        Made::File(path) => (Bytes::File(path), Form::Blob),
        Made::Tree(tree) => {
            listing = listing_of(tree, &from)?;
            (Bytes::Slice(&listing), Form::Tree)
        }
    };
    let mut hashing = Hashing::new(pin.algo());
    bytes.each(|piece| {
        hashing.update(piece);
        Ok(())
    })?;
    let hashed = hashing.finish();
    if let Verdict::Mismatch { expected, actual } = verdict(pin, &hashed) {
        return Err(FetchError::Mismatch { from, expected, actual });
    }
    if let Made::Tree(tree) = made {
        objects.put_files(tree)?;
    }
    let object = objects.file(bytes, hashed.blake3, form)?;
    objects.record(&object, Origin::Pinned { pin, form })?;
    Ok(object)
}
