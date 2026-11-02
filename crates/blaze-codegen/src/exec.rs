//! A test binary's execution table (fastcompile-ident-select 5,
//! docs/v2/plan/lanes/fastcompile/select.md, "Keys"): every function of the binary as test
//! selection keys it, the function of each jump-table slot, the code the binary runs that is
//! in no table, and the functions each test reaches. `blaze_testsel::Image::new` takes its
//! first three as they are, and each test's reached set by position.
//!
//! It is made beside the binary's link, which numbers the slots, and kept with the binary's
//! record (it is plain data), so a binary replayed rather than linked again selects by the
//! table it was linked with.

use std::fmt;

use blaze_ids::{ContentId, Hasher};
use blaze_items::ItemKey;
use serde::{Deserialize, Serialize};

use crate::{
    CompiledFunction, Init, InstanceKey, Settings, Static, Target,
    driver::{Built, reach::Walk},
    plan::Mode,
};

pub const FN_ID_DOMAIN: &str = "blaze v2 exec fn id 1";

/// What an instance is across versions, test selection's `FnId`: its identity
/// ([`crate::backend::Described::identity`]) with the settings it is compiled with, as one
/// instance compiled two ways (asked for by a member and by a dependency) is two functions of
/// a binary. An edit of a body keeps both.
pub fn fn_id(identity: ContentId, settings: Settings) -> ContentId {
    let mut hasher = Hasher::new(FN_ID_DOMAIN);
    hasher.id(&identity).id(&settings.0);
    hasher.finish()
}

pub const BOUND_DOMAIN: &str = "blaze v2 exec bound 1";

/// An instance's code as it runs, test selection's `Bound`: what the front end says of it
/// ([`crate::backend::Described::bound`]: its code, its item's bound hash, its type arguments)
/// with what each of its references binds to, in order: a function by its item (`item_of`,
/// the build's), so that a callee's body edit is the callee's change and which item it calls
/// (a `Drop` impl added under drop glue, a method resolved by its type arguments) the
/// instance's; a static by its item and instance, data by its content and a symbol by its
/// name. The positions, coverage, counters and PGO counters a function names are its own;
/// where the positions are, and the values of the statics, are known only at the link, which
/// [`ExecTable::new`] adds.
pub(crate) fn bound(
    described: &Result<ContentId, String>,
    refs: &[Target],
    item_of: impl Fn(&InstanceKey) -> Option<ItemKey>,
) -> Result<ContentId, String> {
    // clone: an error, only where an item has no bound hash
    let described = described.clone()?;
    let mut hasher = Hasher::new(BOUND_DOMAIN);
    hasher.id(&described).update(&(refs.len() as u64).to_le_bytes());
    for target in refs {
        match target {
            Target::Function(key) | Target::Direct(key) => {
                let Some(item) = item_of(key) else {
                    // alloc: an error, of a walk that missed a callee
                    return Err(format!("refers to {key:?}, which the build has no instance of"));
                };
                hasher.update(&[0]).id(&item.0)
            }
            Target::Static { item, instance } => hasher.update(&[1]).id(&item.0).id(instance),
            Target::Data(id) => hasher.update(&[2]).id(id),
            Target::Extern(name) => hasher.update(&[3]).part(name.as_bytes()),
            Target::Weak(name) => hasher.update(&[4]).part(name.as_bytes()),
            Target::Position(_) | Target::Coverage(_) | Target::Counter(_) | Target::ProfileCounters => {
                hasher.update(&[5])
            }
        };
    }
    Ok(hasher.finish())
}

pub const STATIC_DOMAIN: &str = "blaze v2 exec static 1";
pub const STATIC_VALUE_DOMAIN: &str = "blaze v2 exec static value 1";

