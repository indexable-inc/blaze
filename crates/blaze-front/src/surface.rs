//! `Task::Surface` (docs/v2/plan/design/host.md, "blaze-front"; read-addressed.md): a unit's
//! read-addressed `Surface` record, what `blaze_keyed::front::Front::surface` returns, and the
//! reading of a crate from one.
//!
//! **The record**, written by `record::write_read`, so its head is its address (its key, the
//! unit's own pieces, and the answers its shape got):
//!
//! - `reads`, the shape: one ask per surface of the task's universe (every dependency,
//!   transitively): that surface's exports row ([`facts::exports_id`]) with the digest
//!   `Universe::answer` gave; and one ask per late read the crate's expansions made
//!   (`record::late_id`: `env!`, `option_env!`, `include!`, `include_str!`,
//!   `include_bytes!`), with the digest of the value seen, which must be the answer
//!   ([`surface`]). Sorted by id. Nothing else of the unit's own: its pieces are its key.
//! - `facts`: one row per fact the crate's surface holds ([`exports`]), each id by
//!   `record::fact` with its owner (the crate's own for its items, `Owner::ASKER` for the build
//!   facts every surface holds), sorted by id, each once, so an ask is one binary search in
//!   mapped bytes. A fact that does not exist has no row and every side answers it by
//!   `record::absent`. Two rows of the surface's own: its exports row (the digest of every
//!   other item row) and its seen row (`record::seen_id`: the digest of its exports and of
//!   every surface it read), which answers a listing across crates its unit's bodies ask. The
//!   facts a body makes (a const's value, a hidden type) are no rows here: their makers' records
//!   hold them.
//! - `payload`: a blaze-format buffer of its own, of root [`Sections`], so every section is
//!   16-aligned in place: [`META`] (the crate's key), [`IMAGE`] (the sealed image as
//!   `hir_def::deps::write_image` writes it, under the record's address: its lang items and its
//!   signatures), [`PAGES`] (the def map pages, `hir_def::nameres::frozen::pages::encode`) and
//!   [`TABLES`] (hir-ty's impl tables, `hir_ty::method_resolution::frozen::encode`). No second
//!   format: each section is the bytes the sealed-image writers write today. The payload opens
//!   in place because `record::write_read` starts it 16-aligned in the record
//!   (`blaze_format::Writer::push_buffer`); [`surface`] opens its own output once as the
//!   invariant's check and refuses ([`UNALIGNED`]) if it does not.
//!
//! **Reading** ([`Opened`]). A crate is opened from its record the way a sealed crate is read:
//! its image, pages and impl tables given to the crate's own arena as the snapshot is built
//! (`hir_def::deps::give_image`, `hir_ty::method_resolution::frozen::give`), each a range of the
//! record held where the store keeps it (`hir_def::deps::Image::in_place`), so a reader of one
//! signature pages in one signature.
//!
//! **Refused**, by `hir_def::deps::image_parts`'s refusal's name (`image_refused`): what no
//! key from text and no fact of a shape covers. Real crates reach two of them: a procedural
//! macro whose expansion traced reading a variable or a file ([`IMAGE_UNASKED`]), and a file
//! outside the VFS the crate read ([`IMAGE_UNASKED`]); see `body::front`'s list, item 8.
//!
//! **Floor (rule 67).** A surface task's floor is the crate's own scopes once plus one lookup per
//! fact it asks of a dependency. Here: the crate's scopes, signatures, layouts and impl tables
//! once ([`exports::collect`], one value per row) and the image's writers once; of each
//! dependency, one ask (its exports row) and one O(1) read of its meta. That is under the
//! floor in work and over it in precision: the shape asks each dependency whole (its exports
//! digest), so a signature edit in a dependency moves the address of every dependent surface,
//! where the floor moves only those that read that signature. The fine shape needs the memo to
//! list, for a crate's own boundary queries, the dependency facts behind them, and a
//! crate-level classifier (blaze-item-hash's `reads::fact_of` is per body). Opening a surface
//! is mapping it: [`view`] checks the root and finds four ranges, O(1) in the record's size;
//! [`Opened::give`] gives each crate three slots.

