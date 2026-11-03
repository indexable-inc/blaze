//! The part of `compiler/rustc_ast/src/visit.rs` at rust-lang/rust 12433c307 (MIT or
//! Apache-2.0) rustfmt uses: the kinds of function it formats, and a visitor that finds the macro
//! calls written as items, where `cfg_if!` and `cfg_select!` declare modules.

use crate::ast::*;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum AssocCtxt {
    Trait,
    Impl { of_trait: bool },
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum FnCtxt {
    Free,
    Foreign,
    Assoc(AssocCtxt),
}

#[derive(Copy, Clone, Debug)]
pub enum BoundKind {
    Bound,
    Impl,
    TraitObject,
    SuperTraits,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LifetimeCtxt {
    Ref,
    Bound,
    GenericArg,
}

#[derive(Clone, Copy, Debug)]
pub enum FnKind<'a> {
    /// E.g., `fn foo()`, `fn foo(&self)`, or `extern "Abi" fn foo()`.
    Fn(FnCtxt, &'a Visibility, &'a Fn),

    /// E.g., `|x, y| body`.
    Closure(
        &'a ClosureBinder,
        &'a Option<CoroutineMarker>,
        &'a Box<FnDecl>,
        &'a Box<Expr>,
    ),
}

impl<'a> FnKind<'a> {
    pub fn header(&self) -> Option<&'a FnHeader> {
        match *self {
            FnKind::Fn(_, _, Fn { sig, .. }) => Some(&sig.header),
            FnKind::Closure(..) => None,
        }
    }

    pub fn ident(&self) -> Option<&'a Ident> {
        match self {
            FnKind::Fn(_, _, Fn { ident, .. }) => Some(ident),
            _ => None,
        }
    }

    pub fn decl(&self) -> &'a FnDecl {
        match self {
            FnKind::Fn(_, _, Fn { sig, .. }) => &sig.decl,
            FnKind::Closure(_, _, decl, _) => decl,
        }
    }

    pub fn ctxt(&self) -> Option<FnCtxt> {
        match self {
            FnKind::Fn(ctxt, ..) => Some(*ctxt),
            FnKind::Closure(..) => None,
        }
    }
}

pub trait Visitor<'ast>: Sized {
    fn visit_item(&mut self, i: &'ast Item) {
        walk_item(self, i)
    }

    fn visit_mac_call(&mut self, _mac: &'ast MacCall) {}
}

/// rustc's walk reaches every macro call inside the item; rustfmt calls it only on an item
/// that is a macro call, which is the one it reaches.
pub fn walk_item<'a, V: Visitor<'a>>(visitor: &mut V, item: &'a Item) {
    if let ItemKind::MacCall(mac) = &item.kind {
        visitor.visit_mac_call(mac);
    }
}
