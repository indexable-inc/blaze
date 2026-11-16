//! Every fn reachable from a checked body, each lowered, converted and checked in its own env:
//! a free fn by [`lower_fn`], a member of a trait or an impl by [`lower_member`]. The walk is a
//! work list over the checker's callees (no recursion), capped at [`MOST_FNS`]; each fn's
//! outcome is kept, a failure by its cause's class, so the census is one line and a grouped
//! list. A call is walked to the body it runs ([`callee_target`]): a trait's method on a concrete
//! receiver to the selected impl's method; on a parameter or `dyn`, nothing at the site.

use std::alloc::Global;
use std::boxed::Box;
use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_hash::HashMap;
use blaze_lower::scoped::Prepares;
use blaze_typeck::{Checked, ItemKey};

use super::body::{Body, BodyError, lower_fn, lower_member};
use super::check::{BodyScope, CheckFail, Records, Target, callee_target, check_body, item_shown};
use super::check::{answer_impls, eval_len, make_all, world_at, MOST_ROUNDS};
use super::keys::checker_of;
use super::langs::lang_name;
use blaze_surface::ItemDetail;
use super::buckets::shape_of;
use super::heads::HeadError;
use super::keys::{MemberKind, Origin};
use blaze_typeck as tc;
use super::types::Why;
use super::spell::Spelling;
use crate::front::FnLang;

/// The most fns one walk visits (rule 18: a table has a cap).
pub const MOST_FNS: usize = 4096;

/// The most array-length constants one body may need evaluated before it lowers (rule 18).
const MOST_LENS: u32 = 64;

/// What became of one reachable fn.
#[derive(Debug)]
pub enum Reached {
    /// Lowered, converted and checked with no gap.
    Checked,
    /// No body: a required trait method (its impl is picked by instance resolution, which
    /// this walk does not do), a foreign fn or an intrinsic; counted apart, never as checked.
    NoBody,
    /// A constructor: the language's own fn, nothing to walk.
    Ctor,
    /// A trait method on a type parameter: walked where the instance is made.
    AtInstance,
    /// A trait method on `dyn Trait`: walked where the object is made.
    AtObject,
    /// Stopped: the cause's class (for grouping) and the whole cause.
    Failed { class: Box<str>, detail: Box<str> },
}

/// The walk's outcome: every fn reached, in the order first reached.
pub struct Census<'n> {
    pub fns: Vec<(ItemKey, Reached), Global>,
    /// Each [`Reached::Checked`] fn's lowering and checked body, in the order checked: what a
    /// program's table is made of, so no body is lowered or checked twice (rule 77).
    pub bodies: Vec<(Body<'n>, Checked<'static>), Global>,
    /// Whether [`MOST_FNS`] stopped the walk.
    pub capped: bool,
    /// The calls of bodies that did not check which only their types could name (a method
    /// call, a path through a type, a trait's fn, a qualified path): not followed.
    pub not_followed: usize,
}

/// Where a walk starts: a body that checked, or one that did not, whose calls by name are
/// still followed ([`queue_by_name`]) so the walk is total.
pub enum First<'a, 'c, 'n> {
    Checked(&'a mut Checked<'c>),
    Unchecked(&'a Body<'n>),
}

