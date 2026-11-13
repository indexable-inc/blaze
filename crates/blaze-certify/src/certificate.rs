//! The request certificate's record: what one build that answered read, and what it left
//! (docs/v2/plan/design/certificate.md; end-state.md, "The shared page").
//!
//! It is filed in `blaze_record`'s one form, in the write-once store:
//!
//! - the **base key** ([`base`]) is an identity: the relocated request shape and the tool.
//!   No root id and no content is in it; the tree's state enters as reads;
//! - a **read-list** names what the build read ([`Read`]: a node of the checkout's view, a
//!   variable, a file or a listing outside every view, how its client is drawn), sorted, with
//!   no value ([`Certificate::list`]; its note is this layout's word);
//! - the **answer** ([`Answer`]) is under the list's bytes and the digest each read had when
//!   the build read it ([`Certificate::answer_key`]): the outputs each by the hash of its
//!   bytes, the units' record keys, what a build with nothing to do says. Content only: no
//!   stamp, no absolute path of the checkout ([`relocated`]), so equal builds are equal bytes
//!   and a second put is the same put.
//!
//! What is local to one machine is beside the record, never in it ([`Evidence`]): the stamp
//! each output was last seen to have with the content under it (kept across a start as the
//! journal's marks, remade by hashing the output once where missing), and the digests of the
//! checkout's last build, which is how a miss names the reads that moved.
//!
//! The check and the write are the request machine's ([`crate::request`]).
//!
//! The invariant: a certificate is given only when a fresh build would produce the same
//! outputs and say the same bytes.
//!
//! A sans-IO core (rule 14): no file, clock, watch or store is read here.

#![blaze::sans_io]

use std::fmt;

use blaze_core::Stamp;
use blaze_ids::{ContentId, Hasher};
use blaze_record::{AnswerKey, Digest, ListRef};

/// A place the code a run executes is held in that is no file: an output of the build that
/// placed it there, as a file is (blaze-cargo's `Steps::held_code`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CodeHolder {
    /// The launch host's program (blaze-cargo's `serve::unit::run::launch`): a launcher's
    /// file is the same bytes at every build, and its code is the host's, told to each
    /// launcher.
    Launcher,
    /// The daemon's code memory of a dev executable (blaze-cargo's
    /// `serve::unit::run::inproc`), which its runs map over their file.
    Memory,
}

impl CodeHolder {
    /// Every holder, each once.
    pub const ALL: [CodeHolder; 2] = [CodeHolder::Launcher, CodeHolder::Memory];
}

/// The holder as errors and `blaze why` name it.
impl fmt::Display for CodeHolder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CodeHolder::Launcher => "the launch host",
            CodeHolder::Memory => "the daemon's code memory",
        })
    }
}

/// The digest of the variable `name` of `env`, the request's environment: its first pair of
/// that name, as `blaze_units::query::env_var` reads it; [`Digest::Nothing`] where unset. What
/// a [`Read::Env`] is valued at, by the daemon and by a client alone alike.
pub fn env_digest(env: &[(String, String)], name: &str) -> Digest {
    match env.iter().find(|(it, _)| it == name) {
        Some((_, value)) => Digest::Is(
            Hasher::new("blaze v2 serve request certificate variable").part(value.as_bytes()).finish(),
        ),
        None => Digest::Nothing,
    }
}

/// The layout of a certificate's parts: the word in each read-list's note and at the head of
/// each answer. A change of any field's meaning or order takes the next number and a line in
/// [`PREVIOUS`].
pub const LAYOUT: u32 = 4;

/// The first bytes of a list's note and of every answer.
const MAGIC: &[u8; 8] = b"blzcert\n";

/// The layouts before [`LAYOUT`], each by name: a part of one is refused by that name
/// ([`Undecoded::Previous`]), and the build that follows files the record anew (rule 26).
pub const PREVIOUS: &[(u32, &str)] = &[
    (0, "the draft of 2026-11-12, whose one group was the view's whole root"),
    (1, "the record of 2026-11-12 keyed by the view's root, with its outputs' stamps in its bytes"),
    (2, "the record of 2026-11-12 whose outputs were placed files alone, not the code a launcher or a code memory holds"),
    (3, "the record of 2026-11-12 whose reads had no entry (a path itself, a link not followed), and that no build that looked at one was written as"),
];

