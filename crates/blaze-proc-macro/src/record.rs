//! The expansion record: what one call gave, kept as a row of its crate's results table
//! ([`table`]): `base → read list → answer`.
//!
//! - The base is computed from text before the run ([`base_key`]): the module's wasm digest,
//!   the macro's index, name and kind, the edition, and the input's and the attribute's tokens
//!   as the macro sees them (kind, spacing and text; never a position or a context, which it
//!   cannot see).
//! - The read list names what the run read (`env:NAME`, `file:path`, `dir:path`,
//!   `stat:path`, paths from the workspace root) and the answer each had; a lookup values
//!   each now ([`crate::sandbox::answer_now`]), so a changed file or variable misses exactly
//!   the expansions that read it.
//! - The answer is the [`Produced`] in canonical bytes ([`table::row_bytes`]). Two runs of one key
//!   write equal bytes, wherever the checkout is; two answers under equal reads and digests
//!   are refused (rule 21).
//!
//! The store's folder also holds the build scripts' records, one write-once file each
//! ([`DirStore::build_scripts`], blaze-record's [`Store`]): a put never replaces bytes, and
//! other bytes under a held key are refused.

use std::path::Path;

use bstd::args::ArgList;
use bstd::cx::{Cx, CxSpawn};
use bstd::env::Vars;
use bstd::fs::{Content, Dir, Durable, FsError, FsKind, ReadWrite};
use bstd::io::{Io, Limits, NativeWorld, OpenError};
use bstd::mem::TASK_FRAME;
use core::time::Duration;
use bstd::path::Path as BPath;

use blaze_expand::{
    Ctx, Delim, Edition, LitKind, ProcCall, ProcDiagnostic, ProcKind, ProcLevel, Produced, Raw, ReadAnswer, Reads, Spacing, Text,
    TokenKind, TokenOrigin, TokenStream, Transparency,
};
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AVec, Alloc};
use blaze_record::{Kind, Store, Tally};

use crate::{
    module::Exported,
    sandbox::{Package, answer_now},
};

/// What a record file ends in: the digest of every byte before it under this domain. A file
/// whose end is not its body's seal is a crash's torn write (a publish is not synced: records
/// are derived, remade by a rerun), read as a miss and replaced by the next put.
const SEAL: &str = "blaze expansion record seal 1";
const SEAL_LEN: usize = 32;

/// How long one record read or publish may take (rule 18): a record is a small file, so a
/// minute is a hang, failed by the effect layer naming the record.
const RECORD_BUDGET: Duration = Duration::from_secs(60);

/// What a read of a record's file found ([`OnDir::read`]).
enum Held {
    /// No file under the name.
    Absent,
    /// One file's bytes, all of them.
    Whole(Vec<u8>),
    /// A longer file took the name between the read's two effects: only its start was read.
    Replaced,
}

/// The records' one home: a directory of the effect layer, a narrow `CxSpawn` capability to create
/// per-operation root contexts without holding a whole `Io`, and the machine's boot id (when known)
/// so whole-table vouchers stay valid within one boot.
pub struct OnDir {
    pub cx: CxSpawn,
    pub boot: Option<blaze_core::Boot>,
    pub dir: Dir<ReadWrite>,
}

impl OnDir {
    fn path(name: &RecordName) -> Result<BPath, FilesError> {
        BPath::new(name.as_str()).map_err(|_| FilesError::Name { name: *name })
    }

    fn cx(&self, name: &RecordName) -> Result<Cx, FilesError> {
        self.cx.cx(RECORD_BUDGET, TASK_FRAME).map_err(|_| FilesError::Stopped { name: *name })
    }

