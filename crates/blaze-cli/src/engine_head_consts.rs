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
//! A names lookup records its metadata. A text operation additionally records which tokens
//! it consumed. A member file publishes those tokens under the item's stable chain. A macro
//! or block module publishes its generated text under the expansion that contains the item.

use blaze_drive::key::crate_scope_name;
use blaze_engine::CellId;
use blaze_id::ItemKey as LibKey;
use blaze_ladder::front2::consts::ConstFail;
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::index::{Index, LangTwice};
use blaze_ladder::front2::keys::{Keys, MemberKind, checker_of, member_key_of};
use blaze_ladder::front2::source::{NamesSource, Records};
use blaze_ladder::front2::tables::head::{ConstValues, StoreConsts};
use blaze_ladder::front2::tables::recorder::{MOST_READS, ReadSink, ReadsFull, TableRead};
use blaze_ladder::front2::tables::view::Tables;
use blaze_own::{Alloc, Bump};
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
    pub(crate) bump: Bump<'n>,
    prepared: Option<&'n Prepares<'n>>,
    /// The constants asked of it.
    pub(crate) asked: u32,
}

impl<'r, 'n, S> HeadConsts<'r, 'n, S> {
    pub(crate) fn new(names: S, index: &'r Index<'n>, bump: Bump<'n>) -> HeadConsts<'r, 'n, S> {
        HeadConsts { names, index, bump, prepared: None, asked: 0 }
    }
}

impl<'n, S: NamesSource<'n>> ConstValues<'n> for HeadConsts<'_, 'n, S> {
    fn eval(&mut self, store: &mut Store<'n>, keys: &mut Keys, c: LibKey) -> Result<(), ConstFail> {
        self.asked = self.asked.saturating_add(1);
        let bump = self.bump;
        let prepared = *self.prepared.get_or_insert_with(|| Prepares::new_in(bump));
        StoreConsts { names: self.names, index: self.index, prepared }.eval(store, keys, c)
    }
}

/// The lang index a freeze's constant evaluator reads (front2 index.rs `Index`), over the view
/// `tables` with its reads into `reads`: the one home of the evaluator's index, which a head
/// task (engine_heads.rs `heads_of`, once a job) and a body stretch's seed freezer
/// (engine_bodies.rs `run_stretch`, once a stretch) both build by it, each then evaluating
/// through [`HeadConsts`].
pub(crate) fn lang_index<'n>(tables: Tables<'_, 'n>, reads: &dyn ReadSink, lang: &[usize]) -> Result<Index<'n>, LangTwice> {
    Index::from_source(tables.reading_into(reads), lang)
}

/// What a body stretch's evaluator read (engine_bodies.rs `run_stretch`), kept as it grows so
/// each body's read list can take what was read up to its end: a stretch's [`HeadConsts`]
/// serves every body of it, and its prepared set is the stretch's, so a body takes every read
/// the evaluator made until then (a superset of its own: never a read missed, rule 21). At
/// most [`MOST_READS`] reads (rule 18); past it, [`EvalSink::reads`] refuses.
pub(crate) struct EvalSink<'a> {
    reads: core::cell::RefCell<blaze_own::Vec<TableRead, Alloc<'a>>>,
    full: core::cell::Cell<bool>,
    /// The binding reads' cell keys, each derived once (front2 recorder.rs `ReadSink::binding`).
    binding_keys: core::cell::RefCell<blaze_scope::cell_key::BindingKeys<'a>>,
}

impl<'a> EvalSink<'a> {
    pub(crate) fn new(alloc: Alloc<'a>) -> EvalSink<'a> {
        EvalSink { reads: core::cell::RefCell::new(blaze_own::Vec::new_in(alloc)), full: core::cell::Cell::new(false), binding_keys: core::cell::RefCell::new(blaze_scope::cell_key::BindingKeys::new_in(alloc)) }
    }

    /// How many reads it holds.
    pub(crate) fn len(&self) -> usize {
        self.reads.borrow().len()
    }

    /// Each read it holds, in order, or [`ReadsFull`] past its cap.
    pub(crate) fn reads(&self, each: &mut dyn FnMut(TableRead)) -> Result<(), ReadsFull> {
        if self.full.get() {
            return Err(ReadsFull { most: MOST_READS });
        }
        for one in self.reads.borrow().iter() {
            each(*one);
        }
        Ok(())
    }
}

impl ReadSink for EvalSink<'_> {
    fn read(&self, read: TableRead) {
        let mut reads = self.reads.borrow_mut();
        // The same read again (an evaluation reads one cell many times): kept once in a row.
        if reads.last() == Some(&read) {
            return;
        }
        if reads.len() >= MOST_READS {
            self.full.set(true);
            return;
        }
        reads.push(read);
    }