/// The step a request that was not certified is under in `blaze why`.
pub const STEP: &str = "request certificate";

/// What stands for the checkout's own directory in everything a certificate is made of
/// ([`relocated`]): no absolute path of a checkout is in a key or a record.
pub const ROOT: &str = "\u{0}checkout";

/// `text` with the checkout's directory `root` (absolute, no trailing `/`) replaced by
/// [`ROOT`] wherever it is a whole path prefix: at the start of `text` or after a character
/// no path holds in its middle of a word (`=`, `:`, a space, a quote, a comma), and followed
/// by `/`, one of those characters or the end. What a request's config is made a shape of and what a build said is
/// kept as, so two checkouts of one content ask for one certificate ([`replaced`] undoes it).
pub fn relocated(text: &str, root: &str) -> String {
    let mut out = String::with_capacity(text.len()); // alloc: once a string of a request's config
    if root.is_empty() {
        out.push_str(text);
        return out;
    }
    let mut rest = text;
    let mut at_start = true;
    while let Some(found) = rest.find(root) {
        let before_ok = match rest[..found].chars().next_back() {
            None => at_start,
            Some(c) => matches!(c, '=' | ':' | ' ' | '"' | '\'' | ','),
        };
        let after = &rest[found + root.len()..];
        let after_ok = after.is_empty() || after.starts_with(['/', ':', ' ', '"', '\'', ',']);
        out.push_str(&rest[..found]);
        if before_ok && after_ok {
            out.push_str(ROOT);
        } else {
            out.push_str(root);
        }
        rest = after;
        at_start = false;
    }
    out.push_str(rest);
    out
}

/// `text` as [`relocated`] left it, with the checkout at `root` put back: what a certificate
/// said is said again with for the checkout that asks. [`ROOT`] holds a NUL, which no path
/// and no rendered text holds, so every [`ROOT`] in `text` is one [`relocated`] wrote.
pub fn replaced(text: &str, root: &str) -> String {
    text.replace(ROOT, root) // alloc: once a string said again
}

/// A request's identity, each part relocated: none holds the tree's content.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Shape {
    /// Its config as the plan reads it.
    pub plan: ContentId,
    /// Its config less its environment.
    pub config: ContentId,
    /// What it says past its config (verbosity, the goal).
    pub request: ContentId,
}

/// The base key of the certificates of requests of `shape` served by `tool` (the blaze's own
/// identity; the toolchain is in the shape's config, its mounts by content id). An identity:
/// every checkout, of whatever content, asks under the one base its shape and tool give.
pub fn base(shape: &Shape, tool: &[u8]) -> ContentId {
    Hasher::new("blaze v2 serve request certificate")
        .id(&shape.plan)
        .id(&shape.config)
        .id(&shape.request)
        .part(tool)
        .finish()
}

/// One thing a build read from outside itself, by its name: what a read-list holds. Its
/// value is one digest, [`Digest::Nothing`] where nothing was there (a negative probe).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Read {
    /// A node of the checkout's view, from the view's root, `/`-separated (`""` the root): a
    /// directory by its Merkle id (every file below it and every name that is not there: the
    /// covering first check of a package, never the grain of what is redone) or one file by
    /// its id. A path dependency outside the workspace is one too, by its mount's place.
    Node(String),
    /// A variable of the request's environment: one a step asked for by name, or one read
    /// unasked that was set (the plan's variables, `BLAZE_*`).
    Env(String),
    /// A file outside every view, by its absolute path (a link: by where it points).
    Outside(String),
    /// A directory outside every view, by its listing.
    Listing(String),
    /// Everything below a directory outside every view, by the hash of its walk: what a
    /// build script that hashed a directory whole read.
    Tree(String),
    /// As [`Read::Tree`], walked as a package is.
    Package(String),
    /// A path outside every view as the entry it is, a link not followed (a directory,
    /// nothing, or a file's or a link's id and mode): not a file's content, which
    /// [`Read::Outside`] is, through every link.
    Entry(String),
    /// How the request's client is shown diagnostics: read only by a build that said one.
    Drawn,
}

