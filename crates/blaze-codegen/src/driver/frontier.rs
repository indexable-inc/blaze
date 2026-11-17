//! What a build's walk has reached, while it walks (docs/v2/plan/design/scheduler.md, rules 4
//! and 10 of its model): one entry a function (by the instance and whose code asks for it) and
//! one a static, in a lock-free table, each entry's facts in slots written once. The walk is a
//! worklist ([`super::worklist`]): a describe looks up each instance it refers to, makes the
//! entry of one that has none, and whoever made the entry sends it to be described. An entry is
//! made once, so an instance is described once, by whichever describe reached it first.
//!
//! Which describe that is depends on the schedule, and nothing of the build may. So the table
//! decides nothing but "described or not": once the worklist is empty, [`Order::of`] walks the
//! entries from the roots, breadth first, each entry's references in their order, and numbers
//! them. That order is a function of the roots and the describes alone, the same whichever
//! threads described what when. Everything that reads an order reads that one: which describe
//! a key keeps when two instances share it, the order of the build's errors, the order a lazy
//! build compiles in, the list the front end closes the program's facts over
//! ([`crate::backend::Frontend::closed`]), and the roots and edges of the recursion limit's
//! check ([`super::recursion::check`]).

#![forbid(unsafe_code)]

use std::{
    hash::Hash,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
};

use blaze_hash::SharedMap;

use super::{CheckRoot, FnNode, StaticNode, recursion::Step};
use crate::{
    CodegenError,
    backend::{Counted, FrontRef},
    plan::Origin,
    slot::Slot,
};

/// A function the walk reached: its key, and what the walk found of it, each written once.
pub(super) struct Function<I, S> {
    /// The instance and whose code asks for it: the entry's key.
    pub(super) asked: (I, Origin),
    /// The place on its path of what reached it first; `None` for a root. Of this run's
    /// schedule: it bounds what the worklist describes ([`Step`]) and decides nothing else.
    pub(super) from: Option<Arc<Step>>,
    /// What the recursion limit reads of it, written by whoever made the entry before it sent
    /// it on. Unset only for a function the front end gave no count, whose `made` is the error.
    pub(super) counted: Slot<Counted>,
    /// Sent back by the recursion limit's check, to be described though its path here is past
    /// the limit ([`super::recursion::check`]).
    pub(super) forced: AtomicBool,
    /// Its node, or why it has none, written by the one visit that describes it. Unset for a
    /// function past the limit on its path here, which nothing describes unless the check
    /// sends it back, and for one a stopped build never got to.
    pub(super) made: Slot<Result<FnNode<I, S>, CodegenError>>,
    /// Its number in the [`Order`] of pass `number >> 32`.
    number: AtomicU64,
}

/// A static the walk reached.
pub(super) struct Static<I, S> {
    pub(super) value: S,
    /// As [`Function::from`].
    pub(super) from: Option<Arc<Step>>,
    /// Its node, or why it has none: as [`Function::made`], and always written but in a
    /// stopped build.
    pub(super) made: Slot<Result<StaticNode<I, S>, CodegenError>>,
    number: AtomicU64,
}

/// Every function and static the walk reached, by key. Shared by the walk's describes on all
/// cores: a look is a lock-free read, an entry is made by one insert, and nothing is held
/// between.
pub(super) struct Frontier<I, S> {
    functions: SharedMap<(I, Origin), Arc<Function<I, S>>>,
    statics: SharedMap<S, Arc<Static<I, S>>>,
    /// How many [`Order`]s were made of it: each numbers the entries anew.
    orders: AtomicU32,
}

