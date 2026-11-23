//! Names per group (engine.md "Tables": `Names` per module, a glob or macro cycle one `Group`;
//! piece E4): what a crate's cold solve WRITES, one record per strongly connected component of
//! the modules' reads as the solve found them. The solve stays one pass per crate (the cell
//! worklist with its Tarjan condensation); the groups are its output, recorded, so the next
//! request's plan names groups from the last answer and a re-solve confirms or re-cuts them.
//!
//! A module's READS are found per node, order-free: with every node decided, each cell's rule
//! ([`Cells::cell_value`]) and each import's and call's path ([`Cells::try_item`]) is run once
//! more over the decided values with the read log armed, so what it logs is its DIRECT reads,
//! whichever node the solve decided first (a decision's log would hold the reads of the nodes it
//! decided on the way, which depend on the order). The reads of a module are its nodes' reads.
//!
//! What a group record holds, and the cell each read is:
//!
//! - every cell of another group or crate one of its nodes read: `(table, cell, layer)` with
//!   what it held ([`GroupSeen`], the decided value's digest, decide.rs `digest`): the names
//!   half of a reader's cells ([`crate::names_cells`]);
//! - the textual scope of `macro_rules!`: a call's one-segment path and a `use` in the macro
//!   namespace walk the text backward and out through each `mod` line ([`Cells::textual`]).
//!   Each module whose text (its own or an expansion's) the walk read is a [`GroupRead::Textual`]
//!   cell: the digest of that module's textual sequence, its `macro_rules!`, `mod` lines and
//!   calls in text order with no position ([`textual_digest`]). A `fn` added moves no textual
//!   cell; a `macro_rules!` added moves its module's, and only a group whose call walked that
//!   text reads it. OPEN (the finer cut): the cell "macros in scope at this `mod` line", the
//!   parent's sequence up to the line, so a macro added BELOW a `mod` line moves nothing there;
//! - each member call's expansion ([`GroupRead::Expansion`], its made text by digest): a
//!   `macro_rules!` is named by its place, not its body, so the textual cell does not hold what
//!   a call made, and the expansion's own record is keyed by the definition's text;
//! - each member's finished scope (`ModuleScope` bytes).
//!
//! A group's key is its members' own text (each module's outline rows, a child `mod` by its
//! head alone: names_cells.rs `module_head`) and the reads above; the record is valid by both.
//!
//! Read by every group and so held by none: the crate's module tree (the `mod` lines, the
//! chains `self`, `super` and `crate` walk; its own record, plannable from the outlines) and its
//! root record (root attributes and tools, the extern and injected preludes, the merged
//! macro-written `extern crate`s and `#[macro_export]`s: the `Root` cell). A dependency's cells
//! are rows like any other (its module keys are its own); the derive helpers a call reads are
//! a dependency's (only a proc-macro crate defines a derive, and none uses its own).
//!
//! The record: `BZGR`, form, member count, read count; each member's key; each read; each
//! member's scope as (length, zeros to a 16-byte offset, bytes) in member order
//! ([`NameGroup::scope_of`] reads one in place). Sorted throughout: two solves of one text
//! write equal bytes (rule 25).

#![blaze::sans_io]

use blaze_hash::HashMap;
use blaze_id::{ContentId, ItemKey};
use blaze_own::{AVec, Alloc};

use super::decide::{Seen, digest};
use super::{Cell, CellId, Cells, FragId, Hold, ItemId, Layer, ModId, NAMESPACES, Node, Skip, Slot, Step, Value};
use crate::cell_key::{CellKey, cell_key};
use crate::names_cells::NameCell;
use crate::scope::Resolved;
use crate::Namespace;

/// A decided value as a group read records it.
fn group_seen(v: Option<Value>) -> GroupSeen {
    match v.map(digest) {
        None | Some(Seen::Absent) => GroupSeen::Absent,
        Some(Seen::Speculated) => GroupSeen::Speculated,
        Some(Seen::Digest(d)) => GroupSeen::Digest(d),
    }
}

const MAGIC: [u8; 4] = *b"BZGR";
const FORM: u32 = 4;
/// Where each member's scope starts in a record: a multiple of this from the record's start.
const SCOPE_ALIGN: usize = 16;
const TEXTUAL_DOMAIN: &str = "blaze v2 names group textual 1";
const GROUP_DOMAIN: &str = "blaze v2 names group record 1";
const EXPANSION_DOMAIN: &str = "blaze v2 names group expansion 1";

/// What a group's read of a cell saw.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum GroupSeen {
    /// Not found, or not decided.
    Absent,
    /// Its module had calls not grown when the cell was read (a dependency's never has).
    Speculated,
    /// A decided value, by its digest.
    Digest(ContentId),
}

/// One read a group's scopes are valid by: the cell, with what it held.
///
/// A lookup of a name in a module of another group reads that module's BINDING cell of the
/// name (what its own text binds it to: an item, a `mod`, a named import; `Absent` for none);
/// when that is absent, also the module's GLOBS cell and, through each glob's source, the same
/// name there, and so on (a lookup that finds nothing reads each cell it passed as absent). So
/// every cell a record reads is one a group defines as a function of that group ALONE
/// ([`GroupShape::defines`]): no record depends on which other modules asked it what.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum GroupRead {
    /// `table`'s binding cell of `name` (another group's module or a dependency's).
    Binding { table: ItemKey, name: CellKey, seen: GroupSeen },
    /// `table`'s globs cell ([`globs_cell`]), by its digest.
    Globs { table: ItemKey, seen: ContentId },
    /// The textual sequence of `module`'s texts, by [`textual_digest`].
    Textual { module: ItemKey, seen: ContentId },
    /// The expansion of a member's macro call (names_cells.rs `NameCell::Expansion`): its made
    /// text, by digest, or `Absent` when it made none (refused, or wrote nothing). A
    /// `macro_rules!` is named by its place, not its body, so no cell above holds what a call
    /// expanded to; the expansion's own record is keyed by the definition's text.
    Expansion { call: ItemKey, seen: GroupSeen },
}