    fn binding(&self, module: LibKey, ns: blaze_surface::Namespace, name: &str) {
        let key = self.binding_keys.borrow_mut().key(module, ns, name).key;
        self.read(TableRead::Names(NameCell::Binding { module, name: key }));
    }
}

/// Why an evaluation's read of item `key`'s text could not be placed (rule 21: a read no edit
/// can move is refused, rule 19: naming the item).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unplaced {
    pub(crate) key: LibKey,
    pub(crate) why: UnplacedWhy,
}

/// Which step of [`text_cell`] found no crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnplacedWhy {
    /// Its key's crate prefix (its first 8 bytes) is no crate's of the records: an item of a
    /// crate outside the scope (a proc-macro crate, another unit's), or a key made with no
    /// crate's prefix.
    NoCrate,
    /// Its crate is the records' crate `at`, which has no crate key.
    NoCrateKey { at: usize },
    Record { what: &'static str },
    NoOutline { file: u32 },
}

/// [`Unplaced`] with the names of the scope's crates it was looked for in: what a refusal
/// prints (`crates` cut to the first [`MOST_NAMED_CRATES`]).
pub(crate) struct UnplacedIn<'u, 'c> {
    pub(crate) unplaced: &'u Unplaced,
    pub(crate) crates: &'c [crate::run_names::Crate<'c>],
}

/// The most scope crates an [`UnplacedIn`] names; past it, their count.
const MOST_NAMED_CRATES: usize = 8;

impl core::fmt::Display for UnplacedIn<'_, '_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let key = self.unplaced.key;
        let prefix = u64::from_be_bytes(key.crate_prefix());
        match self.unplaced.why {
            UnplacedWhy::NoCrate => write!(f, "the item of key {key:x} (crate prefix {prefix:016x}) is of no crate of its scope's {} crate(s)", self.crates.len())?,
            UnplacedWhy::NoCrateKey { at } => write!(f, "the item of key {key:x} is of its scope's crate {at}, which has no crate key")?,
            UnplacedWhy::Record { what } => write!(f, "cannot place the text of item {key:x}: {what}")?,
            UnplacedWhy::NoOutline { file } => write!(f, "item {key:x} is in member file {file}, which has no matched checkout outline")?,
        }
        for (i, one) in self.crates.iter().take(MOST_NAMED_CRATES).enumerate() {
            let sep = if i == 0 { ": " } else { ", " };
            write!(f, "{sep}`{}` ({:016x})", one.name, u64::from_be_bytes(one.records.key.prefix()))?;
        }
        if self.crates.len() > MOST_NAMED_CRATES {
            write!(f, " and {} more", self.crates.len() - MOST_NAMED_CRATES)?;
        }
        f.write_str(" (engine_head_consts.rs `text_cell`)")
    }
}

/// Each cell that moves when a text the evaluation read moves (module docs), given to `each`.
pub(crate) fn eval_reads<'n>(records: &'n Records<'n>, subjects: &Subjects, reads: &[TableRead], each: &mut dyn FnMut(CellId)) -> Result<(), Unplaced> {
    for one in reads.iter() {
        if let Some(cell) = source_read(records, subjects, *one)? {
            each(cell);
        }
    }
    Ok(())
}

/// The source facet accompanying a table observation. Its metadata cell is placed by
/// `tables::task::cell_id`, including when the lookup found no definition.
pub(crate) fn source_read<'n>(records: &'n Records<'n>, subjects: &Subjects, read: TableRead) -> Result<Option<CellId>, Unplaced> {
    match read {
        TableRead::Text { key } => text_cell(records, subjects, key),
        TableRead::Signature { key } => signature_text_cell(records, subjects, key),
        _ => Ok(None),
    }
}

/// The item's full definition, including an initializer or const fn body.
pub(crate) fn text_cell<'n>(records: &'n Records<'n>, subjects: &Subjects, key: LibKey) -> Result<Option<CellId>, Unplaced> {
    text_cell_of(records, subjects, key, crate::engine_tasks::definition_cell)
}

pub(crate) fn signature_text_cell<'n>(records: &'n Records<'n>, subjects: &Subjects, key: LibKey) -> Result<Option<CellId>, Unplaced> {
    text_cell_of(records, subjects, key, crate::engine_tasks::signature_cell)
}

/// Whether `key` is a builtin type's (its first eight bytes are blaze-scope's `BUILTIN_PREFIX`,
/// where an item's key holds its crate's prefix): an item with no text ([`text_cell`]).
pub(crate) fn has_no_text(key: LibKey) -> bool {
    key.0.0[..8] == blaze_scope::BUILTIN_PREFIX
}

