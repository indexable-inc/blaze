//! A type-relative path (`Self::X` in a trait's own header, `T::X` in the bounds of `T`) read
//! through the bounds AS WRITTEN in the header being lowered, never through that header's own
//! finished predicates or supertraits.
//!
//! The Reference, "Paths", "Paths in types" and "Associated Items", "Associated Types": `T::X`
//! is `<T as Tr>::X` for the one trait `Tr` among the bounds of `T`, supertraits included, that
//! declares `X`; two that declare it are the ambiguity error (E0221). While a header's bounds
//! are themselves being lowered, the finished list does not exist: `trait Call: LoadState<Error:
//! From<Self::Evm>> + Spawn` asks for `Self::Evm` inside its own supertrait list (reth's
//! rpc-eth-api helpers, the cause of "these items need each other's heads in a cycle" on
//! tempo's census: the trait's members asked for the trait's own supertraits). The language
//! reads only the bounds that can declare the name: so does this.
//!
//! Three steps, each a work list:
//! 1. the bounds written on the subject in the header's text (a trait's supertrait list and its
//!    `where Self: ..`; a parameter's bounds in the generics list and its `where T: ..`);
//! 2. by NAME only (no argument is lowered): which of them is, or has among its supertraits, a
//!    trait declaring `X` (the bound traits' heads and filed supertraits; a head not made yet is
//!    [`Why::Need`], the store's protocol);
//! 3. only those bounds' type arguments are lowered, and the declaring trait's arguments come
//!    from [`Builder::assoc_owner`].
//!
//! A bound that does not declare the name is never lowered here, so `Tr<Self::X>` beside the
//! bound that declares `X` reads. A bound whose own arguments must be read to find the trait of
//! a path written in them (`trait A: B<Self::X>` with `X` declared by `B`) is a cycle in the
//! program (rustc's E0391): step 3 would re-enter itself, which [`Heads::written_depth`]
//! counts and [`MOST_NESTED`] bounds with a typed refusal.

use std::{alloc::Global, vec::Vec};

use blaze_id::ItemKey as LibKey;
use blaze_lower::{View, form as lf, head};
use blaze_surface::Range;
use blaze_typeck::{List, Name, PNode, PTy, PortableTys};

use super::preds::segment_args;
use super::types::{Bounded, Builder, Env, Heads, Why, intern, path_texts, path_texts_of};

/// The most lowerings of one written bound's arguments nested in each other (rule 18): a
/// header has few bounds, and each nesting is one bound read to find a path written in another.
const MOST_NESTED: u32 = 32;

/// The most traits one supertrait walk visits (as `Builder::assoc_owner`'s).
const MOST_SUPERS: usize = 256;

/// Whose written bounds are read.
#[derive(Clone, Copy)]
enum Subject {
    /// `Self` of the trait whose header `v` is: its supertrait list and `where Self: ..`.
    TraitSelf,
    /// The type parameter of this name: its bounds in the generics list and `where T: ..`.
    Param(Name),
}

/// The trait bounds written on `subject` in `v`'s header, as the types that name them, in
/// writing order; `?Trait` and `!Trait` declare nothing of the subject and are left out.
fn written_bounds(v: &View<'_>, subject: Subject, out: &mut Vec<lf::TypeId, Global>) -> Result<(), Why> {
    let mut lists: Vec<lf::List, Global> = Vec::new_in(Global);
    match subject {
        Subject::TraitSelf => lists.push(v.list(head::BOUNDS)),
        Subject::Param(want) => {
            for &row in v.rows(v.list(head::GENERICS))? {
                if let lf::GenericParam::Type { name, bounds, .. } = lf::GenericParam::decode(row)? {
                    if Name(v.name(name)?) == want {
                        lists.push(bounds);
                    }
                }
            }
        }
    }
    for &row in v.rows(v.list(head::WHERE))? {
        let lf::WherePred::Type { ty, bounds, .. } = lf::WherePred::decode(row)? else { continue };
        let lf::TypeRef::Path(p) = v.ty(ty)? else { continue };
        let (abs, texts, _, _) = path_texts(v, p)?;
        let (false, [t]) = (abs, texts.as_slice()) else { continue };
        let on = match subject {
            Subject::TraitSelf => *t == "Self",
            Subject::Param(want) => Name::of(t) == want,
        };
        if on {
            lists.push(bounds);
        }
    }
    for l in lists {
        for &row in v.rows(l)? {
            let lf::Bound::Trait { ty, mods, .. } = lf::Bound::decode(row)? else { continue };
            if mods.maybe || mods.negative {
                continue;
            }
            out.push(ty);
        }
    }
    Ok(())
}

