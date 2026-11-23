//! The library-features record (rule 77): what blaze-scope's `lib_features` walk lists for one
//! side of a world, kept so that no command lexes the toolchain's crates again once a record
//! holds the answer (script_setting.rs `feature_table`: a hit reads the record, a miss walks,
//! writes and answers).
//!
//! THE KEY ([`features_key`]) is computed from the world before the walk and covers every input
//! the walk reads (rule 21):
//! - the tool: the caller's identity of blaze's build, so the walk's own code (rule 52);
//! - the triple, which the rows are keyed for (`probe_ask::feature_rows`);
//! - every shelf tree by its index and its pack's BLAKE3 (the footer
//!   `blaze_toolchain::shelf::open_pack` checked against the toolchain's manifest, so the
//!   toolchain's id names each), and every corrected file in such a tree by its path and its
//!   text's id. The walk reads through [`ShelfTexts`], which answers only these trees: a read
//!   the key does not cover is never made (a path into another tree is the walk's
//!   `ScanError::Missing`, naming it);
//! - each phase ([`phases`]: core's closure, then std's) and in it each crate the walk visits,
//!   in its order: the crate's root path (which names its tree), its cfg atoms and its features.
//!   Whether a crate is read (staged) is read from its root's text, which its tree's digest
//!   covers. A crate of a closure that is not in a shelf tree is
//!   [`FeatureKeyError::NotOnShelf`]: its text has no digest here, so no key could cover it.
//!
//! THE FORM (integers little-endian; one listing makes one byte string, rule 25):
//! - the header, [`HEADER`] bytes: `BZLF`, the version ([`VERSION`]), the record's own key (a
//!   record read under another key is refused), the count of phases;
//! - one row per phase, [`PHASE_ROW`] bytes: its needs (0 core, 1 std), the count of declared
//!   names, the count of mentioned names;
//! - the names, phase by phase, declared then mentioned: each a u32 length and its UTF-8 bytes.
//!
//! Each list is sorted and holds each name once, and no mentioned name is declared
//! (`LibFeatures::listed`'s promise); a record that breaks any of it, ends early, has bytes
//! after its last name or is of another version is [`FeatureRecordError`], naming the record
//! and the byte: never a quiet walk again (rule 3).

use core::fmt;

use blaze_build_script::probe_ask::Needs;
use blaze_id::ContentId;
use blaze_own::{AVec, Alloc};
use blaze_scope::resolve::SourceTexts;

use crate::script_setting::toolchain_of;
use crate::{PackOrigin, Side, Texts, World};

const MAGIC: [u8; 4] = *b"BZLF";
/// 1: the first form.
pub const VERSION: u32 = 1;
/// Magic, version, key, phase count.
pub const HEADER: usize = 4 + 4 + 32 + 4;
/// Needs, declared count, mentioned count.
pub const PHASE_ROW: usize = 12;
/// A listing has at most a phase per [`Needs`].
const MOST_PHASES: u32 = 2;

const KEY_DOMAIN: &str = "blaze-world library features key 1";
const CORRECTED_DOMAIN: &str = "blaze-world library features corrected text 1";

/// Where the records are kept: the store as the caller holds it (sans-IO; blaze-cli's command
/// store, a test's [`FeatureMemory`]).
pub trait FeatureRecords {
    type Error: fmt::Display + fmt::Debug;
    /// The record at `key`, read in place; `None` when the store holds none.
    fn get(&mut self, key: ContentId) -> Result<Option<&[u8]>, Self::Error>;
    /// `bytes` at `key`. Equal bytes already held are left as they are; other bytes are the
    /// store's refusal (rule 21).
    fn put(&mut self, key: ContentId, bytes: &[u8]) -> Result<(), Self::Error>;
}

/// Records held in the caller's allocator for as long as the value lives.
pub struct FeatureMemory<'a> {
    alloc: Alloc<'a>,
    held: AVec<'a, (ContentId, AVec<'a, u8>)>,
}