fn text_cell_of<'n>(
    records: &'n Records<'n>, subjects: &Subjects, key: LibKey, facet: fn(blaze_drive::Name) -> blaze_drive::cells::Cell,
) -> Result<Option<CellId>, Unplaced> {
    if has_no_text(key) {
        return Ok(None);
    }
    let k = NamesSource::crate_of(records, key).ok_or(Unplaced { key, why: UnplacedWhy::NoCrate })?;
    let krate = NamesSource::crate_key(records, k).ok_or(Unplaced { key, why: UnplacedWhy::NoCrateKey { at: k } })?;
    let scope = crate_scope_name(&krate);
    let prefix = krate.prefix();
    let first = subjects.partition_point(|row| row.prefix < prefix);
    if subjects.get(first).is_none_or(|row| row.prefix != prefix) {
        return Ok(Some(CellId { of: scope, cell: WHOLE }));
    }
    let bad = |what| Unplaced { key, why: UnplacedWhy::Record { what } };
    let items = records.items_record(k).ok_or_else(|| bad("no items record"))?;
    let (at, owner, member) = if let Some(row) = items.item(key).map_err(|_| bad("unreadable item row"))? {
        (row.text.ok_or_else(|| bad("item row has no text span"))?.at, key, None)
    } else if let Some((owner, _)) = items.member_owner(key).map_err(|_| bad("unreadable member owner"))? {
        let rows = items.members(owner).map_err(|_| bad("unreadable member rows"))?.ok_or_else(|| bad("owner has no member rows"))?;
        let mut found = None;
        for row in rows.iter() {
            let row = row.map_err(|_| bad("unreadable member row"))?;
            if row.key == key {
                let name = row.name.ok_or_else(|| bad("member owner names an unnamed member"))?;
                let at = row.text.ok_or_else(|| bad("member has no text span"))?.at;
                let placement = (at, owner, Some((row.kind, name)));
                if found.replace(placement).is_some_and(|previous| previous != placement) {
                    return Err(bad("the same member key has definitions at different sources"));
                }
            }
        }
        found.ok_or_else(|| bad("the owner does not list the member"))?
    } else {
        // The operation records Item as well. For an absent item this second negative
        // records the other place its text could appear, the associated-member table.
        let absent = NameCell::MemberOf { member: key };
        return Ok(Some(CellId { of: scope, cell: blaze_drive::cells::Cell(absent.key()) }));
    };
    let file = match at {
        At::Expansion(call) => {
            if items.expansion(call).is_none() {
                return Err(bad("the item's generating expansion has no text row"));
            }
            return Ok(Some(CellId { of: scope, cell: blaze_drive::cells::Cell(NameCell::Expansion { call }.key()) }));
        }
        At::File(file) => file,
    };
    let row = subjects.binary_search_by(|row| (row.prefix, row.file).cmp(&(prefix, file)))
        .map_err(|_| Unplaced { key, why: UnplacedWhy::NoOutline { file } })?;
    let subject = &subjects[row];
    let pieces = items.pieces(owner).map_err(|_| bad("unreadable item chain"))?.ok_or_else(|| bad("missing item chain"))?;
    let entry = subject.signature_file.entry_from_chain(pieces.chain);
    let name = match member {
        Some((kind, name)) => crate::engine_tasks::member_entry(entry, kind, name),
        None => entry,
    };
    Ok(Some(CellId { of: subject.slot, cell: facet(name) }))
}

/// The head `want` makes, as a finding shows its path (`krate::m::S`; an impl's fn
/// `krate::m::<impl>::f`), kept for the request in `g`. A head of no path is named by where it
/// is written (front2 item_shown.rs `ItemShown`), or as an item declared in a body; never by a
/// bare key (rule 19: the clap and serde projects printed `the item of key e459..`).
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
        // An item declared in a body (its key's `INNER` bit: front2 locals.rs): the names records
        // hold no row of it, so it has no path; said as what it is, never as a bare key (rule 19).
        _ if key.is_inner() => leak(g, format_args!("an item declared in a body, which the names records hold no row of (its key is its body's lowering's, front2 locals.rs: {})", blaze_ladder::front2::item_shown::ItemShown::of_lib(records, key))),
        // A path-less item the names do hold (an impl): where it is written, by front2
        // item_shown.rs (its module, its file or the macro call that wrote it), then its key.
        _ => leak(g, format_args!("{}", blaze_ladder::front2::item_shown::ItemShown::of_lib(records, key))),
    }
}