/// One group of a crate's modules: its members, its reads outside it, and its record.
#[derive(Clone, Debug)]
pub struct NameGroup<'a> {
    /// Sorted.
    pub members: AVec<'a, ItemKey>,
    /// Sorted, each once.
    pub reads: AVec<'a, GroupRead>,
    /// Each member's own-text digest, in member order ([`GroupShape::own`]).
    pub own: AVec<'a, ContentId>,
    /// The engine's `Done::defines`: sorted `(cell, digest)`.
    pub defines: AVec<'a, ([u8; 16], ContentId)>,
    /// The engine's `Done::reads`: sorted `(table, cell, digest)`; the table is the `of` the
    /// engine names by the module's (or the call's) task.
    pub cells_read: AVec<'a, (ItemKey, [u8; 16], ContentId)>,
    pub bytes: AVec<'a, u8>,
    /// The record's identity: the digest of `bytes`.
    pub id: ContentId,
}

/// A group as the solve found it: its members and its reads outside it, both sorted.
#[derive(Clone, Debug)]
pub struct GroupShape<'a> {
    pub members: AVec<'a, ItemKey>,
    pub reads: AVec<'a, GroupRead>,
    /// The members' grown macro calls, by key, sorted: each one's expansion is a read
    /// ([`GroupRead::Expansion`]), filled by [`write_groups`].
    pub calls: AVec<'a, ItemKey>,
    /// Each member's own text, in member order: the digest of its own fragment's
    /// `ModuleDecls` record (a child `mod` by its line alone), what the solve read of its text.
    /// From text before the work: the key's other half.
    pub own: AVec<'a, ContentId>,
    /// The cells the group decides, in the engine's `Done::defines` form, a function of the
    /// group ALONE (never of which readers asked it what: rules 25, 26): for each member, every
    /// binding its own text makes (each name it declares or imports by name, in each namespace
    /// that binds it, by its [`NameCell::Binding`] key), ONE globs cell ([`globs_cell`]: its glob
    /// imports' sources and visibilities) and ONE textual cell ([`textual_cell`]), each with its
    /// digest; sorted. Every cell a group's `cells_read` names in this crate is one of these or
    /// absent, so an edit's moved cells are found from the records alone (edit.rs).
    pub defines: AVec<'a, ([u8; 16], ContentId)>,
}

/// The digest a read of a cell with nothing there saw (the engine's `key::ABSENT`).
pub const ABSENT: ContentId = ContentId([0; 32]);

const OWN_DOMAIN: &str = "blaze v2 names group own text 1";
const GLOBS_CELL_DOMAIN: &str = "blaze v2 names group globs cell 2";
const GLOBS_DOMAIN: &str = "blaze v2 names group globs 1";
const TEXTUAL_CELL_DOMAIN: &str = "blaze v2 names group textual cell 1";

/// A member's own-text digest from its `ModuleDecls` record bytes.
pub fn own_digest(decls: &[u8]) -> ContentId {
    ContentId::of(OWN_DOMAIN, decls)
}

impl GroupRead {
    /// The read in the engine's `Done::reads` form: the table it is of (a module, or a call for
    /// an expansion), its 16-byte cell and the digest it saw ([`ABSENT`] for nothing). A
    /// `Speculated` read is refused: a finished group never reads a module still growing.
    pub fn engine(self) -> Result<(ItemKey, [u8; 16], ContentId), ItemKey> {
        let seen = |s: GroupSeen, of: ItemKey| match s {
            GroupSeen::Absent => Ok(ABSENT),
            GroupSeen::Digest(d) => Ok(d),
            GroupSeen::Speculated => Err(of),
        };
        Ok(match self {
            GroupRead::Binding { table, name, seen: s } => (table, NameCell::Binding { module: table, name }.key(), seen(s, table)?),
            GroupRead::Globs { table, seen: s } => (table, globs_cell(table), s),
            GroupRead::Textual { module, seen: s } => (module, textual_cell(module), s),
            GroupRead::Expansion { call, seen: s } => (call, NameCell::Expansion { call }.key(), seen(s, call)?),
        })
    }
}

fn cut(id: ContentId) -> [u8; 16] {
    let mut k = [0u8; 16];
    k.copy_from_slice(&id.0[..16]);
    k
}

/// The engine's cell of `module`'s glob imports: their sources and visibilities, one cell for
/// every name (what a glob brings is the source's binding of the name, read there).
pub fn globs_cell(module: ItemKey) -> [u8; 16] {
    cut(ContentId::of(GLOBS_CELL_DOMAIN, &module.0.0))
}

/// The engine's cell of `module`'s textual sequence ([`GroupRead::Textual`]).
pub fn textual_cell(module: ItemKey) -> [u8; 16] {
    cut(ContentId::of(TEXTUAL_CELL_DOMAIN, &module.0.0))
}

