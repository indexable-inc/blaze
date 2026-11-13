//! The walk's frontier form (docs/v2/plan/design/launcher.md, "The lazy path"): one function
//! made ready to compile from its own describe and those of what it refers to directly, with
//! no walk of the program. A launched program asks for a function when it first calls it
//! ([`crate::plan::Mode::Launched`]); what that takes is the function's places, which name
//! each callee by its key, and a key is a callee's code and arguments, so each callee is
//! described once and nothing it refers to is. A function it builds in is part of its code:
//! that one's places are resolved too, at any depth.
//!
//! What a whole walk ([`Build::walk`]) does that this does not, and why a launched program's
//! first call can go without each:
//!
//! - **Which references go through a slot** (`rebind`: a fixed point over the program).
//!   Launched code calls every function through its slot word (`Spec::slots`), so every
//!   function reference is [`Target::Function`]; the whole walk's [`Target::Direct`] names
//!   the same key.
//! - **The recursion limit's check, a symbol defined twice, the front end's closed facts**
//!   ([`crate::backend::Frontend::closed`]): each is a refusal or a fact of the whole
//!   program. A launched run checks a symbol defined twice over its roots ([`Build::start`]),
//!   and does not check the other two (the start's module doc lists what it defers); a
//!   frontier describe is made against no closed facts, as a walk's first round is.
//! - **Which describe a key keeps** when two instances share it, and a function's least
//!   origin: each is a choice over every describe of the key. One ask sees one describe, so a
//!   callee's identity here is its own origin's ([`Callee::identity`]); a key asked for by
//!   two origins is then two slots of one code.
//! - **Merging equal code** (dedupe): a launched function has a slot of its own.
//!
//! The refinement, tested (`tests::the_frontier_form_of_a_function_is_the_whole_walks`):
//! for every function a whole walk reaches, the frontier form gives the same unit, the same
//! key, the same thing at each place (a function by its key, through a slot or not), the same
//! built-in functions and the same code, having described only the function, what it builds
//! in, and what those refer to.

use std::{alloc::Global, collections::HashMap};

use blaze_ids::ContentId;

use super::{
    Build, FnNode, FrontInstance, FrontStatic, Lookup, Node, Planted, Put, Sorted, StaticId, error,
    kept, later, subject,
};
use crate::{
    CodegenError, CompiledFunction, InstanceKey, Linkage, Target,
    backend::{Frontend, FrontRef, Prepare},
    plan::{Mode, Origin},
};

/// A function a frontier form's code refers to: what the next ask for it is made of, and what
/// a launcher's slot of it is keyed and bound by.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a test's copy, and the caller's list of what to ask next
pub struct Callee<I> {
    /// The instance and whose code asks for it: what [`Build::near`] takes.
    pub asked: (I, Origin),
    pub key: InstanceKey,
    /// What it is across edits of its body ([`subject`]), by the origin of this describe of it:
    /// the whole walk's is by the least origin of every describe of its key.
    pub identity: ContentId,
    /// What a caller's code assumes of it beyond its address ([`crate::backend::Described::abi`]):
    /// with `identity`, what a running program's slot of it is.
    pub abi: ContentId,
    /// The symbol it defines and how, where it defines one.
    pub symbol: Option<(String, Linkage)>,
    /// The content key of its describe ([`Frontend::record_key`]), where the front end has one:
    /// what a launcher's bind key of it is made of.
    pub ask: Option<blaze_cas::Key>,
}

/// One function ready to compile, with no walk of its program ([`Build::near`]).
pub struct Near<I, S> {
    /// The function asked for.
    pub key: InstanceKey,
    /// What it is across edits of its body, as [`Callee::identity`].
    pub identity: ContentId,
    /// It and every function it builds in, at any depth, each with its places resolved and
    /// what it builds in: all a compile of it reads.
    nodes: Sorted<InstanceKey, FnNode<I, S>>,
    /// Every function those refer to, each once, in the order they refer to them: none of
    /// them described past its own describe.
    pub callees: Vec<Callee<I>>,
    /// Every static those refer to, each once, in that order.
    pub statics: Vec<(S, StaticId)>,
    /// Describes made, and kept describes found to hold without one.
    pub described: usize,
    pub verified: usize,
}

impl<I, S> Near<I, S> {
    /// The function asked for: its unit, its places and what it builds in.
    pub(super) fn node(&self) -> &FnNode<I, S> {
        self.nodes.get(&self.key).expect("a frontier form holds the function it is of")
    }

    /// The items of the function asked for and of every body it builds in, at any depth,
    /// sorted, each once: whose span tables its code's positions name.
    pub(super) fn items(&self) -> Vec<blaze_items::ItemKey> {
        // alloc: once a function made, its items and those it builds in
        let mut items: Vec<blaze_items::ItemKey> = self.nodes.values().map(|it| it.unit.item).collect();
        items.sort_unstable();
        items.dedup();
        items
    }

    /// What is at each place of the function asked for.
    pub fn refs(&self) -> &[Target] {
        &self.node().refs
    }
}