/// A compiled static's value as the program reads it: its bytes and those of the data only it
/// refers to, with where each holds an address; what the addresses are of is its targets'.
fn static_value(it: &Static) -> ContentId {
    fn rows(hasher: &mut Hasher, relocs: &crate::Relocs) {
        hasher.update(&(relocs.rows().len() as u64).to_le_bytes());
        for row in relocs.rows() {
            hasher.update(&row.to_le_bytes());
        }
        hasher.update(&(relocs.wide().len() as u64).to_le_bytes());
        for (row, addend) in relocs.wide() {
            hasher.update(&row.to_le_bytes()).update(&addend.to_le_bytes());
        }
    }
    let mut hasher = Hasher::new(STATIC_VALUE_DOMAIN);
    hasher.update(&it.align.to_le_bytes()).update(&[u8::from(it.mutable), u8::from(it.thread_local)]);
    hasher.opt_part(it.section.as_deref());
    match &it.init {
        Init::Bytes { bytes, relocs } => {
            hasher.update(&[0]).part(bytes);
            rows(&mut hasher, relocs);
        }
        Init::Zeroed { size } => {
            hasher.update(&[1]).update(&size.to_le_bytes());
        }
    }
    hasher.update(&(it.data.len() as u64).to_le_bytes());
    for data in &it.data {
        hasher.id(&data.id).update(&data.align.to_le_bytes()).update(&[u8::from(data.writable)]).part(&data.bytes);
        rows(&mut hasher, &data.relocs);
    }
    hasher.finish()
}

/// Every target a compiled static's bytes name: its value's, then each of its data's.
fn static_targets(it: &Static) -> Vec<&Target> {
    let init: &[Target] = match &it.init {
        Init::Bytes { relocs, .. } => relocs.targets(),
        Init::Zeroed { .. } => &[],
    };
    // alloc: once per test binary linked, per static
    init.iter().chain(it.data.iter().flat_map(|data| data.relocs.targets())).collect()
}

/// Every static of a build as the code reading it sees it: its value, with the values of the
/// statics its bytes name and what the functions they name bind to, transitively, and the
/// span anchors all of them name. The statics of one cycle are hashed once, together, so
/// each has the cycle's bound and anchors.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
struct StaticBounds {
    /// The statics by item and instance, sorted.
    keys: Vec<(ItemKey, ContentId)>,
    /// Per static, its cycle's place in `bounds`.
    component: Vec<u32>,
    /// Per cycle (a static on none is a cycle of one), its bound, or why it has none.
    bounds: Vec<Result<ContentId, String>>,
    /// Per cycle, where its anchors start in `anchors`; one more than there are cycles.
    anchor_starts: Vec<u32>,
    /// Per cycle, sorted and each once.
    anchors: Vec<ContentId>,
}

impl StaticBounds {
    const UNSEEN: u32 = u32::MAX;

