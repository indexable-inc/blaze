//! The slot: the one file a request the daemon writes for its client
//! (docs/v2/plan/design/end-state/daemon.md 4.2, "Slot" and "Leaf table" as one file a
//! request; /tmp/plan/10-no-daemon-run.md, section 1).
//!
//! `<state>/slots/<key hex>`, written by the daemon alone, replaced by rename, never read by
//! the daemon for an answer of its own. It is everything a check needs but the disk: the
//! request's base, the certificate's records as the machine read them, the stamps that vouch
//! for each read, the outputs' stamps. A client drives the request machine over it with
//! `lstat`s ([`crate::solo::Solo`]) and decides "nothing changed" with no daemon.
//!
//! It is a projection: the column's one home stays `blaze_vfs::Disk`, the certificate's the
//! store. A slot lost or torn costs one daemon answer and is written again.
//!
//! Its bytes are untrusted to the reader ([`Slot::decode`]): every length is bounded by the
//! bytes left, every flag and kind is one of its values, every text is UTF-8 with no NUL, and
//! every path is a whole absolute path with no `..` (a name of a column or a local mount a
//! relative one), so no path in a slot leads a client's look outside what it names. A slot of
//! another layout is refused by name ([`Undecoded::Layout`]), one whose check word fails is
//! [`Undecoded::Torn`]; the client then asks the daemon, which is the defined answer.
//!
//! The layout, little-endian, a count before what it counts, a text `u32 len | bytes`, a
//! [`Stamp`] its 36 bytes (`Stamp::encode`), a digest `u8 has | [32]` (zeros where it is
//! [`Digest::Nothing`]):
//!
//! ```text
//! "bzslot01" | [32] check = BLAKE3("blaze v2 request slot body 1", all that follows) | [32] key
//! | u64 written_ns | [32] base | u8 traced | [32] drawn | [32] names | u32 n + n texts (unasked)
//! | u32 n + n Premise (text path | Stamp | u64 looked_ns)
//! | text dir | [32] root | u64 looked_ns | u32 n + n (text name | Stamp)
//! | u32 n + n Local (text at | u64 looked_ns | u32 m + m (text name | Stamp))
//! | u32 n + n Outside (u8 kind | text path | digest | Stamp | u64 looked_ns)
//! | u32 n + n Placed (u8 base | text path | Stamp | u64 looked_ns | [32] content)
//! | u32 n + n Record ([32] key | u8 has | u32 len + bytes, none where has is 0)
//! | u32 len + request
//! ```
//!
//! A sans-IO core (rule 14): bytes in, bytes out.

#![blaze::sans_io]

use std::fmt;

use blaze_core::Stamp;
use blaze_ids::{ContentId, Hasher};
use blaze_record::Digest;

use crate::certificate::{ABSOLUTE, Read};

/// The first bytes of a slot: its layout's word. A change of any field's meaning or order
/// takes the next word; a slot of another is refused by it ([`Undecoded::Layout`]).
pub const MAGIC: &[u8; 8] = b"bzslot01";

/// The most bytes a reader reads of a slot: past it the file is no slot this blaze wrote
/// (tempo's, the largest measured target, is an estimated 0.5 MB).
pub const MOST_BYTES: usize = 64 << 20;

/// The first bytes of a launcher's start file (blaze-launch-host's start image).
pub const START_MAGIC: &[u8; 8] = b"bzstart1";

/// What a reader with no decoder reads of a start file: `"bzstart1" | [32] check | [32]
/// program | [32] record`, `record` the build the file is of.
pub const START_HEAD: usize = 104;

/// The domain of a slot's check word.
pub(crate) const CHECK: &str = "blaze v2 request slot body 1";

/// A file whose stamp vouches for what the slot holds as a whole: `[0]` the engine's
/// executable, then the library's `DIGEST`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Premise {
    pub path: String,
    pub stamp: Stamp,
    pub looked_ns: u64,
}

/// The checkout's column: each name under `dir` with its stamp, all looked at by
/// `looked_ns`, which vouch together for the view's root `root`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Column {
    /// The checkout's directory, absolute.
    pub dir: String,
    pub root: ContentId,
    pub looked_ns: u64,
    /// In name order, from `dir`, relative (`.` is `dir` itself).
    pub names: Vec<(String, Stamp)>,
}

