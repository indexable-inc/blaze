//! The impl fns the reach walk's trait fns resolve to where no body's call names one (reach.rs's
//! rounds), asked at the self types the walk meets: never every trait fn times every shape.
//!
//! THE QUESTION. A call of a trait's fn on a type parameter, on a projection, or at arguments
//! holding one below the top (`Target::AtInstance`), or on `dyn Trait` (`Target::AtObject`),
//! names no impl at its site; nor do the calls the builder makes on its own (drop glue's
//! `Drop::drop`, an overloaded index or dereference, `.await`'s `Future::poll`). The question
//! the walk answers for each is: for this trait fn and a self type the walk met, which impl's fn
//! is the callee. The walk is at fn grain: a generic body is checked once, in its own env, with
//! no instance's arguments, and every instance's self type is built from types the reached
//! bodies write; so a self type "met" by a trait fn is a shape the reached bodies' types hold AT
//! WHICH THE TRAIT HAS AN IMPL (the question at any other shape answers nothing an instance can
//! call but the blanket impls, and those are the answer at `Shape::Param`). A trait fn met only
//! on `dyn Trait` meets the types coerced to a `dyn` (an `Unsize` step of a reached body); its
//! vtable is made where the object is, from the coerced type.
//!
//! WHAT WAS ASKED BEFORE. The walk asked `ImplsOf(trait, shape)` for every trait fn reached at
//! an instance times every shape any checked body's types hold, keyed by the trait, the fn's
//! NAME as a string and the shape, kept to the walk's end: 1.8 million pairs, 457 MB, on a hello
//! world (port27/hello-1116/TABLES-NOTES.md, "Peak attribution", holder 1), and the answers
//! filled most of the buckets' answer cache (holder 2).
//!
//! NOW. A pair is `(the trait fn's own key, a shape)`: no string. For a trait fn met at an
//! instance or by the builder the shapes asked are `Param` (the blanket impls) and each shape a
//! reached body holds at which the impl index files an impl of the trait, or an impl whose trait
//! path was not read, or (read once the `Param` question lowered them) an impl filed under no
//! shape whose lowered header is an impl of the trait. Every other shape's answer is a subset of
//! the `Param` answer's (`Buckets::answer`: a blanket fits every shape; a concrete self type
//! fits its own shape alone), so the reached set is the one the product reached. Two classes
//! ask at every met shape, named: a trait whose `Param` question is a gap (a failed candidate
//! is admitted at `Param` and may be left out by a narrower shape's home, whose answer then
//! holds blankets the gap hid), and an impl index filed lazily (`Buckets::lazy`: its buckets are
//! not the whole library). A trait fn met only on `dyn` asks the shapes coerced to a `dyn`,
//! unless a coercion to a `dyn` whose principal is the trait or names it among its supertraits
//! came from a type the site does not know (a type parameter: `impl<E: Error> From<E> for
//! Box<dyn Error>`), which makes the trait's fn asked as at an instance.
//!
//! The set of pairs asked is the set met; each answer is the buckets' own (asked again, it is a
//! lookup), and nothing else is kept.

use std::alloc::Global;
use std::boxed::Box;
use std::format;
use std::vec::Vec;

use blaze_hash::HashMap;
use blaze_id::ItemKey as LibKey;
use blaze_typeck::{self as tc, Checked, ItemKey, PNode, PTy, Shape};

mod slots;

use self::slots::{IN_TABLE, Tables};
use super::{AMBIENT, Census, Reached, note_edges};
use crate::front2::back_needs::BackNeeds;
use crate::front2::buckets::shape_of;
use crate::front2::check::{Records, answer_impls};
use crate::front2::fill::want_of_key;
use crate::front2::keys::{MemberKind, Origin, checker_of};

/// The most type nodes one coercion's source and target are walked through together (rule 18):
/// a type the checker wrote is far smaller; past it the coercion is read as from a type the
/// site does not know (every `dyn` trait's fn then asked as at an instance), never dropped.
const MOST_COERCE_STEPS: usize = 4096;

/// The most traits an open principal's supertrait closure holds (rule 18); past it every trait
/// fn met on `dyn` is asked as at an instance.
const MOST_SUPERTRAITS: usize = 1024;

