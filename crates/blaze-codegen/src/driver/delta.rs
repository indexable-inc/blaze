//! What a build changed of its program since the build before it: [`Built`] is a whole value,
//! and [`Delta`] says which of its functions and statics are other than in the last `Built`
//! of the same program, so a consumer that holds state for that one (a link, an image) patches
//! it and reads nothing of the rest (docs/v2/plan/design/incremental-walk.md).
//!
//! Each function and static of a `Built` has an *entry*: one content ID of everything the
//! `Built` says of it ([`function_entries`], [`static_entries`]). A function or static is
//! *changed* when its entry is another. A `Built`'s [`BuiltId`] is made of the roots of two
//! keyed Merkle sets of its entries ([`blaze_ids::keyed`]), so a build that knows the entries
//! it added, changed and removed makes the new ID from the last build's sets by hashing those
//! entries' paths alone, and gets the ID a build with nothing kept gets.

use std::sync::Arc;

use blaze_ids::{
    ContentId, Hasher,
    keyed::{Hash, Keyed},
};
use blaze_items::CodeHash;

use super::{Built, StaticId};
use crate::{InstanceKey, Linkage, Owner, content, plan::Origin};

pub const FUNCTION_ENTRY_DOMAIN: &str = "blaze v2 built function entry 1";
pub const STATIC_ENTRY_DOMAIN: &str = "blaze v2 built static entry 1";
pub const STATIC_KEY_DOMAIN: &str = "blaze v2 built static key 1";
pub const BUILT_ID_DOMAIN: &str = "blaze v2 built id 2";

/// Which whole [`Built`] one is, by content: a digest of every function's and static's entry
/// and of the lists that are of no one function (the roots, a lazy build's entry, a folded
/// group's identity). Two builds of one ID give a link the same functions, statics, names,
/// owners, symbols and slots.
///
/// It is the hash of two roots, of the keyed set of the function entries and of the keyed set
/// of the static entries ([`Keyed`]: a Merkle tree over the keys' bits, whose shape is of the
/// set alone). A root is a hash of its whole set, so two builds of one ID have the same
/// entries; and it is kept through edits, so the root after an edit is made from the set
/// before by hashing the paths of the entries removed, added and changed. A fresh build makes
/// the set whole and gets the same root. Until 2026-11-10 it was a hash of two wrapping sums
/// of the entries, which is no identity: other entries with the same sum are found by a
/// generalized birthday search (`sets_a_wrapping_sum_takes_for_one_have_other_ids`).
/// Not in it: a lazy build's walk order ([`Built::walked`]) and what a build counts of its
/// compiles and the store (`stats`, `puts`, `memo_used`, `kernel`, `misses`, `walk`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct BuiltId(pub ContentId);

/// The keys of one kind (functions, statics) a build changed since its base. Each list is
/// sorted by key and the three share no key.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: inside `Delta`, once per build
pub struct Changes<K> {
    /// In this `Built` and not in its base.
    pub added: Vec<K>,
    /// In both, with another entry: something the `Built` says of it differs.
    pub changed: Vec<K>,
    /// In the base and not in this `Built`.
    pub removed: Vec<K>,
}

impl<K> Default for Changes<K> {
    fn default() -> Self {
        // alloc: none, the lists are empty
        Changes { added: Vec::new(), changed: Vec::new(), removed: Vec::new() }
    }
}

impl<K> Changes<K> {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }
}

/// Why a [`Built`] has no base, or why its walk took no kept describe
/// ([`super::Walk::fresh`]): what `blaze why` says of a build that did the whole work.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fresh {
    /// The build was given nothing to keep its walk in ([`super::Build::keeping`]).
    Unkept,
    /// Nothing was kept for the front end's database and configuration: the daemon started,
    /// or the database was made again, since the last build. What is kept is memory of one
    /// process and names one database's ids, so it is never restored.
    Restart,
    /// The first build of this program (these roots, check roots, policy and kind of build)
    /// over what is kept: builds of other programs were kept, none of this one.
    First,
    /// A build of this program was kept, and the input named is another than its. Not given
    /// yet: a program is found by a hash of its inputs, so another input is another program
    /// and reads as [`Fresh::First`].
    Inputs(&'static str),
}

