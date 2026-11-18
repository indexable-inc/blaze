//! CONSTANTS IN THE ENGINE'S HEAD TASKS (runtime.md A2, A8): a head task's freeze is given a
//! constant evaluator, so a head whose type holds a constant (`[u8; LEN]`, `[u8; 2 + 2]`) is
//! frozen with its value instead of refused for having no evaluator.
//!
//! WHAT THE EVALUATOR NEEDS, AND WHAT IT IS GIVEN: front2's store evaluator
//! (tables/head.rs `StoreConsts`, check.rs `eval_len`) needs a names source, the lang index
//! (front2 index.rs `Index`, built once a job over the job's lang crates) and a set of prepared
//! definitions (`Prepares`). The names source is a view of the job's records recording into the
//! evaluation's OWN recorder, apart from the freeze's, so what the evaluation read is known
//! apart ([`eval_reads`]). The prepared set is made on the first constant a head asks
//! ([`HeadConsts`]) and is that head's alone: a set shared by two heads would let the second
//! use a definition the first read, and its read list would miss that text (rule 21). The step
//! cap stays the front end's (`MOST_FREEZE_CONSTS` constants a freeze, check.rs `MOST_STEPS`).
//!
//! THE READ LIST OF AN EVALUATED CONSTANT: every names cell the evaluation read, as the
//! freeze's own (engine_heads.rs `names_read`); and for each item whose TEXT it read (a
//! `NameCell::Item` read: the constant's initializer, a const fn's body, an ADT a layout
//! needs), the cell that moves when that text does ([`text_cell`]): the `WHOLE` cell of the
//! outline of the item's file when a member's file holds it (an Outlines cell: no head task
//! reads a cell of its own phase, engine.rs invariant 2), else its crate's names record whole.
//! The same for each constant whose value a frozen cell holds (`CellReads::consts`). The
//! value's own cell (head.rs `const_value_cell`, the constant's head task's) is the floor; it
//! is a Heads cell, so reading it needs the wave rule for heads that read heads (A2), owed.

use blaze_drive::key::crate_scope_name;
use blaze_engine::CellId;
use blaze_id::ItemKey as LibKey;
use blaze_ladder::front2::consts::ConstFail;
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::index::Index;
use blaze_ladder::front2::keys::{Keys, MemberKind, checker_of, member_key_of};
use blaze_ladder::front2::source::{NamesSource, Records};
use blaze_ladder::front2::tables::head::{ConstValues, StoreConsts};
use blaze_ladder::front2::tables::recorder::TableRead;
use blaze_own::Alloc;
use blaze_scope::names_cells::NameCell;
use blaze_scope::prepares::Prepares;
use blaze_scope::resolve::At;

use crate::engine_head_notes::leak;
use crate::engine_heads::Subjects;
use crate::engine_tasks::WHOLE;

/// One head's constant evaluator: front2's store evaluator over a names view of its own, with
/// a prepared set made when the head first asks for a constant.
pub(crate) struct HeadConsts<'r, 'n, S> {
    pub(crate) names: S,
    pub(crate) index: &'r Index<'n>,
    pub(crate) alloc: Alloc<'n>,
    prepared: Option<&'n Prepares<'n>>,
    /// The constants asked of it.
    pub(crate) asked: u32,
}

impl<'r, 'n, S> HeadConsts<'r, 'n, S> {
    pub(crate) fn new(names: S, index: &'r Index<'n>, alloc: Alloc<'n>) -> HeadConsts<'r, 'n, S> {
        HeadConsts { names, index, alloc, prepared: None, asked: 0 }
    }
}

impl<'n, S: NamesSource<'n>> ConstValues<'n> for HeadConsts<'_, 'n, S> {
    fn eval(&mut self, store: &mut Store<'n>, keys: &mut Keys, c: LibKey) -> Result<(), ConstFail> {
        self.asked = self.asked.saturating_add(1);
        let alloc = self.alloc;
        let prepared = *self.prepared.get_or_insert_with(|| Prepares::new_in(alloc));
        StoreConsts { names: self.names, index: self.index, prepared }.eval(store, keys, c)
    }
}

/// Why an evaluation's read could not be placed (rule 21: a read no edit can move is refused).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unplaced;

/// Each cell that moves when a text the evaluation read moves (module docs), given to `each`.
pub(crate) fn eval_reads<'n>(records: &'n Records<'n>, subjects: &Subjects, reads: &[TableRead], each: &mut dyn FnMut(CellId)) -> Result<(), Unplaced> {
    for one in reads.iter() {
        if let TableRead::Names(NameCell::Item { key }) = one {
            each(text_cell(records, subjects, *key).ok_or(Unplaced)?);
        }
    }
    Ok(())
}

/// The cell that moves when item `key`'s text does: its file's outline `WHOLE` when a member's
/// file holds it, else its crate's names record whole (a dependency's text, or one a macro
/// wrote). `None`: its crate is none of `records`'.
pub(crate) fn text_cell<'n>(records: &'n Records<'n>, subjects: &Subjects, key: LibKey) -> Option<CellId> {
    let k = NamesSource::crate_of(records, key)?;
    let krate = NamesSource::crate_key(records, k)?;
    let whole = CellId { of: crate_scope_name(&krate), cell: WHOLE };
    let Some(place) = NamesSource::place_of(records, key) else { return Some(whole) };
    let At::File(file) = place.at else { return Some(whole) };
    let prefix = krate.prefix();
    Some(match subjects.binary_search_by(|(p, f, _)| (*p, *f).cmp(&(prefix, file))) {
        Ok(at) => CellId { of: subjects[at].2, cell: WHOLE },
        Err(_) => whole,
    })
}

/// The head `want` makes, as a finding shows its path (`krate::m::S`; an impl's fn
/// `krate::m::<impl>::f`), kept for the request in `g`. A head of no path is named by its key.
pub(crate) fn head_label<'n>(records: &'n Records<'n>, want: Want, g: Alloc<'static>) -> &'static str {
    let key = want.key();
    let module = NamesSource::home_of(records, key).and_then(|m| NamesSource::module_path(records, m));
    let name = NamesSource::name_of(records, key);
    let member = match want {
        Want::MemberFn(lib, member) => {
            let mut found = None;
            NamesSource::each_member(records, lib, &mut |m| {
                if let (blaze_surface::ItemKind::Function, Some(n)) = (m.kind, m.name)
                    && member_key_of(checker_of(lib), MemberKind::Fn, n) == member
                {
                    found = Some(n);
                }
            });
            Some(found)
        }
        _ => None,
    };
    match (module, name, member) {
        (Some(module), Some(name), None) => leak(g, format_args!("{module}::{name}")),
        (Some(module), Some(owner), Some(Some(member))) => leak(g, format_args!("{module}::{owner}::{member}")),
        (Some(module), None, Some(Some(member))) => leak(g, format_args!("{module}::<impl>::{member}")),
        _ => leak(g, format_args!("the item of key {key:x}")),
    }
}