    /// The bytes at `name`, in a buffer of the file's own length (no frame, no fixed block: a
    /// file of any length is read): [`Held::Absent`] when nothing is there.
    ///
    /// Two effects: the file's length, then ONE read of that length and a byte more. One read
    /// is one file's bytes (another command's publish replaces the name, never the bytes
    /// under a read), so what comes back is a whole file whenever the read met the file's end:
    /// at the length asked, or sooner (a shorter file took the name between the two effects).
    /// A read that filled the byte more met a LONGER file put there in between and holds only
    /// its start: [`Held::Replaced`], which the caller answers by name.
    fn read(&self, name: &RecordName) -> Result<Held, FilesError> {
        let cx = self.cx(name)?;
        let file = match self.dir.open_read(&cx, &Self::path(name)?) {
            Ok(file) => file,
            Err(error) if error.kind == FsKind::NotFound => return Ok(Held::Absent),
            Err(error) => return Err(FilesError::Effect(error)),
        };
        let len = match file.len(&cx) {
            Ok(len) => usize::try_from(len).map_err(|_| FilesError::Stopped { name: *name })?,
            Err(error) if error.kind == FsKind::NotFound => return Ok(Held::Absent),
            Err(error) => return Err(FilesError::Effect(error)),
        };
        let mut bytes = vec![0u8; len + 1];
        let read = file.read_into(&mut bytes, 0).wait(&cx).map_err(FilesError::Effect)?;
        if read > len {
            return Ok(Held::Replaced);
        }
        bytes.truncate(read);
        Ok(Held::Whole(bytes))
    }

    /// `name` made to hold `whole` (a record's body with its seal at its end), replaced all
    /// at once for a reader (never a part), nothing synced: after a power cut it holds what
    /// it held, or a torn part. The slice is handed to the effect as it lies: nothing here
    /// copies it or bounds its length.
    fn publish(&self, name: &RecordName, whole: &[u8]) -> Result<(), FilesError> {
        let cx = self.cx(name)?;
        self.dir.publish(&cx, &Self::path(name)?, Content::bytes(whole), Durable::No).map(|_| ()).map_err(FilesError::Effect)
    }

    /// [`RecordPlant::FramedPublish`]'s write: `whole` copied into the task's frame, then
    /// published from there, so a record longer than the frame is refused. Never taken but by
    /// the planted bug's test.
    fn publish_framed(&self, name: &RecordName, whole: &[u8]) -> Result<(), FilesError> {
        let cx = self.cx(name)?;
        let mut framed = bstd::mem::Bytes::new_in(cx.frame(), "a record copied into the task's frame");
        framed.try_extend(whole).map_err(|_| FilesError::Stopped { name: *name })?;
        self.dir.publish(&cx, &Self::path(name)?, Content::bytes(framed.as_slice()), Durable::No).map(|_| ()).map_err(FilesError::Effect)
    }

    /// [`RecordPlant::RenameFirst`]'s write: `name` cut to nothing in place, then `whole`
    /// appended in three parts. Never taken but by the simulation's planted bug.
    fn write_in_place(&self, name: &RecordName, whole: &[u8]) -> Result<(), FilesError> {
        let cx = self.cx(name)?;
        let file = self.dir.create(&cx, &Self::path(name)?).map_err(FilesError::Effect)?;
        let (body, seal) = whole.split_at(whole.len().saturating_sub(SEAL_LEN));
        let (first, second) = body.split_at(body.len() / 2);
        let appended = file
            .append(&cx, first, Durable::No)
            .and_then(|()| file.append(&cx, second, Durable::No))
            .and_then(|()| file.append(&cx, seal, Durable::No));
        // The file is finished whether or not an append failed: the part written stays there.
        let closed = file.close(&cx, Durable::No).map(|_| ());
        appended.and(closed).map_err(FilesError::Effect)
    }
}

/// A world over the folder `store` for a program's start (an example, a test), and that
/// folder, its root. A command's driver hands its own world instead.
pub fn native_world(store: &Path) -> Result<(Io, Dir<ReadWrite>), FilesError> {
    let io = Io::native(NativeWorld {
        root: store,
        threads: 1,
        env: Vars::given(Vec::new()),
        args: ArgList::given(Vec::new()),
        host_name: std::sync::Arc::from(""),
        seed: Some(0),
        above: 0,
        limits: Limits::USUAL,
    })
    .map_err(FilesError::World)?;
    let root = io.need_fs().map_err(|_| FilesError::NoTree)?.root();
    Ok((io, root))
}

/// A record file that could not be read or written, naming it.
#[derive(Debug)]
pub enum FilesError {
    /// The world over the store's folder could not be opened (the names examples' wrapper).
    World(OpenError),
    /// The world has no file tree to hold the records.
    NoTree,
    Effect(FsError),
    /// A record's name the effect layer refuses as a path.
    Name { name: RecordName },
    /// `name` holds a whole record of other bytes than the put's (rule 21).
    OtherBytes { name: RecordName },
    /// The task's frame refused the bytes of `name`'s record.
    Stopped { name: RecordName },
}

