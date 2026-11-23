//! `Clone::clone` at a `Self` whose `Clone` the LANGUAGE gives: a tuple, a closure, a fn item
//! or a fn pointer (the Reference, "Special types and traits", `Clone`). No impl's text holds
//! it, so instance resolution (`FrontEnd::resolve`, written impls only) has no answer; the
//! builder makes it ([`blaze_build::build_builtin_clone`], clone_fn.rs).
//!
//! Tabled (run.rs's instance loop indexes every instance by a tabled fn): at the trait's own
//! `clone` row, which has no body, so one instance per `Self` serves every caller.
//!
//! `Self: Copy` decides what the function does, so it is asked of the checker
//! (`FrontEnd::holds`, the solver's builtin rule), never re-derived here, and it is part of the
//! instance's KEY (rule 21: `#[derive(Copy)]` added to a field's type changes the code). It is
//! asked where the call is met, in the CALLER's world (a bodiless row has none: the old front
//! panics, the lazy one refuses), and kept per `Self` for the build ([`Clones::note`]): the
//! answer is the same in every world, since impls are global (coherence). The calls the built
//! function makes (each field's `clone`) are resolved in that same caller's world
//! ([`Clones::caller_of`]), for the same reason. A self of any other type is no builtin clone:
//! it stays the typed refusal it is.
//!
//! A closure's `Copy` is asked capture by capture (the Reference: a closure is `Copy` "if it
//! captures values only by immutable reference or by values that are `Copy`"), each capture's
//! type its environment's field: the solver's closure type has no upvars yet (blaze-typeck
//! infer/facts.rs, "The upvars: `()`", the wiring's known limit), so its answer for the closure
//! as a whole would call every closure `Copy`.

use core::cell::RefCell;
use std::alloc::Global;
use std::vec::Vec;

use blaze_build::layout::LayoutFacts;
use blaze_build::{BuildError, Instance};
use blaze_own::Alloc;
use blaze_typeck::{ItemKey, LangItem, List, PNode, PTy, PortableTys, ResolveError};

use crate::seam::{FrontEnd, LangRefusal};

/// The record walk's builtin clone of one `Self` ([`language_clone`]): the trait's `clone` (the
/// row it is tabled at) and what its function does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LanguageClone {
    /// The lang trait's `clone`.
    pub clone: ItemKey,
    /// `true`: `Self`'s bytes copied (a fn item or fn pointer, `Copy` by its type alone).
    /// `false`: each field through its own `Clone::clone`, in order (a tuple's, a closure's
    /// captures), clone_fn.rs `clone_fields`.
    pub bytes: bool,
}

/// THE RECORD WALK'S BUILTIN CLONE (run/make_one.rs): `called` as `Clone::clone` at a `Self`
/// whose `Clone` the LANGUAGE gives (the Reference, "Special types and traits", `Clone`:
/// "function item types, function pointer types, closures that capture no values or only
/// values that implement `Clone`, tuples of types that implement `Clone`"; core's clone.rs `mod
/// impls`: no impl item). Tabled at the trait's `clone` row (made from its head: it has no
/// body), keyed by its name `clone::<Self>` alone, so what the function does is a function of
/// `Self`'s TYPE alone (rule 21):
/// - a fn item or fn pointer: `Copy` by its type, its clone is its bytes (rustc_mir_transform
///   shim.rs: `ty::FnDef(..) | ty::FnPtr(..) => builder.copy_shim()`);
/// - a tuple or a closure: FIELD-WISE, each field's own `Clone::clone` instance called in order
///   (shim.rs `tuple_like_shim`), whatever its fields' `Copy`; each field's call is an instance
///   of its own, selected by its own name. rustc copies the bytes instead when `Self: Copy`
///   (shim.rs `build_clone_shim`: `_ if is_copy => builder.copy_shim()`): the two differ only
///   for a field of a `Copy` type whose hand-written `clone` is not `*self` (core's clone.rs: a
///   `Copy` type's `Clone` "is equivalent to `*self`", a contract clippy's
///   `expl_impl_clone_on_copy` checks), and keying the copy by `Self: Copy` would need that
///   answer in the instance's name (run.rs `build_from` does: [`Clones::key_args`]).
///
/// `None` for any other call.
pub(crate) fn language_clone<P: FrontEnd>(program: &P, called: Instance, ground: &PortableTys<'_>) -> Option<LanguageClone> {
    let clone = program.lang_trait_fn(LangItem::Clone, "clone")?;
    if called.item != clone {
        return None;
    }
    let view = ground.view();
    let &[me] = view.list(called.args).ok()? else { return None };
    match view.node(me).ok()? {
        PNode::FnDef(..) | PNode::FnPtr(..) => Some(LanguageClone { clone, bytes: true }),
        PNode::Tuple(_) | PNode::Record(..) | PNode::Union(_) | PNode::Closure { .. } => Some(LanguageClone { clone, bytes: false }),
        _ => None,
    }
}

