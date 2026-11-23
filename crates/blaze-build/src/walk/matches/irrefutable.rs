//! Irrefutable patterns beyond a binding, `_`, a tuple and an array without `..`: what walk.rs
//! `bind_value` (a `let`, a parameter, a part bound by value) hands here. Each is taken apart
//! from the value's place by the match builder's own projection (matches.rs `peel`,
//! `deref_at`, `parts`, `bind_slot`), never a second copy of it. Rules from the Reference,
//! "Patterns":
//! - "Refutability": "A pattern is said to be irrefutable when it always matches the value it
//!   is being matched against"; `let` without `else` and function parameters take only those.
//!   A struct pattern of a struct and a tuple-struct or struct pattern of an enum's only
//!   variant are irrefutable, and so is a variant whose every other variant is uninhabited
//!   (`let Ok(x) = r;` over `Result<T, Infallible>`, `only_variant`); any other variant of an
//!   enum of more than one is refused by name.
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
//! binding). A part nothing takes by value (`_`, a unit pattern, a `..`, a field a struct
//! pattern leaves out) is not moved: "Wildcard pattern": "Unlike identifier patterns, it does
//! not copy, move or borrow the value it matches", so it stays with the initializer's owner and
//! drops where that owner drops. A `let` whose pattern leaves such a part that needs drop
//! (`let_owns_parts`) is therefore not built here but as a `let`-`else`'s scrutinee is
//! (`bind_let_owned`): a local initializer stays in place, its moved parts cleared from its
//! field flags, the rest dropped at the local's scope end; a temporary one is held by its
//! statement with a word (walk/temps.rs `hold_temp`, `Held::Moved`), the rest dropped at the
//! statement's end ("Temporary scopes": the `let` statement is the initializer's). A parameter
//! pattern leaving such a part (`_: String`, `(x, _)`) is `bind_param`'s: the argument is the
//! callee's, held in the parameters' scope with a word and dropped after the parameter's
//! bindings ("Destructors", "Drop scopes"). A `let` with no initializer declares its pattern's
//! bindings (`declare_let`). An explicit `ref` over the
//! binder's own value is refused by name: it must borrow the initializer's place, and
//! `bind_value` is given a value, not that place.

use core::alloc::Allocator;

use blaze_own::Vec;
use blaze_typeck::{Def, ExprId, PNode, PTy, Pat, PatId};

