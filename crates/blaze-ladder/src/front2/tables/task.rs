//! THE BODY TASK (engine.md, E2/E5): [`check_body`] checks one body over the read-only view
//! ([`Tables`]) and answers the engine's [`Done`]: what it found, the cells it read with the
//! digest each held, what it offers readers, and the records it made inline ([`Made`]). It is
//! a function of `(tables, task, frame, make)` alone, callable from any worker.
//!
//! NOTHING IS SHARED OR KEPT. The checker's mutable world (its key map, head store, impl index,
//! constants and prepared macros: `ProjectWorld`) is made for this one body and dropped with
//! it, so a body's answer and its read list are the same whichever bodies ran before it, in
//! which order, on how many workers. The recorder is the task's own. The engine takes the
//! distinct keys through [`check_body_reads`] and sorts the combined row after placement.
//!
//! TWO ALLOCATORS ([`check_body_split`], lane BD3): `work`, where the check works (the recorder,
//! its `seen` set, the cells before the engine widens them, the verdict's words and bytes),
//! which the caller frees as the body ends; and `out`, where what outlives the body goes (the
//! records made inline). The row is the caller's, copied from the answer's cells at its length.
//!
//! THE BODY is found by its item's cells (`Item{key}`; a member's `Members{owner}` too), never
//! by listing the crate: the task's name's subject is the body's item key.
//!
//! WHAT IS READ, as cells:
//! - every names question through the view, cell by cell (view.rs);
//! - each head, constant and ADT fact the check asked (the body's `facts`, front2 reads.rs),
//!   as a `Signature` cell of the item by its checker key and the fact's kind;
//! - each impl table bucket a lookup read (the `CrateBucket` facts), as an `ImplBucket` cell.
//!
//! Each cell's digest is asked of [`Make`]: the engine's view for a cell an earlier phase made,
//! a record made inline (a [`Made`]) for one no phase made yet. The task never stops to wait
//! for a need: it is entered once and gives one result (entries per result: 1).
//!
//! INTERIM (until lane FH's frozen heads): the heads a body needs are made in its own store
//! from the view, so a body's reads also hold the cells those heads read (a superset: never a
//! read missed, sometimes a body dirtied by a head's text it did not need).

use core::cell::RefCell;
use core::fmt::Write as _;

use blaze_drive::cells::Cell;
use blaze_drive::key::{ABSENT, Kind, Name};
use blaze_engine::{CellId, Done, Made, SVec, Task};
use blaze_id::{ContentId, ItemKey};
use blaze_own::{AString, AVec, Alloc, SharedAlloc};
use blaze_scope::names_cells::NameCell;
use blaze_surface::ItemKind;

use super::super::Front2;
use super::super::borrows::{BorrowRefused, BorrowVerdict, KeptAt};
use super::super::buckets::impl_tables_name;
use super::super::index::{Index, LangTwice};
use super::super::keys::{Keys, MemberKind};
use super::super::project::{BodyRef, BodyVerdict, ProjectWorld};
use super::super::project::every_body::Reached;
use super::super::project_records::Listed;
use super::super::reads::{FactKind, ReadsError};
use super::super::by_origin::{FactHolder, fact_holder, kind_of_key};
use super::super::item_shown::{FactDefinition, ItemShown};
use super::super::source::{NamesSource, Records};
use super::head::{FreezeError, cell_id as head_cell_id, fact_cell};
use super::super::texts::Texts;
use super::super::heads::NeedsMet;
use super::cost::{Cost, Part};
use super::seeds::{HeadSeed, SeedError};
use super::recorder::{PlacedCell, ReadOf, ReadSink, ReadsFull, Recorder, RecorderOpts, TableRead};
use super::view::Tables;

/// The domain of a body result's `out`.
const OUT_DOMAIN: &str = "blaze-ladder front2 body task out 1";
/// The one cell a body's record offers its readers: its verdict (`out`).
pub const VERDICT_CELL: Cell = Cell([0; 16]);

/// What a body task is asked to make inline and what the engine knows: the digest a cell holds
/// today. A cell an earlier phase made is the engine's (`View::cell`); one no phase has made
/// yet is made here, its record pushed to `made`. `ABSENT`: no such cell (an empty bucket, a
/// head never written), which a later write moves.
pub trait Make<'a> {
    fn make(&self, want: &CellId, made: &mut SVec<'a, Made<'a>>) -> Result<ContentId, MakeRefused>;
}

/// [`Make`] could not give a cell's digest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MakeRefused {
    pub want: CellId,
}

/// What a body task runs in.
pub struct BodyFrame<'n> {
    /// The frame a body's lowering keeps its local items and expansions in.
    pub frame: &'n blaze_mem::Frame,
    /// Where the body's world and its result live.
    pub alloc: Alloc<'static>,
    /// PLANTED BUG, tests only: the recorder drops every read of this cell.
    #[doc(hidden)]
    pub plant_drop: Option<NameCell>,
    /// The worker's meter (cost.rs), when one measures where a body's time goes.
    pub cost: Option<&'n Cost<'n>>,
    /// How heads' needs are met (heads.rs `NeedsMet::All` outside the equality test).
    #[doc(hidden)]
    pub needs: NeedsMet,
    /// The request's frozen heads (seeds.rs [`FrozenTable`](super::seeds::FrozenTable), read
    /// only and shared): every head the body needs is filed from them, none lowered. `None`:
    /// the body's store lowers each head it needs (the path before frozen heads).
    pub heads: Option<&'n dyn HeadSeed<'n>>,
    /// What the body's macro calls expand with (body_host.rs `BodySources`: the owner's
    /// environment, the expansion texts, the proc-macro runner), as `check`'s path sets them
    /// (project.rs `set_body_sources`). `None`: each body `env!`, include and proc macro is
    /// refused by name.
    pub sources: Option<&'n dyn super::super::body_host::BodySources>,
    /// The request's table of shared trait goals (goals.rs [`GoalTable`](super::goals::GoalTable),
    /// one for the request's workers): each root goal the body asks is looked up there first.
    /// `None`: every goal is solved in the body.
    pub goals: Option<&'n dyn blaze_solve::share::GoalShare>,
    /// Stretch-scoped declarative macro preparation table shared across bodies in a stretch so
    /// each body allocates 0 bytes in `frame` for `Prepares`. `None`: `ProjectWorld::over`
    /// allocates a fresh one in `frame`.
    pub prepared: Option<&'n blaze_lower::scoped::Prepares<'n>>,
    /// Group-scoped lang-item [`Index`] and receiver [`Pointers`] with their exact recorded
    /// reads, shared across stretches and bodies of a member group so each body replays ~10 reads
    /// instead of rebuilding two `Global` hash maps and scanning all lang items from scratch.
    pub front: Option<&'n StretchFront>,
}

/// Magic header for persisted `BSF1` [`StretchFront`] CAS records (format v1).
pub const STRETCH_FRONT_MAGIC: &[u8; 4] = b"BSF1";

/// The known `receiver::Pointer::Unknown` static reason strings (receiver.rs), mapped to stable
/// 1-byte tags `2..=5` in `BSF1` so `Pointer::Unknown` round-trips with zero allocation or leaks.
const UNKNOWN_POINTER_REASONS: [&str; 4] = [
    "the head store was made without the program's receiver pointer types (`receiver::Pointers::find`)",
    "alloc's `Box` is in no module the names library knows",
    "alloc's `rc::Rc` or `sync::Arc` names no struct",
    "alloc's `rc::Rc` or `sync::Arc` does not resolve",
];

/// Why `BSF1` [`StretchFront`] bytes could not be encoded or decoded (rule 3, rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StretchFrontError {
    /// Input ended before the expected bytes could be read.
    Short { need: usize, left: usize },
    /// Leading 4-byte magic does not match [`STRETCH_FRONT_MAGIC`] (`b"BSF1"`).
    Magic { found: [u8; 4] },
    /// A list or string length does not fit in a `u32` (or exceeds remaining bytes).
    CountOverflow { what: &'static str, count: usize },
    /// A lang-crate index does not fit in a `u32` (or `usize`).
    IndexOverflow { value: usize },
    /// Encoded lang items are non-empty while `scope` is empty.
    ScopeEmptyWithLangs { langs: u32 },
    /// `scope` indices are not strictly ascending.
    UnsortedScope { at: usize },
    /// `langs` names are not strictly ascending.
    UnsortedLangs { at: usize },
    /// `reads` entries are not strictly ascending.
    UnsortedReads { at: usize },
    /// A lang item name is not valid UTF-8.
    InvalidUtf8 { at: usize },
    /// Unrecognized `receiver::Pointer` discriminant tag.
    PointerTag { tag: u8 },
    /// `receiver::Pointer::Unknown` carried a reason string not in [`UNKNOWN_POINTER_REASONS`].
    UnknownPointerReason,
    /// Unrecognized `TableRead` discriminant tag.
    ReadTag { tag: u8 },
    /// Unrecognized `NameCell` discriminant tag.
    NameCellTag { tag: u8 },
    /// Unrecognized `Section` discriminant tag.
    SectionTag { tag: u8 },
    /// `reads` count exceeds [`super::recorder::MOST_READS`].
    TooManyReads { count: u32 },
    /// Unexpected bytes remained after the end of the `BSF1` payload.
    Trailing { extra: usize },
    /// Conflicting lang item entries could not be loaded into [`super::super::index::IndexIn`].
    LangConflict,
}

impl core::fmt::Display for StretchFrontError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Short { need, left } => write!(f, "BSF1 stretch front truncated: needed {need} bytes, only {left} left (front2 tables/task.rs)"),
            Self::Magic { found } => write!(f, "BSF1 stretch front has magic {found:?}, expected {STRETCH_FRONT_MAGIC:?} (front2 tables/task.rs)"),
            Self::CountOverflow { what, count } => write!(f, "BSF1 stretch front {what} count {count} overflows limit (front2 tables/task.rs)"),
            Self::IndexOverflow { value } => write!(f, "BSF1 stretch front crate index {value} overflows u32 (front2 tables/task.rs)"),
            Self::ScopeEmptyWithLangs { langs } => write!(f, "BSF1 stretch front has {langs} lang items with empty scope (front2 tables/task.rs)"),
            Self::UnsortedScope { at } => write!(f, "BSF1 stretch front scope is not strictly ascending at entry {at} (front2 tables/task.rs)"),
            Self::UnsortedLangs { at } => write!(f, "BSF1 stretch front langs are not strictly ascending at entry {at} (front2 tables/task.rs)"),
            Self::UnsortedReads { at } => write!(f, "BSF1 stretch front reads are not strictly ascending at entry {at} (front2 tables/task.rs)"),
            Self::InvalidUtf8 { at } => write!(f, "BSF1 stretch front lang name at entry {at} is not valid UTF-8 (front2 tables/task.rs)"),
            Self::PointerTag { tag } => write!(f, "BSF1 stretch front has unknown pointer tag {tag} (front2 tables/task.rs)"),
            Self::UnknownPointerReason => f.write_str("BSF1 stretch front encountered an unrecognized Pointer::Unknown reason (front2 tables/task.rs)"),
            Self::ReadTag { tag } => write!(f, "BSF1 stretch front has unknown TableRead tag {tag} (front2 tables/task.rs)"),
            Self::NameCellTag { tag } => write!(f, "BSF1 stretch front has unknown NameCell tag {tag} (front2 tables/task.rs)"),
            Self::SectionTag { tag } => write!(f, "BSF1 stretch front has unknown Section tag {tag} (front2 tables/task.rs)"),
            Self::TooManyReads { count } => write!(f, "BSF1 stretch front read count {count} exceeds MOST_READS (front2 tables/task.rs)"),
            Self::Trailing { extra } => write!(f, "BSF1 stretch front has {extra} trailing bytes (front2 tables/task.rs)"),
            Self::LangConflict => f.write_str("BSF1 stretch front lang entries conflicted while building IndexIn (front2 tables/task.rs)"),
        }
    }
}

struct BsfReader<'a> {
    bytes: &'a [u8],
}

impl<'a> BsfReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn remaining(&self) -> usize {
        self.bytes.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], StretchFrontError> {
        match self.bytes.split_at_checked(n) {
            Some((head, rest)) => {
                self.bytes = rest;
                Ok(head)
            }
            None => Err(StretchFrontError::Short { need: n, left: self.bytes.len() }),
        }
    }

    fn take_array<const N: usize>(&mut self) -> Result<[u8; N], StretchFrontError> {
        let slice = self.take(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(slice);
        Ok(out)
    }

    fn take_u8(&mut self) -> Result<u8, StretchFrontError> {
        let [byte] = self.take_array::<1>()?;
        Ok(byte)
    }

    fn take_u32(&mut self) -> Result<u32, StretchFrontError> {
        Ok(u32::from_le_bytes(self.take_array::<4>()?))
    }

    fn take_item_key(&mut self) -> Result<ItemKey, StretchFrontError> {
        Ok(ItemKey(ContentId(self.take_array::<32>()?)))
    }
}

fn encode_pointer<A: core::alloc::Allocator>(pointer: super::super::receiver::Pointer, out: &mut blaze_own::Vec<u8, A>) -> Result<(), StretchFrontError> {
    use super::super::receiver::Pointer;
    match pointer {
        Pointer::Absent => out.push(0),
        Pointer::At(key) => {
            out.push(1);
            out.extend_from_slice(&key.0.0);
        }
        Pointer::Unknown(why) => {
            let Some(pos) = UNKNOWN_POINTER_REASONS.iter().position(|&known| known == why) else {
                return Err(StretchFrontError::UnknownPointerReason);
            };
            let tag = u8::try_from(pos).ok().and_then(|v| v.checked_add(2)).ok_or(StretchFrontError::UnknownPointerReason)?;
            out.push(tag);
        }
    }
    Ok(())
}

fn decode_pointer(r: &mut BsfReader<'_>) -> Result<super::super::receiver::Pointer, StretchFrontError> {
    use super::super::receiver::Pointer;
    match r.take_u8()? {
        0 => Ok(Pointer::Absent),
        1 => Ok(Pointer::At(r.take_item_key()?)),
        tag @ 2..=5 => {
            let idx = usize::from(tag - 2);
            let reason = UNKNOWN_POINTER_REASONS.get(idx).copied().ok_or(StretchFrontError::PointerTag { tag })?;
            Ok(Pointer::Unknown(reason))
        }
        tag => Err(StretchFrontError::PointerTag { tag }),
    }
}

fn encode_name_cell<A: core::alloc::Allocator>(cell: NameCell, out: &mut blaze_own::Vec<u8, A>) {
    use blaze_scope::names_cells::Section;
    match cell {
        NameCell::Binding { module, name } => {
            out.push(1);
            out.extend_from_slice(&module.0.0);
            out.extend_from_slice(&name.0);
        }
        NameCell::Module { module } => {
            out.push(2);
            out.extend_from_slice(&module.0.0);
        }
        NameCell::Root { krate } => {
            out.push(3);
            out.extend_from_slice(&krate);
        }
        NameCell::Variants { owner } => {
            out.push(4);
            out.extend_from_slice(&owner.0.0);
        }
        NameCell::Item { key } => {
            out.push(5);
            out.extend_from_slice(&key.0.0);
        }
        NameCell::Members { owner } => {
            out.push(6);
            out.extend_from_slice(&owner.0.0);
        }
        NameCell::Expansion { call } => {
            out.push(7);
            out.extend_from_slice(&call.0.0);
        }
        NameCell::Whole { krate, section } => {
            let sec = match section {
                Section::Names => 1u8,
                Section::Items => 2u8,
                Section::Expansions => 3u8,
            };
            out.push(8);
            out.extend_from_slice(&krate);
            out.push(sec);
        }
        NameCell::MemberOf { member } => {
            out.push(9);
            out.extend_from_slice(&member.0.0);
        }
        NameCell::VariantOf { variant } => {
            out.push(10);
            out.extend_from_slice(&variant.0.0);
        }
        NameCell::Beginning { first } => {
            out.push(11);
            out.extend_from_slice(&first);
        }
        NameCell::Lang { krate } => {
            out.push(12);
            out.extend_from_slice(&krate);
        }
        NameCell::Incoherent { krate } => {
            out.push(13);
            out.extend_from_slice(&krate);
        }
        NameCell::Textual { module } => {
            out.push(14);
            out.extend_from_slice(&module.0.0);
        }
        NameCell::InternalSymbols { krate } => {
            out.push(15);
            out.extend_from_slice(&krate);
        }
        NameCell::Links { krate } => {
            out.push(16);
            out.extend_from_slice(&krate);
        }
        NameCell::ImplRows { krate } => {
            out.push(17);
            out.extend_from_slice(&krate);
        }
        NameCell::Files { krate } => {
            out.push(18);
            out.extend_from_slice(&krate);
        }
        NameCell::Extensions { krate } => {
            out.push(19);
            out.extend_from_slice(&krate);
        }
        NameCell::CfgAtoms { krate } => {
            out.push(20);
            out.extend_from_slice(&krate);
        }
    }
}

fn decode_name_cell(r: &mut BsfReader<'_>) -> Result<NameCell, StretchFrontError> {
    use blaze_scope::cell_key::CellKey;
    use blaze_scope::names_cells::Section;
    match r.take_u8()? {
        1 => Ok(NameCell::Binding { module: r.take_item_key()?, name: CellKey(r.take_array::<16>()?) }),
        2 => Ok(NameCell::Module { module: r.take_item_key()? }),
        3 => Ok(NameCell::Root { krate: r.take_array::<8>()? }),
        4 => Ok(NameCell::Variants { owner: r.take_item_key()? }),
        5 => Ok(NameCell::Item { key: r.take_item_key()? }),
        6 => Ok(NameCell::Members { owner: r.take_item_key()? }),
        7 => Ok(NameCell::Expansion { call: r.take_item_key()? }),
        8 => {
            let krate = r.take_array::<8>()?;
            let section = match r.take_u8()? {
                1 => Section::Names,
                2 => Section::Items,
                3 => Section::Expansions,
                tag => return Err(StretchFrontError::SectionTag { tag }),
            };
            Ok(NameCell::Whole { krate, section })
        }
        9 => Ok(NameCell::MemberOf { member: r.take_item_key()? }),
        10 => Ok(NameCell::VariantOf { variant: r.take_item_key()? }),
        11 => Ok(NameCell::Beginning { first: r.take_array::<16>()? }),
        12 => Ok(NameCell::Lang { krate: r.take_array::<8>()? }),
        13 => Ok(NameCell::Incoherent { krate: r.take_array::<8>()? }),
        14 => Ok(NameCell::Textual { module: r.take_item_key()? }),
        15 => Ok(NameCell::InternalSymbols { krate: r.take_array::<8>()? }),
        16 => Ok(NameCell::Links { krate: r.take_array::<8>()? }),
        17 => Ok(NameCell::ImplRows { krate: r.take_array::<8>()? }),
        18 => Ok(NameCell::Files { krate: r.take_array::<8>()? }),
        19 => Ok(NameCell::Extensions { krate: r.take_array::<8>()? }),
        20 => Ok(NameCell::CfgAtoms { krate: r.take_array::<8>()? }),
        tag => Err(StretchFrontError::NameCellTag { tag }),
    }
}

fn encode_table_read<A: core::alloc::Allocator>(read: TableRead, out: &mut blaze_own::Vec<u8, A>) {
    match read {
        TableRead::Names(cell) => {
            out.push(0);
            encode_name_cell(cell, out);
        }
        TableRead::Signature { key } => {
            out.push(1);
            out.extend_from_slice(&key.0.0);
        }
        TableRead::Text { key } => {
            out.push(2);
            out.extend_from_slice(&key.0.0);
        }
        TableRead::Head { item, part } => {
            out.push(3);
            out.extend_from_slice(&item);
            out.push(part);
        }
        TableRead::Impls { krate, bucket } => {
            out.push(4);
            out.extend_from_slice(&krate);
            out.extend_from_slice(&bucket);
        }
    }
}

fn decode_table_read(r: &mut BsfReader<'_>) -> Result<TableRead, StretchFrontError> {
    match r.take_u8()? {
        0 => Ok(TableRead::Names(decode_name_cell(r)?)),
        1 => Ok(TableRead::Signature { key: r.take_item_key()? }),
        2 => Ok(TableRead::Text { key: r.take_item_key()? }),
        3 => Ok(TableRead::Head { item: r.take_array::<16>()?, part: r.take_u8()? }),
        4 => Ok(TableRead::Impls { krate: r.take_array::<16>()?, bucket: r.take_array::<16>()? }),
        tag => Err(StretchFrontError::ReadTag { tag }),
    }
}

/// Minimal `NamesSource` adapter used solely to hydrate [`super::super::index::IndexIn`] from
/// validated `BSF1` `(name, key)` entries without modifying `front2/index.rs`.
#[derive(Clone, Copy)]
struct DecodeLangSource<'n> {
    first_crate: Option<usize>,
    langs: &'n [(&'n str, ItemKey)],
}

impl<'n> NamesSource<'n> for DecodeLangSource<'n> {
    fn solved(self) -> Option<&'n blaze_scope::resolve::Names<'n>> { None }
    fn head_lowerings(self) -> Option<&'n super::super::heads::HeadLowerings> { None }
    fn gap(self, _question: &'static str, _key: Option<ItemKey>) {}
    fn crate_count(self) -> usize { 0 }
    fn crate_of(self, _key: ItemKey) -> Option<usize> { None }
    fn crate_key(self, _krate: usize) -> Option<blaze_id::CrateItemKey> { None }
    fn root_module(self, _krate: usize) -> Option<ItemKey> { None }
    fn edition_of(self, _krate: usize) -> Option<blaze_surface::Edition> { None }
    fn cfg_atoms(self, _krate: usize) -> Option<&'n [(&'n str, Option<&'n str>)]> { None }
    fn proc_macro(self, _krate: usize) -> Option<bool> { Some(false) }
    fn std_is(self, _krate: usize) -> Option<blaze_unit_graph::StdIs> { None }
    fn root_attrs(self, _krate: usize) -> Option<blaze_scope::graph::RootAttrs> { None }
    fn has_feature(self, _krate: usize, _name: &str) -> Option<bool> { None }
    fn linked<'a>(self, _roots: &[usize], _alloc: Alloc<'a>) -> Option<Result<super::super::linked::Linked<'a>, super::super::linked::RootsError>> { None }
    fn sysroot_std(self) -> Option<Option<usize>> { None }
    fn may_write_incoherent(self, _krate: usize) -> Option<bool> { None }
    fn walk(self, module: ItemKey, _ns: blaze_surface::Namespace, _written: blaze_scope::names::Written<'_, &str>) -> Result<blaze_scope::names::Resolved, blaze_scope::names::PathError> {
        Err(blaze_scope::names::PathError { module, at: 0, why: blaze_scope::names::Why::NotRead { module, layer: blaze_scope::names::Unheld::Step } })
    }
    fn written_module(self, module: ItemKey, _n: u8) -> Result<ItemKey, blaze_scope::names::PathError> {
        Err(blaze_scope::names::PathError { module, at: 0, why: blaze_scope::names::Why::NotRead { module, layer: blaze_scope::names::Unheld::Step } })
    }
    fn traits_in_scope<'b>(self, module: ItemKey, _alloc: Alloc<'b>) -> Result<AVec<'b, ItemKey>, blaze_scope::names::PathError> {
        Err(blaze_scope::names::PathError { module, at: 0, why: blaze_scope::names::Why::NotRead { module, layer: blaze_scope::names::Unheld::Step } })
    }
    fn variants(self, _owner: ItemKey) -> Option<super::super::source::VariantRows<'n>> { None }
    fn variant_of(self, _variant: ItemKey) -> Option<(ItemKey, u32)> { None }
    fn has_text(self, _key: ItemKey) -> bool { false }
    fn text_of(self, _key: ItemKey) -> Option<blaze_scope::task::ItemText<'n>> { None }
    fn name_of(self, _key: ItemKey) -> Option<&'n str> { None }
    fn home_of(self, _key: ItemKey) -> Option<ItemKey> { None }
    fn place_of(self, _key: ItemKey) -> Option<blaze_scope::resolve::Place> { None }
    fn item_start(self, _key: ItemKey) -> Option<(blaze_scope::resolve::At, u32)> { None }
    fn file_paths(self, _krate: usize, _file: u32) -> Option<super::super::source::FilePaths<'n>> { None }
    fn file_text(self, _krate: usize, _file: u32) -> Option<&'n str> { None }
    fn call_origin(self, _krate: usize, call: ItemKey) -> Result<super::super::expansion_origin::CallOrigin<'n>, super::super::expansion_origin::Unmapped> {
        Err(super::super::expansion_origin::Unmapped::NotServed { call })
    }
    fn member_beginning(self, _krate: usize, _item: blaze_typeck::ItemKey) -> Result<Option<ItemKey>, crate::seam::PositionGap> { Ok(None) }
    fn enclosing_keys(self, _krate: usize, _visit: impl FnMut(ItemKey) -> Result<(), crate::seam::PositionGap>) -> Result<(), crate::seam::PositionGap> { Ok(()) }
    fn expansion_text(self, _krate: usize, _call: ItemKey) -> Option<&'n str> { None }
    fn text_marks(self, _krate: usize, _call: ItemKey) -> Result<Option<blaze_scope::crate_roots::TextMarks<'n>>, super::super::item_site::MarksLost> { Ok(None) }
    fn extern_abi(self, _key: ItemKey) -> Option<Option<&'n str>> { None }
    fn attr_value(self, _key: ItemKey, _attr: &'static str) -> Option<Option<blaze_scope::attr_values::RowValue<'n>>> { None }
    fn kind_of_item(self, _key: ItemKey) -> Option<ItemKind> { None }
    fn detail_of(self, _key: ItemKey) -> Option<blaze_surface::ItemDetail> { None }
    fn is_ctor(self, _key: ItemKey) -> bool { false }
    fn each_member(self, _owner: ItemKey, _each: &mut dyn FnMut(super::super::source::MemberOf<'n>)) -> bool { false }
    fn member_owner(self, _member: ItemKey) -> Option<(ItemKey, &'n str)> { None }
    fn each_impl(self, _krate: usize, _each: &mut dyn FnMut(super::super::index::ImplAt<'n>)) {}
    fn each_impl_row(self, _krate: usize, _each: &mut dyn FnMut(blaze_scope::items_record::ImplRow<'n>)) -> bool { false }
    fn impl_with_header(self, _krate: usize, _key: ItemKey, _module: ItemKey) -> Option<super::super::index::ImplAt<'n>> { None }
    fn each_lang(self, krate: usize, each: &mut dyn FnMut(&'n str, ItemKey)) {
        if self.first_crate == Some(krate) {
            for &(name, key) in self.langs {
                each(name, key);
            }
        }
    }
    fn macro_body_of(self, module: ItemKey, _written: blaze_scope::names::Written<'_, &str>) -> Result<Option<blaze_scope::task::MacroBody<'n>>, blaze_scope::names::PathError> {
        Err(blaze_scope::names::PathError { module, at: 0, why: blaze_scope::names::Why::NotRead { module, layer: blaze_scope::names::Unheld::Step } })
    }
    fn macro_at(self, module: ItemKey, _written: blaze_scope::names::Written<'_, &str>) -> Result<super::super::source::MacroAt<'n>, blaze_scope::names::PathError> {
        Err(blaze_scope::names::PathError { module, at: 0, why: blaze_scope::names::Why::NotRead { module, layer: blaze_scope::names::Unheld::Step } })
    }
    fn module_path(self, _module: ItemKey) -> Option<&'n str> { None }
    fn failed_call<'b>(self, _call: ItemKey, _bump: blaze_own::Bump<'b>) -> Option<blaze_scope::names_record::FailedRow<'b>>
    where
        'n: 'b,
    {
        None
    }
    fn each_item(self, _krate: usize, _each: &mut dyn FnMut(ItemKey, ItemKey)) -> bool { false }
    fn block_binding(self, _module: ItemKey, _ns: blaze_surface::Namespace, _name: &str) -> Option<Option<blaze_surface::Binding>> { None }
    fn binds(self, _module: ItemKey, _ns: blaze_surface::Namespace, _name: &'n str) -> bool { false }
    fn key_beginning(self, _first: [u8; 16]) -> Option<ItemKey> { None }
    fn declared_lifetimes(self, _key: ItemKey) -> Option<u32> { None }
    fn assoc_lifetimes(self, _trait_: ItemKey, _name: blaze_typeck::Name) -> Option<u32> { None }
}

/// Thread-safe, group-scoped lang-item [`Index`] and receiver
/// [`super::super::receiver::Pointers`] built once per member group alongside the exact
/// `TableRead`s their construction performed, replayed into each body's recorder so read lists
/// remain identical (rule 21).
pub struct StretchFront {
    index: super::super::index::IndexIn,
    /// Canonical sorted `(name, key)` lang entries matching `index`, kept so `encode` writes
    /// deterministic `BSF1` bytes independent of hash-map iteration order (rule 25).
    langs: blaze_own::Vec<(blaze_own::String<blaze_own::SharedAlloc<'static>>, ItemKey), blaze_own::SharedAlloc<'static>>,
    pointers: super::super::receiver::Pointers,
    reads: blaze_own::Vec<TableRead, blaze_own::SharedAlloc<'static>>,
}

impl StretchFront {
    pub fn new(tables: Tables<'_, '_>, scratch: Alloc<'_>, alloc: blaze_own::SharedAlloc<'static>) -> Result<Self, TaskError> {
        let mut recorder = Recorder::new(RecorderOpts { alloc: scratch });
        let view = tables.reading_into(&recorder);
        let lang_crates = view.lang_crates();
        let index = super::super::index::IndexIn::from_source_in(view, lang_crates, alloc).map_err(TaskError::Lang)?;
        view.records().name_lang_crates(lang_crates);
        let borrowed = index.borrow();
        let pointers = super::super::receiver::Pointers::find(&borrowed, crate::resolve::Paths { names: view });
        let (recorded, _) = recorder.take_item().map_err(TaskError::Full)?;
        let mut reads = blaze_own::Vec::with_capacity_in(recorded.len(), alloc);
        reads.extend_from_slice(&recorded);
        reads.sort_unstable();
        let mut langs = blaze_own::Vec::with_capacity_in(borrowed.lang_count(), alloc);
        for &krate in lang_crates {
            if tables.records().proc_macro(krate) == Some(true) {
                continue;
            }
            tables.records().each_lang(krate, &mut |name, key| {
                langs.push((blaze_own::String::from_str_in(name, alloc), key));
            });
        }
        langs.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()).then_with(|| a.1.cmp(&b.1)));
        langs.dedup_by(|a, b| a.0.as_str() == b.0.as_str());
        Ok(Self { index, langs, pointers, reads })
    }

    /// Encodes `self` into the deterministic `BSF1` binary layout (rule 25):
    /// `BSF1 | scope_len u32 | scope [u32] | langs_len u32 | langs [(len u32, utf8, key 32)] |
    ///  pointers (boxed, rc, arc, pin) | reads_len u32 | reads [TableRead]`.
    pub fn encode<A: core::alloc::Allocator>(&self, out: &mut blaze_own::Vec<u8, A>) -> Result<(), StretchFrontError> {
        let index = self.index.borrow();
        let scope = index.scope();
        let scope_len = u32::try_from(scope.len()).map_err(|_| StretchFrontError::CountOverflow { what: "scope", count: scope.len() })?;
        let langs_len = u32::try_from(self.langs.len()).map_err(|_| StretchFrontError::CountOverflow { what: "langs", count: self.langs.len() })?;
        let reads_len = u32::try_from(self.reads.len()).map_err(|_| StretchFrontError::CountOverflow { what: "reads", count: self.reads.len() })?;
        if scope.is_empty() && !self.langs.is_empty() {
            return Err(StretchFrontError::ScopeEmptyWithLangs { langs: langs_len });
        }
        out.extend_from_slice(STRETCH_FRONT_MAGIC);
        out.extend_from_slice(&scope_len.to_le_bytes());
        for &krate in scope {
            let idx = u32::try_from(krate).map_err(|_| StretchFrontError::IndexOverflow { value: krate })?;
            out.extend_from_slice(&idx.to_le_bytes());
        }
        out.extend_from_slice(&langs_len.to_le_bytes());
        for (name, key) in self.langs.iter() {
            let name_bytes = name.as_bytes();
            let name_len = u32::try_from(name_bytes.len()).map_err(|_| StretchFrontError::CountOverflow { what: "lang name", count: name_bytes.len() })?;
            out.extend_from_slice(&name_len.to_le_bytes());
            out.extend_from_slice(name_bytes);
            out.extend_from_slice(&key.0.0);
        }
        encode_pointer(self.pointers.boxed, out)?;
        encode_pointer(self.pointers.rc, out)?;
        encode_pointer(self.pointers.arc, out)?;
        encode_pointer(self.pointers.pin, out)?;
        out.extend_from_slice(&reads_len.to_le_bytes());
        for &read in self.reads.iter() {
            encode_table_read(read, out);
        }
        Ok(())
    }

    /// Decodes a `BSF1` record into [`StretchFront`] allocated in `alloc`, validating magic,
    /// bounds, UTF-8, strict canonical ordering, and zero trailing bytes once (rule 3, rule 74).
    pub fn decode(bytes: &[u8], alloc: blaze_own::SharedAlloc<'static>) -> Result<Self, StretchFrontError> {
        let mut r = BsfReader::new(bytes);
        let magic = r.take_array::<4>()?;
        if &magic != STRETCH_FRONT_MAGIC {
            return Err(StretchFrontError::Magic { found: magic });
        }
        let scope_len_u32 = r.take_u32()?;
        let scope_len = usize::try_from(scope_len_u32).map_err(|_| StretchFrontError::CountOverflow { what: "scope", count: usize::MAX })?;
        if scope_len > r.remaining() / 4 {
            return Err(StretchFrontError::Short { need: scope_len.saturating_mul(4), left: r.remaining() });
        }
        let mut scope = blaze_own::Vec::with_capacity_in(scope_len, alloc);
        let mut prev_scope: Option<usize> = None;
        for at in 0..scope_len {
            let raw = r.take_u32()?;
            let krate = usize::try_from(raw).map_err(|_| StretchFrontError::CountOverflow { what: "scope index", count: usize::MAX })?;
            if let Some(prev) = prev_scope
                && krate <= prev
            {
                return Err(StretchFrontError::UnsortedScope { at });
            }
            prev_scope = Some(krate);
            scope.push(krate);
        }
        let langs_len_u32 = r.take_u32()?;
        if scope.is_empty() && langs_len_u32 > 0 {
            return Err(StretchFrontError::ScopeEmptyWithLangs { langs: langs_len_u32 });
        }
        let langs_len = usize::try_from(langs_len_u32).map_err(|_| StretchFrontError::CountOverflow { what: "langs", count: usize::MAX })?;
        if langs_len > r.remaining() / 36 {
            return Err(StretchFrontError::Short { need: langs_len.saturating_mul(36), left: r.remaining() });
        }
        let mut borrowed_langs: blaze_own::Vec<(&str, ItemKey), blaze_own::SharedAlloc<'static>> = blaze_own::Vec::with_capacity_in(langs_len, alloc);
        let mut prev_lang: Option<&str> = None;
        for at in 0..langs_len {
            let name_len_u32 = r.take_u32()?;
            let name_len = usize::try_from(name_len_u32).map_err(|_| StretchFrontError::CountOverflow { what: "lang name", count: usize::MAX })?;
            let raw_name = r.take(name_len)?;
            let name = core::str::from_utf8(raw_name).map_err(|_| StretchFrontError::InvalidUtf8 { at })?;
            if let Some(prev) = prev_lang
                && name <= prev
            {
                return Err(StretchFrontError::UnsortedLangs { at });
            }
            prev_lang = Some(name);
            let key = r.take_item_key()?;
            borrowed_langs.push((name, key));
        }
        let pointers = super::super::receiver::Pointers {
            boxed: decode_pointer(&mut r)?,
            rc: decode_pointer(&mut r)?,
            arc: decode_pointer(&mut r)?,
            pin: decode_pointer(&mut r)?,
        };
        let reads_len_u32 = r.take_u32()?;
        let reads_len = usize::try_from(reads_len_u32).map_err(|_| StretchFrontError::TooManyReads { count: reads_len_u32 })?;
        if reads_len > super::recorder::MOST_READS {
            return Err(StretchFrontError::TooManyReads { count: reads_len_u32 });
        }
        if reads_len > r.remaining() / 10 {
            return Err(StretchFrontError::Short { need: reads_len.saturating_mul(10), left: r.remaining() });
        }
        let mut reads = blaze_own::Vec::with_capacity_in(reads_len, alloc);
        let mut prev_read: Option<TableRead> = None;
        for at in 0..reads_len {
            let read = decode_table_read(&mut r)?;
            if let Some(prev) = prev_read
                && read <= prev
            {
                return Err(StretchFrontError::UnsortedReads { at });
            }
            prev_read = Some(read);
            reads.push(read);
        }
        if r.remaining() != 0 {
            return Err(StretchFrontError::Trailing { extra: r.remaining() });
        }
        let source = DecodeLangSource { first_crate: scope.first().copied(), langs: borrowed_langs.as_slice() };
        let index = super::super::index::IndexIn::from_source_in(source, scope.as_slice(), alloc).map_err(|_| StretchFrontError::LangConflict)?;
        let mut langs = blaze_own::Vec::with_capacity_in(borrowed_langs.len(), alloc);
        for &(name, key) in borrowed_langs.iter() {
            langs.push((blaze_own::String::from_str_in(name, alloc), key));
        }
        Ok(Self { index, langs, pointers, reads })
    }

    /// Borrows the shared lang-item index with zero heap allocation.
    pub fn borrow_index<'n>(&'n self) -> Index<'n> {
        self.index.borrow()
    }

    /// The pre-resolved receiver pointer types (`Box`, `Rc`, `Arc`, `Pin`, `NonNull`, ...).
    pub fn pointers(&self) -> super::super::receiver::Pointers {
        self.pointers
    }

    /// The exact table reads performed while building [`Self`], to replay into a recorder.
    pub fn reads(&self) -> &[TableRead] {
        &self.reads
    }

    /// The canonical sorted `(name, key)` lang entries held by [`Self`].
    pub fn langs(&self) -> &[(blaze_own::String<blaze_own::SharedAlloc<'static>>, ItemKey)] {
        &self.langs
    }
}

/// Configuration for [`StretchStore::new`].
pub struct StretchStoreOpts {
    /// Virtual address-space reservation for the stretch heads frame (e.g. `512 MiB`).
    pub reserve: usize,
    /// Soft live-byte cap on `heads_frame.used()` past which the stretch store rolls over
    /// (drops `Store` + `Keys`, resets `heads_frame`, and trims to `keep`) between bodies.
    pub cap: usize,
    /// Committed bytes retained in `heads_frame` after a rollover trim.
    pub keep: usize,
    /// Allocator for the pinned `Box<blaze_mem::Frame>` and reusable slot header (once per stretch).
    pub alloc: Alloc<'static>,
}

struct StretchStoreInner<'n> {
    store: super::super::heads::Store<'n>,
    keys: Keys,
    facts: blaze_typeck::infer::FactCache<'static>,
    facts_crate: Option<usize>,
    heads_alloc: Alloc<'static>,
}

struct StretchStoreSlot<'n> {
    inner: Option<StretchStoreInner<'n>>,
}

/// Stretch-scoped reusable [`super::super::heads::Store`] and [`Keys`] across consecutive
/// same-group bodies checked over frozen heads (`frame.heads.is_some()`).
///
/// Field order is load-bearing for `Drop`: `slot` drops FIRST (dropping `Store` and `Keys` while
/// `heads_frame` and its embedded `Erased` allocator handle are still alive), and `heads_frame`
/// drops SECOND (unmapping the virtual memory). `heads_frame` is boxed so moving [`StretchStore`]
/// never invalidates the `Erased<&Frame>` handle placed inside `heads_frame`.
pub struct StretchStore<'n> {
    slot: blaze_own::Box<StretchStoreSlot<'n>, Alloc<'static>>,
    heads_frame: blaze_own::Box<blaze_mem::Frame, Alloc<'static>>,
    cap: usize,
    keep: usize,
    rollovers: u64,
}

impl<'n> StretchStore<'n> {
    /// Reserves a dedicated heads frame for reusing imported frozen heads across bodies of one
    /// stretch without allocating into `BODIES_FRAME` or leaking across stretches.
    pub fn new(opts: StretchStoreOpts) -> Result<StretchStore<'n>, blaze_mem::MemError> {
        let frame = blaze_mem::Frame::with_reserve(opts.reserve, blaze_mem::Tag::Infer)?;
        Ok(StretchStore {
            slot: blaze_own::Box::new_in(StretchStoreSlot { inner: None }, opts.alloc),
            heads_frame: blaze_own::Box::new_in(frame, opts.alloc),
            cap: opts.cap,
            keep: opts.keep,
            rollovers: 0,
        })
    }

    /// Bytes currently committed in the stretch heads frame.
    pub fn committed(&self) -> usize {
        self.heads_frame.committed()
    }

    /// Bytes currently live in the stretch heads frame.
    pub fn used(&self) -> usize {
        self.heads_frame.used()
    }

    /// Total virtual reservation of the stretch heads frame.
    pub fn reserved(&self) -> usize {
        self.heads_frame.reserved()
    }

    /// Takes any recorded exhaustion of the stretch heads frame, returning `(used, reserved)`.
    pub fn take_exhaustion(&self) -> Option<(usize, usize)> {
        self.heads_frame.take_exhaustion().map(|used| (used, self.heads_frame.reserved()))
    }

    /// Number of times this stretch store rolled over (due to taint or exceeding `cap`).
    pub fn rollovers(&self) -> u64 {
        self.rollovers
    }

    fn reset_frame(&mut self) {
        // Drop `Store` and `Keys` BEFORE resetting `heads_frame` so every persistent table drops
        // while `heads_frame` and its embedded `Erased` handle are still live.
        drop(self.slot.inner.take());
        self.heads_frame.reset();
        if self.heads_frame.committed() > self.keep {
            self.heads_frame.trim(self.keep);
        }
    }

    #[inline(never)]
    fn take_or_init(
        &mut self,
        sized: Option<ItemKey>,
        pointers: super::super::receiver::Pointers,
        body_frame: &'n blaze_mem::Frame,
    ) -> StretchStoreInner<'n> {
        if let Some(inner) = self.slot.inner.take() {
            return inner;
        }
        if self.heads_frame.used() > 0 {
            self.heads_frame.reset();
            if self.heads_frame.committed() > self.keep {
                self.heads_frame.trim(self.keep);
            }
        }
        let frame_ref: &blaze_mem::Frame = &self.heads_frame;
        let erased: &blaze_own::Erased<&blaze_mem::Frame> = frame_ref.alloc(blaze_own::Erased::new(frame_ref));
        // SAFETY: `self.heads_frame` is boxed at a pinned heap address, `erased` lives inside
        // `self.heads_frame`, `self.slot` drops before `self.heads_frame`, and any taken
        // `StretchStoreInner` is returned or dropped before `reset_frame()` or `Drop` runs via
        // `BodyWorldGuard`.
        let heads_alloc = unsafe { body_world_alloc(erased.handle()) };
        let mut store = super::super::heads::Store::new(sized, body_frame, heads_alloc);
        store.pointers = pointers;
        let keys = Keys::new_in(heads_alloc);
        let facts = blaze_typeck::infer::FactCache::new_in(heads_alloc);
        StretchStoreInner { store, keys, facts, facts_crate: None, heads_alloc }
    }

    #[inline(never)]
    fn put_back(&mut self, inner: StretchStoreInner<'n>, status: super::super::heads::ReuseStatus) {
        if status == super::super::heads::ReuseStatus::Clean && self.heads_frame.used() <= self.cap {
            self.slot.inner = Some(inner);
        } else {
            drop(inner);
            self.rollovers = self.rollovers.saturating_add(1);
            self.reset_frame();
        }
    }
}

/// The ask a failed cell belonged to. At import time the checker may have made several
/// distinct asks about the opaque, or none at all. The rows are sorted by fact kind.
#[derive(Debug)]
pub enum CellAsk {
    Fact(FactKind),
    SeedBeforeFact,
    Several { first: FactKind, another: FactKind },
    SeedWithoutReads(ReadsError),
}

impl CellAsk {
    fn during_seed(verdict: &BodyVerdict<'_>, key: blaze_typeck::ItemKey) -> Self {
        let facts = match &verdict.facts {
            Ok(facts) => facts,
            Err(why) => return Self::SeedWithoutReads(*why),
        };
        let mut kinds = facts.rows.iter().filter(|row| row.fact.key == key.0).map(|row| row.fact.kind);
        let Some(first) = kinds.next() else { return Self::SeedBeforeFact };
        match kinds.find(|&kind| kind != first) {
            Some(another) => Self::Several { first, another },
            None => Self::Fact(first),
        }
    }
}

/// Why a body task gave no result: each a fault of the plan or the engine, never of the body
/// (a body's own failure is a [`Done`] with `failed`).
#[derive(Debug)]
pub enum TaskError {
    /// A head the body needed could not be filed from the request's frozen heads.
    Seed { body: Name, error: SeedError },
    /// No head cell holds the fact of checker key `key`, an item of a kind with a head
    /// (by_origin.rs [`FactHolder::Head`]; head.rs `fact_cell`): `shown` names it by its path
    /// (rule 19).
    Cell { key: blaze_typeck::ItemKey, ask: CellAsk, holder: blaze_typeck::ItemKey, shown: FactDefinition, error: FreezeError },
    /// The task's name is not a body's (`Kind::BodyCheck` of a fn, `const` or `static` item,
    /// or a fn or `const` member).
    NotABody { name: Name },
    /// Two lang items under one name in the lang crates.
    Lang(LangTwice),
    /// The body's read list could not be made into facts.
    Facts(ReadsError),
    /// The body read more than the recorder holds.
    Full(ReadsFull),
    /// A read names a crate the view does not hold.
    NoCrate { prefix: [u8; 8] },
    /// [`Make`] refused a cell.
    Make(MakeRefused),
    /// Writing the result's bytes failed (its allocator).
    Write,
    /// The body's board for the one ask (front2 asks.rs `AsksMet::Once`) could not be made.
    Asks { body: Name, why: blaze_core::claim_table::ClaimError },
    /// The body's clean check could not be written as its checked record (blaze-typeck
    /// checked_record.rs, D163 R1): never a body left without one quietly (rule 3).
    Typed { body: Name, why: blaze_typeck::checked_record::CheckedRecordError },
}

/// Maps a raw table question to the engine cell grain used by its caller's scope.
pub trait ReadKeyer {
    fn key(&self, tables: Tables<'_, '_>, read: TableRead) -> Result<Option<PlacedCell>, TaskError>;

    /// The source facets and head parts have their own, injective identities and cannot
    /// alias another kind. Such a caller can leave placement of these reads to its row.
    fn keeps_item_keys(&self) -> bool { false }
}

struct ObservedReads<'r, 'a, 't, 'n, 'k> {
    recorder: &'r Recorder<'a>,
    tables: Tables<'t, 'n>,
    keyer: Option<&'k dyn ReadKeyer>,
    item_keyer: Option<&'k dyn ReadKeyer>,
    error: RefCell<Option<TaskError>>,
}

impl ReadSink for ObservedReads<'_, '_, '_, '_, '_> {
    fn read(&self, read: TableRead) {
        let keyer = match read {
            TableRead::Head { .. } | TableRead::Signature { .. } | TableRead::Text { .. } => self.item_keyer,
            TableRead::Names(_) | TableRead::Impls { .. } => self.keyer,
        };
        let Some(keyer) = keyer else {
            self.recorder.read(read);
            return;
        };
        if let Err(error) = self.recorder.read_mapped(read, || keyer.key(self.tables, read)) {
            self.error.borrow_mut().get_or_insert(error);
        }
    }

    fn asking(&self, ask: Option<blaze_typeck::Asking>) {
        ReadSink::asking(self.recorder, ask);
    }

    fn binding(&self, module: ItemKey, ns: blaze_surface::Namespace, name: &str) {
        let Some(keyer) = self.keyer else {
            ReadSink::binding(self.recorder, module, ns, name);
            return;
        };
        if let Err(error) = self.recorder.binding_mapped(module, ns, name, |read| keyer.key(self.tables, read)) {
            self.error.borrow_mut().get_or_insert(error);
        }
    }
}

impl core::fmt::Display for TaskError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TaskError::NotABody { name } => write!(f, "the task {name:?} names no body (front2 tables/task.rs)"),
            TaskError::Lang(twice) => write!(f, "two lang items under one name: {twice:?}"),
            TaskError::Facts(error) => write!(f, "the body's read list: {error}"),
            TaskError::Full(full) => write!(f, "{full}"),
            TaskError::NoCrate { prefix } => write!(f, "a read names the crate {prefix:02x?}, which the tables do not hold"),
            TaskError::Make(refused) => write!(f, "no digest for the cell {:?} (front2 tables/task.rs `Make`)", refused.want),
            TaskError::Write => write!(f, "the body's result could not be written"),
            TaskError::Asks { body, why } => write!(f, "body {body:?}: the one ask's board (front2 asks.rs `BOARD_SLOTS`) could not be made: {why:?}"),
            TaskError::Typed { body, why } => write!(f, "body {body:?}: its clean check has no checked record: {why}"),
            TaskError::Seed { body, error } => write!(f, "body {body:?}: a head it needs could not be seeded: {error}"),
            TaskError::Cell { key, ask, holder, shown, error } => {
                writeln!(f, "{shown}")?;
                match ask {
                    CellAsk::Fact(kind) => write!(f, "the body requested {kind:?}")?,
                    CellAsk::SeedBeforeFact => f.write_str("the seed was refused before the checker recorded a fact for this key")?,
                    CellAsk::Several { first, another } => write!(f, "the body requested multiple facts for this key, including {first:?} and {another:?} (the failing ask is ambiguous)")?,
                    CellAsk::SeedWithoutReads(why) => write!(f, "the body's requested facts could not be read: {why}")?,
                }
                write!(f, ", key {:032x}, attempted holder {:032x}: {error}", key.0, holder.0)
            }
        }
    }
}