/// Each group's record over the crate's final scopes `done` (the published exports and the
/// refused calls written in after the solve); `made(call)` is a member call's made text.
pub fn write_groups<'a, N: crate::Name>(
    shapes: &[GroupShape<'a>],
    done: &HashMap<ItemKey, Resolved<'a, N>, Alloc<'a>>,
    made: &dyn Fn(ItemKey) -> Option<&'a str>,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, NameGroup<'a>>, GroupsError> {
    let mut out: AVec<'a, NameGroup<'a>> = AVec::with_capacity_in(shapes.len(), alloc);
    for shape in shapes.iter() {
        let mut reads = shape.reads.clone_in(alloc);
        reads.extend(shape.calls.iter().map(|&call| GroupRead::Expansion {
            call,
            seen: made(call).map_or(GroupSeen::Absent, |text| GroupSeen::Digest(ContentId::of(EXPANSION_DOMAIN, text.as_bytes()))),
        }));
        reads.sort_unstable();
        let mut cells_read: AVec<'a, (ItemKey, [u8; 16], ContentId)> = AVec::with_capacity_in(reads.len(), alloc);
        for r in reads.iter() {
            cells_read.push(r.engine().map_err(|module| GroupsError::Speculated { module })?);
        }
        cells_read.sort_unstable();
        let bytes = record(&shape.members, &shape.own, &reads, done, alloc)?;
        let id = ContentId::of(GROUP_DOMAIN, &bytes);
        out.push(NameGroup { members: shape.members.clone_in(alloc), reads, own: shape.own.clone_in(alloc), defines: shape.defines.clone_in(alloc), cells_read, bytes, id });
    }
    Ok(out)
}

/// Why a crate's groups were not written, naming the module (rule 19).
#[derive(Clone, Debug)]
pub enum GroupsError {
    /// A node of `module` (`reader`) read an undecided node (`on`) when run over the decided
    /// values: the solve left it undecided, or its rule reads something the solve did not.
    Waited { module: ItemKey, reader: NodeAt, on: NodeAt },
    /// A member's scope did not write.
    Scope { module: ItemKey, error: blaze_surface::BuildError },
    /// A group read a cell of `module` while it was still growing (a core bug once solved).
    Speculated { module: ItemKey },
    /// A member's own text (its `ModuleDecls`) was not read or not written.
    Own { module: ItemKey, error: OwnError },
    /// A member has no finished scope to write.
    NoScope { module: ItemKey },
}

/// Why a group record's bytes were not read, naming where (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordError {
    /// The bytes end before the field at `at`.
    Short { at: usize },
    /// Not a names group record.
    Magic,
    /// A form this blaze does not read (an old home: rebuilt, never migrated).
    Form { form: u32 },
    /// A read row with an unknown tag at `at`.
    Tag { at: usize, tag: u8 },
    /// `module` is not a member of the group.
    NotMember { module: ItemKey },
}

/// A little-endian field of `b` at `*at`, moving `*at` past it.
fn take<'b>(b: &'b [u8], at: &mut usize, n: usize) -> Result<&'b [u8], RecordError> {
    let end = at.checked_add(n).filter(|&e| e <= b.len()).ok_or(RecordError::Short { at: *at })?;
    let s = &b[*at..end];
    *at = end;
    Ok(s)
}

fn take_u32(b: &[u8], at: &mut usize) -> Result<u32, RecordError> {
    let s = take(b, at, 4)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn skip_seen(b: &[u8], at: &mut usize) -> Result<(), RecordError> {
    let tag = take(b, at, 1)?[0];
    if tag == 2 {
        take(b, at, 32)?;
    }
    Ok(())
}

impl<'a> NameGroup<'a> {
    /// `member`'s finished scope in this record (`ModuleScope` bytes, blaze_surface
    /// `ScopeView::read`): its own bindings and glob edges, what a one-group solve answers a
    /// module outside the group by. Bounded by the record's counts.
    pub fn scope_of(&self, member: ItemKey) -> Result<&[u8], RecordError> {
        scope_in(&self.members, &self.bytes, member)
    }
}

/// `member`'s scope in the record `b` of a group of `members` ([`NameGroup::scope_of`]).
pub(crate) fn scope_in<'b>(members: &[ItemKey], b: &'b [u8], member: ItemKey) -> Result<&'b [u8], RecordError> {
    {
        let index = members.binary_search(&member).map_err(|_| RecordError::NotMember { module: member })?;
        let mut at = 0usize;
        if take(b, &mut at, 4)? != MAGIC {
            return Err(RecordError::Magic);
        }
        let form = take_u32(b, &mut at)?;
        if form != FORM {
            return Err(RecordError::Form { form });
        }
        let members = take_u32(b, &mut at)? as usize;
        let rows = take_u32(b, &mut at)?;
        let keys = members.checked_mul(64).ok_or(RecordError::Short { at })?;
        take(b, &mut at, keys)?;
        for _ in 0..rows {
            let row = at;
            match take(b, &mut at, 1)?[0] {
                1 => {
                    take(b, &mut at, 32 + 16)?;
                    skip_seen(b, &mut at)?;
                }
                2 | 4 => {
                    take(b, &mut at, 64)?;
                }
                3 => {
                    take(b, &mut at, 32)?;
                    skip_seen(b, &mut at)?;
                }
                tag => return Err(RecordError::Tag { at: row, tag }),
            }
        }
        for i in 0..members {
            let len = take(b, &mut at, 8)?;
            let len = u64::from_le_bytes([len[0], len[1], len[2], len[3], len[4], len[5], len[6], len[7]]);
            let len = usize::try_from(len).map_err(|_| RecordError::Short { at })?;
            let pad = at.next_multiple_of(SCOPE_ALIGN) - at;
            take(b, &mut at, pad)?;
            let scope = take(b, &mut at, len)?;
            if i == index {
                return Ok(scope);
            }
        }
        Err(RecordError::Short { at })
    }
}

/// Why a member's own text was not had.
#[derive(Clone, Debug)]
pub enum OwnError {
    Facts(blaze_surface::SurfaceError),
    Write(blaze_surface::BuildError),
}

impl core::fmt::Display for GroupsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            GroupsError::Waited { module, reader, on } => {
                write!(f, "names groups: {reader:?} of module {module:?} read the undecided {on:?} over the solved crate")
            }
            GroupsError::Scope { module, error } => write!(f, "names groups: the scope of module {module:?}: {error:?}"),
            GroupsError::Speculated { module } => write!(f, "names groups: a cell of module {module:?} was read while the module still grew"),
            GroupsError::Own { module, error } => write!(f, "names groups: the own text of module {module:?}: {error:?}"),
            GroupsError::NoScope { module } => write!(f, "names groups: member {module:?} has no finished scope"),
        }
    }
}