    /// The bounds of `statics`, each by item and instance with its value and the targets its
    /// bytes name. `bind` is what a function a static names binds to (its `FnId`). A static
    /// naming a function or static the build has none of has the error naming it, as has every
    /// static reaching it.
    fn new<'a>(
        statics: impl IntoIterator<Item = ((ItemKey, ContentId), ContentId, Vec<&'a Target>)>,
        bind: impl Fn(&InstanceKey) -> Option<ContentId>,
    ) -> StaticBounds {
        // alloc: once per test binary linked, its statics
        let mut nodes: Vec<((ItemKey, ContentId), ContentId, Vec<&'a Target>)> = statics.into_iter().collect();
        nodes.sort_unstable_by_key(|it| it.0);
        let count = nodes.len();
        assert!(count < StaticBounds::UNSEEN as usize, "fewer statics than `UNSEEN`");
        let mut bounds = StaticBounds {
            // alloc: once per test binary linked, its statics
            keys: nodes.iter().map(|it| it.0).collect(),
            // alloc: once per test binary linked, its statics
            component: vec![StaticBounds::UNSEEN; count],
            // alloc: once per test binary linked, a bound per cycle, at most one per static
            bounds: Vec::with_capacity(count),
            // alloc: once per test binary linked, its statics' cycles
            anchor_starts: vec![0],
            // alloc: once per test binary linked, the anchors its statics name
            anchors: Vec::with_capacity(count),
        };
        let at = |item: &ItemKey, instance: &ContentId, keys: &[(ItemKey, ContentId)]| {
            keys.binary_search(&(*item, *instance)).ok().map(|it| it as u32)
        };

        // Tarjan's strongly connected components, without recursion: a cycle is complete, and
        // hashed, only after every cycle it reaches.
        // alloc: per build, a word per static for the walk
        let mut index = vec![StaticBounds::UNSEEN; count];
        // alloc: per build, a word per static for the walk
        let mut low = vec![0u32; count];
        // alloc: per build, a bit per static for the walk
        let mut on_stack = vec![false; count];
        // alloc: per build, the walk's stacks
        let mut stack: Vec<u32> = Vec::new();
        // alloc: per build, the walk's stacks
        let mut calls: Vec<(u32, usize)> = Vec::new();
        // alloc: per build, the members of one cycle, reused
        let mut members: Vec<u32> = Vec::new();
        let mut next = 0u32;
        for root in 0..count as u32 {
            if index[root as usize] != StaticBounds::UNSEEN {
                continue;
            }
            calls.push((root, 0));
            while let Some(&(node, from)) = calls.last() {
                let n = node as usize;
                if from == 0 && index[n] == StaticBounds::UNSEEN {
                    index[n] = next;
                    low[n] = next;
                    next += 1;
                    stack.push(node);
                    on_stack[n] = true;
                }
                let refs = &nodes[n].2;
                let mut place = from;
                let mut descend = None;
                while place < refs.len() {
                    let target = refs[place];
                    place += 1;
                    let Target::Static { item, instance } = target else { continue };
                    let Some(to) = at(item, instance, &bounds.keys) else { continue };
                    let t = to as usize;
                    if index[t] == StaticBounds::UNSEEN {
                        descend = Some(to);
                        break;
                    }
                    if on_stack[t] {
                        low[n] = low[n].min(index[t]);
                    }
                }
                if let Some(to) = descend {
                    let top = calls.len() - 1;
                    calls[top].1 = place;
                    calls.push((to, 0));
                    continue;
                }
                calls.pop();
                if let Some(&(parent, _)) = calls.last() {
                    low[parent as usize] = low[parent as usize].min(low[n]);
                }
                if low[n] != index[n] {
                    continue;
                }
                members.clear();
                while let Some(member) = stack.pop() {
                    on_stack[member as usize] = false;
                    members.push(member);
                    if member == node {
                        break;
                    }
                }
                members.sort_unstable();
                bounds.close(&nodes, &members, &bind);
            }
        }
        bounds
    }

    /// Hashes the cycle of `members` (sorted), every cycle it reaches done.
    fn close(
        &mut self,
        nodes: &[((ItemKey, ContentId), ContentId, Vec<&Target>)],
        members: &[u32],
        bind: &impl Fn(&InstanceKey) -> Option<ContentId>,
    ) {
        let component = self.bounds.len() as u32;
        for &member in members {
            self.component[member as usize] = component;
        }
        let start = self.anchors.len();
        let mut hasher = Hasher::new(STATIC_DOMAIN);
        hasher.update(&(members.len() as u64).to_le_bytes());
        let mut bound = Ok(());
        for &member in members {
            let ((item, instance), value, refs) = &nodes[member as usize];
            hasher.id(&item.0).id(instance).id(value).update(&(refs.len() as u64).to_le_bytes());
            for &target in refs {
                match target {
                    Target::Function(key) | Target::Direct(key) => match bind(key) {
                        Some(callee) => hasher.update(&[0]).id(&callee),
                        None => {
                            // alloc: an error, of a walk that missed a function a static names
                            bound = Err(format!("the static {item:?} refers to {key:?}, which the build has no instance of"));
                            continue;
                        }
                    },
                    Target::Static { item: to_item, instance: to_instance } => {
                        let Ok(to) = self.keys.binary_search(&(*to_item, *to_instance)) else {
                            // alloc: an error, of a walk that missed a static a static names
                            bound = Err(format!(
                                "the static {item:?} refers to the static {to_item:?} {to_instance:?}, which the build has none of"
                            ));
                            continue;
                        };
                        let theirs = self.component[to] as usize;
                        if theirs == component as usize {
                            let place = members.binary_search(&(to as u32)).expect("a member of its own cycle");
                            hasher.update(&[1]).update(&(place as u32).to_le_bytes())
                        } else {
                            match &self.bounds[theirs] {
                                Ok(it) => {
                                    let (from, to) = (self.anchor_starts[theirs] as usize, self.anchor_starts[theirs + 1] as usize);
                                    self.anchors.extend_from_within(from..to);
                                    hasher.update(&[6]).id(it)
                                }
                                Err(why) => {
                                    // clone: an error, of a static reaching one with none
                                    bound = Err(why.clone());
                                    continue;
                                }
                            }
                        }
                    }
                    Target::Data(id) => hasher.update(&[2]).id(id),
                    Target::Extern(name) => hasher.update(&[3]).part(name.as_bytes()),
                    Target::Weak(name) => hasher.update(&[4]).part(name.as_bytes()),
                    Target::Position(span) => {
                        self.anchors.push(span.anchor);
                        hasher.update(&[5]).id(&span.anchor).update(&span.index.to_le_bytes())
                    }
                    Target::Coverage(_) | Target::Counter(_) | Target::ProfileCounters => hasher.update(&[7]),
                };
            }
        }
        match bound {
            Ok(()) => {
                sort_unique_from(&mut self.anchors, start);
                self.bounds.push(Ok(hasher.finish()));
            }
            Err(why) => {
                self.anchors.truncate(start);
                self.bounds.push(Err(why));
            }
        }
        self.anchor_starts.push(self.anchors.len() as u32);
    }

    /// The bound of the static `item` `instance` and the anchors it names, transitively;
    /// `None` when the build has no such static.
    fn of(&self, item: &ItemKey, instance: &ContentId) -> Option<(&Result<ContentId, String>, &[ContentId])> {
        let at = self.keys.binary_search(&(*item, *instance)).ok()?;
        let component = self.component[at] as usize;
        let anchors = &self.anchors[self.anchor_starts[component] as usize..self.anchor_starts[component + 1] as usize];
        Some((&self.bounds[component], anchors))
    }
}

/// Sorts `out[start..]` and keeps each once.
fn sort_unique_from(out: &mut Vec<ContentId>, start: usize) {
    out[start..].sort_unstable();
    let mut kept = start;
    for at in start..out.len() {
        if kept == start || out[kept - 1] != out[at] {
            out[kept] = out[at];
            kept += 1;
        }
    }
    out.truncate(kept);
}

/// What a function's code names beyond its `Bound`, as [`placed`] takes it: into `anchors`
/// (cleared) every span anchor, sorted and each once, of the positions its code and its data
/// name, of its debug lines (a backtrace shows them) and those the statics it names name,
/// transitively; into `statics` (cleared) the bound of each static it names, in the order of
/// the statics, each once. `Err` is why a static it names has no bound.
fn named_by(
    function: &CompiledFunction,
    bounds: &StaticBounds,
    anchors: &mut Vec<ContentId>,
    statics: &mut Vec<(ItemKey, ContentId, ContentId)>,
) -> Result<(), String> {
    anchors.clear();
    statics.clear();
    let data = function.data.iter().flat_map(|it| it.relocs.targets());
    for target in function.relocs.targets().iter().chain(data) {
        match target {
            Target::Position(span) => anchors.push(span.anchor),
            Target::Static { item, instance } => {
                let Some((bound, named)) = bounds.of(item, instance) else {
                    // alloc: an error, of a build that missed a static its code names
                    return Err(format!("its code refers to the static {item:?} {instance:?}, which the build has none of"));
                };
                // clone: an error, of a static with no bound
                statics.push((*item, *instance, bound.clone()?));
                anchors.extend_from_slice(named);
            }
            _ => {}
        }
    }
    anchors.extend(function.lines.iter().map(|it| it.span.anchor));
    sort_unique_from(anchors, 0);
    statics.sort_unstable();
    statics.dedup();
    Ok(())
}

pub const SPAN_TABLE_DOMAIN: &str = "blaze v2 exec span table 1";

/// The content ID of each span table the link takes, found by its anchor: an open-addressed
/// table keyed by the anchor's first 8 bytes, which are uniform (BLAKE3).
struct SpanIds {
    /// Per slot, a place in `ids` plus one; zero for none.
    slots: Vec<u32>,
    ids: Vec<(ContentId, ContentId)>,
}

impl SpanIds {
    fn new(spans: &[(ContentId, Vec<u8>)]) -> SpanIds {
        use blaze_graph::par;
        assert!(spans.len() < u32::MAX as usize, "fewer span tables than 2^32 - 1");
        // alloc: once per test binary linked, an ID per span table
        let ids: Vec<(ContentId, ContentId)> = par::map(spans, |(anchor, bytes)| (*anchor, ContentId::of(SPAN_TABLE_DOMAIN, bytes)));
        let capacity = (ids.len() * 2).next_power_of_two().max(2);
        // alloc: once per test binary linked, the table's slots
        let mut slots = vec![0u32; capacity];
        let mask = capacity - 1;
        for (place, (anchor, _)) in ids.iter().enumerate() {
            let mut slot = SpanIds::start(anchor) & mask;
            while slots[slot] != 0 {
                slot = (slot + 1) & mask;
            }
            slots[slot] = place as u32 + 1;
        }
        SpanIds { slots, ids }
    }