/// A path dependency's column, as [`Column`] is the checkout's.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Local {
    /// Its directory, absolute.
    pub at: String,
    pub looked_ns: u64,
    pub names: Vec<(String, Stamp)>,
}

/// A read outside every view the certificate holds, with the digest it was valued at and the
/// stamp of its path that vouches for it: [`Read::Outside`], [`Read::Entry`] or
/// [`Read::Listing`] only; `Stamp::GONE` where nothing was there.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Outside {
    pub read: Read,
    pub digest: Digest,
    pub stamp: Stamp,
    pub looked_ns: u64,
}

/// A file output with the stamp its content was placed or hashed under, and when that stamp
/// was looked at: what a client's evidence is seeded from (`Evidence::seeded`). Its base is
/// [`ABSOLUTE`] (a slot holding another is refused) and its path absolute.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Placed {
    pub base: u8,
    pub path: String,
    pub stamp: Stamp,
    pub looked_ns: u64,
    pub content: ContentId,
}

/// One request's slot ([`key_of`] its key).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Slot {
    pub key: ContentId,
    /// When the daemon wrote it, by the file system's clock: which slot is the oldest.
    pub written_ns: u64,
    /// The request's certificate base (`certificate::base`).
    pub base: ContentId,
    pub traced: bool,
    /// The digest of how the request's client is shown diagnostics (`Read::Drawn`).
    pub drawn: ContentId,
    /// The names of the variables set in the request's environment ([`names_of`]).
    pub names: ContentId,
    /// The variables set that a build reads unasked (the daemon's predicate), sorted.
    pub unasked: Vec<String>,
    pub premises: Vec<Premise>,
    pub column: Column,
    pub locals: Vec<Local>,
    pub outside: Vec<Outside>,
    pub placed: Vec<Placed>,
    /// Every get the machine made of the store, in order, with what the store held.
    pub records: Vec<(ContentId, Option<Vec<u8>>)>,
    /// The frame of the request the daemon answered, environment and all.
    pub request: Vec<u8>,
}

/// Why a slot's bytes are not read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Undecoded {
    /// Its first eight bytes are not this layout's word ([`MAGIC`]).
    Layout { found: [u8; 8] },
    /// Its check word is not of the bytes that follow it: torn by a crash, or written over.
    Torn,
    /// It ends, or a field of it is not one of its values, at this byte.
    Cut { at: usize },
    /// It is the slot of another request.
    Key { wanted: ContentId, found: ContentId },
}

impl fmt::Display for Undecoded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Undecoded::Layout { found } => write!(
                f,
                "the request slot is of the layout {:?}; this blaze reads {:?}",
                String::from_utf8_lossy(found),
                String::from_utf8_lossy(MAGIC),
            ),
            Undecoded::Torn => f.write_str("the request slot's check word is not of its bytes: it is torn"),
            Undecoded::Cut { at } => write!(f, "the request slot's bytes end or go on wrongly at byte {at}"),
            Undecoded::Key { wanted, found } => {
                write!(f, "the request slot is of the request {found:x}, not of {wanted:x}")
            }
        }
    }
}

/// Whether `path` is a whole absolute path with no NUL and no `..`: what a slot names a file
/// to look at by.
pub fn absolute(path: &str) -> bool {
    path.starts_with('/') && !path.contains('\0') && !path.split('/').any(|it| it == "..")
}

/// Whether `name` is a path relative to a column's directory that stays below it: `.`, or
/// names with no NUL, no empty part and no `.` or `..` part.
fn relative(name: &str) -> bool {
    name == "."
        || (!name.is_empty()
            && !name.contains('\0')
            && name.split('/').all(|it| !it.is_empty() && it != "." && it != ".."))
}

/// A variable's name as an environment holds it.
fn variable(name: &str) -> bool {
    !name.is_empty() && !name.contains(['\0', '='])
}

fn put_u32(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&u32::try_from(n).expect("a slot's list is shorter than 2^32").to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, n: u64) {
    out.extend_from_slice(&n.to_le_bytes());
}

fn put_text(out: &mut Vec<u8>, text: &str) {
    put_u32(out, text.len());
    out.extend_from_slice(text.as_bytes());
}

fn put_names(out: &mut Vec<u8>, names: &[(String, Stamp)]) {
    put_u32(out, names.len());
    for (name, stamp) in names {
        put_text(out, name);
        stamp.encode(out);
    }
}