/// A node, as an error names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeAt {
    /// A cell of `module`: its namespace, its name's cell key, its layer (0 full, 1 globs), and
    /// whether the groups pass made it (no node of the solve asked for it).
    Cell { module: ItemKey, ns: blaze_surface::Namespace, name: CellKey, layer: u8, made_by_pass: bool },
    /// An import or a call of `module`, by its index there; `call` for a macro call.
    Item { module: ItemKey, index: u32, call: bool },
}

/// How deep [`Cells::settle_made`] follows cells the pass made, each waiting on the next.
const MOST_SETTLED: usize = 64;

impl<'a, N: crate::Name> Cells<'a, N> {
    /// Decides `on`, a cell the groups pass made (no node of the solve asked for it: libc's
    /// module that globs itself reads its own `Full` macro cell from its `Globs` one), from its
    /// rule over the finished values, and each such cell it waits on in turn. The finished
    /// answer fixes its value; deciding it changes no scope written. False for anything else:
    /// an undecided node of the solve, or a cycle among cells the pass made (the solve's fixed
    /// point would be needed), which the caller refuses by name.
    fn settle_made(&mut self, on: Node, cells_before: usize) -> bool {
        let made = |c: CellId, s: &Self| c as usize >= cells_before && s.cells[c as usize].value.is_none();
        let Node::Cell(first) = on else { return false };
        if !made(first, self) {
            return false;
        }
        let mut stack: AVec<'a, CellId> = AVec::new_in(self.alloc);
        stack.push(first);
        // What a made cell's rule reads is not the asker's read (the asker reads the cell; the
        // lookup through it is [`Cells::read_through`]'s): the log is held off meanwhile.
        let log = self.decide_log.take();
        let mut settled = true;
        while let Some(&top) = stack.last() {
            match self.cell_value(top, None, Skip::No) {
                Step::Done(v) => {
                    self.cells[top as usize].value = Some(v);
                    stack.pop();
                }
                Step::Wait(Node::Cell(d)) if made(d, self) && !stack.contains(&d) && stack.len() < MOST_SETTLED => stack.push(d),
                Step::Wait(_) => {
                    settled = false;
                    break;
                }
            }
        }
        self.decide_log = log;
        settled
    }

    /// `m`'s binding of `name` ([`Cells::own_value`]) over the finished values, each cell the
    /// pass made on the way settled; `Err` names the undecided node it stopped on.
    fn settled_own(&mut self, m: ModId, ns: Namespace, name: N, cells_before: usize) -> Result<Option<Value>, Node> {
        let mut last = None;
        for _ in 0..=MOST_SETTLED {
            match self.own_value(m, ns, name, None) {
                Step::Done(v) => return Ok(v),
                Step::Wait(on) if self.settle_made(on, cells_before) => last = Some(on),
                Step::Wait(on) => return Err(on),
            }
        }
        Err(last.unwrap_or(Node::Cell(0)))
    }

    /// The digest of `m`'s globs cell: each glob's source and visibility (sorted, each once) and
    /// whether a glob whose path is not followed brings what is unknown (an enum's one in types
    /// and values alone): what its `Globs` layer is a function of, with its sources' bindings.
    fn globs_digest(&self, m: ModId) -> ContentId {
        let module = &self.modules[m as usize];
        // A module answered from its group's record: the digest that record defines (its
        // glob edges there are merged by source, so they are not this digest's input).
        if let super::Answered::Record(d) = module.answered {
            return d;
        }
        let mut rows: AVec<'_, [u8; 65]> = AVec::with_capacity_in(module.edges.len(), self.alloc);
        for &(src, vis) in module.edges.values() {
            let mut r = [0u8; 65];
            r[..32].copy_from_slice(&self.modules[src as usize].key.0.0);
            let (t, key) = match vis {
                super::Visibility::Public => (0u8, [0u8; 32]),
                super::Visibility::Crate(k) => (1, k.0.0),
                super::Visibility::Restricted(k) => (2, k.0.0),
            };
            r[32] = t;
            r[33..].copy_from_slice(&key);
            rows.push(r);
        }
        rows.sort_unstable();
        rows.dedup();
        let (mut unknown, mut unknown_enum) = (false, false);
        for &g in module.glob_items.iter() {
            if self.items[g as usize].outcome == Some(super::Outcome::Outside) {
                if self.items[g as usize].of_enum {
                    unknown_enum = true;
                } else {
                    unknown = true;
                }
            }
        }
        let mut b: AVec<'_, u8> = AVec::with_capacity_in(rows.len() * 65 + 2, self.alloc);
        b.push(u8::from(unknown));
        b.push(u8::from(unknown_enum));
        for r in rows.iter() {
            b.extend_from_slice(r);
        }
        ContentId::of(GLOBS_DOMAIN, &b)
    }

    /// The rows a read of cell `c` (another module's, or this one's) is valid by, onto `rows`:
    /// the binding cell of the name where the lookup is; when that is absent, the module's globs
    /// cell and the same lookup in each glob's source, each module once (bounded by the module
    /// count). A dependency's cell, an enum's and an unread module's are their decided value.
    fn read_through(&mut self, c: CellId, rows: &mut AVec<'a, (ModId, GroupRead)>, cells_before: usize) -> Result<(), Node> {
        let Cell { module: first, ns, name, layer, .. } = self.cells[c as usize];
        let alloc = self.alloc;
        let cell = cell_key(ns, name.as_ref());
        let mut walked: blaze_own::ABTreeSet<'a, ModId> = blaze_own::ABTreeSet::new_in(alloc);
        // Each entry: a module whose `Full` cell of the name is looked up, or (`true`) its globs.
        let mut todo: AVec<'a, (ModId, bool)> = AVec::new_in(alloc);
        todo.push((first, layer == Layer::Globs));
        while let Some((t, globs)) = todo.pop() {
            let key = self.modules[t as usize].key;
            if !globs {
                if !walked.insert(t) {
                    continue;
                }
                let (foreign, growth) = (self.modules[t as usize].foreign || self.modules[t as usize].unread, self.modules[t as usize].growth);
                let seen = if foreign {
                    let made = self.make_cell(t, ns, name, Layer::Full);
                    group_seen(self.cells[made as usize].value)
                } else if growth > 0 {
                    GroupSeen::Speculated
                } else {
                    match self.settled_own(t, ns, name, cells_before)? {
                        Some(v) => group_seen(Some(v)),
                        None => {
                            rows.push((t, GroupRead::Binding { table: key, name: cell, seen: GroupSeen::Absent }));
                            todo.push((t, true));
                            continue;
                        }
                    }
                };
                rows.push((t, GroupRead::Binding { table: key, name: cell, seen }));
                continue;
            }
            rows.push((t, GroupRead::Globs { table: key, seen: self.globs_digest(t) }));
            let module = &self.modules[t as usize];
            todo.extend(module.edges.values().map(|&(src, _)| (src, false)));
        }
        Ok(())
    }