/// How a trait fn was met: bits of [`Met::how`].
const AT_INSTANCE: u8 = 1;
const AT_OBJECT: u8 = 2;
const BY_BUILDER: u8 = 4;

/// One trait fn the round asks about.
struct Met {
    trait_: ItemKey,
    /// The trait fn's own key: the pair's key, and the source of each edge it queues (but a
    /// builder's question, queued from [`AMBIENT`]).
    f: ItemKey,
    /// Its name, to find each answering impl's fn of that name (read for this round only).
    name: Box<str>,
    how: u8,
}

/// What the `Param` question of a trait left: a gap, or the shapes of the impls filed under no
/// shape whose lowered header is an impl of the trait.
enum AtParam {
    Gap,
    Lowered(Vec<Shape, Global>),
}

/// The unsizing coercions to a `dyn` the walk met: the shapes coerced, and the principals of the
/// coercions whose source type the site does not know.
#[derive(Default)]
pub(super) struct Coerced {
    shapes: HashMap<Shape, (), Global>,
    /// Each `(principal, shape)` coerced: a table the program makes, whose every slot is reached
    /// at the shape (slots.rs).
    tables: HashMap<(ItemKey, Shape), (), Global>,
    /// The principal of every `dyn` the read bodies' types hold: the tables whose slots are
    /// asked as at an instance when a coercion could not be read (`open_all`).
    named: HashMap<ItemKey, (), Global>,
    open: Vec<ItemKey, Global>,
    /// A coercion that could not be read (a type the record does not hold, the step bound):
    /// every trait fn met on `dyn` is asked as at an instance.
    open_all: bool,
}

impl Coerced {
    /// Reads every `Unsize` step of `c`: its source is the step before it on the same
    /// expression, else the expression's own type (columns.rs `Adjusts`: each expression's
    /// steps are consecutive and in order). The source and target are walked together through
    /// references, raw pointers and one ADT's arguments (`Box<T>` to `Box<dyn Tr>`, nested:
    /// rules/coerce_nested.rs) to the `dyn` the target holds.
    pub(super) fn read(&mut self, c: &Checked<'_>) {
        let t = c.types.view();
        slots::name_principals(t, &mut self.named);
        let exprs = c.adjusts.column::<0>();
        let kinds = c.adjusts.column::<1>();
        let tys = c.adjusts.column::<2>();
        let mut pairs: Vec<(PTy, PTy), Global> = Vec::new_in(Global);
        for i in 0..kinds.len() {
            if !matches!(kinds[i], tc::AdjustKind::Unsize) {
                continue;
            }
            let source = match i > 0 && exprs[i - 1] == exprs[i] {
                true => Some(tys[i - 1]),
                false => c.expr_ty.get(exprs[i].0 as usize).copied(),
            };
            let Some(source) = source else {
                self.open_all = true;
                continue;
            };
            pairs.clear();
            pairs.push((source, tys[i]));
            let mut steps = 0;
            while let Some((from, to)) = pairs.pop() {
                steps += 1;
                let (Some(a), Some(b)) = (t.get(from), t.get(to)) else {
                    self.open_all = true;
                    break;
                };
                if steps > MOST_COERCE_STEPS {
                    self.open_all = true;
                    break;
                }
                match (a, b) {
                    // An upcast: the vtable is made from the one the object holds, whose types
                    // were read where that object was made.
                    (PNode::Dynamic(_), PNode::Dynamic(_)) => {}
                    (_, PNode::Dynamic(l)) => match (shape_of(t, from), tc::object_principal(t, *l)) {
                        (Some(s), p) => {
                            self.shapes.insert(s, ());
                            if let Some(p) = p {
                                self.tables.insert((p, s), ());
                            }
                        }
                        (None, Some(p)) => self.open.push(p),
                        // A `dyn` of auto traits alone has no method in its vtable (the
                        // Reference, "Auto traits": they have no associated items): no trait fn
                        // is called through it.
                        (None, None) => {}
                    },
                    (PNode::Ref(_, x) | PNode::Ptr(_, x), PNode::Ref(_, y) | PNode::Ptr(_, y)) => pairs.push((*x, *y)),
                    (PNode::Adt(ka, la), PNode::Adt(kb, lb)) if ka == kb => match (t.list(*la), t.list(*lb)) {
                        (Ok(xs), Ok(ys)) if xs.len() == ys.len() => {
                            for (x, y) in xs.iter().zip(ys.iter()) {
                                pairs.push((*x, *y));
                            }
                        }
                        _ => {
                            self.open_all = true;
                            break;
                        }
                    },
                    // No `dyn` below: a slice's unsizing (`[T; N]` to `[T]`), or a part the two
                    // sides share.
                    _ => {}
                }
            }
        }
    }
}