impl<I, S> Frontier<I, S>
where
    I: Clone + Eq + Hash + Send + Sync,
    S: Clone + Eq + Hash + Send + Sync,
{
    pub(super) fn new() -> Self {
        Frontier {
            functions: SharedMap::default(),
            statics: SharedMap::default(),
            orders: AtomicU32::new(0),
        }
    }

    /// Whether `asked` has an entry: a describe asks before it counts what it newly reaches.
    pub(super) fn has_function(&self, asked: &(I, Origin)) -> bool {
        self.functions.pin().contains_key(asked)
    }

    pub(super) fn has_static(&self, value: &S) -> bool {
        self.statics.pin().contains_key(value)
    }

    /// The entry of `asked`, if the walk reached it.
    pub(super) fn function(&self, asked: &(I, Origin)) -> Option<Arc<Function<I, S>>> {
        // clone: a reference count, once per look of an entry
        self.functions.pin().get(asked).cloned()
    }

    pub(super) fn static_(&self, value: &S) -> Option<Arc<Static<I, S>>> {
        // clone: a reference count, once per look of an entry
        self.statics.pin().get(value).cloned()
    }

    /// Makes the entry of `asked`, reached from `from`, unless it has one: `Some` for the
    /// caller that made it, which alone sends it to be described.
    pub(super) fn claim_function(
        &self,
        asked: (I, Origin),
        from: Option<&Arc<Step>>,
    ) -> Option<Arc<Function<I, S>>> {
        let functions = self.functions.pin();
        if functions.contains_key(&asked) {
            return None;
        }
        // alloc: per build, one per function the walk reaches
        let entry = Arc::new(Function {
            // clone: the walk's handle of the function, the entry's and the table's key
            asked: asked.clone(),
            // clone: a reference count
            from: from.cloned(),
            counted: Slot::new(),
            forced: AtomicBool::new(false),
            made: Slot::new(),
            number: AtomicU64::new(0),
        });
        // clone: a reference count: the table's and the caller's
        functions.try_insert(asked, entry.clone()).is_ok().then_some(entry)
    }

    /// As [`Self::claim_function`], of a static.
    pub(super) fn claim_static(
        &self,
        value: S,
        from: Option<&Arc<Step>>,
    ) -> Option<Arc<Static<I, S>>> {
        let statics = self.statics.pin();
        if statics.contains_key(&value) {
            return None;
        }
        // alloc: per build, one per static the walk reaches
        let entry = Arc::new(Static {
            // clone: the walk's handle of the static, the entry's and the table's key
            value: value.clone(),
            // clone: a reference count
            from: from.cloned(),
            made: Slot::new(),
            number: AtomicU64::new(0),
        });
        // clone: a reference count: the table's and the caller's
        statics.try_insert(value, entry.clone()).is_ok().then_some(entry)
    }
}

/// An entry of an [`Order`].
pub(super) enum Of<I, S> {
    Function(Arc<Function<I, S>>),
    Static(Arc<Static<I, S>>),
}

impl<I, S> Clone for Of<I, S> {
    fn clone(&self) -> Self {
        match self {
            // clone: a reference count
            Of::Function(it) => Of::Function(it.clone()),
            // clone: a reference count
            Of::Static(it) => Of::Static(it.clone()),
        }
    }
}

/// What the walk made of an item, as an [`Order`] found it.
pub(super) enum State {
    /// Not described: a function past the limit on the path the worklist reached it by
    /// (deferred, until the recursion limit's check sends it back), or anything a stopped
    /// build never got to.
    Unmade,
    /// Described: the items it refers to, in its references' order.
    Described(Vec<u32>),
    /// Its describe failed, which is an error of the build.
    Failed,
}

pub(super) struct Item<I, S> {
    pub(super) of: Of<I, S>,
    pub(super) state: State,
}

/// The walk's entries in the one order every build of the program gives them: numbered
/// breadth first from the roots, an entry's references in their order.
pub(super) struct Order<I, S> {
    /// Each entry the roots reach, numbered as first met: the build's roots (functions, then
    /// statics), the check's roots that are none of them, then level by level what the
    /// entries before refer to, each level's functions before its statics.
    pub(super) items: Vec<Item<I, S>>,
    /// The items in the order their references were followed: level by level, each level's
    /// functions before its statics. Where two orders matter (the describe a key keeps, the
    /// build's errors, a lazy build's order), it is this one.
    pub(super) followed: Vec<u32>,
    /// The roots of the recursion limit's check, each once: the [`CheckRoot`]s in their order,
    /// then each root of the build they leave out, in the build's order.
    pub(super) roots: Vec<u32>,
    /// A check root is none of the build's: what only it reaches is described for the check
    /// and left out of the build.
    pub(super) checks_more: bool,
    /// Which order of its frontier this is: what its entries' numbers are tagged with.
    pass: u32,
}