/// A reader of a slot's bytes that knows where it is: every refusal names the byte.
struct Bytes<'a> {
    all: &'a [u8],
    at: usize,
}

impl<'a> Bytes<'a> {
    fn cut(&self) -> Undecoded {
        Undecoded::Cut { at: self.at }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Undecoded> {
        let end = self.at.checked_add(n).filter(|end| *end <= self.all.len()).ok_or(self.cut())?;
        let taken = &self.all[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn u8(&mut self) -> Result<u8, Undecoded> {
        Ok(self.take(1)?[0])
    }

    fn flag(&mut self) -> Result<bool, Undecoded> {
        let at = self.at;
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Undecoded::Cut { at }),
        }
    }

    fn u32(&mut self) -> Result<u32, Undecoded> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, Undecoded> {
        let mut word = [0; 8];
        word.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(word))
    }

    /// A count of items of at least `least` bytes each, refused where the bytes left cannot
    /// hold that many: no count read off a slot reserves more than the slot holds.
    fn count(&mut self, least: usize) -> Result<usize, Undecoded> {
        let at = self.at;
        let n = self.u32()? as usize;
        if n.saturating_mul(least.max(1)) > self.all.len() - self.at {
            return Err(Undecoded::Cut { at });
        }
        Ok(n)
    }

    fn id(&mut self) -> Result<ContentId, Undecoded> {
        let mut id = [0; 32];
        id.copy_from_slice(self.take(32)?);
        Ok(ContentId(id))
    }

    fn bytes(&mut self) -> Result<&'a [u8], Undecoded> {
        let n = self.count(1)?;
        self.take(n)
    }

    /// A text that `ok` holds of.
    fn text(&mut self, ok: fn(&str) -> bool) -> Result<String, Undecoded> {
        let at = self.at;
        let bytes = self.bytes()?;
        match std::str::from_utf8(bytes) {
            // alloc: a slot's own string, once a slot read
            Ok(text) if ok(text) => Ok(text.to_owned()),
            _ => Err(Undecoded::Cut { at }),
        }
    }

    fn stamp(&mut self) -> Result<Stamp, Undecoded> {
        let at = self.at;
        Stamp::decode(self.take(Stamp::BYTES)?).ok_or(Undecoded::Cut { at })
    }

    fn digest(&mut self) -> Result<Digest, Undecoded> {
        let at = self.at;
        let has = self.flag()?;
        let id = self.id()?;
        match has {
            true => Ok(Digest::Is(id)),
            false if id.0 == [0; 32] => Ok(Digest::Nothing),
            false => Err(Undecoded::Cut { at }),
        }
    }

    fn names(&mut self) -> Result<Vec<(String, Stamp)>, Undecoded> {
        let n = self.count(4 + 1 + Stamp::BYTES)?;
        let mut names = Vec::with_capacity(n); // alloc: the slot's own, once a slot read
        for _ in 0..n {
            let name = self.text(relative)?;
            names.push((name, self.stamp()?));
        }
        Ok(names)
    }
}

