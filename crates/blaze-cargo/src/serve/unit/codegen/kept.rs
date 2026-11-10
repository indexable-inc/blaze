//! What a link keeps of its inputs between builds, and how a build's delta changes it
//! (docs/v2/plan/design/patch-in-place.md): a row for each function and static, the id of
//! each span table, and one digest over all of them, each updated for what the build changed
//! and nothing else. No IO and no clock: [`super::relink`] drives it.

#[cfg(test)]
mod tests;

use std::{cell::Cell, collections::BTreeMap, fmt};

use blaze_codegen::{
    BuiltId, CompiledFunction, InstanceKey, Static, StaticId, driver::Built,
};
use blaze_ids::{ContentId, Hasher, keyed::Keyed};
use hir_ty_codegen::v2::{LinkInputs, span_table_id};

use super::{FunctionRow, StaticRow, filled_id, function_rows, static_row, static_rows};

const ENTRY_DOMAIN: &str = "blaze v2 link given entry 1";
const GIVEN_DOMAIN: &str = "blaze v2 link given 2";

/// The kinds of entry the digest holds.
const FUNCTION: u8 = 0;
const STATIC: u8 = 1;
const SPANS: u8 = 2;

const NO_LINK: &str = "the image has no link yet";
const FOLDED: &str = "the build holds folded groups' bodies, which are of no walked function and \
                      in no delta";
const OTHER_BASE: &str = "the image's last link was of another build than this build's base (a \
                          link failed since, or another image took the builds between)";
const NO_BASE: &str = "the build has no base";

/// What bringing a link's rows to a build did, counted: after an edit each follows the edit.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Work {
    /// Functions and statics hashed whole for their rows.
    pub(crate) rows: usize,
    /// The bytes of code those functions hold.
    pub(crate) row_bytes: u64,
    /// Span tables hashed here (a table the front end gave with its id is not).
    pub(crate) tables: usize,
    /// The bytes the digest hashed: 64 a leaf or a node.
    pub(crate) digest_bytes: u64,
    /// Why every row was made again, when that is so.
    pub(crate) whole: Option<&'static str>,
}

/// What a link's image holds, as rows: the other side of a build's delta.
///
/// The invariant: `functions` has the row ([`filled_id`]) of each function of the build
/// `built`, `statics` of each static, `spans` the id of each span table of the snapshot
/// `inputs`; and `digest` holds exactly those, each under its kind and key.
#[derive(Default)]
pub(crate) struct Rows {
    /// The build these are the rows of; `None` before a link.
    pub(crate) built: Option<BuiltId>,
    /// The snapshot of span tables held ([`LinkInputs::serial`]); 0 before a link.
    pub(crate) inputs: u64,
    // BTreeMap: each a build's rows by key, read in order by a link made whole.
    pub(crate) functions: BTreeMap<InstanceKey, ContentId>,
    pub(crate) statics: BTreeMap<StaticId, ContentId>,
    pub(crate) spans: BTreeMap<ContentId, ContentId>,
    pub(crate) digest: Keyed,
    /// The planted bug of `the_oracle_catches_a_row_not_refreshed_for_a_caller`: a function
    /// the delta lists as changed keeps its row.
    #[cfg(test)]
    pub(crate) plant_skips_changed: bool,
    /// The planted bug of `the_oracle_catches_a_given_id_not_updated_for_a_removed_function`:
    /// a removed function's row goes and its digest entry stays.
    #[cfg(test)]
    pub(crate) plant_keeps_removed: bool,
}

/// What a build changed of a link's rows: what the linker's change is made of, and how to
/// take it back if the image refuses the change.
#[derive(Default)]
pub(crate) struct Applied {
    /// The functions whose row is gone or other, and those whose row is new or other, each
    /// sorted: a function filled another way is in both.
    pub(crate) removed: Vec<InstanceKey>,
    pub(crate) added: Vec<InstanceKey>,
    pub(crate) gone_statics: Vec<StaticId>,
    pub(crate) new_statics: Vec<StaticId>,
    /// Whether any span table is new, other or gone.
    pub(crate) spans: bool,
    /// What [`Rows::undo`] takes to put the rows back as they were.
    pub(crate) undo: Undo,
}

/// What [`Rows::undo`] puts back.
#[derive(Default)]
pub(crate) struct Undo {
    built: Option<BuiltId>,
    inputs: u64,
    /// The rows held nothing before: they hold nothing again.
    first: bool,
    functions: Vec<(InstanceKey, Option<ContentId>)>,
    statics: Vec<(StaticId, Option<ContentId>)>,
    spans: Vec<(ContentId, Option<ContentId>)>,
}