    fn node_at(&self, node: Node, cells_before: usize) -> NodeAt {
        match node {
            Node::Cell(c) => {
                let cell = &self.cells[c as usize];
                let layer = match cell.layer {
                    Layer::Full => 0,
                    Layer::Globs => 1,
                };
                NodeAt::Cell { module: self.modules[cell.module as usize].key, ns: cell.ns, name: cell_key(cell.ns, cell.name.as_ref()), layer, made_by_pass: c as usize >= cells_before }
            }
            Node::Item(j) => {
                let it = &self.items[j as usize];
                NodeAt::Item { module: self.modules[it.module as usize].key, index: it.index, call: it.call.is_some() }
            }
        }
    }
}

/// Each module's reads: the binding and globs cells its lookups went through
/// ([`Cells::read_through`]), with the module each is of, and the modules whose text was walked.
struct Reads<'a> {
    rows: AVec<'a, (ModId, GroupRead)>,
    texts: AVec<'a, ModId>,
    /// The cells it defines by its own text: each binding, by its engine key, with its digest.
    binds: AVec<'a, ([u8; 16], ContentId)>,
}

impl<'a, N: crate::Name> Cells<'a, N> {
    /// The groups of this solved crate: one per strongly connected component of the written
    /// modules' (`done`'s keys) reads, with each group's reads outside it. Leaves every decided
    /// value as it was. The records are written from the crate's final scopes ([`write_groups`]).
    pub(crate) fn groups<'o>(&mut self, written: &dyn Fn(ItemKey) -> bool, own: &dyn Fn(ItemKey) -> Result<ContentId, GroupsError>, out: Alloc<'o>) -> Result<AVec<'o, GroupShape<'o>>, GroupsError> {
        // The walk's scratch is the core's own allocator; the shapes are in `out`.
        let alloc = self.alloc;
        // The written modules, by key: the graph's nodes.
        let mut local: AVec<'a, ModId> = AVec::new_in(alloc);
        for (m, module) in self.modules.iter().enumerate() {
            if written(module.key) {
                local.push(m as ModId);
            }
        }
        local.sort_unstable_by_key(|&m| self.modules[m as usize].key);
        let mut at: HashMap<ModId, u32, Alloc<'a>> = HashMap::new_in(alloc);
        for (i, &m) in local.iter().enumerate() {
            at.insert(m, i as u32);
        }
        // Each module's items, taken before any re-run makes a cell. A module's reads are its
        // items' and its own bindings' alone: a cell some other module asked of it is that
        // reader's read (through it), never its own, so no record holds who asked what.
        let mut items_of: AVec<'a, AVec<'a, ItemId>> = AVec::new_in(alloc);
        for _ in 0..local.len() {
            items_of.push(AVec::new_in(alloc));
        }
        for (j, item) in self.items.iter().enumerate() {
            if let (Some(&i), Some(_)) = (at.get(&item.module), item.outcome) {
                items_of[i as usize].push(j as ItemId);
            }
        }
        // Each module's direct reads.
        let cells_before = self.cells.len();
        let mut reads: AVec<'a, Reads<'a>> = AVec::with_capacity_in(local.len(), alloc);
        for (i, &m) in local.iter().enumerate() {
            let module_key = self.modules[m as usize].key;
            self.decide_log = Some(AVec::new_in(alloc));
            self.textual_log = Some(AVec::new_in(alloc));
            self.textual_rec_log = Some(AVec::new_in(alloc));
            // Each binding its own text makes: every name it declares, and every name a named
            // import binds, in each namespace (what it binds there, or nothing).
            let mut names: AVec<'a, (Namespace, N)> = AVec::new_in(alloc);
            names.extend(self.modules[m as usize].explicit.keys().copied());
            for name in self.modules[m as usize].named.keys() {
                names.extend(NAMESPACES.iter().map(|&ns| (ns, *name)));
            }
            names.sort_unstable();
            names.dedup();
            let mut binds: AVec<'a, ([u8; 16], ContentId)> = AVec::with_capacity_in(names.len(), alloc);
            for &(ns, name) in names.iter() {
                match self.settled_own(m, ns, name, cells_before) {
                    Ok(Some(v)) => {
                        if let GroupSeen::Digest(d) = group_seen(Some(v)) {
                            binds.push((NameCell::Binding { module: module_key, name: cell_key(ns, name.as_ref()) }.key(), d));
                        }
                    }
                    Ok(None) => {}
                    Err(on) => {
                        self.decide_log = None;
                        self.textual_log = None;
                        self.textual_rec_log = None;
                        let reader = NodeAt::Cell { module: module_key, ns, name: cell_key(ns, name.as_ref()), layer: 0, made_by_pass: false };
                        return Err(GroupsError::Waited { module: module_key, reader, on: self.node_at(on, cells_before) });
                    }
                }
            }
            binds.sort_unstable();
            for &j in items_of[i].iter() {
                loop {
                    match self.try_item(j, None, Hold::Undecided) {
                        Step::Done(_) => break,
                        Step::Wait(on) if self.settle_made(on, cells_before) => {}
                        Step::Wait(on) => {
                            self.decide_log = None;
                            self.textual_log = None;
                            self.textual_rec_log = None;
                            return Err(GroupsError::Waited { module: module_key, reader: self.node_at(Node::Item(j), cells_before), on: self.node_at(on, cells_before) });
                        }
                    }
                }
            }
            let mut cells = self.decide_log.take().unwrap_or_else(|| AVec::new_in(alloc));
            cells.sort_unstable();
            cells.dedup();
            let frags = self.textual_log.take().unwrap_or_else(|| AVec::new_in(alloc));
            let mut texts: AVec<'a, ModId> = AVec::with_capacity_in(frags.len(), alloc);
            texts.extend(frags.iter().map(|&f| self.frags[f as usize].module).filter(|&t| t != m));
            // The modules outside a one-group solve's members whose sequence a walk read from
            // their records: the same cells the walk over their texts reads.
            if let Some(rec) = self.textual_rec_log.take() {
                texts.extend(rec.iter().copied().filter(|&t| t != m));
            }
            // The `#[macro_use]` children its scope's textual sequence flattens in (write.rs): its
            // record holds their sequences, so it reads them (rule 21), whether a walk did or not.
            self.flattened_into(m, &mut texts);
            texts.sort_unstable();
            texts.dedup();
            // Each cell read, as the binding and globs cells its lookup went through (its own
            // module's too: a lookup in itself through its globs reads their sources).
            let mut rows: AVec<'a, (ModId, GroupRead)> = AVec::new_in(alloc);
            for &c in cells.iter() {
                if let Err(on) = self.read_through(c, &mut rows, cells_before) {
                    return Err(GroupsError::Waited { module: module_key, reader: self.node_at(Node::Cell(c), cells_before), on: self.node_at(on, cells_before) });
                }
            }
            rows.retain(|(t, _)| *t != m);
            rows.sort_unstable();
            rows.dedup();
            reads.push(Reads { rows, texts, binds });
        }
        // The graph: a module reads another written module's cell or text.
        let mut edges: AVec<'a, AVec<'a, u32>> = AVec::with_capacity_in(local.len(), alloc);
        for r in reads.iter() {
            let mut out: AVec<'a, u32> = AVec::new_in(alloc);
            out.extend(r.rows.iter().filter_map(|(t, _)| at.get(t).copied()));
            out.extend(r.texts.iter().filter_map(|t| at.get(t).copied()));
            out.sort_unstable();
            out.dedup();
            edges.push(out);
        }
        let components = strong_components(&edges, alloc);
        // Each expansion's fragment by its call's key: what a textual digest names it by.
        let mut expansion_of: HashMap<FragId, ItemKey, Alloc<'a>> = HashMap::new_in(alloc);
        for it in self.items.iter() {
            if let (Some(x), Some(at)) = (it.expansion, it.call) {
                expansion_of.insert(x, at.key);
            }
        }
        // Each module's fragments (one scan), and each local module's component.
        let mut frags_of: AVec<'a, AVec<'a, FragId>> = AVec::with_capacity_in(self.modules.len(), alloc);
        for _ in 0..self.modules.len() {
            frags_of.push(AVec::new_in(alloc));
        }
        for (f, frag) in self.frags.iter().enumerate() {
            frags_of[frag.module as usize].push(f as FragId);
        }
        let mut textual: HashMap<ModId, ContentId, Alloc<'a>> = HashMap::new_in(alloc);
        // A module outside a one-group solve's members has no fragment here: its textual
        // cell's digest is its record's.
        for (&t, r) in self.recorded_textual.iter() {
            textual.insert(t, r.digest);
        }
        let mut comp_of: AVec<'a, u32> = AVec::with_capacity_in(local.len(), alloc);
        comp_of.resize(local.len(), 0);
        for (ci, members) in components.iter().enumerate() {
            for &i in members.iter() {
                comp_of[i as usize] = ci as u32;
            }
        }
        // Each component's record.
        let mut groups: AVec<'o, GroupShape<'o>> = AVec::with_capacity_in(components.len(), out);
        for (ci, members) in components.iter().enumerate() {
            let mut keys: AVec<'o, ItemKey> = AVec::with_capacity_in(members.len(), out);
            keys.extend(members.iter().map(|&i| self.modules[local[i as usize] as usize].key));
            keys.sort_unstable();
            let inside = |m: ModId| at.get(&m).is_some_and(|&i| comp_of[i as usize] == ci as u32);
            let mut rows: AVec<'o, GroupRead> = AVec::new_in(out);
            for &i in members.iter() {
                let r = &reads[i as usize];
                rows.extend(r.rows.iter().filter(|(t, _)| !inside(*t)).map(|(_, row)| *row));
                for &t in r.texts.iter() {
                    if !inside(t) {
                        let seen = self.textual_digest(t, &frags_of, &expansion_of, &mut textual);
                        rows.push(GroupRead::Textual { module: self.modules[t as usize].key, seen });
                    }
                }
            }
            rows.sort_unstable();
            rows.dedup();
            // The members' calls and defined cells, from their own lists (never a crate scan).
            let mut calls: AVec<'o, ItemKey> = AVec::new_in(out);
            let mut defines: AVec<'o, ([u8; 16], ContentId)> = AVec::new_in(out);
            for &i in members.iter() {
                let key = self.modules[local[i as usize] as usize].key;
                for &j in items_of[i as usize].iter() {
                    let it = &self.items[j as usize];
                    if let (true, Some(call)) = (it.grown, it.call) {
                        calls.push(call.key);
                    }
                }
                defines.extend(reads[i as usize].binds.iter().copied());
                let m = local[i as usize];
                defines.push((globs_cell(key), self.globs_digest(m)));
                // Each member's textual sequence: what a `macro_rules!` lookup walking its text reads.
                let d = self.textual_digest(m, &frags_of, &expansion_of, &mut textual);
                defines.push((textual_cell(key), d));
            }
            calls.sort_unstable();
            calls.dedup();
            defines.sort_unstable();
            let mut by_key: AVec<'o, (ItemKey, ContentId)> = AVec::with_capacity_in(keys.len(), out);
            for &i in members.iter() {
                let key = self.modules[local[i as usize] as usize].key;
                by_key.push((key, own(key)?));
            }
            by_key.sort_unstable();
            let mut owns: AVec<'o, ContentId> = AVec::with_capacity_in(keys.len(), out);
            owns.extend(by_key.iter().map(|(_, o)| *o));
            groups.push(GroupShape { members: keys, reads: rows, calls, own: owns, defines });
        }
        groups.sort_unstable_by_key(|g| g.members[0]);
        Ok(groups)
    }

    /// The `#[macro_use]` children whose sequences module `m`'s scope flattens into its own
    /// (write.rs, the same walk): through its text and its calls' expansions, into each such
    /// child read here and its own. A child outside a one-group solve's members is flattened
    /// from its record alone, whose digest covers its own children ([`Cells::textual_digest`]).
    /// Each fragment belongs to one module or one call, so each is met once (the module tree
    /// and its calls bound the walk: rule 18).
    fn flattened_into(&self, m: ModId, out: &mut AVec<'a, ModId>) {
        let text = self.modules[m as usize].text;
        if text == FragId::MAX {
            return;
        }
        let mut todo: AVec<'a, FragId> = AVec::new_in(self.alloc);
        todo.push(text);
        while let Some(f) = todo.pop() {
            for &(_, slot) in self.frags[f as usize].slots.iter() {
                match slot {
                    Slot::Mod(child, true) => {
                        let c = &self.modules[child as usize];
                        if c.foreign || c.unread {
                            continue;
                        }
                        if child != m {
                            out.push(child);
                        }
                        if c.text != FragId::MAX {
                            todo.push(c.text);
                        }
                    }
                    Slot::Call(j) => {
                        if let Some(e) = self.items[j as usize].expansion {
                            todo.push(e);
                        }
                    }
                    Slot::Rules(..) | Slot::Mod(_, false) => {}
                }
            }
        }
    }

    /// The digest of module `m`'s textual sequence, memoized in `memo` (which holds the
    /// recorded digest of each module outside a one-group solve's members), over its fragments
    /// (`frags_of[m]`: its text, then its calls' expansions by the call's key), each slot in
    /// text order with no position: a `macro_rules!` by name and definition, a `mod` line by
    /// the child's key and its `#[macro_use]`, a call by its key and whether it grew. A
    /// `#[macro_use]` child's line carries the child's own digest too: the record answers a
    /// reader of `m`'s sequence with the children's flattened in (write.rs, `textual_recorded`),
    /// so the cell moves when what it answers moves (rule 21). The recursion follows the module
    /// tree down, so it ends.
    fn textual_digest(&self, m: ModId, frags_of: &[AVec<'a, FragId>], expansion_of: &HashMap<FragId, ItemKey, Alloc<'a>>, memo: &mut HashMap<ModId, ContentId, Alloc<'a>>) -> ContentId {
        if let Some(&d) = memo.get(&m) {
            return d;
        }
        struct Frame<'a> {
            cur: ModId,
            frags: AVec<'a, (Option<ItemKey>, FragId)>,
            ready: bool,
        }
        let mut stack: AVec<'a, Frame<'a>> = AVec::new_in(self.alloc);
        let of = &frags_of[m as usize];
        let mut root_frags: AVec<'a, (Option<ItemKey>, FragId)> = AVec::with_capacity_in(of.len(), self.alloc);
        // An expansion's fragment by its call's key; the module's text first.
        root_frags.extend(of.iter().map(|&f| (expansion_of.get(&f).copied(), f)));
        root_frags.sort_unstable_by_key(|(call, _)| *call);
        stack.push(Frame { cur: m, frags: root_frags, ready: false });
        while let Some(frame) = stack.pop() {
            if memo.contains_key(&frame.cur) {
                continue;
            }
            if !frame.ready {
                let mut children: AVec<'a, ModId> = AVec::new_in(self.alloc);
                for &(_, f) in frame.frags.iter() {
                    if let Some(frag) = usize::try_from(f).ok().and_then(|i| self.frags.get(i)) {
                        for &(_, slot) in frag.slots.iter() {
                            if let Slot::Mod(child, true) = slot
                                && let Some(c) = usize::try_from(child).ok().and_then(|i| self.modules.get(i))
                                && !c.foreign
                                && !c.unread
                                && c.text != FragId::MAX
                                && child != frame.cur
                                && !memo.contains_key(&child)
                            {
                                children.push(child);
                            }
                        }
                    }
                }
                if !children.is_empty() {
                    stack.push(Frame { cur: frame.cur, frags: frame.frags, ready: true });
                    for &child in children.iter().rev() {
                        if let Some(of) = usize::try_from(child).ok().and_then(|i| frags_of.get(i)) {
                            let mut cf: AVec<'a, (Option<ItemKey>, FragId)> = AVec::with_capacity_in(of.len(), self.alloc);
                            cf.extend(of.iter().map(|&f| (expansion_of.get(&f).copied(), f)));
                            cf.sort_unstable_by_key(|(call, _)| *call);
                            stack.push(Frame { cur: child, frags: cf, ready: false });
                        }
                    }
                    continue;
                }
            }
            let mut b: AVec<'_, u8> = AVec::new_in(self.alloc);
            for &(call, f) in frame.frags.iter() {
                b.push(0xFF);
                if let Some(k) = call {
                    b.extend_from_slice(&k.0.0);
                }
                for &(_, slot) in self.frags[f as usize].slots.iter() {
                    match slot {
                        Slot::Rules(name, def, export) => {
                            b.push(1);
                            let name = name.as_ref().as_bytes();
                            b.extend_from_slice(&(name.len() as u32).to_le_bytes());
                            b.extend_from_slice(name);
                            b.extend_from_slice(&def.key.0.0);
                            b.push(u8::from(export));
                        }
                        Slot::Mod(child, macro_use) => {
                            b.push(2);
                            b.extend_from_slice(&self.modules[child as usize].key.0.0);
                            b.push(u8::from(macro_use));
                            let c = &self.modules[child as usize];
                            if macro_use && !c.foreign && !c.unread {
                                // Its sequence, flattened into `m`'s: read here, or its record's
                                // (in `memo`); none held is absent (write.rs counts it unknown).
                                let d = memo.get(&child).copied().unwrap_or(ABSENT);
                                b.extend_from_slice(&d.0);
                            }
                        }
                        Slot::Call(j) => {
                            let it = &self.items[j as usize];
                            b.push(3);
                            if let Some(at) = it.call {
                                b.extend_from_slice(&at.key.0.0);
                            }
                            b.push(u8::from(it.grown));
                        }
                    }
                }
            }
            let d = ContentId::of(TEXTUAL_DOMAIN, &b);
            memo.insert(frame.cur, d);
        }
        memo.get(&m).copied().unwrap_or(ABSENT)
    }
}