impl std::fmt::Display for FilesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilesError::World(error) => write!(f, "the records' folder: {error}"),
            FilesError::NoTree => f.write_str("the records' world has no file tree"),
            FilesError::Effect(error) => write!(f, "a record: {error}"),
            FilesError::Name { name } => write!(f, "the record name {name} is no path under the store"),
            FilesError::Stopped { name } => write!(f, "the record {name}: the task's frame refused its bytes"),
            FilesError::OtherBytes { name } => write!(f, "the record {name} holds other bytes than its put's (rule 21)"),
        }
    }
}

/// A planted bug the crash simulation must catch (`record_tests.rs`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordPlant {
    None,
    /// A read takes a file without checking its seal: a torn record is handed back.
    SealSkipped,
    /// A crate's results table is written in place under its own name, a part at a time (the
    /// name before the bytes are whole): a failure between two parts leaves a part there with
    /// no power lost ([`table`]).
    RenameFirst,
    /// A crate's results table is copied into the task's frame before it is published: a
    /// table longer than the frame is refused ([`table`]'s hundred-megabyte test).
    FramedPublish,
}

/// The folder store of records: each file the record's bytes then their [`SEAL`], published
/// whole (a reader sees all or none) and taken back only when the seal matches.
pub struct DirStore {
    files: OnDir,
    tally: Tally,
    plant: RecordPlant,
    /// How many results tables were found torn ([`table`]): a power cut's, read as absent.
    torn: core::sync::atomic::AtomicU64,
    /// How many results tables were hashed whole: once a boot each, not once a command.
    hashed: core::sync::atomic::AtomicU64,
    /// How many results tables another command replaced by a longer one under this
    /// command's read ([`Held::Replaced`]): read as absent, their calls run again.
    replaced: core::sync::atomic::AtomicU64,
}

impl DirStore {
    /// The expansion records, `proc-macro/records` beneath the store's folder `store`.
    pub fn expansions(cx: CxSpawn, boot: Option<blaze_core::Boot>, store: &Dir<ReadWrite>) -> DirStore {
        DirStore::within(cx, boot, store, "proc-macro/records")
    }

    /// The spawn answers' records (`spawn_records::Recorded`), `build-script/records` beneath the
    /// store's folder `store`. A script run itself is kept by no store here: its only cache is
    /// the command's `Script` task record (D115).
    pub fn build_scripts(cx: CxSpawn, boot: Option<blaze_core::Boot>, store: &Dir<ReadWrite>) -> DirStore {
        DirStore::within(cx, boot, store, "build-script/records")
    }

    fn within(cx: CxSpawn, boot: Option<blaze_core::Boot>, store: &Dir<ReadWrite>, at: &'static str) -> DirStore {
        let at = BPath::new(at).expect("a records folder's name is a relative path by construction");
        DirStore::on(OnDir { cx, boot, dir: store.subdir(&at) }, RecordPlant::None)
    }

    /// The expansion records under `<store>/proc-macro/records`, over a world opened here.
    /// DEBT, for removal: kept only for the names lane's examples (blaze-real
    /// examples/names_load, blaze-world examples/names.rs), which hand no world; every other
    /// caller makes [`DirStore::expansions`] from its own.
    pub fn new(store: &Path) -> DirStore {
        match native_world(store) {
            Ok((io, root)) => DirStore::expansions(io.spawner(), io.boot().ok(), &root),
            Err(error) => panic!("the expansion records at {}: {error}", store.display()),
        }
    }

    /// The store over `files`; `plant` is [`RecordPlant::None`] but in the simulation.
    pub fn on(files: OnDir, plant: RecordPlant) -> DirStore {
        DirStore {
            files,
            tally: Tally::default(),
            plant,
            torn: core::sync::atomic::AtomicU64::new(0),
            hashed: core::sync::atomic::AtomicU64::new(0),
            replaced: core::sync::atomic::AtomicU64::new(0),
        }
    }