impl Read {
    /// Its kind's number and its text: what its name is made of, and what orders reads.
    pub fn parts(&self) -> (u8, &str) {
        match self {
            Read::Node(path) => (0, path.as_str()),
            Read::Env(name) => (1, name.as_str()),
            Read::Outside(path) => (2, path.as_str()),
            Read::Listing(path) => (3, path.as_str()),
            Read::Drawn => (4, ""),
            Read::Tree(path) => (5, path.as_str()),
            Read::Package(path) => (6, path.as_str()),
            Read::Entry(path) => (7, path.as_str()),
        }
    }

    /// Its name in a read-list: its kind, then its text.
    pub fn name(&self) -> Vec<u8> {
        let (kind, text) = self.parts();
        let mut out = Vec::with_capacity(1 + text.len()); // alloc: a read's name, once a list written
        out.push(kind);
        out.extend_from_slice(text.as_bytes());
        out
    }

    /// The read `name` is of ([`Read::name`]), or why it is none.
    pub fn named(name: &[u8]) -> Result<Read, Undecoded> {
        let (kind, text) = name.split_first().ok_or(Undecoded::Malformed("a read's kind"))?;
        // alloc: a read's own string, once a list read
        let text = std::str::from_utf8(text)
            .map(str::to_owned)
            .map_err(|_| Undecoded::Malformed("a read's name"))?;
        Ok(match *kind {
            0 => Read::Node(text),
            1 => Read::Env(text),
            2 => Read::Outside(text),
            3 => Read::Listing(text),
            4 if text.is_empty() => Read::Drawn,
            5 => Read::Tree(text),
            6 => Read::Package(text),
            7 => Read::Entry(text),
            _ => return Err(Undecoded::Malformed("a read's kind")),
        })
    }
}

/// The input as `blaze why` names it.
impl fmt::Display for Read {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Read::Node(path) if path.is_empty() => f.write_str("the checkout's files"),
            Read::Node(path) | Read::Outside(path) => f.write_str(path),
            Read::Env(name) => write!(f, "${name}"),
            Read::Listing(path) => write!(f, "the names in {path}"),
            Read::Tree(path) => write!(f, "everything below {path}"),
            Read::Package(path) => write!(f, "the package below {path}"),
            Read::Entry(path) => write!(f, "what {path} is (a file, a link, a directory or nothing)"),
            Read::Drawn => f.write_str("how diagnostics are shown"),
        }
    }
}

/// The one base a certificate's file outputs are under: the file system's root. Every path is
/// absolute until the shape is relocated (certificate.md, "State"). Moved here from
/// blaze-cargo's certify.rs, so a client checking alone reads the same base.
pub const ABSOLUTE: u8 = 0;

/// The base of an output that is no file: the code the launch host holds for the launcher
/// program whose file is at the output's path ([`CodeHolder::Launcher`]).
pub const LAUNCHER: u8 = 0xF0;

/// The base of an output that is no file: the code the daemon's code memory holds for the dev
/// executable whose file is at the output's path ([`CodeHolder::Memory`]).
pub const MEMORY: u8 = 0xF1;

/// The base a certificate names `holder`'s outputs under.
pub fn code_base(holder: CodeHolder) -> u8 {
    match holder {
        CodeHolder::Launcher => LAUNCHER,
        CodeHolder::Memory => MEMORY,
    }
}

/// The holder whose outputs are under `base`, or `None` for a base of files.
pub fn holder_of(base: u8) -> Option<CodeHolder> {
    match base {
        LAUNCHER => Some(CodeHolder::Launcher),
        MEMORY => Some(CodeHolder::Memory),
        _ => None,
    }
}

/// An output of the build, by content: a file it placed, or the code a place that is no file
/// holds for a program it built ([`holder_of`] its base), which is what a run of the program
/// executes when the program's file is the same bytes at every build (a launcher's) or is
/// overlaid by memory (a dev executable's).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Output {
    /// Which of the daemon's bases `path` is under (the target directory, the build
    /// directory, the standard library's builds), by index; or [`LAUNCHER`] or [`MEMORY`].
    pub base: u8,
    /// From that base, `/`-separated.
    pub path: String,
    /// The hash of the bytes placed there; for a holder's output, the identity of the build
    /// whose code it holds (blaze-cargo's `serve::work::HeldCode::Is`).
    pub content: ContentId,
}