/// What `body` (written at `scope`, it did not check) reaches BY NAME, read from its lowering:
/// each path naming a free fn or an impl's fn, whole, is queued (a call by name resolves
/// without the body's types); each call only its types could name (a method call, a path
/// through a type or a qualified path, a trait's fn, which needs `Self`) is counted in
/// `not_followed`. The records a path's resolution needs are made as the checker makes them.
fn queue_by_name(r: &mut Records<'_, '_>, body: &Body<'_>, scope: &BodyScope<'_>, queue: &mut Vec<(ItemKey, Option<Reached>), Global>, not_followed: &mut usize) {
    let view = body.typeck.view(body.tys.view());
    let mut found: Vec<ItemKey, Global> = Vec::new_in(Global);
    let mut names: Vec<tc::Name, Global> = Vec::new_in(Global);
    let mut by_type = 0;
    for _ in 0..MOST_ROUNDS {
        found.clear();
        by_type = view.exprs.iter().filter(|e| matches!(e, tc::Expr::MethodCall { .. })).count();
        let world = world_at(r, scope);
        for bp in view.paths {
            if bp.anchor.is_some() {
                by_type += 1;
                continue;
            }
            let def = match bp.lang {
                // A lang path (`?`'s `Try::branch`, a panic fn): its fn by the `#[lang]` name.
                Some(l) => match lang_name(l).and_then(|n| r.index.lang(n)) {
                    Some(lib) if world.resolver().member_of(lib).is_some() => {
                        by_type += 1;
                        None
                    }
                    Some(lib) if matches!(world.resolver().detail(lib), Some(ItemDetail::Fn { .. })) => Some(tc::Def::Fn(checker_of(lib))),
                    _ => None,
                },
                None => {
                    names.clear();
                    let (start, len) = (bp.segments.start as usize, bp.segments.len as usize);
                    let Some(segs) = view.segments.get(start..start + len) else { continue };
                    names.extend(segs.iter().map(|s| s.name));
                    match world.path(&names) {
                        Ok(Some(p)) if p.consumed as usize == names.len() => Some(p.def),
                        // `Type::f`: the fn is found on the type's impls, by its type.
                        Ok(Some(_)) => {
                            by_type += 1;
                            None
                        }
                        _ => None,
                    }
                }
            };
            if let Some(tc::Def::Fn(k)) = def {
                found.push(k);
            }
        }
        let fills = world.fills.into_inner();
        drop(world.gaps.into_inner());
        if fills.is_empty() || make_all(r, fills, body.owner).is_err() {
            break;
        }
    }
    for k in found {
        let of_trait = matches!(r.keys.origin(k), Some(Origin::Member { container, kind: MemberKind::Fn, .. }) if !r.store.impls.contains_key(container));
        if of_trait {
            by_type += 1;
        } else {
            queue.push((k, None));
        }
    }
    *not_followed += by_type;
}

/// The class of a cause: its `Debug` form up to the first field.
fn class_of(debug: &str) -> Box<str> {
    let end = debug.find(['(', ' ', '{']).unwrap_or(debug.len());
    Box::from(&debug[..end])
}

fn body_failed(e: &BodyError) -> Reached {
    let d = format!("{e:?}");
    Reached::Failed { class: format!("lower: {}", class_of(&d)).into_boxed_str(), detail: format!("{e}").into_boxed_str() }
}

fn check_failed(r: &Records<'_, '_>, e: &CheckFail) -> Reached {
    let (class, detail): (String, String) = match e {
        // The keys named: the caller, the callee and the item the resolution stopped at.
        CheckFail::Resolve { owner, item, error, args } => {
            let at = match error {
                tc::ResolveError::NotGround { item: stopped } => format!(" (stopped at `{}`)", fn_shown(r, *stopped)),
                _ => String::new(),
            };
            let shown = format!("in `{}`, the call of `{}` at [{args}]: {error}{at}", fn_shown(r, *owner), fn_shown(r, *item));
            (format!("check: {}", class_of(&format!("{e:?}"))), shown)
        }
        CheckFail::Gap { gaps, .. } => {
            let first = gaps.first().map(|g| format!("{g:?}")).unwrap_or_default();
            (format!("check gap: {}", class_of(&first)), format!("{} gaps; the first: {}", gaps.len(), gaps.first().map(|g| format!("{g}")).unwrap_or_default()))
        }
        other => {
            let d = format!("{other:?}");
            (format!("check: {}", class_of(&d)), format!("{other}"))
        }
    };
    Reached::Failed { class: class.into_boxed_str(), detail: detail.into_boxed_str() }
}