    /// How many results tables were replaced by a longer one under this store's read of them.
    pub fn replaced(&self) -> u64 {
        self.replaced.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// How many results tables this store found torn, and how many it hashed whole.
    pub fn torn_and_hashed(&self) -> (u64, u64) {
        (self.torn.load(core::sync::atomic::Ordering::Relaxed), self.hashed.load(core::sync::atomic::Ordering::Relaxed))
    }

    fn name(kind: Kind, key: &ContentId) -> RecordName {
        let kind: &[u8] = match kind {
            Kind::List => b"list/",
            Kind::Answer => b"answer/",
        };
        Self::named(kind, key)
    }

    /// `<folder><64 hex of key>`: `folder` is one of the store's (at most seven bytes).
    fn named(kind: &[u8], key: &ContentId) -> RecordName {
        let mut name = RecordName { bytes: [0; 71], len: 0 };
        name.bytes[..kind.len()].copy_from_slice(kind);
        let mut at = kind.len();
        for byte in key.0 {
            name.bytes[at] = b"0123456789abcdef"[usize::from(byte >> 4)];
            name.bytes[at + 1] = b"0123456789abcdef"[usize::from(byte & 15)];
            at += 2;
        }
        name.len = at as u8;
        name
    }
}

/// A record's file name, `list/<64 hex>` or `answer/<64 hex>`, inline: no allocation a lookup.
#[derive(Clone, Copy, Debug)]
pub struct RecordName {
    bytes: [u8; 71],
    len: u8,
}

impl RecordName {
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).expect("a record name is ASCII by construction")
    }
}

impl std::fmt::Display for RecordName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Store for DirStore {
    type Held = Vec<u8>;
    type Error = FilesError;

    fn get(&self, kind: Kind, key: &ContentId) -> Result<Option<Self::Held>, FilesError> {
        self.get_at(Self::name(kind, key))
    }

    fn put(&self, kind: Kind, key: ContentId, bytes: &[u8]) -> Result<(), FilesError> {
        self.put_at(Self::name(kind, &key), bytes)
    }

    fn bytes(held: &Self::Held) -> &[u8] {
        held
    }

    fn tally(&self) -> &Tally {
        &self.tally
    }
}

impl DirStore {
    /// A spawn's answer record (`spawn_records::Recorded`): `spawn/<64 hex of the request key>`.
    pub(crate) fn get_spawn(&self, key: &ContentId) -> Result<Option<Vec<u8>>, FilesError> {
        self.get_at(Self::named(b"spawn/", key))
    }

    /// Keeps a spawn's answer record, written once: other bytes under the key are refused.
    pub(crate) fn put_spawn(&self, key: &ContentId, bytes: &[u8]) -> Result<(), FilesError> {
        self.put_at(Self::named(b"spawn/", key), bytes)
    }

    /// A build script's `rustc` probe's answer record (blaze-build-script probe_check.rs
    /// `encode`): `probe/<64 hex of the question's key>`.
    pub(crate) fn get_probe(&self, key: &ContentId) -> Result<Option<Vec<u8>>, FilesError> {
        self.get_at(Self::named(b"probe/", key))
    }

    /// Keeps a probe's answer record, written once: other bytes under the key are refused
    /// (rule 21), equal bytes are no write.
    pub(crate) fn put_probe(&self, key: &ContentId, bytes: &[u8]) -> Result<(), FilesError> {
        self.put_at(Self::named(b"probe/", key), bytes)
    }

    /// The record at `name`, its seal checked: `None` when absent or torn.
    fn get_at(&self, name: RecordName) -> Result<Option<Vec<u8>>, FilesError> {
        let mut bytes = match self.files.read(&name)? {
            Held::Absent => return Ok(None),
            Held::Whole(bytes) => bytes,
            // A record of this store is written once and never by other bytes (`put`), so a
            // longer file under its name is not a record of it: refused by name.
            Held::Replaced => return Err(FilesError::OtherBytes { name }),
        };
        // A torn file (any prefix of a publish, or zeros where a sector was lost) is a miss.
        let Some(body_len) = bytes.len().checked_sub(SEAL_LEN) else { return Ok(None) };
        let sealed = ContentId::of(SEAL, &bytes[..body_len]).0 == bytes[body_len..];
        if !sealed && self.plant != RecordPlant::SealSkipped {
            return Ok(None);
        }
        bytes.truncate(body_len);
        Ok(Some(bytes))
    }

    /// `bytes` published at `name` with its seal, written once: a whole record of other bytes
    /// there is refused naming it (rule 21).
    fn put_at(&self, name: RecordName, bytes: &[u8]) -> Result<(), FilesError> {
        if let Some(held) = self.get_at(name)? {
            return if held == bytes { Ok(()) } else { Err(FilesError::OtherBytes { name }) };
        }
        let mut whole = Vec::with_capacity(bytes.len() + SEAL_LEN);
        whole.extend_from_slice(bytes);
        whole.extend_from_slice(&ContentId::of(SEAL, bytes).0);
        self.files.publish(&name, &whole)
    }
}