/// What a certified request is answered with: the value of the record.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Answer {
    /// The outputs, sorted by base then path.
    pub outputs: Vec<Output>,
    /// The units' record keys, in the plan's order: what `blaze why` reads of an answered
    /// request.
    pub units: Vec<ContentId>,
    /// What the build said, as a build with nothing to do says it, and its `Work`: the
    /// daemon's encoding (deterministic CBOR), every text of it [`relocated`]; opaque here.
    pub said: Vec<u8>,
}

/// Why a certificate's bytes are not read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Undecoded {
    /// Not a certificate's part at all.
    NotOne,
    /// Of a layout before this one, by its number and name ([`PREVIOUS`]).
    Previous(u32, &'static str),
    /// Of a layout this blaze does not know.
    Unknown(u32),
    /// Cut short or with bytes left over, at this field.
    Malformed(&'static str),
}

impl fmt::Display for Undecoded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Undecoded::NotOne => f.write_str("the record is no request certificate"),
            Undecoded::Previous(n, name) => {
                write!(f, "the request certificate is of layout {n} ({name}); this blaze reads layout {LAYOUT}")
            }
            Undecoded::Unknown(n) => {
                write!(f, "the request certificate is of layout {n}, which this blaze does not know (it reads layout {LAYOUT})")
            }
            Undecoded::Malformed(field) => {
                write!(f, "the request certificate's bytes end or go on wrongly at `{field}`")
            }
        }
    }
}

fn put_u32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_le_bytes());
}

fn put_len(out: &mut Vec<u8>, n: usize) {
    put_u32(out, u32::try_from(n).expect("a certificate's list is shorter than 2^32"));
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_len(out, bytes.len());
    out.extend_from_slice(bytes);
}

/// A reader of a certificate's bytes: each take names its field for the refusal.
struct Bytes<'a> {
    rest: &'a [u8],
}