impl<'a> FeatureMemory<'a> {
    pub fn new(alloc: Alloc<'a>) -> FeatureMemory<'a> {
        FeatureMemory { held: AVec::new_in(alloc), alloc }
    }

    /// How many records it holds.
    pub fn len(&self) -> usize {
        self.held.len()
    }
}

/// [`FeatureMemory`] was given other bytes for a key it holds.
#[derive(Debug)]
pub struct FeatureMemoryError {
    pub key: ContentId,
}

impl fmt::Display for FeatureMemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the record {:x} is held with other bytes (rule 21: the key misses an input)", self.key)
    }
}

impl core::error::Error for FeatureMemoryError {}

impl FeatureRecords for FeatureMemory<'_> {
    type Error = FeatureMemoryError;

    fn get(&mut self, key: ContentId) -> Result<Option<&[u8]>, FeatureMemoryError> {
        Ok(self.held.iter().find(|(held, _)| *held == key).map(|(_, bytes)| &bytes[..]))
    }

    fn put(&mut self, key: ContentId, bytes: &[u8]) -> Result<(), FeatureMemoryError> {
        if let Some((_, held)) = self.held.iter().find(|(held, _)| *held == key) {
            return if held[..] == *bytes { Ok(()) } else { Err(FeatureMemoryError { key }) };
        }
        self.held.push((key, AVec::from_slice_in(bytes, self.alloc)));
        Ok(())
    }
}

/// One phase of the walk: the crate a probe of no items loads (`needs`) and the crates its
/// closure adds, by index in the world, in the order the walk visits them. A crate an earlier
/// phase visited is not visited again (std's closure holds core's).
#[derive(Debug)]
pub struct Phase<'a> {
    pub needs: Needs,
    pub crates: AVec<'a, usize>,
}

/// The walk's phases for `side`: for core, then std, when the side has it, every toolchain
/// crate its dependencies reach (the crates rustc's `lib_features` reads). The one home of the
/// visit order: the key ([`features_key`]) and the walk (script_setting.rs) both take it.
pub fn phases<'a>(world: &World<'_>, side: Side, alloc: Alloc<'a>) -> AVec<'a, Phase<'a>> {
    let mut out = AVec::new_in(alloc);
    let mut seen: AVec<'a, bool> = AVec::new_in(alloc);
    seen.resize(world.crates.len(), false);
    for needs in [Needs::Core, Needs::Std] {
        let Some(root) = world.crates.iter().position(|krate| toolchain_of(krate, side) && krate.input.name == needs.krate()) else { continue };
        let mut crates = AVec::new_in(alloc);
        let mut stack: AVec<'a, usize> = AVec::new_in(alloc);
        stack.push(root);
        while let Some(at) = stack.pop() {
            if core::mem::replace(&mut seen[at], true) {
                continue;
            }
            crates.push(at);
            stack.extend(world.crates[at].input.deps.iter().map(|(_, dep)| *dep));
        }
        out.push(Phase { needs, crates });
    }
    out
}

/// What one phase lists: both lists sorted, each name once, no mentioned name declared.
#[derive(Debug)]
pub struct PhaseListing<'n, 'a> {
    pub needs: Needs,
    pub declared: AVec<'a, &'n str>,
    pub mentioned: AVec<'a, &'n str>,
}

/// The world's texts as the walk may read them: the shelf's trees alone (the trees the key
/// covers, [`features_key`]); a path into any other tree reads as no file.
pub struct ShelfTexts<'r, 'w> {
    texts: &'r Texts<'w>,
}

impl<'r, 'w> ShelfTexts<'r, 'w> {
    pub fn new(texts: &'r Texts<'w>) -> ShelfTexts<'r, 'w> {
        ShelfTexts { texts }
    }
}

impl<'w> SourceTexts<'w> for ShelfTexts<'_, 'w> {
    fn read(&self, krate: usize, path: &str) -> Option<&'w str> {
        shelf_tree(self.texts, path)?;
        self.texts.read(krate, path)
    }
}