fn kind_byte(kind: ProcKind) -> u8 {
    match kind {
        ProcKind::FnLike => 0,
        ProcKind::Attr => 1,
        ProcKind::Derive => 2,
    }
}

fn edition_byte(edition: Edition) -> u8 {
    edition as u8
}

fn delim_byte(delim: Delim) -> u8 {
    match delim {
        Delim::Paren => 0,
        Delim::Brace => 1,
        Delim::Bracket => 2,
        Delim::Invisible => 3,
    }
}

fn delim_of(byte: u8) -> Option<Delim> {
    Some(match byte {
        0 => Delim::Paren,
        1 => Delim::Brace,
        2 => Delim::Bracket,
        3 => Delim::Invisible,
        _ => return None,
    })
}

fn spacing_byte(spacing: Spacing) -> u8 {
    match spacing {
        Spacing::Alone => 0,
        Spacing::Joint => 1,
        Spacing::JointHidden => 2,
    }
}

fn spacing_of(byte: u8) -> Option<Spacing> {
    Some(match byte {
        0 => Spacing::Alone,
        1 => Spacing::Joint,
        2 => Spacing::JointHidden,
        _ => return None,
    })
}

fn lit_bytes(kind: LitKind) -> [u8; 2] {
    match kind {
        LitKind::Byte => [0, 0],
        LitKind::Char => [1, 0],
        LitKind::Integer => [2, 0],
        LitKind::Float => [3, 0],
        LitKind::Str => [4, 0],
        LitKind::StrRaw(n) => [5, n],
        LitKind::ByteStr => [6, 0],
        LitKind::ByteStrRaw(n) => [7, n],
        LitKind::CStr => [8, 0],
        LitKind::CStrRaw(n) => [9, n],
        LitKind::Err => [10, 0],
    }
}

fn lit_of(tag: u8, n: u8) -> Option<LitKind> {
    Some(match tag {
        0 => LitKind::Byte,
        1 => LitKind::Char,
        2 => LitKind::Integer,
        3 => LitKind::Float,
        4 => LitKind::Str,
        5 => LitKind::StrRaw(n),
        6 => LitKind::ByteStr,
        7 => LitKind::ByteStrRaw(n),
        8 => LitKind::CStr,
        9 => LitKind::CStrRaw(n),
        10 => LitKind::Err,
        _ => return None,
    })
}

/// A token's kind in eight bytes: tag, then its fields.
fn token_bytes(kind: TokenKind) -> [u8; 8] {
    let mut out = [0u8; 8];
    match kind {
        TokenKind::Ident { raw, spacing } => {
            out[0] = 0;
            out[1] = u8::from(raw == Raw::Yes);
            out[2] = spacing_byte(spacing);
        }
        TokenKind::Punct { char, spacing } => {
            out[0] = 1;
            out[1] = spacing_byte(spacing);
            out[4..8].copy_from_slice(&u32::from(char).to_le_bytes());
        }
        TokenKind::Literal { kind, suffix_len, spacing } => {
            out[0] = 2;
            out[1] = spacing_byte(spacing);
            out[2..4].copy_from_slice(&lit_bytes(kind));
            out[4..8].copy_from_slice(&suffix_len.to_le_bytes());
        }
        TokenKind::Open(delim) => {
            out[0] = 3;
            out[1] = delim_byte(delim);
        }
        TokenKind::Close(delim) => {
            out[0] = 4;
            out[1] = delim_byte(delim);
        }
    }
    out
}

fn token_of(bytes: [u8; 8]) -> Option<TokenKind> {
    let word = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    Some(match bytes[0] {
        0 => TokenKind::Ident { raw: if bytes[1] == 1 { Raw::Yes } else { Raw::No }, spacing: spacing_of(bytes[2])? },
        1 => TokenKind::Punct { char: char::from_u32(word)?, spacing: spacing_of(bytes[1])? },
        2 => TokenKind::Literal { kind: lit_of(bytes[2], bytes[3])?, suffix_len: word, spacing: spacing_of(bytes[1])? },
        3 => TokenKind::Open(delim_of(bytes[1])?),
        4 => TokenKind::Close(delim_of(bytes[1])?),
        _ => return None,
    })
}

