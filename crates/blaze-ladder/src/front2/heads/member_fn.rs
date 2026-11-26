//! An impl's member fn's signature as its own want ([`Want::MemberFn`]): work at item grain,
//! one head a task. An impl's head ([`Store::impl_`](super::Store)) lists its fns by name and
//! key, with whether each takes `self` read from its text ([`takes_self`]), and keeps the frame
//! its members are made in ([`MemberFrame`]); it makes no fn's signature. A fn's signature is
//! made when that fn is used: the checker's `FnSig` of it ([`super::super::fill::want_of_key`]),
//! a body lowered for it (body.rs `lower_member`), a census asking for it.
//!
//! Why (census 13, brief 14): crypto_bigint writes `const BYTES` in one `impl Limb` and `fn
//! from_be_bytes(bytes: [u8; Self::BYTES])` in another. When the impl's head made every member
//! fn's signature, finding `Limb::LOG2_BITS` by name (in `nlimbs(64)`, a type alias's length)
//! made the second impl, whose fn's signature needed `Self::BYTES`, whose evaluation asked
//! `Limb`'s inherent items again: a cycle by construction (`ConstFail::Nested`), and the
//! aliases `U64`, `U128`, `U256` were refused across tempo. The Reference, "Implementations":
//! "A type can also have multiple inherent implementations", and "Associated Items": "The path
//! to an associated item is any path to the implementing type, followed by the associated
//! item's identifier as the final path component": finding a member by name reads the impls'
//! names, never their members' signatures. A signature that really needs itself (a length
//! whose constant names that fn) still meets its own want inside its evaluation and is
//! refused by name: the Reference, "Constant evaluation", "Paths to functions and constants.
//! Recursively defining constants is not allowed."

use blaze_id::ItemKey as LibKey;
use blaze_lower::{View, form as lf, head};
use blaze_surface::{ItemDetail, Range};
use blaze_typeck::{ItemKey, Name, PClause, PTy};

use super::super::keys::{Keys, MemberKind, Origin, checker_of};
use super::super::source::NamesSource;
use super::super::types::{Bounds, Env, ParamBound, SelfTy, Why};
// The parent's own imports of the allocator and `format!` (no new line naming `std`:
// tools/ratchets/std-alloc.txt).
use super::{Alloc, Store, Want, format, lower_alloc, members};

/// What an impl's member fns are made in, kept when the impl's head is made (its module, its
/// generic parameters, their bounds, its `Self`): a member fn's want reads it, never lowers
/// the impl's header again. Its associated types and clauses are the impl's own records
/// (`ImplHead::assoc`, `Store::preds`).
pub struct MemberFrame {
    pub module: LibKey,
    pub generics: u32,
    pub is_const: bool,
    pub params: blaze_own::Vec<(Name, u32), Alloc<'static>>,
    pub consts: blaze_own::Vec<(u32, PTy), Alloc<'static>>,
    pub bounds: blaze_own::Vec<ParamBound, Alloc<'static>>,
    pub self_: SelfTy,
}

/// Whether the fn whose lowered text is `v` takes `self` (its first parameter is `self`,
/// `&self`, `&mut self` or `self: T`): read from the syntax alone, so answering a name needs
/// no signature. The same test as `Store::fn_head`'s `self_param`.
pub(super) fn takes_self(v: &View<'_>) -> Result<bool, Why> {
    let Some(&first) = v.rows(v.list(head::PARAMS))?.first() else { return Ok(false) };
    Ok(lf::Param::decode(first)?.self_kind != lf::SelfKind::No)
}

/// The want that makes `key`'s signature when `key` is a fn of an impl ([`Want::MemberFn`]);
/// `None` for any other key (a trait's fn is made with its trait's members, a free fn by
/// `Want::Fn`).
pub fn member_fn_want<'n, S: NamesSource<'n>>(names: S, keys: &Keys, key: ItemKey) -> Option<Want> {
    let Some(Origin::Member { container, kind: MemberKind::Fn, .. }) = keys.origin(key) else { return None };
    let lib = keys.library_in(names, *container)?;
    match names.detail_of(lib) {
        Some(ItemDetail::Impl) => Some(Want::MemberFn(lib, key)),
        _ => None,
    }
}