impl Slot {
    /// Its bytes ([`Slot::decode`] reads them back). One encoding a value: equal slots are
    /// equal bytes.
    pub fn encode(&self) -> Vec<u8> {
        let records: usize = self.records.iter().map(|(_, it)| 37 + it.as_ref().map_or(0, Vec::len)).sum();
        let names = self.column.names.len() * (48 + Stamp::BYTES);
        let mut out = Vec::with_capacity(256 + names + records + self.request.len()); // alloc: the slot, once a write
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&[0; 32]);
        out.extend_from_slice(&self.key.0);
        put_u64(&mut out, self.written_ns);
        out.extend_from_slice(&self.base.0);
        out.push(u8::from(self.traced));
        out.extend_from_slice(&self.drawn.0);
        out.extend_from_slice(&self.names.0);
        put_u32(&mut out, self.unasked.len());
        for name in &self.unasked {
            put_text(&mut out, name);
        }
        put_u32(&mut out, self.premises.len());
        for Premise { path, stamp, looked_ns } in &self.premises {
            put_text(&mut out, path);
            stamp.encode(&mut out);
            put_u64(&mut out, *looked_ns);
        }
        let Column { dir, root, looked_ns, names } = &self.column;
        put_text(&mut out, dir);
        out.extend_from_slice(&root.0);
        put_u64(&mut out, *looked_ns);
        put_names(&mut out, names);
        put_u32(&mut out, self.locals.len());
        for Local { at, looked_ns, names } in &self.locals {
            put_text(&mut out, at);
            put_u64(&mut out, *looked_ns);
            put_names(&mut out, names);
        }
        put_u32(&mut out, self.outside.len());
        for Outside { read, digest, stamp, looked_ns } in &self.outside {
            let (kind, path) = read.parts();
            out.push(kind);
            put_text(&mut out, path);
            match digest {
                Digest::Is(id) => {
                    out.push(1);
                    out.extend_from_slice(&id.0);
                }
                Digest::Nothing => {
                    out.push(0);
                    out.extend_from_slice(&[0; 32]);
                }
            }
            stamp.encode(&mut out);
            put_u64(&mut out, *looked_ns);
        }
        put_u32(&mut out, self.placed.len());
        for Placed { base, path, stamp, looked_ns, content } in &self.placed {
            out.push(*base);
            put_text(&mut out, path);
            stamp.encode(&mut out);
            put_u64(&mut out, *looked_ns);
            out.extend_from_slice(&content.0);
        }
        put_u32(&mut out, self.records.len());
        for (key, held) in &self.records {
            out.extend_from_slice(&key.0);
            out.push(u8::from(held.is_some()));
            let bytes = held.as_deref().unwrap_or_default();
            put_u32(&mut out, bytes.len());
            out.extend_from_slice(bytes);
        }
        put_u32(&mut out, self.request.len());
        out.extend_from_slice(&self.request);
        let check = ContentId::of(CHECK, &out[40..]);
        out[8..40].copy_from_slice(&check.0);
        out
    }

    /// The slot `bytes` are, for the request of key `key`; or why they are not read. Bytes of
    /// another layout are refused by their word, never read as an empty slot; a slot whose
    /// check word fails is [`Undecoded::Torn`]; a field that is not one of its values, or a
    /// path that could lead a look outside what it names, is [`Undecoded::Cut`] at its byte.
    pub fn decode(bytes: &[u8], key: &ContentId) -> Result<Slot, Undecoded> {
        if bytes.len() < MAGIC.len() {
            return Err(Undecoded::Cut { at: bytes.len() });
        }
        if &bytes[..8] != MAGIC {
            let mut found = [0; 8];
            found.copy_from_slice(&bytes[..8]);
            return Err(Undecoded::Layout { found });
        }
        if bytes.len() < 40 {
            return Err(Undecoded::Cut { at: bytes.len() });
        }
        if ContentId::of(CHECK, &bytes[40..]).0[..] != bytes[8..40] {
            return Err(Undecoded::Torn);
        }
        let mut r = Bytes { all: bytes, at: 40 };
        let found = r.id()?;
        if found != *key {
            return Err(Undecoded::Key { wanted: *key, found });
        }
        let written_ns = r.u64()?;
        let base = r.id()?;
        let traced = r.flag()?;
        let drawn = r.id()?;
        let names = r.id()?;
        let n = r.count(5)?;
        let mut unasked = Vec::with_capacity(n); // alloc: the slot's own, once a slot read
        for _ in 0..n {
            unasked.push(r.text(variable)?);
        }
        let n = r.count(4 + 1 + Stamp::BYTES + 8)?;
        let mut premises = Vec::with_capacity(n); // alloc: as above
        for _ in 0..n {
            let path = r.text(absolute)?;
            let stamp = r.stamp()?;
            premises.push(Premise { path, stamp, looked_ns: r.u64()? });
        }
        let dir = r.text(absolute)?;
        let root = r.id()?;
        let looked_ns = r.u64()?;
        let column = Column { dir, root, looked_ns, names: r.names()? };
        let n = r.count(4 + 1 + 8 + 4)?;
        let mut locals = Vec::with_capacity(n); // alloc: as above
        for _ in 0..n {
            let at = r.text(absolute)?;
            let looked_ns = r.u64()?;
            locals.push(Local { at, looked_ns, names: r.names()? });
        }
        let n = r.count(1 + 4 + 1 + 33 + Stamp::BYTES + 8)?;
        let mut outside = Vec::with_capacity(n); // alloc: as above
        for _ in 0..n {
            let at = r.at;
            let kind = r.u8()?;
            let path = r.text(absolute)?;
            let read = match kind {
                2 => Read::Outside(path),
                3 => Read::Listing(path),
                7 => Read::Entry(path),
                _ => return Err(Undecoded::Cut { at }),
            };
            let digest = r.digest()?;
            let stamp = r.stamp()?;
            outside.push(Outside { read, digest, stamp, looked_ns: r.u64()? });
        }
        let n = r.count(1 + 4 + 1 + Stamp::BYTES + 8 + 32)?;
        let mut placed = Vec::with_capacity(n); // alloc: as above
        for _ in 0..n {
            let at = r.at;
            let base = r.u8()?;
            if base != ABSOLUTE {
                return Err(Undecoded::Cut { at });
            }
            let path = r.text(absolute)?;
            let stamp = r.stamp()?;
            let looked_ns = r.u64()?;
            placed.push(Placed { base, path, stamp, looked_ns, content: r.id()? });
        }
        let n = r.count(37)?;
        let mut records = Vec::with_capacity(n); // alloc: as above
        for _ in 0..n {
            let key = r.id()?;
            let has = r.flag()?;
            let at = r.at;
            let bytes = r.bytes()?;
            let held = match has {
                true => Some(bytes.to_vec()), // alloc: as above
                false if bytes.is_empty() => None,
                false => return Err(Undecoded::Cut { at }),
            };
            records.push((key, held));
        }
        let request = r.bytes()?.to_vec(); // alloc: as above
        if r.at != bytes.len() {
            return Err(r.cut());
        }
        Ok(Slot {
            key: found,
            written_ns,
            base,
            traced,
            drawn,
            names,
            unasked,
            premises,
            column,
            locals,
            outside,
            placed,
            records,
            request,
        })
    }
}

