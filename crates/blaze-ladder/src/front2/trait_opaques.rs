//! `impl Trait` and `async fn` in traits: the impl's VALUE of the trait fn's opaque types
//! (port27/tempo-run/ASYNC.md, D-A4 and prompt P4).
//!
//! A trait fn's `impl Trait` (and an `async fn`'s future, index 0) is the opaque
//! `Opaque(opaque_key(trait_fn, j), args)`: rigid to every caller through a generic, which sees
//! its bounds only. The Reference, "Impl trait", "Abstract return types": "the caller ... can
//! only use the methods declared by the trait bounds"; "Impl trait", "`impl Trait` in traits"
//! (stable since 1.75, `async fn` in traits with it): an impl's fn may write the same `impl
//! Trait`, an `async fn`, or a type that implements the bounds, and calls through the impl see
//! that type. So the opaque has one value per impl, fixed by the impl fn's signature:
//!
//! - [`impl_values`]: the trait fn's output (its opaques as holes) walked against the impl fn's
//!   output, one work list, no recursion; at each trait-side opaque the impl side's type (in
//!   the impl fn's generics) is the value. A trait part that holds no opaque is not compared
//!   here (the signature compare is the checker's); a part that holds one must have the impl
//!   side's kind, key and arity, else E0053's rule refuses it by name.
//! - [`trait_members_first`], [`file_impl_values`]: the head store's side. Made ONCE, when an
//!   impl's fn's head is made, as rows `(trait opaque key, value)` sorted by key in
//!   `Store::impl_opaques`, and written in the head record's `ImplOpaques` kind.
//! - [`trait_opaque_at`]: the builder's side (front2/coroutines.rs `opaque_hidden`): an opaque
//!   whose owner is a trait fn is resolved, at its ground arguments, to the impl fn
//!   (`blaze_typeck::resolve_instance`) and that fn's row; a trait fn the impl does not
//!   override (its provided body) defines its own opaques, as a free fn does.
//!
//! Refused by name, never guessed: a mismatch at an opaque; an `impl Trait` inside a
//! projection, a closure, an async block or a hole of a head's type; an impl fn the trait fn
//! resolves to that has no row for the opaque.

use std::format;

use blaze_id::ItemKey as LibKey;
use blaze_own::{Alloc, Vec};
use blaze_surface::Range;
use blaze_typeck::{ItemKey, List, PNode, PTy, PTys, PortableTys, ResolveError};

use super::heads::Store;
use super::keys::{Keys, MemberKind};
use super::source::NamesSource;
use super::types::{SelfTy, Why};

/// The place a head's refusal is reported at: a head's types carry no range of their own.
const WHOLE: Range = Range::new(0, 0);

