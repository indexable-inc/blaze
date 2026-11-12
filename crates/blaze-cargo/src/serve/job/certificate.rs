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
//! each output was last seen to have with the content under it (remade by hashing the output
//! once where missing), and the digests of the checkout's last build, which is how a miss
//! names the reads that moved.
//!
//! The check and the write are the request machine's ([`super::request`]).
//!
//! The invariant: a certificate is given only when a fresh build would produce the same
//! outputs and say the same bytes.
//!
//! A sans-IO core (rule 14): no file, clock, watch or store is read here.

// Until `Jobs::begin` drives the request machine (certificate.md, "State").
#![allow(dead_code)]

use std::fmt;

use blaze_ids::{ContentId, Hasher};
use blaze_record::{AnswerKey, Digest, ListRef};

/// The layout of a certificate's parts: the word in each read-list's note and at the head of
/// each answer. A change of any field's meaning or order takes the next number and a line in
/// [`PREVIOUS`].
pub(super) const LAYOUT: u32 = 2;

/// The first bytes of a list's note and of every answer.
const MAGIC: &[u8; 8] = b"blzcert\n";

/// The layouts before [`LAYOUT`], each by name: a part of one is refused by that name
/// ([`Undecoded::Previous`]), and the build that follows files the record anew (rule 26).
pub(super) const PREVIOUS: &[(u32, &str)] = &[
    (0, "the draft of 2026-11-12, whose one group was the view's whole root"),
    (1, "the record of 2026-11-12 keyed by the view's root, with its outputs' stamps in its bytes"),
];

/// The step a request that was not certified is under in `blaze why`.
pub(super) const STEP: &str = "request certificate";

/// What stands for the checkout's own directory in everything a certificate is made of
/// ([`relocated`]): no absolute path of a checkout is in a key or a record.
pub(super) const ROOT: &str = "\u{0}checkout";

/// How long after an output's change time a look must be for its stamp to vouch for the
/// content hashed under it: within this a second write can leave the same stamp (the
/// coarsest change time of the file systems blaze places outputs on is two seconds, FAT's).
/// Until then the output is hashed at each check; never trusted early.
pub(super) const RACY_NS: u64 = 2_000_000_000;

/// `text` with the checkout's directory `root` (absolute, no trailing `/`) replaced by
/// [`ROOT`] wherever it is a whole path prefix: at the start of `text` or after a character
/// no path holds in its middle of a word (`=`, `:`, a space, a quote, a comma), and followed
/// by `/`, one of those characters or the end. What a request's config is made a shape of and what a build said is
/// kept as, so two checkouts of one content ask for one certificate ([`replaced`] undoes it).
pub(super) fn relocated(text: &str, root: &str) -> String {
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
pub(super) fn replaced(text: &str, root: &str) -> String {
    text.replace(ROOT, root) // alloc: once a string said again
}

/// A request's identity, each part relocated: none holds the tree's content.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Shape {
    /// Its config as the plan reads it.
    pub(super) plan: ContentId,
    /// Its config less its environment.
    pub(super) config: ContentId,
    /// What it says past its config (verbosity, the goal).
    pub(super) request: ContentId,
}

/// The base key of the certificates of requests of `shape` served by `tool` (the blaze's own
/// identity; the toolchain is in the shape's config, its mounts by content id). An identity:
/// every checkout, of whatever content, asks under the one base its shape and tool give.
pub(super) fn base(shape: &Shape, tool: &[u8]) -> ContentId {
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
pub(super) enum Read {
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
    /// How the request's client is shown diagnostics: read only by a build that said one.
    Drawn,
}

impl Read {
    /// Its kind's number and its text: what its name is made of, and what orders reads.
    pub(super) fn parts(&self) -> (u8, &str) {
        match self {
            Read::Node(path) => (0, path.as_str()),
            Read::Env(name) => (1, name.as_str()),
            Read::Outside(path) => (2, path.as_str()),
            Read::Listing(path) => (3, path.as_str()),
            Read::Drawn => (4, ""),
            Read::Tree(path) => (5, path.as_str()),
            Read::Package(path) => (6, path.as_str()),
        }
    }

    /// Its name in a read-list: its kind, then its text.
    pub(super) fn name(&self) -> Vec<u8> {
        let (kind, text) = self.parts();
        let mut out = Vec::with_capacity(1 + text.len()); // alloc: a read's name, once a list written
        out.push(kind);
        out.extend_from_slice(text.as_bytes());
        out
    }

    /// The read `name` is of ([`Read::name`]), or why it is none.
    pub(super) fn named(name: &[u8]) -> Result<Read, Undecoded> {
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
            Read::Drawn => f.write_str("how diagnostics are shown"),
        }
    }
}

/// An output of the build, by content.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Output {
    /// Which of the daemon's bases `path` is under (the target directory, the build
    /// directory, the standard library's builds), by index.
    pub(super) base: u8,
    /// From that base, `/`-separated.
    pub(super) path: String,
    /// The hash of the bytes placed there.
    pub(super) content: ContentId,
}