impl<I, S> Order<I, S>
where
    I: Clone + Eq + Hash + Send + Sync,
    S: Clone + Eq + Hash + Send + Sync,
{
    /// The order of what `frontier` holds, from the build's `roots` and `static_roots` and
    /// the check's `check`. Called between runs of the worklist, when no describe writes: it
    /// reads every entry once and looks up each reference of each.
    pub(super) fn of(
        frontier: &Frontier<I, S>,
        roots: &[(I, Origin)],
        static_roots: &[S],
        check: &[CheckRoot<I, S>],
    ) -> Order<I, S> {
        let mut order = Order {
            // alloc: per order, one per function and static the walk reached
            items: Vec::new(),
            followed: Vec::new(), // alloc: as above
            roots: Vec::new(),    // alloc: per order, one per root
            checks_more: false,
            pass: frontier.orders.fetch_add(1, Ordering::Relaxed) + 1,
        };
        // alloc: per order, a level's functions and statics, and the next level's
        let (mut functions, mut statics) = (Vec::new(), Vec::new());
        // alloc: per order, one per root of the build
        let mut built = Vec::with_capacity(roots.len() + static_roots.len());
        for root in roots {
            if let Some((item, true)) = order.function(frontier, root) {
                built.push(item);
                functions.push(item);
            }
        }
        for root in static_roots {
            if let Some((item, true)) = order.static_(frontier, root) {
                built.push(item);
                statics.push(item);
            }
        }
        for root in check {
            match root {
                CheckRoot::Function(instance, origin) => {
                    // clone: per order, the walk's handle of a check root
                    let asked = (instance.clone(), *origin);
                    let Some((item, new)) = order.function(frontier, &asked) else { continue };
                    if new {
                        order.checks_more = true;
                        functions.push(item);
                    }
                    order.roots.push(item);
                }
                CheckRoot::Static(value) => {
                    let Some((item, new)) = order.static_(frontier, value) else { continue };
                    if new {
                        order.checks_more = true;
                        statics.push(item);
                    }
                    order.roots.push(item);
                }
            }
        }
        order.roots.extend(built);
        // alloc: per order, one flag per root
        let mut seen = vec![false; order.items.len()];
        order.roots.retain(|&it| !std::mem::replace(&mut seen[it as usize], true));

        // alloc: per order, the next level
        let (mut next_functions, mut next_statics) = (Vec::new(), Vec::new());
        while !(functions.is_empty() && statics.is_empty()) {
            for &item in functions.iter().chain(&statics) {
                order.followed.push(item);
                // clone: a reference count, so the entry is read while the order grows
                let of = order.items[item as usize].of.clone();
                let (refs, requester) = match &of {
                    Of::Function(entry) => match entry.made.get() {
                        Some(Ok(node)) => (&node.described.refs, node.described.origin),
                        Some(Err(_)) => {
                            order.items[item as usize].state = State::Failed;
                            continue;
                        }
                        None => continue,
                    },
                    Of::Static(entry) => match entry.made.get() {
                        Some(Ok(node)) => (&node.described.refs, node.described.origin),
                        Some(Err(_)) => {
                            order.items[item as usize].state = State::Failed;
                            continue;
                        }
                        None => continue,
                    },
                };
                // alloc: per order, one per reference of a function or static
                let mut edges = Vec::with_capacity(refs.len());
                for it in refs {
                    // A reference with no entry is one whose describe was not sent on (the
                    // build stopped): no edge, and the build's error says why.
                    match it {
                        FrontRef::Function(callee) => {
                            // clone: the walk's handle of a callee; who asks for it is a copy
                            let asked = (callee.clone(), requester);
                            if let Some((to, new)) = order.function(frontier, &asked) {
                                edges.push(to);
                                if new {
                                    next_functions.push(to);
                                }
                            }
                        }
                        FrontRef::Static(value) => {
                            if let Some((to, new)) = order.static_(frontier, value) {
                                edges.push(to);
                                if new {
                                    next_statics.push(to);
                                }
                            }
                        }
                        FrontRef::Extern(_) | FrontRef::Weak(_) => {}
                    }
                }
                order.items[item as usize].state = State::Described(edges);
            }
            functions.clear();
            statics.clear();
            std::mem::swap(&mut functions, &mut next_functions);
            std::mem::swap(&mut statics, &mut next_statics);
        }
        order
    }

    /// The number of `asked`'s entry, and whether this call gave it: `None` for no entry.
    fn function(&mut self, frontier: &Frontier<I, S>, asked: &(I, Origin)) -> Option<(u32, bool)> {
        let entry = frontier.function(asked)?;
        match self.numbered(&entry.number) {
            Ok(item) => Some((item, false)),
            Err(item) => {
                self.items.push(Item { of: Of::Function(entry), state: State::Unmade });
                Some((item, true))
            }
        }
    }

    fn static_(&mut self, frontier: &Frontier<I, S>, value: &S) -> Option<(u32, bool)> {
        let entry = frontier.static_(value)?;
        match self.numbered(&entry.number) {
            Ok(item) => Some((item, false)),
            Err(item) => {
                self.items.push(Item { of: Of::Static(entry), state: State::Unmade });
                Some((item, true))
            }
        }
    }

    /// The number this order gave an entry (`Ok`), or the one it gives it now (`Err`: the
    /// next item's, which the caller pushes). Only the one thread that makes the order
    /// writes a number, between runs of the worklist.
    fn numbered(&self, number: &AtomicU64) -> Result<u32, u32> {
        let tagged = number.load(Ordering::Relaxed);
        if tagged >> 32 == u64::from(self.pass) {
            return Ok(tagged as u32);
        }
        let item = self.items.len() as u32;
        number.store(u64::from(self.pass) << 32 | u64::from(item), Ordering::Relaxed);
        Err(item)
    }

    /// The function of item `item` and whose code asked for it; `None` for a static.
    pub(super) fn asked(&self, item: u32) -> Option<&(I, Origin)> {
        match &self.items.get(item as usize)?.of {
            Of::Function(entry) => Some(&entry.asked),
            Of::Static(_) => None,
        }
    }

    /// What the recursion limit reads of item `item`; `None` for a static, and for a function
    /// the front end gave no count.
    pub(super) fn counted(&self, item: u32) -> Option<Counted> {
        match &self.items.get(item as usize)?.of {
            Of::Function(entry) => entry.counted.get().copied(),
            Of::Static(_) => None,
        }
    }

    /// Every function the walk counted, by whose code asked for it, with its count, in the
    /// order's: what a kept walk holds its functions to ([`super::patch`]).
    pub(super) fn counts(&self) -> Vec<((I, Origin), Counted)> {
        // alloc: per build that keeps its walk, one per function
        (0..self.items.len() as u32)
            // clone: per build that keeps its walk, the walk's handle of each function
            .filter_map(|item| Some((self.asked(item)?.clone(), self.counted(item)?)))
            .collect()
    }

    /// Every function (with whose code asked for it) and static of the order, each once, in
    /// the order their references were followed: what the front end closes the program's
    /// facts over.
    pub(super) fn reached(&self) -> (Vec<(I, Origin)>, Vec<S>) {
        // alloc: per walk, one per function and static it reached
        let (mut functions, mut statics) = (Vec::new(), Vec::new());
        for &item in &self.followed {
            match &self.items[item as usize].of {
                // clone: per walk, its handle of each function and static it reached
                Of::Function(entry) => functions.push(entry.asked.clone()),
                Of::Static(entry) => statics.push(entry.value.clone()), // clone: as above
            }
        }
        (functions, statics)
    }
}