/// The values the impl fn whose output is `impl_out` (a type of `impl_types`, in the impl fn's
/// generics) gives the opaques of the trait fn whose output is `trait_out` (a type of
/// `trait_types`): one row `(opaque key, value)` per trait-side opaque, the value re-interned
/// into `out`, sorted by key.
///
/// An `async fn` in the trait against `fn .. -> impl Future<Output = T>` in the impl: both
/// are index 0 of their fn, and the value is the impl's opaque; against `fn .. -> Pin<Box<dyn
/// Future ..>>` the value is that type. The `{j}` of a mismatch counts the trait-side opaques
/// met before it in the walk's order (left to right, outside in).
pub(crate) fn impl_values<'a>(
    trait_out: PTy,
    impl_out: PTy,
    trait_types: PTys<'_>,
    impl_types: PTys<'_>,
    out: &mut PortableTys<'_>,
    alloc: Alloc<'a>,
) -> Result<Vec<(ItemKey, PTy), Alloc<'a>>, Why> {
    // Which trait-side nodes hold an opaque: children have smaller indices, so in index order.
    let reached = reached(trait_types, trait_out, alloc)?;
    let mut holds: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(reached.len(), alloc);
    let mut kids: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
    for &t in reached.iter() {
        let node = trait_types.node(t).map_err(|_| past("a trait fn's return type"))?;
        let mut h = matches!(node, PNode::Opaque(..));
        if !h {
            kids.clear();
            blaze_build::instance::each_child(trait_types, node, &mut kids).map_err(|_| past("a trait fn's return type"))?;
            for c in kids.iter() {
                let at = reached.binary_search(c).map_err(|_| past("a trait fn's return type"))?;
                h |= holds[at];
            }
        }
        holds.push(h);
    }
    let holds_at = |t: PTy| -> Result<bool, Why> {
        let at = reached.binary_search(&t).map_err(|_| past("a trait fn's return type"))?;
        Ok(holds[at])
    };

    let mut rows: Vec<(ItemKey, PTy), Alloc<'a>> = Vec::new_in(alloc);
    let mut work: Vec<(PTy, PTy), Alloc<'a>> = Vec::new_in(alloc);
    let mut pairs: Vec<(PTy, PTy), Alloc<'a>> = Vec::new_in(alloc);
    work.push((trait_out, impl_out));
    let mut met: u32 = 0;
    while let Some((t, i)) = work.pop() {
        if !holds_at(t)? {
            continue;
        }
        let tn = trait_types.node(t).map_err(|_| past("a trait fn's return type"))?;
        if let PNode::Opaque(k, _) = tn {
            let value = copy_into(impl_types, i, out, alloc)?;
            match rows.iter().find(|r| r.0 == k) {
                Some(&(_, before)) if before != value => {
                    return Err(Why::Program(format!("the impl fn's return type gives the trait fn's opaque {met} two types (E0053)").into_boxed_str(), WHOLE));
                }
                Some(_) => {}
                None => rows.push((k, value)),
            }
            met += 1;
            continue;
        }
        let inn = impl_types.node(i).map_err(|_| past("an impl fn's return type"))?;
        pairs.clear();
        let same = match (tn, inn) {
            (PNode::Alias { .. }, _) => return Err(Why::Unsupported("an `impl Trait` inside a projection of a trait fn's return type", WHOLE)),
            (PNode::Closure { .. } | PNode::Future(..) | PNode::Infer | PNode::Error, _) => {
                return Err(Why::Unsupported("a closure, an async block's future, a hole or an error type in a trait fn's return type", WHOLE));
            }
            (PNode::Tuple(a), PNode::Tuple(b)) | (PNode::Dynamic(a), PNode::Dynamic(b)) => pair_lists(trait_types, a, impl_types, b, &mut pairs)?,
            (PNode::Ref(m1, a), PNode::Ref(m2, b)) | (PNode::Ptr(m1, a), PNode::Ptr(m2, b)) => {
                pairs.push((a, b));
                m1 == m2
            }
            (PNode::Array(a, n1), PNode::Array(b, n2)) => {
                pairs.push((a, b));
                pairs.push((n1, n2));
                true
            }
            (PNode::Slice(a), PNode::Slice(b)) => {
                pairs.push((a, b));
                true
            }
            (PNode::Pat(a, l1), PNode::Pat(b, l2)) => {
                pairs.push((a, b));
                pair_lists(trait_types, l1, impl_types, l2, &mut pairs)?
            }
            (PNode::Adt(k1, a), PNode::Adt(k2, b)) | (PNode::FnDef(k1, a), PNode::FnDef(k2, b)) | (PNode::ExTrait(k1, a), PNode::ExTrait(k2, b)) => {
                k1 == k2 && pair_lists(trait_types, a, impl_types, b, &mut pairs)?
            }
            (PNode::FnPtr(a, h1), PNode::FnPtr(b, h2)) => h1 == h2 && pair_lists(trait_types, a, impl_types, b, &mut pairs)?,
            (PNode::ExProj { trait_: t1, name: n1, args: a, term: x1 }, PNode::ExProj { trait_: t2, name: n2, args: b, term: x2 }) => {
                let same = t1 == t2 && n1 == n2 && pair_lists(trait_types, a, impl_types, b, &mut pairs)?;
                pairs.push((x1, x2));
                same
            }
            (PNode::ConstValue { ty: a, bits: b1 }, PNode::ConstValue { ty: b, bits: b2 }) => {
                pairs.push((a, b));
                b1 == b2
            }
            _ => false,
        };
        if !same {
            return Err(Why::Program(format!("the impl fn's return type does not have the trait fn's shape at opaque {met} (E0053)").into_boxed_str(), WHOLE));
        }
        // Reversed, so the first child is walked first: the order `{j}` counts in.
        for &p in pairs.iter().rev() {
            work.push(p);
        }
    }
    rows.sort_unstable_by_key(|r| r.0);
    Ok(rows)
}