/// The trait fns met at an instance, on `dyn` and by the builder, and the pairs asked of them.
#[derive(Default)]
pub(super) struct Instances {
    /// The `(trait fn, shape)` pairs asked: exactly the pairs met.
    asked: HashMap<(ItemKey, Shape), (), Global>,
    /// Each shape a reached body's types hold (and `Param`).
    shapes: HashMap<Shape, (), Global>,
    /// The trait fns decided at the site ([`Reached::AtInstance`], [`Reached::AtObject`]) and
    /// not walked as a body yet.
    pub(super) declared: HashMap<ItemKey, (), Global>,
    /// Every trait fn a call site decided at an instance or on `dyn` (bits of [`Met::how`]),
    /// kept for the whole walk whatever became of the fn's census row. A PROVIDED trait fn is
    /// also a body (a concrete call whose impl leaves it out runs it, the Reference, Traits,
    /// "Associated functions"): walked as one, it is in `seen`, so a later site deciding it at
    /// an instance is dropped by the walk's pop, and an earlier site's `AtInstance` row is
    /// replaced by `Checked`. Read from the census alone, such a fn was never asked at its
    /// instances' shapes, and the impls overriding it were never reached (hello's stubs 15 and
    /// 220: `integer_min`/`integer_max` at `usize` reach `<impl PartialOrd for usize>::lt`;
    /// core calls `PartialOrd::lt` as a body through every derived `PartialOrd`).
    decided: HashMap<ItemKey, u8, Global>,
    /// How many of the census's bodies (and whether `firsts`) were read already.
    bodies_read: usize,
    first_read: bool,
    /// The impl index by trait: the shapes of its filed buckets, sorted; made from
    /// `by_trait_of` filed buckets ([`super::super::buckets::Buckets::trait_buckets`]).
    by_trait: HashMap<ItemKey, Vec<Shape, Global>, Global>,
    by_trait_of: Option<usize>,
    /// The shapes holding an impl whose trait path was not read, sorted.
    unread_shapes: Vec<Shape, Global>,
    at_param: HashMap<ItemKey, AtParam, Global>,
    coerced: Coerced,
    /// The slots of the tables the coercions make (slots.rs).
    tables: Tables,
}

impl Instances {
    /// Notes `k` popped from the work list with the outcome `decided` at its site: a trait fn
    /// decided at an instance or on `dyn` is asked about in every round from now on
    /// ([`Instances::decided`]), whether or not the walk files this row.
    pub(super) fn note_decided(&mut self, k: ItemKey, decided: Option<&Reached>) {
        let how = match decided {
            Some(Reached::AtInstance) => AT_INSTANCE,
            Some(Reached::AtObject) => AT_OBJECT,
            _ => return,
        };
        match self.decided.get_mut(&k) {
            Some(h) => *h |= how,
            None => {
                self.decided.insert(k, how);
            }
        }
    }

    /// The pairs asked, sorted: [`Census::instance_pairs`].
    pub(super) fn into_pairs(self) -> Vec<(ItemKey, Shape), Global> {
        let mut out: Vec<(ItemKey, Shape), Global> = Vec::with_capacity_in(self.asked.len(), Global);
        out.extend(self.asked.keys().copied());
        out.sort_unstable();
        out
    }

