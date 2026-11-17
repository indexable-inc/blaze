//! The slots of the vtables the walk's coercions make: every one is a reached instance at the
//! coerced type, whether or not any call names it (rung x0-dyn-03-uncalled-method).
//!
//! THE RULE. The Reference, "Trait objects": a pointer to `dyn Trait` is "a pointer to an
//! instance of a type T implementing SomeTrait" plus "a virtual method table, often just called
//! a vtable, which contains, for each method of SomeTrait and its supertraits that T
//! implements, a pointer to T's implementation (i.e. a function pointer)". The table is made
//! where the pointer is unsized, from the concrete type; what is later called through it is
//! not known there. So every slot of `(T, Principal)`'s table is a fn the program holds the
//! address of, hence a body the walk must reach: the impl's fn of that name for `T`, or the
//! trait's provided body at `T` where the impl leaves the fn out (the Reference, "Traits",
//! "Associated functions").
//!
//! WHICH METHODS HAVE A SLOT. The Reference, "Dyn compatibility": "Dispatchable functions must:
//! Not have any type parameters (although lifetime parameters are allowed). Be a method that
//! does not use `Self` except in the type of the receiver. Have a receiver with one of the
//! following types: `&Self` (i.e. `&self`), `&mut Self` (i.e `&mut self`), `Box<Self>`,
//! `Rc<Self>`, `Arc<Self>`, `Pin<P>` where `P` is one of the types above. Not have an opaque
//! return type ... Not have a `where Self: Sized` bound." and "Explicitly non-dispatchable
//! functions require: Have a `where Self: Sized` bound (receiver type of `Self` (i.e. `self`)
//! implies this)." A non-dispatchable method has no slot and is not reached by the table. The
//! predicate is the one the program's tables are made by (program.rs `dispatchable`, the
//! `ObjectMethod::dispatch` the builder lays slots out by): the reached set and the slots are
//! one rule, never two.
//!
//! WHICH SHAPES. A slot fn of principal `P` is asked at each shape coerced to a `dyn` whose
//! principal is `P` (the pairs [`Coerced`] reads), not at every coerced shape: a type coerced
//! to `dyn A` alone reaches none of `B`'s impl fns. A coercion from a type the site does not
//! know (`Coerced::open`) makes `P`'s slot fns asked as at an instance (the round's `open`); a
//! coercion that could not be read (`Coerced::open_all`) does so for every principal the read
//! bodies' types name (`Coerced::named`: the set the program makes tables for).
//!
//! WHAT IS NOT DONE HERE. A trait whose head or members cannot be made, or a member the front
//! end could not read, gives no slot here: the program's table of that trait is refused by
//! name where it is made (program.rs `object_tables`, `Front2Refusal::ObjectTrait`), so the
//! program never holds a table this walk did not fill.

use super::{Box, Coerced, Global, HashMap, ItemKey, Met, PNode, Records, Shape, Vec, supertrait_closure, tc};
use crate::front2::heads::Want;
use crate::front2::keys::Origin;
use crate::front2::program::dispatchable;

/// How a trait fn was met (a bit of `Met::how`, beside `AT_INSTANCE`, `AT_OBJECT` and
/// `BY_BUILDER`): a slot of a table some coercion makes. Asked at the shapes coerced to a
/// principal whose table holds it; its edges come from `AMBIENT` (the table is the builder's,
/// made where the coercion is, and a test's reach must hold it whichever body coerces).
pub(super) const IN_TABLE: u8 = 8;

/// One slot of a principal's table: the trait declaring the method, the trait fn's key, and its
/// name (to find each answering impl's fn of that name).
struct Slot {
    trait_: ItemKey,
    f: ItemKey,
    name: Box<str>,
}

/// The slots of each principal met, read once, and each slot fn's shapes for the round.
#[derive(Default)]
pub(super) struct Tables {
    /// Each principal's slot fns, its supertraits' included (in the closure's order; the round
    /// sorts what it asks).
    slots: HashMap<ItemKey, Vec<Slot, Global>, Global>,
    /// Each slot fn's shapes this round: the shapes coerced to a principal whose table holds
    /// it, sorted, each once.
    shapes: HashMap<ItemKey, Vec<Shape, Global>, Global>,
}