impl<'a> Bytes<'a> {
    fn take(&mut self, n: usize, field: &'static str) -> Result<&'a [u8], Undecoded> {
        if self.rest.len() < n {
            return Err(Undecoded::Malformed(field));
        }
        let (head, rest) = self.rest.split_at(n);
        self.rest = rest;
        Ok(head)
    }

    fn u8(&mut self, field: &'static str) -> Result<u8, Undecoded> {
        Ok(self.take(1, field)?[0])
    }

    fn u32(&mut self, field: &'static str) -> Result<u32, Undecoded> {
        let bytes = self.take(4, field)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// A list's length, refused where the bytes left cannot hold that many items of at least
    /// `least` bytes each: no length read off a record reserves more than the record holds.
    fn len(&mut self, least: usize, field: &'static str) -> Result<usize, Undecoded> {
        let n = self.u32(field)? as usize;
        if n.saturating_mul(least.max(1)) > self.rest.len() {
            return Err(Undecoded::Malformed(field));
        }
        Ok(n)
    }

    fn bytes(&mut self, field: &'static str) -> Result<&'a [u8], Undecoded> {
        let n = self.len(1, field)?;
        self.take(n, field)
    }

    fn text(&mut self, field: &'static str) -> Result<String, Undecoded> {
        let bytes = self.bytes(field)?;
        // alloc: a certificate's own string, once a record read
        std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| Undecoded::Malformed(field))
    }

    fn id(&mut self, field: &'static str) -> Result<ContentId, Undecoded> {
        let mut id = [0; 32];
        id.copy_from_slice(self.take(32, field)?);
        Ok(ContentId(id))
    }

    /// [`MAGIC`] and [`LAYOUT`], or which other layout the bytes are of.
    fn head(&mut self) -> Result<(), Undecoded> {
        if self.take(MAGIC.len(), "magic").map_err(|_| Undecoded::NotOne)? != MAGIC {
            return Err(Undecoded::NotOne);
        }
        let layout = self.u32("layout")?;
        if layout != LAYOUT {
            return Err(match PREVIOUS.iter().find(|(n, _)| *n == layout) {
                Some((n, name)) => Undecoded::Previous(*n, name),
                None => Undecoded::Unknown(layout),
            });
        }
        Ok(())
    }
}

/// The head of an answer and the whole of a list's note: [`MAGIC`], then `layout`.
fn head_of(layout: u32) -> [u8; 12] {
    let mut out = [0; 12];
    out[..8].copy_from_slice(MAGIC);
    out[8..].copy_from_slice(&layout.to_le_bytes());
    out
}

impl Answer {
    /// Its bytes: [`MAGIC`], [`LAYOUT`], then each field in the struct's order, every number
    /// little-endian, every list and string under its length. One encoding a value: equal
    /// answers are equal bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.said.len()); // alloc: the record, once a build that answered
        out.extend_from_slice(&head_of(LAYOUT));
        put_len(&mut out, self.outputs.len());
        for output in &self.outputs {
            out.push(output.base);
            put_bytes(&mut out, output.path.as_bytes());
            out.extend_from_slice(&output.content.0);
        }
        put_len(&mut out, self.units.len());
        for unit in &self.units {
            out.extend_from_slice(&unit.0);
        }
        put_bytes(&mut out, &self.said);
        out
    }

    /// The answer `bytes` are, or why they are not read. One of a layout before this one is
    /// refused by its name; never read as an empty answer.
    pub fn decode(bytes: &[u8]) -> Result<Answer, Undecoded> {
        let mut bytes = Bytes { rest: bytes };
        bytes.head()?;
        let n = bytes.len(37, "outputs")?;
        let mut outputs = Vec::with_capacity(n); // alloc: the answer's own, once a record read
        for _ in 0..n {
            let base = bytes.u8("output base")?;
            let path = bytes.text("output path")?;
            let content = bytes.id("output content")?;
            outputs.push(Output { base, path, content });
        }
        let n = bytes.len(32, "units")?;
        let mut units = Vec::with_capacity(n); // alloc: as above
        for _ in 0..n {
            units.push(bytes.id("unit")?);
        }
        let said = bytes.bytes("said")?.to_vec(); // alloc: as above
        if !bytes.rest.is_empty() {
            return Err(Undecoded::Malformed("the end"));
        }
        Ok(Answer { outputs, units, said })
    }
}

/// The reads the read-list `list` names, in its order; or why it is no certificate's list.
/// A list filed by a previous layout is refused by that layout's name.
pub fn reads_of(list: ListRef<'_>) -> Result<Vec<Read>, Undecoded> {
    let mut note = Bytes { rest: list.note() };
    note.head()?;
    if !note.rest.is_empty() {
        return Err(Undecoded::Malformed("the list's note"));
    }
    let mut reads = Vec::with_capacity(list.len()); // alloc: a list's reads, once a list read
    for name in list.reads() {
        reads.push(Read::named(name)?);
    }
    Ok(reads)
}

/// One build that answered: what it read, each with the digest it read it at, and what it
/// left.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Certificate {
    /// Sorted, each read once.
    reads: Vec<(Read, Digest)>,
    answer: Answer,
}

impl Certificate {
    /// The certificate of a build that read `reads` and left `answer`: the reads sorted, each
    /// once, and the outputs sorted, so equal builds are equal bytes whatever order their
    /// steps ran in. `Err`: a read the build saw with two values (it moved under the build),
    /// which no one digest stands for: that build has no certificate.
    pub fn new(mut reads: Vec<(Read, Digest)>, mut answer: Answer) -> Result<Certificate, Read> {
        reads.sort_by(|a, b| a.0.cmp(&b.0));
        for pair in reads.windows(2) {
            if pair[0].0 == pair[1].0 && pair[0].1 != pair[1].1 {
                return Err(pair[1].0.clone()); // clone: a refusal's name, once
            }
        }
        reads.dedup_by(|a, b| a.0 == b.0);
        answer.outputs.sort_by(|a, b| (a.base, &a.path).cmp(&(b.base, &b.path)));
        Ok(Certificate { reads, answer })
    }

    pub fn reads(&self) -> &[(Read, Digest)] {
        &self.reads
    }

    pub fn answer(&self) -> &Answer {
        &self.answer
    }

    /// Its read-list under `base`: the note is this layout's word, the reads their names.
    pub fn list(&self, base: &ContentId) -> Vec<u8> {
        let mut out = Vec::new(); // alloc: the list, once a build that answered
        blaze_record::write_list(
            &mut out,
            base,
            &head_of(LAYOUT),
            self.reads.iter().map(|(read, _)| read.name()),
        );
        out
    }

    /// The key its answer is under: `list` (its own, [`Certificate::list`]) and the digest
    /// each read had.
    pub fn answer_key(&self, list: &[u8]) -> ContentId {
        let mut key = AnswerKey::new(list);
        for (_, digest) in &self.reads {
            key.value(*digest);
        }
        key.finish()
    }
}