/// A body task's answer: the engine's result, the records made inline, and what tests compare.
pub struct BodyOut<'a> {
    pub done: Done<'a>,
    pub made: SVec<'a, Made<'a>>,
    /// The verdict's canonical bytes: `done.out` is their digest.
    pub bytes: AVec<'a, u8>,
    /// The recorder's list (sorted, each once), before it is named as cells.
    pub reads: AVec<'a, TableRead>,
    /// The head read of each head the body's own store held after the check, with the head's
    /// checker key (sorted): what the read-list control compares `reads` against.
    pub held: AVec<'a, (TableRead, u128)>,
    /// What the verdict says, as `check` prints it from today's path (front2 said.rs, the one
    /// home of a refusal's words and cause): each type diagnostic, typed lint finding and
    /// borrow finding with its item-relative range, then the reason a body was not checked.
    pub said: AVec<'a, Said<'a>>,
    /// The shared goals the body's check took (blaze-solve share.rs `GoalHit`: the goal's key
    /// and its read digest), in the order taken: the one read each hit is. TODAY each hit's
    /// own reads are in `reads` too (share.rs, "A HIT IS ONE READ"); the engine names a goal
    /// as a cell of its own once goals are records.
    pub goal_hits: AVec<'a, blaze_solve::share::GoalHit>,
    /// The reads the body's recorder was offered and kept (recorder.rs [`ReadTally`](super::recorder::ReadTally)):
    /// the Bodies stage's reads line.
    pub read_tally: super::recorder::ReadTally,
}

/// A body task's answer by [`check_body_split`], in TWO allocators (rule 11: by lifetime): what
/// outlives the body (the records made inline, `made`) in the caller's `out`, and every list
/// that is only a step toward the row (the recorder's list, the cells before the engine widens
/// them, the control's `held`, the verdict's words and bytes) in its `work`, which the caller
/// frees as the body ends (blaze-cli engine_bodies.rs: the body's scratch frame, reset per
/// body). The row itself is made by the caller from `cells` ([`BodyChecked::done_in`]), sized
/// to what it keeps: before, the row's list was `cells` (in the request's arena, sized to the
/// recorder's list) and every list beside it was in the request's arena too.
pub struct BodyChecked<'a, 'w> {
    /// The verdict's digest (`Done::out`).
    pub out: ContentId,
    /// Whether the body was refused, left a gap, or its borrows were refused (`Done::failed`).
    pub failed: bool,
    /// The type, lint and borrow findings (`Done::findings`).
    pub findings: u32,
    /// Each cell the body read with the digest [`Make`] gave, sorted by cell and each once.
    pub cells: AVec<'w, (CellId, ContentId)>,
    pub made: SVec<'a, Made<'a>>,
    /// As [`BodyOut`]'s fields of the same names.
    pub bytes: AVec<'w, u8>,
    pub reads: AVec<'w, TableRead>,
    pub held: AVec<'w, (TableRead, u128)>,
    pub said: AVec<'w, Said<'w>>,
    pub goal_hits: AVec<'w, blaze_solve::share::GoalHit>,
    pub read_tally: super::recorder::ReadTally,
    /// What the check's working lists took in `work` ([`WorkBytes`]).
    pub work: WorkBytes,
    /// As [`BodyReads::typed`].
    pub typed: Option<AVec<'w, u8>>,
}

/// The bytes a body's check took in its `work` allocator, by kind, counted from the lists'
/// capacities as they stood (no walk; rule 16: the Bodies stage's memory line, blaze-cli
/// engine_bodies.rs `BodyMem`). Each is freed with the body; each one's floor is said beside it.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct WorkBytes {
    /// The recorder's tables at their most, its list and its `seen` set (recorder.rs
    /// [`Recorder::table_bytes`]); floor: its distinct reads as one list
    /// ([`WorkBytes::recorder_floor`]).
    pub recorder: u64,
    /// The recorder's distinct reads as one list: `kept` reads of `size_of::<TableRead>()`.
    pub recorder_floor: u64,
    /// The cells before the engine widens them (`cells`); floor: the same, it is one list.
    pub cells: u64,
    /// The control's instrument (`held`, and the keys it is made from); floor: 0 (only tests read it).
    pub held: u64,
    /// The verdict's lines and their words (`said`); floor: the words.
    pub said: u64,
    /// The verdict's canonical bytes (`bytes`); floor: 0, its digest `out` is what is kept.
    pub verdict: u64,
    /// The shared goals taken (`goal_hits`).
    pub goal_hits: u64,
}

impl<'a, 'w> BodyChecked<'a, 'w> {
    /// The engine's row of this answer for `task` under `name`, in `alloc`: its reads `cells`
    /// as they are (no widening), one block of their length; its one define the verdict cell
    /// unless it failed.
    pub fn done_in<'r>(&self, task: &Task, name: Name, alloc: SharedAlloc<'r>) -> Done<'r> {
        let mut reads: SVec<'r, (CellId, ContentId)> = SVec::with_capacity_in(self.cells.len(), alloc);
        reads.extend_from_slice(&self.cells);
        let mut defines: SVec<'r, (Cell, ContentId)> = SVec::with_capacity_in(usize::from(!self.failed), alloc);
        if !self.failed {
            defines.push((VERDICT_CELL, self.out));
        }
        Done { name, key: task.key, out: self.out, defines, reads, findings: self.findings, failed: self.failed }
    }
}

/// One line of a body's verdict ([`BodyOut::said`]): `start..end` is item-relative (rule 71).
#[derive(Debug)]
pub struct Said<'a> {
    /// Path below the task's root, e.g. `mid::inner`. The root's own lines have no prefix.
    pub within: Option<AString<'a>>,
    pub what: SaidWhat<'a>,
    pub start: u32,
    pub end: u32,
    /// The finding's words; for a body not checked, the words after `body `<name>`: `.
    pub words: AString<'a>,
}

/// Which line a [`Said`] is.
#[derive(Debug)]
pub enum SaidWhat<'a> {
    /// A type error, by its code.
    Type { code: &'static str },
    /// A typed lint's finding, by the lint's name (blaze-lint `Rule::from_name`).
    Lint { lint: &'static str },
    /// A move or borrow error, by its code.
    Borrow { code: &'static str },
    /// The body was not checked: refused or past a gap, grouped under `cause` (said.rs). A
    /// failed macro call's expansion (body_types.rs `failed_calls`) is not split out yet.
    NotChecked { cause: AString<'a> },
    /// The borrow check was refused (not for want of clean types).
    BorrowsNotChecked,
}

/// The body `task` names, checked over `tables` (module docs), every list in `shared`: the
/// tests' form, the answer and its row in one allocator.
pub fn check_body<'n, 'a>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, make: &dyn Make<'a>, shared: SharedAlloc<'a>) -> Result<BodyOut<'a>, TaskError> {
    check_body_in(tables, task, frame, make, shared, Coverage::WithLocals)
}

pub(super) enum Coverage {
    WithLocals,
    #[cfg(test)]
    RootOnly,
}

/// [`check_body`] with its coverage named (local_fns_tests.rs: `RootOnly`, the control), every
/// list in `shared`: [`check_split`] with one allocator for both, its row made from its cells.
pub(super) fn check_body_in<'n, 'a>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, make: &dyn Make<'a>, shared: SharedAlloc<'a>, coverage: Coverage) -> Result<BodyOut<'a>, TaskError> {
    let checked = check_split(tables, task, frame, make, shared.local(), shared, coverage)?;
    let done = checked.done_in(task, task.planned.name, shared);
    let BodyChecked { made, bytes, reads, held, said, goal_hits, read_tally, .. } = checked;
    Ok(BodyOut { done, made, bytes, reads, held, said, goal_hits, read_tally })
}

/// The body `task` names and every local fn it reaches, checked over `tables` (module docs), in
/// two allocators ([`BodyChecked`]): `work`, where the check works and which the caller frees as
/// the body ends, and `out`, where what outlives the body (the records made inline) goes. The
/// row is the caller's to make from the answer's `cells` (the engine widens them first).
pub fn check_body_split<'n, 'a, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, make: &dyn Make<'a>, work: Alloc<'w>, out: SharedAlloc<'a>) -> Result<BodyChecked<'a, 'w>, TaskError> {
    check_split(tables, task, frame, make, work, out, Coverage::WithLocals)
}

/// A verdict and the distinct questions that produced it. No ID has been derived and no
/// engine cell has been looked up. A stretch places and combines these with its other reads.
pub struct BodyReads<'w> {
    pub out: ContentId,
    pub failed: bool,
    pub findings: u32,
    pub reads: AVec<'w, TableRead>,
    pub said: AVec<'w, Said<'w>>,
    pub read_tally: super::recorder::ReadTally,
    pub work: WorkBytes,
    /// The defining texts of the constants the body's evaluator read (project.rs
    /// `record_const_texts`): each is a read the engine places beside `reads`.
    pub const_texts: AVec<'static, TableRead>,
    /// The library key of each fn, const and static declared in a body the check reached (`each_from` past the
    /// root, at any depth), in the order reached: bodies this check checked that are no outline
    /// entry of their own, so the caller lists them in its covers (blaze-cli engine_bodies.rs,
    /// work.rs `covered`) and a call of one finds its row. Empty for `Coverage::RootOnly`.
    pub locals: AVec<'w, ItemKey>,
    /// The ROOT body's checked record (blaze-typeck checked_record.rs, D163 R1), in `work`:
    /// `Some` for a root whose types are clean, whatever its borrows say (`keeps_typed`; front2
    /// borrows.rs `check_borrows_in` encodes it): the caller puts it as it is, under the body's
    /// check name.
    pub typed: Option<AVec<'w, u8>>,
    /// The root's types are clean but it has no checked record because its check left stalled
    /// goals, decided after the record (front2 stalled_goals.rs): the run meets a `TypedGap`
    /// for it. The run's census sums this flag over its bodies (the engine's Bodies stage, which
    /// reads `BodyReads`), so a tempo run says how many such gaps it has.
    pub typed_stalled: bool,
    bytes: AVec<'w, u8>,
    held: AVec<'w, (TableRead, u128)>,
    goal_hits: AVec<'w, blaze_solve::share::GoalHit>,
    /// What the one ask counted in the body's store (front2 asks.rs; zero under `Again`).
    asked: blaze_ask::Counts,
}

pub fn check_body_reads<'n, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, work: Alloc<'w>) -> Result<BodyReads<'w>, TaskError> {
    record_body(tables, task, frame, work, Coverage::WithLocals, false)
}

/// [`check_body_reads`] while retaining the recorder for the caller when checking unwinds or
/// refuses with [`ReadsFull`].
pub fn check_body_reads_observed<'n, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, work: Alloc<'w>, recorder: &mut Option<Recorder<'w>>, keyer: Option<&dyn ReadKeyer>) -> Result<BodyReads<'w>, TaskError> {
    let active = recorder.get_or_insert_with(|| Recorder::new(RecorderOpts { alloc: work }));
    let body = record_body_observed(tables, task, frame, work, active, keyer, Coverage::WithLocals, ReadReport::Distinct, None)?;
    drop(recorder.take());
    Ok(body)
}

/// One item of an outline entry. Use the same immutable tables and placement policy
/// for every item sharing this recorder. Each item returns the cells it first observed
/// and its own verdict digest before the next item reuses the encoding buffer.
pub fn check_body_item<'n, 'w>(
    tables: Tables<'_, 'n>,
    task: &Task,
    frame: &BodyFrame<'n>,
    work: Alloc<'w>,
    recorder: &mut Recorder<'w>,
    keyer: Option<&dyn ReadKeyer>,
    encoding: &mut AString<'w>,
    stretch_store: Option<&mut StretchStore<'n>>,
) -> Result<BodyReads<'w>, TaskError> {
    record_body_observed(tables, task, frame, work, recorder, keyer, Coverage::WithLocals, ReadReport::Entry(encoding), stretch_store)
}

/// The one check behind [`check_body`], [`check_body_in`] and [`check_body_split`].
fn check_split<'n, 'a, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, make: &dyn Make<'a>, work: Alloc<'w>, out: SharedAlloc<'a>, coverage: Coverage) -> Result<BodyChecked<'a, 'w>, TaskError> {
    let BodyReads { out: digest, failed, findings, reads, bytes, held, said, goal_hits, read_tally, work: mut work_bytes, typed, .. } = record_body(tables, task, frame, work, coverage, true)?;
    let mut made = SVec::new_in(out);
    let mut cells = AVec::with_capacity_in(reads.len(), work);
    for read in &reads {
        let Some(id) = cell_id(tables, *read)? else { continue };
        let saw = make.make(&id, &mut made).map_err(TaskError::Make)?;
        cells.push((id, saw));
    }
    cells.sort_unstable_by_key(|pair| pair.0);
    cells.dedup_by_key(|pair| pair.0);
    work_bytes.cells = (cells.capacity() * core::mem::size_of::<(CellId, ContentId)>()) as u64;
    Ok(BodyChecked { out: digest, failed, findings, cells, made, bytes, reads, held, said, goal_hits, read_tally, work: work_bytes, typed })
}

fn record_body<'n, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, work: Alloc<'w>, coverage: Coverage, sorted_report: bool) -> Result<BodyReads<'w>, TaskError> {
    let mut recorder = Recorder::new(RecorderOpts { alloc: work });
    let report = if sorted_report { ReadReport::Sorted } else { ReadReport::Distinct };
    record_body_observed(tables, task, frame, work, &mut recorder, None, coverage, report, None)
}

enum ReadReport<'a, 'w> {
    Sorted,
    Distinct,
    Entry(&'a mut AString<'w>),
}

fn record_body_observed<'n, 'w>(
    tables: Tables<'_, 'n>,
    task: &Task,
    frame: &BodyFrame<'n>,
    work: Alloc<'w>,
    recorder: &mut Recorder<'w>,
    keyer: Option<&dyn ReadKeyer>,
    coverage: Coverage,
    report: ReadReport<'_, 'w>,
    stretch_store: Option<&mut StretchStore<'n>>,
) -> Result<BodyReads<'w>, TaskError> {
    // D161 step 3's oracle over EVERY body any test checks (`--features asks-oracle`): the body
    // run both ways first, their answers equal or the test fails naming both.
    #[cfg(feature = "asks-oracle")]
    if let Err(differ) = compare_asks(tables, task, frame, work, super::super::asks::AsksPlant::Clean) {
        panic!("{differ}");
    }
    // The run memo's oracle over the same bodies (recorder.rs `Repeats`): a repeated question
    // muted must leave every body's read list byte-equal.
    #[cfg(feature = "asks-oracle")]
    if let Err(differ) = compare_repeats(tables, task, frame, work, RepeatsPlant::Clean) {
        panic!("{differ}");
    }
    record_body_as(tables, task, frame, work, recorder, keyer, coverage, report, super::super::asks::AsksMet::Again, stretch_store)
}

/// Two answers of one body, one per side of D161 step 3's seam (front2 asks.rs `AsksMet`), that
/// differ: the body, what the one ask made by fact, and both results (rule 19).
pub struct AsksDiffer<'w> {
    pub body: Name,
    pub again: BodyReads<'w>,
    pub once: BodyReads<'w>,
    /// The `Once` side's failure, where it failed and the `Again` side did not (or both).
    pub errors: (Option<TaskError>, Option<TaskError>),
}

impl core::fmt::Display for AsksDiffer<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        writeln!(f, "D161 step 3 oracle: body {:?} answers differ between `AsksMet::Again` and `AsksMet::Once` (front2 asks.rs)", self.body)?;
        let asked = &self.once.asked;
        write!(f, "the one ask: ")?;
        for fact in blaze_ask::Fact::ALL {
            let i = fact.index();
            if asked.asked[i] + asked.made[i] > 0 {
                write!(f, "{} asked {} made {}; ", fact.name(), asked.asked[i], asked.made[i])?;
            }
        }
        writeln!(f, "read {} failed {}", asked.read, asked.failed)?;
        if let Some(e) = &self.errors.0 {
            writeln!(f, "again refused: {e}")?;
        }
        if let Some(e) = &self.errors.1 {
            writeln!(f, "once refused: {e}")?;
        }
        for (side, r) in [("again", &self.again), ("once", &self.once)] {
            writeln!(f, "--- {side}: out {:?} failed {} findings {} reads {} locals {}", r.out, r.failed, r.findings, r.reads.len(), r.locals.len())?;
            if let Some(at) = self.again.reads.iter().zip(self.once.reads.iter()).position(|(a, b)| a != b) {
                writeln!(f, "first read apart at {at}: {:?}", r.reads[at])?;
            }
            writeln!(f, "{}", core::str::from_utf8(&r.bytes).unwrap_or("(the verdict text is not UTF-8)"))?;
        }
        Ok(())
    }
}

impl core::fmt::Debug for AsksDiffer<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(self, f)
    }
}

/// Whether the two sides of D161's seam agree on one body (the relation [`compare_asks`] holds):
/// - the same failure, and for a body that checked (clean or with findings) the same verdict
///   digest, findings, verdict bytes (which carry the final round's read-list digest), constant
///   texts, locals and typed record bytes;
/// - the `Once` side's read list a SUBSET of the `Again` side's (both sorted): under `Once`
///   over frozen heads the run stops at its first hole (asks.rs `Store::stops_at_holes`), so
///   every fill it makes is a question its final round asks; `Again` also keeps the reads of
///   the fills it made past a stand-in that its final round never asked (the superset the
///   module doc's INTERIM paragraph names);
/// - a body whose TYPES both sides checked clean (`typed` kept: [`keeps_typed`]) is held to all of
///   the above whatever its borrows say (a refused borrow check fails the task, both sides
///   alike, and its verdict bytes say so equally);
/// - for a body whose types a side did not check clean, the verdict bytes may differ only by
///   the gaps named: `Again` lists every gap of every round, speculative ones included; `Once`
///   the gaps met before its run stopped. Both must fail.
fn asks_agree(a: &BodyReads<'_>, o: &BodyReads<'_>) -> bool {
    let subset = o.reads.as_slice().iter().all(|read| a.reads.as_slice().binary_search(read).is_ok());
    let types_clean = a.typed.is_some() && o.typed.is_some();
    if (a.failed || o.failed) && !types_clean {
        return a.failed == o.failed && subset;
    }
    a.out == o.out
        && a.failed == o.failed
        && a.findings == o.findings
        && a.bytes.as_slice() == o.bytes.as_slice()
        && subset
        && a.const_texts.as_slice() == o.const_texts.as_slice()
        && a.locals.as_slice() == o.locals.as_slice()
        && a.typed.as_ref().map(|b| b.as_slice()) == o.typed.as_ref().map(|b| b.as_slice())
}

/// D161 step 3's ORACLE (rule 78: each instantiation of the seam tested equal): `task`'s body
/// checked under `AsksMet::Again` and under `AsksMet::Once(plant)`, each with a fresh recorder,
/// no keyer, no meter and no shared goal table (a shared table filled by the first run would
/// give the second hits the first never had), its read list sorted, over the frame's heads or,
/// for a frame with none, a fresh frozen seed a side ([`oracle_seed`]); the two agree as
/// [`asks_agree`] says, or both runs refused alike. A seed refusal of the oracle's own seed is
/// said on its own `asks-oracle SEED REFUSED` line and passes (counted apart, not a
/// difference). `plant` is the control's (`AsksPlant::RefuseMade` must differ on a seeded body
/// that imports a head).
pub fn compare_asks<'n, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, work: Alloc<'w>, plant: super::super::asks::AsksPlant) -> Result<(), AsksDiffer<'w>> {
    use super::super::asks::AsksMet;
    let run = |met: AsksMet, heads: Option<&'n dyn HeadSeed<'n>>| {
        let shadow = BodyFrame { frame: frame.frame, alloc: frame.alloc, plant_drop: frame.plant_drop, cost: None, needs: frame.needs, heads, sources: frame.sources, goals: None, prepared: frame.prepared, front: frame.front };
        let mut recorder = Recorder::new(RecorderOpts { alloc: work });
        record_body_as(tables, task, &shadow, work, &mut recorder, None, Coverage::WithLocals, ReadReport::Sorted, met, None)
    };
    // An unseeded body (a lowering store, where `Once` would meet its heads as `Again` does)
    // gets frozen heads of its own, one fresh seed a side ([`oracle_seed`]), so the oracle
    // exercises the one ask on EVERY body. A head the seed cannot freeze (no constant
    // evaluator, receiver pointers not looked up: the record form's known limits) is a seed
    // refusal, said on its own line and counted apart, never a difference.
    let (again, once) = match frame.heads {
        Some(heads) => (run(AsksMet::Again, Some(heads)), run(AsksMet::Once(plant), Some(heads))),
        None => {
            let side = |met: AsksMet| {
                let seed = oracle_seed(tables, frame);
                let got = run(met, Some(seed as &'n dyn HeadSeed<'n>));
                release_oracle_seed(tables, frame, seed);
                got
            };
            let (again, once) = (side(AsksMet::Again), side(AsksMet::Once(plant)));
            if let Some(error) = [&again, &once].into_iter().find_map(|got| got.as_ref().err().filter(|e| matches!(e, TaskError::Seed { .. }))) {
                std::eprintln!("asks-oracle SEED REFUSED (counted apart, not a difference): {error}");
                return Ok(());
            }
            (again, once)
        }
    };
    let empty = || BodyReads {
        out: ContentId([0; 32]),
        failed: true,
        findings: 0,
        reads: AVec::new_in(work),
        said: AVec::new_in(work),
        read_tally: Default::default(),
        work: Default::default(),
        const_texts: AVec::new_in(frame.alloc),
        locals: AVec::new_in(work),
        typed: None,
        typed_stalled: false,
        bytes: AVec::new_in(work),
        held: AVec::new_in(work),
        goal_hits: AVec::new_in(work),
        asked: blaze_ask::Counts::default(),
    };
    let body = task.planned.name;
    match (again, once) {
        (Ok(a), Ok(o)) => {
            if asks_agree(&a, &o) { Ok(()) } else { Err(AsksDiffer { body, again: a, once: o, errors: (None, None) }) }
        }
        // Both refused: equal when they refuse with one text (`TaskError` has no `Eq`).
        (Err(a), Err(o)) => {
            let mut at = AString::new_in(work);
            let mut ot = AString::new_in(work);
            let shown = write!(at, "{a}").is_ok() && write!(ot, "{o}").is_ok();
            if shown && at.as_str() == ot.as_str() { Ok(()) } else { Err(AsksDiffer { body, again: empty(), once: empty(), errors: (Some(a), Some(o)) }) }
        }
        (Ok(a), Err(o)) => Err(AsksDiffer { body, again: a, once: empty(), errors: (None, Some(o)) }),
        (Err(a), Ok(o)) => Err(AsksDiffer { body, again: empty(), once: o, errors: (Some(a), None) }),
    }
}

