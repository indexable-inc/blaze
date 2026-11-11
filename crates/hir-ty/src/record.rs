//! blaze's records (`#![blaze::records]`, docs/v2/plan/design/records.md): the type
//! `{ a: A, b: B }` is core's `#[lang = "record"]` struct over the field names and the tuple of
//! the field types in canonical order, `Record<"a,b", (A, B)>` (`hir_def::record` is the
//! canonical form). This module makes that type and reads it back: nothing else in hir-ty
//! knows how a record is laid out.
//!
//! The type is an ordinary instance of one core item, so two records with the same fields are
//! one type in every crate and every build with no id of blaze's own, and layout, drop,
//! variance, auto traits and codegen see a struct over a tuple.

use hir_def::{AdtId, StructId, lang_item::LangItems};
use rustc_abi::Size;
use rustc_type_ir::inherent::{IntoKind as _, Ty as _};

use crate::next_solver::{
    Const, ConstKind, Consts, DbInterner, GenericArg, GenericArgKind, GenericArgs, Ty, TyKind,
    Tys, ValTree, ValTreeKind,
};

/// The constant of a record's name list `list` (`hir_def::record::joined`): the `&'static str`
/// that is the first argument of core's `Record`. A `str`'s value tree is a branch of its
/// bytes, as rustc's.
pub fn names_const<'db>(interner: DbInterner<'db>, list: &str) -> Const<'db> {
    let types = interner.default_types();
    let u8_values = &types.consts.u8_values;
    let bytes = Consts::new_from_iter(list.bytes().map(|byte| u8_values[usize::from(byte)]));
    let tree = ValTree::new(ValTreeKind::Branch(bytes));
    Const::new_value(interner, tree, types.types.static_str_ref)
}

/// The record type whose name list is `list` and whose field types, in the list's order, are
/// `tys`: `Record<list, (tys..)>` of the lang item `record`.
pub fn record_ty<'db>(
    interner: DbInterner<'db>,
    record: StructId,
    list: &str,
    tys: &[Ty<'db>],
) -> Ty<'db> {
    debug_assert_eq!(hir_def::record::names(list).count(), tys.len(), "a name per field type");
    let names = GenericArg::from(names_const(interner, list));
    let tuple = GenericArg::from(Ty::new_tup(interner, tys));
    Ty::new_adt(interner, AdtId::StructId(record), GenericArgs::new_from_slice(&[names, tuple]))
}

/// A record type read back: its name list and its field types in the list's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordFields<'db> {
    /// The field names in canonical order, joined (`hir_def::record::joined`).
    pub list: Box<str>,
    /// The field types, in the order of the names.
    pub tys: Tys<'db>,
}

impl<'db> RecordFields<'db> {
    /// The field names, in canonical order.
    pub fn names(&self) -> impl Iterator<Item = &str> + Clone {
        hir_def::record::names(&self.list)
    }

    /// The place of the field `name`: its index in the tuple of the field types.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        hir_def::record::index_of(&self.list, name)
    }

    /// The type of the field at `at`.
    pub fn ty(&self, at: usize) -> Option<Ty<'db>> {
        self.tys.as_slice().get(at).copied()
    }

    /// How many fields the record has.
    pub fn len(&self) -> usize {
        self.tys.as_slice().len()
    }

    pub fn is_empty(&self) -> bool {
        self.tys.as_slice().is_empty()
    }

    /// The tuple of the field types: the type of the one field of core's `Record`.
    pub fn tuple(&self, interner: DbInterner<'db>) -> Ty<'db> {
        Ty::new_tup(interner, self.tys.as_slice())
    }
}

/// Whether `ty` is an instance of core's `Record`, with whatever arguments: a record type, or
/// `Record<FIELDS, T>` as core's own impls write it.
pub fn is_record_adt(lang_items: &LangItems, ty: Ty<'_>) -> bool {
    match (ty.kind(), lang_items.Record) {
        (TyKind::Adt(def, _), Some(record)) => def.def_id() == AdtId::StructId(record),
        _ => false,
    }
}

/// `ty` as a record: `Some` when it is core's `Record` of a known name list over a tuple with
/// a type per name. `None` for every other type, and for `Record<FIELDS, T>` whose arguments
/// are not yet known (an inference variable) or are parameters (core's own impls), which is
/// the plain struct it is declared as.
pub fn record_fields<'db>(lang_items: &LangItems, ty: Ty<'db>) -> Option<RecordFields<'db>> {
    let TyKind::Adt(def, args) = ty.kind() else { return None };
    if def.def_id() != AdtId::StructId(lang_items.Record?) {
        return None;
    }
    let mut args = args.iter();
    let (Some(names), Some(tuple), None) = (args.next(), args.next(), args.next()) else {
        return None;
    };
    let GenericArgKind::Const(names) = names.kind() else { return None };
    let GenericArgKind::Type(tuple) = tuple.kind() else { return None };
    let TyKind::Tuple(tys) = tuple.kind() else { return None };
    let list = name_list(names)?;
    if hir_def::record::names(&list).count() != tys.as_slice().len() {
        return None;
    }
    Some(RecordFields { list, tys })
}

/// The name list a record's first argument holds: the bytes of a `str` value, which
/// [`names_const`] made. `None` for a constant that is not a value yet, or not a string.
fn name_list(names: Const<'_>) -> Option<Box<str>> {
    let ConstKind::Value(value) = names.kind() else { return None };
    let ValTreeKind::Branch(bytes) = value.value.inner() else { return None };
    let mut list = Vec::with_capacity(bytes.as_slice().len()); // alloc: a record's name list
    for byte in bytes.as_slice() {
        let ConstKind::Value(byte) = byte.kind() else { return None };
        let bits = byte.try_to_leaf()?.try_to_bits(Size::from_bits(8u64)).ok()?;
        list.push(u8::try_from(bits).ok()?);
    }
    String::from_utf8(list).ok().map(String::into_boxed_str)
}

/// The function a record value takes its defaults from, and which of its parameters (`self`
/// first) the value is the argument of: `arg` is an argument of a call to a function by name
/// (`f(arg)`, or `x.method(arg)`) in `store`. `None` for a record value anywhere else: an
/// argument of a call through a function pointer, a closure or a `Fn` value, or of a tuple
/// struct's constructor, which declare no defaults.
pub fn record_argument_callee(
    store: &hir_def::expr_store::ExpressionStore,
    infer: &crate::InferenceResult<'_>,
    arg: hir_def::hir::ExprId,
) -> Option<(hir_def::FunctionId, u32)> {
    use hir_def::hir::Expr;
    store.exprs().find_map(|(id, expr)| match expr {
        Expr::Call { callee, args } => {
            let at = args.iter().position(|&it| it == arg)?;
            let TyKind::FnDef(def, _) = infer.expr_ty(callee).kind() else { return None };
            let crate::CallableDefId::FunctionId(function) = def.0 else { return None };
            Some((function, at as u32))
        }
        Expr::MethodCall { args, .. } => {
            let at = args.iter().position(|&it| it == arg)?;
            let (function, _) = infer.method_resolution(id)?;
            Some((function, at as u32 + 1))
        }
        _ => None,
    })
}