/// An output's stamp with the content hashed or placed under it, and when that stamp was
/// looked at (by the file system's clock): kept only while steady (not [`blaze_vfs::racy`]).
#[derive(Clone, PartialEq, Eq, Debug)]
struct Vouched {
    base: u8,
    path: String,
    stamp: Stamp,
    looked_ns: u64,
    content: ContentId,
}

/// A file output a hit was given on, under the stamp its content was checked at by that
/// check (vouched for, or hashed between two equal looks) and when: what the driver records
/// as placed there, so a build after it places nothing again that is as it was placed. Racy
/// or not: the record of a placement is compared with a later look whole, never trusted
/// for its content.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Verified {
    pub base: u8,
    pub path: String,
    pub stamp: Stamp,
    pub looked_ns: u64,
    pub content: ContentId,
}

/// What one machine knows beside the records of one checkout and request shape: never a
/// condition for an answer. Without it a check hashes each output once and a miss cannot
/// name what moved; nothing else differs. Its stamps outlive the daemon as the journal's
/// marks (blaze-cargo's `serve::local::Placed`, one an output path): the driver writes each entry
/// [`Evidence::drain_changed`] hands it and a new evidence is [`Evidence::seeded`] from them,
/// so the first check after a start hashes nothing that is as it was placed or hashed.
///
/// Bounded: `stamps` holds the outputs of one answer (the last given or written) and
/// whatever the marks seeded until the first answer trims it, `last` the reads of one build,
/// `changed` the entries changed since the last drain, `verified` the outputs of one hit. The
/// marks are bounded with it: a mark is written for an entry kept, and removed as its entry
/// leaves every evidence held (trimmed off an answer's outputs, forgotten, or its evidence
/// dropped), so they number at most what the evidence held keeps.
#[derive(Clone, Debug, Default)]
pub struct Evidence {
    /// The reads of the last build or hit of this checkout and shape, each with its digest
    /// then: what a miss is compared with to name the reads that moved.
    pub last: Option<Vec<(Read, Digest)>>,
    /// Sorted by base then path.
    stamps: Vec<Vouched>,
    /// Each entry [`Evidence::keep`] or [`Evidence::forget`] changed since the last
    /// [`Evidence::drain_changed`], by base and path, in the order changed.
    changed: Vec<(u8, String)>,
    /// The file outputs of the last hit ([`Verified`]); replaced at each hit.
    pub verified: Vec<Verified>,
}