/// A refusal for a type index past its table.
fn past(what: &'static str) -> Why {
    Why::Program(format!("{what} names a type past its table").into_boxed_str(), WHOLE)
}

/// Pairs the entries of `a` (of `ta`) and `b` (of `tb`) into `pairs`; `false` when their
/// lengths differ.
fn pair_lists(ta: PTys<'_>, a: List, tb: PTys<'_>, b: List, pairs: &mut Vec<(PTy, PTy), Alloc<'_>>) -> Result<bool, Why> {
    let xs = ta.list(a).map_err(|_| past("a trait fn's return type"))?;
    let ys = tb.list(b).map_err(|_| past("an impl fn's return type"))?;
    if xs.len() != ys.len() {
        return Ok(false);
    }
    for (&x, &y) in xs.iter().zip(ys.iter()) {
        pairs.push((x, y));
    }
    Ok(true)
}

/// Every node `top` reaches in `from` (itself too), each once, in index order. A work list:
/// a head's return type is small, so the seen test is a scan.
fn reached<'a>(from: PTys<'_>, top: PTy, alloc: Alloc<'a>) -> Result<Vec<PTy, Alloc<'a>>, Why> {
    let mut seen: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
    let mut stack: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
    let mut kids: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
    seen.push(top);
    stack.push(top);
    while let Some(t) = stack.pop() {
        let node = from.node(t).map_err(|_| past("a fn's return type"))?;
        kids.clear();
        blaze_build::instance::each_child(from, node, &mut kids).map_err(|_| past("a fn's return type"))?;
        for &c in kids.iter() {
            if !seen.contains(&c) {
                seen.push(c);
                stack.push(c);
            }
        }
    }
    seen.sort_unstable();
    Ok(seen)
}

/// `ty` of `from` interned into `out` (each node after its children, in index order: no
/// recursion). Parameters, projections and opaque types are kept: the value is in the impl
/// fn's generics. A closure, an async block's future, a hole or an error type is no head's.
fn copy_into(from: PTys<'_>, ty: PTy, out: &mut PortableTys<'_>, alloc: Alloc<'_>) -> Result<PTy, Why> {
    let reached = reached(from, ty, alloc)?;
    let mut mapped: Vec<PTy, Alloc<'_>> = Vec::with_capacity_in(reached.len(), alloc);
    let mut list: Vec<PTy, Alloc<'_>> = Vec::new_in(alloc);
    let full = |_| Why::Program("the heads' type table is full".into(), WHOLE);
    for &t in reached.iter() {
        let node = from.node(t).map_err(|_| past("an impl fn's return type"))?;
        let m = |c: PTy, mapped: &[PTy]| -> Result<PTy, Why> {
            let at = reached.binary_search(&c).map_err(|_| past("an impl fn's return type"))?;
            mapped.get(at).copied().ok_or_else(|| past("an impl fn's return type"))
        };
        let mut l = |l: List, mapped: &[PTy], out: &mut PortableTys<'_>| -> Result<List, Why> {
            list.clear();
            for &c in from.list(l).map_err(|_| past("an impl fn's return type"))? {
                list.push(m(c, mapped)?);
            }
            out.intern_list(&list).map_err(full)
        };
        let copied = match node {
            PNode::Closure { .. } | PNode::Future(..) | PNode::Infer | PNode::Error => {
                return Err(Why::Unsupported("a closure, an async block's future, a hole or an error type in an impl fn's return type", WHOLE));
            }
            n @ (PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) | PNode::Str | PNode::Never | PNode::Param(_) | PNode::ConstParam(_) | PNode::ExAuto(_)) => n,
            PNode::ConstValue { ty, bits } => PNode::ConstValue { ty: m(ty, &mapped)?, bits },
            PNode::Tuple(a) => PNode::Tuple(l(a, &mapped, out)?),
            PNode::Ref(mu, a) => PNode::Ref(mu, m(a, &mapped)?),
            PNode::Ptr(mu, a) => PNode::Ptr(mu, m(a, &mapped)?),
            PNode::Array(a, n) => PNode::Array(m(a, &mapped)?, m(n, &mapped)?),
            PNode::Slice(a) => PNode::Slice(m(a, &mapped)?),
            PNode::Pat(a, r) => PNode::Pat(m(a, &mapped)?, l(r, &mapped, out)?),
            PNode::Adt(k, a) => PNode::Adt(k, l(a, &mapped, out)?),
            PNode::FnDef(k, a) => PNode::FnDef(k, l(a, &mapped, out)?),
            PNode::FnPtr(a, h) => PNode::FnPtr(l(a, &mapped, out)?, h),
            PNode::Alias { trait_, name, args } => PNode::Alias { trait_, name, args: l(args, &mapped, out)? },
            PNode::Opaque(k, a) => PNode::Opaque(k, l(a, &mapped, out)?),
            PNode::Dynamic(a) => PNode::Dynamic(l(a, &mapped, out)?),
            PNode::ExTrait(k, a) => PNode::ExTrait(k, l(a, &mapped, out)?),
            PNode::ExProj { trait_, name, args, term } => PNode::ExProj { trait_, name, args: l(args, &mapped, out)?, term: m(term, &mapped)? },
        };
        let at = out.intern(copied).map_err(full)?;
        mapped.push(at);
    }
    let top = reached.binary_search(&ty).map_err(|_| past("an impl fn's return type"))?;
    mapped.get(top).copied().ok_or_else(|| past("an impl fn's return type"))
}