impl Fresh {
    /// The reason, as a span's field and `blaze why` say it.
    pub fn why(self) -> &'static str {
        match self {
            Fresh::Unkept => "the build keeps no walk",
            Fresh::Restart => {
                "nothing was kept for this front end's database and settings: the daemon \
                 started or the database was made again since the last build"
            }
            Fresh::First => "the first build of this program since the daemon started",
            Fresh::Inputs(what) => what,
        }
    }
}

/// What a [`Built`] changed since the last `Built` of the same program its build kept.
///
/// With a `base`, the consumer that holds state for the `Built` of that ID applies
/// `functions` and `statics` to it and has this `Built`: every key in neither list is in both
/// with one entry, so everything this `Built` says of it is what the base said. A consumer
/// that holds no state for `base` reads the whole lists of the `Built`, as before.
///
/// With no `base` (`fresh` says why) the lists are empty and the whole `Built` is new.
///
/// What an entry covers, so what "changed" means, for a function: its code hash
/// ([`Built::inputs`]); its compiled function's architecture, alignment, bytes, relocations
/// and their targets, data, unwind information, lines, coverage and profile
/// ([`content::code_id`]), its side table and what it built in, or, merged away by dedupe, the
/// function kept for it; its debug name; its symbol and linkage; whether it is patchable,
/// tiered, cold, editable, a root, in a lazy build's entry; its owner and its generic's code;
/// its identity; the origin it was made for; its version IDs. For a static: its definition
/// ([`super::static_definition`]: value, relocations, data, mutability, section, imports), its
/// symbol and linkage, its owner. [`Built::reach`] and [`Built::items`] are made of these and
/// hold nothing more.
#[derive(Clone, PartialEq, Eq, Debug, Default)] // clone: inside `Built`, once per build
pub struct Delta {
    /// The [`BuiltId`] of the `Built` this is relative to: [`Built::id`] of the last build of
    /// the same program that the build's kept walk remembers.
    pub base: Option<BuiltId>,
    pub functions: Changes<InstanceKey>,
    pub statics: Changes<StaticId>,
    /// Why there is no base.
    pub fresh: Option<Fresh>,
}

/// What a build keeps of its `Built` for the next build of the same program: each entry, by
/// key, the two keyed sets of them, and the ID. The lists themselves are the consumer's.
pub(super) struct Last {
    pub(super) id: BuiltId,
    pub(super) functions: Vec<(InstanceKey, ContentId)>,
    pub(super) statics: Vec<(StaticId, ContentId)>,
    /// The entries of `functions` and of `statics` as keyed sets: their roots make `id`.
    pub(super) sets: Sets,
}

/// The keyed sets of a `Built`'s function entries and static entries.
#[derive(Clone)] // clone: the last build's sets, moved to this build's with no hash made again
pub(super) struct Sets {
    functions: Keyed,
    statics: Keyed,
}

impl Sets {
    /// Sets function `key`'s entry, or takes it out with `None`: the one path of the set
    /// to it is hashed again, and nothing else.
    pub(super) fn function(&mut self, key: &InstanceKey, entry: Option<ContentId>) {
        match entry {
            Some(entry) => self.functions.set(function_key(key), entry.0),
            None => self.functions.remove(&function_key(key)),
        };
    }

    /// As [`Self::function`], of static `id`.
    pub(super) fn static_(&mut self, id: &StaticId, entry: Option<ContentId>) {
        match entry {
            Some(entry) => self.statics.set(static_key(id), entry.0),
            None => self.statics.remove(&static_key(id)),
        };
    }

    /// The bytes hashed into the two sets since they were made.
    pub(super) fn hashed(&self) -> u64 {
        self.functions.hashed() + self.statics.hashed()
    }

    /// The [`BuiltId`] of a `Built` of these entries, these roots and nothing that is of no
    /// function of its walk (no lazy entry, no folded group): what [`id_of`] gives it.
    pub(super) fn id(&self, roots: &[InstanceKey]) -> BuiltId {
        id_from(self, roots, &[], std::iter::empty(), std::iter::empty())
    }
}

/// A function's key in its set: its instance key, a content ID already.
fn function_key(key: &InstanceKey) -> Hash {
    key.0.0
}

/// A static's key in its set: one hash of its item and its instance.
fn static_key(id: &StaticId) -> Hash {
    let mut hasher = Hasher::new(STATIC_KEY_DOMAIN);
    hasher.id(&id.0.0).id(&id.1);
    hasher.finish().0
}