    fn start(anchor: &ContentId) -> usize {
        u64::from_le_bytes(anchor.0[..8].try_into().expect("8 bytes")) as usize
    }

    /// The content ID of `anchor`'s table, if the link has one.
    fn get(&self, anchor: &ContentId) -> Option<ContentId> {
        let mask = self.slots.len() - 1;
        let mut slot = SpanIds::start(anchor) & mask;
        loop {
            let place = self.slots[slot];
            if place == 0 {
                return None;
            }
            let (at, id) = &self.ids[place as usize - 1];
            if at == anchor {
                return Some(*id);
            }
            slot = (slot + 1) & mask;
        }
    }
}

pub const PLACED_DOMAIN: &str = "blaze v2 exec placed 1";

/// A function's `Bound` with the values of the statics it names (by item, instance and
/// [`StaticBounds`]' bound, sorted) and the content of the span tables it names (`anchors`
/// sorted and each once): what [`ExecTable::fns`] keeps. `Err` is the first anchor of no table.
fn placed(
    bound: ContentId,
    statics: &[(ItemKey, ContentId, ContentId)],
    anchors: &[ContentId],
    spans: &SpanIds,
) -> Result<ContentId, ContentId> {
    let mut hasher = Hasher::new(PLACED_DOMAIN);
    hasher.id(&bound).update(&(statics.len() as u64).to_le_bytes());
    for (item, instance, value) in statics {
        hasher.id(&item.0).id(instance).id(value);
    }
    hasher.update(&(anchors.len() as u64).to_le_bytes());
    for anchor in anchors {
        let table = spans.get(anchor).ok_or(*anchor)?;
        hasher.id(anchor).id(&table);
    }
    Ok(hasher.finish())
}

/// Whether a binary built in `mode` has jump-table slots, so that its fork server records
/// what each test ran (`blaze_usercode::test::fork_server(true)`) and its link starts on the
/// execution log (`blaze_link::Output::exec_log`): a live build reaches its members through
/// slots and an in-process tiered run every function through the lazy table, where a file's
/// calls are direct.
pub fn records(mode: Mode) -> bool {
    matches!(mode, Mode::Live | Mode::Tiered)
}

/// A test binary's table.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct ExecTable {
    /// Every function of the binary, those dedupe merged away included, as `(FnId, Bound)`
    /// in `FnId` order: the order `blaze_testsel::Image::new` keys by, which a table already
    /// in checks with a compare per function (fastcompile-sol-select 2).
    pub fns: Vec<(ContentId, ContentId)>,
    /// Slot `n`'s function, an index into `fns`: one per slot of the binary's jump table.
    /// [`ExecTable::EMPTY`] for a slot an incremental link emptied, which no code names.
    pub slot_fn: Vec<u32>,
    /// The content ID of the code the binary runs that is in no table: the premapped
    /// dependency image; zero for none.
    pub beside: ContentId,
    /// Each test in the harness's order, with the functions it reaches as indices into `fns`,
    /// sorted.
    pub tests: Vec<(String, Vec<u32>)>,
}

