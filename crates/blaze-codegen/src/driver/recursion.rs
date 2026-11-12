//! rustc's recursion limit over the build's walk (`rustc_monomorphize::collector`'s
//! `check_recursion_limit`): an instance whose body already appears more than the limit times
//! along the path it was reached by (drop glue counting a quarter) stops the build with an error
//! naming it, as a chain of instances whose arguments grow with each call
//! (`fn f<T>(x: T) { f((x,)) }`) never ends. Nothing else in the build bounds such a chain.
//!
//! Which path counts. rustc walks depth first from each root in turn, claims every neighbor of
//! an instance when it lists them, before it visits any, and counts an instance on the path it
//! was claimed on. The build's walk is a worklist, its describes on all cores; the path it
//! reaches an instance by is the one of whichever describe reached it first, which is of this
//! run's schedule and can hold more or fewer repeats of a body than rustc's, so it never
//! decides a refusal. [`check`] does: rustc's walk, over the graph the worklist described, in
//! root and reference order. The worklist stops describing an instance past the limit along
//! its own path (each instance carries its place on it, a [`Step`], in place of a level), which
//! bounds a chain that grows forever; one the check reaches within the limit is sent back to
//! be described, and the check runs again over the larger graph, until it reaches no
//! undescribed instance. Only the check's last run errs, and that run is over a graph in which
//! every instance it claims within the limit is described: the same graph, the same claims and
//! the same errors whatever the schedule was.
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

use std::{
    hash::Hash,
    sync::{Arc, atomic::Ordering},
};

use blaze_ids::ContentId;

use super::frontier::{Function, Of, Order, State};
use crate::{
    CodegenError, InstanceKey,
    backend::{Counted, Frontend, Limit},
    plan::Origin,
};
use blaze_items::ItemKey;

/// No node, or no item: what a root is reached from.
pub(super) const NONE: u32 = u32::MAX;

/// How many times [`check`] may send instances back to be described before it stops with an
/// error: each time describes at least one more, of the finitely many within the limit.
const PASSES: u32 = 1 << 16;

/// An instance's place on the path the worklist reached it by: what it carries in place of a
/// level, and hands to what it reaches. Shared by every instance below it on its path, which
/// hold it for as long as they are walked.
pub(super) struct Step {
    /// What it was reached from; `None` at the head of its path.
    parent: Option<Arc<Step>>,
    body: Option<ContentId>,
    /// How many times its body appears above it on its path: rustc's `recursion_depth`.
    depth: u32,
    /// The path's limit; `None` past a head of no crate, whose reaches head their own paths.
    limit: Option<Limit>,
}

impl Step {
    /// A function counted as `counted`, reached from `parent` (`None` for a root): its place
    /// on that path, or on a path of its own ([`Counted::heads_own_path`]), and whether that
    /// place is past the path's limit. [`Paths::place`]'s rule, over a path that is shared
    /// rather than indexed.
    fn at(parent: Option<&Arc<Step>>, counted: &Counted) -> (Step, bool) {
        let up = parent.and_then(|node| node.limit.map(|limit| (node, limit)));
        let (parent, limit) = match (up, counted.limit) {
            // Collected in its own crate's build: a path of its own, held to the larger limit.
            (Some((_, path)), Some(own)) if counted.heads_own_path && own.krate != path.krate => {
                (None, Some(Limit { krate: own.krate, value: own.value.max(path.value) }))
            }
            (Some((up, path)), _) => (Some(up), Some(path)),
            (None, own) => (None, own),
        };
        let (mut depth, mut past) = (0, false);
        if let Some(body) = counted.body {
            // Each step's parent was placed before it, so the climb ends at the head.
            let mut above = parent;
            while let Some(node) = above {
                if node.body == Some(body.body) {
                    depth = node.depth + 1;
                    break;
                }
                above = node.parent.as_ref();
            }
            let counted = if body.drop_glue { depth / 4 } else { depth };
            past = limit.is_some_and(|limit| counted > limit.value);
        }
        // clone: a reference count, the step's hold of its path
        let step =
            Step { parent: parent.cloned(), body: counted.body.map(|it| it.body), depth, limit };
        (step, past)
    }

    /// The place of a function counted as `counted`, reached from `parent`: `None` past the
    /// limit there, where the worklist does not describe it.
    pub(super) fn place(parent: Option<&Arc<Step>>, counted: &Counted) -> Option<Arc<Step>> {
        let (step, past) = Step::at(parent, counted);
        // alloc: per build, one per function the walk describes: its place on its path
        (!past).then(|| Arc::new(step))
    }

