//! A program's last `Built`, kept by row and patched by the next build
//! (docs/v2/plan/design/incremental-walk.md, "`Built` is kept and patched").
//!
//! [`Lists`] holds one row a function and one a static, in maps sorted by key whose chunks
//! are shared ([`rows`](super::rows)): everything the `Built` says of it, its compiled
//! function as it was made and as dedupe left it, and its entry ([`Delta`]). A build whose
//! walk was patched ([`patch::Touched`]) makes again the rows of what the walk touched and
//! of nothing else: their templates are looked up and instantiated, their versions, digests
//! and entries made, dedupe asked of them alone, and the keyed sets of the entries moved by
//! them, so the `Built`'s ID and its delta are the patch's own record of what it replaced.
//!
//! Why a row that is not touched is the row a whole build would make. A row is a function of
//! its node (its describe, unit, slot, places, built-in closure), of the store's template
//! under the node's template key, and of dedupe. The walk lists every node one of whose
//! fields is another, so an unlisted node is the kept one field for field. Its template key
//! ([`dev_template_key`](super::dev_template_key)) covers every input of its template: its
//! template code, type arguments and settings (the template instance's key), the bodies it
//! builds in (`stored`), the kind of each place and which place is itself (`placed`), and
//! the toolchain (`kernel::template`); and its function is that template instantiated with
//! its key, its places and its own anchors, all the node's. So the kept function is the
//! value a lookup and an instantiate would give again (rule keys-cover-every-input). Dedupe
//! is the least merge closed under "equal code after renaming": a function is in it for its
//! own code ID and those of what names it, so a touched function that is in no class, names
//! no changed class and whose code ID after the kept renames is no other kept function's
//! changes no class ([`Lists::patch`]); any other case runs dedupe whole from the functions
//! as made.
//!
//! The lists a `Built` gives are contiguous `Vec`s, which its readers outside this crate
//! index and slice: they are made from the rows each build, one copy an entry (a compiled
//! function's bytes among them). That copy is linear in the program; what is hashed, looked
//! up or compared is not.

use std::{
    alloc::Global,
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};

use super::{
    Build, Built, Digests, Finished, FnNode, KernelEntry, Lookup, Node, Out, Planted, Reach,
    Sorted, StaticId, StaticNode, StaticOf, Walk, Work,
    delta::{self, BuiltId, Changes, Delta, Last, Sets},
    digests, error, names_of,
    patch::Touched,
    reach, stopped,
};
use crate::{
    CodegenError, CompiledFunction, InstanceKey, Linkage, Owner, Static, content,
    backend::{Frontend, Prepare},
    dedupe::{self, DedupeError, Deduped, Originals, dedupe_keeping},
};

/// Everything a `Built` says of one function.
// clone: a chunk of rows a copy of the lists shares, copied by the build that changes it
#[derive(Clone)]
struct FunctionRow {
    item: ItemKey,
    /// The code hash it was made from: [`Built::inputs`], and its generic's in
    /// [`Built::owners`].
    code: CodeHash,
    name: String,
    name_digest: ContentId,
    symbol: Option<(String, Linkage)>,
    slotted: bool,
    tiered: bool,
    editable: bool,
    cold: bool,
    owner: Owner,
    made_as: crate::Made,
    identity: ContentId,
    version: (ContentId, Result<ContentId, String>),
    /// Its function as its template was instantiated, before dedupe.
    made: Arc<CompiledFunction>,
    /// Its function after dedupe: `made` itself where dedupe renamed nothing in it, `None`
    /// merged away.
    kept: Option<Arc<CompiledFunction>>,
    /// [`digests::function_digest`] of it.
    digest: ContentId,
    /// [`content::code_id`] of `kept`.
    code_id: Option<ContentId>,
    /// Whether dedupe may merge it: it is then in [`Lists::ids`] under `code_id`.
    candidate: bool,
    /// Its dev template's kernel entry, as a build that took it from the store records it.
    kernel: Option<KernelEntry>,
    /// Its entry ([`Delta`]).
    entry: ContentId,
}

/// Everything a `Built` says of one static.
// clone: as `FunctionRow`
#[derive(Clone)]
struct StaticRow {
    owner: Owner,
    symbol: Option<(String, Linkage)>,
    /// Its definition as compiled, before dedupe renamed what it points at, with its digest.
    made: Arc<Static>,
    made_digest: ContentId,
    /// Its definition after dedupe, with its digest.
    kept: Arc<Static>,
    digest: ContentId,
    entry: ContentId,
}