/// Builtin `From::from` for structural error unions (`Self = A | B`, `T = A` or strict sub-union).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LanguageFrom {
    /// The lang trait's `from`.
    pub from: ItemKey,
}

/// Recognizes a call to `From::from` where `Self` is a structural union `Dst` and `T` is a strict
/// member or strict sub-union of `Dst` (`Src != Dst`).
pub(crate) fn language_from<P: FrontEnd>(program: &P, called: Instance, ground: &PortableTys<'_>) -> Option<LanguageFrom> {
    let from = program.lang_trait_fn(LangItem::From, "from")?;
    if called.item != from {
        return None;
    }
    let view = ground.view();
    let &[dst, src] = view.list(called.args).ok()? else { return None };
    if dst == src {
        return None;
    }
    let PNode::Union(dst_l) = view.node(dst).ok()? else { return None };
    let dst_members = view.list(dst_l).ok()?;
    let ok = match view.node(src).ok()? {
        PNode::Union(src_l) => {
            let src_members = view.list(src_l).ok()?;
            !src_members.is_empty()
                && src_members.len() < dst_members.len()
                && src_members.iter().all(|sm| dst_members.iter().any(|dm| dm == sm))
        }
        _ => dst_members.iter().any(|&dm| dm == src),
    };
    ok.then_some(LanguageFrom { from })
}

/// Why a builtin clone's `Self: Copy` has no answer: the solver's (a record it could not make),
/// or a closure's environment that could not be grounded at its arguments.
#[derive(Debug)]
pub(crate) enum NoteError {
    Resolve(ResolveError),
    Build(BuildError),
}

/// A `Clone::clone` instance whose `Self` is a type the language gives `Clone`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CloneShim {
    /// The trait's `clone`.
    pub clone: ItemKey,
    /// `Self`.
    pub me: PTy,
}

/// The build's builtin clones: `Clone::clone` and `Copy` as the program declares them (`None`:
/// a `no_core` program without them, which calls no builtin clone or has nothing `Copy`), and
/// each `Self` met with its `Copy` answer.
pub(crate) struct Clones {
    clone: Option<ItemKey>,
    copy: Option<ItemKey>,
    /// Ground `Self`, whether it is `Copy`, and the tabled fn whose world asked it (the one
    /// the built function's own calls resolve in), in the order met; a handful a build.
    copies: RefCell<Vec<(PTy, bool, usize), Global>>,
}

impl Clones {
    /// Asks the lang items once per build; a lookup the front end refuses stops it by name.
    pub(crate) fn of<P: FrontEnd>(program: &P) -> Result<Clones, LangRefusal> {
        Ok(Clones {
            clone: program.lang_trait_fn(LangItem::Clone, "clone"),
            copy: program.lang_item(LangItem::Copy)?,
            copies: RefCell::new(Vec::new_in(Global)),
        })
    }

    /// `called` as a builtin `clone` (module doc); `None` for any other call.
    pub(crate) fn shim_of(&self, called: Instance, ground: &PortableTys<'_>) -> Option<CloneShim> {
        let clone = self.clone?;
        if called.item != clone {
            return None;
        }
        let view = ground.view();
        let &[me] = view.list(called.args).ok()? else { return None };
        match view.node(me).ok()? {
            PNode::Tuple(_) | PNode::Record(..) | PNode::Union(_) | PNode::Closure { .. } | PNode::FnDef(..) | PNode::FnPtr(..) => Some(CloneShim { clone, me }),
            _ => None,
        }
    }

    /// The tabled index of `called` as a builtin clone: the trait's `clone` row (`None` when the
    /// program tables no such fn: the call stays refused by name). `Self: Copy` is asked first,
    /// in fn `caller`'s world, and kept ([`Clones::copy_of`]).
    pub(crate) fn owner<P: FrontEnd>(
        &self,
        program: &P,
        facts: &dyn LayoutFacts,
        caller: usize,
        called: Instance,
        ground: &mut PortableTys<'_>,
        alloc: Alloc<'_>,
    ) -> Option<Result<usize, NoteError>> {
        let shim = self.shim_of(called, ground)?;
        let c = program.fn_index(shim.clone)?;
        Some(self.note(program, facts, caller, shim, ground, alloc).map(|()| c))
    }

