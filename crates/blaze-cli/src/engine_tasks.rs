//! The engine's tasks for `blaze engine-check` (engine.md E6): what one entered task does, by its
//! stage, writing its result into its worker's buffer.
//!
//! Outlines: the planner outlines every file before the first phase (invariant 1 of engine.rs:
//! an edit holds every planned task of each changed file, and those come from the outline), so
//! the `Outlines` task publishes the outline the planner made, including the separately
//! addressable text of each item and the source of each planned signature and body.
//!
//! A panic in a task fails that task alone (rule 20): a failed result, which defines nothing and
//! prints as a failure, and the phase goes on.

use blaze_drive::cells::Cell;
use blaze_drive::{Name, Why};
use blaze_engine::{Buffer, CellId, Done, SVec, Stage, Task, View};
use blaze_ids::ContentId;
use blaze_own::SharedAlloc;
use bstd::cx::Cx;

use crate::work::{self, Record};

/// The cell of a record read whole.
pub(crate) const WHOLE: Cell = Cell([0; 16]);

/// An outline's second cell: its file's surface (blaze-file-items `Outline::surface`, the
/// outline record's `Surface` part), what name resolution reads of the file. A comment or a
/// body's statement moves the outline's [`WHOLE`] cell and leaves this one, so a crate's names,
/// which read only this, are not entered for it.
pub(crate) const SURFACE: Cell = Cell([1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

/// An outline's third cell: its file's heads, every `Signature` part of its record (signature.rs:
/// each entry's head bytes and the file's surface) hashed in the record's order. INTERIM, until a
/// Heads stage defines one cell per head: a body's read of a head (a `Signature` cell no task
/// defines) is widened to this cell of each file of the head's crate (engine_bodies.rs `widen`),
/// so a signature edit enters every body that read a head of that crate's edited file. Coarse and
/// sound: a comment or a statement in a body moves no head, so it moves nothing here.
pub(crate) const HEADS: Cell = Cell([2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

/// A part's source in its file's outline. The name, including the part kind, is stable when
/// an unrelated edit moves the entry. No placement goes into this cell or its digest.
pub(crate) fn part_cell(part: Name) -> Cell {
    named_cell("blaze-cli outline part source cell 1", part)
}

pub(crate) fn definition_cell(entry: Name) -> Cell {
    named_cell("blaze-cli outline full item text cell 1", entry)
}

pub(crate) fn signature_cell(entry: Name) -> Cell {
    named_cell("blaze-cli outline signature text cell 1", entry)
}

fn named_cell(domain: &'static str, name: Name) -> Cell {
    let digest = blaze_ids::Hasher::new(domain).update(&[name.kind as u8]).id(&name.subject).finish();
    Cell(digest.0[..16].try_into().expect("a digest holds sixteen bytes"))
}

/// A named associated item can have only a member row, which has no grain chain. Its
/// owner's chain, member kind and name identify the group of cfg alternatives for that
/// member. The ordered tokens of that group are published alongside the per-entry cells.
pub(crate) fn member_entry(owner: Name, kind: blaze_surface::ItemKind, name: &str) -> Name {
    let digest = blaze_ids::Hasher::new("blaze-cli outline member text group 1").id(&owner.subject).update(&[kind.byte()]).part(name.as_bytes()).finish();
    Name::new(blaze_drive::Kind::Signature, digest)
}

fn item_cells<'a>(file: crate::signature::SignatureFile, view: blaze_surface::FileItemsView<'_>, defines: &mut SVec<'a, (Cell, ContentId)>, alloc: SharedAlloc<'a>) {
    use blaze_surface::{ItemIndex, ItemKind};
    let mut groups = SVec::new_in(alloc);
    for (index, entry) in view.items().enumerate() {
        let at = ItemIndex(index as u32);
        let item = &entry.item;
        let name = file.entry_from_chain(crate::signature::stored_chain(view, at));
        let signature = crate::signature::stored_signature(view, at, item);
        defines.push((definition_cell(name), item.hash));
        defines.push((signature_cell(name), signature));
        if !entry.name.is_empty() && let Some(up) = item.parent
            && view.item(up).is_some_and(|owner| matches!(owner.kind(), ItemKind::Trait | ItemKind::Impl))
        {
            let owner = file.entry_from_chain(crate::signature::stored_chain(view, up));
            let group = member_entry(owner, item.kind(), blaze_scope::task::unraw(entry.name));
            groups.push((group, at.0, item.hash, signature));
        }
    }
    groups.sort_unstable_by_key(|row| (row.0, row.1));
    for rows in groups.chunk_by(|a, b| a.0 == b.0) {
        let mut full = blaze_ids::Hasher::new("blaze-cli member definition alternatives 1");
        let mut head = blaze_ids::Hasher::new("blaze-cli member signature alternatives 1");
        for (_, _, text, sig) in rows {
            full.id(text);
            head.id(sig);
        }
        defines.push((definition_cell(rows[0].0), full.finish()));
        defines.push((signature_cell(rows[0].0), head.finish()));
    }
}

const HEADS_DOMAIN: &str = "blaze-cli engine outline heads 1";

/// The digest of every `Signature` part of an outline record, in order ([`HEADS`]).
pub(crate) fn heads_of<A: core::alloc::Allocator + Copy>(record: &Record<A>) -> ContentId {
    let mut h = blaze_ids::Hasher::new(HEADS_DOMAIN);
    for part in record.parts.iter() {
        if let (blaze_drive::Kind::Signature, blaze_drive::Locate::Source(source)) = (part.name.kind, part.locate) {
            h.update(&part.name.subject.0).update(&source.0);
        }
    }
    h.finish()
}

/// A `CrateScope` task's mark that it defines every cell of its record's cell table
/// (blaze-scope names_cells.rs, the payload's `cells` section) beside its [`WHOLE`] cell: what
/// lets a body keep a names read as its cell (engine_bodies.rs `widen`). A crate that defines
/// it holds each cell of its table and nothing else, so a cell it does not define is ABSENT by
/// the table's contract (every cell a walk can name that holds something is a row), and adding
/// the name later defines it, which moves it for every reader that saw it absent (rule 21). A
/// crate without the mark (a dependency: no file of the checkout feeds its names, so no edit
/// moves them) defines [`WHOLE`] alone and its readers read it whole. Its digest is the cell
/// table's own, so a reader never needs it as a read. (A cell of a `CrateScope` name, never of an
/// outline's: its byte is not [`HEADS`]'s so the two are not read as one.)
pub(crate) const CELLS: Cell = Cell([3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

/// A `CrateScope` task's exports section digest (`names/sections.rs` `Sections::exports`): what
/// downstream `CrateScope` tasks read from a dependency. An edit inside a crate that leaves its
/// exports unchanged (e.g. adding a private function) moves [`WHOLE`] without moving [`EXPORTS`],
/// so no downstream `Names` task re-enters.
pub(crate) const EXPORTS: Cell = Cell([4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

/// An outline's raw-bytes cell (`data_include::data_out(bytes)`, from `FileItemsView::file`): what
/// a data-include reader (`include_str!`, `include_bytes!`, body `include!`) reads when its target
/// file is ALSO outlined as a Rust source module. A comment or whitespace edit leaves [`WHOLE`],
/// [`SURFACE`] and [`HEADS`] unchanged while moving [`DATA`], so data-include readers re-enter
/// without unnecessarily invalidating Rust-module readers.
pub(crate) const DATA: Cell = Cell([5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

/// The surface digest an outline record lists (its `Surface` part's source), or `None` for a
/// record with no surface (a data file).
pub(crate) fn surface_of<A: core::alloc::Allocator + Copy>(record: &Record<A>) -> Option<ContentId> {
    record.parts.iter().find_map(|part| match (part.name.kind, part.locate) {
        (blaze_drive::Kind::Surface, blaze_drive::Locate::Source(source)) => Some(source),
        _ => None,
    })
}

/// The raw-bytes digest of a Rust outline record (`FileItemsView::file`, equal to
/// `data_include::data_out`), or `None` for a non-Rust record (such as a pure `DATA_FILE`, whose
/// [`WHOLE`] cell already holds `data_out` or `absent_out`).
pub(crate) fn data_digest_of<A: core::alloc::Allocator + Copy>(record: &Record<A>) -> Option<ContentId> {
    surface_of(record)?;
    let payload = record.payload_bytes().ok()?;
    let view = blaze_surface::FileItemsView::read(payload).ok()?;
    Some(view.file())
}

const EDITION_DOMAIN: &str = "blaze-cli engine source with edition 1";

/// A text's digest under the edition that parses it. The lint worker verifies the file
/// bytes with this digest. Item tasks apply the same rule to their own source.
pub(crate) fn edition_source(text: ContentId, edition: u8) -> ContentId {
    let mut h = blaze_ids::Hasher::new(EDITION_DOMAIN);
    h.update(&text.0).update(&[edition]);
    h.finish()
}

const OUTLINE_SOURCE_DOMAIN: &str = "blaze-cli engine outline source 2";

/// A slot and these flags determine both the physical file that names its signature
/// parts and the directory in which its child modules are resolved.
pub(crate) fn outline_source(source_with_edition: ContentId, flags: u8) -> ContentId {
    let mut h = blaze_ids::Hasher::new(OUTLINE_SOURCE_DOMAIN);
    h.update(&source_with_edition.0)
        .update(&[flags & (crate::sweep::MODULE | crate::sweep::AT_MOD_RS | crate::sweep::BOTH)]);
    h.finish()
}

/// One file the planner outlined, by its `FileItems` name.
pub(crate) struct Outlined<'h> {
    pub(crate) name: Name,
    /// Its path in the checkout, as read (`sweep::real_path`).
    pub(crate) path: SVec<'h, u8>,
    /// The digest used when the lint worker verifies these bytes ([`edition_source`]).
    pub(crate) source: ContentId,
    /// The `Outlines` task source, including the sweep's module-form flags
    /// ([`outline_source`]).
    pub(crate) outline_source: ContentId,
    /// The edition byte its crate's part gave it: the outline and every body parse by it.
    pub(crate) edition: u8,
    /// Its outline, or why it could not be made.
    pub(crate) made: Result<Record<SharedAlloc<'h>>, Why>,
}

/// What every task reads besides the tables: the planner's outlines, sorted by name.
pub(crate) struct Tasks<'p, 'h> {
    pub(crate) outlined: &'p [Outlined<'h>],
}

impl Tasks<'_, '_> {
    /// Runs `task` into `buffer`, its result's lists in `alloc`; a panic is the task's failure.
    pub(crate) fn work<'a>(&self, _cx: &Cx, _view: View<'_, 'a>, task: &Task, buffer: &mut Buffer<'a>, alloc: SharedAlloc<'a>) {
        let ran = work::unwinding(|| match task.planned.phase.stage {
            Stage::Outlines => self.outline(task, alloc),
            // Not planned yet (engine_check.rs `plan`): a task entered in a stage with no runner
            // here is a failure row, named, never skipped.
            // The units, scripts and names stages run on the coordinator (engine_units.rs,
            // engine_scripts.rs, engine_names.rs), never through this runner.
            Stage::Units | Stage::Scripts | Stage::Names | Stage::Heads | Stage::Bodies | Stage::Lints => failed(task, alloc),
        });
        buffer.done.push(match ran {
            Ok(done) => done,
            Err(work::Panicked) => failed(task, alloc),
        });
    }

    /// The `Outlines` task: the planner's outline of the task's file, published. One code for
    /// both drivers: the phase path's [`Tasks::work`] and the `Ready` kind (engine_ready_check.rs).
    pub(crate) fn outline<'a>(&self, task: &Task, alloc: SharedAlloc<'a>) -> Done<'a> {
        let Ok(at) = self.outlined.binary_search_by(|one| one.name.cmp(&task.planned.name)) else {
            return failed(task, alloc);
        };
        let Ok(record) = &self.outlined[at].made else {
            return failed(task, alloc);
        };
        let mut defines = SVec::new_in(alloc);
        defines.push((WHOLE, record.out));
        if let Some(surface) = surface_of(record) {
            defines.push((SURFACE, surface));
            let payload = record.payload_bytes().unwrap_or_else(|error| panic!("outline {:?} has no item payload: {error:?}", record.name));
            let view = blaze_surface::FileItemsView::read(payload).unwrap_or_else(|error| panic!("outline {:?} has an invalid item payload: {error:?}", record.name));
            defines.push((DATA, view.file()));
            item_cells(crate::signature::SignatureFile::new(&self.outlined[at].path), view, &mut defines, alloc);
        }
        defines.push((HEADS, heads_of(record)));
        for part in &record.parts {
            if matches!(part.name.kind, blaze_drive::Kind::Signature | blaze_drive::Kind::BodyCheck) {
                let blaze_drive::Locate::Source(digest) = part.locate else {
                    panic!("outline {:?} has no source digest for item part {:?}", record.name, part.name);
                };
                defines.push((part_cell(part.name), digest));
            }
        }
        defines.sort_unstable_by_key(|(cell, _)| *cell);
        if let Some(pair) = defines.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            panic!("outline {:?} defines item cell {:?} twice", record.name, pair[0].0);
        }
        Done {
            name: task.planned.name,
            key: task.key,
            out: record.out,
            defines,
            reads: SVec::new_in(alloc),
            findings: u32::try_from(record.findings.len()).unwrap_or(u32::MAX),
            failed: false,
        }
    }
}

/// A failed result: no cell, one finding (the failure itself), the failure's out. It reads
/// nothing, so it is owed: entered again by every request (a panic, a machine fault, a task with
/// no runner); a refusal that is a function of cells is [`failed_with`]'s (plan row A8).
pub(crate) fn failed<'a>(task: &Task, alloc: SharedAlloc<'a>) -> Done<'a> {
    failed_with(task, SVec::new_in(alloc), alloc)
}

/// A failed result that stands on what its failure saw (plan row A8): `reads`, each cell with
/// the digest the view held when the task refused (an absent cell read as ABSENT). They are
/// sorted by cell id and kept once each here, whatever order the caller gathered them in, so a
/// read list names each cell once (rules 21, 25). The row stands until one of them moves, and is
/// then entered again once (invariant 7); with no read it is [`failed`]'s, owed. No cell, one
/// finding (the failure itself), the failure's out.
pub(crate) fn failed_with<'a>(task: &Task, reads: SVec<'a, (CellId, ContentId)>, alloc: SharedAlloc<'a>) -> Done<'a> {
    let mut reads = reads;
    reads.sort_unstable_by_key(|(id, _)| *id);
    reads.dedup_by_key(|(id, _)| *id);
    Done {
        name: task.planned.name,
        key: task.key,
        out: work::failed_out(),
        defines: SVec::new_in(alloc),
        reads,
        findings: 1,
        failed: true,
    }
}

#[cfg(test)]
mod item_text_tests {
    use super::*;
    use blaze_surface::{Edition, FileItemsView, ItemIndex};

    #[test]
    fn restored_outline_names_the_same_text_as_the_item_record() {
        let heap = blaze_own::testing::heap();
        let file = crate::signature::SignatureFile::new(b"src/order/mod.rs");
        let cells = |body: &str, padding: &str| {
            let text = format!("{padding}struct A; impl A {{ const fn r#width() -> usize {{ {body} }} }} impl From<A> for u8 {{ fn from(_: A) -> u8 {{ 0 }} }}");
            let outline = blaze_file_items::outline(&text, Edition::E2021, heap.alloc());
            let bytes = outline.to_file_items(&text, ContentId::of("test file", text.as_bytes()), heap.alloc()).expect("outline serializes");
            let view = FileItemsView::read(&bytes).expect("outline opens");
            let mut defines = SVec::new_in(heap.shared());
            item_cells(file, view, &mut defines, heap.shared());
            let mut width = None;
            for (i, item) in outline.items().iter().enumerate() {
                let chain = blaze_file_items::item_chain(&outline, item, &text);
                assert_eq!(crate::signature::stored_chain(view, ItemIndex(i as u32)), chain, "stored and live item identity at {i}");
                if item.name.is_some_and(|name| item.text(&text, name) == "r#width") {
                    let entry = file.entry_from_chain(chain);
                    let owner = file.entry(&outline, outline.item(item.parent.unwrap()), &text);
                    let member = member_entry(owner, item.kind(), "width");
                    let get = |cell| defines.iter().find(|(id, _)| *id == cell).expect("the file publishes this facet").1;
                    width = Some((entry, get(signature_cell(entry)), get(definition_cell(entry)), get(signature_cell(member)), get(definition_cell(member))));
                }
            }
            width.expect("the raw identifier names the associated const fn")
        };
        let before = cells("3", "");
        assert_eq!(before, cells("3", "// move the declarations\n"));
        let after = cells("4", "");
        assert_eq!((before.0, before.1, before.3), (after.0, after.1, after.3));
        assert_ne!(before.2, after.2, "a call evaluates the edited definition");
        assert_ne!(before.4, after.4, "a call through the member row evaluates that same edit");
    }
}
