//! `args-inline` (AGENTS.md rule 83 `options-struct`, decision D126): an options struct, marked
//! `#[blaze::args]`, is a call's named arguments and nothing else, so a value of it is only ever
//! written as a struct literal directly in the call that takes it:
//!
//! ```text
//! Map::new(MapOpts { hasher: FixedState, alloc: frame, capacity: None })   // the only form
//! ```
//!
//! The first of blaze's typed lints: the body check knows each struct literal's type and the
//! place it is written, which no token rule does. After the end sequence (every type settled),
//! one pass over the body files a [`TypedFinding`] per misuse, each naming the struct:
//!
//! - [`ArgsMisuse::Literal`]: a literal of an args struct that is not directly an argument of a
//!   call or a method call (a `let` initialiser, a `const` or `static` value, a return or tail
//!   value, a field initialiser, a match arm, a closure's result, a method call's receiver);
//! - [`ArgsMisuse::Bound`]: a binding of the struct's type that is not a parameter (a `let`, a
//!   match arm, a `for`): a fn's or closure's parameter of it is the taker, which is fine;
//! - [`ArgsMisuse::Forwarded`]: a parameter of the struct's type passed on whole to another call:
//!   the taker reads its fields and writes a new literal (reading fields is fine);
//! - [`ArgsMisuse::Result`]: the body's result is the struct (a fn's return type, a `const`'s or
//!   `static`'s type), so a helper hands one out.
//!
//! A struct field of an args type has no body: that finding is the head's (owed, see the report
//! of piece AI). Parentheses are looked through: `f((Opts { .. }))` is the literal in the call.
//!
//! What the pass reads is recorded as every read is: the mark is [`TypeAttrs::ARGS`], answered
//! by the world's `attrs` (Question::Attrs), asked once per ADT the pass meets, so marking or
//! unmarking a struct moves exactly the bodies that read it (rule 21). A finding is no type
//! error: it is kept apart from the diagnostics ([`crate::Checked::findings`]), the body still
//! lowers and builds, and its level is the configuration's, decided when printed (rule 81).

use blaze_own::{Alloc, Vec};

use crate::body::{Expr, Pat};
use crate::diag::At;
use crate::error::{CheckError, Limit, Stop};
use crate::ids::{ExprId, ItemKey, PatId, Range};
use crate::machine::Machine;
use crate::ty::{Ty, TyView, TypeCtx};
use crate::world::{TypeAttrs, World};

/// How a value of an options struct was written other than as a literal in the call that
/// takes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArgsMisuse {
    /// A struct literal of it that is not directly an argument of a call or method call.
    Literal,
    /// A binding of its type that is not a parameter.
    Bound,
    /// A parameter of its type passed on whole to another call.
    Forwarded,
    /// The body's result is of its type: a fn's return type, a `const`'s or a `static`'s.
    Result,
}

impl ArgsMisuse {
    /// What was written, in words a finding says after the struct's name.
    pub const fn says(self) -> &'static str {
        match self {
            ArgsMisuse::Literal => "is written here as a literal that is not directly an argument of the call that takes it",
            ArgsMisuse::Bound => "is bound to a name here; write the literal in the call instead",
            ArgsMisuse::Forwarded => "is passed on whole here; read the parameter's fields and write a new literal in the call",
            ArgsMisuse::Result => "is this body's result (a return type, a `const` or a `static`); a helper never hands one out",
        }
    }
}

/// A finding of a lint the body check decides from types (rule 81): a record keyed like the
/// check, with no level and an item-relative range (rule 71).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TypedFinding {
    /// `args-inline`: `adt`, an options struct (`#[blaze::args]`), written as `misuse` says.
    ArgsInline { misuse: ArgsMisuse, adt: ItemKey, at: Range },
    /// `unsafe-op-in-unsafe-fn` (rustc's `unsafe_op_in_unsafe_fn`): `op`, in an `unsafe fn`'s
    /// body of edition 2024 outside any `unsafe` block (rules/unsafety.rs).
    UnsafeOpInUnsafeFn { op: crate::diag::UnsafeOp, at: Range },
}

impl TypedFinding {
    /// The lint's name, as blaze's lint vocabulary spells it (blaze-lint `Rule::name`).
    pub const fn lint(&self) -> &'static str {
        match self {
            TypedFinding::ArgsInline { .. } => "args-inline",
            TypedFinding::UnsafeOpInUnsafeFn { .. } => "unsafe-op-in-unsafe-fn",
        }
    }

    /// Its item-relative range.
    pub const fn at(&self) -> Range {
        match *self {
            TypedFinding::ArgsInline { at, .. } | TypedFinding::UnsafeOpInUnsafeFn { at, .. } => at,
        }
    }

    /// The order the record keeps them in: by place, then by lint, then by kind (rule 25). One
    /// expression or pattern is one unsafe operation, so no two of that lint share a place.
    pub const fn order(&self) -> (u32, u32, u8, u8) {
        match *self {
            TypedFinding::ArgsInline { misuse, at, .. } => (at.start, at.end, 0, misuse as u8),
            TypedFinding::UnsafeOpInUnsafeFn { at, .. } => (at.start, at.end, 1, 0),
        }
    }
}