/// The target at each place of a function or static, and the ask at each place that is a
/// function's ([`Build::resolve`]).
pub(super) type Places<B> = (Vec<Target>, Vec<Option<(FrontInstance<B>, Origin)>>);

/// What a frontier form has resolved so far: every function described, by who asked, and every
/// static, each once; and the two lists of them in the order they were first referred to.
pub(super) struct Resolved<B: Prepare> {
    pub(super) described: HashMap<(FrontInstance<B>, Origin), Node<B>>,
    /// Each static's id and, where it is defined weakly, its name.
    pub(super) statics: HashMap<FrontStatic<B>, (StaticId, Option<String>)>,
    pub(super) callees: Vec<Callee<FrontInstance<B>>>,
    pub(super) named_statics: Vec<(FrontStatic<B>, StaticId)>,
}

impl<B: Prepare> Resolved<B> {
    pub(super) fn new() -> Self {
        Resolved {
            // alloc (each): once an ask, sized by what the function refers to
            described: HashMap::new(),
            statics: HashMap::new(),
            callees: Vec::new(),
            named_statics: Vec::new(),
        }
    }
}

impl<B: Prepare> Build<'_, B> {
    /// The refusal of `what` asked for alone in a build that is not a launched program's: any
    /// other build's places say which callees are direct, which only a whole walk knows.
    pub(super) fn launched_only(&self, what: &str) -> Result<(), CodegenError> {
        if self.policy.mode == Mode::Launched {
            return Ok(());
        }
        let nothing = (InstanceKey(ContentId::default()), blaze_items::ItemKey(ContentId::default()));
        Err(error(
            nothing.0,
            nothing.1,
            // alloc: an error
            format!(
                "{what} asked for alone, with no walk of its program, in a build that is not a launched \
                 program's ({:?}): which of its callees are called directly only a whole walk says",
                self.policy.mode,
            ),
        ))
    }

    /// `asked` (a function and whose code asks for it) ready to compile: described, each
    /// function and static it refers to described and no further, and each function it builds
    /// in resolved as it is. Describes go through the walk kept of the program where the build
    /// has one, so a describe that still holds is not made again.
    ///
    /// Only for a launched build ([`Mode::Launched`]), where every call is through a slot: any
    /// other build's places say which callees are direct, which only a whole walk knows.
    pub fn near(
        &self,
        asked: &(FrontInstance<B>, Origin),
    ) -> Result<Near<FrontInstance<B>, FrontStatic<B>>, CodegenError> {
        // alloc: an error's name of the function
        let what = blaze_report::say!(blaze_report::say::Code(&self.frontend.spell(&asked.0)));
        self.launched_only(&what.flat())?;
        let counts = kept::Counts::default();
        let root = self.described(asked, 0, &counts).and_then(|it| self.node(&asked.0, asked.1, it))?;
        self.near_from(root, &counts)
    }

    /// [`Build::near`] of the function `root` is the node of, described already: by
    /// [`Build::start`], which described it as a root.
    pub(super) fn near_from(
        &self,
        root: Node<B>,
        counts: &kept::Counts,
    ) -> Result<Near<FrontInstance<B>, FrontStatic<B>>, CodegenError> {
        let mut resolved = Resolved::<B>::new();
        let (key, identity) = (root.unit.instance.key(), subject(root.described.identity, root.described.origin));
        // The function and what it builds in: each one's places are resolved.
        let mut whole: blaze_hash::HashMap<InstanceKey, Node<B>, Global> = blaze_hash::HashMap::new_in(Global);
        let mut direct: blaze_hash::HashMap<InstanceKey, Vec<InstanceKey>, Global> =
            blaze_hash::HashMap::new_in(Global);
        // alloc: once an ask, the functions it builds in
        let mut queue = vec![root];
        while let Some(mut node) = queue.pop() {
            let own = node.unit.instance.key();
            if whole.contains_key(&own) {
                continue;
            }
            let (refs, asks) =
                self.resolve(&node.described.refs, node.described.origin, counts, &mut resolved)?;
            // What it builds in directly: each resolved in its turn.
            let mut built_in = Vec::with_capacity(node.described.built_in.len());
            for &at in &node.described.built_in {
                let callee = match (refs.get(at as usize), asks.get(at as usize).and_then(Option::as_ref)) {
                    (Some(Target::Function(_)), Some(ask)) => &resolved.described[ask],
                    _ => {
                        return Err(error(
                            own,
                            node.unit.item,
                            // alloc: an error
                            format!(
                                "builds in place {at}, which is not a function among its {} references",
                                refs.len()
                            ),
                        ));
                    }
                };
                let callee_key = callee.unit.instance.key();
                if callee.patchable {
                    return Err(error(
                        own,
                        node.unit.item,
                        // alloc: an error
                        format!("builds in {callee_key:?}, which is patchable: an edit patches it alone"),
                    ));
                }
                built_in.push(callee_key);
                queue.push(callee.clone()); // clone: a built-in function's node, resolved in its turn
            }
            direct.insert(own, built_in);
            node.refs = refs;
            whole.insert(own, node);
        }
        // What each builds in at any depth, as the whole walk closes it.
        let (mut seen, mut queue) = (blaze_hash::HashSet::new_in(Global), Vec::new());
        for (own, node) in whole.iter_mut() {
            seen.clear();
            queue.extend(direct.get(own).into_iter().flatten().copied());
            while let Some(it) = queue.pop() {
                if it != *own && seen.insert(it) {
                    queue.extend(direct.get(&it).into_iter().flatten().copied());
                }
            }
            node.built_in = seen.iter().copied().collect();
            node.built_in.sort_unstable();
        }
        let (described, verified) = counts.get();
        Ok(Near {
            key,
            identity,
            nodes: Sorted::of(whole),
            callees: resolved.callees,
            statics: resolved.named_statics,
            described,
            verified,
        })
    }

    /// The target at each of `refs`, the references of a function or static whose code is
    /// `requester`'s, as a launched program has them: a function through its slot by its key
    /// (by its name where it is defined weakly), a static by its id (by its name likewise).
    /// Each function and static is described once into `resolved`, the first reference to one
    /// adding it to its list, and nothing it refers to is described. With the ask at each
    /// place that is a function's, for what the code builds in.
    pub(super) fn resolve(
        &self,
        refs: &[FrontRef<FrontInstance<B>, FrontStatic<B>>],
        requester: Origin,
        counts: &kept::Counts,
        resolved: &mut Resolved<B>,
    ) -> Result<Places<B>, CodegenError> {
        let describe = |asked: &(FrontInstance<B>, Origin)| {
            self.described(asked, 0, counts).and_then(|it| self.node(&asked.0, asked.1, it))
        };
        // alloc (each): once a function or static resolved, sized by its references
        let mut targets = Vec::with_capacity(refs.len());
        let mut asks = Vec::with_capacity(refs.len());
        for it in refs {
            let (target, ask) = match it {
                FrontRef::Function(callee) => {
                    // clone: the ask's handle of a callee; who asks for it is a copy
                    let ask = (callee.clone(), requester);
                    if !resolved.described.contains_key(&ask) {
                        let made = describe(&ask)?;
                        let described = &made.described;
                        resolved.callees.push(Callee {
                            asked: ask.clone(), // clone: the caller's list of what to ask next
                            key: made.unit.instance.key(),
                            identity: subject(described.identity, described.origin),
                            abi: described.abi,
                            // clone: a defined symbol's name, a few a program
                            symbol: described.symbol.clone().map(|name| (name, described.linkage)),
                            ask: self.frontend.record_key(&ask.0, &ask.1),
                        });
                        resolved.described.insert(ask.clone(), made); // clone: the map's key
                    }
                    let callee = &resolved.described[&ask];
                    let weak_by_name = self.planted != Some(Planted::WeakByKey);
                    let target = match (&callee.described.symbol, callee.described.linkage) {
                        // A weak definition is called by its name, as the whole walk has it.
                        // clone: a weakly defined function's name, a few a program
                        (Some(name), Linkage::Weak) if weak_by_name => Target::Extern(name.clone()),
                        _ => Target::Function(callee.unit.instance.key()),
                    };
                    (target, Some(ask))
                }
                FrontRef::Static(value) => {
                    if !resolved.statics.contains_key(value) {
                        let made = self
                            .frontend
                            .describe_static(value)
                            .and_then(|it| self.static_node(value, it))?;
                        let id = (made.described.item, made.described.instance);
                        let weak = match made.described.linkage {
                            Linkage::Weak => made.described.symbol.clone(), // clone: as a function's
                            Linkage::Strong => None,
                        };
                        resolved.named_statics.push((value.clone(), id)); // clone: the caller's list
                        resolved.statics.insert(value.clone(), (id, weak)); // clone: the map's key
                    }
                    let target = match &resolved.statics[value] {
                        (_, Some(name)) => Target::Extern(name.clone()), // clone: as a function's
                        ((item, instance), None) => Target::Static { item: *item, instance: *instance },
                    };
                    (target, None)
                }
                FrontRef::Extern(name) => (Target::Extern(name.clone()), None), // clone: the place's own target
                FrontRef::Weak(name) => (Target::Weak(name.clone()), None),     // clone: as above
            };
            targets.push(target);
            asks.push(ask);
        }
        Ok((targets, asks))
    }

    /// `near`'s function compiled alone, as a lazy build's first call compiles one
    /// ([`later::Later::compile_kept`]): the store's template, else one build's of the memo.
    /// With what the caller puts in the store.
    pub fn compile_near<L: Lookup + ?Sized>(
        &self,
        near: &Near<FrontInstance<B>, FrontStatic<B>>,
        store: &L,
    ) -> Result<(CompiledFunction, Vec<Put>), CodegenError> {
        later::compile_alone(self.backend, self.frontend, self.memo, &near.nodes, near.key, store)
    }
}