    /// Queues the impl fns of the pairs met and not asked yet; whether it queued any. Each impl
    /// fn queued is an edge ([`Census::edges`]) from the trait's fn decided at the call, or from
    /// [`AMBIENT`] for a question the builder asks on its own (drop glue, an overloaded index or
    /// dereference, `.await`). `inits` holds the coercions the reached initializers make.
    pub(super) fn round(&mut self, r: &mut Records<'_, '_>, firsts: &[&Checked<'_>], census: &mut Census<'_>, needs: &BackNeeds, inits: &Coerced, queue: &mut Vec<(ItemKey, Option<Reached>), Global>) -> bool {
        let add = |tys: tc::PTys<'_>, shapes: &mut HashMap<Shape, (), Global>| {
            for i in 0..tys.len() {
                if let Some(s) = shape_of(tys, PTy(i as u32)) {
                    shapes.insert(s, ());
                }
            }
        };
        if !self.first_read {
            self.first_read = true;
            // A type parameter's question is answered by the blanket impls alone.
            self.shapes.insert(Shape::Param, ());
            for first in firsts {
                add(first.types.view(), &mut self.shapes);
                self.coerced.read(first);
            }
        }
        for (_, c) in &census.bodies[self.bodies_read..] {
            add(c.types.view(), &mut self.shapes);
            self.coerced.read(c);
        }
        self.bodies_read = census.bodies.len();
        let fns = self.met(r, census, needs, inits);
        if fns.is_empty() {
            return false;
        }
        // The traits whose fns met on `dyn` alone are asked as at an instance (made only when
        // such a fn is met: the closure makes trait heads).
        let mut open: Vec<ItemKey, Global> = Vec::new_in(Global);
        let object_only = fns.iter().any(|m| m.how & (AT_INSTANCE | BY_BUILDER) == 0);
        let open_all = object_only && (self.coerced.open_all || inits.open_all || {
            let mut principals: Vec<ItemKey, Global> = Vec::new_in(Global);
            principals.extend_from_slice(&self.coerced.open);
            principals.extend_from_slice(&inits.open);
            principals.sort_unstable();
            principals.dedup();
            !supertrait_closure(r, &principals, &mut open)
        });
        open.sort_unstable();
        let mut coerced: Vec<Shape, Global> = Vec::new_in(Global);
        coerced.extend(self.coerced.shapes.keys().copied());
        coerced.extend(inits.shapes.keys().copied());
        coerced.sort_unstable();
        coerced.dedup();
        let mut met_sorted: Vec<Shape, Global> = Vec::new_in(Global);
        let mut shapes: Vec<Shape, Global> = Vec::new_in(Global);
        let before = queue.len();
        for m in fns.iter() {
            let wide = m.how & (AT_INSTANCE | BY_BUILDER) != 0 || open_all || open.binary_search(&m.trait_).is_ok();
            shapes.clear();
            if wide {
                shapes.push(Shape::Param);
                if self.every_met_shape(r, m.trait_) {
                    if met_sorted.is_empty() {
                        met_sorted.extend(self.shapes.keys().copied());
                        met_sorted.sort_unstable();
                    }
                    shapes.extend_from_slice(&met_sorted);
                } else {
                    let met = &self.shapes;
                    let filed = self.by_trait.get(&m.trait_).map_or(&[][..], |v| v.as_slice());
                    let lowered = match self.at_param.get(&m.trait_) {
                        Some(AtParam::Lowered(v)) => v.as_slice(),
                        _ => &[],
                    };
                    shapes.extend(filed.iter().chain(self.unread_shapes.iter()).chain(lowered.iter()).copied().filter(|s| met.contains_key(s)));
                }
            } else if m.how & AT_OBJECT != 0 {
                shapes.extend_from_slice(&coerced);
            } else {
                // A slot fn no call names: the shapes coerced to a principal whose table holds it.
                shapes.extend_from_slice(self.tables.shapes_of(m.f));
            }
            shapes.sort_unstable();
            shapes.dedup();
            // A slot's fn is the table's (made where a body coerces, whichever it is): from
            // `AMBIENT`, so every entry's reach holds it.
            let source = match m.how & (BY_BUILDER | IN_TABLE) != 0 {
                true => AMBIENT,
                false => m.f,
            };
            for &shape in shapes.iter() {
                if self.asked.insert((m.f, shape), ()).is_some() {
                    continue;
                }
                // A question whose candidates refused stays a gap: the back half's asking it
                // refuses by name (`trait_gap`), so nothing here is dropped unseen.
                let Some(impls) = answer_impls(r, m.trait_, shape) else { continue };
                let mark = queue.len();
                for imp in impls {
                    match r.keys.filed_member(imp, MemberKind::Fn, &m.name) {
                        Some(k) => queue.push((k, None)),
                        // The impl leaves the fn out: its instance runs the trait's provided
                        // body with `Self` the impl's type (the Reference, Traits: "Associated
                        // functions"), walked once as the trait's own member. A required fn
                        // left out is the checker's E0046, never reached here.
                        None => {
                            if (self.declared.contains_key(&m.f) || m.how & IN_TABLE != 0) && r.store.fns.get(&m.f).is_some_and(|h| h.has_body) {
                                queue.push((m.f, None));
                            }
                        }
                    }
                }
                note_edges(&mut census.edges, source, &queue[mark..], &[]);
            }
        }
        queue.len() > before
    }