/// A program's last `Built`, by row ([`self`]).
pub(super) struct Lists {
    functions: Sorted<InstanceKey, FunctionRow>,
    statics: Sorted<StaticId, StaticRow>,
    /// What dedupe merged ([`Deduped`]), and the same as the map a rename takes.
    merged: BTreeMap<InstanceKey, InstanceKey>,
    renames: blaze_hash::HashMap<InstanceKey, InstanceKey, Global>,
    rounds: u32,
    bytes: u64,
    /// Every function in a class of more than one: those merged away and those kept for
    /// them.
    involved: blaze_hash::HashSet<InstanceKey, Global>,
    /// Each kept candidate by its code ID: no two share one, or dedupe would have merged
    /// them.
    ids: HashMap<ContentId, InstanceKey>,
    reach: Reach,
    /// How many functions and statics are of each item.
    // BTreeMap: `Built::items` is its keys, in order.
    items: BTreeMap<ItemKey, u32>,
    sets: Sets,
    id: BuiltId,
}

/// What became of a build that patched its lists ([`Build::patch_lists`]).
pub(super) enum Patch {
    /// The `Built`, and its lists for the next build.
    Built(Box<Built>, Lists),
    /// The build is refused: the lists are let go, and the next build makes them whole.
    Refused(Vec<CodegenError>),
    /// Nothing was patched, for the reason given: the passes run whole over the walk, and
    /// the lists are the last build's still.
    Whole(&'static str, Lists),
}

impl Lists {
    /// The lists of `built`, a `Built` whose passes ran whole over `nodes` and `statics`:
    /// `originals` what its dedupe changed, as given it, `last` its entries and `codes` the
    /// code ID of each function it kept. `Err` gives `last` back for a build that is not
    /// kept by row: one with a function folded or built by a release backend, whose compiles
    /// are of the whole build.
    pub(super) fn of_whole<I, S>(
        nodes: &Sorted<InstanceKey, FnNode<I, S>>,
        statics: &Sorted<StaticId, StaticNode<I, S>>,
        built: &Built,
        originals: Originals,
        last: Last,
        codes: &[(InstanceKey, ContentId)],
    ) -> Result<Lists, Last> {
        let plain = nodes.values().all(|it| !it.spec.backend.is_release() && !it.spec.fold)
            && built.walked.is_empty()
            && built.inputs.len() == nodes.len()
            && last.functions.len() == nodes.len()
            && last.statics.len() == statics.len()
            && built.versions.len() == nodes.len();
        if !plain {
            return Err(last);
        }
        let mut lists = Lists {
            functions: Sorted::default(),
            statics: Sorted::default(),
            merged: BTreeMap::new(),
            renames: blaze_hash::HashMap::new_in(Global),
            rounds: 0,
            bytes: 0,
            involved: blaze_hash::HashSet::new_in(Global),
            ids: HashMap::with_capacity(codes.len()),
            reach: built.reach.clone(), // clone: the graph the next build patches
            items: BTreeMap::new(),
            sets: last.sets.clone(), // clone: the sets the next build moves
            id: last.id,
        };
        lists.set_deduped(&built.deduped);
        let post = |key: &InstanceKey| {
            let at = built.functions.binary_search_by_key(key, |it| it.key).ok()?;
            Some(&built.functions[at])
        };
        let original = |key: &InstanceKey| {
            let at = originals.functions.binary_search_by_key(key, |it| it.key).ok()?;
            Some(&originals.functions[at])
        };
        let mergeable = |key: &InstanceKey| nodes.get(key).is_none_or(|it| !it.external());
        // alloc: per build kept by row, a row a function
        let mut rows = Vec::with_capacity(nodes.len());
        for (at, (key, node)) in nodes.iter().enumerate() {
            let (kept, before) = (post(key), original(key));
            // clone: per build kept by row, each function as it was made
            let Some(made) = before.or(kept).map(|it| Arc::new(it.clone())) else {
                return Err(last);
            };
            let code_id = codes.binary_search_by_key(key, |it| it.0).ok().map(|at| codes[at].1);
            let candidate = kept.is_some_and(|it| dedupe::candidate(it, mergeable(key)));
            if let (true, Some(id)) = (candidate, code_id) {
                lists.ids.insert(id, *key);
            }
            let kernel = built.kernel.partition_point(|it| it.instance < *key);
            let kernel = built.kernel.get(kernel).filter(|it| it.instance == *key);
            lists.item(node.unit.item, true);
            let row = FunctionRow {
                item: node.unit.item,
                code: node.unit.instance.code,
                name: node.described.name.clone(), // clone: per build kept by row, a name a row
                name_digest: node.name_digest,
                // clone: per build kept by row, its symbols
                symbol: node.described.symbol.clone().map(|it| (it, node.described.linkage)),
                slotted: node.slotted(),
                tiered: node.then.is_some(),
                editable: node.described.editable,
                cold: node.described.cold,
                owner: node.described.owner,
                made_as: crate::Made { backend: node.spec.backend, origin: node.least_origin },
                identity: node.subject(),
                version: built.versions[at].clone(), // clone: per build kept by row
                kept: kept.map(|it| match before {
                    // clone: a function dedupe renamed, as it left it
                    Some(_) => Arc::new(it.clone()),
                    None => made.clone(), // clone: a reference count
                }),
                digest: digests::function_digest(node.unit.instance.code, &made.inlined),
                made,
                code_id,
                candidate,
                kernel: kernel.map(|it| KernelEntry { hit: true, ..*it }),
                entry: last.functions[at].1,
            };
            rows.push((*key, row));
        }
        lists.functions = Sorted::of_sorted(rows.into_iter());
        // alloc: per build kept by row, a row a static
        let mut rows = Vec::with_capacity(statics.len());
        for (at, (id, node)) in statics.iter().enumerate() {
            let find = |list: &[Static]| {
                let at = list.binary_search_by_key(id, |it| (it.item, it.instance)).ok()?;
                // clone: per build kept by row, each static as it is and as it was made
                Some(Arc::new(list[at].clone()))
            };
            let digest = built.digests.statics.binary_search_by_key(id, |it| it.0).ok();
            let (Some(kept), Some(digest)) = (find(&built.statics), digest) else {
                return Err(last);
            };
            let digest = built.digests.statics[digest].1;
            let before = find(&originals.statics);
            lists.item(id.0, true);
            let row = StaticRow {
                owner: node.described.owner,
                // clone: per build kept by row, its symbols
                symbol: node.described.symbol.clone().map(|it| (it, node.described.linkage)),
                made_digest: match &before {
                    Some(before) => digests::static_definition(before),
                    None => digest,
                },
                made: before.unwrap_or_else(|| kept.clone()), // clone: a reference count
                kept,
                digest,
                entry: last.statics[at].1,
            };
            rows.push((*id, row));
        }
        lists.statics = Sorted::of_sorted(rows.into_iter());
        Ok(lists)
    }