    /// As [`Step::place`], for a function the check sent back: placed though past the limit.
    pub(super) fn place_past(parent: Option<&Arc<Step>>, counted: &Counted) -> Arc<Step> {
        // alloc: per build, one per function the check sent back
        Arc::new(Step::at(parent, counted).0)
    }

    /// The place of a static reached from `parent`: no body, on that path, or heading none.
    pub(super) fn place_static(parent: Option<&Arc<Step>>) -> Arc<Step> {
        // alloc: per build, one per static the walk describes
        Arc::new(Step {
            // clone: a reference count, the step's hold of its path
            parent: parent.cloned(),
            body: None,
            depth: 0,
            limit: parent.and_then(|it| it.limit),
        })
    }
}

impl Drop for Step {
    /// Lets go of the path above one step at a time: a path is as long as the program's
    /// deepest chain of references, and a drop that recursed along it would need that much
    /// stack.
    fn drop(&mut self) {
        let mut above = self.parent.take();
        while let Some(step) = above {
            above = Arc::into_inner(step).and_then(|mut last| last.parent.take());
        }
    }
}

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

/// What [`check`] found.
pub(super) enum Checked<I, S> {
    /// It reached these within the limit, which the worklist had left undescribed past the
    /// limit on its own path: describe them ([`Step::place_past`]) and check again.
    Again(Vec<Arc<Function<I, S>>>),
    /// It reached everything: its refusals, if any, are in the errors.
    Done,
}