    /// The trait fns to ask about this round, each once, sorted by key, with how each was met.
    fn met(&mut self, r: &mut Records<'_, '_>, census: &mut Census<'_>, needs: &BackNeeds, inits: &Coerced) -> Vec<Met, Global> {
        let mut fns: Vec<Met, Global> = Vec::new_in(Global);
        for (k, outcome) in census.fns.iter() {
            let how = match outcome {
                Reached::AtInstance => AT_INSTANCE,
                Reached::AtObject => AT_OBJECT,
                _ => continue,
            };
            if let Some(Origin::Member { container, kind: MemberKind::Fn, name }) = r.keys.origin(*k) {
                fns.push(Met { trait_: *container, f: *k, name: name.clone(), how });
            }
        }
        // The sites' decisions the census no longer holds (a provided fn walked as a body too):
        // merged with the rows above by key below.
        for (k, &how) in self.decided.iter() {
            if let Some(Origin::Member { container, kind: MemberKind::Fn, name }) = r.keys.origin(*k) {
                fns.push(Met { trait_: *container, f: *k, name: name.clone(), how });
            }
        }
        // The questions the builder asks on its own, by the lang trait and its fn's name.
        let mut builder: Vec<(&'static str, &'static str), Global> = Vec::new_in(Global);
        builder.push(("drop", "drop"));
        // `a[b]` on a type that is not an array or a slice: the builder calls the selected
        // `Index::index` / `IndexMut::index_mut` impl fn, which no checked body names
        // (back_needs.rs). Asked only once a reached body writes one.
        if needs.overloaded_index {
            builder.extend_from_slice(&[("index", "index"), ("index_mut", "index_mut")]);
        }
        // An overloaded dereference: the builder calls the selected `Deref::deref` /
        // `DerefMut::deref_mut` impl fn (blaze-build walk.rs `deref_call`), which no checked
        // body names (back_needs.rs `overloaded_deref`).
        if needs.overloaded_deref {
            builder.extend_from_slice(&[("deref", "deref"), ("deref_mut", "deref_mut")]);
        }
        // `.await`: the builder calls the selected `Future::poll` impl fn of the held future
        // (back_needs.rs `awaits`). A coroutine's own `poll` is the builder's, no impl's.
        if needs.awaits {
            builder.push(("future_trait", "poll"));
        }
        for (lang, name) in builder {
            let Some(Ok(tr)) = r.index.lang(lang).map(|lib| r.keys.item(lib)) else { continue };
            match r.keys.member(tr, MemberKind::Fn, name) {
                Ok(f) => fns.push(Met { trait_: tr, f, name: Box::from(name), how: BY_BUILDER }),
                // Two origins under the fn's key: refused by name, as the walk's own entries are.
                Err(clash) => {
                    if !census.fns.iter().any(|(k, _)| *k == clash.key) {
                        census.fns.push((clash.key, Reached::Failed { class: Box::from("key clash"), detail: format!("{clash}").into_boxed_str(), refusal: None }));
                    }
                }
            }
        }
        // Every slot of every table the coercions make (the Reference, "Trait objects": the
        // vtable holds each method of the trait and its supertraits for the concrete type),
        // called or not.
        self.tables.add(r, [&self.coerced, inits], &mut fns);
        fns.sort_unstable_by_key(|m| m.f);
        let mut out: Vec<Met, Global> = Vec::with_capacity_in(fns.len(), Global);
        for m in fns {
            match out.last_mut() {
                Some(last) if last.f == m.f => last.how |= m.how,
                _ => out.push(m),
            }
        }
        out
    }