use super::super::{Shape, V, Walk, malformed};
use super::At;
use crate::BuildError;

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Binds `v`, the binder's own value of pattern `root`'s type, to irrefutable pattern
    /// `root`, by a work list. Every form outside "Refutability"'s irrefutable ones, and every
    /// irrefutable one not built here, is refused by name.
    pub(in crate::walk) fn bind_irrefutable(&mut self, root: PatId, v: V) -> Result<(), BuildError> {
        let t = *self.checked.pat_ty.get(root.ix()).ok_or(malformed("a pattern without a type in the record"))?;
        let g = self.ground_ty(t).map_err(|error| Self::pat_ty_err(root, error))?;
        self.bind_irrefutable_at(root, v, g)
    }

    /// [`Self::bind_irrefutable`] of `v`, a value of ground type `g`. The check records a
    /// pattern that peels references ("Binding modes": a struct, tuple-struct, tuple, slice or
    /// path pattern over `&T`) at its peeled type `T` (rules/pat.rs `step_pat`; rustc keeps the
    /// references in `pat_adjustments`, which the record does not hold), so a binder whose value
    /// is the reference (`let Ending { fw, stream } = unsafe { &*p };`) names `g` itself:
    /// `bind_pattern_at` then follows the references (matches.rs `peel`).
    pub(in crate::walk) fn bind_irrefutable_at(&mut self, root: PatId, v: V, g: PTy) -> Result<(), BuildError> {
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
        self.bind_pattern_at(root, At { at: place, g, by_ref: false, copy: false }, true)
    }

    /// Binds irrefutable pattern `root` at `at`, by a work list. `owned`: the place is the
    /// binder's own (a `let`'s value moved in, a parameter's copy), so a by-value binding takes
    /// its part; else (a `let` initializer's place, ref_bind.rs) nothing in it moves and an
    /// explicit `ref` takes its address.
    pub(super) fn bind_pattern_at(&mut self, root: PatId, at: At, owned: bool) -> Result<(), BuildError> {
        // (pattern, its part's place, whether the part is the binder's own: not behind a
        // reference the pattern followed).
        let mut todo: Vec<(PatId, At, bool), A> = Vec::new_in(self.alloc);
        let mut parts: Vec<(PatId, At), A> = Vec::new_in(self.alloc);
        todo.push((root, at, owned));
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
                Pat::Tuple { .. } | Pat::TupleStruct { .. } | Pat::Struct { .. } | Pat::Record { .. } | Pat::Slice { .. } | Pat::Path(_) => {
                    let behind = matches!(self.ground_node(at.g).map_err(|error| Self::pat_ty_err(p, error))?, PNode::Ref(..));
                    let a = self.peel(p, at)?;
                    let owned = owned && !behind;
                    if let Pat::TupleStruct { .. } | Pat::Struct { .. } | Pat::Path(_) = pat {
                        self.only_variant(p, a.g)?;
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
                    if owned && drops && self.left_out_drops(p, a.g)? {
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

    /// Variant pattern `p` over a value of ground type `g` (the ADT itself: references peeled)
    /// names a struct, the only variant of an enum, or a variant every other variant of which
    /// is uninhabited: irrefutable. The Reference, "Patterns", "Refutability", and the
    /// exhaustiveness of empty types: a variant holding a field of an uninhabited type
    /// (`Infallible`, `!`, an enum of no variants, a struct or tuple holding one) has no value,
    /// so `let Ok(x) = r;` over `Result<T, Infallible>` always matches (hybrid-array's
    /// `Array::from_fn`: `let Ok(ret) = Self::try_from_fn::<Infallible>(..);`). It is built as
    /// the plain destructure, with no test: the layout's own answer (`ls`'s `uninhabited`)
    /// says no value of the other variants exists. Any other variant pattern of an enum of more
    /// than one variant is refutable: refused by name (a `let` needs `else`, which walk/lets.rs
    /// builds by tests).
    fn only_variant(&mut self, p: PatId, g: PTy) -> Result<(), BuildError> {
        match self.pat_def(p) {
            Some(Def::Adt(_)) => Ok(()),
            Some(Def::Ctor { adt, variant, .. }) => match self.adts.adt(adt).map(|a| a.variants.len()) {
                Some(1) => Ok(()),
                Some(_) if self.others_uninhabited(p, g, variant)? => Ok(()),
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

    /// Whether every variant of enum `g` but `variant` is uninhabited: one of its fields' types
    /// has no value (`ls`'s `uninhabited`, which a struct, tuple or array of one inherits).
    fn others_uninhabited(&mut self, p: PatId, g: PTy, variant: u32) -> Result<bool, BuildError> {
        let l = self.layout(g).map_err(|error| Self::pat_ty_err(p, error))?;
        let Shape::Enum { first, len, .. } = self.layouts[l as usize].shape else {
            return Err(malformed("a variant pattern of an enum over a value not laid out as an enum"));
        };
        for v in (0..len).filter(|&v| v != variant) {
            let (ff, fl) = self.layout_variants[(first + v) as usize];
            let mut empty = false;
            for row in ff..ff + fl {
                let ft = self.layout_field_tys[row as usize];
                let id = self.ls_of(ft).map_err(|error| Self::pat_ty_err(p, error))?;
                if self.ls.get(id).uninhabited {
                    empty = true;
                    break;
                }
            }
            if !empty {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The ground type the check recorded for pattern `p` (a pattern that peels references at
    /// its peeled type: `bind_irrefutable_at`).
    fn pat_ground(&mut self, p: PatId) -> Result<PTy, BuildError> {
        let t = *self.checked.pat_ty.get(p.ix()).ok_or(malformed("a pattern without a type in the record"))?;
        self.ground_ty(t).map_err(|error| Self::pat_ty_err(p, error))
    }

    /// Refuses by name what makes `root`, a pattern in an irrefutable position (a `let`
    /// without `else`, a parameter), refutable or not built here, before a binder that does
    /// not test (`bind_tested`, given no failure edge) binds it: a literal or range, an
    /// or-pattern, a variant of an enum with another inhabited variant or a path to a constant
    /// (`only_variant`), a slice pattern over a slice `[T]` (its length untested). The same
    /// refusals `bind_pattern_at` makes on its way.
    fn refuse_refutable(&mut self, root: PatId) -> Result<(), BuildError> {
        let mut todo: Vec<PatId, A> = Vec::new_in(self.alloc);
        todo.push(root);
        while let Some(p) = todo.pop() {
            let elems = match self.pat(p)? {
                Pat::Wild => continue,
                Pat::Bind { sub, .. } => {
                    todo.extend(sub);
                    continue;
                }
                Pat::Ref { inner, .. } => {
                    todo.push(inner);
                    continue;
                }
                Pat::Tuple { elems, .. } => elems,
                Pat::Slice { elems, .. } => {
                    let g = self.pat_ground(p)?;
                    if let PNode::Slice(_) = self.ground_node(g).map_err(|error| Self::pat_ty_err(p, error))? {
                        return Err(BuildError::UnsupportedPat {
                            pat: p,
                            what: "a slice pattern over a slice `[T]` in a `let` or parameter pattern",
                        });
                    }
                    elems
                }
                Pat::TupleStruct { elems, .. } => {
                    let g = self.pat_ground(p)?;
                    self.only_variant(p, g)?;
                    elems
                }
                Pat::Struct { fields, .. } => {
                    let g = self.pat_ground(p)?;
                    self.only_variant(p, g)?;
                    let fps = self.body.field_pats.get(fields.range()).ok_or(malformed("a struct pattern's fields past `field_pats`"))?;
                    todo.extend(fps.iter().map(|fp| fp.pat));
                    continue;
                }
                Pat::Path(_) => {
                    let g = self.pat_ground(p)?;
                    self.only_variant(p, g)?;
                    continue;
                }
                Pat::Record { fields, .. } => {
                    let fps = self.body.field_pats.get(fields.range()).ok_or(malformed("a record pattern's fields past `field_pats`"))?;
                    todo.extend(fps.iter().map(|fp| fp.pat));
                    continue;
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
            };
            let subs = self.body.pat_lists.get(elems.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?;
            todo.extend(subs.iter().copied());
        }
        Ok(())
    }

    /// `let pat;`, no initializer and no `else` (num-bigint's `let (mut q, r1, d);`): each
    /// binding of `pat` is declared and none initialized, each joining the innermost drop scope
    /// in declaration order (walk.rs `own` with `init` false: dropped only once an assignment
    /// initializes it). The Reference, "let statements": "introduces a new set of variables,
    /// given by a pattern"; "When no else block is present, the pattern must be irrefutable"
    /// (`refuse_refutable`). No value is matched, so no part is moved or left: `_` and a unit
    /// pattern bind nothing. A `ref` binding, a binding with a subpattern and a `&` pattern
    /// (each binds from a value there is none of) are refused by name.
    pub(in crate::walk) fn declare_let(&mut self, pat: PatId) -> Result<(), BuildError> {
        self.refuse_refutable(pat)?;
        let mut todo: Vec<PatId, A> = Vec::new_in(self.alloc);
        todo.push(pat);
        while let Some(p) = todo.pop() {
            let elems = match self.pat(p)? {
                Pat::Bind { ann, sub: None, .. } if ann.by_ref.is_none() => {
                    self.bind(p)?;
                    self.own(p, false)?;
                    continue;
                }
                Pat::Bind { .. } => {
                    return Err(BuildError::UnsupportedPat {
                        pat: p,
                        what: "a `ref` binding or a binding with a subpattern in a `let` without an initializer",
                    });
                }
                Pat::Ref { .. } => {
                    return Err(BuildError::UnsupportedPat { pat: p, what: "a `&` pattern in a `let` without an initializer" });
                }
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                    let fps = self.body.field_pats.get(fields.range()).ok_or(malformed("a struct pattern's fields past `field_pats`"))?;
                    todo.extend(fps.iter().rev().map(|fp| fp.pat));
                    continue;
                }
                Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Slice { elems, .. } => elems,
                // `_`, a unit pattern: nothing declared (`refuse_refutable` took the rest).
                _ => continue,
            };
            let subs = self.body.pat_lists.get(elems.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?;
            todo.extend(subs.iter().rev().copied());
        }
        Ok(())
    }

    /// Parameter pattern `p` (a fn's, a closure's, the `rust-call` tuple's) bound to `v`, the
    /// callee's own copy of the argument, of ground type `g` (the parameter's type), and its
    /// bindings owned. Rules from the Reference:
    /// - "Function parameters": the pattern is irrefutable (`refuse_refutable`).
    /// - "Destructors", "Drop scopes": "Each function has a drop scope for the whole function";
    ///   "All function parameters are in the scope of the entire function body, so are dropped
    ///   last when evaluating a function. Each actual function parameter is dropped after any
    ///   bindings introduced in that parameter's pattern." (its example: `(x, _)` and `(_, y)`
    ///   drop as `y`, the second's rest, `x`, the first's rest).
    /// - "Wildcard pattern": `_` does not move what it matches, so the argument still owns it.
    ///
    /// A pattern that leaves no part that needs drop unbound is bound as a `let` of the value
    /// (`bind_value_of`, then `own_pat`). One that leaves such a part (`fn from(_: NulError)`,
    /// `(x, _)`, `S { a, .. }`) owns the argument as a temporary scrutinee is owned
    /// (walk/moves.rs `own_param`: a word of its own, all held, `Held::Moved` in the
    /// parameters' scope, pushed before the bindings), then binds by `bind_tested`: each
    /// by-value binding of a part that needs drop moves it out (its bit cleared), so at the
    /// function's exit the bindings drop first, last declared first, then what is left of the
    /// argument, its parts in declaration order (walk/moves.rs `drop_moved`).
    pub(in crate::walk) fn bind_param(&mut self, p: PatId, v: V, g: PTy) -> Result<(), BuildError> {
        let leaves = !matches!(self.pat(p)?, Pat::Bind { .. })
            && self.needs_drop(g).map_err(|error| Self::pat_ty_err(p, error))?
            && self.let_leaves_drops(p)?;
        if !leaves {
            self.bind_value_of(p, v, g)?;
            return self.own_pat(p);
        }
        self.refuse_refutable(p)?;
        let V::Place(base, off) = v else { return Err(malformed("a parameter that needs drop not held in memory")) };
        let m = self.matches.len() as u32;
        self.own_param(m, (base, off), g);
        self.move_arm(m, p, false);
        let bound = self.bind_tested(p, At { at: (base, off), g, by_ref: false, copy: false });
        self.move_arm_end();
        bound
    }

    /// Whether tuple, tuple-struct or struct pattern `p` over a value of ground type `g` (no
    /// reference: what `peel` gives) leaves out a part that needs drop: a `..`'s fields, or the
    /// fields a struct pattern does not name.
    fn left_out_drops(&mut self, p: PatId, g: PTy) -> Result<bool, BuildError> {
        let pat = self.pat(p)?;
        let variant = match pat {
            Pat::Tuple { .. } | Pat::Record { .. } => 0,
            Pat::TupleStruct { .. } | Pat::Struct { .. } => {
                Self::def_variant(self.pat_def(p)).ok_or(malformed("a variant pattern the check did not resolve"))?
            }
            // A slice pattern's `..` is a subpattern of its own (a `_`, or `x @ ..`).
            _ => return Ok(false),
        };
        let l = self.layout(g).map_err(|error| Self::pat_ty_err(p, error))?;
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
            Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
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

    /// Whether `let pat = init;` owns its initializer's parts (the module doc's "Ownership"):
    /// `pat` is neither a binding nor `_` (walk.rs `stmt` builds those), does not borrow
    /// `init`'s place (ref_bind.rs), and leaves unbound a part that needs drop
    /// (`let_leaves_drops`). A pure fact of the body, asked by walk.rs `scan_flags` (the
    /// local's field flags), `stmt` (`init` pushed as a scrutinee) and `Task::Let` (bound by
    /// `bind_let_owned`): all three answers are the same.
    pub(in crate::walk) fn let_owns_parts(&mut self, pat: PatId, init: ExprId) -> Result<bool, BuildError> {
        if matches!(self.pat(pat)?, Pat::Bind { .. } | Pat::Wild) || self.let_borrows_place(pat, init)? {
            return Ok(false);
        }
        // An initializer that needs no drop (a reference the pattern peels: the check records
        // such a pattern at the peeled type, `bind_irrefutable_at`) owns nothing to leave.
        let t = self.adjusted_ty(init)?;
        let g = self.ground_ty(t).map_err(|error| Self::ty_err(init, error))?;
        if !self.needs_drop(g).map_err(|error| Self::ty_err(init, error))? {
            return Ok(false);
        }
        self.let_leaves_drops(pat)
    }

    /// Whether irrefutable pattern `root` leaves unbound, where its value is owned (not behind
    /// a reference it follows, explicitly or by the default binding mode), a part that needs
    /// drop: a `_`, a unit pattern, a `..`'s fields or a field a struct pattern leaves out. A
    /// binding's subpattern is not looked into: a by-value binding takes the whole part, and
    /// a `ref` one borrows it (ref_bind.rs builds that `let`).
    fn let_leaves_drops(&mut self, root: PatId) -> Result<bool, BuildError> {
        let mut todo: Vec<PatId, A> = Vec::new_in(self.alloc);
        todo.push(root);
        while let Some(p) = todo.pop() {
            let pat = self.pat(p)?;
            let t = *self.checked.pat_ty.get(p.ix()).ok_or(malformed("a pattern without a type in the record"))?;
            let g = self.ground_ty(t).map_err(|error| Self::pat_ty_err(p, error))?;
            match pat {
                Pat::Wild | Pat::Path(_) => {
                    if self.needs_drop(g).map_err(|error| Self::pat_ty_err(p, error))? {
                        return Ok(true);
                    }
                }
                Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Slice { elems, .. } => {
                    if let PNode::Ref(..) = self.ground_node(g).map_err(|error| Self::pat_ty_err(p, error))? {
                        continue;
                    }
                    if self.needs_drop(g).map_err(|error| Self::pat_ty_err(p, error))? && self.left_out_drops(p, g)? {
                        return Ok(true);
                    }
                    let subs = self.body.pat_lists.get(elems.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?;
                    todo.extend(subs.iter().copied());
                }
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                    if let PNode::Ref(..) = self.ground_node(g).map_err(|error| Self::pat_ty_err(p, error))? {
                        continue;
                    }
                    if self.needs_drop(g).map_err(|error| Self::pat_ty_err(p, error))? && self.left_out_drops(p, g)? {
                        return Ok(true);
                    }
                    let fps = self.body.field_pats.get(fields.range()).ok_or(malformed("a struct pattern's fields past `field_pats`"))?;
                    todo.extend(fps.iter().map(|fp| fp.pat));
                }
                // A binding takes or borrows its whole part; `&p` is behind a reference; a
                // literal, a range or an or-pattern is refused by `bind_pattern_at`'s own arms.
                _ => {}
            }
        }
        Ok(false)
    }

    /// `Task::Let` of `let pat = init;` that owns its initializer's parts (`let_owns_parts`),
    /// `v` the initializer as `stmt` pushed it (walk/lets.rs `push_scrutinee`: a local, or a
    /// field of one, as its place; anything else as its value). Owned as a `let`-`else`'s
    /// scrutinee (walk/lets.rs `let_else_test`, with no test to fail): `owned_place` roots it
    /// (a local by its field flags, a temporary held by the statement with a word), each
    /// by-value binding of a part that needs drop moves it out (walk/moves.rs `move_out`), and
    /// what no binding took drops with its owner. `bind_tested` owns the bindings in
    /// declaration order, as `own_pat` does ("Destructors": variables drop in reverse order of
    /// declaration). Returns where they start in `scope_pats` (for `place_super_let`).
    pub(in crate::walk) fn bind_let_owned(&mut self, pat: PatId, init: ExprId, v: V) -> Result<usize, BuildError> {
        // `bind_tested` tests nothing: what would make the pattern refutable is refused first.
        self.refuse_refutable(pat)?;
        let srepr = self.adjusted_repr(init)?;
        let m = self.matches.len() as u32;
        let at = self.owned_place(m, init, init, v, srepr)?;
        let from = self.scope_pats.len();
        self.move_arm(m, pat, false);
        let bound = self.bind_tested(pat, at);
        self.move_arm_end();
        bound?;
        Ok(from)
    }
}