/// rustc's walk over `order`, the graph the worklist described so far, once the worklist is
/// empty: from each root in order, depth first, claiming each neighbor of an instance as it
/// lists them and placing it on the path of that instance. Deferred instances it claims within
/// the limit are given back to be described ([`Checked::Again`], each marked
/// [`Function::forced`]); the check runs again once they and what they reach are. Otherwise
/// each instance claimed past the limit is an error. `passes` counts the times it sent
/// instances back, over the walk.
pub(super) fn check<I, S, F>(
    order: &Order<I, S>,
    frontend: &F,
    passes: &mut u32,
    errors: &mut Vec<CodegenError>,
) -> Checked<I, S>
where
    I: Clone + Eq + Hash + Send + Sync,
    S: Clone + Eq + Hash + Send + Sync,
    F: Frontend<Instance = I>,
{
    struct Frame {
        /// Its claimed neighbors are `claimed[start..end]`; `next` is the one to visit.
        start: u32,
        end: u32,
        next: u32,
    }
    let _span = tracing::info_span!(
        target: "blaze",
        "recursion limit check",
        pass = *passes + 1,
        items = order.items.len(),
        roots = order.roots.len(),
    )
    .entered();
    // Places `item` on the check's path of node `parent`: `None` for a function with no count.
    let claim = |paths: &mut Paths, item: u32, parent: u32| -> Option<Result<u32, Past>> {
        match &order.items[item as usize].of {
            Of::Static(_) => Some(Ok(paths.place_static(parent))),
            Of::Function(entry) => {
                entry.counted.get().map(|counted| paths.place(item, counted, parent, false))
            }
        }
    };
    let mut paths = Paths::default();
    // alloc: per check, one flag per item
    let mut visited = vec![false; order.items.len()];
    // alloc: per check, the claimed neighbors of the instances on the path
    let mut claimed: Vec<(u32, u32)> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new(); // alloc: per check, the path
    let mut past: Vec<Past> = Vec::new(); // alloc: per check, one per instance past the limit
    let mut undescribed: Vec<u32> = Vec::new(); // alloc: per check, one per instance to describe
    for &root in &order.roots {
        if std::mem::replace(&mut visited[root as usize], true) {
            continue;
        }
        match claim(&mut paths, root, NONE) {
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
            match &order.items[item as usize].state {
                State::Described(edges) => {
                    let start = claimed.len() as u32;
                    for &edge in edges {
                        if std::mem::replace(&mut visited[edge as usize], true) {
                            continue;
                        }
                        match claim(&mut paths, edge, node) {
                            Some(Ok(node)) => claimed.push((edge, node)),
                            Some(Err(it)) => past.push(it),
                            None => undescribed.push(edge),
                        }
                    }
                    stack.push(Frame { start, end: claimed.len() as u32, next: start });
                }
                State::Failed => {}
                State::Unmade => undescribed.push(item),
            }
        }
    }
    // alloc: per check, the instances it sends back
    let mut again = Vec::new();
    for item in undescribed {
        let entry = &order.items[item as usize];
        // A function with a count and no node is one the worklist left past its limit.
        let (State::Unmade, Of::Function(function)) = (&entry.state, &entry.of) else { continue };
        if function.counted.get().is_some() && !function.forced.swap(true, Ordering::Relaxed) {
            // clone: a reference count, once per instance sent back
            again.push(function.clone());
        }
    }
    if let Some(first) = again.first() {
        *passes += 1;
        if *passes <= PASSES {
            return Checked::Again(again);
        }
        errors.push(CodegenError {
            unit: InstanceKey(ContentId::default()),
            item: ItemKey(ContentId::default()),
            // alloc: the error's message
            what: format!(
                "the recursion limit's check sent instances back to be described {PASSES} \
                 times without reaching them all, the last one `{}`",
                frontend.spell(&first.asked.0)
            ),
            front: None,
        });
        return Checked::Done;
    }
    let instance = |item: u32| order.asked(item).map(|it| &it.0);
    let spelled = |item: u32| match instance(item) {
        Some(instance) => format!("`{}`", frontend.spell(instance)), // alloc: an error's text
        None if item == NONE => "none".to_owned(),                   // alloc: an error's text
        None => "a static".to_owned(),                               // alloc: an error's text
    };
    for it in &past {
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
    let deferred = |it: &super::frontier::Item<I, S>| match (&it.state, &it.of) {
        (State::Unmade, Of::Function(function)) => function.counted.get().is_some(),
        _ => false,
    };
    if past.is_empty()
        && let Some(left) = order.items.iter().position(deferred)
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
    Checked::Done
}

/// rustc's walk ([`check`]) over the graph of a kept walk, every item of which is
/// described: whether it claims an instance past its path's limit, and how many items it
/// claimed. `roots` are the check's, in its order; `edges` gives what an item refers to, in
/// its references' order; `counted` gives a function's count, `None` for a static. The same
/// claims in the same order as the last pass of [`check`] over the same graph, so
/// `false` here is a walk whose check refuses nothing, and a walk with no refusal described
/// everything its roots reach (a deferred instance the check claims within the limit is sent
/// back, and one it claims past it is a refusal).
pub(super) fn claims_past<'e>(
    items: usize,
    roots: &[u32],
    edges: impl Fn(u32) -> &'e [u32],
    counted: impl Fn(u32) -> Option<Counted>,
) -> (bool, usize) {
    struct Frame {
        start: u32,
        end: u32,
        next: u32,
    }
    let mut paths = Paths::default();
    // alloc: per check of a kept walk, one flag per item
    let mut visited = vec![false; items];
    // alloc: per check, the claimed neighbors of the instances on the path
    let mut claimed: Vec<(u32, u32)> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new(); // alloc: per check, the path
    let mut claims = 0;
    let claim = |paths: &mut Paths, item: u32, parent: u32| match counted(item) {
        None => Ok(paths.place_static(parent)),
        Some(counted) => paths.place(item, &counted, parent, false),
    };
    for &root in roots {
        if std::mem::replace(&mut visited[root as usize], true) {
            continue;
        }
        claims += 1;
        match claim(&mut paths, root, NONE) {
            Ok(node) => claimed.push((root, node)),
            Err(_) => return (true, claims),
        }
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
            let start = claimed.len() as u32;
            for &edge in edges(item) {
                if std::mem::replace(&mut visited[edge as usize], true) {
                    continue;
                }
                claims += 1;
                match claim(&mut paths, edge, node) {
                    Ok(node) => claimed.push((edge, node)),
                    Err(_) => return (true, claims),
                }
            }
            stack.push(Frame { start, end: claimed.len() as u32, next: start });
        }
    }
    (false, claims)
}

/// The error of a front end whose [`Frontend::counted`] gave `given` entries for `functions`,
/// which one describe newly reached (or the roots): naming the front end and the functions
/// left without one. Each of `functions` is then left undescribed, with this as why.
pub(super) fn miscounted<I, F: Frontend<Instance = I>>(
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
            "the front end `{}` gave {given} recursion limit entries (`Frontend::counted`) for \
             {} functions, one a function: {which}",
            std::any::type_name::<F>(),
            functions.len(),
        ),
        front: None,
    }
}