/// The key of a request's slot: `Hasher("blaze v2 request slot 1")` over `word` (`build` or
/// `check`), the view's root `root` (each length first), then the CBOR of `targets` with its
/// cargo environment emptied (whose names are [`names_of`] and whose values a certificate
/// reads by name). So two requests that differ in anything else they say, the config files
/// outside the checkout cargo reads (`cargo.files`, by bytes) among it, have two keys. `None`:
/// a request with no cargo command line (an agent's), which has no slot.
pub fn key_of(word: &str, targets: &blaze_api::Targets, root: &str) -> Option<ContentId> {
    targets.cargo.as_ref()?;
    // clone: the request's targets less their environment, once a run
    let mut keyed = targets.clone();
    if let Some(cargo) = &mut keyed.cargo {
        cargo.env = Vec::new(); // alloc: empty
    }
    if keyed_planted() {
        if let Some(cargo) = &mut keyed.cargo {
            cargo.files = Vec::new(); // alloc: empty
        }
    }
    // alloc: the targets' encoding, once a run
    let cbor = blaze_protocol::cbor::to_vec(&keyed).expect("a request's targets are plain data, which CBOR encodes");
    Some(Hasher::new("blaze v2 request slot 1").part(word.as_bytes()).part(root.as_bytes()).part(&cbor).finish())
}

/// The names set in `env`, each once, whatever their order or values:
/// `Hasher("blaze v2 request slot names 1")` over each name sorted, length first.
pub fn names_of(env: &[(String, String)]) -> ContentId {
    let mut names: Vec<&str> = env.iter().map(|(name, _)| name.as_str()).collect(); // alloc: once a run, a name a variable
    names.sort_unstable();
    names.dedup();
    let mut hasher = Hasher::new("blaze v2 request slot names 1");
    for name in names {
        hasher.part(name.as_bytes());
    }
    hasher.finish()
}

/// The start file of the program at `program`: its launcher's start image, beside it.
pub fn start_file(program: &str) -> String {
    format!("{program}.blaze-start") // alloc: a path, once an output looked at
}

/// The planted bug of the key's control: [`key_of`] leaves out the config files outside the
/// checkout (`cargo.files`), so two requests that read two of them have one key.
#[cfg(any(test, feature = "plants"))]
thread_local! {
    pub static KEY_DROPS_FILES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(any(test, feature = "plants"))]
fn keyed_planted() -> bool {
    KEY_DROPS_FILES.get()
}

#[cfg(not(any(test, feature = "plants")))]
fn keyed_planted() -> bool {
    false
}

