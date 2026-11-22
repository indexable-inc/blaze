//! The generic arguments a bound's last segment writes in angle brackets (`Tr<A, N, Name = T>`),
//! each one classified by the parameter of the TRAIT it fills, never by how its tokens look.
//!
//! The Reference, "Generic parameters", "Const generics": "A const argument in a path specifies
//! the const value to use for that item."; "The const expression must be a block expression
//! (surrounded with braces) unless it is a single path segment (an IDENTIFIER) or a literal
//! (with a possibly leading - token)."; and of a name that could be either: "When there is
//! ambiguity if a generic argument could be resolved as either a type or const argument, it is
//! always resolved as a type." So `Uint<LO>: Concat<HI>` cannot be read from `HI` alone: the
//! parser files it as a type (a one-name path). What decides is the
//! trait's own head ([`Heads::const_params`]: which of its parameters are const parameters, and
//! of which type): an argument in a position the trait declares const is a constant, made by
//! the one constructor every other const argument takes ([`Builder::const_term`]: a const
//! parameter in scope is `ConstParam`, a literal or a constant item's value is `ConstValue`,
//! and an expression over a parameter, `{ N + 1 }`, is refused by name: "a constant expression
//! over a generic parameter"); an argument in any other position is a type and takes the route
//! of every type in a header.
//!
//! [`angle_args`] is the one walk of the angle form; [`super::preds::segment_args`] calls it
//! for the callers that do not know the trait yet (a trait object, `impl Trait`), where a const
//! argument stays refused under the name it had.

use blaze_lower::{View, form as lf};
use blaze_surface::Range;
use blaze_typeck::{Name, PTy, PortableTys};

use super::preds::{SegArgs, lifetime_args, segment_args};
use super::types::{Bounded, Builder, Env, Heads, Why};

/// What the trait a bound names declares of its const parameters, and how a constant written
/// for one is made.
pub(super) struct Declared<'f, 't> {
    /// The trait's const parameters: each one's index among its parameters (`Self` is 0) and
    /// its type, as [`Heads::const_params`] gives them.
    pub consts: &'f [(u32, PTy)],
    /// The constant written as `text` at the site given, for a const parameter of the type
    /// given.
    pub make: &'f mut dyn FnMut(&mut PortableTys<'t>, &str, Range, PTy) -> Result<PTy, Why>,
}

/// The type of the const parameter that the written argument `index` (counted after `Self`,
/// lifetimes left out) fills, if the trait declares that parameter const.
fn const_at(consts: &[(u32, PTy)], index: u32) -> Option<PTy> {
    consts.iter().find(|(i, _)| *i == index + 1).map(|&(_, t)| t)
}