fn seen_bytes(b: &mut AVec<'_, u8>, seen: GroupSeen) {
    match seen {
        GroupSeen::Absent => b.push(0),
        GroupSeen::Speculated => b.push(1),
        GroupSeen::Digest(d) => {
            b.push(2);
            b.extend_from_slice(&d.0);
        }
    }
}

/// The record of one group: its members (sorted), its reads (sorted) and each member's scope.
fn record<'a, N: crate::Name>(members: &[ItemKey], own: &[ContentId], rows: &[GroupRead], done: &HashMap<ItemKey, Resolved<'a, N>, Alloc<'a>>, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, GroupsError> {
    let mut b: AVec<'a, u8> = AVec::new_in(alloc);
    b.extend_from_slice(&MAGIC);
    b.extend_from_slice(&FORM.to_le_bytes());
    b.extend_from_slice(&(members.len() as u32).to_le_bytes());
    b.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    for (k, o) in members.iter().zip(own.iter()) {
        b.extend_from_slice(&k.0.0);
        b.extend_from_slice(&o.0);
    }
    for row in rows.iter() {
        match *row {
            GroupRead::Binding { table, name, seen } => {
                b.push(1);
                b.extend_from_slice(&table.0.0);
                b.extend_from_slice(&name.0);
                seen_bytes(&mut b, seen);
            }
            GroupRead::Globs { table, seen } => {
                b.push(4);
                b.extend_from_slice(&table.0.0);
                b.extend_from_slice(&seen.0);
            }
            GroupRead::Textual { module, seen } => {
                b.push(2);
                b.extend_from_slice(&module.0.0);
                b.extend_from_slice(&seen.0);
            }
            GroupRead::Expansion { call, seen } => {
                b.push(3);
                b.extend_from_slice(&call.0.0);
                seen_bytes(&mut b, seen);
            }
        }
    }
    for k in members.iter() {
        // Every member is a key of `done` (the graph's nodes are); one missing would misalign
        // the scopes [`NameGroup::scope_of`] reads.
        let Some(resolved) = done.get(k) else { return Err(GroupsError::NoScope { module: *k }) };
        let scope = resolved.scope.write(alloc).map_err(|error| GroupsError::Scope { module: *k, error })?;
        b.extend_from_slice(&(scope.len() as u64).to_le_bytes());
        // Each scope at a 16-byte offset: read in place from a record held aligned.
        b.resize(b.len().next_multiple_of(SCOPE_ALIGN), 0);
        b.extend_from_slice(&scope);
    }
    Ok(b)
}