/// The want that makes the head of the member `key` (a fn or a constant of a trait or an impl)
/// with its container's first: an impl's fn its own [`Want::MemberFn`] (its impl's head first,
/// `Store::step`), an impl's constant its impl's head ([`Want::Impl`]: made with it), a
/// trait's fn or constant its trait's members ([`Want::Members`]: its head first). `None`: the
/// key is no such member front2 filed (its container's head was never made, so no key was
/// derived for it), or its container is no trait or impl.
pub fn member_head_want<'n, S: NamesSource<'n>>(names: S, keys: &Keys, key: ItemKey) -> Option<Want> {
    let Some(Origin::Member { container, kind, .. }) = keys.origin(key) else { return None };
    let lib = keys.library_in(names, *container)?;
    match (names.detail_of(lib)?, kind) {
        (ItemDetail::Impl, MemberKind::Fn) => Some(Want::MemberFn(lib, key)),
        (ItemDetail::Impl, MemberKind::Const) => Some(Want::Impl(lib)),
        (ItemDetail::Trait, MemberKind::Fn | MemberKind::Const) => Some(Want::Members(lib)),
        _ => None,
    }
}

impl<'n> Store<'n> {
    /// Makes the signature of `fn_`, a fn of impl `impl_` whose head is made: in the impl's
    /// kept frame, with its associated types and its clauses, as `Store::member_fn_head` (a
    /// refusal other than a need is the fn's own gap in `fn_gaps`, never the impl's).
    pub(super) fn member_fn<S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, impl_: LibKey, fn_: ItemKey) -> Result<(), Why> {
        let whole = Range::new(0, 0);
        let ck = checker_of(impl_);
        let name: &str = match keys.origin(fn_) {
            Some(Origin::Member { container, kind: MemberKind::Fn, name }) if *container == ck => name,
            other => return Err(Why::Program(format!("member fn {fn_:?} is filed as {other:?}, not as a fn of impl {ck:?}").into_boxed_str(), whole)),
        };
        let alloc = self.tys.alloc();
        let ms = members(names, impl_, alloc).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))?;
        for m in &ms {
            if !matches!(m.detail, ItemDetail::Fn { .. }) || m.name != name {
                continue;
            }
            let low = lower_alloc(names, impl_, m.text, alloc).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))?;
            let v = low.view();
            if v.cfg_out() {
                continue;
            }
            let frame = self.member_frames.get(&ck).ok_or_else(|| Why::Program(format!("impl {ck:?}: its head is made but its members' frame is not kept").into_boxed_str(), whole))?;
            let (module, generics, is_const, self_) = (frame.module, frame.generics, frame.is_const, frame.self_);
            let mut params = blaze_own::Vec::with_capacity_in(frame.params.len(), alloc);
            params.extend_from_slice(&frame.params);
            let mut consts = blaze_own::Vec::with_capacity_in(frame.consts.len(), alloc);
            consts.extend_from_slice(&frame.consts);
            let mut bounds = blaze_own::Vec::with_capacity_in(frame.bounds.len(), alloc);
            bounds.extend_from_slice(&frame.bounds);
            let head = self.impls.get(&ck).ok_or_else(|| Why::Program(format!("impl {ck:?}: a member fn asked before its head").into_boxed_str(), whole))?;
            let mut self_assoc = blaze_own::Vec::with_capacity_in(head.assoc.len(), alloc);
            self_assoc.extend_from_slice(&head.assoc);
            let clauses = self.preds.get(&ck).ok_or_else(|| Why::Program(format!("impl {ck:?}: its head is made but its clauses are not filed").into_boxed_str(), whole))?;
            let mut inherited: blaze_own::Vec<PClause, Alloc<'static>> = blaze_own::Vec::with_capacity_in(clauses.clauses.len(), alloc);
            inherited.extend_from_slice(&clauses.clauses);
            let env = Env { module, params: &params, bounds: Bounds::Done(&bounds), self_, self_assoc: &self_assoc, anon: &[], consts: &consts, body: None };
            let marks = super::super::item_site::member_marks(names, impl_, m.written_by, m.text).map_err(super::super::item_site::MarksLost::why)?;
            return self.member_fn_head(names, keys, impl_, fn_, Some(ck), &env, generics, &inherited, &v, m, marks, is_const);
        }
        Err(Why::Program(format!("impl {ck:?} holds no fn `{name}` its crate's cfg keeps").into_boxed_str(), whole))
    }
}