/// Why a binary has no table.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ExecError {
    /// A function whose item has no bound hash, and why.
    Unbound { function: InstanceKey, why: String },
    /// Two functions of the binary are one function across versions.
    Twice { function: InstanceKey, other: InstanceKey },
    /// The link put a function in a slot past its jump table's.
    PastTable { function: InstanceKey, slot: u32, slots: u32 },
    /// The link named one slot for two functions.
    TwoInSlot { slot: u32, function: InstanceKey, other: InstanceKey },
    /// A test whose root is not one of the build's.
    NoRoot { test: String, root: usize },
    /// A function the build has no code of when its binary is linked (one compiled later),
    /// so the positions and statics it names are not known.
    NotCompiled { function: InstanceKey },
    /// A function whose code names the span table `anchor`, which the link was not given.
    NoSpans { function: InstanceKey, anchor: ContentId },
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecError::Unbound { function, why } => {
                write!(f, "{function:?} has no bound code, which test selection keys it by: {why}")
            }
            ExecError::Twice { function, other } => {
                write!(f, "{function:?} and {other:?} are one function across versions")
            }
            ExecError::PastTable { function, slot, slots } => {
                write!(f, "{function:?} is in slot {slot} of a jump table of {slots}")
            }
            ExecError::TwoInSlot { slot, function, other } => {
                write!(f, "slot {slot} is named for both {function:?} and {other:?}")
            }
            ExecError::NoRoot { test, root } => {
                write!(f, "the test `{test}` is at root {root}, which the build does not have")
            }
            ExecError::NotCompiled { function } => {
                write!(f, "{function:?} has no code in the build, so what positions and statics it names is not known")
            }
            ExecError::NoSpans { function, anchor } => {
                write!(f, "{function:?} names the span table {anchor:?}, which the link was not given")
            }
        }
    }
}