    /// Asks `shim`'s `Self: Copy` in fn `caller`'s world, once per `Self`. A fn item and a fn
    /// pointer are `Copy` (the Reference, "Special types and traits"); the solver says so where
    /// the program declares `Copy`, and with no `Copy` declared they have no field to clone, so
    /// their clone is their bytes either way. A closure: each capture (module doc).
    fn note<P: FrontEnd>(
        &self,
        program: &P,
        facts: &dyn LayoutFacts,
        caller: usize,
        shim: CloneShim,
        ground: &mut PortableTys<'_>,
        alloc: Alloc<'_>,
    ) -> Result<(), NoteError> {
        if self.copies.borrow().iter().any(|&(t, _, _)| t == shim.me) {
            return Ok(());
        }
        let node = ground.view().node(shim.me).map_err(|e| NoteError::Resolve(ResolveError::Check(e)))?;
        let copy = match (node, self.copy) {
            (PNode::FnDef(..) | PNode::FnPtr(..), _) => true,
            (_, None) => false,
            (PNode::Closure { owner, ordinal, args }, Some(copy)) => {
                let c = facts.closure(owner, ordinal).ok_or(NoteError::Build(BuildError::UnsupportedSig {
                    what: "a closure whose facts the world did not answer (`LayoutFacts::closure`), met as a builtin `clone`'s `Self`",
                }))?;
                let past = |_| NoteError::Build(BuildError::Malformed { what: "a list past the ground table" });
                let a = blaze_own::Vec::from_slice_in(ground.view().list(args).map_err(past)?, alloc);
                let env = blaze_build::ground(c.types, c.env, a.as_slice(), ground, alloc, facts).map_err(NoteError::Build)?;
                let PNode::Tuple(l) = ground.view().node(env).map_err(past)? else {
                    return Err(NoteError::Build(BuildError::Malformed { what: "a closure's environment that is not a tuple" }));
                };
                let fields = blaze_own::Vec::from_slice_in(ground.view().list(l).map_err(past)?, alloc);
                let mut all = true;
                for &f in fields.as_slice() {
                    if !program.holds(caller, copy, f, ground).map_err(NoteError::Resolve)? {
                        all = false;
                        break;
                    }
                }
                all
            }
            (_, Some(copy)) => program.holds(caller, copy, shim.me, ground).map_err(NoteError::Resolve)?,
        };
        self.copies.borrow_mut().push((shim.me, copy, caller));
        Ok(())
    }

    /// `shim`'s `Self: Copy` as [`Clones::note`] asked it; `None` when it was not asked.
    pub(crate) fn copy_of(&self, shim: CloneShim) -> Option<bool> {
        self.copies.borrow().iter().find(|&&(t, _, _)| t == shim.me).map(|&(_, c, _)| c)
    }

    /// The tabled fn whose world asked `shim`'s `Self: Copy`: the world the built function's
    /// own calls resolve in (module doc); `None` when it was not asked.
    pub(crate) fn caller_of(&self, shim: CloneShim) -> Option<usize> {
        self.copies.borrow().iter().find(|&&(t, _, _)| t == shim.me).map(|&(_, _, c)| c)
    }

    /// The list a builtin clone's instance is keyed by: `[Self]`, then for a `Copy` self the
    /// `Copy` trait's item as a `FnDef` (a key part no type list of `clone` has), so the two
    /// functions a `Self` can have never share a key.
    pub(crate) fn key_args(&self, shim: CloneShim, ground: &mut PortableTys<'_>) -> Result<List, BuildError> {
        let full = |_| BuildError::Malformed { what: "the ground table is full" };
        let copy = self.copy_of(shim).ok_or(BuildError::Malformed { what: "a builtin `clone` keyed before its `Self: Copy` was asked" })?;
        match (copy, self.copy) {
            (true, Some(trait_)) => {
                let mark = ground.intern(PNode::FnDef(trait_, List::EMPTY)).map_err(full)?;
                ground.intern_list(&[shim.me, mark]).map_err(full)
            }
            _ => ground.intern_list(&[shim.me]).map_err(full),
        }
    }
}