impl Tables {
    /// Pushes onto `fns` a [`Met`] with [`IN_TABLE`] for every slot of every table the walk's
    /// coercions (`coerced`: the bodies' and the initializers') make, and files each slot fn's
    /// coerced shapes for [`Tables::shapes_of`]. The round merges the pushed `Met`s by key with
    /// the fns met at calls.
    pub(super) fn add(&mut self, r: &mut Records<'_, '_>, coerced: [&Coerced; 2], fns: &mut Vec<Met, Global>) {
        for v in self.shapes.values_mut() {
            v.clear();
        }
        let open_all = coerced.iter().any(|c| c.open_all);
        // (principal, the shape coerced to it, or `None` for a coercion from a type the site
        // does not know), sorted so each principal's entries are together.
        let mut made: Vec<(ItemKey, Option<Shape>), Global> = Vec::new_in(Global);
        for c in coerced {
            made.extend(c.tables.keys().map(|&(p, s)| (p, Some(s))));
            made.extend(c.open.iter().map(|&p| (p, None)));
            if open_all {
                made.extend(c.named.keys().map(|&p| (p, None)));
            }
        }
        made.sort_unstable();
        made.dedup();
        let mut last: Option<ItemKey> = None;
        for (p, at) in made {
            if !self.slots.contains_key(&p) {
                let list = slots_of(r, p);
                self.slots.insert(p, list);
            }
            let Some(list) = self.slots.get(&p) else { continue };
            let first_of_p = last != Some(p);
            last = Some(p);
            for slot in list.iter() {
                if first_of_p {
                    fns.push(Met { trait_: slot.trait_, f: slot.f, name: slot.name.clone(), how: IN_TABLE });
                }
                if let Some(s) = at {
                    match self.shapes.get_mut(&slot.f) {
                        Some(v) => v.push(s),
                        None => {
                            let mut v = Vec::new_in(Global);
                            v.push(s);
                            self.shapes.insert(slot.f, v);
                        }
                    }
                }
            }
        }
        for v in self.shapes.values_mut() {
            v.sort_unstable();
            v.dedup();
        }
    }

    /// The shapes slot fn `f` is asked at this round (empty for one met only through a
    /// coercion from a type the site does not know: asked as at an instance instead).
    pub(super) fn shapes_of(&self, f: ItemKey) -> &[Shape] {
        self.shapes.get(&f).map_or(&[], |v| v.as_slice())
    }
}

/// Files in `named` the principal of every `dyn` a body's types `t` hold: the tables the
/// program can make from that body ([`Tables::add`] reads them when a coercion could not be
/// read).
pub(super) fn name_principals(t: tc::PTys<'_>, named: &mut HashMap<ItemKey, (), Global>) {
    for n in t.nodes() {
        if let PNode::Dynamic(l) = *n
            && let Some(p) = tc::object_principal(t, l)
        {
            named.insert(p, ());
        }
    }
}

/// The slots of `principal`'s tables: each dispatchable method of the principal and of every
/// trait its supertrait clauses name, transitively. Order is the closure's; it is a set to its
/// readers.
fn slots_of(r: &mut Records<'_, '_>, principal: ItemKey) -> Vec<Slot, Global> {
    let mut out: Vec<Slot, Global> = Vec::new_in(Global);
    let mut traits: Vec<ItemKey, Global> = Vec::new_in(Global);
    // A closure that is not whole stopped at a trait whose head was not made (or past the
    // bound): that trait's table is refused by name where it is made (the module's doc); the
    // traits read before it still give their slots.
    let _whole = supertrait_closure(r, &[principal], &mut traits);
    for t in traits {
        // Each `continue` below is a trait or member the table's maker refuses by name
        // (program.rs `object_tables`): "no library key", "its head", "its members", "made, but
        // no head filed", "a member the front end could not read", "a method with no fn filed
        // under its name".
        let Some(lib) = r.keys.library_in(r.names, t) else { continue };
        if r.store.ensure(r.names, r.keys, Want::Head(lib)).is_err() || r.store.ensure(r.names, r.keys, Want::Members(lib)).is_err() {
            continue;
        }
        let Some(head) = r.store.maps.traits.get(&lib) else { continue };
        for m in head.members.iter() {
            let tc::dyn_compat::MemberShape::Fn(facts) = &m.shape else { continue };
            if !dispatchable(m, facts) {
                continue;
            }
            let Some(&(_, f)) = head.fns.iter().find(|(n, _)| *n == m.name) else { continue };
            // The fn's key was filed as the trait's member when its head was made (keys.rs
            // `member`): its origin holds the name each impl's fn is filed under.
            let Some(Origin::Member { name, .. }) = r.keys.origin(f) else { continue };
            out.push(Slot { trait_: t, f, name: name.clone() });
        }
    }
    out
}
