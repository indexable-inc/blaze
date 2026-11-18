//! What a reachable static's or constant's initializer NAMES is reachable too.
//!
//! The Reference, "Static items": "Static initializers are constant expressions evaluated at
//! compile time", and a static "represents an allocation in the program"; "Function item
//! types": a fn item named as a value coerces to a fn pointer. So `static TABLE: [fn() -> i32;
//! 2] = [a, b];` holds pointers to `a` and `b`, which the program calls through the table with
//! no body naming them: they are reached by the static, and the image's relocation to each
//! (statics.rs, run.rs `static_image`) needs their instance built. The same holds for a
//! constant (`const F: fn() = a;`) and for a static another initializer names
//! (`static P: &T = &OTHER;`).
//!
//! The rule, one place: every static and constant a reached body names (`Def::Static`,
//! `Def::Const` in its checked `expr_defs`) has its initializer lowered and checked here, once
//! per walk, and read as a reached body's callees are read: its calls queued by
//! [`super::queue_targets`] (a `const fn` the initializer calls, `Atom::new(1)`), its fns
//! named as values and what the builder calls on its own by [`BackNeeds::read`], and each
//! static or constant it names in turn (a work list, no recursion). An initializer's body is
//! never a fn of the census (it is not built as code: the evaluator makes its bytes), except
//! as its closures' owner:
//!
//! THE BODY-OWNER RULE (one rule for both programs): a closure is built at its OWNER's row,
//! and its owner is the item whose body it is written in, any item with a body: a fn, a
//! `const` or `static` initializer, an associated constant (an anonymous `const { .. }` block
//! is part of its enclosing body and owned by that body's item). So an initializer that
//! writes a closure (`const C: fn(u8) -> u8 = |x| x + 1;`, std's `thread_local!` `Key { get:
//! || .. }`) is handed back to the walk with its checked body ([`Initializers::round`]'s
//! `owners`), and the program tables it as a row like a fn's (program.rs `make_from`, tabled.rs
//! `table_one`: a const's head is a fn head with no inputs, by_origin.rs `Want::Fn`): run.rs
//! `closure_owner` then finds it by `fn_index`. Its own instance is never made (nothing calls
//! a constant: a use reads its value); only its closures' `Fn*` instances are. The lazy program
//! tables the same owner on demand (lazy/fill.rs `target_of`, reached through fn_shim.rs
//! `untabled_owner`).
//!
//! An initializer that does not lower or check is not followed here: the evaluator checks the
//! same body when the static's value is made (consts.rs `Consts::value`, statics.rs
//! `static_values`) and refuses it there, naming the static: it is never silently dropped.
//!
//! Edges ([`super::Census::edges`]): (the body that named the static, the static) and (the
//! static, each fn its initializer queued); an entry's own body names its statics from
//! [`super::AMBIENT`] (the entries' bodies are not tabled by key here: a superset, named).

use std::alloc::Global;
use std::vec::Vec;

use blaze_lower::scoped::Prepares;
use blaze_typeck as tc;
use blaze_typeck::{Checked, ItemKey};

use super::{AMBIENT, Reached, note_edges, queue_targets};
use crate::front2::back_needs::BackNeeds;
use crate::front2::body::{Body, lower_fn, lower_member};
use crate::front2::check::{Records, check_body};
use crate::front2::spell::Spelling;
use crate::front2::statics::MOST_STATICS;

/// The statics and constants whose initializers one walk has read.
pub(super) struct Initializers {
    /// Every static and constant noted, sorted: each initializer read once (rule 77).
    seen: Vec<ItemKey, Global>,
    /// How many of the census's bodies were read for names already.
    bodies_read: usize,
    firsts_read: bool,
    /// The unsizing coercions to a `dyn` the initializers read make (reach/instances.rs).
    pub(super) coerced: super::instances::Coerced,
}

impl Initializers {
    pub(super) fn new() -> Initializers {
        Initializers { seen: Vec::new_in(Global), bodies_read: 0, firsts_read: false, coerced: super::instances::Coerced::default() }
    }

