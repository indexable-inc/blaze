//! `HasAttrs` of `compiler/rustc_ast/src/ast_traits.rs` at rust-lang/rust 12433c307 (MIT or
//! Apache-2.0), for the nodes rustfmt asks the attributes of.

use crate::ast::{
    Arm, AssocItem, AttrItem, AttrVec, Attribute, Block, Crate, Expr, ExprField, FieldDef,
    ForeignItem, GenericParam, Item, Param, Pat, PatField, Path, Stmt, StmtKind, Ty, Variant,
    Visibility, WherePredicate,
};

pub trait HasAttrs {
    /// This is `true` if this `HasAttrs` might support 'custom' (proc-macro) inner
    /// attributes. Attributes like `#![cfg]` and `#![cfg_attr]` are not
    /// considered 'custom' attributes.
    const SUPPORTS_CUSTOM_INNER_ATTRS: bool;
    fn attrs(&self) -> &[Attribute];
    fn visit_attrs(&mut self, f: impl FnOnce(&mut AttrVec));
}

macro_rules! impl_has_attrs {
    (const SUPPORTS_CUSTOM_INNER_ATTRS: bool = $inner:literal, $($T:ty),+ $(,)?) => {
        $(
            impl HasAttrs for $T {
                const SUPPORTS_CUSTOM_INNER_ATTRS: bool = $inner;

                #[inline]
                fn attrs(&self) -> &[Attribute] {
                    &self.attrs
                }

                fn visit_attrs(&mut self, f: impl FnOnce(&mut AttrVec)) {
                    f(&mut self.attrs)
                }
            }
        )+
    };
}

macro_rules! impl_has_attrs_none {
    ($($T:ty),+ $(,)?) => {
        $(
            impl HasAttrs for $T {
                const SUPPORTS_CUSTOM_INNER_ATTRS: bool = false;
                fn attrs(&self) -> &[Attribute] {
                    &[]
                }
                fn visit_attrs(&mut self, _f: impl FnOnce(&mut AttrVec)) {}
            }
        )+
    };
}

impl_has_attrs!(
    const SUPPORTS_CUSTOM_INNER_ATTRS: bool = true,
    AssocItem,
    ForeignItem,
    Item,
);
impl_has_attrs!(
    const SUPPORTS_CUSTOM_INNER_ATTRS: bool = false,
    Arm,
    Crate,
    Expr,
    ExprField,
    FieldDef,
    GenericParam,
    Param,
    PatField,
    Variant,
    WherePredicate,
);
impl_has_attrs_none!(Attribute, AttrItem, Block, Pat, Path, Ty, Visibility);

impl<T: HasAttrs> HasAttrs for Box<T> {
    const SUPPORTS_CUSTOM_INNER_ATTRS: bool = T::SUPPORTS_CUSTOM_INNER_ATTRS;
    fn attrs(&self) -> &[Attribute] {
        (**self).attrs()
    }
    fn visit_attrs(&mut self, f: impl FnOnce(&mut AttrVec)) {
        (**self).visit_attrs(f);
    }
}

impl<T: HasAttrs> HasAttrs for Option<T> {
    const SUPPORTS_CUSTOM_INNER_ATTRS: bool = T::SUPPORTS_CUSTOM_INNER_ATTRS;
    fn attrs(&self) -> &[Attribute] {
        self.as_ref().map(|inner| inner.attrs()).unwrap_or(&[])
    }
    fn visit_attrs(&mut self, f: impl FnOnce(&mut AttrVec)) {
        if let Some(inner) = self.as_mut() {
            inner.visit_attrs(f);
        }
    }
}

impl HasAttrs for StmtKind {
    const SUPPORTS_CUSTOM_INNER_ATTRS: bool = true;

    fn attrs(&self) -> &[Attribute] {
        match self {
            StmtKind::Let(local) => &local.attrs,
            StmtKind::Expr(expr) | StmtKind::Semi(expr) => expr.attrs(),
            StmtKind::Item(item) => item.attrs(),
            StmtKind::Empty => &[],
            StmtKind::MacCall(mac) => &mac.attrs,
        }
    }

    fn visit_attrs(&mut self, f: impl FnOnce(&mut AttrVec)) {
        match self {
            StmtKind::Let(local) => f(&mut local.attrs),
            StmtKind::Expr(expr) | StmtKind::Semi(expr) => expr.visit_attrs(f),
            StmtKind::Item(item) => item.visit_attrs(f),
            StmtKind::Empty => {}
            StmtKind::MacCall(mac) => f(&mut mac.attrs),
        }
    }
}

impl HasAttrs for Stmt {
    const SUPPORTS_CUSTOM_INNER_ATTRS: bool = StmtKind::SUPPORTS_CUSTOM_INNER_ATTRS;
    fn attrs(&self) -> &[Attribute] {
        self.kind.attrs()
    }
    fn visit_attrs(&mut self, f: impl FnOnce(&mut AttrVec)) {
        self.kind.visit_attrs(f);
    }
}