/// The set of `entries`, made whole.
fn whole<K>(entries: &[(K, ContentId)], key: impl Fn(&K) -> Hash) -> Keyed {
    Keyed::of(entries.iter().map(|(of, entry)| (key(of), entry.0)))
}

/// Moves `set`, the set of `before`, to the set of `now` (both sorted by key): the entries
/// `now` lacks removed, the others set. An entry that did not change hashes nothing.
fn moved<K: Copy + Ord>(
    set: &mut Keyed,
    before: &[(K, ContentId)],
    now: &[(K, ContentId)],
    key: impl Fn(&K) -> Hash,
) {
    for (of, _) in before {
        if now.binary_search_by_key(of, |it| it.0).is_err() {
            set.remove(&key(of));
        }
    }
    for (of, entry) in now {
        set.set(key(of), entry.0);
    }
}

impl Last {
    /// Of `built`, whose function entries are `functions` ([`function_entries_and_codes`]),
    /// the build after `before` of the same program (none: the first): its static entries
    /// made, and the sets moved from `before`'s by the entries that differ, or made whole
    /// with none before. Either way the ID is the one a build with nothing kept gets
    /// (`an_id_kept_through_edits_is_a_fresh_builds`).
    pub(super) fn of_entries(
        built: &Built,
        functions: Vec<(InstanceKey, ContentId)>,
        before: Option<&Last>,
    ) -> Last {
        let statics = static_entries(built);
        let sets = match before {
            Some(before) => {
                // clone: the last build's sets, which stay its own (a consumer may hold it)
                let mut sets = before.sets.clone();
                moved(&mut sets.functions, &before.functions, &functions, function_key);
                moved(&mut sets.statics, &before.statics, &statics, static_key);
                sets
            }
            None => Sets {
                functions: whole(&functions, function_key),
                statics: whole(&statics, static_key),
            },
        };
        let id = id_of(built, &sets);
        Last { id, functions, statics, sets }
    }
}

/// The delta from `before`, the last `Built` of the program, to `now`; with none before, a
/// delta of no base that says `fresh`.
pub(super) fn between(before: Option<&Arc<Last>>, now: &Last, fresh: Option<Fresh>) -> Delta {
    match before {
        Some(before) => Delta {
            base: Some(before.id),
            functions: changes(&before.functions, &now.functions),
            statics: changes(&before.statics, &now.statics),
            fresh: None,
        },
        None => Delta { fresh, ..Delta::default() },
    }
}

/// One pass over two lists of entries sorted by key.
fn changes<K: Copy + Ord>(before: &[(K, ContentId)], now: &[(K, ContentId)]) -> Changes<K> {
    let mut made = Changes::default();
    let (mut old, mut new) = (0, 0);
    loop {
        match (before.get(old), now.get(new)) {
            (None, None) => return made,
            (Some((key, _)), None) => {
                made.removed.push(*key);
                old += 1;
            }
            (None, Some((key, _))) => {
                made.added.push(*key);
                new += 1;
            }
            (Some((was, _)), Some((key, _))) if was < key => {
                made.removed.push(*was);
                old += 1;
            }
            (Some((was, _)), Some((key, _))) if key < was => {
                made.added.push(*key);
                new += 1;
            }
            (Some((_, was)), Some((key, entry))) => {
                if was != entry {
                    made.changed.push(*key);
                }
                old += 1;
                new += 1;
            }
        }
    }
}

/// What a `Built` says of one function, gathered from its lists before it is hashed.
#[derive(Clone, Default)]
pub(super) struct Row {
    /// Bits: compiled, has a side table, merged away, has a symbol, weak, patchable, tiered,
    /// cold (0 to 7).
    pub(super) flags: u8,
    /// Bits: editable, a root, in the entry, the workspace's, has an owner, has an identity,
    /// has an origin (0 to 6).
    pub(super) more: u8,
    pub(super) code_id: ContentId,
    pub(super) side: ContentId,
    pub(super) piece: ContentId,
    pub(super) merged: ContentId,
    pub(super) name: ContentId,
    /// Its symbol's place in `Built::symbols`, for a row made from a whole `Built`.
    pub(super) symbol: usize,
    pub(super) generic: ContentId,
    pub(super) identity: ContentId,
    pub(super) origin: Option<Origin>,
}