/// Fresh frozen heads for one side of [`compare_asks`] over a body its frame gives none: the
/// tests' seed (front2_tables_cost.rs's `Freezing`) over the tables' records, with `sized`
/// from the lang crates, no constant evaluator and receiver pointers not looked up (a head
/// that needs either is a seed refusal, counted apart). Placed in the body's frame (`'n`),
/// whose bump never runs its destructor: [`release_oracle_seed`] drops what it froze.
fn oracle_seed<'n>(tables: Tables<'_, 'n>, frame: &BodyFrame<'n>) -> &'n super::seeds::Freezing<'n, 'n, &'n Records<'n>> {
    let names = tables.records();
    frame.frame.alloc(super::seeds::Freezing::new(names, oracle_heads(tables, frame)))
}

/// The empty frozen heads [`oracle_seed`] starts from.
fn oracle_heads<'n>(tables: Tables<'_, 'n>, frame: &BodyFrame<'n>) -> super::head::SeededHeads<'n, 'n, &'n Records<'n>> {
    let names = tables.records();
    let mut sized = None;
    for &at in tables.lang_crates() {
        NamesSource::each_lang(names, at, &mut |name, key| {
            if name == "sized" {
                sized = Some(key);
            }
        });
    }
    super::head::SeededHeads::new(super::head::SeededHeadsOpts { names, sized, pointers: super::super::receiver::Pointers::NOT_LOOKED_UP, frame: frame.frame, scratch: frame.alloc, alloc: frame.alloc, consts: None, known: None, owners: None })
}

/// Drops what an [`oracle_seed`] froze (its cells are in `frame.alloc`, which the frame's bump
/// would never free): its heads are swapped for empty ones, which hold nothing.
fn release_oracle_seed<'n>(tables: Tables<'_, 'n>, frame: &BodyFrame<'n>, seed: &super::seeds::Freezing<'n, 'n, &'n Records<'n>>) {
    drop(seed.heads.replace(oracle_heads(tables, frame)));
}

/// The enter-once cost test's entry (front2 tables/enter_once_tests.rs): `task`'s body on one
/// side of D161's seam, `met`, with the frame as given (its meter included, which
/// [`compare_asks`] drops), a fresh recorder and its read list sorted.
#[cfg(test)]
pub(crate) fn record_body_met<'n, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, work: Alloc<'w>, met: super::super::asks::AsksMet) -> Result<BodyReads<'w>, TaskError> {
    let mut recorder = Recorder::new(RecorderOpts { alloc: work });
    record_body_as(tables, task, frame, work, &mut recorder, None, Coverage::WithLocals, ReadReport::Sorted, met, None)
}

#[cfg(test)]
impl<'w> BodyReads<'w> {
    /// The verdict's bytes (what `out` digests).
    pub(crate) fn verdict_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    /// What the one ask counted in the body's store (zero under `Again`).
    pub(crate) fn asked_counts(&self) -> blaze_ask::Counts {
        self.asked
    }

    /// The head read of each head the body's store held after the check, with its key.
    pub(crate) fn held_heads(&self) -> &[(TableRead, u128)] {
        self.held.as_slice()
    }

    /// Whether the two sides of D161's seam agree on this body ([`asks_agree`]): `self` the
    /// `Again` side's, `once` the `Once` side's, both with sorted read lists.
    pub(crate) fn agrees_with_once(&self, once: &BodyReads<'_>) -> bool {
        asks_agree(self, once)
    }

    /// What of [`asks_agree`]'s relation fails between `self` (the `Again` side) and `once`, by
    /// field: each a line naming the field and how it differs (the first byte apart, the
    /// lengths, the `Once` reads missing from the `Again` list). Empty when they agree.
    pub(crate) fn once_differs(&self, once: &BodyReads<'_>) -> std::string::String {
        let mut s = std::string::String::new();
        let first_apart = |a: &[u8], b: &[u8]| a.iter().zip(b.iter()).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
        if self.out != once.out {
            let _ = writeln!(s, "out: {:?} against {:?}", self.out, once.out);
        }
        if self.failed != once.failed {
            let _ = writeln!(s, "failed: {} against {}", self.failed, once.failed);
        }
        if self.findings != once.findings {
            let _ = writeln!(s, "findings: {} against {}", self.findings, once.findings);
        }
        let (ab, ob) = (self.bytes.as_slice(), once.bytes.as_slice());
        if ab != ob {
            let at = first_apart(ab, ob);
            let near = |b: &[u8]| std::string::String::from_utf8_lossy(&b[at.saturating_sub(40).min(b.len())..(at + 80).min(b.len())]).into_owned();
            let _ = writeln!(s, "verdict bytes: {} against {} bytes, first apart at {at}: again `{}` once `{}`", ab.len(), ob.len(), near(ab), near(ob));
        }
        let missing: std::vec::Vec<&TableRead> = once.reads.as_slice().iter().filter(|r| self.reads.as_slice().binary_search(r).is_err()).collect();
        if !missing.is_empty() {
            let _ = writeln!(s, "reads: {} of the one ask's {} are not in the restart path's {} (the first: {:?})", missing.len(), once.reads.len(), self.reads.len(), missing.first());
        }
        if self.const_texts.as_slice() != once.const_texts.as_slice() {
            let _ = writeln!(s, "const texts: {} against {}", self.const_texts.len(), once.const_texts.len());
        }
        if self.locals.as_slice() != once.locals.as_slice() {
            let _ = writeln!(s, "locals: {} against {}", self.locals.len(), once.locals.len());
        }
        match (self.typed.as_ref().map(|b| b.as_slice()), once.typed.as_ref().map(|b| b.as_slice())) {
            (Some(a), Some(o)) if a != o => {
                let _ = writeln!(s, "typed record: {} against {} bytes, first apart at {}", a.len(), o.len(), first_apart(a, o));
            }
            (a, o) if a.is_some() != o.is_some() => {
                let _ = writeln!(s, "typed record: kept {} against {}", a.is_some(), o.is_some());
            }
            _ => {}
        }
        s
    }
}

/// The planted bug of [`compare_repeats`]'s control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepeatsPlant {
    Clean,
    /// PLANTED BUG: the `Quiet` side drops every read of the first names cell the `Offer`
    /// side kept, as a memo that muted a first answer would. The oracle must see the lists
    /// differ.
    DropFirst,
}

/// Two read lists of one body, one per side of the recorder's repeats seam (recorder.rs
/// `Repeats`), that differ: the body, the first read apart, and both tallies (rule 19).
pub struct RepeatsDiffer<'w> {
    pub body: Name,
    pub offer: Result<BodyReads<'w>, TaskError>,
    pub quiet: Result<BodyReads<'w>, TaskError>,
}

impl core::fmt::Display for RepeatsDiffer<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        writeln!(f, "repeats oracle: body {:?} reads differ between `Repeats::Offer` and `Repeats::Quiet` (front2 tables/recorder.rs)", self.body)?;
        match (&self.offer, &self.quiet) {
            (Ok(o), Ok(q)) => {
                writeln!(f, "offer: {} reads, out {:?}, tally {:?}", o.reads.len(), o.out, o.read_tally)?;
                writeln!(f, "quiet: {} reads, out {:?}, tally {:?}", q.reads.len(), q.out, q.read_tally)?;
                match o.reads.iter().zip(q.reads.iter()).position(|(a, b)| a != b) {
                    Some(at) => writeln!(f, "first read apart at {at}: offer {:?}, quiet {:?}", o.reads[at], q.reads[at]),
                    None => writeln!(f, "one list is a prefix of the other"),
                }
            }
            (o, q) => {
                for (side, r) in [("offer", o), ("quiet", q)] {
                    match r {
                        Ok(r) => writeln!(f, "{side}: {} reads, out {:?}", r.reads.len(), r.out)?,
                        Err(e) => writeln!(f, "{side} refused: {e}")?,
                    }
                }
                Ok(())
            }
        }
    }
}

impl core::fmt::Debug for RepeatsDiffer<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(self, f)
    }
}

/// THE ORACLE of the run memo (rule 78: each side of the seam tested equal): `task`'s body
/// checked with a recorder under `Repeats::Offer` (every repeated question's reads offered
/// again) and one under `Repeats::Quiet` (none), each fresh, with no keyer, no meter and no
/// shared goal table, its read list sorted: equal verdict digests, failure, findings, verdict
/// bytes and READ LISTS byte for byte, or both refused with one text. The `Quiet` side's
/// tally has `memo` > 0 where a run asked a question twice. `plant` is the control's
/// ([`RepeatsPlant::DropFirst`] must differ on any body that reads a names cell). Equal: the
/// two sides' tallies, `Offer`'s then `Quiet`'s (zero for two equal refusals).
pub fn compare_repeats<'n, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, work: Alloc<'w>, plant: RepeatsPlant) -> Result<(super::recorder::ReadTally, super::recorder::ReadTally), RepeatsDiffer<'w>> {
    use super::recorder::Repeats;
    let shadow = BodyFrame { frame: frame.frame, alloc: frame.alloc, plant_drop: frame.plant_drop, cost: None, needs: frame.needs, heads: frame.heads, sources: frame.sources, goals: None, prepared: frame.prepared, front: frame.front };
    let run = |repeats: Repeats, drop: Option<NameCell>| {
        let mut recorder = Recorder::new(RecorderOpts { alloc: work });
        recorder.set_repeats(repeats);
        if let Some(cell) = drop {
            recorder.plant_drop(cell);
        }
        record_body_as(tables, task, &shadow, work, &mut recorder, None, Coverage::WithLocals, ReadReport::Sorted, super::super::asks::AsksMet::Again, None)
    };
    let offer = run(Repeats::Offer, None);
    let drop = match (plant, &offer) {
        (RepeatsPlant::DropFirst, Ok(o)) => o.reads.iter().find_map(|r| match r {
            TableRead::Names(cell) => Some(*cell),
            _ => None,
        }),
        _ => None,
    };
    let quiet = run(Repeats::Quiet, drop);
    let equal = match (&offer, &quiet) {
        (Ok(a), Ok(o)) => a.out == o.out && a.failed == o.failed && a.findings == o.findings && a.bytes.as_slice() == o.bytes.as_slice() && a.reads.as_slice() == o.reads.as_slice(),
        (Err(a), Err(o)) => {
            let mut at = AString::new_in(work);
            let mut ot = AString::new_in(work);
            write!(at, "{a}").is_ok() && write!(ot, "{o}").is_ok() && at.as_str() == ot.as_str()
        }
        _ => false,
    };
    if !equal {
        return Err(RepeatsDiffer { body: task.planned.name, offer, quiet });
    }
    Ok(match (&offer, &quiet) {
        (Ok(o), Ok(q)) => (o.read_tally, q.read_tally),
        _ => Default::default(),
    })
}

/// Borrows a body task's resettable scratch allocator (`work: Alloc<'w>`) as `Alloc<'static>`
/// for a [`ProjectWorld`] created inside [`record_body_as`] and dropped before it returns.
///
/// # Safety
///
/// Every value allocated through the returned handle (the `ProjectWorld`, its `Store`,
/// `Buckets`, `InferCtx`, `Checked`, and any temporary scratch passed to `HeadSeed::seed`)
/// MUST be dropped before `'w` ends (before [`record_body_as`] returns or finishes unwinding),
/// and no copy of the handle or allocation backed by it may escape into [`BodyReads`],
/// [`TaskError`], `frame`, `tables`, or any shared state.
#[inline]
unsafe fn body_world_alloc<'w>(work: Alloc<'w>) -> Alloc<'static> {
    // SAFETY: `Alloc<'w>` and `Alloc<'static>` are instantiations of the same struct differing
    // only in a phantom lifetime marker, so their layouts are identical; the caller enforces
    // that every allocation made through the returned handle is dropped before `'w` ends.
    unsafe { core::mem::transmute::<Alloc<'w>, Alloc<'static>>(work) }
}

/// Stack RAII guard owning the body's [`ProjectWorld`] and (when reusing a [`StretchStore`]) the
/// borrow of that stretch store.
///
/// Guarantees that on EVERY exit path (normal `Ok`, early `?` `Err`, or panic unwind), all
/// `work`-backed fields on `Store`, `Maps`, and `Keys` are detached via `end_body_reuse` BEFORE
/// `record_body_as` returns and BEFORE `BODY_SCRATCH` resets. Only [`Self::finish`] (invoked on
/// clean `Ok` completion) calls `StretchStore::put_back`; `Drop` always drops `Store` + `Keys`
/// and resets `heads_frame`.
struct ReuseHold<'n, 's> {
    ss: &'s mut StretchStore<'n>,
    heads_alloc: Alloc<'static>,
    snapshot: super::super::heads::ReuseSnapshot,
    facts_crate: Option<usize>,
}

struct BodyWorldGuard<'v, 'n, 'w, 's> {
    world: Option<blaze_own::Box<ProjectWorld<'n, Tables<'v, 'n>>, Alloc<'w>>>,
    reuse: Option<ReuseHold<'n, 's>>,
}

impl<'v, 'n, 'w, 's> BodyWorldGuard<'v, 'n, 'w, 's> {
    #[inline(never)]
    fn finish(mut self) {
        let Some(world) = self.world.take() else { return };
        if let Some(hold) = self.reuse.take() {
            let (mut store, mut keys) = blaze_own::Box::into_inner(world).into_store_and_keys();
            let facts = store.stretch_facts.take().unwrap_or_else(|| blaze_typeck::infer::FactCache::new_in(hold.heads_alloc));
            let status = store.end_body_reuse(hold.snapshot, &mut keys);
            hold.ss.put_back(
                StretchStoreInner {
                    store,
                    keys,
                    facts,
                    facts_crate: hold.facts_crate,
                    heads_alloc: hold.heads_alloc,
                },
                status,
            );
        }
    }
}

#[inline(never)]
fn detach_and_reset_reuse<'v, 'n, 'w>(
    world: blaze_own::Box<ProjectWorld<'n, Tables<'v, 'n>>, Alloc<'w>>,
    hold: ReuseHold<'n, '_>,
) {
    let ReuseHold { ss, heads_alloc: _, snapshot, facts_crate: _ } = hold;
    let (mut store, mut keys) = blaze_own::Box::into_inner(world).into_store_and_keys();
    let _ = store.stretch_facts.take();
    let _ = store.end_body_reuse(snapshot, &mut keys);
    drop(store);
    drop(keys);
    ss.reset_frame();
}

impl Drop for BodyWorldGuard<'_, '_, '_, '_> {
    fn drop(&mut self) {
        if let Some(world) = self.world.take()
            && let Some(hold) = self.reuse.take()
        {
            detach_and_reset_reuse(world, hold);
        }
    }
}

/// The body's world over `view`: its lang index, its `Front2` and the world itself, made in
/// THIS frame and boxed in `alloc`. A debug build gives each local and temporary of a
/// function its own slot for the whole call, and [`record_body_as`]'s frame stays on the
/// stack under the whole type and borrow check (blaze-int notes t29 overflow-bt4.txt: 156,400
/// bytes, the deepest of front2's): made there, the index, `Front2` and the world's value
/// would ride under it; made here, only the box's pointer does.
///
/// When `stretch_store` is `Some` and `frame.heads` is `Some` (seeded body), reuses the stretch's
/// persistent `Store` + `Keys` + `FactCache` in `HEADS_STRETCH_FRAME` while binding all per-body
/// scratch tables to `world_alloc` (`BODY_SCRATCH`); otherwise allocates everything in
/// `world_alloc`.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn body_world<'v, 'n, 'w, 's>(
    view: Tables<'v, 'n>,
    sink: &dyn ReadSink,
    name: Name,
    own: ItemKey,
    frame: &BodyFrame<'n>,
    asks: super::super::asks::AsksMet,
    alloc: Alloc<'w>,
    stretch_store: Option<&'s mut StretchStore<'n>>,
) -> Result<BodyWorldGuard<'v, 'n, 'w, 's>, TaskError> {
    // SAFETY: `guard` (and `world` inside it) is owned locally in `record_body_as` and either
    // finished via `BodyWorldGuard::finish` or dropped via `BodyWorldGuard::drop` strictly before
    // `record_body_as` returns while `alloc: Alloc<'w>` is live. Both `finish` and `drop` call
    // `Store::end_body_reuse`, detaching every `world_alloc`-backed field before `Store` is
    // returned to `StretchStore` or dropped.
    let world_alloc = unsafe { body_world_alloc(alloc) };
    let (index, pointers) = match frame.front {
        Some(sf) => {
            for &read in sf.reads.iter() {
                sink.read(read);
            }
            // Per-stretch incoherent-crate registration is idempotent (early-returns after first
            // body in the stretch) and must still run when `StretchFront` is shared across stretches.
            view.records().name_lang_crates(view.lang_crates());
            (sf.index.borrow(), Some(sf.pointers))
        }
        None => {
            let lang_crates = view.lang_crates();
            let index = Index::from_source(view, lang_crates).map_err(TaskError::Lang)?;
            view.records().name_lang_crates(lang_crates);
            (index, None)
        }
    };
    let mut guard = match (stretch_store, frame.heads.is_some()) {
        (Some(ss), true) => {
            // Always resolve receiver pointers when `front` is `None` so read lists match a fresh
            // world even without a precomputed `StretchFront` (rule 21).
            let resolved_pointers = pointers.unwrap_or_else(|| super::super::receiver::Pointers::find(&index, crate::resolve::Paths { names: view }));
            let StretchStoreInner { mut store, mut keys, mut facts, mut facts_crate, heads_alloc } =
                ss.take_or_init(index.lang("sized"), resolved_pointers, frame.frame);
            // Impl visibility (`Seen::of_checked_body`) is a function of the body's crate:
            // never share cached `ImplsOf` answers across bodies from different crates.
            let body_crate = view.crate_of(own);
            if facts_crate != body_crate {
                facts = blaze_typeck::infer::FactCache::new_in(heads_alloc);
                facts_crate = body_crate;
            }
            store.stretch_facts = Some(facts);
            let snapshot = store.begin_body_reuse(world_alloc, &mut keys);
            let front = Front2 { names: view, resolver: crate::resolve::Paths { names: view }, texts: Texts::empty(), keys, index, frame: frame.frame };
            let world = blaze_own::Box::new_in(ProjectWorld::over_with_store(front, store, frame.prepared, world_alloc), alloc);
            BodyWorldGuard {
                world: Some(world),
                reuse: Some(ReuseHold { ss, heads_alloc, snapshot, facts_crate }),
            }
        }
        _ => {
            let front = Front2 { names: view, resolver: crate::resolve::Paths { names: view }, texts: Texts::empty(), keys: Keys::new_in(world_alloc), index, frame: frame.frame };
            let world = blaze_own::Box::new_in(ProjectWorld::over_with_pointers(front, frame.prepared, pointers, world_alloc), alloc);
            BodyWorldGuard { world: Some(world), reuse: None }
        }
    };
    if let Some(world) = guard.world.as_mut() {
        world.record_const_texts();
        world.set_cost(frame.cost);
        world.set_needs(frame.needs);
        world.set_asks(asks).map_err(|why| TaskError::Asks { body: name, why })?;
        world.set_seeds(frame.heads);
        world.set_goal_share(frame.goals);
        if let Some(sources) = frame.sources {
            world.set_body_sources(sources);
        }
    }
    Ok(guard)
}

const _: () = assert!(core::mem::size_of::<blaze_own::Box<ProjectWorld<'static, Tables<'static, 'static>>, Alloc<'static>>>() == 2 * core::mem::size_of::<usize>());

/// [`record_body_observed`] on one side of D161 step 3's seam (`asks`).
#[allow(clippy::too_many_arguments)]
fn record_body_as<'n, 'w>(
    tables: Tables<'_, 'n>,
    task: &Task,
    frame: &BodyFrame<'n>,
    work: Alloc<'w>,
    recorder: &mut Recorder<'w>,
    keyer: Option<&dyn ReadKeyer>,
    coverage: Coverage,
    report: ReadReport<'_, 'w>,
    asks: super::super::asks::AsksMet,
    stretch_store: Option<&mut StretchStore<'n>>,
) -> Result<BodyReads<'w>, TaskError> {
    let name = task.planned.name;
    let setup = Cost::enter(frame.cost, Part::Setup);
    let alloc = work;
    if let Some(cell) = frame.plant_drop {
        recorder.plant_drop(cell);
    }
    let item_keyer = keyer.filter(|keyer| !keyer.keeps_item_keys());
    let observed = ObservedReads { recorder, tables, keyer, item_keyer, error: RefCell::new(None) };
    recorder.checking(Some("the body's lookup and lowering tables"));
    let view = tables.reading_into(&observed);
    let body = body_of(view, name)?;
    // The item whose head holds the body's signature: a member's is its container's.
    let own = match body {
        BodyRef::Member { container, .. } => container,
        BodyRef::Free(key) => key,
    };
    // In its own frame and behind a box: this frame rides under the whole check (the guard:
    // tests/front2_alias_opaques.rs `a_seeded_body_checks_on_a_small_stack`).
    let mut guard = body_world(view, &observed, name, own, frame, asks, alloc, stretch_store)?;
    drop(setup);
    let root = Listed { body, at: None, entry: None };
    let mut goal_hits = AVec::new_in(alloc);
    let mut seed_error = None;
    let check = |w: &mut ProjectWorld<'n, Tables<'_, 'n>>, b| {
        recorder.checking(Some("the body's type and borrow check"));
        let pair = w.check_borrows(b, alloc);
        if let Some(error) = w.take_seed_error() {
            seed_error.get_or_insert_with(|| {
                if let SeedError::Freeze(error @ FreezeError::Record { .. }) = error {
                    if let FreezeError::Record { error: super::super::head_record::HeadRecordError::MissingOpaqueRow { key, origin }, .. } = &error {
                        let ask = CellAsk::during_seed(&pair.0, *key);
                        let shown = FactDefinition::of(view.records(), *key, Some((origin.owner, origin.index)), alloc);
                        return TaskError::Cell { key: *key, ask, holder: origin.owner, shown, error };
                    }
                    return TaskError::Seed { body: name, error: SeedError::Freeze(error) };
                }
                TaskError::Seed { body: name, error }
            });
        }
        recorder.checking(Some("the body's shared goal hit list"));
        goal_hits.extend_from_slice(w.goal_hits());
        recorder.checking(Some("the body's reached verdict and local function lists"));
        pair
    };
    recorder.checking(Some("the body's reached verdict and local function lists"));
    let reached = {
        let Some(world) = guard.world.as_mut() else {
            return Err(TaskError::Write);
        };
        match coverage {
            Coverage::WithLocals => world.each_from(&[root], alloc, check),
            #[cfg(test)]
            Coverage::RootOnly => {
                let mut check = check;
                let mut one = AVec::new_in(alloc);
                one.push(Reached { listed: root, within: None, got: check(&mut *world, body) });
                one
            }
        }
    };
    if let Some(error) = seed_error {
        return Err(error);
    }
    let sorted_report = matches!(report, ReadReport::Sorted);
    let done = collect_body_reads(name, own, frame, alloc, &observed, view, guard, reached, report)?;
    let recorder_bytes = recorder.table_bytes();
    let (mut reads, mut read_tally) = recorder.take_item().map_err(TaskError::Full)?;
    recorder.checking(None);
    if sorted_report {
        reads.sort_unstable_by(|a, b| {
            read_tally.sort_compares += 1;
            a.cmp(b)
        });
    }
    let work_bytes = WorkBytes {
        recorder: recorder_bytes,
        recorder_floor: byte_size(reads.len(), core::mem::size_of::<TableRead>()),
        cells: 0,
        held: done.held_bytes,
        said: done.said_bytes,
        verdict: byte_size(done.verdict_capacity, 1),
        goal_hits: byte_size(goal_hits.capacity(), core::mem::size_of::<blaze_solve::share::GoalHit>()),
    };
    Ok(BodyReads {
        out: done.digest,
        failed: done.failed,
        findings: done.findings,
        bytes: done.bytes,
        reads,
        const_texts: done.const_texts,
        locals: done.locals,
        typed: done.typed,
        typed_stalled: done.typed_stalled,
        held: done.held,
        said: done.said,
        goal_hits,
        read_tally,
        work: work_bytes,
        asked: done.asked,
    })
}

fn byte_size(n: usize, each: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX).saturating_mul(u64::try_from(each).unwrap_or(u64::MAX))
}

struct CollectedBody<'w> {
    digest: ContentId,
    failed: bool,
    findings: u32,
    bytes: AVec<'w, u8>,
    const_texts: AVec<'static, TableRead>,
    locals: AVec<'w, ItemKey>,
    typed: Option<AVec<'w, u8>>,
    typed_stalled: bool,
    held: AVec<'w, (TableRead, u128)>,
    said: AVec<'w, Said<'w>>,
    asked: blaze_ask::Counts,
    verdict_capacity: usize,
    held_bytes: u64,
    said_bytes: u64,
}

/// Outlined post-check summary for [`record_body_as`] so its locals and temporaries do not stay
/// on the stack under `world.each_from` (keeping small-stack body checks well under 512 KiB).
#[inline(never)]
#[expect(clippy::too_many_arguments, reason = "outlined tail of record_body_as")]
fn collect_body_reads<'v, 'n, 'w>(
    name: Name,
    own: ItemKey,
    frame: &BodyFrame<'n>,
    alloc: Alloc<'w>,
    observed: &'v ObservedReads<'_, 'w, '_, 'n, '_>,
    view: Tables<'v, 'n>,
    mut guard: BodyWorldGuard<'v, 'n, 'w, '_>,
    mut reached: AVec<'w, Reached<'n, (BodyVerdict<'w>, BorrowVerdict<'w>)>>,
    report: ReadReport<'_, 'w>,
) -> Result<CollectedBody<'w>, TaskError> {
    let recorder = observed.recorder;
    let Some(world) = guard.world.as_mut() else {
        return Err(TaskError::Write);
    };
    // Each fn declared in a reached body: checked here, so covered by this check. A local is
    // always written `Free` (every_body.rs `each_from`); the root is the one with no `within`.
    recorder.checking(Some("the body's local function key list"));
    let mut locals: AVec<'w, ItemKey> = AVec::with_capacity_in(reached.len().saturating_sub(1), alloc);
    for one in reached.iter().filter(|one| one.within.is_some()) {
        if let BodyRef::Free(key) = one.listed.body {
            locals.push(key);
        }
    }
    let reading = Cost::enter(frame.cost, Part::Reads);
    let mut owned_bytes = AString::new_in(alloc);
    let sorted_report = matches!(report, ReadReport::Sorted);
    let bytes = match report {
        ReadReport::Entry(encoding) => encoding,
        ReadReport::Distinct | ReadReport::Sorted => &mut owned_bytes,
    };
    bytes.clear();
    let mut said = AVec::new_in(alloc);
    let mut failed = false;
    let mut findings = 0u32;
    for (i, one) in reached.iter().enumerate() {
        recorder.checking(Some("the verdict's dependency facts"));
        let (verdict, borrows) = &one.got;
        let facts = verdict.facts.as_ref().map_err(|e| TaskError::Facts(*e))?;
        // An internal-symbol declaration the body named was bound by searching crates' items:
        // that search is the body's read (front2 internal_symbol.rs `record_search`).
        super::super::internal_symbol::record_search(view, &world.store, facts);
        for row in facts.rows.iter() {
            if !fact_kind_has_read(row.fact.kind) || row.fact.key == 0 {
                continue;
            }
            // By the records, unrecorded: which cell holds a fact is the plan's, not the body's.
            // An opaque type's facts are its fn's head's (the fn's head files it); the default
            // of a provided trait fn's `impl Trait` is that fn's (front2 rpitit.rs `filed_as`).
            let asked = blaze_typeck::ItemKey(row.fact.key);
            let owner = world.store.opaques.get(&super::super::rpitit::filed_as(&world.store, asked)).copied();
            if let Some(read) = fact_read(view.records(), &world.front.keys, row.fact.kind, row.fact.key, owner, alloc)? {
                observed.read(read);
            }
        }
        recorder.checking(Some("the body's diagnostic lines and their words"));
        said.extend(said_of(verdict, borrows, view.records(), &world.front.keys, || local_path(&reached[..i], one.within, alloc), alloc)?);
        // The root has precisely its original bytes. Each local appends another complete verdict.
        recorder.checking(Some("the body's encoded verdict bytes"));
        write_verdict(bytes, verdict, borrows, Facts::Digest)?;
        failed |= verdict.refused.is_some() || !verdict.gaps.is_empty() || matches!(borrows.refused, Some(ref r) if !matches!(r, BorrowRefused::Untyped(_)));
        let count = verdict.diagnostics.len() + verdict.findings.len() + borrows.findings.len();
        findings = findings.saturating_add(u32::try_from(count).unwrap_or(u32::MAX));
    }
    // THE BODY'S OWN HEAD (rule 21): its signature (a member's: its container's cell) is an
    // input of every check of it, also one refused before any question (a trait member's
    // missing default body), so it is read whether or not the checker asked for it.
    recorder.checking(Some("the body's own head fact"));
    let own = super::super::keys::checker_of(own);
    if let Some(read) = fact_read(view.records(), &world.front.keys, FactKind::Head, own.0, None, alloc)? {
        observed.read(read);
    }
    // THE CONTROL'S INSTRUMENT: the cell of each head the body's own store holds, beside the
    // read list (tests: every one must be in it; the store makes a head only when asked).
    recorder.checking(Some("the body's held head list"));
    let mut held_keys: AVec<'_, u128> = AVec::new_in(alloc);
    if sorted_report {
        world.each_held_head(&mut |k| held_keys.push(k));
    }
    let mut held: AVec<'w, (TableRead, u128)> = AVec::with_capacity_in(held_keys.len(), alloc);
    for &k in held_keys.iter() {
        if let Some(read) = fact_read(view.records(), &world.front.keys, FactKind::Head, k, None, alloc)? {
            held.push((read, k));
        }
    }
    held.sort_unstable();
    recorder.checking(Some("the body's constant definition reads"));
    let world_const_texts = world.take_const_texts().map_err(TaskError::Full)?;
    let mut const_texts = AVec::new_in(frame.alloc);
    if !world_const_texts.is_empty() {
        const_texts.reserve(world_const_texts.len());
        const_texts.extend_from_slice(&world_const_texts);
    }
    drop(world_const_texts);
    let asked = *world.store.asked.borrow();
    if let Some(error) = observed.error.borrow_mut().take() {
        return Err(error);
    }
    guard.finish();
    let _ = view;
    // The root's verdict bytes, then each local fn's (a root with no local fn keeps its bytes).
    let digest = ContentId::of(OUT_DOMAIN, bytes.as_bytes());
    let verdict_capacity = bytes.capacity();
    let bytes = owned_bytes.into_bytes();
    let held_bytes = byte_size(held_keys.capacity(), core::mem::size_of::<u128>()) + byte_size(held.capacity(), core::mem::size_of::<(TableRead, u128)>());
    let said_bytes = byte_size(said.capacity(), core::mem::size_of::<Said<'_>>())
        + said.iter().map(|s| byte_size(s.words.capacity(), 1) + s.within.as_ref().map_or(0, |w| byte_size(w.capacity(), 1)) + if let SaidWhat::NotChecked { cause } = &s.what { byte_size(cause.capacity(), 1) } else { 0 }).sum::<u64>();
    drop(held_keys);
    drop(reading);
    // The root's checked record (D163 R1), already allocated in `work` beside the rest of the
    // answer, of every root whose TYPES are clean ([`keeps_typed`]); a clean check the record
    // refused fails the task by name (rule 3).
    let root_reached = reached.iter_mut().find(|one| one.within.is_none());
    let clean = root_reached.filter(|one| keeps_typed(&one.got.0));
    let (typed, typed_stalled) = match clean.and_then(|one| one.got.0.typed.take()) {
        Some(Ok(record)) => (Some(record), false),
        // Its stalled goals were not all proved on their exact interiors (front2 stalled_goals.rs;
        // proved ones keep the record, borrows.rs `check_borrows_in`, F18): a `TypedGap` for
        // the run, counted (`typed_stalled`), never a failed task (D163).
        Some(Err(blaze_typeck::checked_record::CheckedRecordError::NotClean { what: "stalled", .. })) => (None, true),
        Some(Err(why)) => return Err(TaskError::Typed { body: name, why }),
        None => (None, false),
    };
    // `findings` summed over the root and its local fns (u32, saturating), as the row's.
    Ok(CollectedBody { digest, failed, findings, bytes, const_texts, locals, typed, typed_stalled, held, said, asked, verdict_capacity, held_bytes, said_bytes })
}

/// Whether a root's checked record is kept (D163 R1, D197 root 1): when its TYPES are clean (no
/// diagnostic, no gap, no refusal), whatever its borrows say. The record holds the type
/// checker's columns, which a borrow verdict never changes. A body whose borrows were refused
/// (D151 `Typed`: types whole, borrows not checked) runs (blaze-cli run_ids.rs
/// `Checked::runs`), so it needs its record; that kind was checked a second time by every run
/// before (the borrow filter here). A body with a borrow finding or a lint finding keeps its
/// record too: whether it runs is the run gate's question, asked by its class, not this one.
fn keeps_typed(types: &BodyVerdict<'_>) -> bool {
    types.is_clean()
}

/// A local fn's path below the task root. The work list puts every parent before its children.
fn local_path<'n, 'a, T>(earlier: &[Reached<'n, T>], within: Option<(BodyRef<'n>, &'n str)>, alloc: Alloc<'a>) -> Option<AString<'a>> {
    let (mut parent, leaf) = within?;
    let root = earlier[0].listed.body;
    if parent == root {
        return Some(AString::from_str_in(leaf, alloc));
    }
    let mut names = AVec::new_in(alloc);
    let mut ancestors = earlier.iter().rev();
    while parent != root {
        let ancestor = ancestors.find(|r| r.listed.body == parent).expect("each_from visits the parent before the local fn");
        let (outer, name) = ancestor.within.expect("each_from marks every non-root fn with its parent");
        names.push(name);
        parent = outer;
    }
    let mut path = AString::new_in(alloc);
    for name in names.iter().rev() {
        path.push_str(name);
        path.push_str("::");
    }
    path.push_str(leaf);
    Some(path)
}