impl<'b, 'n, H: Heads, S: super::source::NamesSource<'n>> Builder<'b, H, S> {
    /// Whether `tr` or one of its supertraits (through theirs, a work list) declares an
    /// associated type `name`: by name only, from the heads; no argument is read.
    fn declares(&self, tr: LibKey, name: Name, r: Range) -> Result<bool, Why> {
        let mut todo: Vec<LibKey, Global> = Vec::new_in(Global);
        todo.push(tr);
        let mut seen: Vec<LibKey, Global> = Vec::new_in(Global);
        for _ in 0..MOST_SUPERS {
            let Some(t) = todo.pop() else { return Ok(false) };
            if seen.contains(&t) {
                continue;
            }
            seen.push(t);
            if self.heads.trait_has_assoc(t, name).ok_or(Why::Need(t))? {
                return Ok(true);
            }
            for &(st, _) in self.heads.trait_supers(t).ok_or(Why::Need(t))? {
                todo.push(st);
            }
        }
        if todo.is_empty() {
            return Ok(false);
        }
        Err(Why::Unsupported("a supertrait tree past 256 traits", r))
    }

    /// The type arguments after `Self` that the last segment `seg` of a bound's path writes,
    /// lowered: its `Name = T` bindings and `Name: Bounds` are not read (they are
    /// `Builder::predicates`'s, and reading them here would ask for more than the language
    /// does to find a name's trait). `Fn(A) -> B` gives the one tuple argument, as
    /// [`segment_args`] reads it.
    fn positional(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, seg: &lf::Segment, r: Range) -> Result<Vec<PTy, Global>, Why> {
        if seg.parenthesized || seg.return_notation {
            return Ok(segment_args(tys, v, seg, r, &mut |tys, t| self.ty(tys, env, v, t))?.0);
        }
        let mut given = Vec::new_in(Global);
        if !seg.has_args {
            return Ok(given);
        }
        for &row in v.rows(seg.args)? {
            match lf::GenericArg::decode(row)? {
                lf::GenericArg::Type(t) => given.push(self.ty(tys, env, v, t)?),
                // A lifetime is erased; a binding (`Name = T`, `N = 3`, `Name: Bounds`) is no
                // positional argument.
                lf::GenericArg::Lifetime(_) | lf::GenericArg::AssocType { .. } | lf::GenericArg::AssocConst { .. } | lf::GenericArg::AssocBound { .. } => {}
                lf::GenericArg::Const(_) => return Err(Why::Unsupported("a const generic argument in a bound", r)),
            }
        }
        Ok(given)
    }