#![blaze::sans_io]

pub mod exports;

#[cfg(test)]
mod tests;

use core::{fmt, mem::offset_of, ops::Range};
use std::{alloc::Global, sync::Arc};

use base_db::{Crate, Db};
use blaze_format::{ALIGN, Aligned, Record as _, Slice, Writer};
use blaze_ids::{ContentId, Hasher};
use blaze_items::key;
use blaze_keyed::{
    driver::TaskFailed,
    front::Universe,
    kind::{Key, Kind as _, Late, Surface},
    record::{self, Fact, Owner, RecordHead, Section, Sections, Traced},
};
use hir_def::deps::{ImageBuf, Mode, PageSource, Paged};

use crate::facts;

/// The section of the crate's key, 32 bytes.
pub const META: u32 = 1;
/// The section of the sealed image (`hir_def::deps::write_image`), under the record's address.
pub const IMAGE: u32 = 2;
/// The section of the def map pages (`hir_def::nameres::frozen::pages::encode`).
pub const PAGES: u32 = 3;
/// The section of hir-ty's impl tables (`hir_ty::method_resolution::frozen::encode`).
pub const TABLES: u32 = 4;

const SECTIONS: [u32; 4] = [META, IMAGE, PAGES, TABLES];
const META_LEN: usize = 32;

/// What a surface task refuses, by name.
pub const RESERVED_PREFIX: &str =
    "a surface task: the crate's key begins with a reserved owner (`record::Owner::ASKER` or `OUTSIDE`)";
pub const TWO_DIGESTS: &str = "a surface task: one row with two digests (a fact mapping that misses an input)";
pub const TWO_SURFACES: &str = "a surface task: two surfaces of one crate prefix in the universe, or the crate's own";
pub const BAD_DEPENDENCY: &str = "a surface task: a dependency's surface record that does not open";
pub const WRONG_OWNER: &str = "a surface task: a table of the universe whose surface is of a crate of another prefix";
/// What the crate's image parts are refused as, by `hir_def::deps::Refused`'s kind
/// ([`image_refused`]).
pub const IMAGE_SESSION_MACROS: &str =
    "a surface task: the crate's def map uses the session's macros (`hir_def::deps::Refused::SessionMacros`)";
pub const IMAGE_PROC_MACRO: &str = "a surface task: a procedural macro's expansion whose reads are not known (`hir_def::deps::Refused::ProcMacro`)";
pub const IMAGE_OUTSIDE_ROOT: &str = "a surface task: a module or an `include!` of a file outside the crate's source root (`hir_def::deps::Refused::OutsideSourceRoot`)";
pub const IMAGE_MODULE_MISSING: &str =
    "a surface task: a `mod` whose file is not there (`hir_def::deps::Refused::ModuleFileMissing`)";
pub const IMAGE_KEY_MOVED: &str =
    "a surface task: the crate's key moved while its image was made (`hir_def::deps::Refused::KeyMoved`)";
pub const IMAGE_MACHINE_PATH: &str =
    "a surface task: an image part holds a path of this checkout (`hir_def::deps::Refused::MachinePath`)";
pub const IMAGE_READS: &str = "a surface task: the crate's reads cannot be kept (`hir_def::deps::Refused::Reads`)";
pub const IMAGE_LATE_TWICE: &str = "a surface task: one late read (an environment variable, an included file) seen with two values in one crate (`hir_def::deps::Refused::LateTwice`)";
pub const IMAGE_ENCODE: &str ="a surface task: an image part (lang items, signatures) does not encode (`hir_def::deps::Refused::Encode`)";
pub const IMAGE_UNASKED: &str = "a surface task: the crate's image depends on what no fact of a shape can ask: what a procedural macro traced reading, or a file outside the VFS (`hir_def::deps::Refused::Unasked`)";
/// What a late read the shape asks is refused as.
pub const LATE_MOVED: &str = "a surface task: a late read (`env!`, `option_env!`, `include!`, `include_str!`, `include_bytes!`) saw another value than the request answers for it: the world moved under the task, so no record is filed";
pub const LATE_TWO_VALUES: &str =
    "a surface task: one late read seen with two values by the crate's expansions (an input changed while it was resolved)";
