//! rustc's recursion limit over the build's walk (`rustc_monomorphize::collector`'s
//! `check_recursion_limit`): an instance whose body already appears more than the limit times
//! along the path it was reached by (drop glue counting a quarter) stops the build with an error
//! naming it, as a chain of instances whose arguments grow with each call
//! (`fn f<T>(x: T) { f((x,)) }`) never ends. Nothing else in the build bounds such a chain.
//!
//! Which path counts. rustc walks depth first from each root in turn, claims every neighbor of
//! an instance when it lists them, before it visits any, and counts an instance on the path it
//! was claimed on. The build's walk describes level by level, in parallel; the path it reaches an
//! instance by first is the one of fewest steps, which can hold more repeats of a body than
//! rustc's (a long chain claims what a short one reaches later in rustc's order), so it never
//! decides a refusal. [`Graph::check`] does: rustc's walk, over the graph the levels described,
//! in root and reference order. The levels stop describing an instance past the limit along
//! their own path ([`Graph::level`] defers it), which bounds a chain that grows forever; one the
//! check reaches within the limit is sent back to be described, and the check runs again over the
//! larger graph, until it reaches no undescribed instance. Only the check's last run errs.
//!
//! Which roots, in which order. As rustc's walk claims first what its first roots reach, the
//! order of the roots decides what builds. The check walks from the [`CheckRoot`]s the build is
//! given, in their order, then from each root of the build they leave out, in the build's order.
//! `hir-ty-codegen`'s roots give rustc's root collector's (`collect_roots`): the crate's free
//! items in definition order, its `main` and every static of it among them (a static no code
//! names is described for what its value refers to, never built for it: what only a check root
//! reaches is left out of the build), then its impls' items, and `lang_start` last; the
//! allocator shim and other crates' kept symbols, which rustc collects in no walk or in their
//! own crate's, follow.
//!
//! Which crate's limit. rustc collects a crate's code in that crate's build, with its limit; a
//! function of another crate that is not generic and not `#[inline]` is collected in its own
//! crate's build. Here a path is held to its head's crate's limit, and such a function reached
//! from another crate's path heads a path of its own, held to the larger of the two limits
//! ([`Counted::heads_own_path`]), so what builds is a superset of what rustc builds.
//!
//! Where it is not rustc's: the order of an instance's references is the front end's
//! ([`crate::backend::Described::refs`]: the union over settings, inlined callees appended),
//! where rustc's is its optimized MIR's; definition order is the front end's declaration order
//! (a module's impls after its other items, items a macro expands to where it expands them); a
//! dependency's function heads its own path from where the program reaches it, where rustc's
//! build of that crate walks it from all of that crate's roots in their order; one instance asked
//! for by two crates' code is two (rustc has a visited set per crate's build); and rustc's
//! `share-generics` skips a generic instance a dependency already built, which is walked here.
//! Compile identity never reads a path (it is per instance, `hir_ty_identity`'s
//! `SHARED_DEPTH`), so a path is never in a key.

use std::{alloc::Global, hash::Hash};

use blaze_ids::ContentId;

use super::CheckRoot;
use crate::{
    CodegenError, InstanceKey,
    backend::{Counted, FrontRef, Frontend, Limit},
    plan::Origin,
};
use blaze_items::ItemKey;

/// No node, or no item: what a root is reached from.
pub(super) const NONE: u32 = u32::MAX;

/// How many times [`Graph::check`] may send instances back to be described before it stops
/// with an error: each time describes at least one more, of the finitely many within the limit.
const PASSES: u32 = 1 << 16;

/// An instance or static on its path.
struct Node {
    /// What it was reached from on its path; [`NONE`] at the head.
    parent: u32,
    body: Option<ContentId>,
    /// How many times its body appears above it on its path: rustc's `recursion_depth`.
    depth: u32,
    /// The path's limit; `None` past a head of no crate, whose reaches head their own paths.
    limit: Option<Limit>,
    /// The item that heads the path.
    head: u32,
    /// The item whose crate's limit [`Self::limit`] is.
    limit_of: u32,
}