/// What a certified request is answered with: the value of the record.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(super) struct Answer {
    /// The outputs, sorted by base then path.
    pub(super) outputs: Vec<Output>,
    /// The units' record keys, in the plan's order: what `blaze why` reads of an answered
    /// request.
    pub(super) units: Vec<ContentId>,
    /// What the build said, as a build with nothing to do says it, and its `Work`: the
    /// daemon's encoding (deterministic CBOR), every text of it [`relocated`]; opaque here.
    pub(super) said: Vec<u8>,
}

/// Why a certificate's bytes are not read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Undecoded {
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
    pub(super) fn encode(&self) -> Vec<u8> {
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
    pub(super) fn decode(bytes: &[u8]) -> Result<Answer, Undecoded> {
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
pub(super) fn reads_of(list: ListRef<'_>) -> Result<Vec<Read>, Undecoded> {
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
pub(super) struct Certificate {
    /// Sorted, each read once.
    reads: Vec<(Read, Digest)>,
    answer: Answer,
}

impl Certificate {
    /// The certificate of a build that read `reads` and left `answer`: the reads sorted, each
    /// once, and the outputs sorted, so equal builds are equal bytes whatever order their
    /// steps ran in. `Err`: a read the build saw with two values (it moved under the build),
    /// which no one digest stands for: that build has no certificate.
    pub(super) fn new(mut reads: Vec<(Read, Digest)>, mut answer: Answer) -> Result<Certificate, Read> {
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

    pub(super) fn reads(&self) -> &[(Read, Digest)] {
        &self.reads
    }

    pub(super) fn answer(&self) -> &Answer {
        &self.answer
    }

    /// Its read-list under `base`: the note is this layout's word, the reads their names.
    pub(super) fn list(&self, base: &ContentId) -> Vec<u8> {
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
    pub(super) fn answer_key(&self, list: &[u8]) -> ContentId {
        let mut key = AnswerKey::new(list);
        for (_, digest) in &self.reads {
            key.value(*digest);
        }
        key.finish()
    }
}

/// What a look at a placed file says: its length and when it last changed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Stamp {
    pub(super) len: u64,
    pub(super) changed_ns: u64,
}

/// An output's stamp with the content hashed under it.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Vouched {
    base: u8,
    path: String,
    stamp: Stamp,
    content: ContentId,
}

/// What one machine knows beside the records of one checkout and request shape: never
/// stored, never a condition for an answer. Without it a check hashes each output once and a
/// miss cannot name what moved; nothing else differs.
///
/// Bounded: `stamps` holds the outputs of one answer (the last given or written), `last` the
/// reads of one build.
#[derive(Clone, Debug, Default)]
pub(super) struct Evidence {
    /// The reads of the last build or hit of this checkout and shape, each with its digest
    /// then: what a miss is compared with to name the reads that moved.
    pub(super) last: Option<Vec<(Read, Digest)>>,
    /// Sorted by base then path.
    stamps: Vec<Vouched>,
}

impl Evidence {
    fn at(&self, base: u8, path: &str) -> Result<usize, usize> {
        self.stamps.binary_search_by(|it| (it.base, it.path.as_str()).cmp(&(base, path)))
    }

    /// The content the output at `path` under `base` holds, where its stamp `now` is the one
    /// that content was hashed under.
    pub(super) fn vouched(&self, base: u8, path: &str, now: Stamp) -> Option<ContentId> {
        let at = self.at(base, path).ok()?;
        (self.stamps[at].stamp == now).then_some(self.stamps[at].content)
    }

    /// Keeps that the output at `path` under `base` held `content` under `stamp`, seen at
    /// `looked_ns` (the file system's clock): only where the look is [`RACY_NS`] past the
    /// stamp's change time, since within it another write can leave the same stamp. What was
    /// kept of the path before goes either way.
    pub(super) fn keep(&mut self, base: u8, path: &str, stamp: Stamp, content: ContentId, looked_ns: u64) {
        let steady = stamp.changed_ns.checked_add(RACY_NS).is_some_and(|it| it <= looked_ns);
        match (self.at(base, path), steady) {
            (Ok(at), true) => {
                self.stamps[at].stamp = stamp;
                self.stamps[at].content = content;
            }
            (Ok(at), false) => {
                self.stamps.remove(at);
            }
            (Err(at), true) => {
                // alloc: one entry an output of the shape's last answer
                self.stamps.insert(at, Vouched { base, path: path.to_owned(), stamp, content });
            }
            (Err(_), false) => {}
        }
    }

    /// Forgets what was kept of the output at `path` under `base`.
    pub(super) fn forget(&mut self, base: u8, path: &str) {
        if let Ok(at) = self.at(base, path) {
            self.stamps.remove(at);
        }
    }

    /// Keeps only the stamps of `outputs` (sorted by base then path, as an [`Answer`]'s are):
    /// the bound.
    pub(super) fn only(&mut self, outputs: &[Output]) {
        self.stamps.retain(|it| {
            outputs
                .binary_search_by(|out| (out.base, out.path.as_str()).cmp(&(it.base, it.path.as_str())))
                .is_ok()
        });
    }

    /// As [`Evidence::keep`] with no regard for [`RACY_NS`]: the planted bug of the racy
    /// stamp's control. For tests.
    #[cfg(test)]
    pub(super) fn keep_racy(&mut self, base: u8, path: &str, stamp: Stamp, content: ContentId) {
        self.keep(base, path, stamp, content, u64::MAX);
    }
}

#[cfg(test)]
mod tests;