impl Evidence {
    /// The evidence the placed outputs `placed` vouch for, every output under `base`: each an
    /// output's path, the stamp it was placed or hashed under, when that stamp was looked at
    /// and the content under it (the driver's decoded marks, blaze-cargo's
    /// `serve::local::Placed`, or a slot's [`crate::slot::Placed`]). Each is kept as
    /// [`Evidence::keep`] keeps, so one looked at inside its racy window vouches for nothing.
    /// Nothing is changed by it ([`Evidence::drain_changed`] hands nothing). A mark the driver
    /// could not decode is the driver's to say and remove: it is not handed here.
    pub fn seeded<'a>(base: u8, placed: impl Iterator<Item = (&'a str, Stamp, u64, ContentId)>) -> Evidence {
        let mut evidence = Evidence::default();
        for (path, stamp, looked_ns, content) in placed {
            evidence.keep(base, path, stamp, content, looked_ns);
        }
        evidence.changed.clear();
        evidence
    }

    fn at(&self, base: u8, path: &str) -> Result<usize, usize> {
        self.stamps.binary_search_by(|it| (it.base, it.path.as_str()).cmp(&(base, path)))
    }

    /// The content the output at `path` under `base` holds, where its stamp `now` is, whole
    /// (inode, size, modified and change times, mode), the one that content was hashed or
    /// placed under.
    pub fn vouched(&self, base: u8, path: &str, now: &Stamp) -> Option<ContentId> {
        let at = self.at(base, path).ok()?;
        (self.stamps[at].stamp == *now).then_some(self.stamps[at].content)
    }

    /// Keeps that the output at `path` under `base` held `content` under `stamp`, looked at
    /// at `looked_ns` (the file system's clock): only where the stamp was steady (not
    /// [`blaze_vfs::racy`], the one racy window), since within its window another write can
    /// leave the same stamp. What was kept of the path before goes either way.
    pub fn keep(&mut self, base: u8, path: &str, stamp: Stamp, content: ContentId, looked_ns: u64) {
        let steady = !blaze_vfs::racy(&stamp, looked_ns);
        self.keep_if(base, path, stamp, content, looked_ns, steady);
    }

    fn keep_if(&mut self, base: u8, path: &str, stamp: Stamp, content: ContentId, looked_ns: u64, steady: bool) {
        let changed = match (self.at(base, path), steady) {
            (Ok(at), true) => {
                let it = &mut self.stamps[at];
                let moved = it.stamp != stamp || it.content != content;
                if moved {
                    (it.stamp, it.looked_ns, it.content) = (stamp, looked_ns, content);
                }
                moved
            }
            (Ok(at), false) => {
                self.stamps.remove(at);
                true
            }
            (Err(at), true) => {
                // alloc: one entry an output of the shape's last answer
                self.stamps.insert(at, Vouched { base, path: path.to_owned(), stamp, looked_ns, content });
                true
            }
            (Err(_), false) => false,
        };
        if changed {
            self.changed.push((base, path.to_owned())); // alloc: one entry an output changed, until drained
        }
    }

    /// Forgets what was kept of the output at `path` under `base`.
    pub fn forget(&mut self, base: u8, path: &str) {
        if let Ok(at) = self.at(base, path) {
            self.stamps.remove(at);
            self.changed.push((base, path.to_owned())); // alloc: as `keep_if`'s
        }
    }

    /// Keeps only the stamps of `outputs` (sorted by base then path, as an [`Answer`]'s are):
    /// the bound. Each entry it drops is changed ([`Evidence::drain_changed`] hands it as
    /// kept no more), so its mark goes with it where no other evidence the driver holds keeps
    /// that path: the marks are bounded by what the evidence held keeps, never by every path
    /// an output was ever placed at.
    pub fn only(&mut self, outputs: &[Output]) {
        let changed = &mut self.changed;
        self.stamps.retain(|it| {
            let kept = outputs
                .binary_search_by(|out| (out.base, out.path.as_str()).cmp(&(it.base, it.path.as_str())))
                .is_ok();
            if !kept {
                // clone: the path of an entry dropped, until drained
                changed.push((it.base, it.path.clone()));
            }
            kept
        });
    }

    /// Whether anything is kept of the output at `path` under `base`.
    pub fn holds(&self, base: u8, path: &str) -> bool {
        self.at(base, path).is_ok()
    }

    /// Every entry kept, by base and path, sorted.
    pub fn entries(&self) -> impl Iterator<Item = (u8, &str)> {
        self.stamps.iter().map(|it| (it.base, it.path.as_str()))
    }

    /// Hands `f` each entry changed since the last call, once each, in the order first
    /// changed: its base, its path and what is kept of it now (its stamp, when that was looked
    /// at, its content), `None` where nothing is (forgotten, or kept racy). What the driver
    /// writes as marks, or removes.
    pub fn drain_changed(&mut self, mut f: impl FnMut(u8, &str, Option<(&Stamp, u64, &ContentId)>)) {
        let mut changed = std::mem::take(&mut self.changed);
        let mut seen: Vec<(u8, &str)> = Vec::with_capacity(changed.len()); // alloc: once a drain, an entry a change
        for (base, path) in &changed {
            if seen.contains(&(*base, path.as_str())) {
                continue;
            }
            seen.push((*base, path.as_str()));
            let kept = self.at(*base, path).ok().map(|at| {
                let it = &self.stamps[at];
                (&it.stamp, it.looked_ns, &it.content)
            });
            f(*base, path, kept);
        }
        drop(seen);
        changed.clear();
        self.changed = changed;
    }

    /// As [`Evidence::keep`] with no regard for the racy window: the planted bug of the racy
    /// stamp's control. For tests (`plants`).
    #[cfg(any(test, feature = "plants"))]
    pub fn keep_racy(&mut self, base: u8, path: &str, stamp: Stamp, content: ContentId) {
        self.keep_if(base, path, stamp, content, u64::MAX, true);
    }
}

#[cfg(test)]
mod tests;