/// An instance placed past its path's limit.
struct Past {
    item: u32,
    depth: u32,
    limit: u32,
    head: u32,
    limit_of: u32,
}

/// Each instance and static on the path it was reached by.
#[derive(Default)]
struct Paths {
    nodes: Vec<Node>,
}

impl Paths {
    /// Puts function `item` on the path of node `parent`: a node, or past the limit (only
    /// without `keep_past`).
    fn place(
        &mut self,
        item: u32,
        counted: &Counted,
        parent: u32,
        keep_past: bool,
    ) -> Result<u32, Past> {
        let up =
            self.nodes.get(parent as usize).and_then(|node| node.limit.map(|limit| (node, limit)));
        let (parent, limit, head, limit_of) = match (up, counted.limit) {
            // Collected in its own crate's build: a path of its own, held to the larger limit.
            (Some((up, path)), Some(own)) if counted.heads_own_path && own.krate != path.krate => {
                match own.value >= path.value {
                    true => (NONE, Some(own), item, item),
                    false => (
                        NONE,
                        Some(Limit { krate: own.krate, value: path.value }),
                        item,
                        up.limit_of,
                    ),
                }
            }
            (Some((up, path)), _) => (parent, Some(path), up.head, up.limit_of),
            (None, own) => (NONE, own, item, item),
        };
        let mut depth = 0;
        if let Some(body) = counted.body {
            // Parents are placed before what they reach, so the climb ends.
            let mut above = parent;
            while let Some(node) = self.nodes.get(above as usize) {
                if node.body == Some(body.body) {
                    depth = node.depth + 1;
                    break;
                }
                above = node.parent;
            }
            let counted = if body.drop_glue { depth / 4 } else { depth };
            if !keep_past
                && let Some(limit) = limit
                && counted > limit.value
            {
                return Err(Past { item, depth, limit: limit.value, head, limit_of });
            }
        }
        self.nodes.push(Node {
            parent,
            body: counted.body.map(|it| it.body),
            depth,
            limit,
            head,
            limit_of,
        });
        Ok(self.nodes.len() as u32 - 1)
    }

    /// Puts a static on the path of node `parent`: no body, on that path, or heading none.
    fn place_static(&mut self, parent: u32) -> u32 {
        let (limit, head, limit_of) = match self.nodes.get(parent as usize) {
            Some(node) => (node.limit, node.head, node.limit_of),
            None => (None, NONE, NONE),
        };
        self.nodes.push(Node { parent, body: None, depth: 0, limit, head, limit_of });
        self.nodes.len() as u32 - 1
    }
}

enum State {
    /// In a level, not yet described.
    Queued,
    /// Past the limit on the levels' path from node `from`, so not described by them.
    Deferred { from: u32 },
    /// Described: the items it refers to, in its references' order.
    Described(Vec<u32>),
    /// Its description failed, which is an error of the build already.
    Failed,
}

struct Item<I> {
    /// The function and whose code asks for it; `None` for a static.
    function: Option<(I, Origin)>,
    counted: Option<Counted>,
    state: State,
    /// Sent back by the check: the levels place it past their limit.
    forced: bool,
}

/// One level of the walk: the functions (with whose code asks for each) and statics to
/// describe, each with its item and the node it was reached from on the levels' path ([`NONE`]
/// for a root); after [`Graph::level`], its own node.
pub(super) struct Level<I, S> {
    pub(super) functions: Vec<(I, Origin)>,
    pub(super) function_items: Vec<u32>,
    pub(super) function_from: Vec<u32>,
    pub(super) statics: Vec<S>,
    pub(super) static_items: Vec<u32>,
    pub(super) static_from: Vec<u32>,
}