/// Which span tables a link is given.
pub(crate) enum Spans<'a> {
    /// The snapshot the rows hold.
    Same,
    /// What differs from the snapshot the rows hold.
    Changed(&'a LinkInputs),
    /// Every table of the snapshot `serial`, sorted by anchor, each with its id where the
    /// front end gave one.
    All { serial: u64, tables: Vec<(ContentId, Option<ContentId>, &'a [u8])> },
}

/// One of a build's lists that a link searches by key: [`Built::functions`] and
/// [`Built::statics`] here, [`Built::debug_names`] and [`Built::cold`] in [`super::relink`].
///
/// The precondition: the list is in key order. `blaze_codegen`'s driver gives each so:
/// `functions` is sorted by key as dedupe ends (`dedupe.rs`, `functions.sort_by_key`),
/// `statics` by item and instance where a build finishes (`driver.rs`,
/// `out.statics.sort_by_key`), and `debug_names` and `cold` are made there by iterating the
/// walk's nodes, a `Sorted` map whose entries are in key order (`Sorted::of`, kept so by
/// `Sorted::rekey`). It is checked, never assumed: asserted where the list is taken, in a
/// build with debug assertions, and in any build a search that finds nothing checks the
/// order once before it says the key is not there, so a list out of order is an error naming
/// the list and the key, never a miss taken for an absent key (rule no-fallback).
pub(crate) struct Listed<'a, T, F> {
    list: &'a [T],
    /// The list's name, as the error says it.
    named: &'static str,
    key: F,
    /// A search that found nothing found the list in order: not checked again.
    ordered: Cell<bool>,
}

impl<'a, T, K, F> Listed<'a, T, F>
where
    K: Ord + fmt::Debug,
    F: Fn(&T) -> K,
{
    /// `list`, a build's list named `named`, each entry's key `key` of it.
    pub(crate) fn new(list: &'a [T], named: &'static str, key: F) -> Self {
        debug_assert!(list.is_sorted_by_key(&key), "a build's `{named}` is in key order");
        Listed::unasserted(list, named, key)
    }

    /// [`Listed::new`] with no assertion: what a build without debug assertions runs, apart
    /// so a test gives it a list out of order.
    fn unasserted(list: &'a [T], named: &'static str, key: F) -> Self {
        Listed { list, named, key, ordered: Cell::new(false) }
    }

    /// The entry of `key`; `None` when the list, in key order, has none. An error when it
    /// has none found and is not in key order, naming the list and the key.
    pub(crate) fn find(&self, key: &K) -> Result<Option<&'a T>, String> {
        if let Ok(at) = self.list.binary_search_by(|it| (self.key)(it).cmp(key)) {
            return Ok(Some(&self.list[at]));
        }
        if !self.ordered.get() {
            if !self.list.is_sorted_by_key(&self.key) {
                // alloc: error path
                return Err(format!(
                    "the build's `{}` is not in key order, so a search of it for {key:?} \
                     proves nothing: a bug in blaze-codegen's driver, which makes the list",
                    self.named
                ));
            }
            self.ordered.set(true);
        }
        Ok(None)
    }

    /// The entry of `key`, which the list is known to hold: an error naming the list and the
    /// key when it does not.
    pub(crate) fn need(&self, key: &K) -> Result<&'a T, String> {
        self.find(key)?.ok_or_else(|| {
            // alloc: error path
            format!(
                "the build's `{}` has no entry of {key:?}, which the link's rows of the build name",
                self.named
            )
        })
    }
}

/// [`Built::functions`] by key ([`Listed`]).
pub(crate) fn functions_of(
    built: &Built,
) -> Listed<'_, CompiledFunction, impl Fn(&CompiledFunction) -> InstanceKey> {
    Listed::new(&built.functions, "functions", |it| it.key)
}

/// [`Built::statics`] by item and instance ([`Listed`]).
pub(crate) fn statics_of(built: &Built) -> Listed<'_, Static, impl Fn(&Static) -> StaticId> {
    Listed::new(&built.statics, "statics", |it| (it.item, it.instance))
}

fn entry_key(kind: u8, parts: &[&[u8; 32]]) -> [u8; 32] {
    let mut hasher = Hasher::new(ENTRY_DOMAIN);
    hasher.update(&[kind]);
    for part in parts {
        hasher.update(*part);
    }
    hasher.finish().0
}