/// The entry of each function of `built`, in key order ([`Built::inputs`]'s): everything
/// `built` says of it ([`Delta`] lists what).
#[cfg(test)]
pub(crate) fn function_entries(built: &Built) -> Vec<(InstanceKey, ContentId)> {
    function_entries_and_codes(built).0
}

/// [`function_entries`], with the code ID ([`content::code_id`]) of each function the build
/// kept after dedupe, in key order: hashed once for the two.
pub(super) fn function_entries_and_codes(
    built: &Built,
) -> (Vec<(InstanceKey, ContentId)>, Vec<(InstanceKey, ContentId)>) {
    let keys = &built.inputs;
    let at = |key: &InstanceKey| keys.binary_search_by_key(key, |it| it.0).ok();
    // alloc: per build, a row a function, freed with the pass
    let mut rows = vec![Row::default(); keys.len()];
    // alloc: per build, a code ID a function it kept
    let mut codes = Vec::with_capacity(built.functions.len());
    for function in &built.functions {
        let Some(at) = at(&function.key) else { continue };
        rows[at].flags |= 1;
        rows[at].code_id = content::code_id(function);
        codes.push((function.key, rows[at].code_id));
        if let Some(side) = function.side {
            rows[at].flags |= 1 << 1;
            rows[at].side = side;
        }
    }
    for (key, piece) in &built.digests.functions {
        if let Some(at) = at(key) {
            rows[at].piece = *piece;
        }
    }
    for (key, kept) in &built.deduped.merged {
        if let Some(at) = at(key) {
            rows[at].flags |= 1 << 2;
            rows[at].merged = kept.0;
        }
    }
    for (key, name) in &built.digests.debug_names {
        if let Some(at) = at(key) {
            rows[at].name = *name;
        }
    }
    for (index, (key, _, linkage)) in built.symbols.iter().enumerate() {
        if let Some(at) = at(key) {
            rows[at].flags |= 1 << 3;
            rows[at].symbol = index;
            if *linkage == Linkage::Weak {
                rows[at].flags |= 1 << 4;
            }
        }
    }
    let mut flag = |keys: &[InstanceKey], set: fn(&mut Row)| {
        for key in keys {
            if let Some(at) = at(key) {
                set(&mut rows[at]);
            }
        }
    };
    flag(&built.patchable, |it| it.flags |= 1 << 5);
    flag(&built.tiered, |it| it.flags |= 1 << 6);
    flag(&built.cold, |it| it.flags |= 1 << 7);
    flag(&built.editable, |it| it.more |= 1);
    flag(&built.roots, |it| it.more |= 1 << 1);
    flag(&built.entry, |it| it.more |= 1 << 2);
    for (key, owner, generic) in &built.owners {
        if let Some(at) = at(key) {
            rows[at].more |= 1 << 4;
            if *owner == Owner::Workspace {
                rows[at].more |= 1 << 3;
            }
            rows[at].generic = generic.0;
        }
    }
    for (key, identity) in &built.identities {
        if let Some(at) = at(key) {
            rows[at].more |= 1 << 5;
            rows[at].identity = *identity;
        }
    }
    for (key, made) in &built.made {
        if let Some(at) = at(key) {
            rows[at].more |= 1 << 6;
            rows[at].origin = Some(made.origin);
        }
    }
    // alloc: per build, an entry a function, kept for the next build's delta
    let mut entries = Vec::with_capacity(keys.len());
    for (index, ((key, code), row)) in keys.iter().zip(&rows).enumerate() {
        let symbol = (row.flags & (1 << 3) != 0).then(|| built.symbols[row.symbol].1.as_str());
        entries.push((*key, function_entry(*key, *code, row, symbol, built.versions.get(index))));
    }
    (entries, codes)
}

/// The entry of function `key` made from `code`: `row` gathered of it, its symbol where it
/// has one (bit 3 of `row.flags`) and its version IDs. The one hash of a function's entry: a
/// whole `Built`'s ([`function_entries`]) and a patched one's (`lists`) are made here.
pub(super) fn function_entry(
    key: InstanceKey,
    code: CodeHash,
    row: &Row,
    symbol: Option<&str>,
    version: Option<&(ContentId, Result<ContentId, String>)>,
) -> ContentId {
    let mut hasher = Hasher::new(FUNCTION_ENTRY_DOMAIN);
    hasher.id(&key.0).id(&code.0).update(&[row.flags, row.more]);
    hasher.id(&row.code_id).id(&row.side).id(&row.piece).id(&row.merged).id(&row.name);
    if let Some(symbol) = symbol {
        hasher.part(symbol.as_bytes());
    }
    hasher.id(&row.generic).id(&row.identity);
    if let Some(origin) = row.origin {
        origin.hash_into(&mut hasher);
    }
    match version {
        Some((version, Ok(bound))) => hasher.update(&[1]).id(version).id(bound),
        Some((version, Err(why))) => hasher.update(&[2]).id(version).part(why.as_bytes()),
        None => hasher.update(&[0]),
    };
    hasher.finish()
}