/// The tree `path` (`/<tree>/...`) is in, when that tree is a shelf pack's.
fn shelf_tree(texts: &Texts<'_>, path: &str) -> Option<usize> {
    let (tree, _) = path.strip_prefix('/')?.split_once('/')?;
    let tree: usize = tree.parse().ok()?;
    match texts.origins().get(tree.checked_sub(1)?)? {
        PackOrigin::Shelf { .. } => Some(tree),
        PackOrigin::Source { .. } => None,
    }
}

/// No key covers the walk of this side: a crate of a closure whose root is in no shelf tree.
#[derive(Debug)]
pub enum FeatureKeyError<'w> {
    NotOnShelf { krate: &'w str, root: &'w str },
}

impl fmt::Display for FeatureKeyError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FeatureKeyError::NotOnShelf { krate, root } => {
                write!(f, "library features: the toolchain crate `{krate}` has its root {root} in no shelf tree, so no key covers its text (rule 21)")
            }
        }
    }
}

impl core::error::Error for FeatureKeyError<'_> {}

/// The key of the listing of `phases` (made by [`phases`] from `world`) for `triple` by `tool`:
/// see the module's words for what it covers. Made from the world alone: no file is read.
pub fn features_key<'w>(world: &World<'w>, phases: &[Phase<'_>], triple: &str, tool: ContentId, alloc: Alloc<'_>) -> Result<ContentId, FeatureKeyError<'w>> {
    let mut bytes: AVec<'_, u8> = AVec::new_in(alloc);
    bytes.extend_from_slice(&tool.0);
    part(&mut bytes, triple.as_bytes());
    part(&mut bytes, b"trees");
    for (at, origin) in world.texts.origins().iter().enumerate() {
        if let PackOrigin::Shelf { digest } = origin {
            bytes.extend_from_slice(&(at as u64 + 1).to_le_bytes());
            bytes.extend_from_slice(digest);
        }
    }
    part(&mut bytes, b"corrected");
    for (tree, path, text) in world.texts.corrected() {
        let shelf = (*tree as usize).checked_sub(1).and_then(|at| world.texts.origins().get(at)).is_some_and(|origin| matches!(origin, PackOrigin::Shelf { .. }));
        if !shelf {
            continue;
        }
        bytes.extend_from_slice(&tree.to_le_bytes());
        part(&mut bytes, path.as_bytes());
        bytes.extend_from_slice(&ContentId::of(CORRECTED_DOMAIN, text.as_bytes()).0);
    }
    for phase in phases {
        part(&mut bytes, phase.needs.krate().as_bytes());
        bytes.extend_from_slice(&(phase.crates.len() as u64).to_le_bytes());
        for &at in phase.crates.iter() {
            let input = &world.crates[at].input;
            if shelf_tree(&world.texts, input.root).is_none() {
                return Err(FeatureKeyError::NotOnShelf { krate: input.name, root: input.root });
            }
            part(&mut bytes, input.root.as_bytes());
            bytes.extend_from_slice(&(input.cfg.len() as u64).to_le_bytes());
            for (name, value) in input.cfg {
                part(&mut bytes, name.as_bytes());
                match value {
                    None => bytes.push(0),
                    Some(value) => {
                        bytes.push(1);
                        part(&mut bytes, value.as_bytes());
                    }
                }
            }
            bytes.extend_from_slice(&(input.features.len() as u64).to_le_bytes());
            for feature in input.features {
                part(&mut bytes, feature.as_bytes());
            }
        }
    }
    Ok(ContentId::of(KEY_DOMAIN, &bytes))
}

/// A length-prefixed part, so `("ab", "c")` and `("a", "bc")` are two keys.
fn part(bytes: &mut AVec<'_, u8>, part: &[u8]) {
    bytes.extend_from_slice(&(part.len() as u64).to_le_bytes());
    bytes.extend_from_slice(part);
}