/// The tokens of `stream` as the macro sees them: kind, spacing and text, each piece handed to
/// `out` in order (a record's buffer, or the key's hasher, which then holds no copy).
fn stream_pieces(stream: &TokenStream<'_>, mut out: impl FnMut(&[u8])) {
    out(&(stream.len() as u32).to_le_bytes());
    for token in stream.tokens() {
        out(&token_bytes(token.kind));
        let text = stream.text_of(token);
        out(&(text.len() as u32).to_le_bytes());
        out(text.as_bytes());
    }
}

/// The version of what the host gives a macro (the bridge server, the sandbox's answers, the
/// streams a call passes): in every expansion's base key, so a change to it never reads an
/// answer an older host made. 2: an attribute without arguments is given an empty stream (1
/// gave none, and every such macro's client panicked reading it). 3: native bridge preserves
/// token spans / `TokenOrigin` hygiene and empty attribute streams. 4: native bridge remaps host
/// `CARGO_MANIFEST_DIR` back to virtual mount root in emitted string literals (D60/D96).
pub const HOST_VERSION: u32 = 4;

/// The base of `call`'s record: computed from text, before the run.
pub fn base_key(module_wasm_sha256: &str, export: &Exported, edition: Edition, call: &ProcCall<'_>) -> ContentId {
    base_key_under(HOST_VERSION, module_wasm_sha256, export, edition, call)
}

/// [`base_key`] as the host of `host_version` computes it: an answer kept by another host
/// version is under another key, so it is never read.
pub(crate) fn base_key_under(host_version: u32, module_wasm_sha256: &str, export: &Exported, edition: Edition, call: &ProcCall<'_>) -> ContentId {
    // Streamed into the hasher, never gathered: the digest of the same bytes in the same
    // order (BLAKE3 hashes the concatenation), so the keys are those a buffer gave, and a
    // lookup allocates nothing for its key.
    let mut hasher = Hasher::new("blaze proc-macro expansion 1");
    hasher.update(&host_version.to_le_bytes()).update(b"module ").update(module_wasm_sha256.as_bytes());
    hasher.update(&export.index.to_le_bytes()).update(&[kind_byte(export.kind)]);
    hasher.update(&(export.name.len() as u32).to_le_bytes()).update(export.name.as_bytes());
    hasher.update(&[kind_byte(call.kind), edition_byte(edition)]);
    stream_pieces(call.input, |piece| {
        hasher.update(piece);
    });
    match call.attr_args {
        Some(attr) => {
            hasher.update(&[1]);
            stream_pieces(attr, |piece| {
                hasher.update(piece);
            });
        }
        None => {
            hasher.update(&[0]);
        }
    }
    hasher.finish()
}

fn origin_bytes(out: &mut Vec<u8>, origin: TokenOrigin) {
    out.extend_from_slice(&origin_piece(origin));
}

/// An origin's canonical bytes: its tag, then its value (little-endian).
fn origin_piece(origin: TokenOrigin) -> [u8; 5] {
    let (tag, value) = match origin {
        TokenOrigin::Input(at) => (0u8, at),
        TokenOrigin::AttrArgs(at) => (1, at),
        TokenOrigin::Def(at) => (2, at),
        TokenOrigin::Params(at) => (3, at),
        TokenOrigin::Made(Transparency::Transparent) => (4, 0),
        TokenOrigin::Made(Transparency::SemiOpaque) => (4, 1),
        TokenOrigin::Made(Transparency::Opaque) => (4, 2),
    };
    let v = value.to_le_bytes();
    [tag, v[0], v[1], v[2], v[3]]
}

fn origin_of(tag: u8, value: u32) -> Option<TokenOrigin> {
    Some(match (tag, value) {
        (0, at) => TokenOrigin::Input(at),
        (1, at) => TokenOrigin::AttrArgs(at),
        (2, at) => TokenOrigin::Def(at),
        (3, at) => TokenOrigin::Params(at),
        (4, 0) => TokenOrigin::Made(Transparency::Transparent),
        (4, 1) => TokenOrigin::Made(Transparency::SemiOpaque),
        (4, 2) => TokenOrigin::Made(Transparency::Opaque),
        _ => return None,
    })
}

fn level_byte(level: ProcLevel) -> u8 {
    match level {
        ProcLevel::Error => 0,
        ProcLevel::Warning => 1,
        ProcLevel::Note => 2,
        ProcLevel::Help => 3,
    }
}