impl<I, S> Level<I, S> {
    pub(super) fn new() -> Self {
        Level {
            // alloc: none until pushed; per build, one per instance of a level
            functions: Vec::new(),
            function_items: Vec::new(), // alloc: as above
            function_from: Vec::new(),  // alloc: as above
            statics: Vec::new(),        // alloc: as above
            static_items: Vec::new(),   // alloc: as above
            static_from: Vec::new(),    // alloc: as above
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.functions.is_empty() && self.statics.is_empty()
    }
}

/// Every function (by whose code asks for it) and static the walk reached, what each refers
/// to, and the levels' paths.
pub(super) struct Graph<I, S> {
    functions: blaze_hash::HashMap<(I, Origin), u32, Global>,
    statics: blaze_hash::HashMap<S, u32, Global>,
    items: Vec<Item<I>>,
    /// The check's roots, each once: the [`CheckRoot`]s in their order, then each root of
    /// the build they leave out, in the build's order.
    roots: Vec<u32>,
    /// A check root is none of the build's: what only it reaches is described for the check,
    /// and left out of the build ([`Self::checks_more`]).
    checks_more: bool,
    /// The levels' paths: a bound on what they describe, never an error.
    levels: Paths,
    /// How many times the check sent instances back to be described.
    passes: u32,
}

impl<I: Clone + Eq + Hash + std::fmt::Debug, S: Clone + Eq + Hash> Graph<I, S> {
    pub(super) fn new() -> Self {
        Graph {
            functions: blaze_hash::HashMap::new_in(Global),
            statics: blaze_hash::HashMap::new_in(Global),
            // alloc: per build, one per function and static the walk reaches
            items: Vec::new(),
            roots: Vec::new(), // alloc: per build, one per root
            checks_more: false,
            levels: Paths::default(),
            passes: 0,
        }
    }

    /// The first level: each root of the build once, in order, then each check root that is
    /// none of them ([`CheckRoot`]). Also for the walk's next round, which reaches everything
    /// again.
    pub(super) fn start(
        &mut self,
        roots: &[(I, Origin)],
        static_roots: &[S],
        check: &[CheckRoot<I, S>],
    ) -> Level<I, S> {
        self.functions.clear();
        self.statics.clear();
        self.items.clear();
        self.roots.clear();
        self.checks_more = false;
        self.levels.nodes.clear();
        self.passes = 0;
        let mut level = Level::new();
        // alloc: per build, one per root of the build
        let mut built = Vec::with_capacity(roots.len() + static_roots.len());
        for root in roots {
            let (item, new) = self.function(root);
            if new {
                built.push(item);
                // clone: per build, the walk's handle of a root
                level.functions.push(root.clone());
                level.function_items.push(item);
                level.function_from.push(NONE);
            }
        }
        for root in static_roots {
            let (item, new) = self.static_item(root);
            if new {
                built.push(item);
                // clone: per build, the walk's handle of a static root
                level.statics.push(root.clone());
                level.static_items.push(item);
                level.static_from.push(NONE);
            }
        }
        for root in check {
            match root {
                CheckRoot::Function(instance, origin) => {
                    // clone: per build, the walk's handle of a check root
                    let asked = (instance.clone(), *origin);
                    let (item, new) = self.function(&asked);
                    if new {
                        self.checks_more = true;
                        level.functions.push(asked);
                        level.function_items.push(item);
                        level.function_from.push(NONE);
                    }
                    self.roots.push(item);
                }
                CheckRoot::Static(value) => {
                    let (item, new) = self.static_item(value);
                    if new {
                        self.checks_more = true;
                        // clone: per build, the walk's handle of a static check root
                        level.statics.push(value.clone());
                        level.static_items.push(item);
                        level.static_from.push(NONE);
                    }
                    self.roots.push(item);
                }
            }
        }
        self.roots.extend(built);
        // alloc: per build, one flag per root
        let mut seen = vec![false; self.items.len()];
        self.roots.retain(|&it| !std::mem::replace(&mut seen[it as usize], true));
        level
    }

    /// Whether a check root is none of the build's roots: what the walk described then holds
    /// what only such a root reaches, which the build leaves out.
    pub(super) fn checks_more(&self) -> bool {
        self.checks_more
    }