pub const LATE_UNKNOWN: &str =
    "a surface task: a late read of a macro byte that is no `blaze_keyed::kind::Late` (`hir_def::deps::LateSeen::which`)";
pub const PAGES_REFUSED: &str = "a surface task: the crate's def map does not write as pages";
pub const TABLES_REFUSED: &str = "a surface task: the crate's impl tables do not encode";
pub const UNALIGNED: &str = "a surface task: its own record does not open as a surface (its payload must start 16-aligned in the record, as `record::write_traced` pushes it with `blaze_format::Writer::push_buffer`)";

/// `Front::surface` (crates/blaze-keyed/src/front.rs) in `db`, where `krate` is the unit's
/// crate built from its text and every crate it depends on was opened from its surface
/// ([`Opened::give`]): the `Surface` record for `key`, its shape over `universe` (the
/// dependencies' tables), 16-aligned, filed at its address.
///
/// The shape also asks every late read the crate's expansions made
/// (`hir_def::deps::ImageParts::late`): each `env!` and `option_env!` (the crate's own
/// variables such as `CARGO_PKG_VERSION`, the process's, `CARGO_PRIMARY_PACKAGE`) and each
/// `include!`, `include_str!` and `include_bytes!` of a file of its source root, as
/// `record::late_id(late, name)`, with the digest of the value the expansion saw. Where
/// `Universe::answer` gives another digest the task fails ([`LATE_MOVED`]) and files nothing.
/// A late read is unit-relative and `late_id` names no unit (`CARGO_PKG_VERSION` is each
/// crate's own): the host's `outside` table in the universe it hands this unit's task must be
/// this unit's environment, so the same id answers per asker.
///
/// Floor: see the module doc. Costs past the floor: the rows sorted once (`r log r`), the
/// record written once and copied once into aligned memory, and opened once to check it.
pub fn surface(db: &Db, krate: Crate, key: Key<Surface>, universe: Universe<'_>) -> Result<Aligned, TaskFailed> {
    let refused = |what| TaskFailed::Refused { what };
    let crate_key = key::crate_key(db, krate);
    let owner = Owner::of_subject(&crate_key);
    if !owner.is_crate() {
        return Err(refused(RESERVED_PREFIX));
    }

    let mut rows = Vec::new_in(Global);
    exports::collect(db, krate, None, &mut rows)?;
    let (mut rows, exports) = finish_rows(rows, &crate_key)?;

    // The image's parts first: running them runs the expansions whose late reads the shape
    // asks, and the address is made from the shape.
    let parts = hir_def::deps::image_parts(db, krate).map_err(|why| refused(image_refused(&why)))?;

    let mut shape = Vec::with_capacity_in(universe.tables.len() + parts.late.len(), Global);
    late_asks(&parts.late, &universe, &mut shape)?;
    for table in universe.tables {
        let dep = view_unkeyed(table.surface).map_err(|_| refused(BAD_DEPENDENCY))?;
        if Owner::of_subject(&dep.meta.crate_key) != table.owner {
            return Err(refused(WRONG_OWNER));
        }
        if table.owner == owner {
            return Err(refused(TWO_SURFACES));
        }
        let id = facts::exports_id(&dep.meta.crate_key);
        let digest = universe.answer(&id).map_err(|it| it.failed())?;
        shape.push(Fact { id, digest });
    }
    shape.sort_unstable();
    if shape.windows(2).any(|pair| pair[0].id == pair[1].id) {
        return Err(refused(TWO_SURFACES));
    }
    insert(&mut rows, Fact { id: record::seen_id(), digest: seen_of(exports, &shape) })?;

    // The image is named by the record's address: what the crate's signatures are depends on
    // what its shape answered, as the record does.
    let at = record::address(key, &shape);
    let image = hir_def::deps::write_image(at.id(), parts.parts.each_ref());
    let pages = hir_def::nameres::frozen::pages::encode(db, krate).map_err(|_| refused(PAGES_REFUSED))?;
    let tables = hir_ty::method_resolution::frozen::encode(db, krate).map_err(|_| refused(TABLES_REFUSED))?;
    let meta = Meta { crate_key };
    let sections: [(u32, u32, &[u8]); 4] = [(META, 1, &meta.bytes()), (IMAGE, 1, &image), (PAGES, 1, &pages), (TABLES, 1, &tables)];
    let payload = payload(&at, &sections);
    let (filed, bytes) = record::write_read::<Surface>(key, &shape, &rows, &payload);
    debug_assert_eq!(filed, at);
    let out = Aligned::copy_from(&bytes);
    view(&out, &filed).map_err(|_| refused(UNALIGNED))?;
    Ok(out)
}