impl std::error::Error for ExecError {}

impl ExecTable {
    /// The function of a slot that runs none: one an incremental link emptied of a function
    /// gone from the binary, which nothing calls until another is put in it.
    pub const EMPTY: u32 = u32::MAX;

    /// The table of the test binary linked from `built`, whose link took the span tables
    /// `spans` (`hir_ty_codegen::v2::LinkInputs::spans`). Each function's `Bound` in
    /// [`ExecTable::fns`] is its [`Built::versions`] one with the values of the statics its
    /// code names and the content of the span tables of the positions and lines it names
    /// ([`placed`]): of its own code, or of the kept one's for one dedupe merged away. `slots`
    /// is how many slots its jump table has and `slot_of` the slot the link named for a
    /// function, if any (an incremental image's `blaze_link::Image::slot`); `beside` is what
    /// [`ExecTable::beside`] says; `tests` are the harness's tests in its order, each with its
    /// function's place in [`Built::roots`].
    pub fn new(
        built: &Built,
        spans: &[(ContentId, Vec<u8>)],
        slots: u32,
        slot_of: impl Fn(&InstanceKey) -> Option<u32>,
        beside: ContentId,
        tests: impl IntoIterator<Item = (String, usize)>,
    ) -> Result<ExecTable, ExecError> {
        use blaze_graph::par;
        assert_eq!(built.versions.len(), built.inputs.len(), "a build gives a version per instance");
        let count = built.inputs.len();
        assert!(count < ExecTable::EMPTY as usize, "fewer functions than `EMPTY`");
        let statics = StaticBounds::new(
            built.statics.iter().map(|it| ((it.item, it.instance), static_value(it), static_targets(it))),
            |key| built.inputs.binary_search_by_key(key, |it| it.0).ok().map(|at| built.versions[at].0),
        );
        let spans = SpanIds::new(spans);
        // Each function's on every core, in key order.
        // alloc: per worker, the anchors and statics of one function, reused
        // alloc: once per test binary, a bound per function
        let placed: Vec<Result<ContentId, ExecError>> = par::map_init(0..count, || (Vec::new(), Vec::new()), |(anchors, named), at| {
            let function = built.inputs[at].0;
            let bound = match &built.versions[at].1 {
                Ok(it) => *it,
                // clone: an error
                Err(why) => return Err(ExecError::Unbound { function, why: why.clone() }),
            };
            let runs = built.deduped.merged.get(&function).unwrap_or(&function);
            let Ok(code) = built.functions.binary_search_by_key(runs, |it| it.key) else {
                return Err(ExecError::NotCompiled { function });
            };
            named_by(&built.functions[code], &statics, anchors, named)
                .map_err(|why| ExecError::Unbound { function, why })?;
            placed(bound, named, anchors, &spans).map_err(|anchor| ExecError::NoSpans { function, anchor })
        });
        // alloc: once per test binary, the identity order
        let mut order: Vec<u32> = (0..count as u32).collect();
        // By place too, so that of two functions with one `FnId` the later is named.
        order.sort_unstable_by_key(|&at| (built.versions[at as usize].0, at));
        // alloc: once per test binary, the table it keeps
        let mut fns: Vec<(ContentId, ContentId)> = Vec::with_capacity(count);
        // alloc: once per test binary, each function's place in the table
        let mut rank = vec![0u32; count];
        for (place, &at) in order.iter().enumerate() {
            let key = built.inputs[at as usize].0;
            let id = &built.versions[at as usize].0;
            let bound = match &placed[at as usize] {
                Ok(it) => *it,
                // clone: an error
                Err(why) => return Err(why.clone()),
            };
            if fns.last().is_some_and(|last| last.0 == *id) {
                let other = built.inputs[order[place - 1] as usize].0;
                return Err(ExecError::Twice { function: key, other });
            }
            rank[at as usize] = place as u32;
            fns.push((*id, bound));
        }

        // alloc: once per test binary, a word per slot
        let mut slot_fn = vec![ExecTable::EMPTY; slots as usize];
        for (at, (key, _)) in built.inputs.iter().enumerate() {
            let Some(slot) = slot_of(key) else { continue };
            let Some(place) = slot_fn.get_mut(slot as usize) else {
                return Err(ExecError::PastTable { function: *key, slot, slots });
            };
            if *place != ExecTable::EMPTY {
                let other = built.inputs[order[*place as usize] as usize].0;
                return Err(ExecError::TwoInSlot { slot, function: *key, other });
            }
            *place = rank[at];
        }

        let mut walk = Walk::default();
        // alloc: once per test binary, a bit per function, cleared by each test that sets it
        let mut bits = vec![0u64; count.div_ceil(64)];
        // alloc: once per test binary, its tests
        let mut reached = Vec::new();
        for (test, root) in tests {
            let functions = built.roots.get(root).and_then(|key| built.reach.functions_of(key, &mut walk));
            let Some(mut functions) = functions else {
                return Err(ExecError::NoRoot { test, root });
            };
            in_table_order(&mut functions, &rank, &mut bits);
            reached.push((test, functions));
        }
        Ok(ExecTable { fns, slot_fn, beside, tests: reached })
    }
}

/// `functions`, places in key order, as places in the table, sorted: by a sort when they are
/// few, else by `bits` (all clear, and left so), as the table's order is no function of the
/// keys'.
fn in_table_order(functions: &mut Vec<u32>, rank: &[u32], bits: &mut [u64]) {
    for it in functions.iter_mut() {
        *it = rank[*it as usize];
    }
    if functions.len() * 4 < bits.len() {
        functions.sort_unstable();
        return;
    }
    for &it in functions.iter() {
        bits[it as usize / 64] |= 1 << (it % 64);
    }
    functions.clear();
    for (word, bits) in bits.iter_mut().enumerate() {
        while *bits != 0 {
            functions.push(word as u32 * 64 + bits.trailing_zeros());
            *bits &= *bits - 1;
        }
    }
}

#[cfg(test)]
mod tests;