    /// `subject::name` (`me` is the subject's type) through the bounds written on it in `v`:
    /// `<me as Tr<..>>::name` for the one trait among them and their supertraits that declares
    /// `name`; `None` when no written bound declares it; two are the language's ambiguity.
    #[expect(clippy::too_many_arguments, reason = "the path, where it is written, and whose bounds are read")]
    fn written_assoc(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, subject: Subject, me: PTy, name: Name, own: &[PTy], r: Range, shown: &str) -> Result<Option<PTy>, Why> {
        let mut bounds = Vec::new_in(Global);
        written_bounds(v, subject, &mut bounds)?;
        let mut found: Vec<(LibKey, List), Global> = Vec::new_in(Global);
        for &ty in &bounds {
            let lf::TypeRef::Path(p) = v.ty(ty)? else { continue };
            let (abs, texts, segs, pr) = path_texts_of(v, p, true)?;
            let key = match self.bound_target(env, v, p, abs, &texts, pr)? {
                Bounded::Trait(key) => key,
                // A trait alias stands for bounds whose arguments are over the alias's own
                // parameters: one that declares the name is refused by name, never guessed.
                Bounded::Alias(alias) => {
                    let head = self.heads.trait_alias(alias).ok_or(Why::Need(alias))?;
                    for &(t, _) in &head.traits {
                        if self.declares(t, name, r)? {
                            return Err(Why::Unsupported("a type-relative path (`T::X`, `Self::X`) declared through a trait alias bound, read while that header's bounds are lowered", r));
                        }
                    }
                    continue;
                }
            };
            if !self.declares(key, name, r)? {
                continue;
            }
            let Some(depth) = self.heads.written_depth() else {
                return Err(Why::Unsupported("a type-relative path read through written bounds, by heads that count no nesting", r));
            };
            let d = depth.get();
            if d >= MOST_NESTED {
                return Err(Why::Program("this type-relative path is written in a bound that must itself be read to find the path's trait: the header's bounds need each other in a cycle".into(), r));
            }
            // Set back on every way out: a `Why::Need` passes up and the item is lowered again.
            depth.set(d + 1);
            let given = match segs.last() {
                Some(last) => self.positional(tys, env, v, last, pr),
                None => Ok(Vec::new_in(Global)),
            };
            depth.set(d);
            let (key, args) = self.trait_args(tys, key, pr, me, &given?)?;
            if let Some(owner) = self.assoc_owner(tys, key, args, name, r, shown)? {
                if !found.contains(&owner) {
                    found.push(owner);
                }
            }
        }
        match found.as_slice() {
            [] => Ok(None),
            &[(t, a)] => self.assoc_with_args(tys, t, name, a, own, r, shown).map(Some),
            many => {
                let mut traits = Vec::with_capacity_in(many.len(), Global);
                traits.extend(many.iter().map(|&(t, _)| t));
                Err(Why::AssocAmbiguous(shown.into(), traits, r))
            }
        }
    }

    /// `Self::name` in the header `v` of a trait whose supertraits are not filed yet (its own
    /// bounds are being lowered) and that does not declare `name` itself: through its
    /// supertrait list and `where Self: ..` as written.
    #[expect(clippy::too_many_arguments, reason = "the associated item's arguments and the site of Self's written bounds")]
    pub(super) fn written_self_assoc(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, name: Name, own: &[PTy], r: Range, shown: &str) -> Result<PTy, Why> {
        let me = intern(tys, PNode::Param(0))?;
        match self.written_assoc(tys, env, v, Subject::TraitSelf, me, name, own, r, shown)? {
            Some(t) => Ok(t),
            None => Err(Why::AssocNotFound(shown.into(), r)),
        }
    }

    /// `T::name` with `T` the parameter `param`, while the bounds of `v`'s header are being
    /// lowered: through the bounds written on `T` in `v`. `None` when none of them declares
    /// `name` (the parameter may be an outer item's, whose bounds `env` holds).
    #[expect(clippy::too_many_arguments, reason = "the path, where it is written, and the parameter")]
    pub(super) fn written_param_assoc(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, param: u32, name: Name, own: &[PTy], r: Range, shown: &str) -> Result<Option<PTy>, Why> {
        let Some(&(pname, _)) = env.params.iter().find(|(_, i)| *i == param) else { return Ok(None) };
        let me = intern(tys, PNode::Param(param))?;
        self.written_assoc(tys, env, v, Subject::Param(pname), me, name, own, r, shown)
    }
}