/// Inserts `row` into `rows` (sorted by id, each once) at its place; an id already there is
/// refused.
fn insert(rows: &mut Vec<Fact, Global>, row: Fact) -> Result<(), TaskFailed> {
    let at = rows.partition_point(|it| it.id < row.id);
    if rows.get(at).is_some_and(|it| it.id == row.id) {
        return Err(TaskFailed::Refused { what: TWO_DIGESTS });
    }
    rows.insert(at, row);
    Ok(())
}

/// What `why`, a refusal of the crate's image parts, fails the task as: one name a kind.
fn image_refused(why: &hir_def::deps::Refused) -> &'static str {
    use hir_def::deps::Refused;
    match why {
        Refused::SessionMacros => IMAGE_SESSION_MACROS,
        Refused::ProcMacro { .. } => IMAGE_PROC_MACRO,
        Refused::OutsideSourceRoot { .. } => IMAGE_OUTSIDE_ROOT,
        Refused::ModuleFileMissing { .. } => IMAGE_MODULE_MISSING,
        Refused::KeyMoved { .. } => IMAGE_KEY_MOVED,
        Refused::MachinePath { .. } => IMAGE_MACHINE_PATH,
        Refused::Reads { .. } => IMAGE_READS,
        Refused::Encode { .. } => IMAGE_ENCODE,
        Refused::Unasked { .. } => IMAGE_UNASKED,
        Refused::LateTwice { .. } => IMAGE_LATE_TWICE,
    }
}

/// The late macros, by their byte (`blaze_keyed::kind::Late as u8`).
const LATES: [Late; 5] = [Late::Env, Late::OptionEnv, Late::Include, Late::IncludeStr, Late::IncludeBytes];

/// The digest of the value a late read saw, by the host's rule for its outside answers
/// (`blaze_host::outside`): a variable's value or its absence (`var_answer`), a file's content
/// ID (blaze-vfs's `FILE_DOMAIN` of its bytes, which is `file_answer`).
pub fn late_digest(value: &hir_def::deps::LateValue) -> ContentId {
    match value {
        hir_def::deps::LateValue::Var(value) => blaze_host::outside::var_answer(value.as_deref().map(str::as_bytes)),
        hir_def::deps::LateValue::File(content) => *content,
    }
}

/// Each late read of `seen` (what the crate's expansions read) asked through `universe`, into
/// `shape` with the digest seen: sorted by id, each once. Two values seen for one id are
/// refused ([`LATE_TWO_VALUES`]); an answer other than the value seen fails the task
/// ([`LATE_MOVED`]), so a record is never filed at an address its expansions did not see.
fn late_asks(seen: &[hir_def::deps::LateSeen], universe: &Universe<'_>, shape: &mut Vec<Fact, Global>) -> Result<(), TaskFailed> {
    let refused = |what| TaskFailed::Refused { what };
    let mut asks: Vec<Fact, Global> = Vec::with_capacity_in(seen.len(), Global);
    for read in seen {
        let late = LATES.iter().find(|late| **late as u8 == read.which).ok_or(refused(LATE_UNKNOWN))?;
        // The planted bug of the late tests' oracle: a late read of a planted macro not asked.
        #[cfg(test)]
        if tests::DROP_LATE.get() & (1 << (*late as u8)) != 0 {
            continue;
        }
        asks.push(Fact { id: record::late_id(*late as u8, &read.name), digest: late_digest(&read.value) });
    }
    asks.sort_unstable();
    asks.dedup();
    if asks.windows(2).any(|pair| pair[0].id == pair[1].id) {
        return Err(refused(LATE_TWO_VALUES));
    }
    for ask in &asks {
        if universe.answer(&ask.id).map_err(|it| it.failed())? != ask.digest {
            return Err(refused(LATE_MOVED));
        }
    }
    shape.extend_from_slice(&asks);
    Ok(())
}