    /// The entries, sets and ID of the `Built` these lists are of: what a build whose passes
    /// run whole makes its delta against.
    pub(super) fn into_last(self) -> Last {
        Last {
            id: self.id,
            // alloc: per whole build after a kept one, the kept one's entries
            functions: self.functions.iter().map(|(key, row)| (*key, row.entry)).collect(),
            // alloc: as above
            statics: self.statics.iter().map(|(id, row)| (*id, row.entry)).collect(),
            sets: self.sets,
        }
    }

    fn set_deduped(&mut self, deduped: &Deduped) {
        self.merged = deduped.merged.clone(); // clone: what dedupe merged, the lists' own copy
        (self.rounds, self.bytes) = (deduped.rounds, deduped.bytes);
        self.renames.clear();
        self.involved.clear();
        for (from, to) in &deduped.merged {
            self.renames.insert(*from, *to);
            self.involved.insert(*from);
            self.involved.insert(*to);
        }
    }

    /// Counts a function or static of `item` in or out.
    fn item(&mut self, item: ItemKey, add: bool) {
        let held = self.items.entry(item).or_insert(0);
        *held = if add { *held + 1 } else { held.saturating_sub(1) };
        if *held == 0 {
            self.items.remove(&item);
        }
    }

    /// Takes the code ID of the row of `key` out of [`Self::ids`], where it is the one
    /// filed there.
    fn forget_code(&mut self, key: &InstanceKey, code_id: Option<ContentId>, candidate: bool) {
        if let (true, Some(id)) = (candidate, code_id)
            && self.ids.get(&id) == Some(key)
        {
            self.ids.remove(&id);
        }
    }