/// The entry of static `id`: whose it is, its definition's digest where it was compiled, and
/// each symbol it is defined as with whether weakly. As [`function_entry`], the one hash.
pub(super) fn static_entry<'s>(
    id: &StaticId,
    owner: Owner,
    digest: Option<ContentId>,
    symbols: impl Iterator<Item = (&'s str, Linkage)>,
) -> ContentId {
    let mut hasher = Hasher::new(STATIC_ENTRY_DOMAIN);
    hasher.id(&id.0.0).id(&id.1).update(&[u8::from(owner == Owner::Workspace)]);
    match digest {
        Some(digest) => hasher.update(&[1]).id(&digest),
        None => hasher.update(&[0]),
    };
    for (symbol, linkage) in symbols {
        hasher.part(symbol.as_bytes()).update(&[u8::from(linkage == Linkage::Weak)]);
    }
    hasher.finish()
}

/// The entry of each static of `built`, by item and instance ([`Built::static_owners`]'s
/// order, which lists every static of the walk).
pub(crate) fn static_entries(built: &Built) -> Vec<(StaticId, ContentId)> {
    let digest = |id: &StaticId| {
        let found = built.digests.statics.binary_search_by_key(id, |it| it.0).ok();
        found.map(|at| built.digests.statics[at].1)
    };
    // alloc: per build, an entry a static, kept for the next build's delta
    let mut entries = Vec::with_capacity(built.static_owners.len());
    for (item, instance, owner) in &built.static_owners {
        let id = (*item, *instance);
        let symbols = (built.static_symbols.iter())
            .filter(|it| (it.0, it.1) == id)
            .map(|it| (it.2.as_str(), it.3));
        entries.push((id, static_entry(&id, *owner, digest(&id), symbols)));
    }
    entries.sort_unstable_by_key(|it| it.0);
    entries
}

/// The [`BuiltId`] of `built`, whose entries are in `sets`.
fn id_of(built: &Built, sets: &Sets) -> BuiltId {
    // What is of no function of the walk: a folded group's body and its identity.
    let walked = |key: &InstanceKey| built.inputs.binary_search_by_key(key, |it| it.0).is_ok();
    id_from(
        sets,
        &built.roots,
        &built.entry,
        built.identities.iter().filter(|it| !walked(&it.0)).copied(),
        (built.functions.iter().filter(|it| !walked(&it.key)))
            .map(|function| (function.key, content::code_id(function))),
    )
}

/// The [`BuiltId`] of the entries in `sets`, the roots, a lazy build's entry, and what is of
/// no function of the walk: each folded group's identity and its body's code ID.
fn id_from(
    sets: &Sets,
    roots: &[InstanceKey],
    entry: &[InstanceKey],
    identities: impl Iterator<Item = (InstanceKey, ContentId)>,
    bodies: impl Iterator<Item = (InstanceKey, ContentId)>,
) -> BuiltId {
    let mut hasher = Hasher::new(BUILT_ID_DOMAIN);
    // Each root says its set's entries and how many there are.
    hasher.update(&sets.functions.root()).update(&sets.statics.root());
    for list in [roots, entry] {
        hasher.update(&(list.len() as u64).to_le_bytes());
        for key in list {
            hasher.id(&key.0);
        }
    }
    for (key, identity) in identities {
        hasher.id(&key.0).id(&identity);
    }
    for (key, code) in bodies {
        hasher.id(&key.0).id(&code);
    }
    BuiltId(hasher.finish())
}

#[cfg(test)]
mod tests {
    use hegel::generators as gs;

    use super::*;

    /// A key of a small space, so two drawn lists share keys.
    fn key(at: &u8) -> Hash {
        ContentId::of("built id test key", &[*at]).0
    }