/// `rows` sorted by id with each id once (two digests of one id refused), and the exports row
/// added: the digest of every other row.
fn finish_rows(mut rows: Vec<Fact, Global>, crate_key: &ContentId) -> Result<(Vec<Fact, Global>, ContentId), TaskFailed> {
    rows.sort_unstable();
    let mut out: Vec<Fact, Global> = Vec::with_capacity_in(rows.len() + 2, Global);
    for row in rows {
        match out.last() {
            Some(last) if last.id == row.id => {
                if last.digest != row.digest {
                    return Err(TaskFailed::Refused { what: TWO_DIGESTS });
                }
            }
            _ => out.push(row),
        }
    }
    let mut hasher = Hasher::new("blaze-front surface exports");
    hasher.update(&(out.len() as u64).to_le_bytes());
    for row in &out {
        hasher.id(&row.id).id(&row.digest);
    }
    let exports = hasher.finish();
    insert(&mut out, Fact { id: facts::exports_id(crate_key), digest: exports })?;
    Ok((out, exports))
}

/// The seen digest: the crate's exports and those of every surface it read, by id.
fn seen_of(exports: ContentId, shape: &[Fact]) -> ContentId {
    let mut hasher = Hasher::new("blaze-front seen");
    hasher.id(&exports).update(&(shape.len() as u64).to_le_bytes());
    for read in shape {
        hasher.id(&read.id).id(&read.digest);
    }
    hasher.finish()
}

/// The meta section.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Meta {
    pub crate_key: ContentId,
}

impl Meta {
    fn bytes(&self) -> [u8; META_LEN] {
        self.crate_key.0
    }

    fn read(bytes: &[u8]) -> Option<Meta> {
        Some(Meta { crate_key: ContentId(bytes.try_into().ok()?) })
    }
}

fn head(at: &Key<Surface>) -> RecordHead {
    RecordHead { key: at.id(), tag: Surface::TAG.0, version: Surface::VERSION, flags: 0 }
}

/// The payload: a buffer of root [`Sections`], headed by the record's address, each section
/// 16-aligned in it.
fn payload(at: &Key<Surface>, sections: &[(u32, u32, &[u8])]) -> Vec<u8> {
    let mut inner = Writer::new(Sections::SCHEMA);
    let mut table = Vec::with_capacity_in(sections.len(), Global);
    for &(id, elem, bytes) in sections {
        let start = inner.bytes.len().next_multiple_of(ALIGN);
        inner.bytes.resize(start, 0);
        let at = inner.push_slice(bytes);
        table.push(Section { at, len: bytes.len() as u64, id, elem });
    }
    let table_at = inner.push_slice(&table);
    let root_at = inner.next::<Sections>();
    let root = Sections {
        head: head(at),
        sections: Slice::between(root_at + offset_of!(Sections, sections) as u64, table_at, table.len()),
    };
    let written = inner.push(&root);
    debug_assert_eq!(written, root_at);
    inner.finish(root_at)
}

/// Why a buffer is not a surface record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SurfaceError {
    Record(record::RecordError),
    Format(blaze_format::FormatError),
    /// A section missing, out of place or of the wrong length, by its id.
    Section(u32),
    /// Two surfaces of one crate prefix.
    TwoOfOnePrefix(Owner),
    /// A crate of the opened surfaces the snapshot has no crate of, by its key.
    NoCrate(ContentId),
    /// The image section does not open as an image (`hir_def::deps::ImageError`).
    Image(hir_def::deps::ImageError),
}

impl fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SurfaceError::Record(error) => write!(f, "a surface record: {error}"),
            SurfaceError::Format(error) => write!(f, "a surface record's sections: {error}"),
            SurfaceError::Section(id) => write!(f, "a surface record whose section {id} is missing or malformed"),
            SurfaceError::TwoOfOnePrefix(owner) => write!(f, "two surfaces of the crate prefix {:02x?}", owner.0),
            SurfaceError::NoCrate(key) => write!(f, "the surface of crate key {key:?}, which the snapshot has no crate of"),
            SurfaceError::Image(error) => write!(f, "a surface record's image: {error}"),
        }
    }
}

