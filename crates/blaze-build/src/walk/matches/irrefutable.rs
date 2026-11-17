//! Irrefutable patterns beyond a binding, `_`, a tuple and an array without `..`: what walk.rs
//! `bind_value` (a `let`, a parameter, a part bound by value) hands here. Each is taken apart
//! from the value's place by the match builder's own projection (matches.rs `peel`,
//! `deref_at`, `parts`, `bind_slot`), never a second copy of it. Rules from the Reference,
//! "Patterns":
//! - "Refutability": "A pattern is said to be irrefutable when it always matches the value it
//!   is being matched against"; `let` without `else` and function parameters take only those.
//!   A struct pattern of a struct and a tuple-struct or struct pattern of an enum's only
//!   variant are irrefutable; a variant of an enum of more than one is refused by name.
//! - "Struct patterns": "Struct patterns match struct, enum, and union values that match all
//!   criteria defined by its subpatterns"; fields left out (with `..`) are not matched.
//! - "Tuple struct patterns" and "Tuple patterns": the subpatterns match the fields by
//!   position; `..` stands for the fields between the prefix and the suffix.
//! - "Reference patterns": "Reference patterns dereference the pointers that are being matched
//!   and, thus, borrow them"; a by-value binding under one copies the part (moving out of a
//!   reference is E0507: refused by name if a part that needs drop reaches here).
//! - "Identifier patterns": `x @ p` "binds the matched value to x" and matches `p` against it.
//! - "Binding modes": "Each time a reference is matched using a non-reference pattern, it will
//!   automatically dereference the value and update the default binding mode. References will
//!   set the default binding mode to `ref`" (matches.rs `peel`).
//! - "Slice patterns": against `[T; N]` the prefix and suffix are at fixed places and the `..`
//!   (a `_`, or `x @ ..`) is the array of the elements between them.
//!
//! Ownership: the value bound is the binder's own (a `let`'s initializer moved in, a
//! parameter's copy). A by-value binding takes its part (walk.rs `own_pat` then owns the
//! binding). A part nothing takes by value (`_`, a `..`, a field a struct pattern leaves out)
//! would never be dropped if it needs drop, so it is refused by name, as `bind_value` refuses
//! `_` over one. An explicit `ref` over the binder's own value is refused by name: it must
//! borrow the initializer's place, and `bind_value` is given a value, not that place.

use core::alloc::Allocator;

use blaze_own::Vec;
use blaze_typeck::{Def, PNode, Pat, PatId};

use super::super::{Shape, V, Walk, malformed};
use super::At;
use crate::BuildError;