/// Why a library-features record is not read (or not written): the record's key, the byte,
/// what is wrong.
#[derive(Debug)]
pub struct FeatureRecordError {
    pub key: ContentId,
    pub why: FeatureRecordWhy,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FeatureRecordWhy {
    /// It ends before byte `at` of what it states.
    Short { at: usize },
    /// It does not start `BZLF`.
    Magic,
    /// Another form: an old home's, reported and made again by whoever removes it (rule 26).
    Version { found: u32 },
    /// It states another key: the store's bytes under `key` are not this key's record.
    Key { found: ContentId },
    /// More phases than [`Needs`] has.
    Phases { count: u32 },
    /// A phase's needs is no [`Needs`], or the phases are not core then std, each once.
    Needs { at: usize, found: u32 },
    /// A name that is not UTF-8.
    Utf8 { at: usize },
    /// A phase's list is not sorted with each name once, or a mentioned name is declared.
    Order { needs: Needs },
    /// Bytes after the last name.
    Trailing { at: usize },
    /// A count or a name too long for its u32 (when it is written).
    TooLarge,
}

impl fmt::Display for FeatureRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the library-features record {:x}: ", self.key)?;
        match &self.why {
            FeatureRecordWhy::Short { at } => write!(f, "it ends before byte {at}"),
            FeatureRecordWhy::Magic => write!(f, "it is not a library-features record (no `BZLF`)"),
            FeatureRecordWhy::Version { found } => write!(f, "form {found}, this blaze reads form {VERSION}: an old home's record (rule 26)"),
            FeatureRecordWhy::Key { found } => write!(f, "it states the key {found:x}"),
            FeatureRecordWhy::Phases { count } => write!(f, "{count} phases, at most {MOST_PHASES}"),
            FeatureRecordWhy::Needs { at, found } => write!(f, "byte {at}: phase {found} is not core then std, each once"),
            FeatureRecordWhy::Utf8 { at } => write!(f, "byte {at}: a name that is not UTF-8"),
            FeatureRecordWhy::Order { needs } => write!(f, "the {} phase's names are not sorted, once each, mentioned apart from declared", needs.krate()),
            FeatureRecordWhy::Trailing { at } => write!(f, "bytes after the last name, from byte {at}"),
            FeatureRecordWhy::TooLarge => write!(f, "a count or a name past a u32"),
        }
    }
}

impl core::error::Error for FeatureRecordError {}

fn needs_code(needs: Needs) -> u32 {
    match needs {
        Needs::Core => 0,
        Needs::Std => 1,
    }
}

fn u32_of(n: usize, key: ContentId) -> Result<u32, FeatureRecordError> {
    u32::try_from(n).map_err(|_| FeatureRecordError { key, why: FeatureRecordWhy::TooLarge })
}

/// The record of `listed` under `key`, in `alloc`.
pub fn encode<'a>(key: ContentId, listed: &[PhaseListing<'_, '_>], alloc: Alloc<'a>) -> Result<AVec<'a, u8>, FeatureRecordError> {
    let names: usize = listed.iter().flat_map(|phase| phase.declared.iter().chain(phase.mentioned.iter())).map(|name| 4 + name.len()).sum();
    let mut out: AVec<'a, u8> = AVec::with_capacity_in(HEADER + PHASE_ROW * listed.len() + names, alloc);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&key.0);
    out.extend_from_slice(&u32_of(listed.len(), key)?.to_le_bytes());
    for phase in listed {
        out.extend_from_slice(&needs_code(phase.needs).to_le_bytes());
        out.extend_from_slice(&u32_of(phase.declared.len(), key)?.to_le_bytes());
        out.extend_from_slice(&u32_of(phase.mentioned.len(), key)?.to_le_bytes());
    }
    for phase in listed {
        for name in phase.declared.iter().chain(phase.mentioned.iter()) {
            out.extend_from_slice(&u32_of(name.len(), key)?.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
        }
    }
    Ok(out)
}