/// A surface record read in place: its shape, its rows and where its sections are.
#[derive(Clone, Debug)]
pub struct View<'a> {
    pub reads: &'a [Fact],
    pub facts: &'a [Fact],
    pub meta: Meta,
    /// Each of [`SECTIONS`]' range in the record's bytes, in that order.
    pub ranges: [Range<usize>; 4],
}

/// `bytes` as the surface record filed at `at` (its address): kind, version and the head
/// checked.
pub fn view<'a>(bytes: &'a [u8], at: &Key<Surface>) -> Result<View<'a>, SurfaceError> {
    let root = record::open::<Surface>(bytes, at).map_err(SurfaceError::Record)?;
    view_root(bytes, root)
}

/// `bytes` as a surface record of a table the host handed the task (its address the driver
/// proved): kind and version checked, the head not compared again.
pub fn view_unkeyed(bytes: &[u8]) -> Result<View<'_>, SurfaceError> {
    let root = blaze_format::open::<Traced>(bytes).map_err(SurfaceError::Format)?;
    use record::RecordError;
    if root.head.tag != Surface::TAG.0 {
        let found = blaze_keyed::kind::Tag(root.head.tag);
        return Err(SurfaceError::Record(RecordError::Kind { found, wanted: Surface::TAG }));
    }
    if root.head.version != Surface::VERSION {
        let (found, wanted) = (root.head.version, Surface::VERSION);
        return Err(SurfaceError::Record(RecordError::Version { found, wanted }));
    }
    view_root(bytes, root)
}

fn view_root<'a>(bytes: &'a [u8], root: &'a Traced) -> Result<View<'a>, SurfaceError> {
    let reads = root.reads.follow(bytes).map_err(SurfaceError::Format)?;
    let facts = root.facts.follow(bytes).map_err(SurfaceError::Format)?;
    let payload = root.payload.follow(bytes).map_err(SurfaceError::Format)?;
    // The payload is a slice of `bytes`: where it starts, as an offset of the record. It opens
    // in place only at a 16-aligned address (`blaze_format::header`).
    let base = payload.as_ptr().addr() - bytes.as_ptr().addr();
    let inner = blaze_format::open::<Sections>(payload).map_err(SurfaceError::Format)?;
    let table = inner.sections.follow(payload).map_err(SurfaceError::Format)?;
    if table.len() != SECTIONS.len() {
        return Err(SurfaceError::Section(0));
    }
    let mut ranges: [Range<usize>; 4] = [0..0, 0..0, 0..0, 0..0];
    for (at, (section, &want)) in table.iter().zip(SECTIONS.iter()).enumerate() {
        let start = usize::try_from(section.at).map_err(|_| SurfaceError::Section(want))?;
        let len = usize::try_from(section.len).map_err(|_| SurfaceError::Section(want))?;
        let end = start.checked_add(len).ok_or(SurfaceError::Section(want))?;
        if section.id != want || end > payload.len() || start % ALIGN != 0 {
            return Err(SurfaceError::Section(want));
        }
        if section.elem == 0 || len % section.elem as usize != 0 {
            return Err(SurfaceError::Section(want));
        }
        ranges[at] = base + start..base + end;
    }
    let meta = Meta::read(&bytes[ranges[0].clone()]).ok_or(SurfaceError::Section(META))?;
    Ok(View { reads, facts, meta, ranges })
}

impl View<'_> {
    /// The digest of the row `id`, by one binary search; `None` when the surface has no row.
    pub fn row(&self, id: &ContentId) -> Option<ContentId> {
        self.facts.binary_search_by(|it| it.id.cmp(id)).ok().map(|at| self.facts[at].digest)
    }
}

/// A range of a record held where the store keeps it, read as a page source: what a crate's
/// def map pages and impl tables are read from in place.
struct Part {
    holder: ImageBuf,
    range: Range<usize>,
}