/// Before the signature of fn `name` of impl `impl_` (a checker key whose head is made): its
/// trait's members (each trait fn's head, its opaques among them) must be made, so the fn's
/// values ([`file_impl_values`]) are made with its signature, once. `Some(lib)`: return
/// `Why::Need(lib)` first (`Store::need` gives a trait's members once its head is made).
/// `None`: an inherent impl, or the members are made.
pub(crate) fn trait_members_first<'n, S: NamesSource<'n>>(store: &Store<'n>, keys: &Keys, names: S, impl_: ItemKey) -> Result<Option<LibKey>, Why> {
    let Some(head) = store.impls.get(&impl_) else {
        return Err(Why::Program(format!("impl {impl_:?}: a member fn asked before its head").into_boxed_str(), WHOLE));
    };
    let Some(trait_) = head.trait_ else { return Ok(None) };
    // `Want::Members` is made when the trait's clauses are filed (heads.rs `step`).
    if store.preds.contains_key(&trait_) {
        return Ok(None);
    }
    match keys.library_in(names, trait_) {
        Some(lib) => Ok(Some(lib)),
        None => Err(Why::Program(format!("the trait {trait_:?} of impl {impl_:?} has no library key").into_boxed_str(), WHOLE)),
    }
}

/// Files the values impl `impl_`'s fn `fn_` (named `name`, its head just made) gives its
/// trait fn's opaques, in `Store::impl_opaques` (no row when the trait fn has none). A
/// mismatch (E0053) makes the fn's signature its gap, as any refusal of a member's head is.
/// A trait fn whose head was refused, or a fn the trait does not declare (E0407, the
/// checker's), gets no row: a use of the trait fn's opaque at this impl is then refused by
/// [`trait_opaque_at`] naming the missing row.
pub(crate) fn file_impl_values(store: &mut Store<'_>, keys: &Keys, impl_: ItemKey, fn_: ItemKey, name: &str, alloc: Alloc<'static>) -> Result<(), Why> {
    let Some(impl_head) = store.fns.get(&fn_) else { return Ok(()) };
    let Some(trait_) = store.impls.get(&impl_).and_then(|h| h.trait_) else { return Ok(()) };
    let Some(trait_fn) = keys.filed_member(trait_, MemberKind::Fn, name) else { return Ok(()) };
    let Some(trait_head) = store.fns.get(&trait_fn) else { return Ok(()) };
    let made = {
        // Both heads are in the store's table: an overlay over it re-interns each value to the
        // index it already has there, so a row's type is a type of `store.tys`.
        let mut out = PortableTys::over(&store.tys, alloc).map_err(|_| Why::Program("the heads' type table is an overlay".into(), WHOLE))?;
        let view = store.tys.view();
        let rows = impl_values(trait_head.output, impl_head.output, view, view, &mut out, alloc);
        if out.view().len() != view.len() {
            return Err(Why::Program(format!("fn {fn_:?}: a value of its trait fn's `impl Trait` is a type the heads' table does not hold").into_boxed_str(), WHOLE));
        }
        rows
    };
    match made {
        Ok(rows) if rows.is_empty() => Ok(()),
        Ok(rows) => {
            store.impl_opaques.insert(fn_, rows);
            Ok(())
        }
        Err(why) => {
            store.fns.remove(&fn_);
            store.fn_gaps.insert(fn_, why);
            Ok(())
        }
    }
}