    fn function(&mut self, asked: &(I, Origin)) -> (u32, bool) {
        if let Some(item) = self.functions.get(asked) {
            return (*item, false);
        }
        let item = self.items.len() as u32;
        // clone: per build, the walk's handle of a function and who asks for it, twice: its
        // item's and the map's
        self.functions.insert(asked.clone(), item);
        self.items.push(Item {
            function: Some(asked.clone()),
            counted: None,
            state: State::Queued,
            forced: false,
        });
        (item, true)
    }

    fn static_item(&mut self, value: &S) -> (u32, bool) {
        if let Some(item) = self.statics.get(value) {
            return (*item, false);
        }
        let item = self.items.len() as u32;
        // clone: per build, the walk's handle of a static
        self.statics.insert(value.clone(), item);
        self.items.push(Item {
            function: None,
            counted: None,
            state: State::Queued,
            forced: false,
        });
        (item, true)
    }

    /// Puts each of `level` on the levels' path before it is described. A function past the
    /// limit there is deferred and taken out of the level, never described by the levels; its
    /// refusal is the check's ([`Self::check`]). Each kept one's `from` then holds its own node.
    /// A function the front end gave no [`Counted`] is an error in `errors`, never described.
    pub(super) fn level<F: Frontend<Instance = I>>(
        &mut self,
        frontend: &F,
        level: &mut Level<I, S>,
        errors: &mut Vec<CodegenError>,
    ) {
        debug_assert_eq!(level.functions.len(), level.function_from.len());
        let counted = {
            let _span = tracing::info_span!(
                target: "blaze",
                "recursion limit: count a level",
                functions = level.functions.len(),
            )
            .entered();
            frontend.counted(&level.functions)
        };
        if counted.len() != level.functions.len() {
            errors.push(miscounted(frontend, &level.functions, counted.len()));
        }
        let mut kept = 0;
        for at in 0..level.functions.len() {
            let (item, from) = (level.function_items[at], level.function_from[at]);
            let Some(&counted) = counted.get(at) else {
                // Named by the error above: never described, so never built.
                self.items[item as usize].state = State::Failed;
                continue;
            };
            let entry = &mut self.items[item as usize];
            entry.counted = Some(counted);
            match self.levels.place(item, &counted, from, entry.forced) {
                Ok(node) => {
                    level.functions.swap(kept, at);
                    level.function_items[kept] = item;
                    level.function_from[kept] = node;
                    kept += 1;
                }
                Err(_) => self.items[item as usize].state = State::Deferred { from },
            }
        }
        level.functions.truncate(kept);
        level.function_items.truncate(kept);
        level.function_from.truncate(kept);
        for from in &mut level.static_from {
            *from = self.levels.place_static(*from);
        }
    }

    /// Item `item` was described with `refs`, asked for by `requester`'s code, at node `at` of
    /// the levels' path: each function and static it refers to that is new goes to `next`.
    pub(super) fn described(
        &mut self,
        item: u32,
        refs: &[FrontRef<I, S>],
        requester: &Origin,
        at: u32,
        next: &mut Level<I, S>,
    ) {
        // alloc: per build, one per reference of an instance or static
        let mut edges = Vec::with_capacity(refs.len());
        for it in refs {
            match it {
                FrontRef::Function(callee) => {
                    // clone: per build, the walk's handle of a callee; who asks for it is a copy
                    let asked = (callee.clone(), *requester);
                    let (callee, new) = self.function(&asked);
                    edges.push(callee);
                    if new {
                        next.functions.push(asked);
                        next.function_items.push(callee);
                        next.function_from.push(at);
                    }
                }
                FrontRef::Static(value) => {
                    let (value_item, new) = self.static_item(value);
                    edges.push(value_item);
                    if new {
                        // clone: per build, the walk's handle of a static
                        next.statics.push(value.clone());
                        next.static_items.push(value_item);
                        next.static_from.push(at);
                    }
                }
                FrontRef::Extern(_) | FrontRef::Weak(_) => {}
            }
        }
        self.items[item as usize].state = State::Described(edges);
    }