impl<A: Allocator + Clone> Walk<'_, '_, '_, A> {
    /// Binds `v`, the binder's own value of pattern `root`'s type, to irrefutable pattern
    /// `root`, by a work list. Every form outside "Refutability"'s irrefutable ones, and every
    /// irrefutable one not built here, is refused by name.
    pub(in crate::walk) fn bind_irrefutable(&mut self, root: PatId, v: V) -> Result<(), BuildError> {
        let t = *self.checked.pat_ty.get(root.ix()).ok_or(malformed("a pattern without a type in the record"))?;
        let g = self.ground_ty(t).map_err(|error| Self::pat_ty_err(root, error))?;
        let repr = self.repr(g).map_err(|error| Self::pat_ty_err(root, error))?;
        let place = match v {
            V::Place(p, off) => (p, off),
            V::Unsized(..) => {
                return Err(BuildError::UnsupportedPat { pat: root, what: "an irrefutable pattern over an unsized value" });
            }
            // A scalar (a reference, an integer) or no value: put in a place of its own, so the
            // projection reads it like any other.
            v => {
                let at = self.temp(repr);
                self.store(v, repr, at)?;
                at
            }
        };
        // (pattern, its part's place, whether the part is the binder's own: not behind a
        // reference the pattern followed).
        let mut todo: Vec<(PatId, At, bool), A> = Vec::new_in(self.alloc.clone());
        let mut parts: Vec<(PatId, At), A> = Vec::new_in(self.alloc.clone());
        todo.push((root, At { at: place, g, by_ref: false, copy: false }, true));
        while let Some((p, at, owned)) = todo.pop() {
            let pat = self.pat(p)?;
            match pat {
                Pat::Wild => {
                    if owned && self.needs_drop(at.g).map_err(|error| Self::pat_ty_err(p, error))? {
                        return Err(BuildError::UnsupportedPat { pat: p, what: "a `_` pattern over a value that needs drop" });
                    }
                }
                Pat::Bind { ann, sub, .. } => {
                    // rules/pat.rs `pat_rule`: an explicit `ref`, else `mut` binds by move, else
                    // the default binding mode.
                    let by_ref = ann.by_ref.is_some() || (!ann.mutable && at.by_ref);
                    let drops = self.needs_drop(at.g).map_err(|error| Self::pat_ty_err(p, error))?;
                    let bound = if by_ref {
                        if owned {
                            return Err(BuildError::UnsupportedPat {
                                pat: p,
                                what: "a `ref` binding over a `let`'s or parameter's own value (it borrows the initializer's place, which the binder is not given)",
                            });
                        }
                        match self.ground_node(at.g).map_err(|error| Self::pat_ty_err(p, error))? {
                            // The wide pointer itself.
                            PNode::Slice(_) | PNode::Str => V::Place(at.at.0, at.at.1),
                            _ => V::Scalar(self.address(at.at)),
                        }
                    } else {
                        if drops && !owned {
                            return Err(BuildError::UnsupportedPat {
                                pat: p,
                                what: "a by-value binding of a part that needs drop behind a reference (a move out of a reference, E0507)",
                            });
                        }
                        if drops && sub.is_some() {
                            return Err(BuildError::UnsupportedPat {
                                pat: p,
                                what: "`x @ p` by value over a value that needs drop (`p` would use what `x` moved, E0382)",
                            });
                        }
                        let r = self.repr(at.g).map_err(|error| Self::pat_ty_err(p, error))?;
                        self.load(r, at.at)
                    };
                    self.bind_slot(p, bound)?;
                    if let Some(s) = sub {
                        todo.push((s, at, owned));
                    }
                }
                Pat::Ref { inner, .. } => {
                    let PNode::Ref(_, to) = self.ground_node(at.g).map_err(|error| Self::pat_ty_err(p, error))? else {
                        return Err(malformed("a `&` pattern over other than a reference"));
                    };
                    let mut a = self.deref_at(p, at, to)?;
                    a.by_ref = false;
                    todo.push((inner, a, false));
                }
                Pat::Tuple { .. } | Pat::TupleStruct { .. } | Pat::Struct { .. } | Pat::Slice { .. } | Pat::Path(_) => {
                    let behind = matches!(self.ground_node(at.g).map_err(|error| Self::pat_ty_err(p, error))?, PNode::Ref(..));
                    let a = self.peel(p, at)?;
                    let owned = owned && !behind;
                    if let Pat::TupleStruct { .. } | Pat::Struct { .. } | Pat::Path(_) = pat {
                        self.only_variant(p)?;
                    }
                    if let Pat::Slice { .. } = pat
                        && let PNode::Slice(_) = self.ground_node(a.g).map_err(|error| Self::pat_ty_err(p, error))?
                    {
                        return Err(BuildError::UnsupportedPat {
                            pat: p,
                            what: "a slice pattern over a slice `[T]` in a `let` or parameter pattern",
                        });
                    }
                    let drops = self.needs_drop(a.g).map_err(|error| Self::pat_ty_err(p, error))?;
                    if let Pat::Path(_) = pat {
                        // A unit struct or an only unit variant: nothing to bind.
                        if owned && drops {
                            return Err(BuildError::UnsupportedPat { pat: p, what: "a unit pattern over a value that needs drop" });
                        }
                        continue;
                    }
                    if owned && drops && self.left_out_drops(p, a)? {
                        return Err(BuildError::UnsupportedPat {
                            pat: p,
                            what: "a `..` or a left-out field over a part that needs drop, in a `let` or parameter pattern",
                        });
                    }
                    parts.clear();
                    self.parts(p, a, &mut parts)?;
                    for &(s, sa) in parts.iter().rev() {
                        todo.push((s, sa, owned));
                    }
                }
                Pat::Or { .. } => {
                    return Err(BuildError::UnsupportedPat { pat: p, what: "an or-pattern in a `let` or parameter pattern" });
                }
                Pat::Lit(_) | Pat::NegLit(_) | Pat::Range { .. } => {
                    return Err(BuildError::UnsupportedPat {
                        pat: p,
                        what: "a literal or range pattern in a `let` or parameter pattern (refutable)",
                    });
                }
                Pat::Missing => return Err(malformed("a missing pattern in a built body")),
            }
        }
        Ok(())
    }

    /// Variant pattern `p` names a struct, or the only variant of an enum: irrefutable.
    fn only_variant(&mut self, p: PatId) -> Result<(), BuildError> {
        match self.pat_def(p) {
            Some(Def::Adt(_)) => Ok(()),
            Some(Def::Ctor { adt, .. }) => match self.adts.adt(adt) {
                Some(a) if a.variants.len() == 1 => Ok(()),
                Some(_) => Err(BuildError::UnsupportedPat {
                    pat: p,
                    what: "a variant pattern of an enum of more than one variant in a `let` or parameter pattern (refutable: a `let` needs `else`)",
                }),
                None => Err(malformed("a variant pattern of an enum the build does not know")),
            },
            Some(_) => Err(BuildError::UnsupportedPat {
                pat: p,
                what: "a path pattern naming a constant or a static in a `let` or parameter pattern (refutable)",
            }),
            None => Err(malformed("a variant pattern the check did not resolve")),
        }
    }

    /// Whether tuple, tuple-struct or struct pattern `p` at `a` leaves out a part that needs
    /// drop: a `..`'s fields, or the fields a struct pattern does not name.
    fn left_out_drops(&mut self, p: PatId, a: At) -> Result<bool, BuildError> {
        let pat = self.pat(p)?;
        let variant = match pat {
            Pat::Tuple { .. } => 0,
            Pat::TupleStruct { .. } | Pat::Struct { .. } => {
                Self::def_variant(self.pat_def(p)).ok_or(malformed("a variant pattern the check did not resolve"))?
            }
            // A slice pattern's `..` is a subpattern of its own (a `_`, or `x @ ..`).
            _ => return Ok(false),
        };
        let l = self.layout(a.g).map_err(|error| Self::pat_ty_err(p, error))?;
        let (first, len, _) = match self.layouts[l as usize].shape {
            Shape::Fields { .. } | Shape::Enum { .. } => self.variant_rows(l, variant)?,
            Shape::Array { .. } => return Err(malformed("a tuple or variant pattern over an array")),
        };
        match pat {
            Pat::Tuple { elems, rest: Some(r) } | Pat::TupleStruct { elems, rest: Some(r), .. } => {
                let n = len as usize;
                let subs = elems.len as usize;
                if subs > n {
                    return Err(malformed("a tuple or variant pattern of another arity than its type"));
                }
                for k in r as usize..r as usize + (n - subs) {
                    let g = self.layout_field_tys[first as usize + k];
                    if self.needs_drop(g).map_err(|error| Self::pat_ty_err(p, error))? {
                        return Ok(true);
                    }
                }
            }
            Pat::Struct { fields, .. } => {
                for row in first..first + len {
                    let name = self.layout_fields[row as usize].0;
                    let named = self
                        .body
                        .field_pats
                        .get(fields.range())
                        .ok_or(malformed("a struct pattern's fields past `field_pats`"))?
                        .iter()
                        .any(|fp| fp.name == name);
                    let g = self.layout_field_tys[row as usize];
                    if !named && self.needs_drop(g).map_err(|error| Self::pat_ty_err(p, error))? {
                        return Ok(true);
                    }
                }
            }
            _ => {}
        }
        Ok(false)
    }
}