/// What an opaque type of fn `owner` is at ground arguments, for the builder.
pub enum TraitOpaqueAt<'a> {
    /// `owner` is no trait's fn: its own checked body defines the opaque.
    OwnBody,
    /// `owner` is a trait fn the selected impl does not override: its provided body is the
    /// instance (`resolve_instance` gives the trait fn back) and defines the opaque.
    DefaultBody,
    /// The impl fn the trait fn resolves to, its ground arguments (a list of the ground
    /// table), and its value of the opaque: a type of `types` in the impl fn's generics.
    Impl { impl_fn: ItemKey, args: List, value: PTy, types: PTys<'a> },
}

/// Why [`trait_opaque_at`] gave no answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraitOpaqueWhy {
    /// The trait fn did not resolve at the opaque's arguments.
    Resolve(ResolveError),
    /// The program has no body whose world resolves in (no entry fn).
    NoWorld,
    /// The impl fn has no value of the opaque (its trait fn's head was refused, or the impl
    /// fn's head was).
    NoRow { impl_fn: ItemKey, opaque: ItemKey },
}

impl TraitOpaqueWhy {
    /// The refusal's text, as the builder's `UnsupportedSig` carries it.
    pub fn what(&self) -> &'static str {
        match self {
            TraitOpaqueWhy::Resolve(_) => "an `impl Trait` type of a trait fn whose call does not resolve to an impl at its arguments",
            TraitOpaqueWhy::NoWorld => "an `impl Trait` type of a trait fn in a program with no entry fn to resolve it in",
            TraitOpaqueWhy::NoRow { .. } => "an `impl Trait` type of a trait fn whose impl fn has no value of it (its head or the trait fn's was refused)",
        }
    }
}