/// The angle form of a bound's last segment `seg` (`r`: the bound's path): each positional
/// argument in writing order (a lifetime is erased), each `Name = T`, and each `Name: Bounds`
/// (its rows of [`lf::Bound`], made by the caller). `make` makes a written type. With
/// `declared`, an argument in a position the trait declares const is made as a constant from
/// its text, and a constant written where the trait declares a type is the program's error;
/// without it (the trait is not known to the caller) a const argument is refused by name.
pub(super) fn angle_args<'t>(
    tys: &mut PortableTys<'t>,
    v: &View<'_>,
    seg: &lf::Segment,
    r: Range,
    mut declared: Option<Declared<'_, 't>>,
    make: &mut dyn FnMut(&mut PortableTys<'t>, lf::TypeId) -> Result<PTy, Why>,
) -> Result<SegArgs, Why> {
    let (mut given, mut binds, mut nested) = <SegArgs as Default>::default();
    // The argument's position among the trait's parameters after `Self`.
    let mut index = 0u32;
    for &n in v.rows(seg.args)? {
        match lf::GenericArg::decode(n)? {
            lf::GenericArg::Type(t) => {
                let made = match declared.as_mut() {
                    Some(d) => match const_at(d.consts, index) {
                        // `Tr<N>`, `Tr<CAP>`: written like a type, a constant by the trait's
                        // head.
                        Some(want) => {
                            let at = v.type_range(t)?;
                            (d.make)(tys, v.text_at(at)?, v.site(at)?, want)?
                        }
                        None => make(tys, t)?,
                    },
                    None => make(tys, t)?,
                };
                given.push(made);
                index += 1;
            }
            lf::GenericArg::Const(c) => {
                let Some(d) = declared.as_mut() else {
                    return Err(Why::Unsupported("a const generic argument in a bound", r));
                };
                let at = v.const_range(c)?;
                let site = v.site(at)?;
                let Some(want) = const_at(d.consts, index) else {
                    return Err(Why::Program("a constant is written as a bound's argument where the trait declares a type parameter".into(), site));
                };
                given.push((d.make)(tys, v.text_at(at)?, site, want)?);
                index += 1;
            }
            lf::GenericArg::Lifetime(_) => {}
            // A lifetime argument of a generic associated type is erased like every lifetime
            // (`Pattern<Searcher<'a>: Clone>`, core/src/str/iter.rs:612), as `P::Searcher<'a>`
            // is in a type.
            lf::GenericArg::AssocType { name, args, ty } if lifetime_args(v, args)? => binds.push((Name(v.name(name)?), make(tys, ty)?)),
            lf::GenericArg::AssocBound { name, args, bounds } if lifetime_args(v, args)? => nested.push((Name(v.name(name)?), bounds)),
            // A binding of a generic associated type with type arguments (hybrid_array's `U:
            // ArraySize<ArrayType<T> = [T; N]>`) is a projection whose list carries those
            // arguments after the trait's: made by `Builder::gat_bindings` (gat.rs), which
            // reads the segment again, for the one caller that knows the trait and makes
            // clauses (`Builder::one_bound`, through `bound_segment_args`, which refuses the
            // form on a trait alias). Not in `binds`: that list has no place for them.
            lf::GenericArg::AssocType { .. } if declared.is_some() => {}
            // An associated type bound on a generic associated type with type or const
            // arguments (`U: ArraySize<ArrayType<T>: Copy>`): made by `Builder::gat_bounds`
            // (gat_bounds.rs), which reads the segment again, for the one caller that knows the
            // trait (`Builder::one_bound`; `bound_segment_args` refuses it on a trait alias).
            lf::GenericArg::AssocBound { .. } if declared.is_some() => {}
            // A caller that does not know the trait (a trait object, `impl Trait`): refused
            // by name.
            lf::GenericArg::AssocType { .. } => return Err(Why::Unsupported("a binding of a generic associated type with type or const arguments (`Name<T> = U`)", r)),
            lf::GenericArg::AssocBound { .. } => return Err(Why::Unsupported("an associated type bound with type or const arguments (`Item<T>: Bound`)", r)),
            lf::GenericArg::AssocConst { .. } => return Err(Why::Unsupported("an associated const binding in a bound (`Tr<N = 3>`)", r)),
        }
    }
    Ok((given, binds, nested))
}

// `'s` is the names source's.
impl<'b, 's, H: Heads, S: super::source::NamesSource<'s>> Builder<'b, H, S> {
    /// The arguments the last segment `seg` of a bound's path writes, the bound naming
    /// `target` (a trait or a trait alias: both file their const parameters with their head):
    /// [`segment_args`] for the forms with no const argument (`Fn(A) -> B`, no arguments), else
    /// [`angle_args`] with the target's const parameters, each constant made by
    /// [`Builder::const_term`].
    pub(super) fn bound_segment_args(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, target: Bounded, seg: &lf::Segment, r: Range) -> Result<SegArgs, Why> {
        if seg.return_notation || seg.parenthesized || !seg.has_args {
            return segment_args(tys, v, seg, r, &mut |tys, t| self.ty(tys, env, v, t));
        }
        let key = match target {
            Bounded::Trait(key) => key,
            // A trait alias takes no binding (`one_bound` refuses `Alias<Name = T>`), and
            // one with type arguments is never handed back by `angle_args`: refused here
            // under the same name.
            Bounded::Alias(key) => {
                for &n in v.rows(seg.args)? {
                    match lf::GenericArg::decode(n)? {
                        lf::GenericArg::AssocType { args, .. } if !lifetime_args(v, args)? => {
                            return Err(Why::Unsupported("an associated type binding on a trait alias (`Alias<Name = T>`)", r));
                        }
                        lf::GenericArg::AssocBound { args, .. } if !lifetime_args(v, args)? => {
                            return Err(Why::Unsupported("an associated type bound on a trait alias (`Alias<Name<T>: Bound>`)", r));
                        }
                        _ => {}
                    }
                }
                key
            }
        };
        let consts = self.heads.const_params(key).ok_or(Why::Need(key))?;
        let declared = Declared { consts, make: &mut |tys, text, at, want| self.const_term(tys, env, text, at, &|_| Ok(want)) };
        angle_args(tys, v, seg, r, Some(declared), &mut |tys, t| self.ty(tys, env, v, t))
    }
}