/// A fn's name for messages: a member's as `container::name`.
pub(crate) fn fn_shown(r: &Records<'_, '_>, k: ItemKey) -> String {
    match r.keys.origin(k) {
        Some(Origin::Member { container, name, .. }) => format!("{}::{name}", item_shown(r, *container)),
        _ => item_shown(r, k),
    }
}

/// A body that finished with diagnostics, shown so a reader can judge each: the fn's name (a
/// member's as `container::name`), its text's first line, and the text each diagnostic covers.
fn diags_failed(r: &Records<'_, '_>, k: ItemKey, e: &CheckFail) -> Reached {
    let mut out = check_failed(r, e);
    let Reached::Failed { detail, .. } = &mut out else { return out };
    let diags: &[(_, _, _)] = match e {
        CheckFail::Diags { diags, .. } => diags,
        _ => &[],
    };
    let name = fn_shown(r, k);
    let text = match r.keys.library(k) {
        Some(lib) => r.names.item_text(lib).map(|t| t.text),
        None => r.store.fns.get(&k).map(|h| h.text),
    };
    let first = text.and_then(|t| t.lines().next()).unwrap_or("?");
    let mut d = format!("{name} (`{first}`): {detail}");
    for (_, at, _) in diags {
        let covered = text.and_then(|t| t.get(at.start as usize..at.end as usize)).unwrap_or("?");
        let cut = covered.char_indices().nth(240).map_or(covered, |(i, _)| &covered[..i]);
        d.push_str(&format!("\n    text {}..{}: {cut}", at.start, at.end));
    }
    *detail = d.into_boxed_str();
    out
}

/// Queues what each call in `c` (checked as `owner`, written at `scope`) runs: a body to walk,
/// or an outcome decided at the site (a ctor; a trait method on a parameter or on `dyn`; a
/// failed selection, kept under the callee's key).
fn queue_targets(r: &mut Records<'_, '_>, scope: &BodyScope<'_>, owner: ItemKey, c: &mut Checked<'_>, queue: &mut Vec<(ItemKey, Option<Reached>), Global>) {
    for i in 0..c.callees.len() {
        let item = c.callees.column::<1>()[i];
        let args = c.callees.column::<2>()[i];
        queue.push(match callee_target(r, scope, owner, item, args, &mut c.types) {
            Ok(Target::Body(k)) => (k, None),
            Ok(Target::Ctor(k)) => (k, Some(Reached::Ctor)),
            Ok(Target::AtInstance(k)) => (k, Some(Reached::AtInstance)),
            Ok(Target::AtObject(k)) => (k, Some(Reached::AtObject)),
            Err(e) => (item, Some(check_failed(r, &e))),
        });
    }
}