/// `opaque`, made by fn `owner`'s head, at ground arguments: for a trait fn, `resolve`
/// (the trait fn at the opaque's arguments, in `ground`: `blaze_typeck::resolve_instance`)
/// and the impl fn's row for the key.
pub(crate) fn trait_opaque_at<'s>(
    store: &'s Store<'_>,
    opaque: ItemKey,
    owner: ItemKey,
    ground: &mut PortableTys<'_>,
    resolve: impl FnOnce(&mut PortableTys<'_>) -> Result<(ItemKey, List), TraitOpaqueWhy>,
) -> Result<TraitOpaqueAt<'s>, TraitOpaqueWhy> {
    let is_trait_fn = store.fns.get(&owner).is_some_and(|h| matches!(h.self_, SelfTy::Trait(_)));
    if !is_trait_fn {
        return Ok(TraitOpaqueAt::OwnBody);
    }
    let (impl_fn, args) = resolve(ground)?;
    if impl_fn == owner {
        return Ok(TraitOpaqueAt::DefaultBody);
    }
    let row = store.impl_opaques.get(&impl_fn).and_then(|rows| rows.binary_search_by_key(&opaque, |r| r.0).ok().map(|i| rows[i].1));
    let Some(value) = row else { return Err(TraitOpaqueWhy::NoRow { impl_fn, opaque }) };
    Ok(TraitOpaqueAt::Impl { impl_fn, args, value, types: store.tys.view() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_typeck::UintTy;

    fn ty(t: &mut PortableTys<'_>, n: PNode) -> PTy {
        t.intern(n).expect("a test table has room")
    }
    fn list(t: &mut PortableTys<'_>, l: &[PTy]) -> List {
        t.intern_list(l).expect("a test table has room")
    }

    /// The Reference, "Impl trait", "`impl Trait` in traits": a trait fn `fn get(&self) ->
    /// Option<impl Sized>` is implemented by `fn get(&self) -> Option<u32>`, and through that
    /// impl the opaque is `u32`. The trait's and the impl's types are in tables with different
    /// numberings, and the value is re-interned into a third: the row must hold `out`'s `u32`,
    /// at the trait fn's opaque key. A wrong walk (no descent into `Option`, or the impl's own
    /// index kept) gives no row or another index.
    ///
    /// Prints: `test front2::trait_opaques::tests::the_value_is_the_impls_type_at_the_opaque ... ok`
    #[test]
    fn the_value_is_the_impls_type_at_the_opaque() {
        let (opt, k) = (ItemKey(0x0f), ItemKey(0x70));
        let mut tt = PortableTys::new_in(Alloc::global());
        let me = ty(&mut tt, PNode::Opaque(k, List::EMPTY));
        let args = list(&mut tt, &[me]);
        let trait_out = ty(&mut tt, PNode::Adt(opt, args));
        let mut it = PortableTys::new_in(Alloc::global());
        let _shift = ty(&mut it, PNode::Bool);
        let u = ty(&mut it, PNode::Uint(UintTy::U32));
        let args = list(&mut it, &[u]);
        let impl_out = ty(&mut it, PNode::Adt(opt, args));
        let mut out = PortableTys::new_in(Alloc::global());
        let _shift = ty(&mut out, PNode::Char);
        let _shift = ty(&mut out, PNode::Str);
        let rows = impl_values(trait_out, impl_out, tt.view(), it.view(), &mut out, Alloc::global()).expect("the shapes match");
        let want = ty(&mut out, PNode::Uint(UintTy::U32));
        assert_eq!(&rows[..], &[(k, want)][..]);
    }

    /// An `async fn get(&self) -> u32` in the trait is its opaque 0; an impl's `fn get(&self)
    /// -> impl Future<Output = u32>` is the impl fn's own opaque 0 (the Reference, "Async
    /// functions": "roughly equivalent to" a fn returning `impl Future`): the value is the
    /// impl's opaque, with its own arguments, copied whole.
    ///
    /// Prints: `test front2::trait_opaques::tests::an_async_fn_against_an_impl_future_is_the_impls_opaque ... ok`
    #[test]
    fn an_async_fn_against_an_impl_future_is_the_impls_opaque() {
        let (k_trait, k_impl) = (ItemKey(0x71), ItemKey(0x72));
        let mut tt = PortableTys::new_in(Alloc::global());
        let p0 = ty(&mut tt, PNode::Param(0));
        let args = list(&mut tt, &[p0]);
        let trait_out = ty(&mut tt, PNode::Opaque(k_trait, args));
        let mut it = PortableTys::new_in(Alloc::global());
        let p1 = ty(&mut it, PNode::Param(1));
        let p0 = ty(&mut it, PNode::Param(0));
        let args = list(&mut it, &[p0, p1]);
        let impl_out = ty(&mut it, PNode::Opaque(k_impl, args));
        let mut out = PortableTys::new_in(Alloc::global());
        let rows = impl_values(trait_out, impl_out, tt.view(), it.view(), &mut out, Alloc::global()).expect("an opaque takes any value");
        let p0 = ty(&mut out, PNode::Param(0));
        let p1 = ty(&mut out, PNode::Param(1));
        let args = list(&mut out, &[p0, p1]);
        let want = ty(&mut out, PNode::Opaque(k_impl, args));
        assert_eq!(&rows[..], &[(k_trait, want)][..]);
    }

    /// E0053's rule: an impl fn's signature must match the trait fn's. A trait fn returning
    /// `(impl Sized, bool)` against an impl returning `(u32,)` has no value for the opaque:
    /// refused naming the shape, never a row guessed from the first field.
    ///
    /// Prints: `test front2::trait_opaques::tests::a_shape_mismatch_at_an_opaque_is_refused ... ok`
    #[test]
    fn a_shape_mismatch_at_an_opaque_is_refused() {
        let k = ItemKey(0x73);
        let mut tt = PortableTys::new_in(Alloc::global());
        let me = ty(&mut tt, PNode::Opaque(k, List::EMPTY));
        let b = ty(&mut tt, PNode::Bool);
        let fields = list(&mut tt, &[me, b]);
        let trait_out = ty(&mut tt, PNode::Tuple(fields));
        let mut it = PortableTys::new_in(Alloc::global());
        let u = ty(&mut it, PNode::Uint(UintTy::U32));
        let fields = list(&mut it, &[u]);
        let impl_out = ty(&mut it, PNode::Tuple(fields));
        let mut out = PortableTys::new_in(Alloc::global());
        match impl_values(trait_out, impl_out, tt.view(), it.view(), &mut out, Alloc::global()) {
            Err(Why::Program(text, _)) => assert!(text.contains("does not have the trait fn's shape at opaque 0 (E0053)"), "{text}"),
            Err(other) => panic!("expected the E0053 refusal, got {other:?}"),
            Ok(rows) => panic!("expected the E0053 refusal, got rows {:?}", &rows[..]),
        }
    }
}