    /// Notes each static and constant `c` names that is not noted yet, with the body that
    /// named it (`from`).
    fn note(&mut self, from: ItemKey, c: &Checked<'_>, todo: &mut Vec<(ItemKey, ItemKey), Global>) {
        for d in c.expr_defs.column::<1>().iter() {
            let k = match *d {
                tc::Def::Static(k, _) | tc::Def::Const(k) => k,
                _ => continue,
            };
            if let Err(at) = self.seen.binary_search(&k) {
                self.seen.insert(at, k);
                todo.push((from, k));
            }
        }
    }

    /// Reads the initializers the bodies checked since the last round (and the entries' first
    /// bodies, once) name; queues what they reach. Whether it queued anything: the walk then
    /// drains its list again before it asks the instances' round. Each initializer that
    /// writes a closure is added to `bodies` (the census's) with its checked body: its
    /// closures' owner row (the module's body-owner rule).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn round<'n, B>(
        &mut self,
        r: &mut Records<'_, 'n>,
        prepared: &Prepares<'n>,
        firsts: &[&Checked<'_>],
        bodies: &mut B,
        needs: &mut BackNeeds,
        queue: &mut Vec<(ItemKey, Option<Reached>), Global>,
        fns: &mut Vec<(ItemKey, Reached), Global>,
        edges: &mut Vec<(ItemKey, ItemKey), Global>,
    ) -> bool
    where
        B: core::ops::Deref<Target = [(Body<'n>, Checked<'static>)]> + Extend<(Body<'n>, Checked<'static>)>,
    {
        let mut todo: Vec<(ItemKey, ItemKey), Global> = Vec::new_in(Global);
        if !self.firsts_read {
            self.firsts_read = true;
            for c in firsts {
                self.note(AMBIENT, c, &mut todo);
            }
        }
        for (body, c) in bodies.get(self.bodies_read..).unwrap_or(&[]) {
            self.note(body.owner, c, &mut todo);
        }
        self.bodies_read = bodies.len();
        let before = queue.len();
        // Ascending key order for each round's work: the walk's outcome is a function of the
        // program, never of the order names were met in (rule 25).
        todo.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        while let Some((from, k)) = todo.pop() {
            edges.push((from, k));
            // Bounded as the statics' work list is (rule 18): `seen` holds each key once.
            if self.seen.len() > MOST_STATICS {
                break;
            }
            let lowered = match r.keys.library_in(r.names, k) {
                Some(lib) => lower_fn(r.names, prepared, r.keys, r.store, lib),
                None => lower_member(r.names, prepared, r.keys, r.store, k),
            };
            let Ok(body) = lowered else { continue };
            // The view, spelling and scope borrow `body`: they end with this block, so the
            // body can be handed on below.
            let checked = {
                let view = body.lowered.view();
                let Ok(spell) = Spelling::of(&view) else { continue };
                let scope = body.scope(&spell);
                let Ok(mut c) = check_body(r, &body, &scope) else { continue };
                let (q, f) = (queue.len(), fns.len());
                queue_targets(r, &scope, body.owner, &mut c, queue);
                needs.read(r, body.owner, Some(&body), &c, queue, fns);
                note_edges(edges, k, &queue[q..], &fns[f..]);
                c
            };
            let c = checked;
            self.coerced.read(&c);
            let mut more: Vec<(ItemKey, ItemKey), Global> = Vec::new_in(Global);
            self.note(k, &c, &mut more);
            more.sort_unstable_by(|a, b| b.1.cmp(&a.1));
            todo.extend(more);
            // The body-owner rule (module docs): an initializer that writes a closure is its
            // closures' owner row.
            if !c.closures.is_empty() {
                bodies.extend(core::iter::once((body, c)));
            }
        }
        // An owner added here was noted above already (rule 77).
        self.bodies_read = bodies.len();
        queue.len() > before
    }
}