/// The most parentheses looked through around one argument (rule 18).
const MOST_PARENS: u32 = 64;

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// The `args-inline` pass, after the end sequence; `output` is the body's result type as
    /// `start_body` made it.
    pub(crate) fn args_inline(&mut self, output: Ty) -> Result<(), Stop> {
        let body = self.body;
        let alloc = self.alloc;
        // Which expressions are directly an argument of a call or method call, and which
        // patterns are parameters (the fn's, and each closure's).
        let mut is_arg: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(body.exprs.len(), alloc);
        is_arg.resize(body.exprs.len(), false);
        let mut is_param: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(body.pats.len(), alloc);
        is_param.resize(body.pats.len(), false);
        for &p in body.params {
            if let Some(slot) = is_param.get_mut(p.ix()) {
                *slot = true;
            }
        }
        let mut args: Vec<ExprId, Alloc<'a>> = Vec::new_in(alloc);
        for &x in body.exprs {
            match x {
                Expr::Call { args: list, .. } | Expr::MethodCall { args: list, .. } => {
                    for &a in body.expr_list(list)? {
                        let a = self.unparen(a)?;
                        if let Some(slot) = is_arg.get_mut(a.ix()) {
                            *slot = true;
                        }
                        args.push(a);
                    }
                }
                Expr::Closure { params, .. } => {
                    for &p in body.pat_list(params)? {
                        if let Some(slot) = is_param.get_mut(p.ix()) {
                            *slot = true;
                        }
                    }
                }
                _ => {}
            }
        }
        // Each ADT's mark, asked once.
        let mut marks: Vec<(ItemKey, bool), Alloc<'a>> = Vec::new_in(alloc);
        for (i, &x) in body.exprs.iter().enumerate() {
            if !matches!(x, Expr::Struct { .. }) || is_arg.get(i).copied().unwrap_or(false) {
                continue;
            }
            let e = ExprId(i as u32);
            let t = self.expr_ty[e.ix()];
            if let Some(adt) = self.args_adt(t, &mut marks)? {
                self.file_args(ArgsMisuse::Literal, adt, At::Expr(e));
            }
        }
        for &a in args.iter() {
            let Expr::Local(p) = body.expr(a)? else { continue };
            if !is_param.get(p.ix()).copied().unwrap_or(false) {
                // A non-parameter binding of the type is found where it is bound.
                continue;
            }
            let t = self.local_ty[p.ix()];
            if let Some(adt) = self.args_adt(t, &mut marks)? {
                self.file_args(ArgsMisuse::Forwarded, adt, At::Expr(a));
            }
        }
        for (i, &pat) in body.pats.iter().enumerate() {
            if !matches!(pat, Pat::Bind { .. }) || is_param.get(i).copied().unwrap_or(false) {
                continue;
            }
            let p = PatId(i as u32);
            let t = self.local_ty[p.ix()];
            if let Some(adt) = self.args_adt(t, &mut marks)? {
                self.file_args(ArgsMisuse::Bound, adt, At::Pat(p));
            }
        }
        if let Some(adt) = self.args_adt(output, &mut marks)? {
            self.file_args(ArgsMisuse::Result, adt, At::Expr(body.root));
        }
        Ok(())
    }

    /// `e` with the parentheses around it looked through; past [`MOST_PARENS`], refused.
    fn unparen(&self, mut e: ExprId) -> Result<ExprId, Stop> {
        for _ in 0..MOST_PARENS {
            match self.body.expr(e)? {
                Expr::Paren(inner) => e = inner,
                _ => return Ok(e),
            }
        }
        Err(CheckError::Limit(Limit::Table).into())
    }

    /// The ADT `t` is when it is an options struct (`#[blaze::args]`): its mark read once per
    /// ADT through the world's recorded `attrs`.
    fn args_adt(&mut self, t: Ty, marks: &mut Vec<(ItemKey, bool), Alloc<'a>>) -> Result<Option<ItemKey>, Stop> {
        let TyView::Adt(k, _) = self.view(t) else { return Ok(None) };
        if let Some(&(_, marked)) = marks.iter().find(|(m, _)| *m == k) {
            return Ok(marked.then_some(k));
        }
        let marked = self.reads.attrs(k)?.0 & TypeAttrs::ARGS != 0;
        marks.push((k, marked));
        Ok(marked.then_some(k))
    }

    fn file_args(&mut self, misuse: ArgsMisuse, adt: ItemKey, at: At) {
        let at = self.range_of(at);
        self.typed_findings.push(TypedFinding::ArgsInline { misuse, adt, at });
    }
}