    /// A list of entries sorted by key, each key once.
    #[hegel::composite]
    fn entries(tc: &hegel::TestCase) -> Vec<(u8, ContentId)> {
        let keys: Vec<u8> = tc.draw(gs::vecs(gs::integers::<u8>().max_value(15)).max_size(16));
        let mut made: Vec<(u8, ContentId)> = Vec::new();
        for at in keys {
            if made.iter().all(|it| it.0 != at) {
                let value = tc.draw(gs::integers::<u8>().max_value(3));
                made.push((at, ContentId::of("built id test entry", &[value])));
            }
        }
        made.sort_unstable_by_key(|it| it.0);
        made
    }

    /// The root of the set of `before` moved to `now`, with `plant` leaving out the removals.
    fn moved_root(before: &[(u8, ContentId)], now: &[(u8, ContentId)], plant: bool) -> Hash {
        let mut set = whole(before, key);
        match plant {
            false => moved(&mut set, before, now, key),
            true => now.iter().for_each(|(of, entry)| {
                set.set(key(of), entry.0);
            }),
        }
        set.root()
    }

    /// The invariant: the ID maintained incrementally equals a fresh build's. The set of one
    /// build's entries moved to the next build's has the root of the next build's entries made
    /// whole, whatever was added, changed and removed between them.
    #[hegel::test]
    fn an_id_kept_through_edits_is_a_fresh_builds(tc: hegel::TestCase) {
        let (before, now) = (tc.draw(entries()), tc.draw(entries()));
        assert_eq!(
            moved_root(&before, &now, false),
            whole(&now, key).root(),
            "moved from {before:?} to {now:?}"
        );
    }

    /// A control of [`an_id_kept_through_edits_is_a_fresh_builds`]: a move that removes
    /// nothing keeps a removed entry in the root.
    #[test]
    fn the_oracle_catches_a_removed_entry_kept_in_the_id() {
        let entry = |value: u8| ContentId::of("built id test entry", &[value]);
        let before = [(1, entry(0)), (2, entry(1))];
        let now = [(1, entry(0))];
        assert_eq!(moved_root(&before, &now, false), whole(&now, key).root());
        assert_ne!(moved_root(&before, &now, true), whole(&now, key).root());
    }

    /// What [`BuiltId`] was made of until 2026-11-10: the entries read as four 64-bit words
    /// and added word by word, wrapping.
    fn wrapping_sum(entries: &[(u8, ContentId)]) -> [u64; 4] {
        let mut sum = [0u64; 4];
        for (_, entry) in entries {
            for (word, bytes) in sum.iter_mut().zip(entry.0.chunks_exact(8)) {
                let mut eight = [0; 8];
                eight.copy_from_slice(bytes);
                *word = word.wrapping_add(u64::from_le_bytes(eight));
            }
        }
        sum
    }

    /// The bug the sum had, in miniature: two sets of two entries under the same keys, the
    /// second's entries the first's with one word moved from one to the other, have one sum
    /// and one count, so had one ID; and so do the entries `{x, -x}` and `{0, 0}`. Their
    /// keyed sets have other roots.
    #[test]
    fn sets_a_wrapping_sum_takes_for_one_have_other_ids() {
        let (a, b) = (ContentId::of("built id test entry", &[1]), ContentId::of("built id test entry", &[2]));
        let shifted = |entry: ContentId, by: u64| {
            let mut eight = [0; 8];
            eight.copy_from_slice(&entry.0[..8]);
            let mut made = entry;
            made.0[..8].copy_from_slice(&u64::from_le_bytes(eight).wrapping_add(by).to_le_bytes());
            made
        };
        let one = [(1, a), (2, b)];
        let other = [(1, shifted(a, 1)), (2, shifted(b, u64::MAX))];
        assert_eq!(wrapping_sum(&one), wrapping_sum(&other), "the sum takes them for one");
        assert_ne!(whole(&one, key).root(), whole(&other, key).root());

        let x = shifted(ContentId([0; 32]), 7);
        let minus = shifted(ContentId([0; 32]), 7u64.wrapping_neg());
        let cancelling = [(1, x), (2, minus)];
        let zeros = [(1, ContentId([0; 32])), (2, ContentId([0; 32]))];
        assert_eq!(wrapping_sum(&cancelling), wrapping_sum(&zeros), "the sum takes them for one");
        assert_ne!(whole(&cancelling, key).root(), whole(&zeros, key).root());
    }
}