    /// The entry of the row of `key` ([`delta::function_entry`]), from the row: what
    /// [`delta::function_entries`] makes of the `Built` these lists give.
    fn entry(&self, key: &InstanceKey, row: &FunctionRow, roots: &[InstanceKey]) -> ContentId {
        let mut it = delta::Row::default();
        if let Some(kept) = &row.kept {
            it.flags |= 1;
            it.code_id = row.code_id.unwrap_or_default();
            if let Some(side) = kept.side {
                it.flags |= 1 << 1;
                it.side = side;
            }
            it.piece = row.digest;
        }
        if let Some(to) = self.merged.get(key) {
            it.flags |= 1 << 2;
            it.merged = to.0;
        }
        if let Some((_, linkage)) = &row.symbol {
            it.flags |= 1 << 3;
            it.flags |= u8::from(*linkage == Linkage::Weak) << 4;
        }
        it.flags |= u8::from(row.slotted) << 5;
        it.flags |= u8::from(row.tiered) << 6;
        it.flags |= u8::from(row.cold) << 7;
        it.more |= u8::from(row.editable);
        it.more |= u8::from(roots.contains(key)) << 1;
        it.more |= u8::from(row.owner == Owner::Workspace) << 3;
        it.more |= (1 << 4) | (1 << 5) | (1 << 6);
        (it.name, it.generic, it.identity) = (row.name_digest, row.code.0, row.identity);
        it.origin = Some(row.made_as.origin);
        let symbol = row.symbol.as_ref().map(|it| it.0.as_str());
        delta::function_entry(*key, row.code, &it, symbol, Some(&row.version))
    }