impl Rows {
    /// Whether the rows can take `built` as its delta (`Ok(false)`), are of `built` already
    /// (`Ok(true)`), or have to be made again from its whole lists, and why.
    pub(crate) fn takes(&self, built: &Built) -> Result<bool, &'static str> {
        match (self.built, built.delta.base) {
            (None, _) => Err(NO_LINK),
            (Some(mine), _) if mine == built.id => Ok(true),
            // A folded group's body is a function of the build and of no walked instance: the
            // delta lists walked instances, so a change of one is in none of its lists.
            (Some(_), _) if built.identities.len() != built.inputs.len() => Err(FOLDED),
            (Some(mine), Some(base)) if mine == base => Ok(false),
            (Some(_), Some(_)) => Err(OTHER_BASE),
            (Some(_), None) => Err(built.delta.fresh.map_or(NO_BASE, |it| it.why())),
        }
    }

    /// Which span tables `inputs` gives rows that hold the snapshot `self.inputs`: `None`
    /// when the rows hold neither the snapshot nor its base and `inputs` is not every table,
    /// so the caller has to ask for them all.
    pub(crate) fn spans_of<'a>(&self, inputs: &'a LinkInputs) -> Option<Spans<'a>> {
        if inputs.serial == self.inputs {
            return Some(Spans::Same);
        }
        if self.inputs != 0 && inputs.base == Some(self.inputs) {
            return Some(Spans::Changed(inputs));
        }
        if inputs.base.is_none() && inputs.work.whole.is_some() {
            // alloc: per link given every table, a row a table
            let all = inputs.tables.iter().map(|it| (it.anchor, Some(it.id), &it.bytes[..]));
            return Some(Spans::All { serial: inputs.serial, tables: all.collect() });
        }
        None
    }

    fn set_digest(&mut self, key: [u8; 32], row: Option<ContentId>) {
        match row {
            Some(row) => self.digest.set(key, row.0),
            None => self.digest.remove(&key),
        };
    }

    fn set_function(&mut self, key: InstanceKey, row: Option<ContentId>, applied: &mut Applied) {
        let was = match row {
            Some(row) => self.functions.insert(key, row),
            None => self.functions.remove(&key),
        };
        if was == row {
            return;
        }
        applied.undo.functions.push((key, was));
        if was.is_some() {
            applied.removed.push(key);
        }
        if row.is_some() {
            applied.added.push(key);
        }
        #[cfg(test)]
        {
            if self.plant_keeps_removed && row.is_none() {
                return;
            }
        }
        self.set_digest(entry_key(FUNCTION, &[&key.0.0]), row);
    }

    fn set_static(&mut self, id: StaticId, row: Option<ContentId>, applied: &mut Applied) {
        let was = match row {
            Some(row) => self.statics.insert(id, row),
            None => self.statics.remove(&id),
        };
        if was == row {
            return;
        }
        applied.undo.statics.push((id, was));
        if was.is_some() {
            applied.gone_statics.push(id);
        }
        if row.is_some() {
            applied.new_statics.push(id);
        }
        self.set_digest(entry_key(STATIC, &[&id.0.0.0, &id.1.0]), row);
    }

    fn set_spans(&mut self, anchor: ContentId, id: Option<ContentId>, applied: &mut Applied) {
        let was = match id {
            Some(id) => self.spans.insert(anchor, id),
            None => self.spans.remove(&anchor),
        };
        if was == id {
            return;
        }
        applied.undo.spans.push((anchor, was));
        applied.spans = true;
        self.set_digest(entry_key(SPANS, &[&anchor.0]), id);
    }

    /// Brings the rows to `built` and `spans`: by the build's delta where [`Rows::takes`] it,
    /// else from its whole lists (`work.whole` says why). What changed is returned, with what
    /// [`Rows::undo`] needs to take it back. An error (a static that does not encode, a
    /// function filled two ways) leaves the rows as they were.
    pub(crate) fn apply(
        &mut self,
        built: &Built,
        spans: Spans<'_>,
        work: &mut Work,
    ) -> Result<Applied, String> {
        let hashed = self.digest.hashed();
        let mut applied = Applied::default();
        applied.undo.built = self.built;
        applied.undo.inputs = self.inputs;
        let nothing = self.functions.is_empty() && self.statics.is_empty() && self.spans.is_empty();
        let spans = match (spans, self.takes(built)) {
            // An image's first link: every entry is new, so the digest is made of them all
            // at once, two hashes an entry, never an entry at a time.
            (Spans::All { serial, tables }, Err(why)) if nothing => {
                work.whole = Some(why);
                self.apply_first(built, serial, tables, work, &mut applied)?;
                return Ok(applied);
            }
            (spans, _) => spans,
        };
        let made = match self.takes(built) {
            Ok(true) => Ok(()),
            Ok(false) => self.apply_delta(built, work, &mut applied),
            Err(why) => {
                work.whole = Some(why);
                self.apply_whole(built, work, &mut applied)
            }
        };
        if let Err(what) = made {
            self.undo(applied.undo);
            return Err(what);
        }
        self.built = Some(built.id);
        match spans {
            Spans::Same => {}
            Spans::Changed(inputs) => {
                for anchor in &inputs.removed {
                    self.set_spans(*anchor, None, &mut applied);
                }
                for table in &inputs.tables {
                    self.set_spans(table.anchor, Some(table.id), &mut applied);
                }
                self.inputs = inputs.serial;
            }
            Spans::All { serial, tables } => {
                // alloc: per link given every table, the anchors gone
                let gone: Vec<ContentId> = self
                    .spans
                    .keys()
                    .filter(|it| tables.binary_search_by(|row| row.0.cmp(*it)).is_err())
                    .copied()
                    .collect();
                for anchor in gone {
                    self.set_spans(anchor, None, &mut applied);
                }
                for (anchor, id, bytes) in tables {
                    let id = id.unwrap_or_else(|| {
                        work.tables += 1;
                        span_table_id(bytes)
                    });
                    self.set_spans(anchor, Some(id), &mut applied);
                }
                self.inputs = serial;
            }
        }
        applied.removed.sort_unstable();
        applied.added.sort_unstable();
        applied.gone_statics.sort_unstable();
        applied.new_statics.sort_unstable();
        work.digest_bytes += self.digest.hashed() - hashed;
        Ok(applied)
    }

    /// The rows of `built` and `tables` into rows that hold nothing: each hashed, and the
    /// digest made of them whole. An error leaves the rows empty.
    fn apply_first(
        &mut self,
        built: &Built,
        serial: u64,
        tables: Vec<(ContentId, Option<ContentId>, &[u8])>,
        work: &mut Work,
        applied: &mut Applied,
    ) -> Result<(), String> {
        let (functions, _, _) = function_rows(&[], &built.functions, &built.debug_names)?;
        let (statics, _, _) = static_rows(&[], &built.statics)?;
        work.rows += built.functions.len() + built.statics.len();
        work.row_bytes += built.functions.iter().map(|it| it.code.len() as u64).sum::<u64>();
        // alloc: per image's first link, an entry a row, freed once the digest is made
        let mut entries = Vec::with_capacity(functions.len() + statics.len() + tables.len());
        for (key, row) in functions {
            if self.functions.insert(key, row).is_none() {
                entries.push((entry_key(FUNCTION, &[&key.0.0]), row.0));
                applied.added.push(key);
            }
        }
        for (item, instance, row) in statics {
            if self.statics.insert((item, instance), row).is_none() {
                entries.push((entry_key(STATIC, &[&item.0.0, &instance.0]), row.0));
                applied.new_statics.push((item, instance));
            }
        }
        for (anchor, id, bytes) in tables {
            let id = id.unwrap_or_else(|| {
                work.tables += 1;
                span_table_id(bytes)
            });
            if self.spans.insert(anchor, id).is_none() {
                entries.push((entry_key(SPANS, &[&anchor.0]), id.0));
            }
        }
        applied.spans = !self.spans.is_empty();
        applied.undo.first = true;
        self.digest = Keyed::of(entries);
        work.digest_bytes += self.digest.hashed();
        self.built = Some(built.id);
        self.inputs = serial;
        Ok(())
    }

    /// The rows of the functions and statics `built`'s delta lists, each hashed; no other
    /// function or static of the build is read: each is found by a search of its list, whose
    /// key order is checked ([`Listed`]). An error (a list out of order, a static that does
    /// not encode) is [`Rows::apply`]'s, which takes back what was set.
    fn apply_delta(&mut self, built: &Built, work: &mut Work, applied: &mut Applied) -> Result<(), String> {
        let delta = &built.delta;
        for key in &delta.functions.removed {
            self.set_function(*key, None, applied);
        }
        #[cfg(test)]
        let changed: &[InstanceKey] = match self.plant_skips_changed {
            true => &[],
            false => &delta.functions.changed,
        };
        #[cfg(not(test))]
        let changed: &[InstanceKey] = &delta.functions.changed;
        let functions = functions_of(built);
        for key in delta.functions.added.iter().chain(changed) {
            // One dedupe merged away is a function of the walk with no code of its own.
            let row = match functions.find(key)? {
                Some(function) => {
                    work.rows += 1;
                    work.row_bytes += function.code.len() as u64;
                    Some(filled_id(function))
                }
                None => None,
            };
            self.set_function(*key, row, applied);
        }
        for id in &delta.statics.removed {
            self.set_static(*id, None, applied);
        }
        let statics = statics_of(built);
        for id in delta.statics.added.iter().chain(&delta.statics.changed) {
            let row = match statics.find(id)? {
                Some(it) => {
                    work.rows += 1;
                    Some(static_row(it)?.2)
                }
                None => None,
            };
            self.set_static(*id, row, applied);
        }
        Ok(())
    }

    /// Every row made from `built`'s whole lists, and what of the rows held is gone and new.
    fn apply_whole(&mut self, built: &Built, work: &mut Work, applied: &mut Applied) -> Result<(), String> {
        // alloc: per link made whole, the rows held, in order
        let old: Vec<FunctionRow> = self.functions.iter().map(|(key, row)| (*key, *row)).collect();
        let (functions, _, _) = function_rows(&old, &built.functions, &built.debug_names)?;
        work.rows += built.functions.len() + built.statics.len();
        work.row_bytes += built.functions.iter().map(|it| it.code.len() as u64).sum::<u64>();
        // alloc: per link made whole, the statics' rows held, in order
        let old: Vec<StaticRow> =
            self.statics.iter().map(|(id, row)| (id.0, id.1, *row)).collect();
        let (statics, _, _) = static_rows(&old, &built.statics)?;
        // What is gone, then what is new or other: through the same setters as a delta, so
        // the lists and the digest are the ones a delta of the same change gives, and
        // [`Rows::undo`] takes either back the same way.
        // alloc: per link made whole, the keys gone
        let gone: Vec<InstanceKey> = self
            .functions
            .keys()
            .filter(|key| functions.binary_search_by(|row| row.0.cmp(*key)).is_err())
            .copied()
            .collect();
        for key in gone {
            self.set_function(key, None, applied);
        }
        for (key, row) in functions {
            self.set_function(key, Some(row), applied);
        }
        // alloc: per link made whole, the statics gone
        let gone: Vec<StaticId> = self
            .statics
            .keys()
            .filter(|id| statics.binary_search_by(|row| (row.0, row.1).cmp(*id)).is_err())
            .copied()
            .collect();
        for id in gone {
            self.set_static(id, None, applied);
        }
        for (item, instance, row) in statics {
            self.set_static((item, instance), Some(row), applied);
        }
        Ok(())
    }

    /// Takes an [`Rows::apply`] back: the rows are what they were before it, digest and all
    /// (the digest is of its entries alone, never of their order).
    pub(crate) fn undo(&mut self, undo: Undo) {
        let Undo { built, inputs, first, functions, statics, spans } = undo;
        if first {
            *self = Rows::default();
            return;
        }
        let mut scratch = Applied::default();
        for (key, was) in functions.into_iter().rev() {
            self.set_function(key, was, &mut scratch);
        }
        for (id, was) in statics.into_iter().rev() {
            self.set_static(id, was, &mut scratch);
        }
        for (anchor, was) in spans.into_iter().rev() {
            self.set_spans(anchor, was, &mut scratch);
        }
        self.built = built;
        self.inputs = inputs;
    }

    /// What a link of these rows is given, as one id: the build, every row and span table
    /// (the digest's root) and the natives in their order. Two links of one image with one
    /// id stage the same image. It is a sound identity of the link's inputs: the root is a
    /// Merkle hash of every function's, static's and span table's content
    /// ([`blaze_ids::keyed`]), the build's id says the rest (names, debug names, symbols,
    /// which functions are patchable or cold: `BuiltId`), and the natives are hashed here
    /// whole, a few a link.
    pub(crate) fn given(&self, built: BuiltId, natives: &[ContentId]) -> ContentId {
        let mut hasher = Hasher::new(GIVEN_DOMAIN);
        hasher.id(&built.0).update(&self.digest.root());
        hasher.update(&(natives.len() as u64).to_le_bytes());
        for id in natives {
            hasher.id(id);
        }
        hasher.finish()
    }

    /// The bytes the rows hold.
    pub(crate) fn held(&self) -> u64 {
        let rows = self.functions.len() * 64 + self.statics.len() * 96 + self.spans.len() * 64;
        rows as u64 + self.digest.held()
    }
}