impl PageSource for Part {
    fn size(&self) -> u64 {
        self.range.len() as u64
    }

    // dyn: as `PageSource::read`
    fn read(&self, at: u64, len: u64, with: &mut dyn FnMut(&[u8])) -> Result<(), String> {
        let bytes = &self.holder.bytes()[self.range.clone()];
        let range = usize::try_from(at).ok().zip(usize::try_from(len).ok());
        match range.and_then(|(at, len)| bytes.get(at..at.checked_add(len)?)) {
            Some(part) => {
                with(part);
                Ok(())
            }
            None => Err(format!("bytes {at}+{len} of a surface section of {}", bytes.len())),
        }
    }
}

/// One crate opened from its surface record.
struct OpenedCrate {
    owner: Owner,
    /// The record's address.
    at: ContentId,
    meta: Meta,
    holder: ImageBuf,
    ranges: [Range<usize>; 4],
}

/// The surfaces of a task's universe, opened: what a snapshot's crates are read from.
pub struct Opened {
    /// Sorted by owner, each once.
    crates: Vec<OpenedCrate, Global>,
}

impl Opened {
    /// Opens each surface of `surfaces` (its bytes held where the store keeps them, and the
    /// address it is filed at). One [`view`] each: O(1) in the records' sizes.
    pub fn open(surfaces: &[(ImageBuf, Key<Surface>)]) -> Result<Opened, SurfaceError> {
        let mut crates = Vec::with_capacity_in(surfaces.len(), Global);
        for (holder, at) in surfaces {
            let view = view(holder.bytes(), at)?;
            crates.push(OpenedCrate {
                owner: Owner::of_subject(&view.meta.crate_key),
                at: at.id(),
                meta: view.meta,
                holder: Arc::clone(holder),
                ranges: view.ranges,
            });
        }
        crates.sort_unstable_by_key(|it| it.owner);
        if let Some(pair) = crates.windows(2).find(|pair| pair[0].owner == pair[1].owner) {
            return Err(SurfaceError::TwoOfOnePrefix(pair[0].owner));
        }
        Ok(Opened { crates })
    }

    /// Gives each opened crate what it is read from, in `edit` (the edit that commits the
    /// snapshot a task runs in): its image (lang items, signatures, def map pages:
    /// `hir_def::deps::give_image`) and its impl tables (`hir_ty::method_resolution::frozen::give`),
    /// each a range of the record in place. `crate_of` names the snapshot's crate of a crate key.
    ///
    /// Floor: three slots a crate, no byte of a section read.
    pub fn give(&self, edit: &mut base_db::Edit<'_>, crate_of: impl Fn(&ContentId) -> Option<Crate>) -> Result<(), SurfaceError> {
        use hir_ty::method_resolution::frozen::{self as tables, Tables};
        for opened in &self.crates {
            let krate = crate_of(&opened.meta.crate_key).ok_or(SurfaceError::NoCrate(opened.meta.crate_key))?;
            let krate_id = opened.meta.crate_key;
            let section_key = |id: u32| {
                let mut hasher = Hasher::new("blaze-front surface section");
                hasher.id(&opened.at).update(&id.to_le_bytes());
                hasher.finish()
            };
            let paged = |at: usize, id: u32| Paged {
                key: section_key(id),
                // dyn: `Paged::source` is a trait object; one source a crate and section.
                source: Arc::new(Part { holder: Arc::clone(&opened.holder), range: opened.ranges[at].clone() })
                    as Arc<dyn PageSource>,
            };
            let image = hir_def::deps::Image::in_place(
                opened.at,
                opened.at,
                Arc::clone(&opened.holder),
                opened.ranges[1].clone(),
                Mode::Serve,
                Some(paged(2, PAGES)),
            )
            .map_err(SurfaceError::Image)?;
            hir_def::deps::give_image(edit, krate, krate_id, image);
            let tables = Tables { pages: paged(3, TABLES), mode: Mode::Serve };
            let fact = tables::tables_fact(krate_id, &tables);
            tables::give(edit, [(krate, fact, tables)]);
        }
        Ok(())
    }
}