    /// Whether `trait_` is asked at every met shape (its `Param` question is a gap, or the impl
    /// index is lazy: the two named classes of the module's doc); else its candidate shapes are
    /// ready in `by_trait`, `unread_shapes` and `at_param`.
    fn every_met_shape(&mut self, r: &mut Records<'_, '_>, trait_: ItemKey) -> bool {
        if !self.at_param.contains_key(&trait_) {
            // Asked first (it makes the buckets and lowers the shapeless candidates); the
            // round's own `(f, Param)` question is then a lookup.
            let state = match answer_impls(r, trait_, Shape::Param) {
                None => AtParam::Gap,
                Some(_) => {
                    let mut ks: Vec<LibKey, Global> = Vec::new_in(Global);
                    if let Some(b) = r.buckets.as_ref() {
                        ks.extend(b.shapeless_unread());
                        // Narrowed as the `(trait_, Param)` question just answered was.
                        ks.extend_from_slice(&b.negative_of(trait_, Shape::Param, b.home_of(trait_, Shape::Param)));
                    }
                    let view = r.store.tys.view();
                    let mut lowered: Vec<Shape, Global> = Vec::new_in(Global);
                    for k in ks {
                        if let Some(h) = r.store.impls.get(&checker_of(k))
                            && h.trait_ == Some(trait_)
                            && let Some(s) = shape_of(view, h.self_ty)
                        {
                            lowered.push(s);
                        }
                    }
                    lowered.sort_unstable();
                    lowered.dedup();
                    AtParam::Lowered(lowered)
                }
            };
            self.at_param.insert(trait_, state);
        }
        if matches!(self.at_param.get(&trait_), Some(AtParam::Gap)) {
            return true;
        }
        let Some(b) = r.buckets.as_ref() else { return true };
        if b.is_lazy() {
            return true;
        }
        let filed = b.trait_buckets();
        if self.by_trait_of != Some(filed) {
            self.by_trait.clear();
            let by_trait = &mut self.by_trait;
            b.each_trait_bucket(|t, s| match by_trait.get_mut(&t) {
                Some(v) => v.push(s),
                None => {
                    let mut v = Vec::new_in(Global);
                    v.push(s);
                    by_trait.insert(t, v);
                }
            });
            for v in self.by_trait.values_mut() {
                v.sort_unstable();
            }
            self.unread_shapes.clear();
            let unread = &mut self.unread_shapes;
            b.each_unread_shape(|s| unread.push(s));
            self.unread_shapes.sort_unstable();
            self.by_trait_of = Some(filed);
        }
        false
    }
}

/// Fills `out` with `principals` and every trait their supertrait clauses name, transitively (a
/// work list); whether it is whole (each trait's head made, within [`MOST_SUPERTRAITS`]).
fn supertrait_closure(r: &mut Records<'_, '_>, principals: &[ItemKey], out: &mut Vec<ItemKey, Global>) -> bool {
    let mut stack: Vec<ItemKey, Global> = Vec::new_in(Global);
    stack.extend_from_slice(principals);
    while let Some(t) = stack.pop() {
        if out.contains(&t) {
            continue;
        }
        if out.len() == MOST_SUPERTRAITS {
            return false;
        }
        out.push(t);
        if !r.store.trait_defs.contains_key(&t) {
            let Ok(want) = want_of_key(r.names, r.keys, t) else { return false };
            if r.store.ensure(r.names, r.keys, want).is_err() {
                return false;
            }
        }
        let Some(def) = r.store.trait_defs.get(&t) else { return false };
        for c in def.supertraits.iter() {
            match c {
                tc::PClause::Trait { trait_, .. } | tc::PClause::Projection { trait_, .. } => stack.push(*trait_),
                tc::PClause::Const { .. } | tc::PClause::ForFields { .. } | tc::PClause::TypeOutlives { .. } | tc::PClause::RegionOutlives { .. } => {}
            }
        }
    }
    true
}