/// The lines `check` prints for a verdict (blaze-cli body_types.rs, today's path), in `alloc`.
fn said_of<'n, 'a>(verdict: &BodyVerdict<'_>, borrows: &BorrowVerdict<'_>, names: &'n Records<'n>, keys: &Keys, local_path: impl Fn() -> Option<AString<'a>>, alloc: Alloc<'a>) -> Result<AVec<'a, Said<'a>>, TaskError> {
    let text = |s: &str| -> AString<'a> {
        let mut t = AString::new_in(alloc);
        t.push_str(s);
        t
    };
    let mut said: AVec<'a, Said<'a>> = AVec::new_in(alloc);
    let mut push = |what, start, end, words| {
        let within = match said.last() {
            Some(previous) => previous.within.clone(),
            None => local_path(),
        };
        said.push(Said { within, what, start, end, words });
    };
    for d in verdict.diagnostics.iter() {
        push(SaidWhat::Type { code: d.code }, d.start, d.end, text(d.text.as_str()));
    }
    for d in verdict.findings.iter() {
        push(SaidWhat::Lint { lint: d.lint }, d.start, d.end, text(d.text.as_str()));
    }
    if let Some(refused) = &verdict.refused {
        let cause = super::super::said::refused_cause(names, keys, refused);
        let words = super::super::said::refused_words(names, refused);
        push(SaidWhat::NotChecked { cause: text(&cause) }, 0, 0, text(&words));
    } else if let Some(words) = super::super::said::gap_words(&verdict.gaps) {
        push(SaidWhat::NotChecked { cause: text(super::super::said::GAP_CAUSE) }, 0, 0, text(&words));
    }
    // The three parts of a borrow finding (rule 19): the conflicting action is the finding's
    // own range and words; the borrow and the later use follow, item-relative (rule 71) until
    // the printer places them (blaze-cli engine_bodies.rs `found_of`).
    for d in borrows.findings.iter() {
        let mut words = text(d.text);
        if let Some((start, end)) = d.borrowed {
            write!(words, "; the borrow is taken at {start}..{end}").map_err(|_| TaskError::Write)?;
        }
        match d.kept {
            Some(KeptAt::Use { start, end }) => write!(words, "; the borrow is later used at {start}..{end}"),
            Some(KeptAt::Signature) => write!(words, "; the borrow must outlive the call: a lifetime of the signature holds it"),
            Some(KeptAt::Unfound) => write!(words, "; the use that keeps the borrow live was not found (blaze-flow loans.rs `loan_parts`: an origin holding it is live, yet no use and no signature lifetime was reached)"),
            None => Ok(()),
        }
        .map_err(|_| TaskError::Write)?;
        if d.borrowed.is_some() {
            words.push_str(" (byte ranges within the item)");
        }
        push(SaidWhat::Borrow { code: d.code }, d.start, d.end, words);
    }
    if let Some(refused) = &borrows.refused
        && !matches!(refused, BorrowRefused::Untyped(_))
    {
        let (start, end) = refused.range().unwrap_or((0, 0));
        let mut words = AString::new_in(alloc);
        match refused {
            BorrowRefused::Bound(bound @ blaze_flow::Bound::TierTwo { function, .. }) =>
                write!(words, "{}", bound.in_function(ItemShown::of(names, keys, *function))),
            // Rule 19: an unknown variance names its ADT by path; the `[Unified/VarianceUnknown]`
            // class prefix stays first, so refusals still split by it.
            BorrowRefused::Stricter(step) => match step.form {
                blaze_flow_lower::Form::Unified { why: blaze_flow_lower::Unified::VarianceUnknown { adt } } =>
                    write!(words, "{refused} (variance of `{}` unknown)", ItemShown::of(names, keys, adt)),
                _ => write!(words, "{refused}"),
            },
            _ => write!(words, "{refused}"),
        }.map_err(|_| TaskError::Write)?;
        push(SaidWhat::BorrowsNotChecked, start, end, words);
    }
    Ok(said)
}

/// Whether [`verdict_bytes`] holds the digest of the body's facts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Facts {
    Digest,
    /// Left out: to compare two impl indexes whose lookups read other buckets for one answer.
    Left,
}

/// A body's verdict and borrow verdict as canonical bytes: what a body result's `out` digests.
pub fn verdict_bytes<'a>(verdict: &BodyVerdict<'_>, borrows: &BorrowVerdict<'_>, facts: Facts, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, TaskError> {
    let mut text = AString::new_in(alloc);
    write_verdict(&mut text, verdict, borrows, facts)?;
    Ok(text.into_bytes())
}

fn write_verdict(text: &mut AString<'_>, verdict: &BodyVerdict<'_>, borrows: &BorrowVerdict<'_>, facts: Facts) -> Result<(), TaskError> {
    write!(text, "diagnostics {:?}\nfindings {:?}\ngaps {:?}\nrefused {:?}\nborrows {:?}\n", verdict.diagnostics, verdict.findings, verdict.gaps, verdict.refused, borrows).map_err(|_| TaskError::Write)?;
    if facts == Facts::Digest {
        write!(text, "reads {:?}\n", verdict.reads).map_err(|_| TaskError::Write)?;
    }
    Ok(())
}

/// The body a `BodyCheck` name's subject (an item key) is, found by its item's cells.
fn body_of<'n>(view: Tables<'_, 'n>, name: Name) -> Result<BodyRef<'n>, TaskError> {
    if name.kind != Kind::BodyCheck {
        return Err(TaskError::NotABody { name });
    }
    let key = ItemKey(name.subject);
    // A member: its kind is its row in the owner's members cell (no items row of its own).
    if let Some((owner, member)) = view.member_owner(key) {
        let is_trait = view.kind_of_item(owner) == Some(ItemKind::Trait);
        let mut kind = None;
        view.each_member(owner, &mut |m| {
            if m.key == key {
                kind = Some(m.kind);
            }
        });
        let kind = match kind {
            Some(ItemKind::Function) => MemberKind::Fn,
            Some(ItemKind::Const) => MemberKind::Const,
            _ => return Err(TaskError::NotABody { name }),
        };
        return Ok(BodyRef::Member { container: owner, is_trait, kind, name: member });
    }
    match view.kind_of_item(key).ok_or(TaskError::NotABody { name })? {
        ItemKind::Function | ItemKind::Const | ItemKind::Static => Ok(BodyRef::Free(key)),
        _ => Err(TaskError::NotABody { name }),
    }
}

/// The read a body fact is beyond what the view recorded: a fact of an item's head (its
/// signature, predicates, ADT, impl header, constant, ..) by the item's checker key, and an
/// impl table bucket. Names facts (paths, traits in scope, lang paths) were recorded by the
/// view as asked; a fact of the body's own closures and async blocks is its own.
///
/// THE CELL is the one that holds the fact, by the key's class (by_origin.rs [`FactHolder`],
/// told once over every kind):
/// - a kind with a head: its head cell (head.rs [`fact_cell`], FI's one home): a
///   constructor's is its ADT's, a trait member's its trait's, an impl constant's its impl's.
///   ONE PART: a head cell is read whole (head.rs `cell_id`, part 0), whatever the fact's kind;
/// - a kind with no head (a module, a `use`, a macro, ..): its names cell `Item { key }`,
///   which holds all a check takes of it (that it is there, and of which kind). Before, such a
///   key was asked for a head cell no want makes (`NoHead(.., Module)`) and the body refused;
/// - an anonymous constant: no cell. Its fact is a function of its key and of reads the task
///   records anyway ([`FactHolder::OwnKey`]). Before, its fact was sent to its MODULE's head
///   cell (`KeyKind::Anon`'s `module`), which no want makes: every body whose store filed an
///   anonymous constant (an array length or a repeat count written as an expression, in its
///   own text or in a head or constant it lowered) was refused, 467 of them on the
///   1,100-crate workspace (g190500).
fn fact_kind_has_read(kind: FactKind) -> bool {
    use FactKind as K;
    matches!(
        kind,
        K::Head
            | K::Predicates
            | K::Adt
            | K::ImplHeader
            | K::AssocItem
            | K::Const
            | K::Opaque
            | K::Attrs
            | K::TraitDef
            | K::AssocParent
            | K::ConstHolds
            | K::ConstParams
            | K::AssocGenerics
            | K::BorrowAdt
            | K::BorrowHead
            | K::TypeAlias
            | K::FieldDefaults
            | K::MemberVisible
            | K::MemberUnstable
    )
}

fn fact_read<'n>(names: &'n Records<'n>, keys: &Keys, kind: FactKind, key: u128, owner: Option<(blaze_typeck::ItemKey, u32)>, alloc: Alloc<'_>) -> Result<Option<TableRead>, TaskError> {
    if !fact_kind_has_read(kind) || key == 0 {
        return Ok(None);
    }
    let key = blaze_typeck::ItemKey(key);
    let fact = owner.map_or(key, |(holder, _)| holder);
    let refused = |error: FreezeError| TaskError::Cell { key, ask: CellAsk::Fact(kind), holder: fact, shown: FactDefinition::of(names, key, owner, alloc), error };
    let class = kind_of_key(names, keys, fact).map_err(|error| refused(FreezeError::NoWant { key: fact, error }))?;
    match fact_holder(class) {
        // A block item's head comes from the enclosing body's text. No Signature task
        // publishes it; reads made while checking it go to this task's recorder.
        FactHolder::Head if keys.local_kind(fact).is_some() => Ok(None),
        FactHolder::Head => {
            let want = fact_cell(names, keys, fact).map_err(refused)?;
            let id = head_cell_id(want);
            let mut item = [0u8; 16];
            item.copy_from_slice(&id.of.subject.0[..16]);
            Ok(Some(TableRead::Head { item, part: 0 }))
        }
        FactHolder::Names { lib, .. } => Ok(Some(TableRead::Names(NameCell::Item { key: lib }))),
        FactHolder::OwnKey => Ok(None),
    }
}

/// A read as the engine names it (recorder.rs [`ReadOf`]): a names read (the lang rows too) is a cell of
/// its crate's `CrateScope` record, a head read a cell of the item's `Signature`, an impl read
/// a cell of its crate's `ImplBucket` table (all zero: the fact of the whole graph).
pub fn cell_id(view: Tables<'_, '_>, read: TableRead) -> Result<Option<CellId>, TaskError> {
    let records = view.records();
    // RecordItems indexes crate keys by their first eight bytes. An impl table carries
    // sixteen, so also verify its full prefix against the indexed crate.
    let crate_by = |prefix: &[u8]| {
        let mut bytes = [0; 32];
        bytes[..prefix.len()].copy_from_slice(prefix);
        let at = records.crate_of(ItemKey(ContentId(bytes)))?;
        records.crate_key(at).filter(|key| key.0.0.starts_with(prefix))
    };
    let of = match read.of() {
        // A builtin type's cell (`u32`'s item) is the language's, in no crate's record: the
        // toolchain every key covers fixes it, so no edit moves it.
        ReadOf::Names { krate } if krate == blaze_scope::BUILTIN_PREFIX => return Ok(None),
        ReadOf::Names { krate } => {
            // The crate's names task by its one name (blaze-drive key.rs): before, the raw key
            // was the subject, a name no task defines, so a names edit dirtied no body.
            let key = crate_by(&krate).ok_or(TaskError::NoCrate { prefix: krate })?;
            blaze_drive::key::crate_scope_name(&key)
        }
        ReadOf::Source { key } if key.crate_prefix() == blaze_scope::BUILTIN_PREFIX => return Ok(None),
        ReadOf::Source { key } => blaze_drive::key::fact_of(&key.0.0),
        ReadOf::Head { item } => super::head::signature_name(item),
        ReadOf::Impls { krate } if krate == [0; 16] => Name::new(Kind::ImplBucket, ABSENT),
        ReadOf::Impls { krate } => impls_name(view, krate)?,
    };
    Ok(Some(CellId { of, cell: Cell(read.cell()) }))
}

/// The impl-tables task's name of the crate whose key starts with `krate` (sixteen bytes; the
/// record index is by the first eight, so the full prefix is verified): [`impl_tables_name`],
/// one BLAKE3 of the crate key. [`ReadIds`] keeps it per crate, so a body's many impl reads of
/// one crate (one per cell) hash it once.
fn impls_name(view: Tables<'_, '_>, krate: [u8; 16]) -> Result<Name, TaskError> {
    let records = view.records();
    let mut bytes = [0; 32];
    bytes[..16].copy_from_slice(&krate);
    let mut prefix = [0u8; 8];
    prefix.copy_from_slice(&krate[..8]);
    let key = records
        .crate_of(ItemKey(ContentId(bytes)))
        .and_then(|at| records.crate_key(at))
        .filter(|key| key.0.0.starts_with(&krate))
        .ok_or(TaskError::NoCrate { prefix })?;
    Ok(impl_tables_name(key))
}

/// One immutable scope's translation from recorder keys to engine IDs. The table keeps at
/// most 65,536 keys (including builtins with no cell). A miss at capacity clears it in place.
/// Clearing changes only the amount of work: every miss uses the same canonical `cell_id`.
pub struct ReadIds<'t, 'n, 'a> {
    tables: Tables<'t, 'n>,
    ids: blaze_hash::HashMap<TableRead, Option<CellId>, Alloc<'a>>,
    /// Per crate (its key's first sixteen bytes), its impl-tables task's name ([`impls_name`]):
    /// one hash per crate, answered directly without populating or evicting `ids`.
    impl_names: blaze_hash::HashMap<[u8; 16], Name, Alloc<'a>>,
    /// Per crate (its key's first eight bytes), its `CrateScope` task's name for `NameCell::Root`,
    /// answered directly without populating or evicting `ids`.
    scope_names: blaze_hash::HashMap<[u8; 8], Name, Alloc<'a>>,
    made: u64,
    hits: u64,
}

impl<'t, 'n, 'a> ReadIds<'t, 'n, 'a> {
    const LIMIT: usize = 1 << 16;

    pub fn new(tables: Tables<'t, 'n>, alloc: Alloc<'a>) -> Self {
        Self {
            tables,
            ids: blaze_hash::HashMap::new_in(alloc),
            impl_names: blaze_hash::HashMap::new_in(alloc),
            scope_names: blaze_hash::HashMap::new_in(alloc),
            made: 0,
            hits: 0,
        }
    }

    pub fn id(&mut self, read: TableRead) -> Result<Option<CellId>, TaskError> {
        match read {
            TableRead::Impls { krate, bucket } => {
                if krate == [0; 16] {
                    self.hits += 1;
                    return Ok(Some(CellId { of: Name::new(Kind::ImplBucket, ABSENT), cell: Cell(bucket) }));
                }
                let of = match self.impl_names.get(&krate) {
                    Some(&of) => {
                        self.hits += 1;
                        of
                    }
                    None => {
                        let of = impls_name(self.tables, krate)?;
                        if self.impl_names.len() == Self::LIMIT {
                            self.impl_names.clear();
                        }
                        self.impl_names.insert(krate, of);
                        self.made += 1;
                        of
                    }
                };
                return Ok(Some(CellId { of, cell: Cell(bucket) }));
            }
            TableRead::Names(NameCell::Root { krate }) => {
                if krate == blaze_scope::BUILTIN_PREFIX {
                    self.hits += 1;
                    return Ok(None);
                }
                let of = match self.scope_names.get(&krate) {
                    Some(&of) => {
                        self.hits += 1;
                        of
                    }
                    None => {
                        let records = self.tables.records();
                        let mut bytes = [0; 32];
                        bytes[..8].copy_from_slice(&krate);
                        let key = records
                            .crate_of(ItemKey(ContentId(bytes)))
                            .and_then(|at| records.crate_key(at))
                            .filter(|key| key.0.0.starts_with(&krate))
                            .ok_or(TaskError::NoCrate { prefix: krate })?;
                        let of = blaze_drive::key::crate_scope_name(&key);
                        if self.scope_names.len() == Self::LIMIT {
                            self.scope_names.clear();
                        }
                        self.scope_names.insert(krate, of);
                        self.made += 1;
                        of
                    }
                };
                return Ok(Some(CellId { of, cell: Cell(read.cell()) }));
            }
            _ => {}
        }
        if let Some(id) = self.ids.get(&read) {
            self.hits += 1;
            return Ok(*id);
        }
        let id = cell_id(self.tables, read)?;
        if self.ids.len() == Self::LIMIT {
            self.ids.clear();
        }
        self.ids.insert(read, id);
        self.made += u64::from(id.is_some());
        Ok(id)
    }

    pub fn counts(&self) -> (u64, u64) {
        (self.made, self.hits)
    }
}

/// THE KEPT BODY (run.md R4, D119): beside its facts record, a body checked clean leaves its
/// generic body in blaze-body's flat form with the table it indexes ([`keep_checked`]), so a
/// run builds a called instance from it and lowers nothing again. Its identity is its bytes'
/// (blaze-cli engine_body_record.rs `key_of`), never the verdict's: two bodies with one clean
/// verdict (`return 1`, `return 2`) are two records.
///
/// The values the checked record does not hold, which the lowering asks for by expression,
/// pattern or variant: each table sorted by its key, one entry per key, read by binary search.
/// A key a table lacks is "not known" and the lowering refuses its construct by name
/// (blaze-body-lower `Construct::NoLiteralValue`, `NoDiscriminant`), never a guess (rule 3).
#[derive(Clone, Copy)]
pub struct KeepValues<'v> {
    pub ints: &'v [(blaze_typeck::ExprId, u128)],
    pub floats: &'v [(blaze_typeck::ExprId, u128)],
    pub chars: &'v [(blaze_typeck::ExprId, u32)],
    pub strs: &'v [(blaze_typeck::ExprId, &'v [u8])],
    pub int_pats: &'v [(blaze_typeck::PatId, u128)],
    pub float_pats: &'v [(blaze_typeck::PatId, u128)],
    pub str_pats: &'v [(blaze_typeck::PatId, &'v [u8])],
    pub discrs: &'v [((blaze_typeck::ItemKey, u32), blaze_body_lower::Discr)],
    /// Every callee of the body that is a trait's method, sorted: a callee not listed is a
    /// plain fn item. Complete by its maker: it lists each callee of `Checked::callees` whose
    /// definition is a trait member.
    pub trait_items: &'v [blaze_typeck::ItemKey],
    /// `[Deref::deref, DerefMut::deref_mut]`.
    pub deref_fns: [Option<blaze_typeck::ItemKey>; 2],
    /// `[Index::index, IndexMut::index_mut]`.
    pub index_fns: [Option<blaze_typeck::ItemKey>; 2],
    /// `[Fn::call, FnMut::call_mut, FnOnce::call_once]`.
    pub fn_trait_methods: [Option<blaze_typeck::ItemKey>; 3],
    /// Sorted by `(owner, ordinal)` for closures defined outside this body.
    pub closure_kinds: &'v [((blaze_typeck::ItemKey, u32), blaze_typeck::ClosureKind)],
    pub pointer_bits: u32,
}

/// The lowering's facts over front2's: the flow facts the borrow check gathered for this body
/// (front2 borrows.rs `gather_facts`, gathered once and shared, rule 77) and its
/// [`KeepValues`]. Only the questions blaze-body-lower asks are answered from `flow`.
pub struct KeepFacts<'f, F> {
    pub flow: &'f F,
    pub values: KeepValues<'f>,
}

impl<F: blaze_flow_lower::Facts> blaze_flow_lower::Facts for KeepFacts<'_, F> {
    fn adt(&self, adt: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::AdtFacts> {
        self.flow.adt(adt)
    }
    fn callee(&self, item: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::CalleeFacts> {
        self.flow.callee(item)
    }
    fn field(&self, adt: blaze_typeck::ItemKey, variant: u32, name: blaze_typeck::Name) -> Option<u32> {
        self.flow.field(adt, variant, name)
    }
    fn field_count(&self, adt: blaze_typeck::ItemKey, variant: u32) -> Option<u32> {
        self.flow.field_count(adt, variant)
    }
    fn param_copy(&self, index: u32) -> bool {
        self.flow.param_copy(index)
    }
    fn trait_lifetimes(&self, trait_: blaze_typeck::ItemKey) -> Option<u32> {
        self.flow.trait_lifetimes(trait_)
    }
    fn alias_lifetimes(&self, trait_: blaze_typeck::ItemKey, name: blaze_typeck::Name) -> Option<u32> {
        self.flow.alias_lifetimes(trait_, name)
    }
    // Every other fact: the answer the trait's removed default gave, spelled out (`Facts` has
    // no default, so a fact left out here does not compile). These are NOT forwarded yet: a
    // forward changes what blaze-body-lower sees, and it lands as its own save, measured alone.
    fn regions(&self) -> &dyn blaze_flow_lower::RegionFacts {
        &blaze_flow_lower::NoRegions
    }
    fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
        None
    }
    fn adt_variances(&self, _: blaze_typeck::ItemKey) -> Option<&[blaze_flow_lower::Variance]> {
        None
    }
    fn implied_bounds(&self) -> Option<(&[(u32, blaze_flow_lower::Region)], &[(blaze_flow_lower::Region, blaze_flow_lower::Region)])> {
        None
    }
    fn constant_initializer(&self) -> bool {
        false
    }
    fn fn_binders(&self) -> bool {
        true
    }
    fn closure_bodies_checked(&self) -> bool {
        false
    }
    fn closure_requirements(&self, _: blaze_typeck::ExprId) -> &[blaze_flow_lower::Requirement] {
        &[]
    }
    fn async_bodies_checked(&self) -> bool {
        false
    }
    fn is_future(&self, _: blaze_typeck::PTy) -> Option<bool> {
        None
    }
    fn shape(&self, _: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::Shape<'_>> {
        None
    }
    fn object_trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<bool> {
        None
    }
    fn call_bounds(&self, _: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::CallBounds<'_>> {
        None
    }
    fn ties(&self, _: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::Ties<'_>> {
        None
    }
    fn field_param(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<u32> {
        None
    }
    fn field_lead(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<blaze_flow_lower::FieldLead> {
        None
    }
    fn field_regions(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<&[blaze_flow_lower::FieldRegion]> {
        None
    }
    fn copy_args(&self, adt: blaze_typeck::ItemKey) -> Option<&[bool]> {
        self.flow.copy_args(adt)
    }
}

/// The entry of `key` in a table sorted by key.
fn sorted_get<K: Ord + Copy, V: Copy>(table: &[(K, V)], key: K) -> Option<V> {
    table.binary_search_by(|(k, _)| k.cmp(&key)).ok().map(|at| table[at].1)
}

impl<F: blaze_flow_lower::Facts> blaze_body_lower::BodyFacts for KeepFacts<'_, F> {
    fn int_lit(&self, e: blaze_typeck::ExprId) -> Option<u128> {
        sorted_get(self.values.ints, e)
    }
    fn float_lit(&self, e: blaze_typeck::ExprId) -> Option<u128> {
        sorted_get(self.values.floats, e)
    }
    fn char_lit(&self, e: blaze_typeck::ExprId) -> Option<u32> {
        sorted_get(self.values.chars, e)
    }
    fn str_lit(&self, e: blaze_typeck::ExprId) -> Option<&[u8]> {
        sorted_get(self.values.strs, e)
    }
    fn int_pat(&self, p: blaze_typeck::PatId) -> Option<u128> {
        sorted_get(self.values.int_pats, p)
    }
    fn float_pat(&self, p: blaze_typeck::PatId) -> Option<u128> {
        sorted_get(self.values.float_pats, p)
    }
    fn str_pat(&self, p: blaze_typeck::PatId) -> Option<&[u8]> {
        sorted_get(self.values.str_pats, p)
    }
    fn discr(&self, adt: blaze_typeck::ItemKey, variant: u32) -> Option<blaze_body_lower::Discr> {
        sorted_get(self.values.discrs, (adt, variant))
    }
    fn pointer_bits(&self) -> Option<u32> {
        Some(self.values.pointer_bits)
    }
    fn trait_item(&self, item: blaze_typeck::ItemKey) -> Option<bool> {
        Some(self.values.trait_items.binary_search(&item).is_ok())
    }
    fn deref_fn(&self, mutbl: blaze_typeck::Mutbl) -> Option<blaze_typeck::ItemKey> {
        match mutbl {
            blaze_typeck::Mutbl::Not => self.values.deref_fns[0],
            blaze_typeck::Mutbl::Mut => self.values.deref_fns[1],
        }
    }
    fn index_fn(&self, mutbl: blaze_typeck::Mutbl) -> Option<blaze_typeck::ItemKey> {
        match mutbl {
            blaze_typeck::Mutbl::Not => self.values.index_fns[0],
            blaze_typeck::Mutbl::Mut => self.values.index_fns[1],
        }
    }
    fn fn_trait_method(&self, kind: blaze_typeck::ClosureKind) -> Option<blaze_typeck::ItemKey> {
        match kind {
            blaze_typeck::ClosureKind::Fn => self.values.fn_trait_methods[0],
            blaze_typeck::ClosureKind::FnMut => self.values.fn_trait_methods[1],
            blaze_typeck::ClosureKind::FnOnce => self.values.fn_trait_methods[2],
        }
    }
    fn closure_kind(&self, owner: blaze_typeck::ItemKey, ordinal: u32) -> Option<blaze_typeck::ClosureKind> {
        sorted_get(self.values.closure_kinds, (owner, ordinal))
    }
}

/// The kept record of a body checked clean as `done` over `view` (run.md R4): lowered once by
/// the one lowering and written with `done`'s own table, in `alloc`. The caller is the borrow
/// check's step, where the view, the check and the flow facts already are (front2 borrows.rs
/// `borrows_of`, after `gather_facts_planted` and `body.typeck.view(tys)`): nothing is checked,
/// gathered or lowered for it again. A refusal is the run's to tell, naming the body: the kept
/// record is the run's input, and a body without one is never run from another form (rule 3).
pub fn keep_checked<'a, F: blaze_flow_lower::Facts>(
    view: &blaze_typeck::BodyView<'_>,
    done: &blaze_typeck::Checked<'_>,
    facts: &KeepFacts<'_, F>,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, u8>, blaze_body_lower::KeepError> {
    blaze_body_lower::keep(view, done, facts, alloc)
}

/// D119 Wave 4: `blaze_flow_lower::Facts` + `blaze_body_lower::BodyFacts` answered directly
/// over any [`crate::seam::FrontEnd`] (`Front` or `Front2Program`) for fn row `fn_index` and
/// its clean [`blaze_typeck::Checked`] record.
pub struct FrontEndKeepFacts<'a, P> {
    pub program: &'a P,
    pub fn_index: usize,
    pub owner: blaze_typeck::ItemKey,
    pub done: &'a blaze_typeck::Checked<'a>,
    pub checked: &'a [Option<blaze_typeck::Checked<'static>>],
    pub alloc: Alloc<'a>,
}

impl<P: crate::seam::FrontEnd> FrontEndKeepFacts<'_, P> {
    fn adt_is_copy(&self, adt: blaze_typeck::ItemKey, generics: u32) -> bool {
        let Some(copy_trait) = self.program.lang_item(blaze_typeck::LangItem::Copy).ok().flatten() else {
            return false;
        };
        let mut g = blaze_typeck::PortableTys::new_in(self.alloc);
        let list = if generics == 0 {
            blaze_typeck::List::EMPTY
        } else {
            let mut params = AVec::with_capacity_in(usize::try_from(generics).unwrap_or(0), self.alloc);
            for i in 0..generics {
                let Ok(p) = g.intern(blaze_typeck::PNode::Param(i)) else { return false };
                params.push(p);
            }
            let Ok(l) = g.intern_list(&params) else { return false };
            l
        };
        let Ok(ty) = g.intern(blaze_typeck::PNode::Adt(adt, list)) else { return false };
        self.program.holds(self.fn_index, copy_trait, ty, &mut g).unwrap_or(false)
    }

    fn float_bits_for(&self, wide: u64, narrow: u32, ty: blaze_typeck::PTy) -> Option<u128> {
        match self.done.types.view().node(ty).ok()? {
            blaze_typeck::PNode::Float(blaze_typeck::FloatTy::F32) => Some(u128::from(narrow)),
            blaze_typeck::PNode::Float(blaze_typeck::FloatTy::F64) => Some(u128::from(wide)),
            _ => None,
        }
    }
}

impl<P: crate::seam::FrontEnd> blaze_flow_lower::Facts for FrontEndKeepFacts<'_, P> {
    fn adt(&self, adt_key: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::AdtFacts> {
        let info = self.program.adt(adt_key)?;
        let is_enum = matches!(info.kind, blaze_typeck::AdtKind::Enum);
        let union = matches!(info.kind, blaze_typeck::AdtKind::Union);
        let is_box = self.program.lang_item(blaze_typeck::LangItem::OwnedBox).ok().flatten() == Some(adt_key);
        let is_manually_drop = self.program.lang_item(blaze_typeck::LangItem::ManuallyDrop).ok().flatten() == Some(adt_key);
        let drop_impl = match self.program.drop_trait() {
            Some((trait_, _)) => self.program.adt_drop_impl(self.fn_index, trait_, adt_key, self.alloc).ok().flatten().is_some(),
            None => false,
        };
        let copy = self.adt_is_copy(adt_key, info.generics);
        let needs_drop = if is_manually_drop {
            false
        } else if drop_impl || is_box {
            true
        } else {
            !copy && !union
        };
        Some(blaze_flow_lower::AdtFacts { copy, drop_impl, needs_drop, lifetimes: 0, union, is_enum, is_box })
    }

    fn callee(&self, item: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::CalleeFacts> {
        Some(blaze_flow_lower::CalleeFacts {
            writes_lifetimes: false,
            self_ref: false,
            c_variadic: self.program.c_variadic(item).is_some(),
        })
    }

    fn field(&self, adt: blaze_typeck::ItemKey, variant: u32, name: blaze_typeck::Name) -> Option<u32> {
        let info = self.program.adt(adt)?;
        let v = info.variants.get(usize::try_from(variant).ok()?)?;
        let start = usize::try_from(v.fields.start).ok()?;
        let len = usize::try_from(v.fields.len).ok()?;
        let fields = info.fields.get(start..start + len)?;
        fields.iter().position(|f| f.name == name).and_then(|i| u32::try_from(i).ok())
    }

    fn field_count(&self, adt: blaze_typeck::ItemKey, variant: u32) -> Option<u32> {
        let info = self.program.adt(adt)?;
        let v = info.variants.get(usize::try_from(variant).ok()?)?;
        Some(v.fields.len)
    }

    fn param_copy(&self, index: u32) -> bool {
        let Some(copy_trait) = self.program.lang_item(blaze_typeck::LangItem::Copy).ok().flatten() else {
            return false;
        };
        let mut g = blaze_typeck::PortableTys::new_in(self.alloc);
        let Ok(p) = g.intern(blaze_typeck::PNode::Param(index)) else { return false };
        self.program.holds(self.fn_index, copy_trait, p, &mut g).unwrap_or(false)
    }

    fn regions(&self) -> &dyn blaze_flow_lower::RegionFacts {
        &blaze_flow_lower::NoRegions
    }
    fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
        None
    }
    fn adt_variances(&self, _: blaze_typeck::ItemKey) -> Option<&[blaze_flow_lower::Variance]> {
        None
    }
    fn implied_bounds(&self) -> Option<(&[(u32, blaze_flow_lower::Region)], &[(blaze_flow_lower::Region, blaze_flow_lower::Region)])> {
        None
    }
    fn constant_initializer(&self) -> bool {
        matches!(self.program.function(self.fn_index).kind, crate::front::FnKind::Const | crate::front::FnKind::Static)
    }
    fn fn_binders(&self) -> bool {
        true
    }
    fn closure_bodies_checked(&self) -> bool {
        false
    }
    fn closure_requirements(&self, _: blaze_typeck::ExprId) -> &[blaze_flow_lower::Requirement] {
        &[]
    }
    fn async_bodies_checked(&self) -> bool {
        false
    }
    fn is_future(&self, _: blaze_typeck::PTy) -> Option<bool> {
        None
    }
    fn shape(&self, _: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::Shape<'_>> {
        None
    }
    fn object_trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<bool> {
        None
    }
    fn call_bounds(&self, _: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::CallBounds<'_>> {
        None
    }
    fn trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<u32> {
        None
    }
    fn alias_lifetimes(&self, _: blaze_typeck::ItemKey, _: blaze_typeck::Name) -> Option<u32> {
        None
    }
    fn ties(&self, _: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::Ties<'_>> {
        None
    }
    fn field_param(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<u32> {
        None
    }
    fn field_lead(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<blaze_flow_lower::FieldLead> {
        None
    }
    fn field_regions(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<&[blaze_flow_lower::FieldRegion]> {
        None
    }
    fn copy_args(&self, _: blaze_typeck::ItemKey) -> Option<&[bool]> {
        None
    }
}

impl<P: crate::seam::FrontEnd> blaze_body_lower::BodyFacts for FrontEndKeepFacts<'_, P> {
    fn int_lit(&self, e: blaze_typeck::ExprId) -> Option<u128> {
        self.program.int_lit(self.owner, e)
    }

    fn float_lit(&self, e: blaze_typeck::ExprId) -> Option<u128> {
        let (wide, narrow) = self.program.float_lit(self.owner, e)?;
        let ty = *self.done.expr_ty.get(e.ix())?;
        self.float_bits_for(wide, narrow, ty)
    }

    fn char_lit(&self, e: blaze_typeck::ExprId) -> Option<u32> {
        self.program.int_lit(self.owner, e).and_then(|v| u32::try_from(v).ok())
    }

    fn str_lit(&self, e: blaze_typeck::ExprId) -> Option<&[u8]> {
        self.program.string(self.owner, e)
    }

    fn int_pat(&self, p: blaze_typeck::PatId) -> Option<u128> {
        self.program.int_pat(self.owner, p)
    }

    fn float_pat(&self, p: blaze_typeck::PatId) -> Option<u128> {
        let (wide, narrow) = self.program.float_pat(self.owner, p)?;
        let ty = *self.done.pat_ty.get(p.ix())?;
        self.float_bits_for(wide, narrow, ty)
    }

    fn str_pat(&self, p: blaze_typeck::PatId) -> Option<&[u8]> {
        self.program.str_pat(self.owner, p)
    }

    fn discr(&self, adt: blaze_typeck::ItemKey, variant: u32) -> Option<blaze_body_lower::Discr> {
        use blaze_body_lower::{Discr, DiscrInt};
        use blaze_build::layout::IntRepr;
        use blaze_typeck::{IntTy, UintTy};
        let raw = self.program.discriminant(adt, variant).ok()?;
        let val = raw.unwrap_or_else(|| i128::from(variant));
        let repr = self.program.repr(adt).ok().unwrap_or(blaze_build::layout::ReprAttrs::RUST);
        let (int, width): (DiscrInt, u32) = match repr.int {
            Some(IntRepr::Signed(it)) => {
                let w = match it {
                    IntTy::I8 => 8,
                    IntTy::I16 => 16,
                    IntTy::I32 => 32,
                    IntTy::I64 | IntTy::Isize => 64,
                    IntTy::I128 => 128,
                };
                (DiscrInt::Int(it), w)
            }
            Some(IntRepr::Unsigned(ut)) => {
                let w = match ut {
                    UintTy::U8 => 8,
                    UintTy::U16 => 16,
                    UintTy::U32 => 32,
                    UintTy::U64 | UintTy::Usize => 64,
                    UintTy::U128 => 128,
                };
                (DiscrInt::Uint(ut), w)
            }
            None => (DiscrInt::Int(IntTy::Isize), 64),
        };
        let mask = if width == 128 { u128::MAX } else { (1u128 << width) - 1 };
        Some(Discr { int, bits: val.cast_unsigned() & mask })
    }

    fn pointer_bits(&self) -> Option<u32> {
        Some(64)
    }

    fn trait_item(&self, item: blaze_typeck::ItemKey) -> Option<bool> {
        if let Some(i) = self.program.fn_index(item) {
            return Some(matches!(self.program.function(i).kind, crate::front::FnKind::Trait(_)));
        }
        None
    }

    fn deref_fn(&self, mutbl: blaze_typeck::Mutbl) -> Option<blaze_typeck::ItemKey> {
        match mutbl {
            blaze_typeck::Mutbl::Not => self.program.lang_trait_fn(blaze_typeck::LangItem::Deref, "deref"),
            blaze_typeck::Mutbl::Mut => self.program.lang_trait_fn(blaze_typeck::LangItem::DerefMut, "deref_mut"),
        }
    }

    fn index_fn(&self, mutbl: blaze_typeck::Mutbl) -> Option<blaze_typeck::ItemKey> {
        match mutbl {
            blaze_typeck::Mutbl::Not => self.program.lang_trait_fn(blaze_typeck::LangItem::Index, "index"),
            blaze_typeck::Mutbl::Mut => self.program.lang_trait_fn(blaze_typeck::LangItem::IndexMut, "index_mut"),
        }
    }

    fn fn_trait_method(&self, kind: blaze_typeck::ClosureKind) -> Option<blaze_typeck::ItemKey> {
        let which = match kind {
            blaze_typeck::ClosureKind::Fn => blaze_build::FnTrait::Fn,
            blaze_typeck::ClosureKind::FnMut => blaze_build::FnTrait::FnMut,
            blaze_typeck::ClosureKind::FnOnce => blaze_build::FnTrait::FnOnce,
        };
        let (lang, name) = crate::front2::langs::fn_trait_method(which);
        self.program.lang_trait_fn(lang, name)
    }

    fn closure_kind(&self, owner: blaze_typeck::ItemKey, ordinal: u32) -> Option<blaze_typeck::ClosureKind> {
        let idx = self.program.fn_index(owner)?;
        let done = self.checked.get(idx)?.as_ref()?;
        let row = usize::try_from(ordinal).ok()?;
        done.closures.column::<1>().get(row).copied()
    }
}

/// D119 Wave 4: lowers and encodes fn row `fn_index`'s checked body from any
/// [`crate::seam::FrontEnd`] into a verified kept body record (`blaze_body::kept`).
pub fn keep_frontend_body<'a, P: crate::seam::FrontEnd>(
    program: &P,
    fn_index: usize,
    done: &blaze_typeck::Checked<'_>,
    checked: &[Option<blaze_typeck::Checked<'static>>],
    alloc: Alloc<'a>,
) -> Result<AVec<'a, u8>, blaze_body_lower::KeepError> {
    let function = program.function(fn_index);
    let Some((body, _)) = function.body else {
        return Err(blaze_body_lower::KeepError::Lower(blaze_body_lower::LowerError {
            construct: blaze_body_lower::Construct::Malformed,
            at: blaze_body_lower::At::Body,
            range: blaze_typeck::Range { start: 0, end: 0 },
        }));
    };
    let view = body.view(program.body_types(fn_index));
    let facts = FrontEndKeepFacts { program, fn_index, owner: function.key, done, checked, alloc };
    blaze_body_lower::keep(&view, done, &facts, alloc)
}

/// D119 Wave 5: lowers and encodes the `ordinal`-th closure body inside fn row `fn_index`
/// into a verified kept body record (`blaze_body::kept`).
pub fn keep_frontend_closure_body<'a, P: crate::seam::FrontEnd>(
    program: &P,
    fn_index: usize,
    ordinal: u32,
    done: &blaze_typeck::Checked<'_>,
    checked: &[Option<blaze_typeck::Checked<'static>>],
    alloc: Alloc<'a>,
) -> Result<AVec<'a, u8>, blaze_body_lower::KeepError> {
    let function = program.function(fn_index);
    let Some((body, _)) = function.body else {
        return Err(blaze_body_lower::KeepError::Lower(blaze_body_lower::LowerError {
            construct: blaze_body_lower::Construct::Malformed,
            at: blaze_body_lower::At::Body,
            range: blaze_typeck::Range { start: 0, end: 0 },
        }));
    };
    let view = body.view(program.body_types(fn_index));
    let Some(closure_expr) = view
        .exprs
        .iter()
        .enumerate()
        .filter(|(_, x)| matches!(x, blaze_typeck::Expr::Closure { .. }))
        .nth(usize::try_from(ordinal).unwrap_or(usize::MAX))
        .and_then(|(idx, _)| u32::try_from(idx).ok().map(blaze_typeck::ExprId))
    else {
        return Err(blaze_body_lower::KeepError::Lower(blaze_body_lower::LowerError {
            construct: blaze_body_lower::Construct::Malformed,
            at: blaze_body_lower::At::Body,
            range: blaze_typeck::Range { start: 0, end: 0 },
        }));
    };
    let facts = FrontEndKeepFacts { program, fn_index, owner: function.key, done, checked, alloc };
    let lowered = blaze_body_lower::lower_closure(&view, done, &facts, closure_expr, alloc).map_err(blaze_body_lower::KeepError::Lower)?;
    blaze_body::kept::encode(&lowered, done.types.view(), alloc).map_err(blaze_body_lower::KeepError::Kept)
}