/// The strongly connected components of `edges` (node `i` reads each of `edges[i]`), each
/// sorted, by an iterative Tarjan walk from each node in order: a function of the graph alone.
fn strong_components<'a>(edges: &[AVec<'a, u32>], alloc: Alloc<'a>) -> AVec<'a, AVec<'a, u32>> {
    const UNSEEN: u32 = u32::MAX;
    let n = edges.len();
    let mut index: AVec<'a, u32> = AVec::new_in(alloc);
    index.resize(n, UNSEEN);
    let mut low: AVec<'a, u32> = AVec::new_in(alloc);
    low.resize(n, 0);
    let mut on: AVec<'a, bool> = AVec::new_in(alloc);
    on.resize(n, false);
    let mut stack: AVec<'a, u32> = AVec::new_in(alloc);
    // The walk: (node, next edge to look at).
    let mut walk: AVec<'a, (u32, usize)> = AVec::new_in(alloc);
    let mut out: AVec<'a, AVec<'a, u32>> = AVec::new_in(alloc);
    let mut next = 0u32;
    for root in 0..n as u32 {
        if index[root as usize] != UNSEEN {
            continue;
        }
        walk.push((root, 0));
        index[root as usize] = next;
        low[root as usize] = next;
        next += 1;
        stack.push(root);
        on[root as usize] = true;
        while let Some(&mut (v, ref mut e)) = walk.last_mut() {
            if let Some(&w) = edges[v as usize].get(*e) {
                *e += 1;
                if index[w as usize] == UNSEEN {
                    index[w as usize] = next;
                    low[w as usize] = next;
                    next += 1;
                    stack.push(w);
                    on[w as usize] = true;
                    walk.push((w, 0));
                } else if on[w as usize] {
                    low[v as usize] = low[v as usize].min(index[w as usize]);
                }
                continue;
            }
            walk.pop();
            if let Some(&(u, _)) = walk.last() {
                low[u as usize] = low[u as usize].min(low[v as usize]);
            }
            if low[v as usize] == index[v as usize] {
                let mut component: AVec<'a, u32> = AVec::new_in(alloc);
                while let Some(w) = stack.pop() {
                    on[w as usize] = false;
                    component.push(w);
                    if w == v {
                        break;
                    }
                }
                component.sort_unstable();
                out.push(component);
            }
        }
    }
    out
}