/// Reads a record's bytes from the front, each step checked against its end.
struct Cursor<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Cursor<'b> {
    fn take(&mut self, n: usize) -> Result<&'b [u8], FeatureRecordWhy> {
        let end = self.at.checked_add(n).filter(|end| *end <= self.bytes.len()).ok_or(FeatureRecordWhy::Short { at: self.at.saturating_add(n) })?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn u32(&mut self) -> Result<u32, FeatureRecordWhy> {
        let mut word = [0u8; 4];
        word.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(word))
    }

    fn name(&mut self) -> Result<&'b str, FeatureRecordWhy> {
        let len = self.u32()? as usize;
        let at = self.at;
        core::str::from_utf8(self.take(len)?).map_err(|_| FeatureRecordWhy::Utf8 { at })
    }

    /// `count` names: refused before anything is held when the bytes left cannot hold them.
    fn names<'a>(&mut self, count: u32, alloc: Alloc<'a>) -> Result<AVec<'a, &'b str>, FeatureRecordWhy> {
        let count = count as usize;
        if count.saturating_mul(4) > self.bytes.len() - self.at {
            return Err(FeatureRecordWhy::Short { at: self.at.saturating_add(count.saturating_mul(4)) });
        }
        let mut out = AVec::with_capacity_in(count, alloc);
        for _ in 0..count {
            out.push(self.name()?);
        }
        Ok(out)
    }
}

/// The listing the record `bytes` holds, read in place (each name borrows `bytes`), checked to
/// be the record of `key` in this form.
pub fn decode<'b, 'a>(key: ContentId, bytes: &'b [u8], alloc: Alloc<'a>) -> Result<AVec<'a, PhaseListing<'b, 'a>>, FeatureRecordError> {
    decoded(key, bytes, alloc).map_err(|why| FeatureRecordError { key, why })
}

fn decoded<'b, 'a>(key: ContentId, bytes: &'b [u8], alloc: Alloc<'a>) -> Result<AVec<'a, PhaseListing<'b, 'a>>, FeatureRecordWhy> {
    let mut cursor = Cursor { bytes, at: 0 };
    if cursor.take(4)? != MAGIC {
        return Err(FeatureRecordWhy::Magic);
    }
    let version = cursor.u32()?;
    if version != VERSION {
        return Err(FeatureRecordWhy::Version { found: version });
    }
    let mut found = [0u8; 32];
    found.copy_from_slice(cursor.take(32)?);
    if found != key.0 {
        return Err(FeatureRecordWhy::Key { found: ContentId(found) });
    }
    let count = cursor.u32()?;
    if count > MOST_PHASES {
        return Err(FeatureRecordWhy::Phases { count });
    }
    let mut rows: AVec<'_, (Needs, u32, u32)> = AVec::with_capacity_in(count as usize, alloc);
    let mut next = 0;
    for _ in 0..count {
        let at = cursor.at;
        let code = cursor.u32()?;
        let needs = match code {
            0 => Needs::Core,
            1 => Needs::Std,
            _ => return Err(FeatureRecordWhy::Needs { at, found: code }),
        };
        if code < next {
            return Err(FeatureRecordWhy::Needs { at, found: code });
        }
        next = code + 1;
        rows.push((needs, cursor.u32()?, cursor.u32()?));
    }
    let mut out = AVec::with_capacity_in(rows.len(), alloc);
    for (needs, declared, mentioned) in rows.iter().copied() {
        let declared = cursor.names(declared, alloc)?;
        let mentioned = cursor.names(mentioned, alloc)?;
        let once = |names: &[&str]| names.windows(2).all(|pair| pair[0] < pair[1]);
        if !once(&declared[..]) || !once(&mentioned[..]) || mentioned.iter().any(|name| declared.binary_search(name).is_ok()) {
            return Err(FeatureRecordWhy::Order { needs });
        }
        out.push(PhaseListing { needs, declared, mentioned });
    }
    if cursor.at != bytes.len() {
        return Err(FeatureRecordWhy::Trailing { at: cursor.at });
    }
    Ok(out)
}