    /// Every function the walk counted, by whose code asked for it, with its count: what a
    /// kept walk holds its functions to ([`super::patch`]).
    pub(super) fn counts(&self) -> Vec<((I, Origin), Counted)> {
        // clone: per build that keeps its walk, the walk's handle of each function
        let counted = |it: &Item<I>| Some((it.function.clone()?, it.counted?));
        // alloc: per build that keeps its walk, one per function
        self.items.iter().filter_map(counted).collect()
    }

    /// Item `item`'s description failed.
    pub(super) fn failed(&mut self, item: u32) {
        self.items[item as usize].state = State::Failed;
    }

    /// rustc's walk over the graph described so far, once the levels have run out: from each
    /// root in order, depth first, claiming each neighbor of an instance as it lists them and
    /// placing it on the path of that instance. A deferred instance it claims within the limit
    /// is put in `next`, to be described, and `true` returned: the check runs again once the
    /// levels have run out again. Otherwise each instance claimed past the limit is an error.
    pub(super) fn check<F: Frontend<Instance = I>>(
        &mut self,
        frontend: &F,
        next: &mut Level<I, S>,
        errors: &mut Vec<CodegenError>,
    ) -> bool {
        struct Frame {
            /// Its claimed neighbors are `claimed[start..end]`; `next` is the one to visit.
            start: u32,
            end: u32,
            next: u32,
        }
        let _span = tracing::info_span!(
            target: "blaze",
            "recursion limit check",
            pass = self.passes + 1,
            items = self.items.len(),
            roots = self.roots.len(),
        )
        .entered();
        let mut paths = Paths::default();
        // alloc: per check, one flag per item
        let mut visited = vec![false; self.items.len()];
        // alloc: per check, the claimed neighbors of the instances on the path
        let mut claimed: Vec<(u32, u32)> = Vec::new();
        let mut stack: Vec<Frame> = Vec::new(); // alloc: per check, the path
        let mut past: Vec<Past> = Vec::new(); // alloc: per check, one per instance past the limit
        let mut undescribed: Vec<u32> = Vec::new(); // alloc: per check, one per instance to describe
        for root in 0..self.roots.len() {
            let root = self.roots[root];
            if std::mem::replace(&mut visited[root as usize], true) {
                continue;
            }
            match self.claim(&mut paths, root, NONE) {
                Some(Ok(node)) => claimed.push((root, node)),
                Some(Err(it)) => past.push(it),
                None => undescribed.push(root),
            }
            // The root is the one claimed: visit it as a frame of one.
            if claimed.len() == 1 {
                stack.push(Frame { start: 0, end: 1, next: 0 });
            }
            while let Some(top) = stack.last_mut() {
                if top.next == top.end {
                    claimed.truncate(top.start as usize);
                    stack.pop();
                    continue;
                }
                let (item, node) = claimed[top.next as usize];
                top.next += 1;
                match &self.items[item as usize].state {
                    State::Described(edges) => {
                        let start = claimed.len() as u32;
                        for &edge in edges {
                            if std::mem::replace(&mut visited[edge as usize], true) {
                                continue;
                            }
                            match self.claim(&mut paths, edge, node) {
                                Some(Ok(node)) => claimed.push((edge, node)),
                                Some(Err(it)) => past.push(it),
                                None => undescribed.push(edge),
                            }
                        }
                        stack.push(Frame { start, end: claimed.len() as u32, next: start });
                    }
                    State::Failed => {}
                    State::Queued | State::Deferred { .. } => undescribed.push(item),
                }
            }
        }
        let mut more = false;
        for item in undescribed {
            let entry = &mut self.items[item as usize];
            let (State::Deferred { from }, Some(function)) = (&entry.state, &entry.function) else {
                continue;
            };
            // clone: per build, the walk's handle of a deferred function, once
            next.functions.push(function.clone());
            next.function_items.push(item);
            next.function_from.push(*from);
            entry.state = State::Queued;
            entry.forced = true;
            more = true;
        }
        if more {
            self.passes += 1;
            if self.passes <= PASSES {
                return true;
            }
            let (instance, _) = &next.functions[0];
            errors.push(CodegenError {
                unit: InstanceKey(ContentId::default()),
                item: ItemKey(ContentId::default()),
                // alloc: the error's message
                what: format!(
                    "the recursion limit's check sent instances back to be described {PASSES} \
                     times without reaching them all, the last one `{}`",
                    frontend.spell(instance)
                ),
                front: None,
            });
            next.functions.clear();
            next.function_items.clear();
            next.function_from.clear();
            return false;
        }
        let spelled =
            |item: u32| match self.items.get(item as usize).and_then(|it| it.function.as_ref()) {
                Some((instance, _)) => format!("`{}`", frontend.spell(instance)), // alloc: an error's text
                None if item == NONE => "none".to_owned(), // alloc: an error's text
                None => "a static".to_owned(),             // alloc: an error's text
            };
        for it in &past {
            let instance = |item: u32| {
                self.items.get(item as usize).and_then(|it| it.function.as_ref()).map(|it| &it.0)
            };
            let (Some(at), Some(head), Some(limit_of)) =
                (instance(it.item), instance(it.head), instance(it.limit_of))
            else {
                // A path's head and the item whose limit holds it are functions by construction
                // ([`Paths::place`]); were one not, the refusal still stands, named as it is.
                errors.push(CodegenError {
                    unit: InstanceKey(ContentId::default()),
                    item: ItemKey(ContentId::default()),
                    // alloc: the error's message
                    what: format!(
                        "{} repeats its body {} times on its path, past {}; the path's head is {} \
                         and the limit is that of {}'s crate, which `Paths::place` makes functions",
                        spelled(it.item),
                        it.depth,
                        it.limit,
                        spelled(it.head),
                        spelled(it.limit_of)
                    ),
                    front: None,
                });
                continue;
            };
            errors.push(frontend.past_recursion_limit(at, it.depth, it.limit, head, limit_of));
        }
        if past.is_empty()
            && let Some(left) =
                self.items.iter().position(|it| matches!(it.state, State::Deferred { .. }))
        {
            errors.push(CodegenError {
                unit: InstanceKey(ContentId::default()),
                item: ItemKey(ContentId::default()),
                // alloc: the error's message
                what: format!(
                    "the recursion limit's check never reached {}, which the build's walk \
                     deferred past the limit, though it refused nothing",
                    spelled(left as u32)
                ),
                front: None,
            });
        }
        false
    }