#[cfg(test)]
mod kept_body_tests {
    use super::{KeepFacts, KeepValues, sorted_get};
    use blaze_body_lower::BodyFacts as _;
    use blaze_flow_lower::{AdtFacts, CalleeFacts, Facts};
    use blaze_typeck::{ExprId, ItemKey, Name, PatId};

    /// No ADT and no callee is known: the values alone are under test.
    struct NoFlow;

    impl Facts for NoFlow {
        // Every other fact not known here, written out: `Facts` has no default answer.
        fn regions(&self) -> &dyn blaze_flow_lower::RegionFacts {
            &blaze_flow_lower::NoRegions
        }
        fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
            None
        }
        fn adt_variances(&self, _: ItemKey) -> Option<&[blaze_flow_lower::Variance]> {
            None
        }
        fn implied_bounds(&self) -> Option<(&[(u32, blaze_flow_lower::Region)], &[(blaze_flow_lower::Region, blaze_flow_lower::Region)])> {
            None
        }
        fn constant_initializer(&self) -> bool {
            false
        }
        fn fn_binders(&self) -> bool {
            true
        }
        fn closure_bodies_checked(&self) -> bool {
            false
        }
        fn closure_requirements(&self, _: ExprId) -> &[blaze_flow_lower::Requirement] {
            &[]
        }
        fn async_bodies_checked(&self) -> bool {
            false
        }
        fn is_future(&self, _: blaze_typeck::PTy) -> Option<bool> {
            None
        }
        fn shape(&self, _: ItemKey) -> Option<blaze_flow_lower::Shape<'_>> {
            None
        }
        fn object_trait_lifetimes(&self, _: ItemKey) -> Option<bool> {
            None
        }
        fn call_bounds(&self, _: ItemKey) -> Option<blaze_flow_lower::CallBounds<'_>> {
            None
        }
        fn trait_lifetimes(&self, _: ItemKey) -> Option<u32> {
            None
        }
        fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Option<u32> {
            None
        }
        fn ties(&self, _: ItemKey) -> Option<blaze_flow_lower::Ties<'_>> {
            None
        }
        fn field_param(&self, _: ItemKey, _: u32, _: u32) -> Option<u32> {
            None
        }
        fn field_lead(&self, _: ItemKey, _: u32, _: u32) -> Option<blaze_flow_lower::FieldLead> {
            None
        }
        fn field_regions(&self, _: ItemKey, _: u32, _: u32) -> Option<&[blaze_flow_lower::FieldRegion]> {
            None
        }
        fn copy_args(&self, _: ItemKey) -> Option<&[bool]> {
            None
        }
        fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
            None
        }
        fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
            None
        }
        fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> {
            None
        }
        fn field_count(&self, _: ItemKey, _: u32) -> Option<u32> {
            None
        }
        fn param_copy(&self, _: u32) -> bool {
            false
        }
    }

    /// Each value is answered for its own key and no other; a key the tables lack is unknown
    /// (the lowering then refuses by name), and a callee is a trait's method only when listed.
    #[test]
    fn values_answer_their_own_keys_only() {
        let ints = [(ExprId(1), 1u128), (ExprId(4), 2)];
        let strs: [(ExprId, &[u8]); 1] = [(ExprId(2), b"hi")];
        let pats = [(PatId(0), 7u128)];
        let traits = [ItemKey(9)];
        let facts = KeepFacts {
            flow: &NoFlow,
            values: KeepValues {
                ints: &ints,
                floats: &[],
                chars: &[],
                strs: &strs,
                int_pats: &pats,
                float_pats: &[],
                str_pats: &[],
                discrs: &[],
                trait_items: &traits,
                deref_fns: [None, None],
                index_fns: [None, None],
                fn_trait_methods: [None, None, None],
                closure_kinds: &[],
                pointer_bits: 64,
            },
        };
        assert_eq!(facts.int_lit(ExprId(1)), Some(1));
        assert_eq!(facts.int_lit(ExprId(4)), Some(2));
        // Planted: a key between two entries is not answered with a neighbour's value.
        assert_eq!(facts.int_lit(ExprId(2)), None);
        assert_eq!(facts.str_lit(ExprId(2)), Some(&b"hi"[..]));
        assert_eq!(facts.int_pat(PatId(0)), Some(7));
        assert_eq!(facts.float_lit(ExprId(1)), None);
        assert_eq!(facts.trait_item(ItemKey(9)), Some(true));
        assert_eq!(facts.trait_item(ItemKey(8)), Some(false));
        assert_eq!(sorted_get(&[(1u32, 'a'), (3, 'c')], 3), Some('c'));
    }

    // ---- D119 Wave 4 live cutover & side-by-side ladder oracles -----------------------------

    use super::{keep_frontend_body, keep_frontend_closure_body};
    use blaze_own::{AString, AVec, Alloc};
    use core::fmt::Write as _;

    trait RecipeAlloc {
        type A: core::alloc::Allocator;
    }
    impl<A: core::alloc::Allocator> RecipeAlloc for blaze_flow_lower::state_recipe::StateRecipe<A> {
        type A = A;
    }
    type BuildRecAlloc = <crate::seam::SeamRecipe as RecipeAlloc>::A;

    struct RungSpec {
        name: &'static str,
        files: &'static [(&'static str, &'static str)],
        expected: u8,
        /// True if the rung calls extern C functions (e.g. `write`, `puts`) that LIR eval does not execute.
        has_extern_call: bool,
        /// True if LIR eval does not support an op in this rung (e.g. Vtable `DataAddr` or stack-slot `Expose`),
        /// in which case we still verify both builders + AArch64 emit succeed.
        eval_skip: bool,
    }

    const LADDER_RUNGS: &[RungSpec] = &[
        RungSpec { name: "1-empty", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/1-empty/src/main.rs"))], expected: 0, has_extern_call: false, eval_skip: false },
        RungSpec { name: "2-exit", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/2-exit/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec { name: "3-hello", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/3-hello/src/main.rs"))], expected: 0, has_extern_call: true, eval_skip: true },
        RungSpec {
            name: "4-modules",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/4-modules/src/main.rs")),
                ("src/util.rs", include_str!("../../../../blaze-cli/tests/ladder/4-modules/src/util.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
        RungSpec { name: "5-generics", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/5-generics/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec { name: "6-aggregates", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/6-aggregates/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec { name: "7-traits", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/7-traits/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec { name: "7a-drops", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/7a-drops/src/main.rs"))], expected: 42, has_extern_call: true, eval_skip: true },
        RungSpec { name: "7b-dyn", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/7b-dyn/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: true },
        RungSpec { name: "support/s1-layout", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/support/s1-layout/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: true },
        RungSpec { name: "support/s2-statics", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/support/s2-statics/src/main.rs"))], expected: 42, has_extern_call: true, eval_skip: true },
        RungSpec { name: "support/s3-extern-c", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/support/s3-extern-c/src/main.rs"))], expected: 42, has_extern_call: true, eval_skip: true },
        RungSpec { name: "support/s4-intrinsics", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/support/s4-intrinsics/src/main.rs"))], expected: 42, has_extern_call: true, eval_skip: true },
        RungSpec { name: "front2/f1-defaults", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/f1-defaults/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec { name: "front2/f2-generic-self", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/f2-generic-self/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec { name: "front2/f4-assoc", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/f4-assoc/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec { name: "front2/f5-closures", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/f5-closures/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec { name: "front2/f6-literals", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/f6-literals/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec { name: "front2/x0-const-assoc-01", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-const-assoc-01/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec { name: "front2/x0-const-block-01", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-const-block-01/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec { name: "front2/x0-dropglue-01", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-dropglue-01/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec { name: "front2/x0-static-01", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-static-01/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec { name: "front2/x0-flit-01", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-flit-01/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec {
            name: "front2/x0-adt-09-discriminant-high-bit",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-adt-09-discriminant-high-bit/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-adt-09-discriminant-high-bit/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
        RungSpec {
            name: "front2/x0-int-11-three-way",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-int-11-three-way/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-int-11-three-way/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
        RungSpec {
            name: "front2/x0-move-11-field-of-temp",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-move-11-field-of-temp/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-move-11-field-of-temp/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
        RungSpec {
            name: "front2/x0-move-12-match-on-deref",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-move-12-match-on-deref/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-move-12-match-on-deref/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
        RungSpec { name: "front2/x0-drop-11-zst-temporary", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-drop-11-zst-temporary/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec { name: "front2/x0-drop-12-let-else-temp", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-drop-12-let-else-temp/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec { name: "front2/x0-drop-13-match-temp-in-expr", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-drop-13-match-temp-in-expr/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec { name: "front2/x0-drop-14-if-let-temp", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-drop-14-if-let-temp/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec { name: "front2/x0-static-03-fn-pointers", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-static-03-fn-pointers/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        // Wave 5 expanded coverage: closures, drops, drop glue, interior-mutable statics, extern-local, unions, struct update.
        RungSpec { name: "front2/x0-static-05-interior-mut", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-static-05-interior-mut/src/main.rs"))], expected: 0, has_extern_call: true, eval_skip: true },
        RungSpec { name: "front2/x0-static-02-late-body", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-static-02-late-body/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec { name: "front2/x0-static-04-generic-field", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-static-04-generic-field/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec { name: "front2/x0-extern-local-01", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-extern-local-01/src/main.rs"))], expected: 42, has_extern_call: true, eval_skip: true },
        RungSpec {
            name: "front2/x0-clo-02-by-mut",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-clo-02-by-mut/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-clo-02-by-mut/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
        RungSpec {
            name: "front2/x0-clo-03-move-copy",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-clo-03-move-copy/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-clo-03-move-copy/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
        RungSpec {
            name: "front2/x0-clo-04-generic-fn",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-clo-04-generic-fn/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-clo-04-generic-fn/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
        RungSpec {
            name: "front2/x0-clo-08-fn-pointers",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-clo-08-fn-pointers/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-clo-08-fn-pointers/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
        RungSpec {
            name: "front2/x0-clo-09-move-zst-drop",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-clo-09-move-zst-drop/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-clo-09-move-zst-drop/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
        RungSpec {
            name: "front2/x0-clo-10-call-zst-drop",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-clo-10-call-zst-drop/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-clo-10-call-zst-drop/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
        RungSpec {
            name: "front2/x0-drop-01-locals",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-drop-01-locals/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-drop-01-locals/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
        RungSpec {
            name: "front2/x0-drop-02-fields",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-drop-02-fields/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-drop-02-fields/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
        RungSpec {
            name: "front2/x0-drop-04-temporaries",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-drop-04-temporaries/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-drop-04-temporaries/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
        RungSpec {
            name: "front2/x0-drop-05-reassign",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-drop-05-reassign/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-drop-05-reassign/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
        RungSpec {
            name: "front2/x0-drop-07-drop-flags",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-drop-07-drop-flags/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-drop-07-drop-flags/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
        RungSpec {
            name: "front2/x0-drop-08-return-break",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-drop-08-return-break/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-drop-08-return-break/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
        RungSpec {
            name: "front2/x0-drop-09-partial-move",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-drop-09-partial-move/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-drop-09-partial-move/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
        RungSpec { name: "front2/x0-dropglue-02-slice", files: &[("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-dropglue-02-slice/src/main.rs"))], expected: 42, has_extern_call: false, eval_skip: false },
        RungSpec {
            name: "front2/x0-dyn-05-drop-in-place",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-dyn-05-drop-in-place/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-dyn-05-drop-in-place/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: true,
        },
        RungSpec {
            name: "front2/x0-union-01-maybe-uninit-copy",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-union-01-maybe-uninit-copy/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-union-01-maybe-uninit-copy/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
        RungSpec {
            name: "front2/x0-struct-update-01",
            files: &[
                ("src/main.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-struct-update-01/src/main.rs")),
                ("src/prelude.rs", include_str!("../../../../blaze-cli/tests/ladder/front2/x0-struct-update-01/src/prelude.rs")),
            ],
            expected: 42,
            has_extern_call: false,
            eval_skip: false,
        },
    ];

    /// Runs `f` with a checked `Front2Program` built from `#![no_core]` source files.
    fn with_front2<R>(
        files: &[(&str, &str)],
        alloc: Alloc<'static>,
        f: impl FnOnce(&mut crate::front2::program::Front2Program<'_>, &[Option<blaze_typeck::Checked<'static>>]) -> R,
    ) -> R {
        let mut views = AVec::with_capacity_in(files.len(), alloc);
        for &(path, text) in files {
            views.push(crate::refusal::SourceFile { path, text });
        }
        let new = crate::run::NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
        let panic = blaze_build::Panic::Unwind;
        let tc = crate::library_set::toolchain_for(&views, "src/main.rs", new, panic, alloc).expect("no_core toolchain");
        let texts = crate::program_texts::ProgramTexts::new(&views, "src/main.rs", new, &tc, panic).expect("program texts");
        let user = texts.user();
        let names_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("names frame");
        let names_erased = blaze_own::Erased::new(&names_frame);
        let inputs = crate::crates::Inputs::new(&user, &tc).expect("inputs");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, names_erased.bump()).expect("names resolve");
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("front2 frame");
        let mut program = crate::front2::program::Front2Program::new(&names, &frame, &views, alloc).expect("Front2Program::new");
        let checked = crate::seam::FrontEnd::check_all(&mut program).expect("check_all");
        f(&mut program, &checked)
    }

    /// Precomputed per-Drop-row plan for `FlowDropFacts`.
    enum PreDrop<'a> {
        Uninit,
        Init,
        Maybe,
        Partial(AVec<'a, blaze_body_drops::Part<'a>>),
        Enum { discr_ty: blaze_typeck::PTy, variants: AVec<'a, blaze_body_drops::VariantDrop<'a>> },
    }

    /// DropFacts over `blaze_body_flow::Projected` + `blaze_flow::moves::inits` + `FrontEnd`,
    /// supporting full structural (struct/tuple) and enum-variant partial drops.
    struct FlowDropFacts<'a, P> {
        program: &'a P,
        body: blaze_body::Verified<'a>,
        tys: blaze_typeck::PTys<'a>,
        plans: AVec<'a, (blaze_body::Stmt, PreDrop<'a>)>,
        bool_ty: blaze_typeck::PTy,
        alloc: Alloc<'a>,
    }

    impl<'a, P: crate::seam::FrontEnd> FlowDropFacts<'a, P> {
        fn build(
            program: &'a P,
            body: blaze_body::Verified<'a>,
            tys: blaze_typeck::PTys<'a>,
            proj: &blaze_body_flow::Projected<Alloc<'a>>,
            alloc: Alloc<'a>,
            arena: &'a blaze_mem::Frame,
        ) -> Result<Self, blaze_flow::Bound> {
            let st = blaze_flow::moves::inits(&proj.flow, blaze_flow::record::Plant::None, alloc)?;
            let find_node = |want: blaze_typeck::PNode| -> Option<blaze_typeck::PTy> {
                for (idx, n) in tys.nodes().enumerate() {
                    if *n == want {
                        return u32::try_from(idx).ok().map(blaze_typeck::PTy);
                    }
                }
                if let Some(base) = body.body().ty_base() {
                    for i in 0..body.own_tys() {
                        let t = blaze_typeck::PTy(base + i);
                        if matches!(body.own_ty(t), Ok(Some(n)) if n == want) {
                            return Some(t);
                        }
                    }
                }
                None
            };
            let bool_ty = find_node(blaze_typeck::PNode::Bool).unwrap_or(blaze_typeck::PTy(0));
            let mut plans = AVec::new_in(alloc);

            // Helper: find child flow place of `parent` with projection `want_proj`.
            let child_flow_place = |parent: blaze_flow::Place, want: blaze_flow::Proj| -> Option<blaze_flow::Place> {
                for idx in 0..proj.flow.places.len() {
                    let qp = blaze_flow::Place(u32::try_from(idx).unwrap_or(0));
                    let row = proj.flow.place(qp);
                    if row.parent == Some(parent) && row.proj == Some(want) {
                        return Some(qp);
                    }
                }
                None
            };

            // Recursively collect remaining initialized/maybe parts under `(fp, ty)`.
            fn collect_parts<'a, P: crate::seam::FrontEnd>(
                program: &P,
                body: blaze_body::Verified<'a>,
                tys: blaze_typeck::PTys<'a>,
                flow: &blaze_flow::Flow<Alloc<'a>>,
                st: &blaze_flow::moves::Inits<Alloc<'a>>,
                fs: blaze_flow::Stmt,
                fp: Option<blaze_flow::Place>,
                parent_kind: blaze_flow::DropKind,
                ty: blaze_typeck::PTy,
                prefix: &mut AVec<'a, blaze_body_drops::Field>,
                out: &mut AVec<'a, blaze_body_drops::Part<'a>>,
                arena: &'a blaze_mem::Frame,
                alloc: Alloc<'a>,
                depth: u32,
            ) {
                use blaze_body_drops::{Field, Part, PartState};
                use blaze_flow::{DropKind, Proj};
                use blaze_typeck::PNode;
                if depth > 16 {
                    return;
                }
                let child_of = |parent: blaze_flow::Place, want: Proj| -> Option<blaze_flow::Place> {
                    for idx in 0..flow.places.len() {
                        let qp = blaze_flow::Place(u32::try_from(idx).unwrap_or(0));
                        let row = flow.place(qp);
                        if row.parent == Some(parent) && row.proj == Some(want) {
                            return Some(qp);
                        }
                    }
                    None
                };
                let Ok(node) = body.body().ty(&tys, ty) else { return };
                // Extract ordered field types for tuples and structs.
                let mut field_tys: AVec<'a, blaze_typeck::PTy> = AVec::new_in(alloc);
                match node {
                    PNode::Tuple(l) => {
                        if let Ok(entries) = body.body().ty_list(&tys, l) {
                            for sub in entries.iter() {
                                field_tys.push(sub);
                            }
                        }
                    }
                    PNode::Adt(adt_key, adt_args) => {
                        if let Some(info) = program.adt(adt_key)
                            && matches!(info.kind, blaze_typeck::AdtKind::Struct)
                            && let Some(v0) = info.variants.first()
                        {
                            let start = usize::try_from(v0.fields.start).unwrap_or(0);
                            let len = usize::try_from(v0.fields.len).unwrap_or(0);
                            if let Some(fields) = info.fields.get(start..start + len) {
                                // Map field type into body's table when possible (non-generic fields match by node).
                                for f in fields {
                                    if let Ok(fnode) = info.types.node(f.ty) {
                                        let mapped = match fnode {
                                            PNode::Param(pidx) => body
                                                .body()
                                                .ty_list(&tys, adt_args)
                                                .ok()
                                                .and_then(|l| l.iter().nth(pidx as usize)),
                                            other => {
                                                let mut found = None;
                                                for (ti, n) in tys.nodes().enumerate() {
                                                    if *n == other {
                                                        found = u32::try_from(ti).ok().map(blaze_typeck::PTy);
                                                        break;
                                                    }
                                                }
                                                found
                                            }
                                        };
                                        if let Some(ft) = mapped {
                                            field_tys.push(ft);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
                for (fi, &fty) in field_tys.iter().enumerate() {
                    let fidx = u32::try_from(fi).unwrap_or(0);
                    let cfp = fp.and_then(|p| child_of(p, Proj::Field(fidx)));
                    let ckind = match cfp {
                        Some(q) => blaze_flow::moves::drop_kind(flow, st, fs, q),
                        None => match parent_kind {
                            DropKind::Nothing => DropKind::Nothing,
                            DropKind::Flag => DropKind::Flag,
                            DropKind::Static | DropKind::Open => DropKind::Static,
                        },
                    };
                    prefix.push(Field { index: fidx, ty: fty });
                    match ckind {
                        DropKind::Nothing => {}
                        DropKind::Static => {
                            let slice = arena.alloc_slice_copy(prefix.as_slice());
                            out.push(Part { path: slice, state: PartState::Init });
                        }
                        DropKind::Flag => {
                            let slice = arena.alloc_slice_copy(prefix.as_slice());
                            out.push(Part { path: slice, state: PartState::Maybe });
                        }
                        DropKind::Open => {
                            collect_parts(program, body, tys, flow, st, fs, cfp, ckind, fty, prefix, out, arena, alloc, depth + 1);
                        }
                    }
                    prefix.pop();
                }
            }

            // Compute plan for every Drop statement in the body.
            for s in 0..body.stmts() {
                let stmt = blaze_body::Stmt(s);
                let blaze_body::StmtKind::Drop { place, .. } = body.stmt(stmt).kind else { continue };
                let (Some(fs), Some(fp)) = (proj.flow_stmt(stmt), proj.flow_place(place)) else {
                    plans.push((stmt, PreDrop::Init));
                    continue;
                };
                let dk = blaze_flow::moves::drop_kind(&proj.flow, &st, fs, fp);
                let plan = match dk {
                    blaze_flow::DropKind::Nothing => PreDrop::Uninit,
                    blaze_flow::DropKind::Static => PreDrop::Init,
                    blaze_flow::DropKind::Flag => PreDrop::Maybe,
                    blaze_flow::DropKind::Open => {
                        let place_ty = body.local(body.place(place).local).ty;
                        let node = body.body().ty(&tys, place_ty).ok();
                        if let Some(blaze_typeck::PNode::Adt(adt_key, adt_args)) = node
                            && let Some(info) = program.adt(adt_key)
                            && matches!(info.kind, blaze_typeck::AdtKind::Enum)
                        {
                            let repr = program.repr(adt_key).ok().unwrap_or(blaze_build::layout::ReprAttrs::RUST);
                            let (discr_node, width): (blaze_typeck::PNode, u32) = match repr.int {
                                Some(blaze_build::layout::IntRepr::Signed(it)) => {
                                    let w = match it {
                                        blaze_typeck::IntTy::I8 => 8,
                                        blaze_typeck::IntTy::I16 => 16,
                                        blaze_typeck::IntTy::I32 => 32,
                                        blaze_typeck::IntTy::I64 | blaze_typeck::IntTy::Isize => 64,
                                        blaze_typeck::IntTy::I128 => 128,
                                    };
                                    (blaze_typeck::PNode::Int(it), w)
                                }
                                Some(blaze_build::layout::IntRepr::Unsigned(ut)) => {
                                    let w = match ut {
                                        blaze_typeck::UintTy::U8 => 8,
                                        blaze_typeck::UintTy::U16 => 16,
                                        blaze_typeck::UintTy::U32 => 32,
                                        blaze_typeck::UintTy::U64 | blaze_typeck::UintTy::Usize => 64,
                                        blaze_typeck::UintTy::U128 => 128,
                                    };
                                    (blaze_typeck::PNode::Uint(ut), w)
                                }
                                None => (blaze_typeck::PNode::Int(blaze_typeck::IntTy::Isize), 64),
                            };
                            let mask = if width == 128 { u128::MAX } else { (1u128 << width) - 1 };
                            let discr_ty = find_node(discr_node).unwrap_or(bool_ty);
                            let mut variants = AVec::new_in(alloc);
                            for (vi, variant) in info.variants.iter().enumerate() {
                                let v_idx = u32::try_from(vi).unwrap_or(0);
                                let raw_d = program.discriminant(adt_key, v_idx).ok().flatten().unwrap_or_else(|| i128::from(v_idx));
                                let discr = raw_d.cast_unsigned() & mask;
                                let vfp = child_flow_place(fp, blaze_flow::Proj::Downcast(v_idx));
                                let vkind = match vfp {
                                    Some(q) => blaze_flow::moves::drop_kind(&proj.flow, &st, fs, q),
                                    None => blaze_flow::DropKind::Static,
                                };
                                let start = usize::try_from(variant.fields.start).unwrap_or(0);
                                let len = usize::try_from(variant.fields.len).unwrap_or(0);
                                let mut vparts = AVec::new_in(alloc);
                                if let Some(fields) = info.fields.get(start..start + len) {
                                    for (fi, f) in fields.iter().enumerate() {
                                        let fidx = u32::try_from(fi).unwrap_or(0);
                                        let Ok(fnode) = info.types.node(f.ty) else { continue };
                                        let mapped = match fnode {
                                            blaze_typeck::PNode::Param(pidx) => body
                                                .body()
                                                .ty_list(&tys, adt_args)
                                                .ok()
                                                .and_then(|l| l.iter().nth(pidx as usize)),
                                            other => find_node(other),
                                        };
                                        let Some(fty) = mapped else { continue };
                                        let cfp = vfp.and_then(|p| child_flow_place(p, blaze_flow::Proj::Field(fidx)));
                                        let ckind = match cfp {
                                            Some(q) => blaze_flow::moves::drop_kind(&proj.flow, &st, fs, q),
                                            None => match vkind {
                                                blaze_flow::DropKind::Nothing => blaze_flow::DropKind::Nothing,
                                                blaze_flow::DropKind::Flag => blaze_flow::DropKind::Flag,
                                                blaze_flow::DropKind::Static | blaze_flow::DropKind::Open => blaze_flow::DropKind::Static,
                                            },
                                        };
                                        let mut prefix = AVec::new_in(alloc);
                                        prefix.push(blaze_body_drops::Field { index: fidx, ty: fty });
                                        match ckind {
                                            blaze_flow::DropKind::Nothing => {}
                                            blaze_flow::DropKind::Static => {
                                                let slice = arena.alloc_slice_copy(prefix.as_slice());
                                                vparts.push(blaze_body_drops::Part { path: slice, state: blaze_body_drops::PartState::Init });
                                            }
                                            blaze_flow::DropKind::Flag => {
                                                let slice = arena.alloc_slice_copy(prefix.as_slice());
                                                vparts.push(blaze_body_drops::Part { path: slice, state: blaze_body_drops::PartState::Maybe });
                                            }
                                            blaze_flow::DropKind::Open => {
                                                collect_parts(program, body, tys, &proj.flow, &st, fs, cfp, ckind, fty, &mut prefix, &mut vparts, arena, alloc, 1);
                                            }
                                        }
                                    }
                                }
                                let parts_slice = arena.alloc_slice_copy(vparts.as_slice());
                                variants.push(blaze_body_drops::VariantDrop { variant: v_idx, discr, parts: parts_slice });
                            }
                            PreDrop::Enum { discr_ty, variants }
                        } else {
                            let mut prefix = AVec::new_in(alloc);
                            let mut parts = AVec::new_in(alloc);
                            collect_parts(program, body, tys, &proj.flow, &st, fs, Some(fp), dk, place_ty, &mut prefix, &mut parts, arena, alloc, 0);
                            if parts.is_empty() {
                                PreDrop::Uninit
                            } else {
                                PreDrop::Partial(parts)
                            }
                        }
                    }
                };
                plans.push((stmt, plan));
            }
            Ok(Self { program, body, tys, plans, bool_ty, alloc })
        }

        fn ty_node(&self, t: blaze_typeck::PTy) -> Option<blaze_typeck::PNode> {
            self.body.body().ty(&self.tys, t).ok()
        }

        fn classify_needs_drop(&self, t: blaze_typeck::PTy, depth: u32) -> blaze_body_drops::NeedsDrop {
            use blaze_body_drops::NeedsDrop;
            use blaze_typeck::PNode;
            if depth > 32 {
                return NeedsDrop::Yes;
            }
            let Some(node) = self.ty_node(t) else { return NeedsDrop::Yes };
            match node {
                PNode::Bool
                | PNode::Char
                | PNode::Int(_)
                | PNode::Uint(_)
                | PNode::Float(_)
                | PNode::Never
                | PNode::Ref(..)
                | PNode::Ptr(..)
                | PNode::FnDef(..)
                | PNode::FnPtr(..)
                | PNode::Str => NeedsDrop::No,
                PNode::Param(_) | PNode::Alias { .. } | PNode::Opaque(..) | PNode::ConstParam(_) => NeedsDrop::Generic,
                PNode::Tuple(l) => {
                    let Ok(entries) = self.body.body().ty_list(&self.tys, l) else { return NeedsDrop::Yes };
                    let mut generic = false;
                    for sub in entries.iter() {
                        match self.classify_needs_drop(sub, depth + 1) {
                            NeedsDrop::Yes => return NeedsDrop::Yes,
                            NeedsDrop::Generic => generic = true,
                            NeedsDrop::No => {}
                        }
                    }
                    if generic { NeedsDrop::Generic } else { NeedsDrop::No }
                }
                PNode::Array(elem, _) | PNode::Slice(elem) => self.classify_needs_drop(elem, depth + 1),
                PNode::Adt(k, args) => {
                    if self.program.lang_item(blaze_typeck::LangItem::ManuallyDrop).ok().flatten() == Some(k) {
                        return NeedsDrop::No;
                    }
                    if self.program.lang_item(blaze_typeck::LangItem::OwnedBox).ok().flatten() == Some(k) {
                        return NeedsDrop::Yes;
                    }
                    if let Some((drop_trait, _)) = self.program.drop_trait() {
                        if self.program.adt_drop_impl(0, drop_trait, k, self.alloc).ok().flatten().is_some() {
                            return NeedsDrop::Yes;
                        }
                    }
                    let Some(info) = self.program.adt(k) else { return NeedsDrop::Yes };
                    if matches!(info.kind, blaze_typeck::AdtKind::Union) {
                        return NeedsDrop::No;
                    }
                    if args.len > 0 {
                        if let Ok(entries) = self.body.body().ty_list(&self.tys, args) {
                            for a in entries.iter() {
                                if matches!(self.classify_needs_drop(a, depth + 1), NeedsDrop::Generic | NeedsDrop::Yes) {
                                    return NeedsDrop::Generic;
                                }
                            }
                        }
                    }
                    let mut generic = false;
                    for f in info.fields {
                        if let Ok(fn_node) = info.types.node(f.ty) {
                            if matches!(fn_node, PNode::Param(_) | PNode::Adt(..)) {
                                generic = true;
                            }
                        }
                    }
                    if generic { NeedsDrop::Yes } else { NeedsDrop::No }
                }
                _ => NeedsDrop::Yes,
            }
        }
    }

    impl<P: crate::seam::FrontEnd> blaze_body_drops::DropFacts for FlowDropFacts<'_, P> {
        fn at(&self, drop: blaze_body::Stmt) -> blaze_body_drops::DropState<'_> {
            use blaze_body_drops::DropState;
            match self.plans.iter().find(|(s, _)| *s == drop).map(|(_, p)| p) {
                Some(PreDrop::Uninit) => DropState::Uninit,
                Some(PreDrop::Init) | None => DropState::Init,
                Some(PreDrop::Maybe) => DropState::Maybe,
                Some(PreDrop::Partial(parts)) => DropState::Partial(parts.as_slice()),
                Some(PreDrop::Enum { discr_ty, variants }) => DropState::Enum { discr_ty: *discr_ty, variants: variants.as_slice() },
            }
        }

        fn needs_drop(&self, ty: blaze_typeck::PTy) -> blaze_body_drops::NeedsDrop {
            self.classify_needs_drop(ty, 0)
        }

        fn bool_ty(&self) -> blaze_typeck::PTy {
            self.bool_ty
        }

        fn project_ty(&self, ty: blaze_typeck::PTy, proj: blaze_body::Proj) -> Option<blaze_typeck::PTy> {
            use blaze_body::Proj;
            use blaze_typeck::PNode;
            let node = self.ty_node(ty)?;
            match (proj, node) {
                (Proj::Deref, PNode::Ref(_, inner) | PNode::Ptr(_, inner)) => Some(inner),
                (Proj::DerefBox, PNode::Adt(_, args)) => self.body.body().ty_list(&self.tys, args).ok()?.iter().next(),
                (Proj::Index(_) | Proj::ConstIndex { .. }, PNode::Array(elem, _) | PNode::Slice(elem)) => Some(elem),
                (Proj::Subslice { .. }, PNode::Slice(_)) => Some(ty),
                (Proj::Downcast(_), _) => Some(ty),
                _ => None,
            }
        }
    }

    /// Minimal `blaze_build::Adts` + `LayoutFacts` adapter over `Front2Program` for the side-by-side
    /// `blaze-body-build` vs legacy `blaze-build` oracle.
    struct OracleAdts<'a, P> {
        program: &'a P,
        fn_index: usize,
        checked: &'a [Option<blaze_typeck::Checked<'static>>],
        closures: &'a crate::run::ClosureTable,
        const_at: &'a crate::front2::const_at::ConstTable<Alloc<'static>>,
        alloc: Alloc<'static>,
    }

    impl<P: crate::seam::FrontEnd> blaze_build::layout::LayoutFacts for OracleAdts<'_, P> {
        fn adt(&self, key: ItemKey) -> Option<blaze_typeck::Adt<'_>> {
            self.program.adt(key)
        }
        fn repr(&self, key: ItemKey) -> Result<blaze_build::layout::ReprAttrs, blaze_build::layout::Missing> {
            self.program.repr(key)
        }
        fn discriminant(&self, adt: ItemKey, variant: u32) -> Result<Option<i128>, blaze_build::layout::Missing> {
            self.program.discriminant(adt, variant)
        }
        fn valid_range(&self, key: ItemKey) -> Result<blaze_build::layout::ValidRange, blaze_build::layout::Missing> {
            self.program.valid_range(key)
        }
        fn normalize(&self, trait_: ItemKey, name: Name, args: &[blaze_typeck::PTy], ground: &mut blaze_typeck::PortableTys<'_>) -> Result<blaze_typeck::PTy, blaze_build::BuildError> {
            match self.program.normalize(trait_, name, args, ground).map_err(blaze_build::BuildError::Select)? {
                Some(ty) => Ok(ty),
                None => Err(blaze_build::BuildError::Malformed { what: "projection not normalized in oracle" }),
            }
        }
        fn closure(&self, owner: ItemKey, ordinal: u32) -> Option<blaze_build::layout::ClosureFacts<'_>> {
            self.closures.get(owner, ordinal)
        }
    }

    impl<P: crate::seam::FrontEnd> blaze_build::Adts for OracleAdts<'_, P> {
        fn adt(&self, key: ItemKey) -> Option<blaze_typeck::Adt<'_>> {
            self.program.adt(key)
        }
        fn opaque_of(&self, opaque: ItemKey) -> Option<(ItemKey, u32)> {
            self.program.opaque_of(opaque)
        }
        fn overflow_checks(&self) -> blaze_build::OverflowChecks {
            blaze_build::OverflowChecks::Off
        }
        fn ub_checks(&self) -> blaze_build::UbChecks {
            blaze_build::UbChecks::Off
        }
        fn panic(&self) -> blaze_build::Panic {
            blaze_build::Panic::Abort
        }
        fn facts(&self) -> &dyn blaze_build::layout::LayoutFacts {
            self
        }
        fn int_lit(&self, owner: ItemKey, expr: ExprId) -> Option<u128> {
            self.program.int_lit(owner, expr)
        }
        fn int_pat(&self, owner: ItemKey, pat: PatId) -> Option<u128> {
            self.program.int_pat(owner, pat)
        }
        fn byte_str(&self, owner: ItemKey, expr: ExprId) -> Option<&[u8]> {
            self.program.string(owner, expr)
        }
        fn str_lit(&self, owner: ItemKey, expr: ExprId) -> Option<&str> {
            core::str::from_utf8(self.program.string(owner, expr)?).ok()
        }
        fn float_lit(&self, owner: ItemKey, expr: ExprId) -> Option<u128> {
            let (wide, narrow) = self.program.float_lit(owner, expr)?;
            let done = self.checked.get(self.program.fn_index(owner)?)?.as_ref()?;
            let ty = *done.expr_ty.get(expr.ix())?;
            match done.types.view().node(ty).ok()? {
                blaze_typeck::PNode::Float(blaze_typeck::FloatTy::F32) => Some(u128::from(narrow)),
                blaze_typeck::PNode::Float(blaze_typeck::FloatTy::F64) => Some(u128::from(wide)),
                _ => None,
            }
        }
        fn float_pat(&self, owner: ItemKey, pat: PatId) -> Option<u128> {
            let (wide, narrow) = self.program.float_pat(owner, pat)?;
            let done = self.checked.get(self.program.fn_index(owner)?)?.as_ref()?;
            let ty = *done.pat_ty.get(pat.ix())?;
            match done.types.view().node(ty).ok()? {
                blaze_typeck::PNode::Float(blaze_typeck::FloatTy::F32) => Some(u128::from(narrow)),
                blaze_typeck::PNode::Float(blaze_typeck::FloatTy::F64) => Some(u128::from(wide)),
                _ => None,
            }
        }
        fn str_pat(&self, owner: ItemKey, pat: PatId) -> Option<&[u8]> {
            self.program.str_pat(owner, pat)
        }
        fn fn_trait_method(&self, which: blaze_build::FnTrait) -> Option<ItemKey> {
            let (lang, name) = crate::front2::langs::fn_trait_method(which);
            self.program.lang_trait_fn(lang, name)
        }
        fn lang_fn(&self, name: &str) -> Option<ItemKey> {
            self.program.fn_lang(crate::front::FnLang::named(name)?)
        }
        fn drop_impl(&self, ty: blaze_typeck::PTy, ground: &mut blaze_typeck::PortableTys<'_>) -> Result<Option<ItemKey>, blaze_build::BuildError> {
            let Some((trait_, f)) = self.program.drop_trait() else { return Ok(None) };
            let blaze_typeck::PNode::Adt(adt, _) = ground.view().node(ty).map_err(|_| blaze_build::BuildError::Malformed { what: "past ground table" })? else {
                return Ok(None);
            };
            if self.program.adt_drop_impl(self.fn_index, trait_, adt, ground.alloc()).map_err(blaze_build::BuildError::Select)?.is_none() {
                return Ok(None);
            }
            f.map(Some).ok_or(blaze_build::BuildError::Malformed { what: "drop trait has no drop fn" })
        }
        fn index_fn(&self, mutbl: blaze_typeck::Mutbl) -> Option<ItemKey> {
            match mutbl {
                blaze_typeck::Mutbl::Not => self.program.lang_trait_fn(blaze_typeck::LangItem::Index, "index"),
                blaze_typeck::Mutbl::Mut => self.program.lang_trait_fn(blaze_typeck::LangItem::IndexMut, "index_mut"),
            }
        }
        fn deref_fn(&self, mutbl: blaze_typeck::Mutbl) -> Option<ItemKey> {
            match mutbl {
                blaze_typeck::Mutbl::Not => self.program.lang_trait_fn(blaze_typeck::LangItem::Deref, "deref"),
                blaze_typeck::Mutbl::Mut => self.program.lang_trait_fn(blaze_typeck::LangItem::DerefMut, "deref_mut"),
            }
        }
        fn lang_adt(&self, item: blaze_typeck::LangItem) -> Option<ItemKey> {
            self.program.lang_item(item).ok().flatten()
        }
        fn object_trait(&self, t: ItemKey) -> Option<blaze_build::vtable::ObjectTrait<'_>> {
            self.program.object_trait(t)
        }
        fn abi(&self, item: ItemKey) -> Option<blaze_build::Abi> {
            self.program.fn_abi(item)
        }
        fn nounwind(&self, owner: ItemKey) -> Option<bool> {
            self.program.nounwind(owner)
        }
        fn fn_attrs(&self, owner: ItemKey) -> blaze_lir::func::FnAttrs {
            self.program.fn_attrs(owner)
        }
        fn static_item(&self, item: ItemKey) -> Option<blaze_build::StaticItem> {
            self.program.static_item(item)
        }
        fn const_value(&self, item: ItemKey, args: &[blaze_typeck::PTy]) -> Option<&blaze_build::data::DataRecord<BuildRecAlloc>> {
            if let Some(record) = self.const_at.get(crate::front2::const_at::ConstNeed::Item(item), args) {
                return Some(record);
            }
            self.program.const_record(item)
        }
        fn const_block_value(&self, owner: ItemKey, block: ExprId, args: &[blaze_typeck::PTy]) -> Option<&blaze_build::data::DataRecord<BuildRecAlloc>> {
            self.const_at.get(crate::front2::const_at::ConstNeed::Block { owner, block }, args)
        }
        fn promoted_value(&self, owner: ItemKey, borrow: ExprId, args: &[blaze_typeck::PTy]) -> Option<&blaze_build::data::DataRecord<BuildRecAlloc>> {
            self.const_at.get(crate::front2::const_at::ConstNeed::Promoted { owner, borrow }, args)
        }
        fn const_fn_instance(&self, key: blaze_build::InstanceKey, ground: &mut blaze_typeck::PortableTys<'_>) -> Option<blaze_build::Instance> {
            self.program.const_fn_instance(key, ground, self.alloc)
        }
        fn const_vtable_ref(&self, key: blaze_build::vtable::VtableKey, ground: &mut blaze_typeck::PortableTys<'_>) -> Option<blaze_build::vtable::VtableRef> {
            self.program.const_vtable_ref(key, ground, self.alloc)
        }
        fn intrinsic(&self, item: ItemKey) -> Option<blaze_build::Intrinsic> {
            self.program.intrinsic(item)
        }
    }

    /// Multi-function LIR program for `blaze_lir::eval::run_calls_in`.
    struct EvalProgram<'a> {
        funcs: AVec<'a, blaze_build::Built<Alloc<'a>>>,
        /// Per func: per RefId, the target func index (or `u32::MAX` if unresolved/extern).
        call_targets: AVec<'a, AVec<'a, u32>>,
        /// Statics by DatumId: key, bytes, align_log2, writable, relocs.
        datums: AVec<'a, (ItemKey, AVec<'a, u8>, u8, bool, AVec<'a, blaze_lir::eval::DatumReloc>)>,
        /// Per func: per static RefId, datum index.
        static_targets: AVec<'a, AVec<'a, u32>>,
    }

    fn body_id_of(idx: usize) -> blaze_lir::eval::BodyId {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&(idx as u64).to_le_bytes());
        blaze_lir::eval::BodyId(bytes)
    }

    fn idx_of_body_id(id: blaze_lir::eval::BodyId) -> usize {
        u64::from_le_bytes([id.0[0], id.0[1], id.0[2], id.0[3], id.0[4], id.0[5], id.0[6], id.0[7]]) as usize
    }

    fn datum_id_of(idx: u32) -> blaze_lir::eval::DatumId {
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(&idx.to_le_bytes());
        blaze_lir::eval::DatumId(bytes)
    }

    fn idx_of_datum_id(id: blaze_lir::eval::DatumId) -> usize {
        u32::from_le_bytes([id.0[0], id.0[1], id.0[2], id.0[3]]) as usize
    }

    impl blaze_lir::eval::Bodies for EvalProgram<'_> {
        fn callee(&self, from: blaze_lir::eval::BodyId, reference: blaze_lir::func::RefId) -> Option<(blaze_lir::eval::BodyId, blaze_lir::View<'_>)> {
            let fi = idx_of_body_id(from);
            let f = self.funcs.get(fi)?;
            let r = f.func.view().refs.get(reference.0 as usize)?;
            if blaze_lir::func::RefKind::from_word(r.kind) != Some(blaze_lir::func::RefKind::Func) {
                return None;
            }
            let target = *self.call_targets.get(fi)?.get(r.place as usize)?;
            if target == u32::MAX {
                return None;
            }
            let ti = target as usize;
            Some((body_id_of(ti), self.funcs.get(ti)?.func.view()))
        }

        fn target(&self, from: blaze_lir::eval::BodyId, reference: blaze_lir::func::RefId) -> Option<blaze_lir::body::Target> {
            let fi = idx_of_body_id(from);
            let f = self.funcs.get(fi)?;
            let r = f.func.view().refs.get(reference.0 as usize)?;
            match blaze_lir::func::RefKind::from_word(r.kind)? {
                blaze_lir::func::RefKind::Func => {
                    let target = *self.call_targets.get(fi)?.get(r.place as usize)?;
                    (target != u32::MAX).then(|| blaze_lir::body::Target::Func(body_id_of(target as usize)))
                }
                blaze_lir::func::RefKind::Static => {
                    let di = *self.static_targets.get(fi)?.get(r.place as usize)?;
                    (di != u32::MAX).then(|| blaze_lir::body::Target::Datum(datum_id_of(di)))
                }
                _ => None,
            }
        }

        fn datum(&self, id: blaze_lir::eval::DatumId) -> Option<blaze_lir::eval::Datum<'_>> {
            let (_, bytes, align, writable, relocs) = self.datums.get(idx_of_datum_id(id))?;
            Some(blaze_lir::eval::Datum { bytes: bytes.as_slice(), align: *align, writable: *writable, relocs: relocs.as_slice() })
        }

        fn body(&self, id: blaze_lir::eval::BodyId) -> Option<blaze_lir::View<'_>> {
            self.funcs.get(idx_of_body_id(id)).map(|f| f.func.view())
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum BuilderSide {
        Legacy,
        D119Body,
    }

    /// Builds all reachable instances from `main` using either `blaze_build::build` or
    /// `blaze_body_build::build` (from kept + drop-elaborated bodies), emits every built fn
    /// to AArch64, and returns the runnable `EvalProgram`.
    fn build_rung_side<'a, P: crate::seam::FrontEnd>(
        program: &mut P,
        checked: &[Option<blaze_typeck::Checked<'static>>],
        side: BuilderSide,
        alloc: Alloc<'static>,
        out_alloc: Alloc<'a>,
    ) -> Result<(EvalProgram<'a>, u32), AString<'a>> {
        use crate::front::{FnKind, FnLang};
        let Some(entry_row) = program.entry() else {
            let mut s = AString::new_in(out_alloc);
            let _ = write!(s, "no entry");
            return Err(s);
        };
        let mut closures = crate::run::ClosureTable::new(alloc);
        for (idx, done) in checked.iter().enumerate() {
            let Some(done) = done else { continue };
            let sig = program.sig(idx);
            let generics = sig.parent_count + sig.own_count;
            let Some((body, _)) = program.function(idx).body else { continue };
            closures.add(&body.view(program.body_types(idx)), done, generics, alloc);
        }
        let coroutines = crate::front2::coroutines::Coroutines::of(program, checked, alloc);
        let mut const_at = crate::front2::const_at::ConstTable::new_in(alloc);
        let mut const_needs = crate::front2::const_at::Needs::new_in(alloc);
        let mut ground = blaze_typeck::PortableTys::new_in(alloc);
        let entry_item = program.function(entry_row).key;
        #[derive(Clone, Copy)]
        enum OracleSlot {
            Normal,
            Closure { of: blaze_build::ClosureOf, ty: blaze_typeck::PTy, which: blaze_build::FnTrait },
            FnShim { callee: blaze_typeck::PTy, tuple: blaze_typeck::PTy, which: blaze_build::FnTrait },
        }

        let mut instances: AVec<'a, blaze_build::Instance> = AVec::new_in(out_alloc);
        let mut funcs: AVec<'a, blaze_build::Built<Alloc<'a>>> = AVec::new_in(out_alloc);
        let mut call_targets: AVec<'a, AVec<'a, u32>> = AVec::new_in(out_alloc);
        let mut static_targets: AVec<'a, AVec<'a, u32>> = AVec::new_in(out_alloc);
        let mut datums: AVec<'a, (ItemKey, AVec<'a, u8>, u8, bool, AVec<'a, blaze_lir::eval::DatumReloc>)> = AVec::new_in(out_alloc);
        let mut slots: AVec<'a, OracleSlot> = AVec::new_in(out_alloc);
        instances.push(blaze_build::Instance { item: entry_item, args: blaze_typeck::List::EMPTY });
        slots.push(OracleSlot::Normal);

        let mut body_built_count = 0u32;
        let mut cursor = 0usize;
        while cursor < instances.len() {
            let inst = instances[cursor];
            let slot = slots[cursor];
            cursor += 1;
            let (i, raw_args_list) = match slot {
                OracleSlot::Closure { of, .. } => {
                    let Some(idx) = program.fn_index(of.owner) else {
                        let mut s = AString::new_in(out_alloc);
                        let _ = write!(s, "missing closure owner {:?}", of.owner);
                        return Err(s);
                    };
                    (idx, of.owner_args)
                }
                OracleSlot::FnShim { callee, .. } => match ground.view().node(callee).ok() {
                    Some(blaze_typeck::PNode::FnDef(item, fn_args)) => {
                        let Some(idx) = program.fn_index(item) else {
                            let mut s = AString::new_in(out_alloc);
                            let _ = write!(s, "missing fn_shim item {:?}", item);
                            return Err(s);
                        };
                        (idx, fn_args)
                    }
                    _ => (entry_row, blaze_typeck::List::EMPTY),
                },
                OracleSlot::Normal => {
                    let Some(idx) = program.fn_index(inst.item) else {
                        let mut s = AString::new_in(out_alloc);
                        let _ = write!(s, "missing fn_index for {:?}", inst.item);
                        return Err(s);
                    };
                    (idx, inst.args)
                }
            };
            let raw_args = ground.view().list(raw_args_list).expect("ground args");
            let mut args_copy = AVec::with_capacity_in(raw_args.len(), alloc);
            args_copy.extend_from_slice(raw_args);

            // Pull any generic const / const-block / promoted needs for this instance before building.
            if program.evaluates_at_args()
                && !matches!(slot, OracleSlot::FnShim { .. })
                && let Some((body, _)) = program.function(i).body
                && let Some(done) = checked[i].as_ref()
            {
                let view = body.view(program.body_types(i));
                const_needs.clear();
                {
                    let adts_for_needs = OracleAdts { program: &*program, fn_index: i, checked, closures: &closures, const_at: &const_at, alloc };
                    let _ = crate::front2::const_at::needs_of(&view, done, &args_copy, &mut ground, &adts_for_needs, alloc, &mut const_needs);
                }
                if !const_needs.is_empty() {
                    let _ = crate::front2::const_at::pull(
                        program,
                        &const_needs,
                        ground.view(),
                        &mut const_at,
                        crate::front2::const_at::BuildFacts::new(&closures, &coroutines),
                    );
                }
            }

            // Build with on-demand `make_asked` retries (mirrors `run.rs`'s lazy impl/head fill).
            let mut retries = 0u32;
            let built: blaze_build::Built<Alloc<'a>> = loop {
                let function = program.function(i);
                let adts = OracleAdts { program: &*program, fn_index: i, checked, closures: &closures, const_at: &const_at, alloc };
                let mut subst = blaze_build::Subst { args: &args_copy, ground: &mut ground, scratch: alloc };
                let is_missing_body_err = |e: &blaze_body_build::BuildError| {
                    matches!(
                        e,
                        blaze_body_build::BuildError::Facts { error: blaze_build::BuildError::Select(blaze_typeck::ResolveError::Missing(_)), .. }
                            | blaze_body_build::BuildError::NeedsDrop { error: blaze_build::drop_facts::NeedsDropError::Ask(blaze_build::BuildError::Select(blaze_typeck::ResolveError::Missing(_))), .. }
                    )
                };
                let attempt: Result<blaze_build::Built<Alloc<'a>>, (bool, AString<'a>)> = match slot {
                    OracleSlot::Closure { of, ty: closure_ty, which } => match side {
                        BuilderSide::Legacy => {
                            let (body, _) = function.body.expect("closure owner body");
                            let done = checked[i].as_ref().expect("closure owner checked");
                            let view = body.view(program.body_types(i));
                            blaze_build::build_closure(&view, done, of.ordinal, which, &program.sig(i), &mut subst, &adts, out_alloc).map_err(|e| {
                                let missing = matches!(e, blaze_build::BuildError::Select(blaze_typeck::ResolveError::Missing(_)));
                                let mut s = AString::new_in(out_alloc);
                                let _ = write!(s, "legacy closure `{}`#{}: {e}", function.path, of.ordinal);
                                (missing, s)
                            })
                        }
                        BuilderSide::D119Body => {
                            let own_kind = closures.get(of.owner, of.ordinal).map(|c| c.kind);
                            if own_kind == Some(which) {
                                (|| -> Result<blaze_build::Built<Alloc<'a>>, (bool, AString<'a>)> {
                                    let done = checked[i].as_ref().expect("closure owner checked");
                                    let kept_bytes = keep_frontend_closure_body(&*program, i, of.ordinal, done, checked, out_alloc).map_err(|e| {
                                        let mut s = AString::new_in(out_alloc);
                                        let _ = write!(s, "keep closure `{}`#{}: {e:?}", function.path, of.ordinal);
                                        (false, s)
                                    })?;
                                    let kept = blaze_body::kept::open(&kept_bytes, out_alloc).map_err(|e| {
                                        let mut s = AString::new_in(out_alloc);
                                        let _ = write!(s, "open closure `{}`#{}: {e:?}", function.path, of.ordinal);
                                        (false, s)
                                    })?;
                                    let proj = blaze_body_flow::project(kept.verified(), out_alloc).map_err(|e| {
                                        let mut s = AString::new_in(out_alloc);
                                        let _ = write!(s, "project closure `{}`#{}: {e}", function.path, of.ordinal);
                                        (false, s)
                                    })?;
                                    let drop_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("drop frame");
                                    let df = FlowDropFacts::build(&*program, kept.verified(), kept.tys(), &proj, out_alloc, &drop_frame).map_err(|e| {
                                        let mut s = AString::new_in(out_alloc);
                                        let _ = write!(s, "drop_facts closure `{}`#{}: {e:?}", function.path, of.ordinal);
                                        (false, s)
                                    })?;
                                    let elab = blaze_body_drops::elaborate(kept.verified(), &df, out_alloc).map_err(|e| {
                                        let mut s = AString::new_in(out_alloc);
                                        let _ = write!(s, "elaborate closure `{}`#{}: {e:?}", function.path, of.ordinal);
                                        (false, s)
                                    })?;
                                    let verified = blaze_body::verify(&elab).map_err(|e| {
                                        let mut s = AString::new_in(out_alloc);
                                        let _ = write!(s, "verify closure `{}`#{}: {e:?}", function.path, of.ordinal);
                                        (false, s)
                                    })?;
                                    blaze_body_build::build_closure_body(verified, kept.tys(), of.owner, &mut subst, &adts, out_alloc).map_err(|e| {
                                        let mut s = AString::new_in(out_alloc);
                                        let _ = write!(s, "d119 closure `{}`#{}: {e}", function.path, of.ordinal);
                                        (is_missing_body_err(&e), s)
                                    })
                                })()
                            } else {
                                blaze_body_build::build_closure_shim(closure_ty, which, &mut subst, &adts, out_alloc).map_err(|e| {
                                    let mut s = AString::new_in(out_alloc);
                                    let _ = write!(s, "d119 closure shim `{}`#{}: {e}", function.path, of.ordinal);
                                    (is_missing_body_err(&e), s)
                                })
                            }
                        }
                    },
                    OracleSlot::FnShim { callee, tuple, which } => {
                        let is_fn_def = matches!(subst.ground.view().node(callee), Ok(blaze_typeck::PNode::FnDef(..)));
                        let sig = is_fn_def.then(|| program.sig(i));
                        match side {
                            BuilderSide::Legacy => blaze_build::build_fn_shim(callee, tuple, which, sig.as_ref(), &mut subst, &adts, out_alloc).map_err(|e| {
                                let missing = matches!(e, blaze_build::BuildError::Select(blaze_typeck::ResolveError::Missing(_)));
                                let mut s = AString::new_in(out_alloc);
                                let _ = write!(s, "legacy fn_shim: {e}");
                                (missing, s)
                            }),
                            BuilderSide::D119Body => blaze_body_build::build_fn_shim(callee, tuple, which, sig.as_ref(), &mut subst, &adts, out_alloc).map_err(|e| {
                                let mut s = AString::new_in(out_alloc);
                                let _ = write!(s, "d119 fn_shim: {e}");
                                (is_missing_body_err(&e), s)
                            }),
                        }
                    }
                    OracleSlot::Normal if matches!(function.kind, FnKind::Foreign) => {
                        let mut s = AString::new_in(out_alloc);
                        let _ = write!(s, "extern fn `{}`", function.path);
                        Err((false, s))
                    }
                    OracleSlot::Normal if program.ctor_of(function.key).is_some() => {
                        let (_, variant) = program.ctor_of(function.key).expect("ctor_of");
                        match side {
                            BuilderSide::Legacy => blaze_build::build_ctor_fn(function.key, variant, &program.sig(i), &mut subst, &adts, out_alloc).map_err(|e| {
                                let missing = matches!(e, blaze_build::BuildError::Select(blaze_typeck::ResolveError::Missing(_)));
                                let mut s = AString::new_in(out_alloc);
                                let _ = write!(s, "legacy ctor `{}`: {e}", function.path);
                                (missing, s)
                            }),
                            BuilderSide::D119Body => blaze_body_build::build_ctor_fn(function.key, variant, &program.sig(i), &mut subst, &adts, out_alloc).map_err(|e| {
                                let mut s = AString::new_in(out_alloc);
                                let _ = write!(s, "d119 ctor `{}`: {e}", function.path);
                                (is_missing_body_err(&e), s)
                            }),
                        }
                    }
                    OracleSlot::Normal if Some(function.key) == program.fn_lang(FnLang::DropInPlace) => match side {
                        BuilderSide::Legacy => {
                            let (body, _) = function.body.expect("drop_in_place body");
                            let done = checked[i].as_ref().expect("drop_in_place checked");
                            let view = body.view(program.body_types(i));
                            blaze_build::build_drop_glue(&view, done, &program.sig(i), &mut subst, &adts, out_alloc).map_err(|e| {
                                let missing = matches!(e, blaze_build::BuildError::Select(blaze_typeck::ResolveError::Missing(_)));
                                let mut s = AString::new_in(out_alloc);
                                let _ = write!(s, "legacy drop_glue: {e}");
                                (missing, s)
                            })
                        }
                        BuilderSide::D119Body => blaze_body_build::build_drop_glue(&mut subst, &adts, out_alloc).map_err(|e| {
                            let mut s = AString::new_in(out_alloc);
                            let _ = write!(s, "d119 drop_glue: {e}");
                            (is_missing_body_err(&e), s)
                        }),
                    },
                    OracleSlot::Normal => {
                        let Some((body, _)) = function.body else {
                            let mut s = AString::new_in(out_alloc);
                            let _ = write!(s, "no body for `{}`", function.path);
                            return Err(s);
                        };
                        let Some(done) = checked[i].as_ref() else {
                            let mut s = AString::new_in(out_alloc);
                            let _ = write!(s, "unchecked `{}`", function.path);
                            return Err(s);
                        };
                        match side {
                            BuilderSide::Legacy => {
                                let view = body.view(program.body_types(i));
                                blaze_build::build(&view, done, &program.sig(i), &mut subst, &adts, out_alloc).map_err(|e| {
                                    let missing = matches!(e, blaze_build::BuildError::Select(blaze_typeck::ResolveError::Missing(_)));
                                    let mut s = AString::new_in(out_alloc);
                                    let _ = write!(s, "legacy build `{}`: {e}", function.path);
                                    (missing, s)
                                })
                            }
                            BuilderSide::D119Body => (|| -> Result<blaze_build::Built<Alloc<'a>>, (bool, AString<'a>)> {
                                let kept_bytes = keep_frontend_body(&*program, i, done, checked, out_alloc).map_err(|e| {
                                    let mut s = AString::new_in(out_alloc);
                                    let _ = write!(s, "keep `{}`: {e:?}", function.path);
                                    (false, s)
                                })?;
                                let kept = blaze_body::kept::open(&kept_bytes, out_alloc).map_err(|e| {
                                    let mut s = AString::new_in(out_alloc);
                                    let _ = write!(s, "open `{}`: {e:?}", function.path);
                                    (false, s)
                                })?;
                                let proj = blaze_body_flow::project(kept.verified(), out_alloc).map_err(|e| {
                                    let mut s = AString::new_in(out_alloc);
                                    let _ = write!(s, "project `{}`: {e}", function.path);
                                    (false, s)
                                })?;
                                let drop_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("drop frame");
                                let df = FlowDropFacts::build(&*program, kept.verified(), kept.tys(), &proj, out_alloc, &drop_frame).map_err(|e| {
                                    let mut s = AString::new_in(out_alloc);
                                    let _ = write!(s, "drop_facts `{}`: {e:?}", function.path);
                                    (false, s)
                                })?;
                                let elab = blaze_body_drops::elaborate(kept.verified(), &df, out_alloc).map_err(|e| {
                                    let mut s = AString::new_in(out_alloc);
                                    let _ = write!(s, "elaborate `{}`: {e:?}", function.path);
                                    (false, s)
                                })?;
                                let verified = blaze_body::verify(&elab).map_err(|e| {
                                    let mut s = AString::new_in(out_alloc);
                                    let _ = write!(s, "verify_elab `{}`: {e:?}", function.path);
                                    (false, s)
                                })?;
                                blaze_body_build::build(verified, kept.tys(), function.key, &mut subst, &adts, out_alloc).map_err(|e| {
                                    let mut s = AString::new_in(out_alloc);
                                    let _ = write!(s, "body_build `{}`: {e}", function.path);
                                    (is_missing_body_err(&e), s)
                                })
                            })(),
                        }
                    }
                };
                match attempt {
                    Ok(b) => {
                        body_built_count += 1;
                        break b;
                    }
                    Err((true, _)) if retries < 16 && matches!(program.make_asked(i), Ok(true)) => {
                        retries += 1;
                    }
                    Err((_, msg)) => return Err(msg),
                }
            };
            // Verify AArch64 baseline code emission succeeds for every built LIR function.
            let mut via = AVec::with_capacity_in(built.refs.len(), out_alloc);
            for _ in 0..built.refs.len() {
                via.push(blaze_emit::Via::Slot);
            }
            let mut code = blaze_emit::Code::new_in(out_alloc);
            let isa = blaze_emit::Isa { lse: true, os: blaze_emit::Os::MacOs };
            blaze_emit::emit(built.func.verified(), &via, isa, &mut code).map_err(|e| {
                let mut s = AString::new_in(out_alloc);
                let _ = write!(s, "emit `{}`: {e:?}", program.function(i).path);
                s
            })?;

            // Resolve every callee reference to its instance slot (including trait methods, closures, fn shims, foreign decls, and `make_asked` retries).
            let mut targets = AVec::with_capacity_in(built.refs.len(), out_alloc);
            for &callee in built.refs.iter() {
                // External C declarations (`extern "C" { fn ...; }`) have no body to build: record `u32::MAX` (unresolved/extern).
                if program.fn_index(callee.item).is_some_and(|ci| matches!(program.function(ci).kind, FnKind::Foreign)) {
                    targets.push(u32::MAX);
                    continue;
                }
                // Check if this is an `Fn`/`FnMut`/`FnOnce` call (closure, fn item, or fn pointer).
                let fn_trait_call = [blaze_build::FnTrait::Fn, blaze_build::FnTrait::FnMut, blaze_build::FnTrait::FnOnce]
                    .into_iter()
                    .find(|&w| {
                        let (lang, name) = crate::front2::langs::fn_trait_method(w);
                        program.lang_trait_fn(lang, name) == Some(callee.item)
                    })
                    .and_then(|which| {
                        let args_slice = ground.view().list(callee.args).ok()?;
                        let first = *args_slice.first()?;
                        if let Some(of) = blaze_build::closure_of(ground.view(), first) {
                            Some(OracleSlot::Closure { of, ty: first, which })
                        } else if matches!(ground.view().node(first), Ok(blaze_typeck::PNode::FnDef(..) | blaze_typeck::PNode::FnPtr(..)))
                            && let Some(&tuple) = args_slice.get(1)
                        {
                            Some(OracleSlot::FnShim { callee: first, tuple, which })
                        } else {
                            None
                        }
                    });
                let (resolved, cslot) = if let Some(s) = fn_trait_call {
                    (callee, s)
                } else if Some(callee.item) == program.fn_lang(FnLang::DropInPlace) {
                    (callee, OracleSlot::Normal)
                } else if program.fn_index(callee.item).is_some_and(|ci| matches!(program.function(ci).kind, FnKind::Ctor)) {
                    (callee, OracleSlot::Normal)
                } else {
                    // Always resolve through `FrontEnd::resolve` (covers trait methods with OR without default bodies, and retries `make_asked`).
                    let mut r_retries = 0u32;
                    let res = loop {
                        match program.resolve(i, callee.item, callee.args, &mut ground) {
                            Ok((ritem, rargs)) => break Ok(blaze_build::Instance { item: ritem, args: rargs }),
                            Err(blaze_typeck::ResolveError::Missing(_)) if r_retries < 16 && matches!(program.make_asked(i), Ok(true)) => {
                                r_retries += 1;
                            }
                            Err(e) => break Err(e),
                        }
                    };
                    match res {
                        Ok(r) => (r, OracleSlot::Normal),
                        Err(_) if program.fn_index(callee.item).is_some() => (callee, OracleSlot::Normal),
                        Err(e) => {
                            let mut s = AString::new_in(out_alloc);
                            let _ = write!(s, "resolve `{}` -> {:?}: {e:?}", program.function(i).path, callee.item);
                            return Err(s);
                        }
                    }
                };
                if let Some(pos) = instances.iter().position(|&x| x == resolved) {
                    targets.push(u32::try_from(pos).unwrap_or(u32::MAX));
                } else {
                    let pos = u32::try_from(instances.len()).unwrap_or(u32::MAX);
                    instances.push(resolved);
                    slots.push(cslot);
                    targets.push(pos);
                }
            }

            // Helper to intern a static record (and all its sub-allocations + target statics) into `datums`.
            fn intern_static<'a, P: crate::seam::FrontEnd>(
                program: &P,
                st_key: ItemKey,
                ground: &mut blaze_typeck::PortableTys<'_>,
                instances: &mut AVec<'a, blaze_build::Instance>,
                slots: &mut AVec<'a, OracleSlot>,
                datums: &mut AVec<'a, (ItemKey, AVec<'a, u8>, u8, bool, AVec<'a, blaze_lir::eval::DatumReloc>)>,
                alloc: Alloc<'static>,
                out_alloc: Alloc<'a>,
            ) -> u32 {
                if let Some(pos) = datums.iter().position(|(k, ..)| *k == st_key) {
                    return u32::try_from(pos).unwrap_or(u32::MAX);
                }
                let Some(rec) = program.static_records().iter().find(|(k, _)| *k == st_key).map(|(_, r)| r) else {
                    return u32::MAX;
                };
                let base_pos = u32::try_from(datums.len()).unwrap_or(u32::MAX);
                let writable_static = program.static_item(st_key).is_some_and(|s| s.mutable);
                // Reserve datum slots for all allocations in this record first so Alloc relocs know their target DatumId.
                for (ai, a) in rec.allocs.iter().enumerate() {
                    let mut bytes = AVec::new_in(out_alloc);
                    bytes.extend_from_slice(&a.bytes);
                    let key = if ai == 0 { st_key } else { ItemKey(0) };
                    let writable = a.mutable || (ai == 0 && writable_static);
                    datums.push((key, bytes, a.align, writable, AVec::new_in(out_alloc)));
                }
                if rec.allocs.is_empty() {
                    datums.push((st_key, AVec::new_in(out_alloc), 0, writable_static, AVec::new_in(out_alloc)));
                    return base_pos;
                }
                for (ai, a) in rec.allocs.iter().enumerate() {
                    let mut relocs = AVec::new_in(out_alloc);
                    for rel in a.relocs.iter() {
                        let off = usize::try_from(rel.offset).unwrap_or(usize::MAX);
                        let addend = if off.saturating_add(8) <= a.bytes.len() {
                            i64::from_le_bytes([
                                a.bytes[off], a.bytes[off + 1], a.bytes[off + 2], a.bytes[off + 3],
                                a.bytes[off + 4], a.bytes[off + 5], a.bytes[off + 6], a.bytes[off + 7],
                            ])
                        } else {
                            0
                        };
                        match rel.target {
                            blaze_build::data::DataTarget::Fn(fkey) => {
                                if let Some(inst) = program.const_fn_instance(fkey, ground, alloc) {
                                    let fslot = if let Some(p) = instances.iter().position(|&x| x == inst) {
                                        p
                                    } else {
                                        let p = instances.len();
                                        instances.push(inst);
                                        slots.push(OracleSlot::Normal);
                                        p
                                    };
                                    relocs.push(blaze_lir::eval::DatumReloc {
                                        offset: rel.offset,
                                        target: blaze_lir::body::Target::Func(body_id_of(fslot)),
                                        addend,
                                    });
                                }
                            }
                            blaze_build::data::DataTarget::Local(sub) => {
                                relocs.push(blaze_lir::eval::DatumReloc {
                                    offset: rel.offset,
                                    target: blaze_lir::body::Target::Datum(datum_id_of(base_pos + sub)),
                                    addend,
                                });
                            }
                            blaze_build::data::DataTarget::Static(target_st) => {
                                let tpos = intern_static(program, target_st, ground, instances, slots, datums, alloc, out_alloc);
                                if tpos != u32::MAX {
                                    relocs.push(blaze_lir::eval::DatumReloc {
                                        offset: rel.offset,
                                        target: blaze_lir::body::Target::Datum(datum_id_of(tpos)),
                                        addend,
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                    datums[usize::try_from(base_pos).unwrap_or(0) + ai].4 = relocs;
                }
                base_pos
            }

            let mut statics = AVec::with_capacity_in(built.statics.len(), out_alloc);
            for &st_key in built.statics.iter() {
                let pos = intern_static(&*program, st_key, &mut ground, &mut instances, &mut slots, &mut datums, alloc, out_alloc);
                statics.push(pos);
            }

            call_targets.push(targets);
            static_targets.push(statics);
            funcs.push(built);
        }

        Ok((EvalProgram { funcs, call_targets, datums, static_targets }, body_built_count))
    }

    /// Oracle 1 & 2: lowers EVERY body of ALL 32 `#![no_core]` ladder rungs through
    /// `keep_frontend_body` -> `blaze_body::kept::open` (deterministic round-trip) ->
    /// `blaze_body_flow::project` -> `blaze_flow::moves::inits` -> `blaze_body_drops::elaborate` ->
    /// `blaze_body::verify`.
    #[test]
    fn d119_ladder_kept_body_flow_and_drops_oracle() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let mut total_bodies = 0u32;
        let mut kept_ok = 0u32;
        let mut proj_ok = 0u32;
        let mut elab_ok = 0u32;
        let mut report = AString::new_in(alloc);

        for rung in LADDER_RUNGS {
            with_front2(rung.files, alloc, |program, checked| {
                let mut r_bodies = 0u32;
                let mut r_kept = 0u32;
                let mut r_proj = 0u32;
                let mut r_elab = 0u32;
                for i in 0..crate::seam::FrontEnd::fn_count(program) {
                    let function = crate::seam::FrontEnd::function(program, i);
                    let Some(_) = function.body else { continue };
                    let Some(done) = checked[i].as_ref() else { continue };
                    r_bodies += 1;
                    total_bodies += 1;
                    let bytes = match keep_frontend_body(program, i, done, checked, alloc) {
                        Ok(b) => b,
                        Err(e) => {
                            let _ = writeln!(report, "  KEEP_ERR {}::{}: {:?}", rung.name, function.path, e);
                            continue;
                        }
                    };
                    let kept = match blaze_body::kept::open(&bytes, alloc) {
                        Ok(k) => k,
                        Err(e) => {
                            let _ = writeln!(report, "  OPEN_ERR {}::{}: {:?}", rung.name, function.path, e);
                            continue;
                        }
                    };
                    let again = blaze_body::kept::encode_ref(&kept, alloc).expect("kept re-encode");
                    assert_eq!(bytes.as_slice(), again.as_slice(), "kept round-trip mismatch in {}::{}", rung.name, function.path);
                    r_kept += 1;
                    kept_ok += 1;

                    let proj = match blaze_body_flow::project(kept.verified(), alloc) {
                        Ok(p) => p,
                        Err(e) => {
                            let _ = writeln!(report, "  PROJ_ERR {}::{}: {}", rung.name, function.path, e);
                            continue;
                        }
                    };
                    r_proj += 1;
                    proj_ok += 1;

                    let drop_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("drop frame");
                    let df = match FlowDropFacts::build(program, kept.verified(), kept.tys(), &proj, alloc, &drop_frame) {
                        Ok(df) => df,
                        Err(e) => {
                            let _ = writeln!(report, "  MOVES_ERR {}::{}: {:?}", rung.name, function.path, e);
                            continue;
                        }
                    };
                    match blaze_body_drops::elaborate(kept.verified(), &df, alloc) {
                        Ok(elab) => {
                            if let Err(ve) = blaze_body::verify(&elab) {
                                let _ = writeln!(report, "  VERIFY_ELAB_ERR {}::{}: {:?}", rung.name, function.path, ve);
                            } else {
                                r_elab += 1;
                                elab_ok += 1;
                            }
                        }
                        Err(e) => {
                            let _ = writeln!(report, "  ELAB_ERR {}::{}: {:?}", rung.name, function.path, e);
                        }
                    }
                }
                let _ = writeln!(report, "rung {}: {r_elab}/{r_proj}/{r_kept}/{r_bodies} (elab/proj/kept/total)", rung.name);
            });
        }
        assert!(
            kept_ok == total_bodies && proj_ok == total_bodies && elab_ok == total_bodies,
            "D119 kept/flow/drops oracle: elab={elab_ok}, proj={proj_ok}, kept={kept_ok} of {total_bodies} bodies:\n{report}"
        );
    }

    /// Oracle 3: side-by-side execution oracle comparing legacy `blaze_build::build` vs
    /// `blaze_body_build::build` (from kept + drop-elaborated bodies) + AArch64 emit +
    /// `blaze_lir::eval::run_calls_in` across the pure `#![no_core]` ladder rungs.
    #[test]
    fn d119_ladder_body_build_vs_legacy_build_oracle() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let mut passed = 0u32;
        let mut diffs = AString::new_in(alloc);

        for rung in LADDER_RUNGS {
            with_front2(rung.files, alloc, |program, checked| {
                let old = build_rung_side(program, checked, BuilderSide::Legacy, alloc, alloc);
                let new = build_rung_side(program, checked, BuilderSide::D119Body, alloc, alloc);
                match (old, new) {
                    (Ok((old_prog, old_n)), Ok((new_prog, new_n))) => {
                        if rung.has_extern_call || rung.eval_skip {
                            assert!(old_n > 0 && new_n > 0);
                            passed += 1;
                            return;
                        }
                        let old_out = blaze_lir::eval::run_calls_in(old_prog.funcs[0].func.view(), body_id_of(0), &[], 500_000, &old_prog, alloc);
                        let new_out = blaze_lir::eval::run_calls_in(new_prog.funcs[0].func.view(), body_id_of(0), &[], 500_000, &new_prog, alloc);
                        match (old_out, new_out) {
                            (Ok(o), Ok(n)) => {
                                let want_ok = match &n {
                                    blaze_lir::eval::Outcome::Returned(vals) if vals.is_empty() => rung.expected == 0,
                                    blaze_lir::eval::Outcome::Returned(vals) => matches!(vals[0], blaze_lir::eval::Val::Int(v) if v == u128::from(rung.expected)),
                                    _ => false,
                                };
                                // Note: on `front2/x0-drop-14-if-let-temp` and `front2/x0-drop-08-return-break`, legacy
                                // `blaze-build` hits a dead-slot UB in `blaze_lir::eval` while the D119 pipeline
                                // cleanly returns `42`.
                                let legacy_dead_slot_bug = matches!(rung.name, "front2/x0-drop-14-if-let-temp" | "front2/x0-drop-08-return-break")
                                    && matches!(o, blaze_lir::eval::Outcome::Undefined { why: "an access to a dead slot", .. });
                                if (!legacy_dead_slot_bug && o != n) || !want_ok {
                                    let _ = writeln!(diffs, "EXEC_DIFF {}: old={o:?} new={n:?} want={} (bodies {old_n}/{new_n})", rung.name, rung.expected);
                                } else {
                                    passed += 1;
                                }
                            }
                            (o, n) => {
                                let _ = writeln!(diffs, "EVAL_ERR {}: old={o:?} new={n:?}", rung.name);
                            }
                        }
                    }
                    (Err(oe), Err(ne)) => {
                        let _ = writeln!(diffs, "BOTH_BUILD_ERR {}: old={oe} | new={ne}", rung.name);
                    }
                    (Ok(_), Err(ne)) => {
                        let _ = writeln!(diffs, "D119_ONLY_BUILD_ERR {}: {ne}", rung.name);
                    }
                    (Err(oe), Ok(_)) => {
                        let _ = writeln!(diffs, "LEGACY_ONLY_BUILD_ERR {}: {oe}", rung.name);
                    }
                }
            });
        }
        assert!(
            diffs.is_empty() && usize::try_from(passed) == Ok(LADDER_RUNGS.len()),
            "D119 body-build vs legacy-build oracle ({passed}/{} passed):\n{diffs}",
            LADDER_RUNGS.len(),
        );
    }

    /// World implementing both `blaze_consteval::ConstWorld` and `blaze_body_eval::BodyWorld`
    /// over `Front2Program` + kept bodies for side-by-side const-eval parity verification.
    struct OracleConstWorld<'a, P> {
        program: &'a P,
        checked: &'a [Option<blaze_typeck::Checked<'static>>],
        kept: AVec<'a, Option<blaze_body::Kept<'a, Alloc<'a>>>>,
        store: AVec<'a, blaze_consteval::ConstRecord<Alloc<'a>>>,
    }

    impl<P: crate::seam::FrontEnd> blaze_build::layout::LayoutFacts for OracleConstWorld<'_, P> {
        fn adt(&self, key: ItemKey) -> Option<blaze_typeck::Adt<'_>> {
            self.program.adt(key)
        }
        fn repr(&self, key: ItemKey) -> Result<blaze_build::layout::ReprAttrs, blaze_build::layout::Missing> {
            self.program.repr(key)
        }
        fn discriminant(&self, adt: ItemKey, variant: u32) -> Result<Option<i128>, blaze_build::layout::Missing> {
            self.program.discriminant(adt, variant)
        }
        fn valid_range(&self, key: ItemKey) -> Result<blaze_build::layout::ValidRange, blaze_build::layout::Missing> {
            self.program.valid_range(key)
        }
        fn normalize(&self, trait_: ItemKey, name: Name, args: &[blaze_typeck::PTy], ground: &mut blaze_typeck::PortableTys<'_>) -> Result<blaze_typeck::PTy, blaze_build::BuildError> {
            match self.program.normalize(trait_, name, args, ground).map_err(blaze_build::BuildError::Select)? {
                Some(ty) => Ok(ty),
                None => Err(blaze_build::BuildError::Malformed { what: "projection not normalized" }),
            }
        }
    }

    impl<P: crate::seam::FrontEnd> blaze_consteval::ConstWorld for OracleConstWorld<'_, P> {
        fn body(&self, item: ItemKey) -> blaze_typeck::Fetch<Option<blaze_consteval::Typed<'_>>> {
            let Some(i) = self.program.fn_index(item) else { return Ok(None) };
            let function = self.program.function(i);
            let Some((body, l)) = function.body else { return Ok(None) };
            let Some(checked) = self.checked[i].as_ref() else { return Err(blaze_typeck::Missing) };
            let lits = blaze_consteval::Lits { text: &l.text, exprs: (&l.exprs, &l.expr_ranges), pats: (&l.pats, &l.pat_ranges) };
            let view = body.view(self.program.body_types(i));
            Ok(Some(blaze_consteval::Typed::new(view, checked, function.text, lits, self.program.code_of(i))))
        }

        fn fn_kind(&self, item: ItemKey) -> blaze_consteval::FnKind {
            let Some(i) = self.program.fn_index(item) else { return blaze_consteval::FnKind::NotConst };
            let function = self.program.function(i);
            match function.kind {
                crate::front::FnKind::Ctor => self.program.ctor_of(item).map_or(blaze_consteval::FnKind::NotConst, |(adt, variant)| blaze_consteval::FnKind::Ctor { adt, variant }),
                _ if function.const_fn => blaze_consteval::FnKind::Const,
                _ => blaze_consteval::FnKind::NotConst,
            }
        }

        fn code_id(&self, item: ItemKey) -> blaze_ids::ContentId {
            match self.program.fn_index(item) {
                Some(i) => self.program.code_of(i),
                None => crate::symbols::item_identity(item),
            }
        }

        fn is_static(&self, item: ItemKey) -> bool {
            self.program.static_item(item).is_some()
        }

        fn record(&self, key: blaze_consteval::RecordKey) -> blaze_typeck::Fetch<&blaze_consteval::ConstRecord<Alloc<'_>>> {
            self.store.iter().find(|r| r.key == key).ok_or(blaze_typeck::Missing)
        }

        fn explicit_discriminant(&self, _: ItemKey, _: u32) -> Option<ItemKey> {
            None
        }

        fn fn_trait_method(&self, which: blaze_build::FnTrait) -> blaze_consteval::FnTraitMethod {
            let (lang, name) = crate::front2::langs::fn_trait_method(which);
            self.program.lang_trait_fn(lang, name).map_or(blaze_consteval::FnTraitMethod::Absent, blaze_consteval::FnTraitMethod::Found)
        }

        fn resolve(&self, item: ItemKey, args: blaze_typeck::List, ground: &mut blaze_typeck::PortableTys<'_>, _scratch: Alloc<'_>) -> Option<(ItemKey, blaze_typeck::List)> {
            let entry = self.program.entry().unwrap_or(0);
            self.program.resolve(entry, item, args, ground).ok()
        }
    }

    impl<P: crate::seam::FrontEnd> blaze_body_eval::BodyWorld for OracleConstWorld<'_, P> {
        fn verified(&self, item: ItemKey) -> blaze_typeck::Fetch<Option<blaze_body_eval::Bodied<'_>>> {
            let Some(i) = self.program.fn_index(item) else { return Ok(None) };
            let Some(kept) = self.kept.get(i).and_then(|k| k.as_ref()) else { return Ok(None) };
            Ok(Some(blaze_body_eval::Bodied { item, body: kept.verified(), types: kept.tys() }))
        }

        fn drop_glue(&self, _ty: blaze_typeck::PTy, _ground: &mut blaze_typeck::PortableTys<'_>, _scratch: Alloc<'_>) -> Result<blaze_body_eval::DropGlue, blaze_consteval::Refusal<'static>> {
            Ok(blaze_body_eval::DropGlue::Nothing)
        }

        fn is_mut_static(&self, item: ItemKey) -> bool {
            self.program.static_item(item).is_some_and(|s| s.mutable)
        }
    }

    const CONST_ORACLE_SRC: &str = r#"
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}
#[lang = "copy"]
trait Copy {}
#[lang = "sync"]
unsafe trait Sync {}
#[lang = "unsafe_cell"]
#[repr(transparent)]
struct UnsafeCell<T> { value: T }
impl Copy for u8 {}
impl Copy for i32 {}
impl Copy for u32 {}
impl Copy for u64 {}
impl Copy for usize {}
impl Copy for bool {}

struct Pair { a: i32, b: i32 }
enum Choice { Zero, One(i32), Two(i32, i32) }
struct AtomicU8 { v: UnsafeCell<u8> }
enum CellSlot { Empty, Full(UnsafeCell<u32>) }

unsafe impl Sync for i32 {}
unsafe impl Sync for u8 {}
unsafe impl Sync for AtomicU8 {}
unsafe impl Sync for (u8, AtomicU8) {}
unsafe impl Sync for [AtomicU8; 2] {}
unsafe impl Sync for CellSlot {}
unsafe impl<'a> Sync for &'a AtomicU8 {}

const fn add(a: i32, b: i32) -> i32 { a + b }
const fn fact(n: i32) -> i32 {
    let mut acc = 1;
    let mut i = 2;
    while i <= n {
        acc *= i;
        i += 1;
    }
    acc
}
const fn sum_pair(p: Pair) -> i32 { p.a + p.b }
const fn eval_choice(c: Choice) -> i32 {
    match c {
        Choice::Zero => 0,
        Choice::One(x) => x * 2,
        Choice::Two(a, b) => a + b,
    }
}

const C_ADD: i32 = add(19, 23);
const C_FACT: i32 = fact(5);
const C_PAIR: Pair = Pair { a: 10, b: 32 };
const C_PAIR_SUM: i32 = sum_pair(C_PAIR);
const C_ENUM: i32 = eval_choice(Choice::Two(17, 25));
const C_ARR: [i32; 4] = [1, 2, 3, 4];
const C_ARR_SUM: i32 = C_ARR[0] + C_ARR[1] + C_ARR[2] + C_ARR[3];
const C_REF: &[i32; 4] = &C_ARR;
const K_ATOMIC: AtomicU8 = AtomicU8 { v: UnsafeCell { value: 1 } };
static S_VAL: i32 = C_ADD + C_ENUM;
static S_ATOMIC: AtomicU8 = AtomicU8 { v: UnsafeCell { value: 2 } };
static S_TUPLE_CELL: (u8, AtomicU8) = (3, AtomicU8 { v: UnsafeCell { value: 4 } });
static S_ARR_CELL: [AtomicU8; 2] = [AtomicU8 { v: UnsafeCell { value: 5 } }, AtomicU8 { v: UnsafeCell { value: 6 } }];
static S_ENUM_CELL: CellSlot = CellSlot::Full(UnsafeCell { value: 7 });
static S_REF_ATOMIC: &AtomicU8 = &AtomicU8 { v: UnsafeCell { value: 8 } };
static mut S_MUT_SCALAR: i32 = 9;

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    C_ADD + C_FACT + C_PAIR_SUM + C_ENUM + C_ARR_SUM + C_REF[2] + S_VAL + (K_ATOMIC.v.value as i32)
}
"#;

    /// Oracle 4: side-by-side const evaluation (`blaze_body_eval::eval_body` vs legacy
    /// `blaze_consteval::value_of`) across all `const` and `static` items in ladder rungs
    /// plus a dedicated `const fn` / struct / enum / array / static suite.
    #[test]
    fn d119_ladder_body_eval_vs_consteval_oracle() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let mut compared = 0u32;
        let mut diffs = AString::new_in(alloc);

        let check_files = |label: &str, files: &[(&str, &str)], compared: &mut u32, diffs: &mut AString<'_>| {
            let mut views = AVec::with_capacity_in(files.len(), alloc);
            for &(path, text) in files {
                views.push(crate::refusal::SourceFile { path, text });
            }
            let mut attrs = crate::front::BlazeAttrs::default();
            let Ok(mut program) = crate::front::Front::new(&views, "src/main.rs", &mut attrs, alloc) else {
                return;
            };
            let Ok(checked) = crate::seam::FrontEnd::check_all(&mut program) else {
                return;
            };
            let program = &program;
            let checked = &checked;
            let n = crate::seam::FrontEnd::fn_count(program);
            let mut raw_kept: AVec<'_, Option<AVec<'_, u8>>> = AVec::with_capacity_in(n, alloc);
            for i in 0..n {
                let function = crate::seam::FrontEnd::function(program, i);
                if function.body.is_some()
                    && let Some(done) = checked[i].as_ref()
                {
                    raw_kept.push(keep_frontend_body(program, i, done, checked, alloc).ok());
                } else {
                    raw_kept.push(None);
                }
            }
            let mut kept: AVec<'_, Option<blaze_body::Kept<'_, Alloc<'_>>>> = AVec::with_capacity_in(n, alloc);
            for slot in raw_kept.iter() {
                kept.push(slot.as_ref().and_then(|bytes| blaze_body::kept::open(bytes, alloc).ok()));
            }

            let mut world = OracleConstWorld { program, checked, kept, store: AVec::new_in(alloc) };
            let mut ground = blaze_typeck::PortableTys::new_in(alloc);
            let mut layout = blaze_build::layout::Layouts::new_in(alloc);
            let mut codes = blaze_consteval::Codes::new_in(alloc);

            for i in 0..n {
                let function = crate::seam::FrontEnd::function(program, i);
                if !matches!(function.kind, crate::front::FnKind::Const | crate::front::FnKind::Static)
                    || function.body.is_none()
                    || function.condition
                {
                    continue;
                }
                // Settle dependencies in legacy evaluator first so `world.store` holds prerequisites.
                let mut settle = blaze_consteval::Settle::new(
                    blaze_consteval::Need { kind: blaze_consteval::NeedKind::Value, item: function.key, args: blaze_typeck::List::EMPTY },
                    64,
                    alloc,
                );
                let mut old_key = None;
                let mut old_err = None;
                while let Some(need) = settle.next() {
                    match blaze_consteval::value_of(&world, &mut layout, &mut codes, need.item, need.args, &mut ground, blaze_consteval::Limits::DEFAULT, alloc) {
                        Ok(blaze_consteval::Answer::Done(record)) => {
                            old_key = Some(record.key);
                            if !world.store.iter().any(|r| r.key == record.key) {
                                world.store.push(record);
                            }
                            settle.done();
                        }
                        Ok(blaze_consteval::Answer::Need(dep)) => {
                            if let Err(e) = settle.need(dep) {
                                old_err = Some(e);
                                break;
                            }
                        }
                        Err(e) => {
                            old_err = Some(e);
                            break;
                        }
                    }
                }
                if let Some(e) = old_err {
                    let _ = writeln!(diffs, "LEGACY_EVAL_ERR {label}::{}: {e:?}", function.path);
                    continue;
                }
                let Some(old_rec) = old_key.and_then(|k| world.store.iter().find(|r| r.key == k)) else {
                    continue;
                };

                // Now evaluate the EXACT same item via `blaze_body_eval::eval_body`!
                match blaze_body_eval::eval_body(&world, &mut layout, &mut ground, function.key, blaze_typeck::List::EMPTY, blaze_consteval::Limits::DEFAULT, alloc) {
                    Ok(blaze_consteval::Answer::Done(new_data)) => {
                        let same_len = old_rec.data.allocs.len() == new_data.allocs.len();
                        let same_allocs = same_len
                            && old_rec.data.allocs.iter().zip(new_data.allocs.iter()).all(|(a, b)| {
                                a.bytes == b.bytes && a.align == b.align && a.mutable == b.mutable && a.relocs == b.relocs
                            });
                        if !same_allocs {
                            let _ = writeln!(
                                diffs,
                                "EVAL_MISMATCH {label}::{}: old_allocs={} new_allocs={} old0={:?} new0={:?}",
                                function.path,
                                old_rec.data.allocs.len(),
                                new_data.allocs.len(),
                                old_rec.data.allocs.first().map(|a| &a.bytes),
                                new_data.allocs.first().map(|a| &a.bytes),
                            );
                        } else {
                            *compared += 1;
                        }
                    }
                    Ok(blaze_consteval::Answer::Need(need)) => {
                        let _ = writeln!(diffs, "BODY_EVAL_NEED {label}::{}: {need:?}", function.path);
                    }
                    Err(e) => {
                        let _ = writeln!(diffs, "BODY_EVAL_ERR {label}::{}: {e:?}", function.path);
                    }
                }
            }
        };

        check_files("const_oracle_src", &[("src/main.rs", CONST_ORACLE_SRC)], &mut compared, &mut diffs);
        for rung in LADDER_RUNGS {
            check_files(rung.name, rung.files, &mut compared, &mut diffs);
        }
        assert!(diffs.is_empty() && compared >= 8, "D119 body-eval vs consteval oracle ({compared} matched):\n{diffs}");
    }
}

#[cfg(test)]
mod read_id_tests {
    use super::*;
    use super::super::head_tests::{CORE_AT, a, world_of};
    use super::super::view::TablesOpts;

    #[test]
    fn sibling_items_return_only_new_cells_and_keep_their_own_verdicts() {
        use blaze_drive::key::Key;
        use blaze_engine::{Phase, Planned, Stage};
        use super::super::head_tests::cells;

        let (records, _, frame) = world_of("#![no_std]\npub fn base() -> u8 { 1u8 }\npub fn left() -> u8 { base() }\npub fn right() -> u16 { base() }");
        let unused = Recorder::new(RecorderOpts { alloc: a() });
        let tables = Tables::new(TablesOpts { records, impls: None, lang_crates: &[CORE_AT], reads: &unused });
        let frame = BodyFrame { frame, alloc: a(), plant_drop: None, cost: None, needs: NeedsMet::All, heads: None, sources: None, goals: None, prepared: None, front: None };
        let heads = cells(records);
        let mut entry = Recorder::new(RecorderOpts { alloc: a() });
        let mut encoding = AString::new_in(a());
        let mut union = std::collections::BTreeSet::new();
        for label in ["left", "right"] {
            let key = heads.iter().map(|want| want.key()).find(|&key| records.name_of(key) == Some(label)).expect("fixture function");
            let task = Task { planned: Planned { name: Name::new(Kind::BodyCheck, key.0), phase: Phase::new(Stage::Bodies, 0), source: ABSENT, file: Name::new(Kind::FileItems, ABSENT) }, key: Key(ABSENT) };
            let alone = check_body_reads(tables, &task, &frame, a()).expect("item checked alone");
            let together = check_body_item(tables, &task, &frame, a(), &mut entry, None, &mut encoding, None).expect("item checked in the entry");
            let fresh: std::collections::BTreeSet<_> = alone.reads.iter().copied().filter(|key| union.insert(*key)).collect();
            assert_eq!(together.reads.iter().copied().collect::<std::collections::BTreeSet<_>>(), fresh);
            assert_eq!(together.reads.len(), fresh.len());
            assert_eq!(together.read_tally.offered, alone.read_tally.offered);
            assert_eq!(together.read_tally.kept, fresh.len() as u64);
            assert_eq!(together.out, alone.out);
            assert_eq!(together.said.iter().filter(|line| matches!(line.what, SaidWhat::Type { code: "E0308" })).count(), usize::from(label == "right"));
            if label == "right" {
                assert!(fresh.len() < alone.reads.len(), "both siblings depend on base");
            }
        }
        std::println!("shared cells kept once; left checks, right reports E0308");
    }

    #[test]
    fn resetting_the_stretch_memo_preserves_cell_identity() {
        let (records, _, _) = world_of("#![no_std]\npub fn f() {}");
        let recorder = Recorder::new(RecorderOpts { alloc: a() });
        let tables = Tables::new(TablesOpts { records, impls: None, lang_crates: &[CORE_AT], reads: &recorder });
        let read = TableRead::Names(NameCell::Lang { krate: records.crate_key(CORE_AT).expect("fixture core").prefix() });
        let expected = cell_id(tables, read).expect("core belongs to this scope").expect("core's lang cell");
        let mut memo = ReadIds::new(tables, a());
        assert_eq!(memo.id(read).unwrap(), Some(expected));
        assert_eq!(memo.id(read).unwrap(), Some(expected));
        assert_eq!(memo.counts(), (1, 1));
        for at in 1..=ReadIds::LIMIT {
            memo.id(TableRead::Head { item: (at as u128).to_be_bytes(), part: 0 }).unwrap();
        }
        assert_eq!(memo.ids.len(), 1, "the first miss at capacity resets the table");
        assert_eq!(memo.id(read).unwrap(), Some(expected));
        assert_eq!(memo.counts(), (ReadIds::LIMIT as u64 + 2, 1));
        std::println!("read ID unchanged after 65,536 entries and a reset");
    }

    #[test]
    fn impls_and_root_reads_bypass_and_do_not_evict_the_ids_table() {
        let (records, _, _) = world_of("#![no_std]\npub fn f() {}");
        let recorder = Recorder::new(RecorderOpts { alloc: a() });
        let tables = Tables::new(TablesOpts { records, impls: None, lang_crates: &[CORE_AT], reads: &recorder });
        let Some(core_key) = records.crate_key(CORE_AT) else {
            assert!(false, "fixture core key");
            return;
        };
        let mut krate16 = [0u8; 16];
        krate16.copy_from_slice(&core_key.0.0[..16]);
        let lang_read = TableRead::Names(NameCell::Lang { krate: core_key.prefix() });
        let root_read = TableRead::Names(NameCell::Root { krate: core_key.prefix() });
        let mut memo = ReadIds::new(tables, a());
        assert_eq!(memo.id(lang_read).ok(), cell_id(tables, lang_read).ok());
        assert_eq!(memo.ids.len(), 1);
        assert_eq!(memo.id(root_read).ok(), cell_id(tables, root_read).ok());
        assert_eq!(memo.ids.len(), 1, "NameCell::Root must not populate `ids`");
        for i in 0u32..512 {
            let mut bucket = [0u8; 16];
            bucket[..4].copy_from_slice(&i.to_be_bytes());
            let impl_read = TableRead::Impls { krate: krate16, bucket };
            assert_eq!(memo.id(impl_read).ok(), cell_id(tables, impl_read).ok());
        }
        let absent_impl = TableRead::Impls { krate: [0; 16], bucket: [7; 16] };
        assert_eq!(memo.id(absent_impl).ok(), cell_id(tables, absent_impl).ok());
        assert_eq!(memo.ids.len(), 1, "TableRead::Impls must not populate or evict `ids`");
    }

    #[test]
    fn stretch_store_reused_matches_fresh_forward_and_reversed() {
        use blaze_drive::key::Key;
        use blaze_engine::{Phase, Planned, Stage};
        use super::super::head_tests::cells;

        let src = "\
#![no_std]
pub trait Score {
    type Out;
    fn score(&self) -> Self::Out;
}
pub struct Pair<T> {
    pub left: T,
    pub right: T,
}
impl Score for Pair<u8> {
    type Out = u8;
    fn score(&self) -> u8 {
        self.left
    }
}
pub fn use_score<T: Score>(x: &T) -> T::Out {
    x.score()
}
pub fn base(p: Pair<u8>) -> u8 {
    use_score(&p)
}
pub fn alpha() -> u8 {
    let p = Pair { left: 1u8, right: 2u8 };
    base(p)
}
pub fn beta_with_local() -> u8 {
    fn inner(x: u8) -> u8 {
        x
    }
    let p = Pair { left: inner(3u8), right: 4u8 };
    base(p)
}
pub fn gamma_err() -> u16 {
    let p = Pair { left: 5u8, right: 6u8 };
    base(p)
}";
        let (records, _, body_frame) = world_of(src);
        let unused = Recorder::new(RecorderOpts { alloc: a() });
        let base_tables = Tables::new(TablesOpts { records, impls: None, lang_crates: &[CORE_AT], reads: &unused });
        let base_frame = BodyFrame { frame: body_frame, alloc: a(), plant_drop: None, cost: None, needs: NeedsMet::First, heads: None, sources: None, goals: None, prepared: None, front: None };
        let seed: &'static _ = body_frame.alloc(super::super::seeds::Freezing::new(records, oracle_heads(base_tables, &base_frame)));
        let heads = cells(records);
        let names = ["base", "alpha", "beta_with_local", "gamma_err"];
        let mut tasks: blaze_own::Vec<(&str, Task), Alloc<'static>> = blaze_own::Vec::new_in(a());
        for label in names {
            let Some(key) = heads.iter().map(|want| want.key()).find(|&key| records.name_of(key) == Some(label)) else {
                panic!("missing fixture function {label}");
            };
            tasks.push((
                label,
                Task { planned: Planned { name: Name::new(Kind::BodyCheck, key.0), phase: Phase::new(Stage::Bodies, 0), source: ABSENT, file: Name::new(Kind::FileItems, ABSENT) }, key: Key(ABSENT) },
            ));
        }

        fn check_once<'n>(
            records: &'n super::super::super::source::Records<'n>,
            unused: &Recorder<'_>,
            frame: &BodyFrame<'n>,
            task: &Task,
            scratch: &mut blaze_mem::Frame,
            reuse: Option<&mut StretchStore<'n>>,
        ) -> (ContentId, bool, u32, blaze_own::Vec<TableRead, Alloc<'static>>, blaze_own::Vec<ItemKey, Alloc<'static>>, Option<blaze_own::Vec<u8, Alloc<'static>>>) {
            let tables = Tables::new(TablesOpts { records, impls: None, lang_crates: &[CORE_AT], reads: unused });
            scratch.reset();
            let erased = blaze_own::Erased::new(&*scratch);
            let w = erased.handle();
            let mut rec = Recorder::new(RecorderOpts { alloc: w });
            let mut enc = AString::new_in(w);
            let got = match check_body_item(tables, task, frame, w, &mut rec, None, &mut enc, reuse) {
                Ok(got) => got,
                Err(error) => panic!("fixture body check failed: {error}"),
            };
            let mut reads: blaze_own::Vec<TableRead, Alloc<'static>> = blaze_own::Vec::with_capacity_in(got.reads.len(), a());
            reads.extend_from_slice(got.reads.as_slice());
            reads.sort_unstable();
            let mut locals: blaze_own::Vec<ItemKey, Alloc<'static>> = blaze_own::Vec::with_capacity_in(got.locals.len(), a());
            locals.extend_from_slice(got.locals.as_slice());
            let typed = got.typed.as_ref().map(|b| {
                let mut v = blaze_own::Vec::with_capacity_in(b.len(), a());
                v.extend_from_slice(b.as_slice());
                v
            });
            (got.out, got.failed, got.findings, reads, locals, typed)
        }
        let frame = BodyFrame { frame: body_frame, alloc: a(), plant_drop: None, cost: None, needs: NeedsMet::First, heads: Some(seed), sources: None, goals: None, prepared: None, front: None };

        let Ok(mut scratch) = blaze_mem::Frame::with_reserve(32 << 20, blaze_mem::Tag::Diagnostics) else {
            panic!("test scratch frame reservation failed");
        };
        let mut fresh_results = blaze_own::Vec::new_in(a());
        for &(label, ref task) in tasks.iter() {
            fresh_results.push((label, check_once(records, &unused, &frame, task, &mut scratch, None)));
        }

        // Forward reuse across all 4 bodies (including one with a body-local fn).
        let Ok(mut forward_store) = StretchStore::new(StretchStoreOpts { reserve: 64 << 20, cap: 16 << 20, keep: 1 << 20, alloc: a() }) else {
            panic!("forward StretchStore reservation failed");
        };
        for ((label, task), (_, expected)) in tasks.iter().zip(fresh_results.iter()) {
            let got = check_once(records, &unused, &frame, task, &mut scratch, Some(&mut forward_store));
            assert_eq!(got.0, expected.0, "forward out mismatch on {label}");
            assert_eq!(got.1, expected.1, "forward failed mismatch on {label}");
            assert_eq!(got.2, expected.2, "forward findings mismatch on {label}");
            assert_eq!(got.3, expected.3, "forward reads mismatch on {label}");
            assert_eq!(got.4, expected.4, "forward locals mismatch on {label}");
            assert_eq!(got.5, expected.5, "forward typed mismatch on {label}");
        }
        assert_eq!(forward_store.rollovers(), 0, "surgical body-local cleanup must not taint stretch store");
        assert!(forward_store.used() > 0, "reused store retains persistent heads");

        // Reversed reuse across all 4 bodies.
        let Ok(mut reverse_store) = StretchStore::new(StretchStoreOpts { reserve: 64 << 20, cap: 16 << 20, keep: 1 << 20, alloc: a() }) else {
            panic!("reverse StretchStore reservation failed");
        };
        for ((label, task), (_, expected)) in tasks.iter().rev().zip(fresh_results.iter().rev()) {
            let got = check_once(records, &unused, &frame, task, &mut scratch, Some(&mut reverse_store));
            assert_eq!(got.0, expected.0, "reversed out mismatch on {label}");
            assert_eq!(got.1, expected.1, "reversed failed mismatch on {label}");
            assert_eq!(got.2, expected.2, "reversed findings mismatch on {label}");
            assert_eq!(got.3, expected.3, "reversed reads mismatch on {label}");
            assert_eq!(got.4, expected.4, "reversed locals mismatch on {label}");
            assert_eq!(got.5, expected.5, "reversed typed mismatch on {label}");
        }
        assert_eq!(reverse_store.rollovers(), 0, "reversed reuse must not taint stretch store");

        // Combined StretchStore + GoalTable(ReplayMode::SameScope) in forward and reversed order.
        let goals: &'static _ = body_frame.alloc(super::super::goals::GoalTable::new_in(
            super::super::goals::GoalTableOpts { share: super::super::goals::share_opts_same_scope(), most: super::super::goals::MOST_GOALS },
            blaze_own::testing::heap().shared(),
        ));
        let shared_frame = BodyFrame { goals: Some(goals), ..frame };
        let Ok(mut shared_fwd) = StretchStore::new(StretchStoreOpts { reserve: 64 << 20, cap: 16 << 20, keep: 1 << 20, alloc: a() }) else {
            panic!("shared forward StretchStore reservation failed");
        };
        for ((label, task), (_, expected)) in tasks.iter().zip(fresh_results.iter()) {
            let got = check_once(records, &unused, &shared_frame, task, &mut scratch, Some(&mut shared_fwd));
            assert_eq!(got.0, expected.0, "shared forward out mismatch on {label}");
            assert_eq!(got.1, expected.1, "shared forward failed mismatch on {label}");
            assert_eq!(got.2, expected.2, "shared forward findings mismatch on {label}");
            assert_eq!(got.3, expected.3, "shared forward reads mismatch on {label}");
            assert_eq!(got.4, expected.4, "shared forward locals mismatch on {label}");
            assert_eq!(got.5, expected.5, "shared forward typed mismatch on {label}");
        }
        let Ok(mut shared_rev) = StretchStore::new(StretchStoreOpts { reserve: 64 << 20, cap: 16 << 20, keep: 1 << 20, alloc: a() }) else {
            panic!("shared reverse StretchStore reservation failed");
        };
        for ((label, task), (_, expected)) in tasks.iter().rev().zip(fresh_results.iter().rev()) {
            let got = check_once(records, &unused, &shared_frame, task, &mut scratch, Some(&mut shared_rev));
            assert_eq!(got.0, expected.0, "shared reversed out mismatch on {label}");
            assert_eq!(got.1, expected.1, "shared reversed failed mismatch on {label}");
            assert_eq!(got.2, expected.2, "shared reversed findings mismatch on {label}");
            assert_eq!(got.3, expected.3, "shared reversed reads mismatch on {label}");
            assert_eq!(got.4, expected.4, "shared reversed locals mismatch on {label}");
            assert_eq!(got.5, expected.5, "shared reversed typed mismatch on {label}");
        }
    }

    #[test]
    fn stretch_store_cap_rollover_resets_heads_frame_and_preserves_outputs() {
        use blaze_drive::key::Key;
        use blaze_engine::{Phase, Planned, Stage};
        use super::super::head_tests::cells;

        let src = "#![no_std]\npub fn base() -> u8 { 1u8 }\npub fn one() -> u8 { base() }\npub fn two() -> u8 { one() }";
        let (records, _, body_frame) = world_of(src);
        let unused = Recorder::new(RecorderOpts { alloc: a() });
        let base_tables = Tables::new(TablesOpts { records, impls: None, lang_crates: &[CORE_AT], reads: &unused });
        let base_frame = BodyFrame { frame: body_frame, alloc: a(), plant_drop: None, cost: None, needs: NeedsMet::First, heads: None, sources: None, goals: None, prepared: None, front: None };
        let seed: &'static _ = body_frame.alloc(super::super::seeds::Freezing::new(records, oracle_heads(base_tables, &base_frame)));
        let heads = cells(records);
        fn run_item<'n>(
            records: &'n super::super::super::source::Records<'n>,
            unused: &Recorder<'_>,
            frame: &BodyFrame<'n>,
            task: &Task,
            scratch: &mut blaze_mem::Frame,
            reuse: Option<&mut StretchStore<'n>>,
        ) -> (ContentId, blaze_own::Vec<TableRead, Alloc<'static>>) {
            let tables = Tables::new(TablesOpts { records, impls: None, lang_crates: &[CORE_AT], reads: unused });
            scratch.reset();
            let erased = blaze_own::Erased::new(&*scratch);
            let w = erased.handle();
            let mut rec = Recorder::new(RecorderOpts { alloc: w });
            let mut enc = AString::new_in(w);
            let got = match check_body_item(tables, task, frame, w, &mut rec, None, &mut enc, reuse) {
                Ok(got) => got,
                Err(error) => panic!("check failed: {error}"),
            };
            let mut reads: blaze_own::Vec<TableRead, Alloc<'static>> = blaze_own::Vec::with_capacity_in(got.reads.len(), a());
            reads.extend_from_slice(got.reads.as_slice());
            reads.sort_unstable();
            (got.out, reads)
        }
        let frame = BodyFrame { frame: body_frame, alloc: a(), plant_drop: None, cost: None, needs: NeedsMet::First, heads: Some(seed), sources: None, goals: None, prepared: None, front: None };
        let Ok(mut scratch) = blaze_mem::Frame::with_reserve(32 << 20, blaze_mem::Tag::Diagnostics) else {
            panic!("test scratch frame reservation failed");
        };
        let Ok(mut capped) = StretchStore::new(StretchStoreOpts { reserve: 16 << 20, cap: 1, keep: 4 << 10, alloc: a() }) else {
            panic!("capped StretchStore reservation failed");
        };
        for label in ["base", "one", "two"] {
            let Some(key) = heads.iter().map(|want| want.key()).find(|&key| records.name_of(key) == Some(label)) else {
                panic!("missing fixture function {label}");
            };
            let task = Task { planned: Planned { name: Name::new(Kind::BodyCheck, key.0), phase: Phase::new(Stage::Bodies, 0), source: ABSENT, file: Name::new(Kind::FileItems, ABSENT) }, key: Key(ABSENT) };
            let (fresh_out, fresh_reads) = run_item(records, &unused, &frame, &task, &mut scratch, None);
            let (reused_out, reused_reads) = run_item(records, &unused, &frame, &task, &mut scratch, Some(&mut capped));
            assert_eq!(reused_out, fresh_out, "capped rollover out mismatch on {label}");
            assert_eq!(reused_reads, fresh_reads, "capped rollover reads mismatch on {label}");
            assert_eq!(capped.used(), 0, "heads_frame must be reset after cap rollover on {label}");
        }
        assert_eq!(capped.rollovers(), 3, "each body past 1-byte cap must trigger rollover");
    }

    /// Determinism oracle for stretch-reused [`blaze_typeck::infer::FactCache`] inside
    /// [`StretchStore`] (rule 25, rule 78): exercises supertraits, associated types, concrete and
    /// blanket impls, ADTs, fn sigs, lang items, body-local (`INNER`) structs/impls, opaque return
    /// types, and diagnostics under BOTH `impls: None` AND a real `TableSet` (`impls: Some(...)`),
    /// proving 100% identical outputs, findings, locals, typed records, and sorted read lists in
    /// forward and reversed execution orders (also exercising `SharedBindingKeys`).
    #[test]
    fn stretch_store_fact_cache_reuse_determinism_oracle() {
        use blaze_drive::key::Key;
        use blaze_engine::{Phase, Planned, Stage};
        use super::super::head_tests::cells;
        use super::super::recorder::SharedBindingKeys;

        let src = "\
#![no_std]
pub trait Base {
    fn base_val(&self) -> u8;
}
pub trait Sub: Base {
    type Out;
    fn sub_val(&self) -> Self::Out;
}
pub struct Wrap<T> {
    pub inner: T,
}
pub enum Choice {
    Left(u8),
    Right(u8),
}
impl Base for u8 {
    fn base_val(&self) -> u8 {
        *self
    }
}
impl<T: Base> Base for Wrap<T> {
    fn base_val(&self) -> u8 {
        self.inner.base_val()
    }
}
impl<T: Base> Sub for Wrap<T> {
    type Out = u8;
    fn sub_val(&self) -> u8 {
        self.base_val()
    }
}
pub fn via_sub<T: Sub>(x: &T) -> T::Out {
    x.sub_val()
}
pub fn mk_opaque(v: u8) -> impl Base {
    Wrap { inner: v }
}
pub fn body_wrap() -> u8 {
    let w = Wrap { inner: 7u8 };
    via_sub(&w)
}
pub fn body_enum(c: Choice) -> u8 {
    let v = match c {
        Choice::Left(a) => a,
        Choice::Right(b) => b,
    };
    let w = Wrap { inner: v };
    via_sub(&w)
}
pub fn body_with_inner() -> u8 {
    struct Local(u8);
    impl Base for Local {
        fn base_val(&self) -> u8 {
            self.0
        }
    }
    let w = Wrap { inner: Local(9u8) };
    via_sub(&w)
}
pub fn body_opaque() -> u8 {
    let o = mk_opaque(4u8);
    o.base_val()
}
pub fn body_diag() -> u16 {
    let w = Wrap { inner: 1u8 };
    via_sub(&w)
}";
        let (records, _, body_frame) = world_of(src);
        let unused = Recorder::new(RecorderOpts { alloc: a() });
        let base_tables = Tables::new(TablesOpts { records, impls: None, lang_crates: &[CORE_AT], reads: &unused });
        let base_frame = BodyFrame { frame: body_frame, alloc: a(), plant_drop: None, cost: None, needs: NeedsMet::First, heads: None, sources: None, goals: None, prepared: None, front: None };
        let seed: &'static _ = body_frame.alloc(super::super::seeds::Freezing::new(records, oracle_heads(base_tables, &base_frame)));
        let mut table_set = crate::front2::buckets::TableSet::new(a());
        for krate in 0..records.crate_count() {
            let mut rows = blaze_own::Vec::new_in(a());
            if NamesSource::each_impl_row(records, krate, &mut |r| rows.push(r)) {
                let written = crate::front2::buckets::write_impl_tables(records, krate, &rows, a()).expect("fixture impl tables");
                table_set.put(written);
            }
        }
        let table_set: &'static crate::front2::buckets::TableSet<'static> = body_frame.alloc(table_set);
        let heads = cells(records);
        let names = ["via_sub", "mk_opaque", "body_wrap", "body_enum", "body_with_inner", "body_opaque", "body_diag"];
        let mut tasks: blaze_own::Vec<(&str, Task), Alloc<'static>> = blaze_own::Vec::new_in(a());
        for label in names {
            let Some(key) = heads.iter().map(|want| want.key()).find(|&key| records.name_of(key) == Some(label)) else {
                panic!("missing fixture function {label}");
            };
            tasks.push((
                label,
                Task { planned: Planned { name: Name::new(Kind::BodyCheck, key.0), phase: Phase::new(Stage::Bodies, 0), source: ABSENT, file: Name::new(Kind::FileItems, ABSENT) }, key: Key(ABSENT) },
            ));
        }

        #[allow(clippy::type_complexity)]
        fn run_one<'n>(
            records: &'n super::super::super::source::Records<'n>,
            impls: Option<&'n crate::front2::buckets::TableSet<'n>>,
            unused: &Recorder<'_>,
            frame: &BodyFrame<'n>,
            task: &Task,
            scratch: &mut blaze_mem::Frame,
            reuse: Option<&mut StretchStore<'n>>,
            shared_bindings: Option<&SharedBindingKeys>,
        ) -> (ContentId, bool, u32, blaze_own::Vec<TableRead, Alloc<'static>>, blaze_own::Vec<ItemKey, Alloc<'static>>, Option<blaze_own::Vec<u8, Alloc<'static>>>) {
            let tables = Tables::new(TablesOpts { records, impls, lang_crates: &[CORE_AT], reads: unused });
            scratch.reset();
            let erased = blaze_own::Erased::new(&*scratch);
            let w = erased.handle();
            let mut rec = match shared_bindings {
                Some(shared) => Recorder::with_shared_bindings(RecorderOpts { alloc: w }, shared),
                None => Recorder::new(RecorderOpts { alloc: w }),
            };
            let mut enc = AString::new_in(w);
            let got = match check_body_item(tables, task, frame, w, &mut rec, None, &mut enc, reuse) {
                Ok(got) => got,
                Err(error) => panic!("fixture body check failed: {error}"),
            };
            let mut reads: blaze_own::Vec<TableRead, Alloc<'static>> = blaze_own::Vec::with_capacity_in(got.reads.len(), a());
            reads.extend_from_slice(got.reads.as_slice());
            reads.sort_unstable();
            let mut locals: blaze_own::Vec<ItemKey, Alloc<'static>> = blaze_own::Vec::with_capacity_in(got.locals.len(), a());
            locals.extend_from_slice(got.locals.as_slice());
            let typed = got.typed.as_ref().map(|b| {
                let mut v = blaze_own::Vec::with_capacity_in(b.len(), a());
                v.extend_from_slice(b.as_slice());
                v
            });
            (got.out, got.failed, got.findings, reads, locals, typed)
        }

        let frame = BodyFrame { frame: body_frame, alloc: a(), plant_drop: None, cost: None, needs: NeedsMet::First, heads: Some(seed), sources: None, goals: None, prepared: None, front: None };
        let Ok(mut scratch) = blaze_mem::Frame::with_reserve(32 << 20, blaze_mem::Tag::Diagnostics) else {
            panic!("test scratch frame reservation failed");
        };

        fn assert_reads_sound_and_equal(
            got: &[TableRead],
            expected: &[TableRead],
            context: &str,
        ) {
            // Every read recorded under reuse must also be in the fresh oracle (no cross-body
            // read leakage).
            for r in got {
                assert!(
                    expected.binary_search(r).is_ok(),
                    "{context}: unexpected extra read under StretchStore+FactCache reuse: {r:?}"
                );
            }
            // Every read in the fresh oracle must be present under reuse, except a cold-miss
            // `Names(Item { key })` probe whose strictly stronger `Head { item: key, .. }` read
            // is already present (StretchStore holds the already-imported head so `detail(lib)`
            // does not need to probe `Names` before `want`).
            for r in expected {
                if got.binary_search(r).is_ok() {
                    continue;
                }
                let covered_by_head = match r {
                    TableRead::Names(blaze_scope::names_cells::NameCell::Item { key })
                    | TableRead::Names(blaze_scope::names_cells::NameCell::Members { owner: key })
                    | TableRead::Text { key } => {
                        let bytes = crate::front2::keys::checker_of(*key).0.to_be_bytes();
                        got.iter().any(|g| matches!(g, TableRead::Head { item, .. } if *item == bytes))
                    }
                    _ => false,
                };
                assert!(
                    covered_by_head,
                    "{context}: missing read under StretchStore+FactCache reuse (not covered by Head): {r:?}"
                );
            }
        }

        for (mode, impls) in [("unindexed", None), ("table_set", Some(table_set))] {
            let mut fresh = blaze_own::Vec::new_in(a());
            for &(label, ref task) in tasks.iter() {
                fresh.push((label, run_one(records, impls, &unused, &frame, task, &mut scratch, None, None)));
            }
            let shared_fwd_bindings = SharedBindingKeys::new(a());
            let Ok(mut fwd_store) = StretchStore::new(StretchStoreOpts { reserve: 64 << 20, cap: 16 << 20, keep: 1 << 20, alloc: a() }) else {
                panic!("forward StretchStore reservation failed");
            };
            for ((label, task), (_, expected)) in tasks.iter().zip(fresh.iter()) {
                let got = run_one(records, impls, &unused, &frame, task, &mut scratch, Some(&mut fwd_store), Some(&shared_fwd_bindings));
                assert_eq!(got.0, expected.0, "{mode} forward out mismatch on {label}");
                assert_eq!(got.1, expected.1, "{mode} forward failed mismatch on {label}");
                assert_eq!(got.2, expected.2, "{mode} forward findings mismatch on {label}");
                assert_reads_sound_and_equal(&got.3, &expected.3, label);
                assert_eq!(got.4, expected.4, "{mode} forward locals mismatch on {label}");
                assert_eq!(got.5, expected.5, "{mode} forward typed mismatch on {label}");
            }
            assert_eq!(fwd_store.rollovers(), 0, "{mode} forward reuse must not rollover");

            let shared_rev_bindings = SharedBindingKeys::new(a());
            let Ok(mut rev_store) = StretchStore::new(StretchStoreOpts { reserve: 64 << 20, cap: 16 << 20, keep: 1 << 20, alloc: a() }) else {
                panic!("reverse StretchStore reservation failed");
            };
            for ((label, task), (_, expected)) in tasks.iter().rev().zip(fresh.iter().rev()) {
                let got = run_one(records, impls, &unused, &frame, task, &mut scratch, Some(&mut rev_store), Some(&shared_rev_bindings));
                assert_eq!(got.0, expected.0, "{mode} reversed out mismatch on {label}");
                assert_eq!(got.1, expected.1, "{mode} reversed failed mismatch on {label}");
                assert_eq!(got.2, expected.2, "{mode} reversed findings mismatch on {label}");
                assert_reads_sound_and_equal(&got.3, &expected.3, label);
                assert_eq!(got.4, expected.4, "{mode} reversed locals mismatch on {label}");
                assert_eq!(got.5, expected.5, "{mode} reversed typed mismatch on {label}");
            }
            assert_eq!(rev_store.rollovers(), 0, "{mode} reversed reuse must not rollover");
        }
    }

    /// Rule 25 & Rule 26: `BSF1` round-trip of a real [`StretchFront`] equals fresh in `IndexIn`,
    /// `Pointers`, `TableRead`s, re-encoded bytes, and live body check verdict + read list.
    #[test]
    fn stretch_front_bsf1_round_trip_matches_fresh() {
        use blaze_drive::key::Key;
        use blaze_engine::{Phase, Planned, Stage};
        use super::super::head_tests::cells;

        let src = "\
#![no_std]
pub trait Show {
    fn val(&self) -> u8;
}
impl Show for u8 {
    fn val(&self) -> u8 { *self }
}
pub fn entry(x: u8) -> u8 {
    x.val()
}";
        let (records, _, body_frame) = world_of(src);
        let unused = Recorder::new(RecorderOpts { alloc: a() });
        let tables = Tables::new(TablesOpts { records, impls: None, lang_crates: &[0, CORE_AT], reads: &unused });
        let shared = blaze_own::testing::heap().shared();

        let fresh = StretchFront::new(tables, a(), shared).expect("fresh StretchFront");
        let mut encoded = blaze_own::Vec::new_in(a());
        fresh.encode(&mut encoded).expect("encode BSF1");
        assert!(encoded.starts_with(STRETCH_FRONT_MAGIC));

        let decoded = StretchFront::decode(&encoded, shared).expect("decode BSF1");
        assert_eq!(decoded.borrow_index().scope(), fresh.borrow_index().scope());
        assert_eq!(decoded.borrow_index().lang_count(), fresh.borrow_index().lang_count());
        assert_eq!(decoded.langs().len(), fresh.langs().len());
        for (name, key) in fresh.langs().iter() {
            assert_eq!(decoded.borrow_index().lang(name.as_str()), Some(*key), "lang mismatch for {}", name.as_str());
            assert_eq!(decoded.borrow_index().lang_name_of(*key), fresh.borrow_index().lang_name_of(*key), "reverse lang mismatch for {}", name.as_str());
        }
        assert_eq!(decoded.pointers(), fresh.pointers());
        assert_eq!(decoded.reads(), fresh.reads());

        let mut reencoded = blaze_own::Vec::new_in(a());
        decoded.encode(&mut reencoded).expect("re-encode BSF1");
        assert_eq!(reencoded.as_slice(), encoded.as_slice(), "BSF1 encoding must be deterministic");

        // Verify live body checking produces identical verdict and reads across None / fresh / decoded.
        let heads = cells(records);
        let key = heads.iter().map(|want| want.key()).find(|&k| records.name_of(k) == Some("entry")).expect("entry fn");
        let task = Task {
            planned: Planned { name: Name::new(Kind::BodyCheck, key.0), phase: Phase::new(Stage::Bodies, 0), source: ABSENT, file: Name::new(Kind::FileItems, ABSENT) },
            key: Key(ABSENT),
        };
        let fresh_ref: &'static StretchFront = body_frame.alloc(fresh);
        let decoded_ref: &'static StretchFront = body_frame.alloc(decoded);
        let mut results = blaze_own::Vec::with_capacity_in(3, a());
        for front in [None, Some(fresh_ref), Some(decoded_ref)] {
            let bf = BodyFrame { frame: body_frame, alloc: a(), plant_drop: None, cost: None, needs: NeedsMet::First, heads: None, sources: None, goals: None, prepared: None, front };
            let mut rec = Recorder::new(RecorderOpts { alloc: a() });
            let mut enc = AString::new_in(a());
            let got = check_body_item(tables, &task, &bf, a(), &mut rec, None, &mut enc, None).expect("check entry");
            let mut reads = blaze_own::Vec::with_capacity_in(got.reads.len(), a());
            reads.extend_from_slice(got.reads.as_slice());
            reads.sort_unstable();
            results.push((got.out, got.failed, got.findings, reads));
        }
        assert_eq!(results[2], results[1], "decoded StretchFront must match fresh StretchFront");
        assert_eq!(results[2], results[0], "decoded StretchFront must match uncached check");
    }

    /// Rule 3 & Rule 26: previous layout magic `BSF0` is refused with a typed `Magic` error.
    #[test]
    fn previous_stretch_front_magic_bsf0_is_rejected() {
        let shared = blaze_own::testing::heap().shared();
        let mut bytes = blaze_own::Vec::new_in(a());
        bytes.extend_from_slice(b"BSF0");
        // Minimal valid payload tail (empty scope, empty langs, 4 absent pointers, empty reads).
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 4]);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        let err = StretchFront::decode(&bytes, shared).err().expect("BSF0 must be rejected");
        assert_eq!(err, StretchFrontError::Magic { found: *b"BSF0" });
    }

    /// Rule 3 & Rule 25: exhaustive coverage of all `Pointer` and `TableRead`/`NameCell` variants,
    /// plus rejection of truncated, trailing, unsorted, invalid UTF-8, and bad-tag payloads.
    #[test]
    fn stretch_front_bsf1_all_variants_and_malformed_rejection() {
        use blaze_scope::cell_key::CellKey;
        use blaze_scope::names_cells::Section;
        use super::super::super::receiver::{Pointer, Pointers};

        let shared = blaze_own::testing::heap().shared();
        let k = |b: u8| ItemKey(ContentId([b; 32]));
        let c8 = |b: u8| [b; 8];
        let c16 = |b: u8| [b; 16];

        let mut all_reads = blaze_own::Vec::new_in(a());
        all_reads.extend_from_slice(&[
            TableRead::Names(NameCell::Binding { module: k(1), name: CellKey(c16(2)) }),
            TableRead::Names(NameCell::Module { module: k(3) }),
            TableRead::Names(NameCell::Root { krate: c8(4) }),
            TableRead::Names(NameCell::Variants { owner: k(5) }),
            TableRead::Names(NameCell::Item { key: k(6) }),
            TableRead::Names(NameCell::Members { owner: k(7) }),
            TableRead::Names(NameCell::Expansion { call: k(8) }),
            TableRead::Names(NameCell::MemberOf { member: k(9) }),
            TableRead::Names(NameCell::VariantOf { variant: k(10) }),
            TableRead::Names(NameCell::Beginning { first: c16(11) }),
            TableRead::Names(NameCell::Lang { krate: c8(12) }),
            TableRead::Names(NameCell::Incoherent { krate: c8(13) }),
            TableRead::Names(NameCell::InternalSymbols { krate: c8(14) }),
            TableRead::Names(NameCell::Links { krate: c8(15) }),
            TableRead::Names(NameCell::ImplRows { krate: c8(16) }),
            TableRead::Names(NameCell::Files { krate: c8(17) }),
            TableRead::Names(NameCell::Extensions { krate: c8(18) }),
            TableRead::Names(NameCell::CfgAtoms { krate: c8(19) }),
            TableRead::Names(NameCell::Textual { module: k(20) }),
            TableRead::Names(NameCell::Whole { krate: c8(21), section: Section::Names }),
            TableRead::Names(NameCell::Whole { krate: c8(22), section: Section::Items }),
            TableRead::Names(NameCell::Whole { krate: c8(23), section: Section::Expansions }),
            TableRead::Signature { key: k(24) },
            TableRead::Text { key: k(25) },
            TableRead::Head { item: c16(26), part: 7 },
            TableRead::Impls { krate: c16(27), bucket: c16(28) },
        ]);
        all_reads.sort_unstable();
        all_reads.dedup();

        // Build a synthetic BSF1 payload covering all Pointer::Unknown reasons & all TableRead tags.
        let mut raw = blaze_own::Vec::new_in(a());
        raw.extend_from_slice(STRETCH_FRONT_MAGIC);
        raw.extend_from_slice(&2u32.to_le_bytes()); // scope = [0, 3]
        raw.extend_from_slice(&0u32.to_le_bytes());
        raw.extend_from_slice(&3u32.to_le_bytes());
        raw.extend_from_slice(&2u32.to_le_bytes()); // langs = [("owned_box", k(30)), ("sized", k(31))]
        for (name, key) in [("owned_box", k(30)), ("sized", k(31))] {
            let name_len = u32::try_from(name.len()).expect("lang name len fits u32");
            raw.extend_from_slice(&name_len.to_le_bytes());
            raw.extend_from_slice(name.as_bytes());
            raw.extend_from_slice(&key.0.0);
        }
        // Pointers: At, Absent, Unknown(0), Unknown(3)
        let pointers = Pointers {
            boxed: Pointer::At(k(30)),
            rc: Pointer::Absent,
            arc: Pointer::Unknown(UNKNOWN_POINTER_REASONS[1]),
            pin: Pointer::Unknown(UNKNOWN_POINTER_REASONS[3]),
        };
        encode_pointer(pointers.boxed, &mut raw).expect("encode boxed");
        encode_pointer(pointers.rc, &mut raw).expect("encode rc");
        encode_pointer(pointers.arc, &mut raw).expect("encode arc");
        encode_pointer(pointers.pin, &mut raw).expect("encode pin");
        let reads_len = u32::try_from(all_reads.len()).expect("reads len fits u32");
        raw.extend_from_slice(&reads_len.to_le_bytes());
        for &read in all_reads.iter() {
            encode_table_read(read, &mut raw);
        }

        let decoded = StretchFront::decode(&raw, shared).expect("decode synthetic BSF1");
        assert_eq!(decoded.borrow_index().scope(), &[0, 3]);
        assert_eq!(decoded.borrow_index().lang("owned_box"), Some(k(30)));
        assert_eq!(decoded.borrow_index().lang("sized"), Some(k(31)));
        assert_eq!(decoded.pointers(), pointers);
        assert_eq!(decoded.reads(), all_reads.as_slice());

        let mut reencoded = blaze_own::Vec::new_in(a());
        decoded.encode(&mut reencoded).expect("re-encode synthetic BSF1");
        assert_eq!(reencoded.as_slice(), raw.as_slice());

        // Truncated input rejected.
        assert!(matches!(StretchFront::decode(&raw[..raw.len() - 1], shared), Err(StretchFrontError::Short { .. })));
        assert!(matches!(StretchFront::decode(&raw[..3], shared), Err(StretchFrontError::Short { .. })));

        // Trailing bytes rejected.
        let mut trailing = blaze_own::Vec::new_in(a());
        trailing.extend_from_slice(&raw);
        trailing.push(0);
        assert_eq!(StretchFront::decode(&trailing, shared).err(), Some(StretchFrontError::Trailing { extra: 1 }));

        // Unsorted scope rejected.
        let mut bad_scope = blaze_own::Vec::new_in(a());
        bad_scope.extend_from_slice(STRETCH_FRONT_MAGIC);
        bad_scope.extend_from_slice(&2u32.to_le_bytes());
        bad_scope.extend_from_slice(&3u32.to_le_bytes());
        bad_scope.extend_from_slice(&1u32.to_le_bytes());
        assert_eq!(StretchFront::decode(&bad_scope, shared).err(), Some(StretchFrontError::UnsortedScope { at: 1 }));

        // Non-empty langs with empty scope rejected.
        let mut empty_scope = blaze_own::Vec::new_in(a());
        empty_scope.extend_from_slice(STRETCH_FRONT_MAGIC);
        empty_scope.extend_from_slice(&0u32.to_le_bytes());
        empty_scope.extend_from_slice(&1u32.to_le_bytes());
        assert_eq!(StretchFront::decode(&empty_scope, shared).err(), Some(StretchFrontError::ScopeEmptyWithLangs { langs: 1 }));

        // Unknown pointer tag rejected.
        let mut bad_ptr = blaze_own::Vec::new_in(a());
        bad_ptr.extend_from_slice(STRETCH_FRONT_MAGIC);
        bad_ptr.extend_from_slice(&0u32.to_le_bytes());
        bad_ptr.extend_from_slice(&0u32.to_le_bytes());
        bad_ptr.extend_from_slice(&[99u8, 0, 0, 0]);
        bad_ptr.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(StretchFront::decode(&bad_ptr, shared).err(), Some(StretchFrontError::PointerTag { tag: 99 }));
    }
}

/// D197 root 1, the `Typed` kind: a body whose types are clean and whose borrows were refused
/// (D151 `Typed`, which runs) keeps its checked record, so a run reads it instead of checking
/// it again. The fixture is borrows.rs `region_record_tests`'s shape: `calls_soft_refused`
/// calls `m`, whose parameter is a type macro, so its borrows are refused `RegionsUnread`
/// while its types are whole.
#[cfg(test)]
mod keeps_typed_tests {
    use super::keeps_typed;
    use crate::crates::{DirShelf, Inputs, Toolchain, UserCrate};
    use crate::front2::borrows::BorrowRefused;
    use crate::front2::project::{BodyRef, ProjectWorld};
    use crate::refusal::SourceFile;

    const SOURCE: &str = r#"
macro_rules! byte {
    () => {
        u8
    };
}

fn m(x: &u8, _y: byte!()) -> &u8 {
    x
}

fn calls_soft_refused() {
    let a = 1u8;
    let r = m(&a, 2);
    let _ = *r;
}

fn plain() -> u8 {
    let a = 1u8;
    a
}

fn mistyped() -> u8 {
    true
}

fn main() {}
"#;

    /// The cause (rule 23): the old filter, which kept a record only when the borrows were
    /// checked clean too. CONTROL: under it the `Typed` body keeps none, which is what sent
    /// every such body to the run's second check. TWIN: a body with a type error keeps none
    /// under either (its columns are not clean, so there is no record to keep).
    ///
    /// Prints: `keeps_typed: calls_soft_refused kept (borrows refused), plain kept, mistyped none`
    #[test]
    fn a_body_whose_borrows_were_refused_keeps_its_typed_record() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let home = std::env::var("HOME").expect("HOME is set");
        let tc = Toolchain::open(&mut DirShelf::new(std::format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
        let files = [("/kt/src/main.rs".to_string(), SOURCE.to_string())];
        let edition = edition::PackageEdition::of_manifest("2024").expect("Rust 2024");
        let user = UserCrate { name: "kt", dir: "/kt/", root: "/kt/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
        let erased = blaze_own::Erased::new(&frame);
        let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, erased.bump()).expect("fixture names");
        let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
        let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front2");
        let bodies = world.bodies(alloc);
        let body = |name: &str| {
            bodies
                .iter()
                .find_map(|&(body, _)| {
                    let BodyRef::Free(key) = body else { return None };
                    (names.item_text(key).and_then(|item| item.name) == Some(name)).then_some(body)
                })
                .unwrap_or_else(|| panic!("missing fixture body {name}"))
        };
        let old = |types: &crate::front2::project::BodyVerdict<'_>, borrows: &crate::front2::borrows::BorrowVerdict<'_>| types.is_clean() && borrows.findings.is_empty() && borrows.refused.is_none();

        let (types, borrows) = world.check_borrows(body("calls_soft_refused"), alloc);
        assert!(types.is_clean(), "calls_soft_refused: types whole: {types:?}");
        assert!(matches!(borrows.refused, Some(ref r) if !matches!(r, BorrowRefused::Untyped(_))), "calls_soft_refused: borrows refused by name (D151 `Typed`): {borrows:?}");
        assert!(matches!(types.typed, Some(Ok(_))), "calls_soft_refused: the check encoded its record");
        assert!(keeps_typed(&types), "calls_soft_refused: a `Typed` body keeps its record");
        assert!(!old(&types, &borrows), "CONTROL: the old filter kept none for it");

        let (types, borrows) = world.check_borrows(body("plain"), alloc);
        assert!(matches!(types.typed, Some(Ok(_))) && keeps_typed(&types) && old(&types, &borrows), "plain: a clean body keeps its record under both: {types:?} {borrows:?}");

        let (types, borrows) = world.check_borrows(body("mistyped"), alloc);
        assert!(!keeps_typed(&types) && !old(&types, &borrows), "TWIN: a type error keeps no record: {types:?}");
        assert!(types.typed.is_none(), "TWIN: no record was encoded for a type error");
        std::println!("keeps_typed: calls_soft_refused kept (borrows refused), plain kept, mistyped none");
    }
}
