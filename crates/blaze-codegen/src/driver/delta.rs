//! What a build changed of its program since the build before it: [`Built`] is a whole value,
//! and [`Delta`] says which of its functions and statics are other than in the last `Built`
//! of the same program, so a consumer that holds state for that one (a link, an image) patches
//! it and reads nothing of the rest (docs/v2/plan/design/incremental-walk.md).
//!
//! Each function and static of a `Built` has an *entry*: one content ID of everything the
//! `Built` says of it ([`function_entries`], [`static_entries`]). A function or static is
//! *changed* when its entry is another. A `Built`'s [`BuiltId`] is made of the sum of its
//! entries, so a build that knows the entries it added, changed and removed makes the new ID
//! from the old sum without reading the rest, and gets the ID a build with nothing kept gets.

use std::sync::Arc;

use blaze_ids::{ContentId, Hasher};
use blaze_items::CodeHash;

use super::{Built, StaticId};
use crate::{InstanceKey, Linkage, Owner, content, plan::Origin};

pub const FUNCTION_ENTRY_DOMAIN: &str = "blaze v2 built function entry 1";
pub const STATIC_ENTRY_DOMAIN: &str = "blaze v2 built static entry 1";
pub const BUILT_ID_DOMAIN: &str = "blaze v2 built id 1";

/// Which whole [`Built`] one is, by content: a digest of every function's and static's entry
/// and of the lists that are of no one function (the roots, a lazy build's entry, a folded
/// group's identity). Two builds of one ID give a link the same functions, statics, names,
/// owners, symbols and slots.
///
/// It is the hash of two sums, of the function entries and of the static entries, each entry
/// read as four 64-bit words added word by word, wrapping: adding is in any order and has an
/// inverse, so the sum after an edit is the sum before, less the entries removed and changed,
/// plus the entries added and changed. A fresh build adds every entry and gets the same sum.
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
/// key, and the ID. 64 bytes a function; the lists themselves are the consumer's.
pub(super) struct Last {
    pub(super) id: BuiltId,
    functions: Vec<(InstanceKey, ContentId)>,
    statics: Vec<(StaticId, ContentId)>,
}

impl Last {
    /// Of `built`: every entry made and summed. Linear in the build; a build that patches its
    /// walk makes only the changed entries and moves the sums by them.
    pub(super) fn of(built: &Built) -> Last {
        let functions = function_entries(built);
        let statics = static_entries(built);
        let id = id_of(built, &functions, &statics);
        Last { id, functions, statics }
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
struct Row {
    /// Bits: compiled, has a side table, merged away, has a symbol, weak, patchable, tiered,
    /// cold (0 to 7).
    flags: u8,
    /// Bits: editable, a root, in the entry, the workspace's, has an owner, has an identity,
    /// has an origin (0 to 6).
    more: u8,
    code_id: ContentId,
    side: ContentId,
    piece: ContentId,
    merged: ContentId,
    name: ContentId,
    symbol: usize,
    generic: ContentId,
    identity: ContentId,
    origin: Option<Origin>,
}

/// The entry of each function of `built`, in key order ([`Built::inputs`]'s): everything
/// `built` says of it ([`Delta`] lists what).
pub(crate) fn function_entries(built: &Built) -> Vec<(InstanceKey, ContentId)> {
    let keys = &built.inputs;
    let at = |key: &InstanceKey| keys.binary_search_by_key(key, |it| it.0).ok();
    // alloc: per build, a row a function, freed with the pass
    let mut rows = vec![Row::default(); keys.len()];
    for function in &built.functions {
        let Some(at) = at(&function.key) else { continue };
        rows[at].flags |= 1;
        rows[at].code_id = content::code_id(function);
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
        entries.push((*key, function_entry(built, index, *key, *code, row)));
    }
    entries
}

fn function_entry(
    built: &Built,
    index: usize,
    key: InstanceKey,
    code: CodeHash,
    row: &Row,
) -> ContentId {
    let mut hasher = Hasher::new(FUNCTION_ENTRY_DOMAIN);
    hasher.id(&key.0).id(&code.0).update(&[row.flags, row.more]);
    hasher.id(&row.code_id).id(&row.side).id(&row.piece).id(&row.merged).id(&row.name);
    if row.flags & (1 << 3) != 0 {
        hasher.part(built.symbols[row.symbol].1.as_bytes());
    }
    hasher.id(&row.generic).id(&row.identity);
    if let Some(origin) = row.origin {
        origin.hash_into(&mut hasher);
    }
    match built.versions.get(index) {
        Some((version, Ok(bound))) => hasher.update(&[1]).id(version).id(bound),
        Some((version, Err(why))) => hasher.update(&[2]).id(version).part(why.as_bytes()),
        None => hasher.update(&[0]),
    };
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
        let mut hasher = Hasher::new(STATIC_ENTRY_DOMAIN);
        hasher.id(&item.0).id(instance).update(&[u8::from(*owner == Owner::Workspace)]);
        match digest(&id) {
            Some(digest) => hasher.update(&[1]).id(&digest),
            None => hasher.update(&[0]),
        };
        for (_, _, symbol, linkage) in
            built.static_symbols.iter().filter(|it| (it.0, it.1) == id)
        {
            hasher.part(symbol.as_bytes()).update(&[u8::from(*linkage == Linkage::Weak)]);
        }
        entries.push((id, hasher.finish()));
    }
    entries.sort_unstable_by_key(|it| it.0);
    entries
}

/// `entry` added to `sum`, word by word.
fn add(sum: &mut [u64; 4], entry: &ContentId) {
    for (word, bytes) in sum.iter_mut().zip(entry.0.chunks_exact(8)) {
        let mut eight = [0; 8];
        eight.copy_from_slice(bytes);
        *word = word.wrapping_add(u64::from_le_bytes(eight));
    }
}

/// The [`BuiltId`] of `built`, whose entries are `functions` and `statics`.
fn id_of(
    built: &Built,
    functions: &[(InstanceKey, ContentId)],
    statics: &[(StaticId, ContentId)],
) -> BuiltId {
    let (mut of_functions, mut of_statics) = ([0u64; 4], [0u64; 4]);
    functions.iter().for_each(|(_, entry)| add(&mut of_functions, entry));
    statics.iter().for_each(|(_, entry)| add(&mut of_statics, entry));
    let mut hasher = Hasher::new(BUILT_ID_DOMAIN);
    for sum in [of_functions, of_statics] {
        for word in sum {
            hasher.update(&word.to_le_bytes());
        }
    }
    hasher.update(&(functions.len() as u64).to_le_bytes());
    hasher.update(&(statics.len() as u64).to_le_bytes());
    for list in [&built.roots, &built.entry] {
        hasher.update(&(list.len() as u64).to_le_bytes());
        for key in list {
            hasher.id(&key.0);
        }
    }
    // What is of no function of the walk: a folded group's body and its identity.
    let walked = |key: &InstanceKey| built.inputs.binary_search_by_key(key, |it| it.0).is_ok();
    for (key, identity) in built.identities.iter().filter(|it| !walked(&it.0)) {
        hasher.id(&key.0).id(identity);
    }
    for function in built.functions.iter().filter(|it| !walked(&it.key)) {
        hasher.id(&function.key.0).id(&content::code_id(function));
    }
    BuiltId(hasher.finish())
}