    /// Places `item` on the check's path of node `parent`: `None` for a function not yet
    /// counted (never leveled).
    fn claim(&self, paths: &mut Paths, item: u32, parent: u32) -> Option<Result<u32, Past>> {
        let entry = &self.items[item as usize];
        match entry.function {
            None => Some(Ok(paths.place_static(parent))),
            Some(_) => entry.counted.map(|counted| paths.place(item, &counted, parent, false)),
        }
    }
}

/// The error of a front end whose [`Frontend::counted`] gave `given` entries for a level of
/// `functions`: naming the front end and the functions left without one.
fn miscounted<I, F: Frontend<Instance = I>>(
    frontend: &F,
    functions: &[(I, Origin)],
    given: usize,
) -> CodegenError {
    /// How many functions left without an entry the error names before it says how many more.
    const NAMED: usize = 8;
    let left = functions.get(given..).unwrap_or_default();
    // alloc: the error's message
    let mut names: Vec<String> =
        left.iter().take(NAMED).map(|(it, _)| format!("`{}`", frontend.spell(it))).collect();
    if left.len() > NAMED {
        names.push(format!("and {} more", left.len() - NAMED)); // alloc: the error's message
    }
    let which = match left.is_empty() {
        true => "more than there are functions".to_owned(), // alloc: the error's message
        false => format!("none for {}", names.join(", ")),  // alloc: the error's message
    };
    CodegenError {
        unit: InstanceKey(ContentId::default()),
        item: ItemKey(ContentId::default()),
        // alloc: the error's message
        what: format!(
            "the front end `{}` gave {given} recursion limit entries (`Frontend::counted`) for a \
             level of {} functions, one a function: {which}",
            std::any::type_name::<F>(),
            functions.len(),
        ),
        front: None,
    }
}