fn level_of(byte: u8) -> Option<ProcLevel> {
    Some(match byte {
        0 => ProcLevel::Error,
        1 => ProcLevel::Warning,
        2 => ProcLevel::Note,
        3 => ProcLevel::Help,
        _ => return None,
    })
}

/// `produced` without its reads, in canonical bytes: the tokens with their origins, the
/// diagnostics and their text. What a crate's results table keeps a call's answer as
/// ([`table`]); the reads are the row's read list.
fn answer_into(out: &mut Vec<u8>, produced: &Produced<'_>) {
    answer_pieces(produced, |piece| out.extend_from_slice(piece));
}

/// [`answer_into`]'s bytes, each piece handed to `out` in order: what a buffer is filled from,
/// and what [`answer_len`] measures, so an answer is written into ONE buffer of its own length
/// (two passes: measure, then write), never into one grown a push at a time.
fn answer_pieces(produced: &Produced<'_>, mut out: impl FnMut(&[u8])) {
    stream_pieces(&produced.tokens, &mut out);
    for origin in produced.origins.iter() {
        out(&origin_piece(*origin));
    }
    out(&(produced.diagnostics.len() as u32).to_le_bytes());
    for diagnostic in produced.diagnostics.iter() {
        match *diagnostic {
            ProcDiagnostic::Emitted { level, message, at } => {
                out(&[0, level_byte(level)]);
                out(&message.at.to_le_bytes());
                out(&message.len.to_le_bytes());
                match at {
                    Some(origin) => {
                        out(&[1]);
                        out(&origin_piece(origin));
                    }
                    None => out(&[0]),
                }
            }
            ProcDiagnostic::Panicked { message } => {
                out(&[1]);
                out(&message.at.to_le_bytes());
                out(&message.len.to_le_bytes());
            }
        }
    }
    out(&(produced.text.len() as u32).to_le_bytes());
    out(produced.text.as_slice());
}

/// How many bytes [`answer_pieces`] hands out for `produced`: the first of the two passes.
fn answer_len(produced: &Produced<'_>) -> usize {
    let mut len = 0usize;
    answer_pieces(produced, |piece| len += piece.len());
    len
}

/// Why a record's bytes are not one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Corrupt(pub &'static str);

struct Cursor<'b> {
    bytes: &'b [u8],
}