    /// Dedupe over every function as it was made: the classes, each function as dedupe left
    /// it and each static, made again whole.
    fn dedupe_whole(
        &mut self,
        mergeable: impl Fn(&InstanceKey) -> bool,
    ) -> Result<(), DedupeError> {
        // clone: per whole dedupe of kept lists, every function and static as made
        let mut functions: Vec<CompiledFunction> =
            self.functions.values().map(|row| (*row.made).clone()).collect();
        // clone: as above
        let mut statics: Vec<Static> = self.statics.values().map(|row| (*row.made).clone()).collect();
        let mut originals = Originals::default();
        let (deduped, renamed) =
            dedupe_keeping(&mut functions, &mut statics, &mergeable, Some(&mut originals))?;
        self.set_deduped(&deduped);
        self.ids.clear();
        // alloc: per whole dedupe, the rows' keys
        let keys: Vec<InstanceKey> = self.functions.keys().copied().collect();
        let mut at = 0;
        for key in keys {
            while at < functions.len() && functions[at].key < key {
                at += 1;
            }
            let kept = functions.get(at).filter(|it| it.key == key);
            let was_renamed = originals.functions.binary_search_by_key(&key, |it| it.key).is_ok();
            let Some(row) = self.functions.get_mut(&key) else { continue };
            match kept {
                None => (row.kept, row.code_id, row.candidate) = (None, None, false),
                Some(function) => {
                    row.kept = Some(match was_renamed {
                        true => Arc::new(function.clone()), // clone: a function dedupe renamed
                        false => row.made.clone(),           // clone: a reference count
                    });
                    let id = content::code_id(function);
                    row.code_id = Some(id);
                    row.candidate = dedupe::candidate(function, mergeable(&key));
                    if row.candidate {
                        self.ids.insert(id, key);
                    }
                }
            }
        }
        // alloc: per whole dedupe, the rows' keys
        let ids: Vec<StaticId> = self.statics.keys().copied().collect();
        for (at, id) in ids.into_iter().enumerate() {
            let Some(row) = self.statics.get_mut(&id) else { continue };
            match renamed.contains(&at) {
                true => {
                    row.digest = digests::static_definition(&statics[at]);
                    row.kept = Arc::new(statics[at].clone()); // clone: a static dedupe renamed
                }
                false => (row.kept, row.digest) = (row.made.clone(), row.made_digest), // clone: a count
            }
        }
        Ok(())
    }
}

impl<'a, B: Prepare> Build<'a, B> {
    /// The `Built` of a walk patched from the walk of `lists`' build: the rows of what the
    /// walk touched are made again, and every other row is the kept one ([`self`]).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn patch_lists(
        &self,
        mut lists: Lists,
        nodes: &Sorted<InstanceKey, Node<B>>,
        statics: &Sorted<StaticId, StaticOf<B>>,
        roots: &[InstanceKey],
        touched: &Touched,
        mut work: Work,
        mut walk: Walk,
        store: &impl Lookup,
    ) -> Patch {
        // A function folded with others or built by a release backend is compiled with the
        // whole build: its passes are not by row.
        for key in &touched.functions {
            match nodes.get(key) {
                Some(node) if node.spec.backend.is_release() || node.spec.fold => {
                    return Patch::Whole(
                        "a changed function is folded or built by a release backend",
                        lists,
                    );
                }
                Some(_) => {}
                None => return Patch::Whole("the walk touched a function it does not hold", lists),
            }
        }
        if touched.statics.iter().any(|id| !statics.contains(id)) {
            return Patch::Whole("the walk touched a static it does not hold", lists);
        }
        let refused = |key: InstanceKey, item: ItemKey, what: String| {
            Patch::Refused(vec![error(key, item, what)])
        };

        // The templates of the touched functions and the definitions of the touched
        // statics: looked up, else compiled, and instantiated, as a whole build does each.
        let mut out = Out::default();
        {
            let _span = tracing::info_span!(
                target: "blaze",
                "codegen templates",
                functions = touched.functions.len(),
                kept = nodes.len() - touched.functions.len(),
            )
            .entered();
            // clone: per build, the keys of the functions it makes again
            self.dev_of(nodes, touched.functions.clone(), store, &mut out);
        }
        if self.frontend.stopped() {
            // alloc: an error, once
            return Patch::Refused(vec![stopped()]);
        }
        {
            let _span = tracing::info_span!(
                target: "blaze",
                "codegen statics",
                statics = touched.statics.len(),
                kept = statics.len() - touched.statics.len(),
            )
            .entered();
            self.statics(statics, Some(&touched.statics), store, &mut out);
        }
        out.profiled(self.policy.pgo);
        work.lookups = touched.functions.len() + touched.statics.len();
        let function_digests: HashMap<InstanceKey, ContentId> =
            std::mem::take(&mut out.function_digests).into_iter().collect();
        let static_digests: HashMap<StaticId, ContentId> =
            std::mem::take(&mut out.static_digests).into_iter().collect();
        let Finished {
            functions,
            statics: compiled_statics,
            puts,
            mut stats,
            errors,
            used,
            kernel,
            misses,
        } = out.finish(names_of::<_, _, B>(nodes, Some(statics)));
        if !errors.is_empty() {
            return Patch::Refused(errors);
        }
        let finishing = tracing::info_span!(
            target: "blaze",
            "codegen finish",
            functions = touched.functions.len(),
            kept = nodes.len() - touched.functions.len(),
            replaced = tracing::field::Empty,
            hashed = tracing::field::Empty,
            copied = tracing::field::Empty,
            dedupe_whole = tracing::field::Empty,
        );
        let _finishing = finishing.enter();
        let mut made: HashMap<InstanceKey, CompiledFunction> =
            functions.into_iter().map(|it| (it.key, it)).collect();
        let mut made_statics: HashMap<StaticId, Static> =
            compiled_statics.into_iter().map(|it| ((it.item, it.instance), it)).collect();
        let mergeable = |key: &InstanceKey| nodes.get(key).is_none_or(|it| !it.external());
        let dedupe_refused = |why: DedupeError| {
            let item = nodes.get(&why.unit()).map_or(why.item(), |it| it.unit.item);
            // alloc: an error
            Patch::Refused(vec![error(why.unit(), item, why.to_string())])
        };
        let hashed = lists.sets.hashed();
        let base = lists.id;
        let mut changes: Changes<InstanceKey> = Changes::default();
        let mut static_changes: Changes<StaticId> = Changes::default();
        // Whether a touched function is in a class dedupe merged, or is equal to a kept
        // one: dedupe then runs whole.
        let mut whole = false;

        // The rows that go.
        for key in &touched.removed {
            let Some(row) = lists.functions.remove(key) else { continue };
            whole |= lists.involved.contains(key);
            lists.forget_code(key, row.code_id, row.candidate);
            lists.item(row.item, false);
            lists.sets.function(key, None);
            changes.removed.push(*key);
            work.replaced += 1;
        }
        for id in &touched.removed_statics {
            if lists.statics.remove(id).is_none() {
                continue;
            }
            lists.item(id.0, false);
            lists.sets.static_(id, None);
            static_changes.removed.push(*id);
            work.replaced += 1;
        }

        // The rows made again: each touched function's, from its node and its function.
        // Each with the entry its row had, `None` for a new one.
        // alloc: per build, the rows it makes again
        let mut remade: Vec<(InstanceKey, Option<ContentId>)> =
            Vec::with_capacity(touched.functions.len());
        for key in &touched.functions {
            let node = &nodes[key];
            let Some(function) = made.remove(key) else {
                // alloc: an error
                let what = format!(
                    "the walk touched `{}`, and no function was made of it",
                    node.described.name
                );
                return refused(*key, node.unit.item, what);
            };
            let old = lists.functions.get(key).map(|it| (it.entry, it.item, it.code_id, it.candidate));
            if let Some((_, item, code_id, candidate)) = old {
                whole |= lists.involved.contains(key);
                lists.forget_code(key, code_id, candidate);
                lists.item(item, false);
            }
            let digest = function_digests.get(key).copied().unwrap_or_else(|| {
                digests::function_digest(node.unit.instance.code, &function.inlined)
            });
            // alloc: once per function made again, shared by its row's two forms
            let function = Arc::new(function);
            // What it names of the classes dedupe kept is renamed, as dedupe did for it.
            let kept = match lists.renames.is_empty() {
                true => function.clone(), // clone: a reference count
                false => {
                    // clone: a function made again, to rename what it names of a class
                    let mut renamed = (*function).clone();
                    match dedupe::rename(&mut renamed, &lists.renames) {
                        Ok(true) => Arc::new(renamed), // alloc: a renamed function
                        Ok(false) => function.clone(), // clone: a reference count
                        Err(why) => return dedupe_refused(why),
                    }
                }
            };
            let code_id = content::code_id(&kept);
            work.hashed += kept.code.len() as u64;
            work.deduped += 1;
            let candidate = dedupe::candidate(&kept, mergeable(key));
            if candidate {
                match lists.ids.get(&code_id) {
                    Some(other) if other != key => whole = true,
                    _ => {
                        lists.ids.insert(code_id, *key);
                    }
                }
            }
            let item_of = |key: &InstanceKey| nodes.get(key).map(|it| it.unit.item);
            let version = (
                crate::exec::fn_id(node.subject(), node.unit.instance.settings),
                crate::exec::bound(&node.described.bound, &node.refs, item_of),
            );
            work.versions += 1;
            let entry = kernel.partition_point(|it| it.instance < *key);
            let entry = kernel.get(entry).filter(|it| it.instance == *key);
            lists.item(node.unit.item, true);
            let row = FunctionRow {
                item: node.unit.item,
                code: node.unit.instance.code,
                name: node.described.name.clone(), // clone: a row's name, once made again
                name_digest: node.name_digest,
                // clone: a row's symbol, once made again
                symbol: node.described.symbol.clone().map(|it| (it, node.described.linkage)),
                slotted: node.slotted(),
                tiered: node.then.is_some(),
                editable: node.described.editable,
                cold: node.described.cold,
                // The planted bug of a control keeps the owner its row had.
                owner: match (self.planted, lists.functions.get(key)) {
                    (Some(Planted::KeptOwner), Some(row)) => row.owner,
                    _ => node.described.owner,
                },
                made_as: crate::Made { backend: node.spec.backend, origin: node.least_origin },
                identity: node.subject(),
                version,
                made: function,
                kept: Some(kept),
                digest,
                code_id: Some(code_id),
                candidate,
                // A later build takes it from these lists, as a store hit.
                kernel: entry.map(|it| KernelEntry { hit: true, ..*it }),
                entry: ContentId::default(),
            };
            lists.functions.insert(*key, row);
            remade.push((*key, old.map(|it| it.0)));
            work.replaced += 1;
        }
        // alloc: per build, the static rows it makes again
        let mut remade_statics: Vec<(StaticId, Option<ContentId>)> =
            Vec::with_capacity(touched.statics.len());
        for id in &touched.statics {
            let node = &statics[id];
            let Some(value) = made_statics.remove(id) else {
                // alloc: an error
                let what = format!(
                    "the walk touched static `{}`, and no definition was made of it",
                    node.described.name
                );
                return refused(InstanceKey(node.described.value), id.0, what);
            };
            let made_digest = (static_digests.get(id).copied())
                .unwrap_or_else(|| digests::static_definition(&value));
            // alloc: once per static made again, shared by its row's two forms
            let value = Arc::new(value);
            let (kept, digest) = match lists.renames.is_empty() {
                true => (value.clone(), made_digest), // clone: a reference count
                false => {
                    // clone: a static made again, to rename what it names of a class
                    let mut renamed = (*value).clone();
                    match dedupe::rename_static(&mut renamed, &lists.renames) {
                        Ok(true) => {
                            let digest = digests::static_definition(&renamed);
                            (Arc::new(renamed), digest) // alloc: a renamed static
                        }
                        Ok(false) => (value.clone(), made_digest), // clone: a reference count
                        Err(why) => return dedupe_refused(why),
                    }
                }
            };
            let old = lists.statics.get(id).map(|it| it.entry);
            if old.is_none() {
                lists.item(id.0, true);
            }
            let row = StaticRow {
                owner: node.described.owner,
                // clone: a row's symbol, once made again
                symbol: node.described.symbol.clone().map(|it| (it, node.described.linkage)),
                made: value,
                made_digest,
                kept,
                digest,
                entry: ContentId::default(),
            };
            lists.statics.insert(*id, row);
            remade_statics.push((*id, old));
            work.replaced += 1;
        }

        // Dedupe: nothing more for a build that touched no class; else whole, over every
        // function as it was made, and every row's entry is made again.
        // The planted bug of a control leaves the classes as they were.
        if whole && self.planted != Some(Planted::UnsplitClass) {
            let _span = tracing::info_span!(
                target: "blaze",
                "codegen dedupe",
                functions = lists.functions.len()
            )
            .entered();
            if let Err(why) = lists.dedupe_whole(mergeable) {
                return dedupe_refused(why);
            }
            work.deduped = lists.functions.len();
            work.hashed += lists.functions.values().map(|it| it.made.code.len() as u64).sum::<u64>();
            // Every row, with the entry it had: a row made again above keeps what it had
            // before the build.
            let new: HashMap<InstanceKey, Option<ContentId>> = remade.iter().copied().collect();
            remade = (lists.functions.iter())
                .map(|(key, row)| (*key, new.get(key).copied().unwrap_or(Some(row.entry))))
                // alloc: per whole dedupe, every row
                .collect();
            let new: HashMap<StaticId, Option<ContentId>> = remade_statics.iter().copied().collect();
            remade_statics = (lists.statics.iter())
                .map(|(id, row)| (*id, new.get(id).copied().unwrap_or(Some(row.entry))))
                // alloc: per whole dedupe, every row
                .collect();
        }
        finishing.record("dedupe_whole", whole);

        // The entries of the rows made again, the delta they are, and the ID through them.
        let delta_span = tracing::info_span!(
            target: "blaze",
            "codegen delta",
            functions = remade.len(),
            added = tracing::field::Empty,
            changed = tracing::field::Empty,
            removed = changes.removed.len(),
        );
        let delta_entered = delta_span.enter();
        for (key, old) in &remade {
            let Some(row) = lists.functions.get(key) else { continue };
            let entry = lists.entry(key, row, roots);
            if row.entry != entry
                && let Some(row) = lists.functions.get_mut(key)
            {
                row.entry = entry;
            }
            match old {
                Some(old) if *old == entry => continue,
                Some(_) => changes.changed.push(*key),
                None => changes.added.push(*key),
            }
            lists.sets.function(key, Some(entry));
        }
        for (id, old) in &remade_statics {
            let Some(row) = lists.statics.get(id) else { continue };
            let symbols = row.symbol.iter().map(|it| (it.0.as_str(), it.1));
            let entry = delta::static_entry(id, row.owner, Some(row.digest), symbols);
            if row.entry != entry
                && let Some(row) = lists.statics.get_mut(id)
            {
                row.entry = entry;
            }
            match old {
                Some(old) if *old == entry => continue,
                Some(_) => static_changes.changed.push(*id),
                None => static_changes.added.push(*id),
            }
            lists.sets.static_(id, Some(entry));
        }
        // The planted bug of a control: the delta names no function the build removed.
        if self.planted == Some(Planted::DeltaOmitsRemoved) {
            changes.removed.clear();
        }
        lists.id = lists.sets.id(roots);
        work.hashed += lists.sets.hashed() - hashed;
        delta_span.record("added", changes.added.len());
        delta_span.record("changed", changes.changed.len());
        drop(delta_entered);

        // What each root reaches: the kept graph with the touched nodes' edges made again.
        // alloc: per build, the nodes whose edges it makes again
        let reach_functions: Vec<(InstanceKey, reach::Node<'_>)> = (touched.functions.iter())
            .map(|key| {
                let node = &nodes[key];
                let marks = [node.unit.instance.code.0, node.unit.instance.args];
                (*key, reach::Node { marks, refs: &node.refs })
            })
            .collect();
        // alloc: as above
        let reach_statics: Vec<(StaticId, reach::Node<'_>)> = (touched.statics.iter())
            .map(|id| {
                let node = &statics[id];
                let marks = [node.described.value, node.described.instance];
                (*id, reach::Node { marks, refs: &node.refs })
            })
            .collect();
        lists.reach = match Reach::patched(
            &lists.reach,
            &touched.removed,
            &touched.removed_statics,
            &reach_functions,
            &reach_statics,
        ) {
            Ok(reach) => reach,
            Err(which) => {
                let none = (InstanceKey(ContentId::default()), ItemKey(ContentId::default()));
                // alloc: an error
                let what = format!(
                    "{which} refers to a function or static this build's walk removed, and \
                     the walk did not say it changed: the lists kept of the last build are \
                     not this build's"
                );
                return refused(none.0, none.1, what);
            }
        };
        work.reach = reach_functions.len() + reach_statics.len();

        // The `Built`: its contiguous lists, an entry copied a row.
        let count = lists.functions.len();
        let mut built = Built {
            roots: roots.to_vec(), // clone: per build, its roots' keys
            // alloc: per build, what it gives the linker, a copy a row
            functions: Vec::with_capacity(count),
            statics: Vec::with_capacity(lists.statics.len()),
            inputs: Vec::with_capacity(count),
            debug_names: Vec::with_capacity(count),
            owners: Vec::with_capacity(count),
            versions: Vec::with_capacity(count),
            identities: Vec::with_capacity(count),
            made: Vec::with_capacity(count),
            kernel: Vec::with_capacity(count),
            digests: Digests {
                functions: Vec::with_capacity(count),
                statics: Vec::with_capacity(lists.statics.len()),
                debug_names: Vec::with_capacity(count),
            },
            ..Built::default()
        };
        for (key, row) in lists.functions.iter() {
            let key = *key;
            built.inputs.push((key, row.code));
            built.debug_names.push((key, row.name.clone())); // clone: the view's copy of a name
            built.digests.debug_names.push((key, row.name_digest));
            if let Some((symbol, linkage)) = &row.symbol {
                built.symbols.push((key, symbol.clone(), *linkage)); // clone: the view's copy
            }
            if row.slotted {
                built.patchable.push(key);
            }
            if row.tiered {
                built.tiered.push(key);
            }
            if row.editable {
                built.editable.push(key);
            }
            if row.cold {
                built.cold.push(key);
            }
            built.owners.push((key, row.owner, row.code));
            built.made.push((key, row.made_as));
            built.identities.push((key, row.identity));
            built.versions.push(row.version.clone()); // clone: the view's copy
            if let Some(kept) = &row.kept {
                // clone: the view's copy of a compiled function, its readers' contiguous list
                built.functions.push((**kept).clone());
                built.digests.functions.push((key, row.digest));
            }
            built.kernel.extend(row.kernel);
        }
        for (id, row) in lists.statics.iter() {
            built.statics.push((*row.kept).clone()); // clone: the view's copy of a static
            built.digests.statics.push((*id, row.digest));
            built.static_owners.push((id.0, id.1, row.owner));
            if let Some((symbol, linkage)) = &row.symbol {
                built.static_symbols.push((id.0, id.1, symbol.clone(), *linkage)); // clone: the view's copy
            }
        }
        // The touched functions' kernel entries are this build's, taken or made.
        for entry in &kernel {
            let at = built.kernel.partition_point(|it| it.instance < entry.instance);
            match built.kernel.get_mut(at).filter(|it| it.instance == entry.instance) {
                Some(held) => *held = *entry,
                None => built.kernel.insert(at, *entry),
            }
        }
        built.items = lists.items.keys().copied().collect(); // alloc: per build, its items
        built.reach = lists.reach.clone(); // clone: the view's copy of the graph
        // clone: the view's copy of what dedupe merged
        built.deduped = Deduped { merged: lists.merged.clone(), rounds: lists.rounds, bytes: lists.bytes };
        work.copied = count + lists.statics.len();
        work.patched = true;
        stats.instances = count;
        stats.statics = lists.statics.len();
        (built.puts, built.stats, built.memo_used, built.misses) = (puts, stats, used, misses);
        built.id = lists.id;
        built.delta = Delta { base: Some(base), functions: changes, statics: static_changes, fresh: None };
        walk.dropped = built.delta.functions.removed.len();
        (built.walk, built.work) = (walk, work);
        finishing.record("replaced", work.replaced);
        finishing.record("hashed", work.hashed);
        finishing.record("copied", work.copied);
        // alloc: once per build, its `Built` moved out of the way of the lists
        Patch::Built(Box::new(built), lists)
    }
}
