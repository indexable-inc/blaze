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
//!   program. They stay with the whole walk, which a launched build still runs behind the
//!   program (launcher.md, section 4: the eager check); a frontier describe is made against
//!   no closed facts, as a walk's first round is.
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
    Build, FnNode, FrontInstance, FrontStatic, Lookup, Node, Put, Sorted, StaticId, error, kept,
    later, subject,
};
use crate::{
    CodegenError, CompiledFunction, InstanceKey, Linkage, Target,
    backend::{Frontend, FrontRef, Prepare},
    plan::{Mode, Origin},
};

/// A function a frontier form's code refers to: what the next ask for it is made of.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a test's copy, and the caller's list of what to ask next
pub struct Callee<I> {
    /// The instance and whose code asks for it: what [`Build::near`] takes.
    pub asked: (I, Origin),
    pub key: InstanceKey,
    /// What it is across edits of its body ([`subject`]), by the origin of this describe of it:
    /// the whole walk's is by the least origin of every describe of its key.
    pub identity: ContentId,
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

    /// What is at each place of the function asked for.
    pub fn refs(&self) -> &[Target] {
        &self.node().refs
    }
}

impl<B: Prepare> Build<'_, B> {
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
        let nothing = || (InstanceKey(ContentId::default()), blaze_items::ItemKey(ContentId::default()));
        if self.policy.mode != Mode::Launched {
            let (unit, item) = nothing();
            return Err(error(
                unit,
                item,
                // alloc: an error
                format!(
                    "`{}` asked for alone, with no walk of its program, in a build that is not a launched \
                     program's ({:?}): which of its callees are called directly only a whole walk says",
                    self.frontend.spell(&asked.0),
                    self.policy.mode,
                ),
            ));
        }
        let counts = kept::Counts::default();
        let describe = |asked: &(FrontInstance<B>, Origin)| {
            self.described(asked, 0, &counts).and_then(|it| self.node(&asked.0, asked.1, it))
        };
        // Every function described, by who asked: each once.
        // alloc (each below): once an ask, sized by what the function refers to
        let mut described: HashMap<(FrontInstance<B>, Origin), Node<B>> = HashMap::new();
        let mut statics: HashMap<FrontStatic<B>, (StaticId, Option<String>)> = HashMap::new();
        let (mut callees, mut named_statics) = (Vec::new(), Vec::new());
        let root = describe(asked)?;
        let (key, identity) = (root.unit.instance.key(), subject(root.described.identity, root.described.origin));
        // The function and what it builds in: each one's places are resolved.
        let mut whole: blaze_hash::HashMap<InstanceKey, Node<B>, Global> = blaze_hash::HashMap::new_in(Global);
        let mut direct: blaze_hash::HashMap<InstanceKey, Vec<InstanceKey>, Global> =
            blaze_hash::HashMap::new_in(Global);
        let mut queue = vec![root];
        while let Some(mut node) = queue.pop() {
            let own = node.unit.instance.key();
            if whole.contains_key(&own) {
                continue;
            }
            let requester = node.described.origin;
            let mut refs = Vec::with_capacity(node.described.refs.len());
            // The callee at each place, for what the function builds in.
            let mut at_place: Vec<Option<&(FrontInstance<B>, Origin)>> = Vec::new();
            let mut asks = Vec::with_capacity(node.described.refs.len());
            for it in &node.described.refs {
                asks.push(match it {
                    // clone: the ask's handle of a callee; who asks for it is a copy
                    FrontRef::Function(callee) => Some((callee.clone(), requester)),
                    _ => None,
                });
            }
            for (it, ask) in node.described.refs.iter().zip(&asks) {
                at_place.push(ask.as_ref());
                refs.push(match it {
                    FrontRef::Function(_) => {
                        let ask = ask.as_ref().expect("a function reference has its ask");
                        if !described.contains_key(ask) {
                            let made = describe(ask)?;
                            callees.push(Callee {
                                asked: ask.clone(), // clone: the caller's list of what to ask next
                                key: made.unit.instance.key(),
                                identity: subject(made.described.identity, made.described.origin),
                            });
                            described.insert(ask.clone(), made); // clone: the map's key
                        }
                        let callee = &described[ask];
                        match (&callee.described.symbol, callee.described.linkage) {
                            // A weak definition is called by its name, as the whole walk has it.
                            // clone: a weakly defined function's name, a few a program
                            (Some(name), Linkage::Weak) => Target::Extern(name.clone()),
                            _ => Target::Function(callee.unit.instance.key()),
                        }
                    }
                    FrontRef::Static(value) => {
                        if !statics.contains_key(value) {
                            let made = self
                                .frontend
                                .describe_static(value)
                                .and_then(|it| self.static_node(value, it))?;
                            let id = (made.described.item, made.described.instance);
                            let weak = match made.described.linkage {
                                Linkage::Weak => made.described.symbol.clone(), // clone: as a function's
                                Linkage::Strong => None,
                            };
                            named_statics.push((value.clone(), id)); // clone: the caller's list
                            statics.insert(value.clone(), (id, weak)); // clone: the map's key
                        }
                        match &statics[value] {
                            (_, Some(name)) => Target::Extern(name.clone()), // clone: as a function's
                            ((item, instance), None) => Target::Static { item: *item, instance: *instance },
                        }
                    }
                    FrontRef::Extern(name) => Target::Extern(name.clone()), // clone: the place's own target
                    FrontRef::Weak(name) => Target::Weak(name.clone()),     // clone: as above
                });
            }
            // What it builds in directly: each resolved in its turn.
            let mut built_in = Vec::with_capacity(node.described.built_in.len());
            for &at in &node.described.built_in {
                let callee = match (refs.get(at as usize), at_place.get(at as usize).copied().flatten()) {
                    (Some(Target::Function(_)), Some(ask)) => &described[ask],
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
            drop(at_place);
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
            callees,
            statics: named_statics,
            described,
            verified,
        })
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