impl<'b> Cursor<'b> {
    fn take(&mut self, n: usize, what: &'static str) -> Result<&'b [u8], Corrupt> {
        if self.bytes.len() < n {
            return Err(Corrupt(what));
        }
        let (head, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Ok(head)
    }

    fn u8(&mut self, what: &'static str) -> Result<u8, Corrupt> {
        Ok(self.take(1, what)?[0])
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, Corrupt> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn str(&mut self, what: &'static str) -> Result<&'b str, Corrupt> {
        let len = self.u32(what)? as usize;
        std::str::from_utf8(self.take(len, what)?).map_err(|_| Corrupt(what))
    }

    fn origin(&mut self) -> Result<TokenOrigin, Corrupt> {
        let tag = self.u8("an origin")?;
        let value = self.u32("an origin")?;
        origin_of(tag, value).ok_or(Corrupt("an origin's tag"))
    }

    /// A read's answer as the note holds it.
    fn answer(&mut self) -> Result<ReadAnswer, Corrupt> {
        Ok(match self.u8("a read's answer")? {
            0 => ReadAnswer::Absent,
            1 => ReadAnswer::Digest(ContentId(self.take(32, "a read's digest")?.try_into().map_err(|_| Corrupt("a read's digest"))?)),
            _ => return Err(Corrupt("a read's answer tag")),
        })
    }
}

/// The [`Produced`] whose answer ([`answer_into`]'s bytes) is all `c` still holds, with the
/// reads `reads`, in `alloc`.
fn answer_of<'a>(mut c: Cursor<'_>, reads: Reads<'a>, edition: Edition, alloc: Alloc<'a>) -> Result<Produced<'a>, Corrupt> {
    let ctx = Ctx::root(edition);
    let mut tokens = TokenStream::new_in(alloc);
    let count = c.u32("the token count")?;
    for _ in 0..count {
        let kind: [u8; 8] = c.take(8, "a token")?.try_into().map_err(|_| Corrupt("a token"))?;
        let kind = token_of(kind).ok_or(Corrupt("a token's kind"))?;
        let text = c.str("a token's text")?;
        tokens.push(kind, text, ctx).map_err(|_| Corrupt("a token that does not fit"))?;
    }
    let mut origins: AVec<'a, TokenOrigin> = AVec::new_in(alloc);
    for _ in 0..count {
        origins.push(c.origin()?);
    }
    let (diagnostics, text) = tail_of(c, alloc)?;
    Ok(Produced { tokens, origins, reads, diagnostics, text })
}

/// An answer's diagnostics and their text: what `c` holds after the tokens and their origins,
/// in the loose form ([`answer_into`]) and in a table's dense one alike.
fn tail_of<'a>(mut c: Cursor<'_>, alloc: Alloc<'a>) -> Result<(AVec<'a, ProcDiagnostic>, AVec<'a, u8>), Corrupt> {
    let mut diagnostics: AVec<'a, ProcDiagnostic> = AVec::new_in(alloc);
    for _ in 0..c.u32("the diagnostic count")? {
        let diagnostic = match c.u8("a diagnostic")? {
            0 => {
                let level = level_of(c.u8("a level")?).ok_or(Corrupt("a level"))?;
                let message = Text { at: c.u32("a message")?, len: c.u32("a message")? };
                let at = match c.u8("a diagnostic's place")? {
                    0 => None,
                    _ => Some(c.origin()?),
                };
                ProcDiagnostic::Emitted { level, message, at }
            }
            1 => ProcDiagnostic::Panicked { message: Text { at: c.u32("a message")?, len: c.u32("a message")? } },
            _ => return Err(Corrupt("a diagnostic's tag")),
        };
        diagnostics.push(diagnostic);
    }
    let len = c.u32("the text")? as usize;
    let mut text: AVec<'a, u8> = AVec::new_in(alloc);
    text.extend_from_slice(c.take(len, "the text")?);
    if !c.bytes.is_empty() {
        return Err(Corrupt("bytes after the record's end"));
    }
    Ok((diagnostics, text))
}

/// Why a record could not be read or kept.
#[derive(Debug)]
pub enum RecordError {
    /// The row of `base` in its crate's table is not this layout's answer.
    Corrupt { base: ContentId, what: &'static str },
    /// The run's read `read` (its index) had an answer a read list does not hold.
    Unkeepable { base: ContentId, read: usize },
    /// Another answer is kept under `base` for the same reads with the same digests: the run
    /// read or depended on something its key and list do not name (rule 21).
    TwoAnswers { base: ContentId },
    /// A crate's results table, or the file vouching for it, could not be read or written.
    Files(FilesError),
    /// The results table of the crate named `krate` is whole by its seal and is no table of
    /// this layout: `what` in it is wrong.
    Table { krate: ContentId, what: &'static str },
    /// The results of the crate named `krate` have `count` of `what` (rows, read lists, the
    /// bytes of one answer or of one read list): more than the table's 32-bit word for it
    /// holds. The table's own length is not bounded: its places are 64-bit.
    TooLarge { krate: ContentId, what: &'static str, count: usize },
    /// The allocator the results were handed refused the `bytes` of the row of `base` (its
    /// read list or its answer, each one block of its own length).
    NoRoom { base: ContentId, bytes: usize },
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordError::Corrupt { base, what } => write!(f, "the expansion record of base {base:x}: {what}"),
            RecordError::Unkeepable { base, read } => {
                write!(f, "the expansion of base {base:x}: its read {read} has an answer that is neither absent nor a digest")
            }
            RecordError::TwoAnswers { base } => write!(
                f,
                "the expansion of base {base:x}: another answer is kept under the same reads and digests; the run read something its key does not name (rule 21)"
            ),
            RecordError::Files(error) => write!(f, "a crate's proc-macro results table: {error}"),
            RecordError::Table { krate, what } => write!(f, "the proc-macro results table of crate {krate:x} (crate/{krate:x}): {what} is not this layout's"),
            RecordError::TooLarge { krate, what, count } => {
                write!(f, "the proc-macro results of crate {krate:x}: {count} {what}, more than the table's 32-bit word for it holds; not kept")
            }
            RecordError::NoRoom { base, bytes } => {
                write!(f, "the expansion of base {base:x}: its allocator refused the {bytes} bytes of its row (blaze-proc-macro table.rs `CrateResults::insert`)")
            }
        }
    }
}

impl std::error::Error for RecordError {}

#[path = "table.rs"]
pub mod table;

#[cfg(test)]
#[path = "record_tests.rs"]
mod tests;