/// Walks from `first`'s callees (`first` checked as `owner` at `scope`) and from each fn the
/// back half calls on its own account ([`FnLang::ALL`], the seam's one list) that the
/// program's world declares: each fn once, its own callees queued when it checks.
/// A body that did not check (`first` unchecked, or one met on the way) is no dead end: what it
/// calls by name is walked too ([`queue_by_name`]), so the walk visits every fn reachable.
pub fn reach<'n>(r: &mut Records<'_, 'n>, prepared: &Prepares<'n>, owner: ItemKey, first: First<'_, '_, '_>, scope: &BodyScope<'_>) -> Census<'n> {
    let mut seen: HashMap<ItemKey, (), Global> = HashMap::new_in(Global);
    // `first` is walked already: a call back to it is no new fn.
    seen.insert(owner, ());
    let mut queue: Vec<(ItemKey, Option<Reached>), Global> = Vec::new_in(Global);
    for l in FnLang::ALL {
        let Some(lib) = r.index.lang(l.lang()) else { continue };
        queue.push(match r.keys.item(lib) {
            Ok(k) => (k, None),
            Err(clash) => (clash.key, Some(Reached::Failed { class: Box::from("key clash"), detail: format!("{clash}").into_boxed_str() })),
        });
    }
    let mut census = Census { fns: Vec::new_in(Global), bodies: Vec::new_in(Global), capped: false, not_followed: 0 };
    let first: Option<&Checked<'_>> = match first {
        First::Checked(c) => {
            queue_targets(r, scope, owner, c, &mut queue);
            Some(c)
        }
        First::Unchecked(body) => {
            queue_by_name(r, body, scope, &mut queue, &mut census.not_followed);
            None
        }
    };
    let mut instances = Instances::default();
    // Rounds: the work list drained, then the impl fns the back half's instances can resolve
    // to, until a round adds none (each round adds a fn, so `MOST_FNS` bounds the rounds).
    loop {
    while let Some((k, decided)) = queue.pop() {
        // A trait's provided fn met on a parameter or `dyn` is decided at the site; it is walked
        // as a body only when an instance resolves to it (an impl leaves it out: `Instances`),
        // and its decided outcome is then replaced by the walk's.
        let provided = decided.is_none() && instances.declared.remove(&k).is_some();
        if provided {
            census.fns.retain(|(f, _)| *f != k);
        } else if seen.insert(k, ()).is_some() {
            continue;
        }
        if matches!(decided, Some(Reached::AtInstance | Reached::AtObject)) {
            instances.declared.insert(k, ());
        }
        if census.fns.len() == MOST_FNS {
            census.capped = true;
            break;
        }
        if let Some(outcome) = decided {
            census.fns.push((k, outcome));
            continue;
        }
        // A fn whose head says it has no body (a foreign fn's declaration, an intrinsic's):
        // nothing to lower; what calls it is the table's to say.
        if r.store.fns.get(&k).is_some_and(|h| !h.has_body) {
            census.fns.push((k, Reached::NoBody));
            continue;
        }
        // A length naming a constant (`[0; HELD]`, or one in a head the body needs) has it
        // evaluated and the body lowered again, at most `MOST_LENS` times.
        let mut lens = 0;
        let lowered = loop {
            let lowered = match r.keys.library(k) {
                Some(lib) => lower_fn(r.names, prepared, r.keys, r.store, lib),
                None => lower_member(r.names, prepared, r.keys, r.store, k),
            };
            let c = match &lowered {
                Err(BodyError::Convert { why: Some(Why::NeedConst(c, _)), .. }) | Err(BodyError::Head { error: HeadError::Item(_, Why::NeedConst(c, _)), .. }) if lens < MOST_LENS => *c,
                _ => break lowered,
            };
            lens += 1;
            if let Err(e) = eval_len(r, c) {
                break Err(BodyError::Const { item: c, error: Box::new_in(e, Global) });
            }
        };
        let body = match lowered {
            Ok(b) => b,
            Err(BodyError::NoBody(_)) => {
                census.fns.push((k, Reached::NoBody));
                continue;
            }
            Err(e) => {
                census.fns.push((k, body_failed(&e)));
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = match Spelling::of(&view) {
            Ok(s) => s,
            Err(e) => {
                let d = format!("{e:?}");
                census.fns.push((k, Reached::Failed { class: Box::from("spelling"), detail: d.into_boxed_str() }));
                continue;
            }
        };
        let scope = body.scope(&spell);
        let checked = match check_body(r, &body, &scope) {
            Ok(mut c) => {
                queue_targets(r, &scope, body.owner, &mut c, &mut queue);
                census.fns.push((k, Reached::Checked));
                Some(c)
            }
            Err(e) => {
                census.fns.push((k, diags_failed(r, k, &e)));
                queue_by_name(r, &body, &scope, &mut queue, &mut census.not_followed);
                None
            }
        };
        drop(scope);
        drop(spell);
        drop(view);
        if let Some(c) = checked {
            census.bodies.push((body, c));
        }
    }
    if census.capped || !instances.round(r, first, &census, &mut queue) {
        break;
    }
    }
    census
}

/// What the back half resolves at an instance, made before the build: for each trait fn the
/// walk met on a parameter or `dyn` (and `Drop::drop`, which drop glue calls from the type
/// alone), the impls answering `ImplsOf(trait, shape)` at every shape a checked body's types
/// have, and each one's fn of that name, walked like any fn. An instance's self type is built
/// from the types bodies write, so its shape is among theirs: the set is a function of the
/// program, never of the order the back half asks in (rule 25), and every question it asks is
/// answered in the same buckets the checker's own questions use.
#[derive(Default)]
struct Instances {
    /// The `(trait, fn, shape)` questions answered so far.
    asked: HashMap<(ItemKey, Box<str>, tc::Shape), (), Global>,
    shapes: HashMap<tc::Shape, (), Global>,
    /// The trait fns decided at the site ([`Reached::AtInstance`], [`Reached::AtObject`]) and
    /// not walked as a body yet.
    declared: HashMap<ItemKey, (), Global>,
    /// How many of the census's bodies (and whether `first`) gave their shapes already.
    bodies_read: usize,
    first_read: bool,
}

impl Instances {
    /// Queues the impl fns of the questions not asked yet; whether it queued any.
    fn round(&mut self, r: &mut Records<'_, '_>, first: Option<&Checked<'_>>, census: &Census<'_>, queue: &mut Vec<(ItemKey, Option<Reached>), Global>) -> bool {
        let mut add = |tys: tc::PTys<'_>, shapes: &mut HashMap<tc::Shape, (), Global>| {
            for i in 0..tys.nodes.len() {
                if let Some(s) = shape_of(tys, tc::PTy(i as u32)) {
                    shapes.insert(s, ());
                }
            }
        };
        if !self.first_read {
            self.first_read = true;
            // A type parameter's question is answered by the blanket impls alone.
            self.shapes.insert(tc::Shape::Param, ());
            if let Some(first) = first {
                add(first.types.view(), &mut self.shapes);
            }
        }
        for (_, c) in &census.bodies[self.bodies_read..] {
            add(c.types.view(), &mut self.shapes);
        }
        self.bodies_read = census.bodies.len();
        let mut traits: Vec<(ItemKey, Box<str>), Global> = Vec::new_in(Global);
        for (k, outcome) in census.fns.iter() {
            if let (Reached::AtInstance | Reached::AtObject, Some(Origin::Member { container, kind: MemberKind::Fn, name })) = (outcome, r.keys.origin(*k)) {
                traits.push((*container, name.clone()));
            }
        }
        if let Some(Ok(drop)) = r.index.lang("drop").map(|lib| r.keys.item(lib)) {
            traits.push((drop, Box::from("drop")));
        }
        traits.sort_unstable();
        traits.dedup();
        let mut shapes: Vec<(String, tc::Shape), Global> = Vec::new_in(Global);
        shapes.extend(self.shapes.keys().map(|s| (format!("{s:?}"), *s)));
        shapes.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let before = queue.len();
        for (trait_, name) in traits.iter() {
            for (_, shape) in shapes.iter() {
                if self.asked.insert((*trait_, name.clone(), *shape), ()).is_some() {
                    continue;
                }
                // A question whose candidates refused stays a gap: the back half's asking it
                // refuses by name (`trait_gap`), so nothing here is dropped unseen.
                let Some(impls) = answer_impls(r, *trait_, *shape) else { continue };
                for imp in impls {
                    match r.keys.filed_member(imp, MemberKind::Fn, name) {
                        Some(m) => queue.push((m, None)),
                        // The impl leaves the fn out: its instance runs the trait's provided
                        // body with `Self` the impl's type (the Reference, Traits: "Associated
                        // functions"), walked once as the trait's own member. A required fn
                        // left out is the checker's E0046, never reached here.
                        None => {
                            let provided = r.keys.filed_member(*trait_, MemberKind::Fn, name);
                            if let Some(d) = provided.filter(|d| self.declared.contains_key(d) && r.store.fns.get(d).is_some_and(|h| h.has_body)) {
                                queue.push((d, None));
                            }
                        }
                    }
                }
            }
        }
        queue.len() > before
    }
}
