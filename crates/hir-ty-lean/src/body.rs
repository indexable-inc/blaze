//! Structured MIR: a function's built, borrow-checked, unoptimized MIR as typed statements
//! under structured control flow ([`crate::structured`]). Every back end reads this.
//!
//! What it does to the MIR, beyond the control flow:
//!
//! - **Unwinding goes.** No unwind edge is followed, so no cleanup block is reached. v0 models
//!   a panic as `Result.fail` whatever the panic strategy: `catch_unwind` is refused, safe v0
//!   code has no interior mutability, so nothing a caller can observe survives a panic.
//! - **Storage markers, fake reads and drops go.** A type with a destructor is refused, so
//!   every drop does nothing.
//! - **Checks are one operation.** Built MIR keeps `a + b` in one statement
//!   (`CheckedBinaryOp`) and an index in its place; both stay so, carrying their panic
//!   ([`Rvalue::Bin`]'s `checked`, [`Elem::Index`]). Runtime MIR's pairs
//!   (`t = AddWithOverflow(a, b); assert(!t.1); x = t.0`, and `len`, `i < len`, `assert`
//!   before an index) are folded back to the same ([`fold`]).
//! - **A `&mut` temporary is the place it borrows.** A `&mut` local assigned once, by a borrow
//!   or a move of another, is replaced by its place everywhere, so a write through it is a
//!   write of the place. A `&mut` parameter is a local holding the referent's value, which
//!   the function gives back when it returns.
//! - **A read of a discriminant followed by a switch on it is one `match`**
//!   ([`Cond::Discriminant`]), and a chain of them on one place, which is how MIR building
//!   tests a `match`'s arms, is one `match` of all their arms ([`merge_tests`]).
//!
//! Anything outside v0 is a [`NotTranslated`] naming the construct and where it is.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use base_db::Db;
use hir_def::{
    AdtId, AttrDefId, attrs::AttrFlags,
    builtin_derive::BuiltinDeriveImplTrait, CallableDefId, DefWithBodyId, FunctionId, HasModule as _, ImplId, ItemContainerId,
    Lookup as _, ModuleId, TraitId, VariantId,
    expr_store::{ExpressionStore, pretty::print_expr_hir},
    hir::generics::GenericParams,
    item_tree::FieldsShape,
    signatures::{EnumSignature, FunctionSignature, StructSignature, TraitSignature},
};
use hir_ty::{
    MemoryMap, ParamEnvAndCrate,
    db::HirDatabase as _,
    method_resolution::NoImplMethod,
    mir as m,
    next_solver::{
        AliasConstKind, Const as HConst, ConstKind, DbInterner, FreeConstAliasId, GenericArg,
        GenericArgs, InherentAssocConstId, ParamEnv, TraitRef, Ty as HTy, TyKind, TypingMode,
        infer::{DbInternerInferExt as _, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
    },
};
use rustc_type_ir::{
    IntTy as RIntTy, TypeVisitableExt as _, UintTy,
    inherent::{AdtDef as _, GenericArgs as _, IntoKind as _},
};

use crate::{
    NotTranslated,
    structured::{self, Cfg, CfgBlock, End, Exit, Node},
};

pub mod embed;

pub type Local = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IntTy {
    U8,
    U16,
    U32,
    U64,
    U128,
    Usize,
    I8,
    I16,
    I32,
    I64,
    I128,
    Isize,
}

impl IntTy {
    /// The Lean library's type (`Blaze.U64`).
    pub fn lean(self) -> &'static str {
        match self {
            IntTy::U8 => "U8",
            IntTy::U16 => "U16",
            IntTy::U32 => "U32",
            IntTy::U64 => "U64",
            IntTy::U128 => "U128",
            IntTy::Usize => "Usize",
            IntTy::I8 => "I8",
            IntTy::I16 => "I16",
            IntTy::I32 => "I32",
            IntTy::I64 => "I64",
            IntTy::I128 => "I128",
            IntTy::Isize => "Isize",
        }
    }

    pub fn signed(self) -> bool {
        matches!(
            self,
            IntTy::I8 | IntTy::I16 | IntTy::I32 | IntTy::I64 | IntTy::I128 | IntTy::Isize
        )
    }

    /// Its width. `usize` and `isize` are 64 bits: v0 translates for 64-bit targets, and the
    /// width is part of the translation key.
    pub fn bits(self) -> u32 {
        match self {
            IntTy::U8 | IntTy::I8 => 8,
            IntTy::U16 | IntTy::I16 => 16,
            IntTy::U32 | IntTy::I32 => 32,
            IntTy::U64 | IntTy::I64 | IntTy::Usize | IntTy::Isize => 64,
            IntTy::U128 | IntTy::I128 => 128,
        }
    }

    /// The value the low [`bits`](IntTy::bits) of `bits` are, in decimal.
    pub fn decimal(self, bits: u128) -> String {
        let width = self.bits();
        let low = if width == 128 { bits } else { bits & ((1u128 << width) - 1) };
        if !self.signed() {
            return low.to_string();
        }
        let shift = 128 - width;
        (((low << shift) as i128) >> shift).to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    Bool,
    Int(IntTy),
    Unit,
    Never,
    Tuple(Vec<Ty>),
    /// A struct or enum of the crate, by its Lean name; its declaration is in [`Body::adts`].
    Adt(String),
    Option(Box<Ty>),
    Result(Box<Ty>, Box<Ty>),
    /// A shared reference: the value.
    Ref(Box<Ty>),
    /// A `&mut` reference: the referent's value, written back by who lent it.
    RefMut(Box<Ty>),
    Slice(Box<Ty>),
    Array(Box<Ty>, u64),
    /// `alloc::vec::Vec<T>` in the global allocator: its elements, as a slice's value. A `Vec`
    /// in another allocator is refused: a bounded allocator's push can fail.
    Vec(Box<Ty>),
    /// `core::num::TryFromIntError`, the error of a `TryFrom` between integers.
    TryFromIntError,
    /// `core::time::Duration`: `lean/Blaze/Duration.lean`'s `core.time.Duration`.
    Duration,
    /// `core::cmp::Ordering`: `lean/Blaze/Ordering.lean`'s `core.cmp.Ordering`.
    Ordering,
    /// `str`, behind any reference: its bytes (`lean/Blaze/Str.lean`'s `Str`), with the
    /// UTF-8 invariant not carried.
    Str,
}

impl Ty {
    /// The type under any references.
    pub fn value(&self) -> &Ty {
        match self {
            Ty::Ref(inner) | Ty::RefMut(inner) => inner.value(),
            other => other,
        }
    }

    /// The structs and enums it names, by Lean name, into `out`.
    pub fn adts(&self, out: &mut BTreeSet<String>) {
        match self {
            Ty::Bool
            | Ty::Int(_)
            | Ty::Unit
            | Ty::Never
            | Ty::TryFromIntError
            | Ty::Duration
            | Ty::Ordering
            | Ty::Str => {}
            Ty::Adt(name) => {
                out.insert(name.clone());
            }
            Ty::Tuple(all) => all.iter().for_each(|it| it.adts(out)),
            Ty::Result(ok, err) => {
                ok.adts(out);
                err.adts(out);
            }
            Ty::Option(inner)
            | Ty::Ref(inner)
            | Ty::RefMut(inner)
            | Ty::Slice(inner)
            | Ty::Vec(inner)
            | Ty::Array(inner, _) => inner.adts(out),
        }
    }
}

impl AdtDecl {
    /// The structs and enums its fields name, by Lean name.
    pub fn uses(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let fields: Vec<&Field> = match &self.kind {
            AdtKind::Struct(fields) => fields.iter().collect(),
            AdtKind::Enum(variants) => variants.iter().flat_map(|it| &it.fields).collect(),
        };
        fields.iter().for_each(|it| it.ty.adts(&mut out));
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub ty: Ty,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variant {
    pub name: String,
    pub discriminant: u128,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdtKind {
    Struct(Vec<Field>),
    Enum(Vec<Variant>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdtDecl {
    /// Its Lean name ([`adt_path`]): `krate.module.Name`, and for an instance of a generic
    /// type the [`segment`] of its arguments after it (`krate.Pair.u32`).
    pub name: String,
    /// What it is in Rust, every type by its whole path (`krate::Pair<u32>`): two instances
    /// with one Lean name have two of these, which makes them a refusal, never one.
    pub display: String,
    pub kind: AdtKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Elem {
    Deref,
    /// A tuple's field, of `arity` fields.
    TupleField { index: u32, arity: u32 },
    /// A struct's field, by name.
    Field(String),
    /// A field of a variant of an enum: read only inside the `match` arm of that variant.
    VariantField { variant: u32, index: u32 },
    /// `base[i]`: panics when `i` is out of bounds.
    Index(Local),
    ConstIndex(u64),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Place {
    pub local: Local,
    pub elems: Vec<Elem>,
}

impl Place {
    pub fn local(local: Local) -> Place {
        Place { local, elems: Vec::new() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operand {
    Place(Place),
    /// An integer constant: its bits.
    Int(IntTy, u128),
    Bool(bool),
    Unit,
    /// A string constant (a literal): its bytes.
    Str(Vec<u8>),
    /// A `Duration` constant (`Duration::ZERO`, `Duration::MAX`): its seconds and its
    /// nanoseconds, below `1_000_000_000`.
    Duration { secs: u64, nanos: u32 },
    /// An `Ordering` constant: its variant, `0` `Less`, `1` `Equal`, `2` `Greater`.
    Ordering(u32),
}

/// What an operator works on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpTy {
    Bool,
    Int(IntTy),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitXor,
    BitAnd,
    BitOr,
    Shl,
    Shr,
    Eq,
    Lt,
    Le,
    Ne,
    Ge,
    Gt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rvalue {
    Use(Operand),
    /// `checked`: `+ - * << >>` panic on overflow; else they wrap. `/` and `%` always panic
    /// on a zero divisor and on `MIN / -1`.
    Bin { op: BinOp, ty: OpTy, checked: bool, left: Operand, right: Operand },
    Not { ty: OpTy, of: Operand },
    /// `checked`: `-MIN` panics; else it wraps.
    Neg { ty: IntTy, checked: bool, of: Operand },
    /// `of as to`, between integers or from `bool`.
    Cast { from: OpTy, to: IntTy, of: Operand },
    /// The length of a slice.
    Len(Place),
    /// The discriminant of an enum of this type; only as what a `Branch` after it tests,
    /// which [`fold`] makes a [`Cond::Discriminant`].
    Discriminant(Place, Ty),
    Tuple(Vec<Operand>),
    Struct { adt: String, fields: Vec<(String, Operand)> },
    /// A value of a variant of the enum `ty`.
    Variant { ty: Ty, variant: u32, args: Vec<Operand> },
    Array(Vec<Operand>),
    Repeat(Operand, u64),
    /// The empty `Vec` of this element type: `Vec::new()`, `Vec::with_capacity(n)`, `vec![]`,
    /// `Vec::clear`'s and `mem::take`'s default. The capacity is not observable in the subset.
    EmptyVec(Ty),
    /// Runtime MIR's `(a op b, overflowed)`; [`fold`] makes the pair, its assert and the read
    /// of its `.0` one checked [`Rvalue::Bin`].
    WithOverflow { op: BinOp, ty: IntTy, left: Operand, right: Operand },
}

/// What a failed assert panics for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    Overflow,
    Bounds,
    DivisionByZero,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Callee {
    /// A function of the Lean library standing for one of core's (`Scalar.wrapping_mul`);
    /// `monadic` when it gives a `Result`. `ty`, when there is one, is the integer type it
    /// makes, its first argument (`Scalar.cast .U64 x`): the arguments do not say it.
    /// `lends_first` when its first argument is a `&mut` the model gives back after its
    /// result, as a translated function gives back its `&mut` parameters
    /// (`Option::take`).
    Model { name: &'static str, monadic: bool, ty: Option<IntTy>, lends_first: bool },
    /// A translated function, by its Lean name.
    Function { name: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stmt {
    Assign { place: Place, value: Rvalue, at: String },
    /// An argument of a `&mut` parameter is the place lent, which takes the value the callee
    /// gives back.
    Call { dest: Place, callee: Callee, args: Vec<Operand>, at: String },
    /// Panics unless `cond == expected`.
    Assert { cond: Operand, expected: bool, check: Check, at: String },
}

/// What a `Branch` tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cond {
    /// A `bool` (0 or 1) or an integer's bits.
    Operand(Operand, OpTy),
    /// The discriminant of the enum of this type at the place: a `match`.
    Discriminant(Place, Ty),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalDecl {
    /// The binding the local holds, if it is one.
    pub name: Option<String>,
    pub ty: Ty,
}

/// A function as structured MIR.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Body {
    /// Its Lean name, `krate.module.name`.
    pub name: String,
    /// `krate::module::name`.
    pub display: String,
    /// Local 0 is the return slot.
    pub locals: Vec<LocalDecl>,
    pub params: Vec<Local>,
    /// The declarations its types use, each after those it uses.
    pub adts: Vec<AdtDecl>,
    pub nodes: Vec<Node<Stmt, Cond>>,
}

impl Body {
    pub fn adt(&self, name: &str) -> Option<&AdtDecl> {
        self.adts.iter().find(|it| it.name == name)
    }

    /// The variants of the enum type `ty`: a declared enum's, `Option`'s or `Result`'s.
    pub fn variants(&self, ty: &Ty) -> Option<Vec<Variant>> {
        let field = |ty: &Ty| vec![Field { name: "f0".to_owned(), ty: ty.clone() }];
        let variant = |name: &str, discriminant, fields| Variant {
            name: name.to_owned(),
            discriminant,
            fields,
        };
        match ty {
            Ty::Adt(name) => match &self.adt(name)?.kind {
                AdtKind::Enum(variants) => Some(variants.clone()),
                AdtKind::Struct(_) => None,
            },
            Ty::Option(inner) => {
                Some(vec![variant("none", 0, Vec::new()), variant("some", 1, field(inner))])
            }
            Ty::Result(ok, err) => {
                Some(vec![variant("Ok", 0, field(ok)), variant("Err", 1, field(err))])
            }
            Ty::Ordering => Some(ORDERING.iter().map(|&(name, it)| variant(name, it, Vec::new())).collect()),
            _ => None,
        }
    }
}

/// `core::cmp::Ordering`'s variants and their discriminants as a `match` tests them (an
/// enum's discriminant, sign-extended to 128 bits): `Less = -1`, `Equal = 0`, `Greater = 1`
/// (`library/core/src/cmp.rs:420`, `#[repr(i8)] pub enum Ordering`).
pub(crate) const ORDERING: [(&str, u128); 3] =
    [("Less", -1i128 as u128), ("Equal", 0), ("Greater", 1)];

impl Operand {
    fn reads(&self, out: &mut BTreeSet<Local>) {
        if let Operand::Place(place) = self {
            place.reads(out);
        }
    }
}

impl Place {
    /// The locals a read of the place reads.
    pub fn reads(&self, out: &mut BTreeSet<Local>) {
        out.insert(self.local);
        for elem in &self.elems {
            if let Elem::Index(index) = elem {
                out.insert(*index);
            }
        }
    }

    pub fn indexed_by(&self, index: Local) -> bool {
        self.elems.contains(&Elem::Index(index))
    }
}

impl Rvalue {
    fn operands(&self) -> Vec<&Operand> {
        match self {
            Rvalue::Use(of)
            | Rvalue::Not { of, .. }
            | Rvalue::Neg { of, .. }
            | Rvalue::Cast { of, .. }
            | Rvalue::Repeat(of, _) => vec![of],
            Rvalue::Bin { left, right, .. } | Rvalue::WithOverflow { left, right, .. } => {
                vec![left, right]
            }
            Rvalue::Len(_) | Rvalue::Discriminant(..) | Rvalue::EmptyVec(_) => Vec::new(),
            Rvalue::Tuple(all) | Rvalue::Array(all) | Rvalue::Variant { args: all, .. } => {
                all.iter().collect()
            }
            Rvalue::Struct { fields, .. } => fields.iter().map(|(_, of)| of).collect(),
        }
    }

    fn places(&self) -> Vec<&Place> {
        let mut out: Vec<&Place> = self
            .operands()
            .into_iter()
            .filter_map(|it| match it {
                Operand::Place(place) => Some(place),
                _ => None,
            })
            .collect();
        if let Rvalue::Len(place) | Rvalue::Discriminant(place, _) = self {
            out.push(place);
        }
        out
    }
}

impl Stmt {
    /// The locals the statement reads. A write of a part of a local reads the local.
    pub fn reads(&self, out: &mut BTreeSet<Local>) {
        let written = |place: &Place, out: &mut BTreeSet<Local>| {
            if !place.elems.is_empty() {
                place.reads(out);
            }
        };
        match self {
            Stmt::Assign { place, value, .. } => {
                for read in value.places() {
                    read.reads(out);
                }
                written(place, out);
            }
            Stmt::Call { dest, args, .. } => {
                for arg in args {
                    arg.reads(out);
                }
                written(dest, out);
            }
            Stmt::Assert { cond, .. } => cond.reads(out),
        }
    }

    /// The local the statement gives a whole new value, whatever it held.
    pub fn writes(&self) -> Option<Local> {
        match self {
            Stmt::Assign { place, .. } | Stmt::Call { dest: place, .. } => {
                place.elems.is_empty().then_some(place.local)
            }
            Stmt::Assert { .. } => None,
        }
    }

    /// The locals the statement may change: what it assigns, and what it lends to a call.
    pub fn changes(&self, out: &mut BTreeSet<Local>) {
        match self {
            Stmt::Assign { place, .. } => {
                out.insert(place.local);
            }
            Stmt::Call { dest, args, .. } => {
                out.insert(dest.local);
                for arg in args {
                    if let Operand::Place(place) = arg {
                        out.insert(place.local);
                    }
                }
            }
            Stmt::Assert { .. } => {}
        }
    }

    fn mentions_index(&self, index: Local) -> bool {
        match self {
            Stmt::Assign { place, value, .. } => {
                place.indexed_by(index) || value.places().iter().any(|it| it.indexed_by(index))
            }
            Stmt::Call { dest, args, .. } => {
                dest.indexed_by(index)
                    || args
                        .iter()
                        .any(|it| matches!(it, Operand::Place(place) if place.indexed_by(index)))
            }
            Stmt::Assert { .. } => false,
        }
    }
}

fn is_local(operand: &Operand, local: Local) -> bool {
    matches!(operand, Operand::Place(place) if place.local == local && place.elems.is_empty())
}

fn is_field(operand: &Operand, local: Local, field: u32) -> bool {
    matches!(operand, Operand::Place(place) if place.local == local
        && matches!(place.elems.as_slice(), [Elem::TupleField { index, .. }] if *index == field))
}

/// A `match` on `on` whose rest is only another `match` on the same place is one `match`.
/// MIR building tests a `match`'s arms one after the other (hir-ty-mir's
/// `pattern_matching_variant`: `d = discriminant(p); switch d { v => arm, _ => next }` for
/// each arm), so a `match` of `n` variants is `n` tests nested in each other's rest. The inner
/// test runs only on a discriminant none of the outer arms has, and nothing runs between the
/// two, so an inner arm for a value an outer arm already has is never reached and goes, and
/// the inner rest is the rest of the whole. `arms` and `otherwise` are folded already, so a
/// chain of any length is one `match` by the time it gets here.
fn merge_tests(
    on: &Cond,
    mut arms: Vec<(u128, Vec<Node<Stmt, Cond>>)>,
    mut otherwise: Vec<Node<Stmt, Cond>>,
) -> (Vec<(u128, Vec<Node<Stmt, Cond>>)>, Vec<Node<Stmt, Cond>>) {
    let Cond::Discriminant(place, ty) = on else { return (arms, otherwise) };
    let same = matches!(
        otherwise.as_slice(),
        [Node::Branch { on: Cond::Discriminant(inner, inner_ty), .. }]
            if inner == place && inner_ty == ty
    );
    if !same {
        return (arms, otherwise);
    }
    let Some(Node::Branch { arms: inner_arms, otherwise: rest, .. }) = otherwise.pop() else {
        unreachable!("matched above")
    };
    for (value, arm) in inner_arms {
        if !arms.iter().any(|(it, _)| *it == value) {
            arms.push((value, arm));
        }
    }
    (arms, rest)
}

/// The node `nodes` run first, under any labelled blocks they start with.
fn first_node(nodes: &mut [Node<Stmt, Cond>]) -> Option<&mut Node<Stmt, Cond>> {
    match nodes.first_mut()? {
        Node::Block(body) => first_node(body),
        first => Some(first),
    }
}

/// Folds what runtime MIR spreads over statements back into one operation, and a discriminant
/// read with the switch on it into a `match`: see the module.
pub fn fold(nodes: Vec<Node<Stmt, Cond>>) -> Vec<Node<Stmt, Cond>> {
    let mut out: Vec<Node<Stmt, Cond>> = Vec::with_capacity(nodes.len());
    let mut rest = nodes.into_iter().peekable();
    while let Some(node) = rest.next() {
        let node = match node {
            Node::Branch { on, arms, otherwise } => {
                let arms = arms.into_iter().map(|(value, arm)| (value, fold(arm))).collect();
                let otherwise = fold(otherwise);
                // `d = discriminant(p); switch d` is a match on `p`.
                let matched = match (&on, out.last()) {
                    (
                        Cond::Operand(tested, _),
                        Some(Node::Do(Stmt::Assign {
                            place,
                            value: Rvalue::Discriminant(of, ty),
                            ..
                        })),
                    ) if place.elems.is_empty() && is_local(tested, place.local) => {
                        Some(Cond::Discriminant(of.clone(), ty.clone()))
                    }
                    _ => None,
                };
                match matched {
                    Some(on) => {
                        out.pop();
                        let (arms, otherwise) = merge_tests(&on, arms, otherwise);
                        Node::Branch { on, arms, otherwise }
                    }
                    None => Node::Branch { on, arms, otherwise },
                }
            }
            // `d = discriminant(p); 'a: { switch d { .. } .. }` is a match on `p` too: the
            // label runs nothing, and the normal form puts the read before it (an or-pattern
            // whose alternatives all go on to one arm makes the label).
            Node::Block(body) => {
                let mut body = fold(body);
                let read = match out.last() {
                    Some(Node::Do(Stmt::Assign { place, value: Rvalue::Discriminant(of, ty), .. }))
                        if place.elems.is_empty() =>
                    {
                        Some((place.local, of.clone(), ty.clone()))
                    }
                    _ => None,
                };
                if let Some((local, of, ty)) = read
                    && let Some(Node::Branch { on, arms, otherwise }) = first_node(&mut body)
                    && matches!(on, Cond::Operand(tested, _) if is_local(tested, local))
                {
                    *on = Cond::Discriminant(of, ty);
                    let (merged, rest) =
                        merge_tests(on, std::mem::take(arms), std::mem::take(otherwise));
                    *arms = merged;
                    *otherwise = rest;
                    out.pop();
                }
                Node::Block(body)
            }
            Node::Loop(body) => Node::Loop(fold(body)),
            // `t = a op? b; assert(!t.1); x = t.0` is `x = a op b`, checked.
            Node::Do(Stmt::Assign { place, value: Rvalue::Use(used), at }) => {
                let folded = match out.as_slice() {
                    [
                        ..,
                        Node::Do(Stmt::Assign {
                            place: pair,
                            value: Rvalue::WithOverflow { op, ty, left, right },
                            ..
                        }),
                        Node::Do(Stmt::Assert {
                            cond,
                            expected: false,
                            check: Check::Overflow,
                            ..
                        }),
                    ] if pair.elems.is_empty()
                        && is_field(cond, pair.local, 1)
                        && is_field(&used, pair.local, 0) =>
                    {
                        Some(Rvalue::Bin {
                            op: *op,
                            ty: OpTy::Int(*ty),
                            checked: true,
                            left: left.clone(),
                            right: right.clone(),
                        })
                    }
                    _ => None,
                };
                match folded {
                    Some(value) => {
                        out.pop();
                        out.pop();
                        Node::Do(Stmt::Assign { place, value, at })
                    }
                    None => Node::Do(Stmt::Assign { place, value: Rvalue::Use(used), at }),
                }
            }
            // `len = Len(p); c = i < len; assert(c)` before a statement that indexes by `i`
            // is that index's own check.
            Node::Do(Stmt::Assert { cond, expected: true, check: Check::Bounds, at }) => {
                let index = match out.as_slice() {
                    [
                        ..,
                        Node::Do(Stmt::Assign { place: len, value: Rvalue::Len(_), .. }),
                        Node::Do(Stmt::Assign {
                            place: less,
                            value:
                                Rvalue::Bin {
                                    op: BinOp::Lt,
                                    left: Operand::Place(index),
                                    right,
                                    ..
                                },
                            ..
                        }),
                    ] if len.elems.is_empty()
                        && less.elems.is_empty()
                        && index.elems.is_empty()
                        && is_local(right, len.local)
                        && is_local(&cond, less.local) =>
                    {
                        Some(index.local)
                    }
                    _ => None,
                };
                let indexes = index.is_some_and(|index| {
                    matches!(rest.peek(), Some(Node::Do(next)) if next.mentions_index(index))
                });
                if indexes {
                    out.pop();
                    out.pop();
                    continue;
                }
                Node::Do(Stmt::Assert { cond, expected: true, check: Check::Bounds, at })
            }
            other => other,
        };
        out.push(node);
    }
    out
}

/// What [`from_mir`] gives: the body, and the instances it calls by their Lean names.
pub(crate) struct Lowered<'db> {
    pub(crate) body: Body,
    pub(crate) callees: Vec<(String, Instance<'db>)>,
    /// The structs and enums whose derived `==` the body calls, or that a field of one holds:
    /// each gets a Lean `DecidableEq` instance ([`crate::eq_module`]).
    pub(crate) derives: BTreeSet<String>,
}

/// Why a `&mut` into a `Vec` (`v[i]`, `&mut v[..]`, `as_mut_slice`) is refused when it is not
/// a place: it was stored, or the local holding it was assigned twice.
const UNLENT_VEC: &str = "a `&mut` into a `Vec` that is not a temporary of one borrow (stored, \
    chosen at run time or assigned twice)";

/// Core's panic entry points: a call of one is a panic.
const PANICS: &[&str] = &[
    "panic",
    "panic_fmt",
    "panic_nounwind",
    "panic_explicit",
    "panic_display",
    "panic_str_2015",
    "unreachable_display",
    "panic_bounds_check",
];

/// The inherent methods of core's integers the Lean library models (`lean/Blaze/Scalar.lean`
/// and `lean/Blaze/ScalarOps.lean`), on every integer type that has them: the method, its
/// model, and whether the model gives a `Result` (it panics in Rust). Recognised by the method
/// itself, never by its body, which in `library/core` is mostly an intrinsic.
const INT_MODELS: &[(&str, &str, bool)] = &[
    ("wrapping_add", "Scalar.wrapping_add", false),
    ("wrapping_sub", "Scalar.wrapping_sub", false),
    ("wrapping_mul", "Scalar.wrapping_mul", false),
    ("wrapping_neg", "Scalar.wrapping_neg", false),
    ("wrapping_shl", "Scalar.wrapping_shl", false),
    ("wrapping_shr", "Scalar.wrapping_shr", false),
    ("wrapping_div", "Scalar.wrapping_div", true),
    ("wrapping_rem", "Scalar.wrapping_rem", true),
    ("wrapping_abs", "Scalar.wrapping_abs", false),
    ("wrapping_pow", "Scalar.wrapping_pow", false),
    ("saturating_add", "Scalar.saturating_add", false),
    ("saturating_sub", "Scalar.saturating_sub", false),
    ("saturating_mul", "Scalar.saturating_mul", false),
    ("saturating_pow", "Scalar.saturating_pow", false),
    ("checked_add", "Scalar.checked_add", false),
    ("checked_sub", "Scalar.checked_sub", false),
    ("checked_mul", "Scalar.checked_mul", false),
    ("checked_div", "Scalar.checked_div", false),
    ("checked_rem", "Scalar.checked_rem", false),
    ("checked_neg", "Scalar.checked_neg", false),
    ("checked_shl", "Scalar.checked_shl", false),
    ("checked_shr", "Scalar.checked_shr", false),
    ("checked_pow", "Scalar.checked_pow", false),
    ("overflowing_add", "Scalar.overflowing_add", false),
    ("overflowing_sub", "Scalar.overflowing_sub", false),
    ("overflowing_mul", "Scalar.overflowing_mul", false),
    ("leading_zeros", "Scalar.leading_zeros", false),
    ("trailing_zeros", "Scalar.trailing_zeros", false),
    ("count_ones", "Scalar.count_ones", false),
    ("count_zeros", "Scalar.count_zeros", false),
    ("is_power_of_two", "Scalar.is_power_of_two", false),
    ("reverse_bits", "Scalar.reverse_bits", false),
    ("swap_bytes", "Scalar.swap_bytes", false),
    ("rotate_left", "Scalar.rotate_left", false),
    ("rotate_right", "Scalar.rotate_right", false),
    ("unsigned_abs", "Scalar.unsigned_abs", false),
    ("abs_diff", "Scalar.abs_diff", false),
    ("signum", "Scalar.signum", false),
    ("is_multiple_of", "Scalar.is_multiple_of", false),
    ("to_le_bytes", "Scalar.to_le_bytes", false),
    ("to_be_bytes", "Scalar.to_be_bytes", false),
    ("to_ne_bytes", "Scalar.to_ne_bytes", false),
];

/// The integer methods whose overflow follows the calling crate's overflow checks (core marks
/// them `#[rustc_inherit_overflow_checks]`): the method, its model with the checks on (a
/// `Result`, failing where it panics), and its model without them (total, wrapping).
const INT_INHERITED: &[(&str, &str, &str)] = &[
    ("abs", "Scalar.abs", "Scalar.wrapping_abs"),
    ("pow", "Scalar.pow", "Scalar.wrapping_pow"),
    ("next_power_of_two", "Scalar.next_power_of_two", "Scalar.wrapping_next_power_of_two"),
];

/// The integer functions that make a value of their own type from bytes: the type is the
/// model's first argument. `_ne_` is little-endian: every target blaze translates for is.
const INT_FROM_BYTES: &[(&str, &str)] = &[
    ("from_le_bytes", "Scalar.from_le_bytes"),
    ("from_be_bytes", "Scalar.from_be_bytes"),
    ("from_ne_bytes", "Scalar.from_ne_bytes"),
];

/// The closure-free methods of `Option` the Lean library models (`lean/Blaze/Std.lean`): the
/// method, its model, whether the model gives a `Result`, whether it lends its first
/// argument. `expect`'s message is dropped; `unwrap_or_default` is lowered on its own.
const OPTION_MODELS: &[(&str, &str, bool, bool)] = &[
    ("is_some", "core.option.Option.is_some", false, false),
    ("is_none", "core.option.Option.is_none", false, false),
    ("unwrap", "core.option.Option.unwrap", true, false),
    ("expect", "core.option.Option.expect", true, false),
    ("unwrap_or", "core.option.Option.unwrap_or", false, false),
    ("ok_or", "core.option.Option.ok_or", false, false),
    ("take", "core.option.Option.take", false, true),
    ("replace", "core.option.Option.replace", false, true),
];

/// The closure-free methods of `Result` the Lean library models, as [`OPTION_MODELS`].
const RESULT_MODELS: &[(&str, &str, bool, bool)] = &[
    ("is_ok", "core.result.Result.is_ok", false, false),
    ("is_err", "core.result.Result.is_err", false, false),
    ("ok", "core.result.Result.ok", false, false),
    ("err", "core.result.Result.err", false, false),
    ("unwrap", "core.result.Result.unwrap", true, false),
    ("expect", "core.result.Result.expect", true, false),
    ("unwrap_or", "core.result.Result.unwrap_or", false, false),
];

/// The inherent functions of `core::time::Duration` the Lean library models
/// (`lean/Blaze/Duration.lean`): the function, its model, and whether the model gives a
/// `Result` (it panics in Rust).
const DURATION_MODELS: &[(&str, &str, bool)] = &[
    ("new", "core.time.Duration.new", true),
    ("from_secs", "core.time.Duration.from_secs", false),
    ("from_millis", "core.time.Duration.from_millis", false),
    ("from_micros", "core.time.Duration.from_micros", false),
    ("from_nanos", "core.time.Duration.from_nanos", false),
    ("is_zero", "core.time.Duration.is_zero", false),
    ("as_secs", "core.time.Duration.as_secs", false),
    ("subsec_millis", "core.time.Duration.subsec_millis", false),
    ("subsec_micros", "core.time.Duration.subsec_micros", false),
    ("subsec_nanos", "core.time.Duration.subsec_nanos", false),
    ("as_millis", "core.time.Duration.as_millis", false),
    ("as_micros", "core.time.Duration.as_micros", false),
    ("as_nanos", "core.time.Duration.as_nanos", false),
    ("checked_add", "core.time.Duration.checked_add", false),
    ("checked_sub", "core.time.Duration.checked_sub", false),
    ("saturating_add", "core.time.Duration.saturating_add", false),
    ("saturating_sub", "core.time.Duration.saturating_sub", false),
];

/// The inherent methods of `core::cmp::Ordering` the Lean library models
/// (`lean/Blaze/Ordering.lean`); none panics.
const ORDERING_MODELS: &[(&str, &str)] = &[
    ("is_eq", "core.cmp.Ordering.is_eq"),
    ("is_ne", "core.cmp.Ordering.is_ne"),
    ("is_lt", "core.cmp.Ordering.is_lt"),
    ("is_gt", "core.cmp.Ordering.is_gt"),
    ("is_le", "core.cmp.Ordering.is_le"),
    ("is_ge", "core.cmp.Ordering.is_ge"),
    ("reverse", "core.cmp.Ordering.reverse"),
    ("then", "core.cmp.Ordering.then"),
];

/// The inherent methods of `str` the Lean library models (`lean/Blaze/Str.lean`), on its
/// bytes; none panics.
const STR_MODELS: &[(&str, &str)] =
    &[("len", "Str.len"), ("is_empty", "Str.is_empty"), ("as_bytes", "Str.as_bytes")];

/// A trait whose standard method the model takes as the value's own structure, when the
/// type's impl is the standard library's or derived: `Clone::clone` is the value, `==` is
/// Lean's `=`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Structural {
    Clone,
    PartialEq,
}

/// Where a function of the standard library is, which tells its model.
enum Home {
    /// An inherent method of this integer type.
    Int(IntTy),
    Option,
    Result,
    Slice,
    /// An inherent method of `alloc::vec::Vec`.
    Vec,
    /// An inherent method of `core::time::Duration`.
    Duration,
    /// An inherent method of `core::cmp::Ordering`.
    Ordering,
    /// An inherent method of `str`.
    Str,
    /// A method of a trait or of an impl of one: the trait's name and the module it is in,
    /// under its crate (`["cmp"]`).
    Trait { name: String, module: Vec<String> },
    /// A function of a module, by the module's path under its crate.
    Module(Vec<String>),
    Other,
}

/// The integer type `ty` is, if it is one v0 has.
fn int_ty(ty: HTy<'_>) -> Option<IntTy> {
    Some(match ty.kind() {
        TyKind::Int(int) => match int {
            RIntTy::I8 => IntTy::I8,
            RIntTy::I16 => IntTy::I16,
            RIntTy::I32 => IntTy::I32,
            RIntTy::I64 => IntTy::I64,
            RIntTy::I128 => IntTy::I128,
            RIntTy::Isize => IntTy::Isize,
            RIntTy::Bits(_) => return None,
        },
        TyKind::Uint(uint) => match uint {
            UintTy::U8 => IntTy::U8,
            UintTy::U16 => IntTy::U16,
            UintTy::U32 => IntTy::U32,
            UintTy::U64 => IntTy::U64,
            UintTy::U128 => IntTy::U128,
            UintTy::Usize => IntTy::Usize,
            UintTy::Bits(_) => return None,
        },
        _ => return None,
    })
}

/// Whether `adt` is `alloc::vec::Vec`: the struct `Vec` of the module `vec` of a crate of the
/// language (`alloc`; `core` in a test's minicore). It is no lang item, so its definition's
/// path says what it is.
fn is_vec(db: &Db, adt: AdtId) -> bool {
    let AdtId::StructId(it) = adt else { return false };
    adt.krate(db).data(db).origin.is_lang()
        && StructSignature::of(db, it).name.as_str() == "Vec"
        && module_path(db, adt.module(db))[1..] == ["vec"]
}

fn crate_name(db: &Db, krate: base_db::Crate) -> String {
    krate
        .extra_data(db)
        .display_name
        .as_ref()
        .map(|it| it.crate_name().as_str().to_owned())
        .unwrap_or_else(|| "crate".to_owned())
}

/// The crate's name, then the modules from its root down to `module`.
fn module_path(db: &Db, module: ModuleId) -> Vec<String> {
    let mut segments = Vec::new();
    let mut at = Some(module.to_db(db));
    while let Some(module) = at {
        if let Some(name) = module.name(db) {
            segments.push(name.as_str().to_owned());
        }
        at = module.containing_module(db);
    }
    segments.push(crate_name(db, module.krate(db)));
    segments.reverse();
    segments
}

/// What a generic function asked for alone is: neither translated nor refused, but translated
/// at each instance a theorem reaches (docs/v2/plan/design/proofs-for-sans-io.md, section 3,
/// "Generics are translated by instance"). Its leading words, `a generic function`, are what
/// `blaze prove --subset` counts the line by.
pub const GENERIC_UNASKED: &str =
    "a generic function, translated at each instance a theorem reaches (none is asked for here)";

/// A `#[blaze::theorem]` function with type or const parameters: a theorem's statement is
/// closed, so it is refused, never stated per instance.
pub const GENERIC_THEOREM: &str = "a generic `#[blaze::theorem]` function (a theorem's statement \
    is closed: it states one function, so its type and const parameters are given in it)";

/// How many instances of generic functions one chain of calls may hold: deeper is a function
/// that calls itself, directly or not, at an ever larger type (`f::<T>` calling
/// `f::<Option<T>>`), which no finite set of Lean modules translates, and is refused
/// ([`crate::emit`]). rustc stops such a program at its recursion limit (128) while it
/// instantiates; 32 is far above the chains the tests reach (one instance deep).
pub const INSTANCE_DEPTH: usize = 32;

/// A function at its generic arguments: what is translated (docs/v2/plan/design/
/// proofs-for-sans-io.md, section 3). A theorem function is closed, so everything it reaches
/// is reached at concrete arguments, as in codegen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Instance<'db> {
    pub(crate) func: FunctionId,
    /// `None` for a function with no type or const parameters, its own or its impl's: its body
    /// is its MIR as built, in its own environment. Otherwise every argument (the impl's, then
    /// its own), each concrete: its body is its MIR monomorphized at them
    /// (`HirDatabase::monomorphized_mir_body`), every type substituted before it is modelled
    /// and every callee's arguments with them.
    pub(crate) args: Option<GenericArgs<'db>>,
    /// The crate an instance's calls of a trait's methods are resolved in, with no where
    /// clauses (as codegen resolves them): the crate of the function the chain of instances
    /// began at, which sees every impl its arguments' types have.
    pub(crate) krate: base_db::Crate,
}

impl<'db> Instance<'db> {
    /// `func` itself, at no arguments.
    pub(crate) fn of(db: &Db, func: FunctionId) -> Instance<'db> {
        Instance { func, args: None, krate: func.krate(db) }
    }
}

/// Whether `func` has type or const parameters, its own or its impl's. Lifetimes are dropped:
/// a function generic only over lifetimes is one function.
pub(crate) fn is_generic(db: &Db, func: FunctionId) -> bool {
    if GenericParams::of(db, func.into()).len_type_or_consts() != 0 {
        return true;
    }
    match func.lookup(db).container {
        ItemContainerId::ImplId(imp) => GenericParams::of(db, imp.into()).len_type_or_consts() != 0,
        _ => false,
    }
}

/// The naming of instances: how generic arguments are written in a Lean name, as one name
/// component that is also a file name.
///
/// - A function's instance is its path, then one component of its own arguments: `pick` at
///   `u32` is `krate.pick.u32`, module `BlazeModel.krate.pick.u32`, file
///   `BlazeModel/krate/pick/u32.lean`.
/// - A generic struct or enum's instance is its path, then one component of its arguments
///   (`krate.Pair.u32`, `krate.Pair.Vec_u8`), a Lean structure or inductive type of its own,
///   in the module `<name>.type`; its inherent methods are in its namespace
///   (`krate.Pair.u32.swap`), and a method of a trait impl in the namespace of the type and the
///   trait (`krate.Pair.u32.Shape.area`), with one more component for a trait's arguments other
///   than `Self` (`krate.Big.From.Small.from`).
/// - The component: each type or const argument, joined by `_`; lifetimes are dropped, and a
///   function with no type or const argument has none (so every name of a non-generic item is
///   what it was). A type is written prefix, each head of a fixed arity: an integer, `bool`,
///   `str` and `char` by their names, `()` as `unit`, `!` as `never`, a tuple of `n` as
///   `tuple<n>_..`, `&T` as `ref_T`, `&mut T` as `mut_T`, `[T]` as `slice_T`, `[T; N]` as
///   `array<N>_T`, a struct or enum (the standard library's too) by its own name then its
///   arguments (`Option_u32`; `Vec`'s `Global` allocator is not written), and a const argument
///   as `c` and its value's bits in decimal (`c4`). Only ASCII letters, digits and `_`, never
///   first a digit: a legal file name on Linux and macOS and a Lean identifier.
/// - Injective as far as the names say: a struct or enum is written by its own name, not its
///   path, and a type's name may hold `_`. Two instances that would get one Lean name are a
///   refusal where both are imported, never one replacing the other: each module's text names
///   its instance in Rust with every type by its whole path ([`Named::display`]), so the two
///   texts differ ([`crate::emit`]'s guard, which also refuses two module files that differ
///   only by letter case).
fn segment<'db>(
    db: &'db Db,
    args: impl IntoIterator<Item = GenericArg<'db>>,
) -> Result<Option<String>, String> {
    let mut parts = Vec::new();
    for arg in args {
        if let Some(ty) = arg.ty() {
            parts.push(ty_segment(db, ty)?);
        } else if let Some(konst) = arg.konst() {
            parts.push(format!("c{}", const_value(konst)?));
        }
        // A lifetime is dropped.
    }
    Ok((!parts.is_empty()).then(|| parts.join("_")))
}

/// The value of a const argument, its bits.
fn const_value(konst: HConst<'_>) -> Result<u128, String> {
    let value = match konst.kind() {
        ConstKind::Value(value) => value.try_to_leaf().map(|leaf| leaf.to_bits_unchecked()),
        _ => None,
    };
    value.ok_or_else(|| "a const argument that is not a number".to_owned())
}

/// The arguments of the struct or enum `adt` that are written: all, but `Vec`'s allocator when
/// it is `Global`.
fn written_args<'db>(db: &'db Db, adt: AdtId, args: GenericArgs<'db>) -> Vec<GenericArg<'db>> {
    let global = |arg: &GenericArg<'db>| {
        arg.ty().is_some_and(|ty| {
            matches!(ty.kind(), TyKind::Adt(def, _) if def.def_id().krate(db).data(db).origin.is_lang()
                && matches!(def.def_id(), AdtId::StructId(it) if StructSignature::of(db, it).name.as_str() == "Global"))
        })
    };
    let mut all: Vec<GenericArg<'db>> = args.iter().collect();
    if is_vec(db, adt) && all.len() == 2 && global(&all[1]) {
        all.truncate(1);
    }
    all
}

/// `ty` as one part of a [`segment`].
fn ty_segment<'db>(db: &'db Db, ty: HTy<'db>) -> Result<String, String> {
    if let Some(int) = int_ty(ty) {
        return Ok(format!("{int:?}").to_lowercase());
    }
    let all = |tys: &mut dyn Iterator<Item = HTy<'db>>| -> Result<String, String> {
        Ok(tys.map(|it| ty_segment(db, it)).collect::<Result<Vec<_>, _>>()?.join("_"))
    };
    Ok(match ty.kind() {
        TyKind::Bool => "bool".to_owned(),
        TyKind::Str => "str".to_owned(),
        TyKind::Never => "never".to_owned(),
        TyKind::Char => return Err("the type `char`".to_owned()),
        TyKind::Float(_) => return Err("a floating-point number".to_owned()),
        TyKind::Tuple(fields) if fields.is_empty() => "unit".to_owned(),
        TyKind::Tuple(fields) => format!("tuple{}_{}", fields.len(), all(&mut fields.iter())?),
        TyKind::Ref(_, inner, mutability) => {
            format!("{}_{}", if mutability.is_mut() { "mut" } else { "ref" }, ty_segment(db, inner)?)
        }
        TyKind::Slice(elem) => format!("slice_{}", ty_segment(db, elem)?),
        TyKind::Array(elem, len) => format!("array{}_{}", const_value(len)?, ty_segment(db, elem)?),
        TyKind::Adt(def, args) => {
            let adt = def.def_id();
            let own = adt_own_name(db, adt)?;
            match segment(db, written_args(db, adt, args))? {
                Some(rest) => format!("{own}_{rest}"),
                None => own,
            }
        }
        TyKind::RawPtr(..) => return Err("a raw pointer (rule RawPointer)".to_owned()),
        TyKind::Dynamic(..) => return Err("a trait object (rule DynTrait)".to_owned()),
        TyKind::Param(_) => return Err("a type parameter".to_owned()),
        TyKind::Alias(..) => return Err("an associated or opaque type".to_owned()),
        TyKind::FnDef(..) | TyKind::FnPtr(..) => return Err("a function used as a value".to_owned()),
        TyKind::Closure(..) | TyKind::CoroutineClosure(..) => return Err("a closure".to_owned()),
        TyKind::Coroutine(..) | TyKind::CoroutineWitness(..) => {
            return Err("`async` or a coroutine (rule Async)".to_owned());
        }
        _ => return Err(blaze_query::db::attach(db, || format!("the type `{ty:?}`"))),
    })
}

/// The type and const arguments in `args`, as Rust writes them with every type by its whole
/// path (`<u32, krate::Small>`); empty when there is none.
fn rust_args<'db>(db: &'db Db, args: impl IntoIterator<Item = GenericArg<'db>>) -> String {
    let mut parts = Vec::new();
    for arg in args {
        if let Some(ty) = arg.ty() {
            parts.push(rust_of(db, ty));
        } else if let Some(konst) = arg.konst() {
            parts.push(match const_value(konst) {
                Ok(value) => value.to_string(),
                Err(_) => blaze_query::db::attach(db, || format!("{konst:?}")),
            });
        }
    }
    match parts.is_empty() {
        true => String::new(),
        false => format!("<{}>", parts.join(", ")),
    }
}

/// `ty` as Rust writes it, a struct or enum by its whole path.
fn rust_of<'db>(db: &'db Db, ty: HTy<'db>) -> String {
    if let Some(int) = int_ty(ty) {
        return format!("{int:?}").to_lowercase();
    }
    match ty.kind() {
        TyKind::Bool => "bool".to_owned(),
        TyKind::Str => "str".to_owned(),
        TyKind::Never => "!".to_owned(),
        TyKind::Tuple(fields) if fields.len() == 1 => format!("({},)", rust_of(db, fields.iter().next().expect("one field"))),
        TyKind::Tuple(fields) => {
            format!("({})", fields.iter().map(|it| rust_of(db, it)).collect::<Vec<_>>().join(", "))
        }
        TyKind::Ref(_, inner, mutability) => {
            format!("&{}{}", if mutability.is_mut() { "mut " } else { "" }, rust_of(db, inner))
        }
        TyKind::Slice(elem) => format!("[{}]", rust_of(db, elem)),
        TyKind::Array(elem, len) => match const_value(len) {
            Ok(len) => format!("[{}; {len}]", rust_of(db, elem)),
            Err(_) => blaze_query::db::attach(db, || format!("{ty:?}")),
        },
        TyKind::Adt(def, args) => {
            let adt = def.def_id();
            let mut path = module_path(db, adt.module(db));
            path.push(adt_own_name(db, adt).unwrap_or_else(|it| it));
            format!("{}{}", path.join("::"), rust_args(db, written_args(db, adt, args)))
        }
        _ => blaze_query::db::attach(db, || format!("{ty:?}")),
    }
}

/// The struct or enum `adt`'s own name.
fn adt_own_name(db: &Db, adt: AdtId) -> Result<String, String> {
    match adt {
        AdtId::StructId(it) => Ok(StructSignature::of(db, it).name.as_str().to_owned()),
        AdtId::EnumId(it) => Ok(EnumSignature::of(db, it).name.as_str().to_owned()),
        AdtId::UnionId(_) => Err("a union".to_owned()),
    }
}

/// The struct or enum `adt` at `args`: its Lean name's components (its path, and the
/// [`segment`] of its arguments when it has a type or const one) and what it is in Rust,
/// component by component (`krate`, `Pair<u32>`).
pub(crate) fn adt_path<'db>(
    db: &'db Db,
    adt: AdtId,
    args: GenericArgs<'db>,
) -> Result<(Vec<String>, Vec<String>), String> {
    let own = adt_own_name(db, adt)?;
    let written = written_args(db, adt, args);
    let mut lean = module_path(db, adt.module(db));
    let mut display = lean.clone();
    lean.push(own.clone());
    if let Some(rest) = segment(db, written.iter().copied())? {
        lean.push(rest);
    }
    display.push(format!("{own}{}", rust_args(db, written)));
    Ok((lean, display))
}

/// A function's names: Lean's (`krate.Pair.u32.swap`) and Rust's, every type by its whole path
/// (`krate::Pair<u32>::swap`).
pub(crate) struct Named {
    pub(crate) lean: String,
    pub(crate) display: String,
}

/// The names of `func` at `args` (`None`: itself): its owner's path ([`owner`]: its module, or
/// its type's and trait's), its name, and the [`segment`] of its own arguments (those its impl
/// has are in its type's name). A method lives in its type's namespace (`krate.Counter.bump`),
/// where Lean has already put the type's fields, constructors and what it derives from them; a
/// method of one of those names is `<name>_fn`. `Err` names what is not translated.
pub(crate) fn instance_name<'db>(
    db: &'db Db,
    func: FunctionId,
    args: Option<GenericArgs<'db>>,
) -> Result<Named, String> {
    let name = FunctionSignature::of(db, func).name.as_str().to_owned();
    let (mut lean, mut display, own) = match owner(db, func, args)? {
        Some(Owner { lean, display, taken }) => {
            let own = match taken.contains(&name) || LEAN_DERIVED.contains(&name.as_str()) {
                true => format!("{name}_fn"),
                false => name.clone(),
            };
            (lean, display, own)
        }
        None => {
            let path = module_path(db, func.module(db));
            (path.clone(), path, name.clone())
        }
    };
    lean.push(own);
    let own_args: Vec<GenericArg<'db>> = match args {
        Some(args) => {
            let parent = hir_ty::generics::generics(db, func.into()).len_parent();
            args.iter().skip(parent).collect()
        }
        None => Vec::new(),
    };
    match segment(db, own_args.iter().copied())? {
        Some(rest) => {
            lean.push(rest);
            display.push(format!("{name}::{}", rust_args(db, own_args)));
        }
        None => display.push(name),
    }
    Ok(Named { lean: lean.join("."), display: display.join("::") })
}

/// `func`'s path as Rust writes it, its segments: the crate, the modules, the type (and the
/// trait) for a method or an associated function of an impl, the name. A function refused for
/// its impl is named by its module: the refusal says the rest.
pub(crate) fn function_path(db: &Db, func: FunctionId) -> Vec<String> {
    let mut path = match owner(db, func, None) {
        Ok(Some(owner)) => owner.display,
        Ok(None) | Err(_) => module_path(db, func.module(db)),
    };
    path.push(FunctionSignature::of(db, func).name.as_str().to_owned());
    path
}

/// The names Lean declares in the namespace of every structure and inductive type.
const LEAN_DERIVED: &[&str] = &[
    "mk", "rec", "recOn", "casesOn", "noConfusion", "noConfusionType", "below", "brecOn",
    "binductionOn", "ibelow", "sizeOf", "ctorIdx", "toCtorIdx", "ctorElim", "inj", "injEq", "ext",
    "ext_iff",
];

/// The namespace a method's Lean name is in, what it is in Rust, and the names Lean's
/// declarations have already put there, which a method of the same name must not take.
struct Owner {
    lean: Vec<String>,
    display: Vec<String>,
    taken: BTreeSet<String>,
}

/// The namespace a method's Lean name is in ([`Owner`]): for a method of an inherent impl the
/// type's ([`adt_path`]: `krate.Counter`, `krate.Pair.u32`), whose fields or variants are
/// taken; for a method of a trait impl the type's and the trait's name (`krate.Square.Shape`),
/// so two traits with a method of one name on one type are two namespaces, and a component of
/// the trait's arguments other than `Self` when it has any (`krate.Big.From.Small`). The impl
/// is at `args`, the method's (the impl's first); with `None`, an impl with type or const
/// parameters is a generic function not asked for at an instance. Two traits of one name from
/// two modules on one type are refused where both are imported ([`crate::emit`]). `None` for a
/// function of a module; `Err` names the kind of function that is not translated.
fn owner<'db>(
    db: &'db Db,
    func: FunctionId,
    args: Option<GenericArgs<'db>>,
) -> Result<Option<Owner>, String> {
    let imp = match func.lookup(db).container {
        ItemContainerId::ModuleId(_) => return Ok(None),
        ItemContainerId::ImplId(imp) => imp,
        ItemContainerId::ExternBlockId(_) => {
            return Err("a function without a body (rule Ffi)".to_owned());
        }
        // A trait's own method: with a body, it is the trait's default, written once for every
        // `Self`, which is generic over `Self` as a generic function is over its parameters.
        _ => {
            return Err("a method of a trait (a default body, generic over `Self`; only an \
                        impl's method is translated)"
                .to_owned());
        }
    };
    blaze_query::db::attach(db, || {
        let interner = DbInterner::new_with(db, func.krate(db));
        let generic_impl = GenericParams::of(db, imp.into()).len_type_or_consts() != 0;
        let (self_ty, of) = match args {
            Some(args) => (
                db.impl_self_ty(imp).instantiate(interner, args).skip_norm_wip(),
                db.impl_trait(imp).map(|it| it.instantiate(interner, args).skip_norm_wip()),
            ),
            None if generic_impl => return Err(GENERIC_UNASKED.to_owned()),
            None => (db.impl_self_ty(imp).skip_binder(), db.impl_trait(imp).map(|it| it.skip_binder())),
        };
        if let Some(of) = of {
            return trait_impl_owner(db, imp, of, self_ty).map(Some);
        }
        let TyKind::Adt(def, adt_args) = self_ty.kind() else {
            return Err("a method of a type that is not a struct or an enum".to_owned());
        };
        let adt = def.def_id();
        let mut taken = BTreeSet::new();
        match adt {
            AdtId::StructId(it) => {
                let data = VariantId::StructId(it).fields(db);
                let positional = matches!(data.shape, FieldsShape::Tuple);
                for (at, (_, field)) in data.fields().iter().enumerate() {
                    taken.insert(match positional {
                        true => format!("f{at}"),
                        false => field.name.as_str().to_owned(),
                    });
                }
            }
            AdtId::EnumId(it) => {
                for (variant, _) in it.enum_variants(db).variants.iter() {
                    taken.insert(variant.as_str().to_owned());
                }
            }
            AdtId::UnionId(_) => return Err("a method of a union".to_owned()),
        }
        let (lean, display) = adt_path(db, adt, adt_args)?;
        Ok(Some(Owner { lean, display, taken }))
    })
}

/// [`owner`] of a method of the impl `imp` of the trait `of` for `self_ty`, both at the
/// instance: refused unless `self_ty` is a struct or enum, an integer or `bool`.
fn trait_impl_owner<'db>(
    db: &'db Db,
    imp: ImplId,
    of: TraitRef<'db>,
    self_ty: HTy<'db>,
) -> Result<Owner, String> {
    let trait_id = of.def_id.0;
    let trait_name = TraitSignature::of(db, trait_id).name.as_str().to_owned();
    // The trait's arguments other than `Self`, lifetimes dropped: none written when each is
    // `Self` (`impl PartialEq for Point`), as an impl of a trait without arguments is named.
    let rest: Vec<GenericArg<'db>> = of.args.iter().skip(1).filter(|it| it.region().is_none()).collect();
    let selfish = rest.iter().all(|arg| arg.ty() == Some(self_ty));
    let (mut lean, mut display) = match self_ty.kind() {
        TyKind::Adt(def, args) => {
            let adt = def.def_id();
            if matches!(adt, AdtId::UnionId(_)) {
                return Err(format!("a method of a trait impl of `{trait_name}` for a union"));
            }
            adt_path(db, adt, args)?
        }
        // A primitive has no module: its impls are named in the impl's.
        TyKind::Bool => {
            let mut path = module_path(db, imp.module(db));
            path.push("bool".to_owned());
            (path.clone(), path)
        }
        _ if int_ty(self_ty).is_some() => {
            let mut path = module_path(db, imp.module(db));
            path.push(format!("{:?}", int_ty(self_ty).expect("an integer")).to_lowercase());
            (path.clone(), path)
        }
        _ => {
            return Err(format!(
                "a method of a trait impl of `{trait_name}` for a type that is not a struct, an \
                 enum, an integer or `bool`"
            ));
        }
    };
    lean.push(trait_name.clone());
    match selfish {
        true => display.push(trait_name),
        false => {
            if let Some(rest_segment) = segment(db, rest.iter().copied())? {
                lean.push(rest_segment);
            }
            display.push(format!("{trait_name}{}", rust_args(db, rest)));
        }
    }
    // Nothing is declared in `<type>.<trait>` but the impl's methods.
    Ok(Owner { lean, display, taken: BTreeSet::new() })
}

fn index(local: m::LocalId) -> Local {
    local.into_raw().into_u32()
}

/// What a `&mut` temporary stands for: a place and, for the `&mut T` that `IndexMut::index_mut`
/// gives of a `Vec`, the local the index is copied into at that call, so the temporary is the
/// place `v[i]` with `i` as it was when it was borrowed.
#[derive(Clone, Debug)]
struct Alias {
    place: m::StoredPlace,
    index: Option<Local>,
}

/// One step of a place being resolved through the `&mut` temporaries it names: MIR's, or the
/// index an [`Alias`] adds.
#[derive(Clone, Copy, Debug)]
enum Proj {
    Mir(m::PlaceElem),
    Index(Local),
}

/// A call of a function of the standard library, as [`Cx::std_call`] reads it: the function,
/// its generic arguments, its name and where it is.
struct Std<'a, 'db> {
    callee: FunctionId,
    generics: GenericArgs<'db>,
    name: &'a str,
    container: ItemContainerId,
}

struct Cx<'db> {
    db: &'db Db,
    /// Where its calls of a trait's methods are resolved: a function's own environment and
    /// crate, or for an instance no where clauses and the crate its chain began at
    /// ([`Instance::krate`]).
    env: ParamEnvAndCrate<'db>,
    interner: DbInterner<'db>,
    display: String,
    mir: &'db m::MirBody<'db>,
    store: &'db ExpressionStore,
    locals: Vec<LocalDecl>,
    /// A `&mut` temporary and the place it stands for.
    alias: BTreeMap<Local, Alias>,
    adts: Vec<AdtDecl>,
    callees: Vec<(String, Instance<'db>)>,
    checks_overflow: bool,
    /// The temporaries and bindings of a type outside the subset, and why: each is refused
    /// where a place names it (with the statement or call that does), not where it is declared,
    /// so one the model never reads (`expect`'s message) is no refusal.
    unusable: BTreeMap<Local, String>,
    /// `core::time::Duration`, once a type names it: its layout says where a constant's
    /// fields are.
    duration: Option<AdtId>,
    /// The crate's structs and enums whose `==` the model takes as Lean's `=`, by Lean name:
    /// each derives `PartialEq`, as do the types its fields hold (see [`Cx::structural`]).
    derives: BTreeSet<String>,
}

type Refused<T> = Result<T, NotTranslated>;

impl<'db> Cx<'db> {
    fn refuse(&self, what: impl Into<String>, at: &str) -> NotTranslated {
        NotTranslated { function: self.display.clone(), what: what.into(), at: at.to_owned() }
    }

    /// Where `span` is, as the source says it.
    fn at(&self, span: m::MirSpan) -> String {
        match span {
            m::MirSpan::ExprId(expr) | m::MirSpan::ExprEnd(expr) => print_expr_hir(
                self.db,
                self.store,
                self.mir.owner.expression_store_owner(self.db),
                expr,
                syntax::Edition::CURRENT,
            ),
            m::MirSpan::SelfParam => "self".to_owned(),
            _ => format!("fn {}", self.display),
        }
    }

    /// `ty` in v0's types. A `&mut` is taken only at the `top` of a local's type: one inside
    /// a tuple or a struct needs a backward function per lifetime, which v0 does not make.
    fn ty(&mut self, ty: HTy<'db>, top: bool, at: &str) -> Refused<Ty> {
        Ok(match ty.kind() {
            TyKind::Bool => Ty::Bool,
            TyKind::Never => Ty::Never,
            TyKind::Int(int) => Ty::Int(match int {
                RIntTy::I8 => IntTy::I8,
                RIntTy::I16 => IntTy::I16,
                RIntTy::I32 => IntTy::I32,
                RIntTy::I64 => IntTy::I64,
                RIntTy::I128 => IntTy::I128,
                RIntTy::Isize => IntTy::Isize,
                RIntTy::Bits(_) => return Err(self.refuse("an arbitrary-width integer type", at)),
            }),
            TyKind::Uint(uint) => Ty::Int(match uint {
                UintTy::U8 => IntTy::U8,
                UintTy::U16 => IntTy::U16,
                UintTy::U32 => IntTy::U32,
                UintTy::U64 => IntTy::U64,
                UintTy::U128 => IntTy::U128,
                UintTy::Usize => IntTy::Usize,
                UintTy::Bits(_) => return Err(self.refuse("an arbitrary-width integer type", at)),
            }),
            TyKind::Tuple(fields) => {
                let mut tys = Vec::with_capacity(fields.len());
                for field in fields.iter() {
                    tys.push(self.ty(field, false, at)?);
                }
                if tys.is_empty() { Ty::Unit } else { Ty::Tuple(tys) }
            }
            TyKind::Ref(_, inner, mutability) => {
                let is_mut = mutability.is_mut();
                if is_mut && !top {
                    return Err(self.refuse(
                        "a `&mut` reference inside another type (rule NestedBorrow)",
                        at,
                    ));
                }
                let inner = Box::new(self.ty(inner, false, at)?);
                if is_mut { Ty::RefMut(inner) } else { Ty::Ref(inner) }
            }
            TyKind::Slice(elem) => Ty::Slice(Box::new(self.ty(elem, false, at)?)),
            TyKind::Array(elem, len) => {
                let elem = Box::new(self.ty(elem, false, at)?);
                let len = match len.kind() {
                    ConstKind::Value(value) => {
                        value.try_to_leaf().map(|leaf| leaf.to_bits_unchecked())
                    }
                    _ => None,
                };
                match len {
                    Some(len) => Ty::Array(elem, len as u64),
                    None => return Err(self.refuse("an array whose length is not a number", at)),
                }
            }
            TyKind::Adt(def, args) => {
                let adt = def.def_id();
                if def.is_box() {
                    return Err(self.refuse("the type `Box` (a heap allocation)", at));
                }
                if adt.krate(self.db).data(self.db).origin.is_lang() {
                    let lang = self.interner.lang_items();
                    let (global, full) = (lang.GlobalAlloc, lang.RangeFull);
                    let is_global =
                        |it: AdtId| matches!(it, AdtId::StructId(it) if Some(it) == global);
                    if is_vec(self.db, adt) && args.len() == 2 {
                        let in_global = matches!(args.type_at(1).kind(), TyKind::Adt(alloc, _)
                            if is_global(alloc.def_id()));
                        if !in_global {
                            return Err(self.refuse(
                                "a `Vec` in an allocator other than `Global` (its push can fail)",
                                at,
                            ));
                        }
                        return Ok(Ty::Vec(Box::new(self.ty(args.type_at(0), false, at)?)));
                    }
                    // `alloc::alloc::Global`, the global allocator, what `new_in` takes, and
                    // `RangeFull`, the `..` of `&v[..]`: units, which say nothing at run time.
                    if is_global(adt) || matches!(adt, AdtId::StructId(it) if Some(it) == full) {
                        return Ok(Ty::Unit);
                    }
                    let name = match adt {
                        AdtId::EnumId(it) => EnumSignature::of(self.db, it).name.as_str(),
                        AdtId::StructId(it) => StructSignature::of(self.db, it).name.as_str(),
                        AdtId::UnionId(_) => "a union",
                    };
                    let module = module_path(self.db, adt.module(self.db)).split_off(1);
                    match (name, module.as_slice()) {
                        ("Duration", [time]) if time == "time" => {
                            self.duration = Some(adt);
                            return Ok(Ty::Duration);
                        }
                        ("Ordering", [cmp]) if cmp == "cmp" => return Ok(Ty::Ordering),
                        _ => {}
                    }
                    return match (name, args.len()) {
                        ("Option", 1) => {
                            Ok(Ty::Option(Box::new(self.ty(args.type_at(0), false, at)?)))
                        }
                        ("Result", 2) => Ok(Ty::Result(
                            Box::new(self.ty(args.type_at(0), false, at)?),
                            Box::new(self.ty(args.type_at(1), false, at)?),
                        )),
                        ("TryFromIntError", 0) => Ok(Ty::TryFromIntError),
                        _ => Err(self.refuse(
                            format!(
                                "the standard library's type `{name}`, which has no model \
                                 (rule NoModel)"
                            ),
                            at,
                        )),
                    };
                }
                if def.destructor(self.interner).is_some() {
                    return Err(self.refuse("a type with a destructor (`impl Drop`)", at));
                }
                // A generic struct or enum at its arguments is a type of its own.
                Ty::Adt(self.adt(adt, args, at)?)
            }
            TyKind::RawPtr(..) => return Err(self.refuse("a raw pointer (rule RawPointer)", at)),
            TyKind::Float(_) => return Err(self.refuse("a floating-point number", at)),
            TyKind::Char => return Err(self.refuse("the type `char`", at)),
            TyKind::Str => Ty::Str,
            TyKind::Dynamic(..) => return Err(self.refuse("a trait object (rule DynTrait)", at)),
            TyKind::Param(_) => return Err(self.refuse("a type parameter", at)),
            TyKind::Alias(..) => return Err(self.refuse("an associated or opaque type", at)),
            TyKind::FnDef(..) | TyKind::FnPtr(..) => {
                return Err(self.refuse("a function used as a value", at));
            }
            TyKind::Closure(..) | TyKind::CoroutineClosure(..) => {
                return Err(self.refuse("a closure", at));
            }
            TyKind::Coroutine(..) | TyKind::CoroutineWitness(..) => {
                return Err(self.refuse("`async` or a coroutine (rule Async)", at));
            }
            _ => {
                let what = blaze_query::db::attach(self.db, || format!("the type `{ty:?}`"));
                return Err(self.refuse(what, at));
            }
        })
    }

    /// The Lean name of the crate's struct or enum `adt` at `args` ([`adt_path`]), declared on
    /// first use with each field's type at `args`. Two instances with one Lean name are
    /// refused, never one declared for both.
    fn adt(&mut self, adt: AdtId, args: GenericArgs<'db>, at: &str) -> Refused<String> {
        let (name, display) = match adt_path(self.db, adt, args) {
            Ok((lean, display)) => (lean.join("."), display.join("::")),
            Err(why) => return Err(self.refuse(why, at)),
        };
        if let Some(it) = self.adts.iter().find(|it| it.name == name) {
            if it.display != display {
                return Err(self.refuse(
                    format!("two types with one Lean name, `{name}` (`{}` and `{display}`)", it.display),
                    at,
                ));
            }
            return Ok(name);
        }
        let kind = match adt {
            AdtId::StructId(it) => AdtKind::Struct(self.fields(VariantId::StructId(it), args, at)?),
            AdtId::EnumId(it) => {
                let mut variants = Vec::new();
                for (variant_name, &(variant, _)) in it.enum_variants(self.db).variants.iter() {
                    let fields = self.fields(VariantId::EnumVariantId(variant), args, at)?;
                    let discriminant = match self.db.const_eval_discriminant(variant) {
                        Ok(value) => value as u128,
                        Err(_) => {
                            return Err(self.refuse(
                                format!(
                                    "the discriminant of `{}`, which did not evaluate",
                                    variant_name.as_str()
                                ),
                                at,
                            ));
                        }
                    };
                    variants.push(Variant {
                        name: variant_name.as_str().to_owned(),
                        discriminant,
                        fields,
                    });
                }
                AdtKind::Enum(variants)
            }
            AdtId::UnionId(_) => return Err(self.refuse("a union", at)),
        };
        // A type that holds itself was refused above: its field's declaration is not there yet
        // and would be asked for again, without end; the depth of a body's types bounds this.
        self.adts.push(AdtDecl { name: name.clone(), display, kind });
        Ok(name)
    }

    /// The fields of `variant` of a struct or enum at `args`; a positional field `i` is named
    /// `f{i}`.
    fn fields(&mut self, variant: VariantId, args: GenericArgs<'db>, at: &str) -> Refused<Vec<Field>> {
        let db = self.db;
        let data = variant.fields(db);
        let positional = matches!(data.shape, FieldsShape::Tuple);
        let mut out = Vec::new();
        for (at_field, (local, field)) in db.field_types(variant).iter().enumerate() {
            let field_ty = field.ty().instantiate(self.interner, args).skip_norm_wip();
            // A field typed by an associated type of a parameter (`T::Out`) is that alias at
            // the instance: it is what the trait solver normalizes it to, as codegen's layout
            // has it; one the solver cannot normalize is refused.
            let field_ty = if field_ty.has_aliases() {
                let infcx = self.interner.infer_ctxt().build(TypingMode::PostAnalysis);
                let ocx = ObligationCtxt::new(&infcx);
                match ocx.deeply_normalize(&ObligationCause::dummy(), self.env.param_env, field_ty) {
                    Ok(normalized) if !normalized.has_aliases() && !normalized.has_infer() => normalized,
                    _ => {
                        let shown = blaze_query::db::attach(db, || format!("{field_ty:?}"));
                        return Err(self.refuse(
                            format!(
                                "a field whose type is an associated type of a type argument \
                                 (`{shown}`) that the trait solver does not normalize at the \
                                 instance"
                            ),
                            at,
                        ));
                    }
                }
            } else {
                field_ty
            };
            let ty = self.ty(field_ty, false, at)?;
            let name = if positional {
                format!("f{at_field}")
            } else {
                data.fields()[local].name.as_str().to_owned()
            };
            out.push(Field { name, ty });
        }
        Ok(out)
    }

    /// The enum type `ty`'s variants, as [`Body::variants`].
    fn variants(&self, ty: &Ty) -> Option<Vec<Variant>> {
        let probe = Body {
            name: String::new(),
            display: String::new(),
            locals: Vec::new(),
            params: Vec::new(),
            adts: self.adts.clone(),
            nodes: Vec::new(),
        };
        probe.variants(ty)
    }

    /// The place `stored` is, a `&mut` temporary in it replaced by what it borrows, and its
    /// type.
    fn place(&self, stored: &m::StoredPlace, at: &str) -> Refused<(Place, Ty)> {
        let mut local = index(stored.local);
        let mut elems: Vec<Proj> =
            stored.projection.as_slice().iter().map(|it| Proj::Mir(*it)).collect();
        // Each hop goes to a local assigned before, so the locals bound the hops.
        for _ in 0..=self.locals.len() {
            let Some(target) = self.alias.get(&local) else { break };
            let rest: &[Proj] = match elems.split_first() {
                Some((Proj::Mir(m::ProjectionElem::Deref), rest)) => rest,
                None => &[],
                Some(_) => return Err(self.refuse("a projection of a `&mut` reference", at)),
            };
            let mut next: Vec<Proj> =
                target.place.projection.as_slice().iter().map(|it| Proj::Mir(*it)).collect();
            next.extend(target.index.map(Proj::Index));
            next.extend_from_slice(rest);
            local = index(target.place.local);
            elems = next;
        }
        if let Some(what) = self.unusable.get(&local) {
            return Err(self.refuse(what.clone(), at));
        }
        let mut ty = self.locals[local as usize].ty.clone();
        let mut out = Vec::with_capacity(elems.len());
        let mut at_elem = 0;
        while at_elem < elems.len() {
            let elem = match elems[at_elem] {
                Proj::Mir(elem) => elem,
                Proj::Index(by) => {
                    let (Ty::Vec(elem) | Ty::Slice(elem)) = &ty else {
                        return Err(self.refuse(format!("an index of a `{ty:?}`"), at));
                    };
                    let elem = (**elem).clone();
                    out.push(Elem::Index(by));
                    at_elem += 1;
                    ty = elem;
                    continue;
                }
            };
            at_elem += 1;
            let (elem, next) = match (elem, &ty) {
                (m::ProjectionElem::Deref, Ty::Ref(inner) | Ty::RefMut(inner)) => {
                    (Elem::Deref, (**inner).clone())
                }
                (m::ProjectionElem::Field(field), Ty::Tuple(fields)) => {
                    let Some(field_ty) = fields.get(field.0 as usize) else {
                        return Err(self.refuse("a field a tuple does not have", at));
                    };
                    (
                        Elem::TupleField { index: field.0, arity: fields.len() as u32 },
                        field_ty.clone(),
                    )
                }
                (m::ProjectionElem::Field(field), Ty::Adt(name)) => {
                    let found = self.adts.iter().find(|it| it.name == *name).and_then(|it| {
                        match &it.kind {
                            AdtKind::Struct(fields) => fields.get(field.0 as usize),
                            AdtKind::Enum(_) => None,
                        }
                    });
                    let Some(found) = found else {
                        return Err(self.refuse("a field of a value that is not a struct", at));
                    };
                    (Elem::Field(found.name.clone()), found.ty.clone())
                }
                (m::ProjectionElem::Downcast(VariantId::EnumVariantId(variant)), _) => {
                    let Some(Proj::Mir(m::ProjectionElem::Field(field))) = elems.get(at_elem).copied()
                    else {
                        return Err(self.refuse("a downcast not followed by a field", at));
                    };
                    at_elem += 1;
                    let variant = variant.index(self.db) as u32;
                    let found = self.variants(&ty).and_then(|all| {
                        all.get(variant as usize)
                            .and_then(|it| it.fields.get(field.0 as usize).cloned())
                    });
                    let Some(found) = found else {
                        return Err(self.refuse("a field of a variant of a value that is not an enum", at));
                    };
                    (Elem::VariantField { variant, index: field.0 }, found.ty)
                }
                // A `Vec` is indexed here when a `&mut` into it (`as_mut_slice`) stands for it.
                (
                    m::ProjectionElem::Index(by),
                    Ty::Slice(elem) | Ty::Vec(elem) | Ty::Array(elem, _),
                ) => (Elem::Index(index(by)), (**elem).clone()),
                (
                    m::ProjectionElem::ConstantIndex { offset, from_end: false, .. },
                    Ty::Slice(elem) | Ty::Vec(elem) | Ty::Array(elem, _),
                ) => (Elem::ConstIndex(offset), (**elem).clone()),
                (m::ProjectionElem::ConstantIndex { .. } | m::ProjectionElem::Subslice { .. }, _) => {
                    return Err(self.refuse("a slice pattern", at));
                }
                (m::ProjectionElem::ReflectedField(_), _) => {
                    return Err(self.refuse("a reflected field (`for const`)", at));
                }
                (other, _) => {
                    return Err(self.refuse(format!("the projection `{other:?}` of a `{ty:?}`"), at));
                }
            };
            out.push(elem);
            ty = next;
        }
        Ok((Place { local, elems: out }, ty))
    }

    fn operand(&mut self, operand: &m::Operand, at: &str) -> Refused<(Operand, Ty)> {
        match &operand.kind {
            m::OperandKind::Copy(place) | m::OperandKind::Move(place) => {
                let (place, ty) = self.place(place, at)?;
                Ok((Operand::Place(place), ty))
            }
            m::OperandKind::Constant { konst, ty } => {
                if operand.fn_def().is_some() {
                    return Err(self.refuse("a function used as a value", at));
                }
                let ty = self.ty(ty.as_ref(), false, at)?;
                let constant = match konst.as_ref().kind() {
                    ConstKind::Value(value) => match value.try_to_leaf() {
                        Some(leaf) => self.scalar_constant(&ty, Some(leaf.to_bits_unchecked()), at)?,
                        None => self.scalar_constant(&ty, None, at)?,
                    },
                    ConstKind::Alias(_, alias) => {
                        let allocation = self.named_const(alias, at)?;
                        self.constant(&ty, &allocation.memory, &allocation.memory_map, at)?
                    }
                    _ => self.scalar_constant(&ty, None, at)?,
                };
                Ok((constant, ty))
            }
            m::OperandKind::Allocation { allocation } => {
                if operand.fn_def().is_some() {
                    return Err(self.refuse("a function used as a value", at));
                }
                let allocation = allocation.as_ref();
                let ty = self.ty(allocation.ty, false, at)?;
                let constant = self.constant(&ty, &allocation.memory, &allocation.memory_map, at)?;
                Ok((constant, ty))
            }
            m::OperandKind::Static(_) => Err(self.refuse("a static (rule Ffi)", at)),
        }
    }

    /// A constant of type `ty` that is its `bits`: `()`, a `bool` or an integer.
    fn scalar_constant(&self, ty: &Ty, bits: Option<u128>, at: &str) -> Refused<Operand> {
        Ok(match (ty, bits) {
            (Ty::Unit, _) => Operand::Unit,
            (Ty::Bool, Some(bits)) => Operand::Bool(bits != 0),
            (Ty::Int(int), Some(bits)) => Operand::Int(*int, bits),
            _ => return Err(self.refuse(format!("a constant of type `{ty:?}`"), at)),
        })
    }

    /// The constant of type `ty` whose bytes are `memory`, what it points to in `map`: a
    /// scalar ([`Cx::scalar_constant`]), a string, a `Duration` or an `Ordering`. Every target
    /// blaze translates for is little-endian with 8-byte pointers.
    fn constant(&self, ty: &Ty, memory: &[u8], map: &MemoryMap<'_>, at: &str) -> Refused<Operand> {
        let word = |at: usize| -> Option<u64> {
            Some(u64::from_le_bytes(memory.get(at..at + 8)?.try_into().ok()?))
        };
        match ty {
            // `&str`: a pointer to the bytes and their length.
            Ty::Ref(inner) if **inner == Ty::Str => {
                let (Some(addr), Some(len)) = (word(0), word(8)) else {
                    return Err(self.refuse("a string constant that is not a pointer and a length", at));
                };
                let bytes = match len {
                    0 => Vec::new(),
                    _ => match map.get(addr as usize, len as usize) {
                        Some(bytes) => bytes.to_vec(),
                        None => {
                            return Err(self.refuse("a string constant whose bytes are not in its memory", at));
                        }
                    },
                };
                Ok(Operand::Str(bytes))
            }
            Ty::Duration => {
                let (secs, nanos) = self.duration_fields(at)?;
                let secs = memory.get(secs..secs + 8).and_then(|it| it.try_into().ok()).map(u64::from_le_bytes);
                let nanos =
                    memory.get(nanos..nanos + 4).and_then(|it| it.try_into().ok()).map(u32::from_le_bytes);
                match (secs, nanos) {
                    (Some(secs), Some(nanos)) if nanos < 1_000_000_000 => Ok(Operand::Duration { secs, nanos }),
                    _ => Err(self.refuse("a `Duration` constant whose nanoseconds are not below a second", at)),
                }
            }
            Ty::Ordering => {
                // The discriminant, sign-extended from its width (`i8` in core).
                let Some(&last) = memory.last() else {
                    return Err(self.refuse("an `Ordering` constant without bytes", at));
                };
                let mut bytes = [if last & 0x80 != 0 { 0xff } else { 0 }; 16];
                if memory.len() > 16 {
                    return Err(self.refuse("an `Ordering` constant wider than 16 bytes", at));
                }
                bytes[..memory.len()].copy_from_slice(memory);
                let raw = u128::from_le_bytes(bytes);
                match ORDERING.iter().position(|it| it.1 == raw) {
                    Some(variant) => Ok(Operand::Ordering(variant as u32)),
                    None => Err(self.refuse("an `Ordering` constant that is no variant", at)),
                }
            }
            _ => {
                let bits = (memory.len() <= 16).then(|| {
                    let mut bytes = [0u8; 16];
                    bytes[..memory.len()].copy_from_slice(memory);
                    u128::from_le_bytes(bytes)
                });
                self.scalar_constant(ty, bits, at)
            }
        }
    }

    /// Where `Duration`'s fields are in its bytes: `secs`, a `u64`, then `nanos`, a `u32`
    /// (`library/core/src/time.rs:81`; core's `nanos` is a `Nanoseconds(u32)`, whose bytes are
    /// its `u32`'s), by the struct's layout.
    fn duration_fields(&self, at: &str) -> Refused<(usize, usize)> {
        let db = self.db;
        let Some(adt) = self.duration else {
            return Err(self.refuse("a `Duration` constant before its type", at));
        };
        let env = ParamEnvAndCrate { param_env: db.trait_environment(adt.into()), krate: adt.krate(db) };
        let Ok(layout) = db.layout_of_adt(adt, GenericArgs::empty().store(), env.store()) else {
            return Err(self.refuse("a `Duration` constant, whose type has no layout", at));
        };
        let offset = |field: usize| layout.fields.offset(field).bytes() as usize;
        Ok((offset(0), offset(1)))
    }

    /// The allocation of the named constant `alias` (`u32::MAX`, `Duration::ZERO`, a crate's
    /// `const`), by evaluating it in its own environment.
    fn named_const(
        &self,
        alias: hir_ty::next_solver::AliasConst<'db>,
        at: &str,
    ) -> Refused<hir_ty::next_solver::Allocation<'db>> {
        let id = match alias.kind {
            AliasConstKind::Free { def_id: FreeConstAliasId(id) }
            | AliasConstKind::InherentImpl { def_id: InherentAssocConstId(id) } => id,
            AliasConstKind::Projection { .. } => {
                return Err(self.refuse("a trait's associated constant", at));
            }
            AliasConstKind::Anon { .. } => {
                return Err(self.refuse("an anonymous constant (`const { .. }`)", at));
            }
            AliasConstKind::InherentSelf { .. } => {
                return Err(self.refuse("an inherent constant named through `Self`", at));
            }
        };
        match self.db.const_eval(id, alias.args, None) {
            Ok(allocation) => Ok(allocation),
            Err(_) => Err(self.refuse("a constant that does not evaluate", at)),
        }
    }

    fn op_ty(&self, ty: &Ty, at: &str) -> Refused<OpTy> {
        match ty.value() {
            Ty::Bool => Ok(OpTy::Bool),
            Ty::Int(int) => Ok(OpTy::Int(*int)),
            other => Err(self.refuse(format!("an operator on a `{other:?}`"), at)),
        }
    }

    /// `rvalue` assigned to a place of type `dest`; `None` when it is the borrow a `&mut`
    /// temporary stands for.
    fn rvalue(&mut self, rvalue: &m::Rvalue, dest: &Ty, at: &str) -> Refused<Option<Rvalue>> {
        Ok(Some(match rvalue {
            m::Rvalue::Use(operand) => Rvalue::Use(self.operand(operand, at)?.0),
            m::Rvalue::CopyForDeref(place)
            | m::Rvalue::Ref(m::BorrowKind::Shared | m::BorrowKind::Shallow, place) => {
                Rvalue::Use(Operand::Place(self.place(place, at)?.0))
            }
            m::Rvalue::Ref(m::BorrowKind::Mut { .. }, _) => {
                return Err(self.refuse(
                    "a `&mut` reference that is not a temporary of one borrow (stored, chosen \
                     at run time or assigned twice)",
                    at,
                ));
            }
            m::Rvalue::BinaryOp(op, left, right) | m::Rvalue::CheckedBinaryOp(op, left, right) => {
                let (left, left_ty) = self.operand(left, at)?;
                let (right, _) = self.operand(right, at)?;
                let ty = self.op_ty(&left_ty, at)?;
                let overflowing = |op| match (op, ty) {
                    (op, OpTy::Int(ty)) => Ok(Rvalue::WithOverflow { op, ty, left: left.clone(), right: right.clone() }),
                    (_, OpTy::Bool) => Err(()),
                };
                let op = match op {
                    m::BinOp::Add => BinOp::Add,
                    m::BinOp::Sub => BinOp::Sub,
                    m::BinOp::Mul => BinOp::Mul,
                    m::BinOp::Div => BinOp::Div,
                    m::BinOp::Rem => BinOp::Rem,
                    m::BinOp::BitXor => BinOp::BitXor,
                    m::BinOp::BitAnd => BinOp::BitAnd,
                    m::BinOp::BitOr => BinOp::BitOr,
                    m::BinOp::Shl => BinOp::Shl,
                    m::BinOp::Shr => BinOp::Shr,
                    m::BinOp::Eq => BinOp::Eq,
                    m::BinOp::Lt => BinOp::Lt,
                    m::BinOp::Le => BinOp::Le,
                    m::BinOp::Ne => BinOp::Ne,
                    m::BinOp::Ge => BinOp::Ge,
                    m::BinOp::Gt => BinOp::Gt,
                    m::BinOp::Offset => {
                        return Err(self.refuse("pointer arithmetic (rule RawPointer)", at));
                    }
                    m::BinOp::AddWithOverflow
                    | m::BinOp::SubWithOverflow
                    | m::BinOp::MulWithOverflow => {
                        let plain = match op {
                            m::BinOp::AddWithOverflow => BinOp::Add,
                            m::BinOp::SubWithOverflow => BinOp::Sub,
                            _ => BinOp::Mul,
                        };
                        return match overflowing(plain) {
                            Ok(value) => Ok(Some(value)),
                            Err(()) => Err(self.refuse("an overflowing operation on a `bool`", at)),
                        };
                    }
                };
                let checked = matches!(rvalue, m::Rvalue::CheckedBinaryOp(..))
                    && self.checks_overflow
                    && matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Shl | BinOp::Shr);
                Rvalue::Bin { op, ty, checked, left, right }
            }
            m::Rvalue::UnaryOp(op, of) => {
                let (of, of_ty) = self.operand(of, at)?;
                let ty = self.op_ty(&of_ty, at)?;
                match (op, ty) {
                    (m::UnOp::Not, ty) => Rvalue::Not { ty, of },
                    (m::UnOp::Neg, OpTy::Int(ty)) => {
                        Rvalue::Neg { ty, checked: self.checks_overflow, of }
                    }
                    (m::UnOp::Neg, OpTy::Bool) => {
                        return Err(self.refuse("the negation of a `bool`", at));
                    }
                }
            }
            m::Rvalue::Cast(m::CastKind::IntToInt, of, to) => {
                let (of, of_ty) = self.operand(of, at)?;
                let from = self.op_ty(&of_ty, at)?;
                match self.ty(to.as_ref(), false, at)? {
                    Ty::Int(to) => Rvalue::Cast { from, to, of },
                    other => return Err(self.refuse(format!("a cast to `{other:?}`"), at)),
                }
            }
            m::Rvalue::Cast(kind, ..) => {
                return Err(self.refuse(format!("a `{kind:?}` cast"), at));
            }
            m::Rvalue::Len(place) => {
                let (place, ty) = self.place(place, at)?;
                match ty.value() {
                    Ty::Slice(_) | Ty::Vec(_) => Rvalue::Len(place),
                    Ty::Array(_, len) => Rvalue::Use(Operand::Int(IntTy::Usize, *len as u128)),
                    other => return Err(self.refuse(format!("the length of a `{other:?}`"), at)),
                }
            }
            m::Rvalue::Discriminant(place) => {
                let (place, ty) = self.place(place, at)?;
                let ty = ty.value().clone();
                if self.variants(&ty).is_none() {
                    return Err(self.refuse(format!("the discriminant of a `{ty:?}`"), at));
                }
                Rvalue::Discriminant(place, ty)
            }
            m::Rvalue::Aggregate(kind, operands) => {
                let mut args = Vec::with_capacity(operands.len());
                for operand in operands.iter() {
                    args.push(self.operand(operand, at)?.0);
                }
                match kind {
                    m::AggregateKind::Tuple(_) if args.is_empty() => Rvalue::Use(Operand::Unit),
                    m::AggregateKind::Tuple(_) => Rvalue::Tuple(args),
                    m::AggregateKind::Array(_) => Rvalue::Array(args),
                    m::AggregateKind::Adt(variant, _) => self.construct(*variant, args, dest, at)?,
                    m::AggregateKind::Union(..) => return Err(self.refuse("a union", at)),
                    m::AggregateKind::Closure(_) => return Err(self.refuse("a closure", at)),
                }
            }
            m::Rvalue::Repeat(of, len) => {
                let len = match len.as_ref().kind() {
                    ConstKind::Value(value) => {
                        value.try_to_leaf().map(|leaf| leaf.to_bits_unchecked())
                    }
                    _ => None,
                };
                let Some(len) = len else {
                    return Err(self.refuse("an array whose length is not a number", at));
                };
                Rvalue::Repeat(self.operand(of, at)?.0, len as u64)
            }
            m::Rvalue::ThreadLocalRef(never)
            | m::Rvalue::AddressOf(never)
            | m::Rvalue::NullaryOp(never) => match *never {},
        }))
    }

    /// A value of `variant` (a struct, or a variant of an enum) with `args` as its fields, of
    /// type `dest`.
    fn construct(
        &mut self,
        variant: VariantId,
        args: Vec<Operand>,
        dest: &Ty,
        at: &str,
    ) -> Refused<Rvalue> {
        match (variant, dest.value()) {
            (VariantId::StructId(_), Ty::Adt(name)) => {
                let Some(AdtKind::Struct(fields)) =
                    self.adts.iter().find(|it| it.name == *name).map(|it| it.kind.clone())
                else {
                    return Err(self.refuse("a struct value of a type that is not a struct", at));
                };
                let fields = fields.into_iter().map(|it| it.name).zip(args).collect();
                Ok(Rvalue::Struct { adt: name.clone(), fields })
            }
            (VariantId::EnumVariantId(variant), ty) if self.variants(ty).is_some() => {
                Ok(Rvalue::Variant { ty: ty.clone(), variant: variant.index(self.db) as u32, args })
            }
            (VariantId::UnionId(_), _) => Err(self.refuse("a union", at)),
            // `Global` and `RangeFull`, the structs whose type is `()` ([`Cx::ty`]).
            (VariantId::StructId(_), Ty::Unit) if args.is_empty() => Ok(Rvalue::Use(Operand::Unit)),
            (_, other) => Err(self.refuse(format!("a struct or enum value of type `{other:?}`"), at)),
        }
    }

    fn statement(&mut self, statement: &m::Statement, out: &mut Vec<Stmt>) -> Refused<()> {
        let at = self.at(statement.span);
        match &statement.kind {
            m::StatementKind::Assign(place, rvalue) => {
                if place.projection.is_empty() && self.alias.contains_key(&index(place.local)) {
                    return Ok(());
                }
                // A constant, a closure, or a borrow of another such local, put in a local of a
                // type outside the subset (`expect`'s message, a closure given to a method), does
                // nothing until the local is read, which `place` refuses at the read: the call
                // it is given to.
                let unusable = |local: m::LocalId| self.unusable.contains_key(&index(local));
                if place.projection.is_empty()
                    && unusable(place.local)
                    && match rvalue {
                        m::Rvalue::Use(m::Operand {
                            kind: m::OperandKind::Constant { .. } | m::OperandKind::Allocation { .. },
                            ..
                        })
                        | m::Rvalue::Aggregate(m::AggregateKind::Closure(_), _) => true,
                        m::Rvalue::Ref(_, from) | m::Rvalue::CopyForDeref(from) => unusable(from.local),
                        m::Rvalue::Use(m::Operand {
                            kind: m::OperandKind::Copy(from) | m::OperandKind::Move(from),
                            ..
                        }) => unusable(from.local),
                        _ => false,
                    }
                {
                    return Ok(());
                }
                // A `&mut` temporary is its place only while it is the one name of the borrow:
                // moved into a local that is assigned again, a write through that local would
                // not reach the place.
                if let m::Rvalue::Use(m::Operand {
                    kind: m::OperandKind::Move(from) | m::OperandKind::Copy(from),
                    ..
                }) = rvalue
                    && from.projection.is_empty()
                    && self.alias.contains_key(&index(from.local))
                {
                    return Err(self.refuse(
                        "a `&mut` reference that is not a temporary of one borrow (stored, chosen \
                         at run time or assigned twice)",
                        &at,
                    ));
                }
                let (place, ty) = self.place(place, &at)?;
                if let Some(value) = self.rvalue(rvalue, &ty, &at)? {
                    out.push(Stmt::Assign { place, value, at });
                }
            }
            m::StatementKind::FakeRead(_)
            | m::StatementKind::Nop
            | m::StatementKind::StorageLive(_)
            | m::StatementKind::StorageDead(_) => {}
            m::StatementKind::Deinit(_) => {
                return Err(self.refuse("a `Deinit` statement, which built MIR never has", &at));
            }
            m::StatementKind::InlineAsm(_) => {
                return Err(self.refuse("inline assembly (rule InlineAsm)", &at));
            }
        }
        Ok(())
    }

    fn call(
        &mut self,
        func: &m::Operand,
        args: &[m::Operand],
        destination: &m::StoredPlace,
        target: Option<usize>,
        at: &str,
        out: &mut Vec<Stmt>,
    ) -> Refused<Exit<Cond>> {
        let Some((callee, generics)) = func.fn_def() else {
            return Err(self.refuse("a call of a function pointer or a closure", at));
        };
        let db = self.db;
        let callee = match callee {
            CallableDefId::FunctionId(callee) => callee,
            CallableDefId::StructId(it) => {
                let (place, ty) = self.place(destination, at)?;
                let args = self.operands(args, at)?;
                let value = self.construct(VariantId::StructId(it), args, &ty, at)?;
                out.push(Stmt::Assign { place, value, at: at.to_owned() });
                return self.goto(target, at);
            }
            CallableDefId::EnumVariantId(it) => {
                let (place, ty) = self.place(destination, at)?;
                let args = self.operands(args, at)?;
                let value = self.construct(VariantId::EnumVariantId(it), args, &ty, at)?;
                out.push(Stmt::Assign { place, value, at: at.to_owned() });
                return self.goto(target, at);
            }
        };
        let name = FunctionSignature::of(db, callee).name.as_str().to_owned();
        let (callee, generics) = self.dispatch(callee, generics, &name, at)?;
        let container = callee.lookup(db).container;
        let callee = if callee.krate(db).data(db).origin.is_lang() {
            if PANICS.contains(&name.as_str()) {
                return Ok(Exit::End(End::Panic));
            }
            if name == "catch_unwind" {
                return Err(self.refuse("`catch_unwind` (rule CatchUnwind)", at));
            }
            // The `&mut` this call gives is a temporary that stands for the place it borrows
            // (`find_aliases`): the call is that borrow, and the index it takes is copied here.
            if destination.projection.is_empty()
                && let Some(alias) = self.alias.get(&index(destination.local)).cloned()
            {
                if let Some(fresh) = alias.index {
                    let (by, _) = self.operand(&args[1], at)?;
                    out.push(Stmt::Assign {
                        place: Place::local(fresh),
                        value: Rvalue::Use(by),
                        at: at.to_owned(),
                    });
                }
                return self.goto(target, at);
            }
            let std = Std { callee, generics, name: &name, container };
            if let Some((callee, args)) = self.std_call(std, args, destination, at, out)? {
                let (dest, _) = self.place(destination, at)?;
                out.push(Stmt::Call { dest, callee, args, at: at.to_owned() });
            }
            return self.goto(target, at);
        } else {
            if let ItemContainerId::ExternBlockId(_) = container {
                return Err(self.refuse(format!("a call of the extern function `{name}` (rule Ffi)"), at));
            }
            self.crate_callee(callee, generics, &name, at)?
        };
        let (dest, _) = self.place(destination, at)?;
        let args = self.operands(args, at)?;
        out.push(Stmt::Call { dest, callee, args, at: at.to_owned() });
        self.goto(target, at)
    }

    /// A call of the crate's function `callee` with `generics`: the instance it names (the
    /// function itself when it has no type or const parameters), its model's name, and the
    /// instance put among the callees, to be translated after this body.
    fn crate_callee(
        &mut self,
        callee: FunctionId,
        generics: GenericArgs<'db>,
        name: &str,
        at: &str,
    ) -> Refused<Callee> {
        let db = self.db;
        let args = is_generic(db, callee).then_some(generics);
        let named = match instance_name(db, callee, args) {
            Ok(named) => named,
            Err(why) => return Err(self.refuse(format!("a call of `{name}`, {why}"), at)),
        };
        let instance = Instance { func: callee, args, krate: self.env.krate };
        if !self.callees.iter().any(|(_, it)| *it == instance) {
            self.callees.push((named.lean.clone(), instance));
        }
        Ok(Callee::Function { name: named.lean })
    }

    /// What static dispatch runs for a call of `callee` with `generics`: for a method of a
    /// trait (`square.area()`, `Shape::area(&s)`, `<Square as Shape>::area(&s)`,
    /// `Clock::default()`, `a == b` and `a + b` of a type with its own impl, which MIR calls
    /// as the trait's method with `Self` among the arguments), the method of the impl the
    /// trait solver picks for those arguments in this function's environment, as codegen
    /// picks it. A call of a function that is no trait's method is itself.
    ///
    /// A method of the standard library's trait that no impl of this program's crates
    /// implements (an integer's `Ord::max`, a derive's `PartialEq::eq`, `From` between
    /// integers) stays the trait's: [`Cx::std_call`] models it by its trait and name, as
    /// before. A method of a crate's own trait that is not an impl's is refused by what it is:
    /// a call through a generic bound, through a trait object, or of the trait's default body.
    fn dispatch(
        &self,
        callee: FunctionId,
        generics: GenericArgs<'db>,
        name: &str,
        at: &str,
    ) -> Refused<(FunctionId, GenericArgs<'db>)> {
        let db = self.db;
        let ItemContainerId::TraitId(of) = callee.lookup(db).container else {
            return Ok((callee, generics));
        };
        let trait_name = TraitSignature::of(db, of).name.as_str().to_owned();
        let of_std = callee.krate(db).data(db).origin.is_lang();
        if matches!(generics.type_at(0).kind(), TyKind::Dynamic(..)) {
            return Err(self.refuse(
                format!(
                    "a call of `{trait_name}::{name}` through a trait object (`dyn \
                     {trait_name}`; rule DynTrait)"
                ),
                at,
            ));
        }
        let env = self.env;
        let looked = db
            .lookup_impl_method_or_why(env, callee, generics)
            .map(|(found, args)| (found.left(), args));
        match looked {
            Ok((Some(found), args)) if !found.krate(db).data(db).origin.is_lang() => {
                match found.lookup(db).container {
                    ItemContainerId::ImplId(_) => Ok((found, args)),
                    _ => Err(self.refuse(
                        format!(
                            "a call of `{trait_name}::{name}`, the trait's default method, whose \
                             body is generic over `Self` (only an impl's method is translated)"
                        ),
                        at,
                    )),
                }
            }
            // The standard library's: its model is the trait's method's, by name.
            Ok(_) if of_std => Ok((callee, generics)),
            Err(why) if of_std && why.is_expected() => Ok((callee, generics)),
            Ok((Some(found), _)) => Err(self.refuse(
                format!(
                    "a call of `{trait_name}::{name}`, which runs the standard library's `{}`",
                    FunctionSignature::of(db, found).name.as_str()
                ),
                at,
            )),
            Ok((None, _)) => Err(self.refuse(
                format!("a call of `{trait_name}::{name}`, which a built-in derive implements"),
                at,
            )),
            Err(NoImplMethod::Generic | NoImplMethod::Param) => Err(self.refuse(
                format!(
                    "a call of `{trait_name}::{name}` through a generic bound (the impl is chosen \
                     by the instance; generics by instance are not translated yet)"
                ),
                at,
            )),
            Err(why) => Err(self.refuse(
                format!("a call of `{trait_name}::{name}`, whose impl is not found: {}", why.describe()),
                at,
            )),
        }
    }

    /// Where the standard library's function in `container` is.
    fn home(&self, container: ItemContainerId) -> Home {
        let db = self.db;
        let of_trait = |it: TraitId| Home::Trait {
            name: TraitSignature::of(db, it).name.as_str().to_owned(),
            module: module_path(db, it.lookup(db).container).split_off(1),
        };
        match container {
            ItemContainerId::TraitId(it) => of_trait(it),
            ItemContainerId::ImplId(imp) => {
                if let Some(of) = db.impl_trait(imp) {
                    return of_trait(of.skip_binder().def_id.0);
                }
                let ty = db.impl_self_ty(imp).skip_binder();
                if let Some(int) = int_ty(ty) {
                    return Home::Int(int);
                }
                let module = |adt: AdtId| module_path(db, adt.module(db)).split_off(1);
                match ty.kind() {
                    TyKind::Slice(_) => Home::Slice,
                    TyKind::Str => Home::Str,
                    TyKind::Adt(def, _) => match def.def_id() {
                        AdtId::EnumId(it) => match EnumSignature::of(db, it).name.as_str() {
                            "Option" => Home::Option,
                            "Result" => Home::Result,
                            "Ordering" if module(it.into()) == ["cmp"] => Home::Ordering,
                            _ => Home::Other,
                        },
                        adt if is_vec(db, adt) => Home::Vec,
                        AdtId::StructId(it)
                            if StructSignature::of(db, it).name.as_str() == "Duration"
                                && module(it.into()) == ["time"] =>
                        {
                            Home::Duration
                        }
                        _ => Home::Other,
                    },
                    _ => Home::Other,
                }
            }
            ItemContainerId::ModuleId(module) => Home::Module(module_path(db, module).split_off(1)),
            ItemContainerId::ExternBlockId(_) => Home::Other,
        }
    }

    /// The model of a call of the standard library's function `std`, and the arguments the
    /// model takes; `None` when the call is statements, put in `out` (`mem::replace`, a
    /// `Vec`'s deref). Each is recognised by where the callee is and its name, or by the lang
    /// item it is, never by its body; one with no model is refused by name (rule NoModel).
    fn std_call(
        &mut self,
        std: Std<'_, 'db>,
        args: &[m::Operand],
        destination: &m::StoredPlace,
        at: &str,
        out: &mut Vec<Stmt>,
    ) -> Refused<Option<(Callee, Vec<Operand>)>> {
        let Std { callee, generics, name, container } = std;
        let model = |name: &'static str, monadic: bool| Callee::Model {
            name,
            monadic,
            ty: None,
            lends_first: false,
        };
        let no_model = |this: &Self| {
            this.refuse(
                format!(
                    "a call of the standard library's `{name}`, which has no model (rule NoModel)"
                ),
                at,
            )
        };
        // A closure or a function given to the callee is what is outside the subset, more
        // than the callee (`Option::map`, a closure's own `call`): it is named first.
        for arg in args {
            if let m::OperandKind::Copy(place) | m::OperandKind::Move(place) = &arg.kind
                && let Some(what) = self.unusable.get(&index(place.local))
                && (what == "a closure" || what == "a function used as a value")
            {
                return Err(self.refuse(what.clone(), at));
            }
        }
        // `Index::index`, `Deref::deref` and their `&mut` twins, of a `Vec`: by the lang item
        // the callee is. A `&mut` one that `find_aliases` made a place of never gets here.
        let lang = self.interner.lang_items();
        let is = |item: Option<FunctionId>| item == Some(callee);
        let (index_mut, deref_mut) = (is(lang.IndexMut_index_mut), is(lang.DerefMut_deref_mut));
        if is(lang.Index_index) || index_mut || is(lang.Deref_deref) || deref_mut {
            if !self.is_vec_ty(generics.type_at(0)) {
                return Err(no_model(self));
            }
            let by_element = match is(lang.Index_index) || index_mut {
                true => self.vec_index(generics.type_at(1), at)?,
                false => false,
            };
            if index_mut || deref_mut {
                return Err(self.refuse(UNLENT_VEC, at));
            }
            if by_element {
                return Ok(Some((model("Slice.index", true), self.operands(args, at)?)));
            }
            // `&v[..]` and `*v`: the `Vec` as a slice, which is its value.
            let (dest, _) = self.place(destination, at)?;
            let (of, _) = self.operand(&args[0], at)?;
            out.push(Stmt::Assign { place: dest, value: Rvalue::Use(of), at: at.to_owned() });
            return Ok(None);
        }
        match self.home(container) {
            Home::Int(int) => {
                if let Some(&(_, it, monadic)) = INT_MODELS.iter().find(|it| it.0 == name) {
                    return Ok(Some((model(it, monadic), self.operands(args, at)?)));
                }
                if let Some(&(_, checked, wrapping)) = INT_INHERITED.iter().find(|it| it.0 == name)
                {
                    let callee = if self.checks_overflow {
                        model(checked, true)
                    } else {
                        model(wrapping, false)
                    };
                    return Ok(Some((callee, self.operands(args, at)?)));
                }
                if let Some(&(_, it)) = INT_FROM_BYTES.iter().find(|it| it.0 == name) {
                    let callee =
                        Callee::Model { name: it, monadic: false, ty: Some(int), lends_first: false };
                    return Ok(Some((callee, self.operands(args, at)?)));
                }
                // A signed `div_ceil` rounds toward positive infinity and is not modelled.
                if name == "div_ceil" && !int.signed() {
                    return Ok(Some((model("Scalar.div_ceil", true), self.operands(args, at)?)));
                }
                Err(no_model(self))
            }
            home @ (Home::Option | Home::Result) => {
                let (table, unwrap_or) = match home {
                    Home::Option => (OPTION_MODELS, "core.option.Option.unwrap_or"),
                    _ => (RESULT_MODELS, "core.result.Result.unwrap_or"),
                };
                if name == "unwrap_or_default" {
                    // `T::default()` of the integers and `bool`, which are their zero and `false`.
                    let default = match self.place(destination, at)?.1 {
                        Ty::Int(int) => Operand::Int(int, 0),
                        Ty::Bool => Operand::Bool(false),
                        other => {
                            return Err(self.refuse(
                                format!("`unwrap_or_default` of a `{other:?}`, whose default has no model"),
                                at,
                            ));
                        }
                    };
                    let mut operands = self.operands(&args[..1], at)?;
                    operands.push(default);
                    return Ok(Some((model(unwrap_or, false), operands)));
                }
                let Some(&(_, it, monadic, lends_first)) = table.iter().find(|it| it.0 == name)
                else {
                    return Err(no_model(self));
                };
                // `expect`'s message is what the panic prints, which the model does not keep.
                let args = if name == "expect" { &args[..1] } else { args };
                let callee = Callee::Model { name: it, monadic, ty: None, lends_first };
                Ok(Some((callee, self.operands(args, at)?)))
            }
            Home::Vec => self.vec_call(name, args, destination, at, out),
            Home::Slice => self.slice_call(name, args, at),
            Home::Duration => {
                let Some(&(_, it, monadic)) = DURATION_MODELS.iter().find(|it| it.0 == name) else {
                    return Err(self.refuse(
                        format!("a call of `Duration::{name}`, which has no model (rule NoModel)"),
                        at,
                    ));
                };
                Ok(Some((model(it, monadic), self.operands(args, at)?)))
            }
            Home::Ordering => {
                let Some(&(_, it)) = ORDERING_MODELS.iter().find(|it| it.0 == name) else {
                    return Err(self.refuse(
                        format!("a call of `Ordering::{name}`, which has no model (rule NoModel)"),
                        at,
                    ));
                };
                Ok(Some((model(it, false), self.operands(args, at)?)))
            }
            Home::Str => {
                let Some(&(_, it)) = STR_MODELS.iter().find(|it| it.0 == name) else {
                    return Err(self.refuse(
                        format!(
                            "a call of the `str` method `{name}`, which has no model (rule \
                             NoModel; a `str` is its bytes, and only `len`, `is_empty`, \
                             `as_bytes`, `==` and `!=` are modelled)"
                        ),
                        at,
                    ));
                };
                Ok(Some((model(it, false), self.operands(args, at)?)))
            }
            Home::Trait { name: of, module } => {
                let module: Vec<&str> = module.iter().map(String::as_str).collect();
                let self_ty = generics.type_at(0);
                match (module.as_slice(), of.as_str(), name) {
                    (["clone"], "Clone", "clone") => {
                        self.structural(callee, self_ty, Structural::Clone, &mut BTreeSet::new(), at)?;
                        // The clone of a value whose `Clone` is a copy of its structure is the
                        // value: the argument, a `&T`, is `T`'s value in the model.
                        let (dest, _) = self.place(destination, at)?;
                        let (of, _) = self.operand(&args[0], at)?;
                        out.push(Stmt::Assign { place: dest, value: Rvalue::Use(of), at: at.to_owned() });
                        Ok(None)
                    }
                    (["cmp"], "PartialEq", "eq" | "ne") => {
                        if generics.type_at(1) != self_ty {
                            return Err(self.refuse(
                                format!(
                                    "a call of `PartialEq::{name}` between two types (only `==` of \
                                     one type with itself is modelled; rule NoModel)"
                                ),
                                at,
                            ));
                        }
                        let eq = self.interner.lang_items().PartialEq_eq;
                        let Some(eq) = eq else { return Err(no_model(self)) };
                        // `a == b` of two `&T` runs the standard library's `impl PartialEq<&B>
                        // for &A`, which calls `T`'s own `eq` (cmp.rs, `PartialEq::eq(*self,
                        // *other)`): when `T` is the crate's type with a hand-written impl, that
                        // impl's method is called as the function it is, a reference being its
                        // value in the model.
                        if name == "eq"
                            && let Some(own) = self.own_eq(eq, self_ty, at)?
                        {
                            return Ok(Some((own, self.operands(args, at)?)));
                        }
                        self.structural(eq, self_ty, Structural::PartialEq, &mut BTreeSet::new(), at)?;
                        let it = if name == "eq" { "core.cmp.PartialEq.eq" } else { "core.cmp.PartialEq.ne" };
                        Ok(Some((model(it, false), self.operands(args, at)?)))
                    }
                    (["cmp"], "Ord", "cmp") | (["cmp"], "PartialOrd", "partial_cmp" | "lt" | "le" | "gt" | "ge") => {
                        self.order_call(of.as_str(), name, self_ty, generics, args, at).map(Some)
                    }
                    (["cmp"], "Ord", "max" | "min" | "clamp") => self.int_order(name, args, at).map(Some),
                    ([ops, ..], "Add" | "Sub" | "AddAssign" | "SubAssign", "add" | "sub" | "add_assign" | "sub_assign")
                        if *ops == "ops" =>
                    {
                        let peeled = |ty: HTy<'db>| match ty.kind() {
                            TyKind::Ref(_, inner, _) => inner,
                            _ => ty,
                        };
                        let on_duration = |this: &mut Self, ty: HTy<'db>| -> Refused<bool> {
                            Ok(this.ty(peeled(ty), false, at)? == Ty::Duration)
                        };
                        if !on_duration(self, self_ty)? || !on_duration(self, generics.type_at(1))? {
                            return Err(no_model(self));
                        }
                        // `+` and `-` panic where `checked_add` and `checked_sub` give `None`
                        // (time.rs:1345, 1365); `+=` and `-=` are them, written back
                        // (time.rs:1353, 1373).
                        let (it, lends_first) = match name {
                            "add" => ("core.time.Duration.add", false),
                            "sub" => ("core.time.Duration.sub", false),
                            "add_assign" => ("core.time.Duration.add_assign", true),
                            _ => ("core.time.Duration.sub_assign", true),
                        };
                        let callee = Callee::Model { name: it, monadic: true, ty: None, lends_first };
                        Ok(Some((callee, self.operands(args, at)?)))
                    }
                    (["convert"], "From", "from") | (["convert"], "Into", "into") => {
                        let operands = self.operands(args, at)?;
                        let from = self.operand(&args[0], at)?.1;
                        // A `From` between integers is lossless: the cast keeps the value.
                        let (callee, to) = match (from.value(), self.place(destination, at)?.1) {
                            (Ty::Int(_), Ty::Int(to)) => ("Scalar.cast", to),
                            (Ty::Bool, Ty::Int(to)) => ("Scalar.ofBool", to),
                            // `x.into()` into a type with the crate's own `From` runs the
                            // standard library's blanket `impl<T, U: From<T>> Into<U> for T`,
                            // whose body (`U::from(self)`) is the standard library's: not
                            // translated (step c), and not stood in for by the `From` it calls.
                            (_, to) if name == "into" => {
                                return Err(self.refuse(
                                    format!(
                                        "a call of `Into::into` into `{}`, the standard library's \
                                         blanket impl over `From` (its body is not translated; \
                                         call `From::from`, which is; rule NoModel)",
                                        crate::rust_ty(&to)
                                    ),
                                    at,
                                ));
                            }
                            _ => return Err(no_model(self)),
                        };
                        let callee =
                            Callee::Model { name: callee, monadic: false, ty: Some(to), lends_first: false };
                        Ok(Some((callee, operands)))
                    }
                    (["convert"], "TryFrom", "try_from") | (["convert"], "TryInto", "try_into") => {
                        let operands = self.operands(args, at)?;
                        let from = self.operand(&args[0], at)?.1;
                        let dest = self.place(destination, at)?.1;
                        let (Ty::Int(_), Ty::Result(ok, err)) = (from.value(), &dest) else {
                            return Err(no_model(self));
                        };
                        let (Ty::Int(to), Ty::TryFromIntError) = (ok.as_ref(), err.as_ref()) else {
                            return Err(no_model(self));
                        };
                        let callee = Callee::Model {
                            name: "Scalar.try_from",
                            monadic: false,
                            ty: Some(*to),
                            lends_first: false,
                        };
                        Ok(Some((callee, operands)))
                    }
                    _ => Err(no_model(self)),
                }
            }
            Home::Module(module) if module == ["cmp"] && matches!(name, "max" | "min") => {
                self.int_order(name, args, at).map(Some)
            }
            Home::Module(module) if module == ["mem"] => self.mem_call(name, args, destination, at, out),
            Home::Module(module) if module == ["vec"] && name == "from_elem" => {
                let operands = self.operands(args, at)?;
                let elem = self.operand(&args[0], at)?.1;
                self.cloned_is_copied(&elem, "`vec![x; n]`", at)?;
                Ok(Some((model("Vec.from_elem", false), operands)))
            }
            Home::Module(_) | Home::Other => Err(no_model(self)),
        }
    }

    /// Whether `ty` is a `Vec`.
    fn is_vec_ty(&self, ty: HTy<'db>) -> bool {
        matches!(ty.kind(), TyKind::Adt(def, _) if is_vec(self.db, def.def_id()))
    }

    /// What a `Vec` is indexed by: `true` for an element (`usize`), `false` for the whole of
    /// it (`..`); any other range, a subslice, is refused.
    fn vec_index(&self, by: HTy<'db>, at: &str) -> Refused<bool> {
        let full = self.interner.lang_items().RangeFull;
        match by.kind() {
            TyKind::Uint(UintTy::Usize) => Ok(true),
            TyKind::Adt(def, _) if matches!(def.def_id(), AdtId::StructId(it) if Some(it) == full) => {
                Ok(false)
            }
            _ => Err(self.refuse(
                "an index of a `Vec` by a range other than `..` (a subslice, which has no model)",
                at,
            )),
        }
    }

    /// Refused unless a clone of `elem` is a copy of it, as for an integer or a `bool`: `what`
    /// clones each element it makes.
    fn cloned_is_copied(&self, elem: &Ty, what: &str, at: &str) -> Refused<()> {
        fn copied(ty: &Ty) -> bool {
            match ty {
                Ty::Bool | Ty::Int(_) | Ty::Unit => true,
                Ty::Tuple(all) => all.iter().all(copied),
                _ => false,
            }
        }
        if copied(elem.value()) {
            return Ok(());
        }
        Err(self.refuse(
            format!(
                "{what} of a `{elem:?}`, which clones each element (only an integer's, a \
                 `bool`'s or a tuple of them is a copy the model makes)"
            ),
            at,
        ))
    }

    /// An inherent method of `Vec`: its model, or the statements it is.
    fn vec_call(
        &mut self,
        name: &str,
        args: &[m::Operand],
        destination: &m::StoredPlace,
        at: &str,
        out: &mut Vec<Stmt>,
    ) -> Refused<Option<(Callee, Vec<Operand>)>> {
        let model = |name: &'static str, monadic: bool, lends_first: bool| Callee::Model {
            name,
            monadic,
            ty: None,
            lends_first,
        };
        let callee = match name {
            // The capacity and the allocator (`Global`, a unit) are not kept: each is empty.
            "new" | "with_capacity" | "new_in" | "with_capacity_in" => {
                let (dest, ty) = self.place(destination, at)?;
                let Ty::Vec(elem) = ty else {
                    return Err(self.refuse(format!("`Vec::{name}` making a `{ty:?}`"), at));
                };
                out.push(Stmt::Assign { place: dest, value: Rvalue::EmptyVec(*elem), at: at.to_owned() });
                return Ok(None);
            }
            // The `Vec` as a slice, which is its value.
            "as_slice" => {
                let (dest, _) = self.place(destination, at)?;
                let (of, _) = self.operand(&args[0], at)?;
                out.push(Stmt::Assign { place: dest, value: Rvalue::Use(of), at: at.to_owned() });
                return Ok(None);
            }
            "len" => model("Slice.len", false, false),
            "is_empty" => model("Slice.is_empty", false, false),
            "push" => model("Vec.push", true, true),
            "pop" => model("Vec.pop", false, true),
            "clear" => model("Vec.clear", false, true),
            "truncate" => model("Vec.truncate", false, true),
            "insert" => model("Vec.insert", true, true),
            "remove" => model("Vec.remove", true, true),
            "swap_remove" => model("Vec.swap_remove", true, true),
            "extend_from_slice" => {
                if let (_, Ty::Vec(elem)) = self.operand(&args[0], at)? {
                    self.cloned_is_copied(&elem, "`extend_from_slice`", at)?;
                }
                model("Vec.extend_from_slice", true, true)
            }
            "capacity" => {
                return Err(self.refuse(
                    "`Vec::capacity`, which the model does not keep (a `Vec` is its elements; \
                     its capacity is not observable in the subset)",
                    at,
                ));
            }
            "as_mut_slice" => return Err(self.refuse(UNLENT_VEC, at)),
            _ => {
                return Err(self.refuse(
                    format!("a call of `Vec::{name}`, which has no model (rule NoModel)"),
                    at,
                ));
            }
        };
        Ok(Some((callee, self.operands(args, at)?)))
    }

    /// A method of a slice: its model.
    fn slice_call(&mut self, name: &str, args: &[m::Operand], at: &str) -> Refused<Option<(Callee, Vec<Operand>)>> {
        let (name, monadic, lends_first) = match name {
            "len" => ("Slice.len", false, false),
            "is_empty" => ("Slice.is_empty", false, false),
            "first" => ("Slice.first", false, false),
            "last" => ("Slice.last", false, false),
            "get" => {
                if self.operand(&args[1], at)?.1.value() != &Ty::Int(IntTy::Usize) {
                    return Err(self.refuse("`get` by a range (a subslice, which has no model)", at));
                }
                ("Slice.get", false, false)
            }
            "swap" => ("Slice.swap", true, true),
            _ => {
                return Err(self.refuse(
                    format!("a call of the slice method `{name}`, which has no model (rule NoModel)"),
                    at,
                ));
            }
        };
        let callee = Callee::Model { name, monadic, ty: None, lends_first };
        Ok(Some((callee, self.operands(args, at)?)))
    }

    /// `core::mem::replace`, `swap` and `take`: moves between the places lent, as statements.
    fn mem_call(
        &mut self,
        name: &str,
        args: &[m::Operand],
        destination: &m::StoredPlace,
        at: &str,
        out: &mut Vec<Stmt>,
    ) -> Refused<Option<(Callee, Vec<Operand>)>> {
        let lent = |this: &mut Self, arg: &m::Operand| -> Refused<(Place, Ty)> {
            match this.operand(arg, at)? {
                (Operand::Place(place), ty) => Ok((place, ty.value().clone())),
                _ => Err(this.refuse("a `&mut` argument that is not a place", at)),
            }
        };
        let assign = |place: Place, value: Rvalue| Stmt::Assign { place, value, at: at.to_owned() };
        let (dest, _) = self.place(destination, at)?;
        match name {
            "replace" => {
                let (place, _) = lent(self, &args[0])?;
                let (value, _) = self.operand(&args[1], at)?;
                out.push(assign(dest, Rvalue::Use(Operand::Place(place.clone()))));
                out.push(assign(place, Rvalue::Use(value)));
            }
            "swap" => {
                let (a, ty) = lent(self, &args[0])?;
                let (b, _) = lent(self, &args[1])?;
                let held = self.locals.len() as Local;
                self.locals.push(LocalDecl { name: None, ty });
                out.push(assign(Place::local(held), Rvalue::Use(Operand::Place(a.clone()))));
                out.push(assign(a, Rvalue::Use(Operand::Place(b.clone()))));
                out.push(assign(b, Rvalue::Use(Operand::Place(Place::local(held)))));
                out.push(assign(dest, Rvalue::Use(Operand::Unit)));
            }
            "take" => {
                let (place, ty) = lent(self, &args[0])?;
                // `T::default()` of the types whose default the standard library gives.
                let default = match &ty {
                    Ty::Int(int) => Rvalue::Use(Operand::Int(*int, 0)),
                    Ty::Bool => Rvalue::Use(Operand::Bool(false)),
                    Ty::Unit => Rvalue::Use(Operand::Unit),
                    Ty::Option(_) => Rvalue::Variant { ty: ty.clone(), variant: 0, args: Vec::new() },
                    Ty::Vec(elem) => Rvalue::EmptyVec((**elem).clone()),
                    other => {
                        return Err(self.refuse(
                            format!("`mem::take` of a `{other:?}`, whose default has no model"),
                            at,
                        ));
                    }
                };
                out.push(assign(dest, Rvalue::Use(Operand::Place(place.clone()))));
                out.push(assign(place, default));
            }
            _ => {
                return Err(self.refuse(
                    format!("a call of `mem::{name}`, which has no model (rule NoModel)"),
                    at,
                ));
            }
        }
        Ok(None)
    }

    /// The place a call of the standard library lends a `&mut` into, if the `&mut` it gives is
    /// one: `IndexMut::index_mut` of a `Vec` by an element (the element, with `true`: the
    /// index is copied at the call), or by `..`, `DerefMut::deref_mut` and `as_mut_slice` of a
    /// `Vec` (the `Vec`, its own slice). The `&mut` it is given must be a local.
    fn lends_into(&self, func: &m::Operand, args: &[m::Operand]) -> Option<(m::StoredPlace, bool)> {
        let (CallableDefId::FunctionId(callee), generics) = func.fn_def()? else { return None };
        let db = self.db;
        if !callee.krate(db).data(db).origin.is_lang() {
            return None;
        }
        let lang = self.interner.lang_items();
        let indexed = if Some(callee) == lang.IndexMut_index_mut {
            if !self.is_vec_ty(generics.type_at(0)) {
                return None;
            }
            self.vec_index(generics.type_at(1), "").ok()?
        } else if Some(callee) == lang.DerefMut_deref_mut {
            if !self.is_vec_ty(generics.type_at(0)) {
                return None;
            }
            false
        } else if matches!(self.home(callee.lookup(db).container), Home::Vec)
            && FunctionSignature::of(db, callee).name.as_str() == "as_mut_slice"
        {
            false
        } else {
            return None;
        };
        let (m::OperandKind::Move(from) | m::OperandKind::Copy(from)) = &args.first()?.kind else {
            return None;
        };
        let is_mut = matches!(self.locals.get(index(from.local) as usize).map(|it| &it.ty), Some(Ty::RefMut(_)));
        if !from.projection.is_empty() || !is_mut {
            return None;
        }
        Some((from.as_ref().project(m::ProjectionElem::Deref).store(), indexed))
    }

    /// `Ord::max`, `Ord::min`, `Ord::clamp`, `cmp::max` or `cmp::min` of integers.
    fn int_order(
        &mut self,
        name: &str,
        args: &[m::Operand],
        at: &str,
    ) -> Refused<(Callee, Vec<Operand>)> {
        let mut operands = Vec::with_capacity(args.len());
        let mut on_duration = false;
        for arg in args {
            let (operand, ty) = self.operand(arg, at)?;
            match ty.value() {
                Ty::Int(_) => {}
                // `Ord::max` and `Ord::min`, the trait's default methods, of `Duration`, whose
                // `Ord` is derived (cmp.rs:1101, 1140); `clamp` of one is not modelled.
                Ty::Duration if name != "clamp" => on_duration = true,
                _ => {
                    return Err(self.refuse(
                        format!("a call of the standard library's `{name}` on a `{ty:?}`, which has no model (rule NoModel)"),
                        at,
                    ));
                }
            }
            operands.push(operand);
        }
        let (name, monadic) = match (name, on_duration) {
            ("max", true) => ("core.time.Duration.max", false),
            ("min", true) => ("core.time.Duration.min", false),
            ("max", _) => ("Scalar.max", false),
            ("min", _) => ("Scalar.min", false),
            // `clamp` panics when its least is above its most.
            _ => ("Scalar.ord_clamp", true),
        };
        Ok((Callee::Model { name, monadic, ty: None, lends_first: false }, operands))
    }

    /// `Ord::cmp`, `PartialOrd::partial_cmp`, `lt`, `le`, `gt` or `ge` (the operators `<`,
    /// `<=`, `>`, `>=` of a type that is not a primitive) of `self_ty` with itself: of an
    /// integer (`impl Ord for u32`, cmp.rs:2305 and 2316, and the trait's default `lt`..,
    /// cmp.rs:1502-1556) or of a `Duration` (derived, lexicographic on its seconds then its
    /// nanoseconds). Any other order, a derived one of the crate's types among them, is
    /// refused by name.
    fn order_call(
        &mut self,
        of: &str,
        name: &str,
        self_ty: HTy<'db>,
        generics: GenericArgs<'db>,
        args: &[m::Operand],
        at: &str,
    ) -> Refused<(Callee, Vec<Operand>)> {
        let mut peeled = self_ty;
        while let TyKind::Ref(_, inner, _) = peeled.kind() {
            peeled = inner;
        }
        let ty = self.ty(peeled, false, at)?;
        let same = match of {
            "Ord" => true,
            _ => generics.type_at(1) == self_ty,
        };
        let it = match (&ty, name) {
            (_, _) if !same => None,
            (Ty::Int(_), "cmp") => Some("Scalar.cmp"),
            (Ty::Int(_), "partial_cmp") => Some("Scalar.partial_cmp"),
            (Ty::Int(_), "lt") => Some("Scalar.lt"),
            (Ty::Int(_), "le") => Some("Scalar.le"),
            (Ty::Int(_), "gt") => Some("Scalar.gt"),
            (Ty::Int(_), "ge") => Some("Scalar.ge"),
            (Ty::Duration, "cmp") => Some("core.time.Duration.cmp"),
            (Ty::Duration, "partial_cmp") => Some("core.time.Duration.partial_cmp"),
            (Ty::Duration, "lt") => Some("core.time.Duration.lt"),
            (Ty::Duration, "le") => Some("core.time.Duration.le"),
            (Ty::Duration, "gt") => Some("core.time.Duration.gt"),
            (Ty::Duration, "ge") => Some("core.time.Duration.ge"),
            _ => None,
        };
        let Some(it) = it else {
            let shown = crate::rust_ty(&ty);
            let why = match ty {
                Ty::Adt(_) => ", whose order is derived or the crate's own (a derived order has no model)",
                _ => "",
            };
            return Err(self.refuse(
                format!("a call of `{of}::{name}` on `{shown}`{why}, which has no model (rule NoModel)"),
                at,
            ));
        };
        Ok((Callee::Model { name: it, monadic: false, ty: None, lends_first: false }, self.operands(args, at)?))
    }

    /// The crate's own `PartialEq::eq` of the type under the references `self_ty` is, when
    /// `self_ty` is a reference and that type's impl is a hand-written one of the crate: the
    /// function [`Cx::call`] would call for `==` of the type itself. `None` otherwise.
    fn own_eq(&mut self, eq: FunctionId, self_ty: HTy<'db>, at: &str) -> Refused<Option<Callee>> {
        let db = self.db;
        let mut peeled = self_ty;
        while let TyKind::Ref(_, inner, _) = peeled.kind() {
            peeled = inner;
        }
        let TyKind::Adt(def, _) = peeled.kind() else { return Ok(None) };
        if peeled == self_ty || def.def_id().krate(db).data(db).origin.is_lang() {
            return Ok(None);
        }
        let env = self.env;
        let generics = GenericArgs::new_from_slice(&[peeled.into(), peeled.into()]);
        let (found, args) = match db.lookup_impl_method_or_why(env, eq, generics) {
            Ok((found, args)) => (found.left(), args),
            Err(_) => (None, generics),
        };
        let Some(found) = found else { return Ok(None) };
        if found.krate(db).data(db).origin.is_lang() {
            return Ok(None);
        }
        self.crate_callee(found, args, "eq", at).map(Some)
    }

    /// Refused unless `ty`'s `Clone` (`which` is [`Structural::Clone`]) is a copy of its value
    /// or its `PartialEq` is the equality of its value, as the standard library's are for
    /// integers, `bool`, `()`, tuples, references, `str`, slices, arrays, `Vec`, `Option`,
    /// `Result`, `Duration`, `Ordering` and `TryFromIntError` of such types, and as a derive
    /// makes it for a struct or enum whose fields are such types. `method` is the trait's
    /// method (`Clone::clone`, `PartialEq::eq`), looked up for each of the crate's types:
    /// its impl must be the built-in derive (rustc's derive of `Clone` or `PartialEq`, which
    /// clones or compares field by field), not a hand-written one. A float's `==` is not
    /// equality of its value (`NaN != NaN`). Each of the crate's types whose `==` is taken is
    /// put in `self.derives`; `seen` stops a type that holds itself.
    fn structural(
        &mut self,
        method: FunctionId,
        ty: HTy<'db>,
        which: Structural,
        seen: &mut BTreeSet<String>,
        at: &str,
    ) -> Refused<()> {
        let db = self.db;
        let what = match which {
            Structural::Clone => "`Clone::clone`",
            Structural::PartialEq => "`==`",
        };
        match ty.kind() {
            TyKind::Bool | TyKind::Str | TyKind::Never => Ok(()),
            _ if int_ty(ty).is_some() => Ok(()),
            TyKind::Tuple(fields) => {
                for field in fields.iter() {
                    self.structural(method, field, which, seen, at)?;
                }
                Ok(())
            }
            TyKind::Ref(_, inner, mutability) => {
                if which == Structural::Clone && mutability.is_mut() {
                    return Err(self.refuse("`Clone::clone` of a `&mut` reference", at));
                }
                self.structural(method, inner, which, seen, at)
            }
            TyKind::Slice(elem) | TyKind::Array(elem, _) => self.structural(method, elem, which, seen, at),
            TyKind::Float(_) => Err(self.refuse(
                format!("{what} of a floating-point number (rule NoModel; `NaN != NaN`)"),
                at,
            )),
            TyKind::Adt(def, args) => {
                let adt = def.def_id();
                if adt.krate(db).data(db).origin.is_lang() {
                    return match self.ty(ty, false, at)? {
                        Ty::Duration | Ty::Ordering | Ty::TryFromIntError | Ty::Unit => Ok(()),
                        Ty::Option(_) | Ty::Vec(_) => self.structural(method, args.type_at(0), which, seen, at),
                        Ty::Result(..) => {
                            self.structural(method, args.type_at(0), which, seen, at)?;
                            self.structural(method, args.type_at(1), which, seen, at)
                        }
                        other => Err(self.refuse(
                            format!("{what} of `{}`, which has no model (rule NoModel)", crate::rust_ty(&other)),
                            at,
                        )),
                    };
                }
                let lean = self.ty(ty, false, at)?;
                let Ty::Adt(lean) = lean else {
                    return Err(self.refuse(format!("{what} of `{lean:?}`"), at));
                };
                if !seen.insert(lean.clone()) {
                    return Ok(());
                }
                let shown = lean.replace('.', "::");
                let env = self.env;
                let generics = match which {
                    Structural::Clone => GenericArgs::new_from_slice(&[ty.into()]),
                    Structural::PartialEq => GenericArgs::new_from_slice(&[ty.into(), ty.into()]),
                };
                let (wanted, trait_name) = match which {
                    Structural::Clone => (BuiltinDeriveImplTrait::Clone, "Clone"),
                    Structural::PartialEq => (BuiltinDeriveImplTrait::PartialEq, "PartialEq"),
                };
                let derived = match db.lookup_impl_method_or_why(env, method, generics) {
                    Ok((found, _)) => match found.right() {
                        Some((id, _)) => id.loc(db).trait_ == wanted,
                        None => false,
                    },
                    Err(_) => false,
                };
                if !derived {
                    return Err(self.refuse(
                        format!(
                            "{what} of `{shown}`, whose `{trait_name}` is not derived (a hand-written \
                             impl is called as a function, not taken apart field by field; rule \
                             NoModel)"
                        ),
                        at,
                    ));
                }
                let variants: Vec<VariantId> = match adt {
                    AdtId::StructId(it) => vec![VariantId::StructId(it)],
                    AdtId::EnumId(it) => it
                        .enum_variants(db)
                        .variants
                        .iter()
                        .map(|(_, &(variant, _))| VariantId::EnumVariantId(variant))
                        .collect(),
                    AdtId::UnionId(_) => return Err(self.refuse("a union", at)),
                };
                for variant in variants {
                    let fields: Vec<HTy<'db>> = db
                        .field_types(variant)
                        .iter()
                        .map(|(_, field)| field.ty().instantiate(self.interner, args).skip_norm_wip())
                        .collect();
                    for field in fields {
                        self.structural(method, field, which, seen, at)?;
                    }
                }
                if which == Structural::PartialEq {
                    self.derives.insert(lean);
                }
                Ok(())
            }
            _ => Err(self.refuse(
                format!("{what} of `{}`, which has no model (rule NoModel)", blaze_query::db::attach(db, || format!("{ty:?}"))),
                at,
            )),
        }
    }

    fn operands(&mut self, operands: &[m::Operand], at: &str) -> Refused<Vec<Operand>> {
        let mut out = Vec::with_capacity(operands.len());
        for operand in operands {
            out.push(self.operand(operand, at)?.0);
        }
        Ok(out)
    }

    fn goto(&self, target: Option<usize>, at: &str) -> Refused<Exit<Cond>> {
        match target {
            Some(target) => Ok(Exit::Goto(target)),
            None => Err(self.refuse("a call that never returns and is not a panic", at)),
        }
    }

    /// The `&mut` temporaries of the blocks `reached`: see the module.
    fn find_aliases(&mut self, reached: &[m::BasicBlockId]) {
        let mir = self.mir;
        let mut assigned: BTreeMap<Local, u32> = BTreeMap::new();
        // What each candidate stands for, and whether it is a `Vec`'s element by an index.
        let mut candidate: BTreeMap<Local, (m::StoredPlace, bool)> = BTreeMap::new();
        let is_mut_ref =
            |local: Local, locals: &[LocalDecl]| matches!(locals[local as usize].ty, Ty::RefMut(_));
        for &block in reached {
            let data = &mir.basic_blocks[block];
            for statement in &data.statements {
                let m::StatementKind::Assign(place, rvalue) = &statement.kind else { continue };
                if !place.projection.is_empty() {
                    continue;
                }
                let local = index(place.local);
                *assigned.entry(local).or_default() += 1;
                let target = match rvalue {
                    m::Rvalue::Ref(m::BorrowKind::Mut { .. }, borrowed) => Some(borrowed.clone()),
                    m::Rvalue::Use(m::Operand {
                        kind: m::OperandKind::Move(from) | m::OperandKind::Copy(from),
                        ..
                    })
                    | m::Rvalue::CopyForDeref(from)
                        if from.projection.is_empty()
                            && is_mut_ref(index(from.local), &self.locals) =>
                    {
                        Some(from.as_ref().project(m::ProjectionElem::Deref).store())
                    }
                    _ => None,
                };
                if let Some(target) = target {
                    candidate.insert(local, (target, false));
                }
            }
            if let Some(m::Terminator {
                kind: m::TerminatorKind::Call { func, args, destination, .. }, ..
            }) = &data.terminator
                && destination.projection.is_empty()
            {
                let local = index(destination.local);
                *assigned.entry(local).or_default() += 1;
                if let Some(target) = self.lends_into(func, args) {
                    candidate.insert(local, target);
                }
            }
        }
        let params: BTreeSet<Local> = mir.param_locals.iter().map(|it| index(*it)).collect();
        for (local, (place, indexed)) in candidate {
            if assigned.get(&local) == Some(&1)
                && !params.contains(&local)
                && is_mut_ref(local, &self.locals)
            {
                // The index, copied at the call into a local of its own that nothing else
                // assigns, so a later write of the index's own local does not move the place.
                let index = indexed.then(|| {
                    self.locals.push(LocalDecl { name: None, ty: Ty::Int(IntTy::Usize) });
                    (self.locals.len() - 1) as Local
                });
                self.alias.insert(local, Alias { place, index });
            }
        }
    }
}

/// The successors of a terminator that are not unwinding.
fn normal_successors(kind: &m::TerminatorKind) -> Vec<m::BasicBlockId> {
    match kind {
        m::TerminatorKind::Goto { target }
        | m::TerminatorKind::FalseEdge { real_target: target, .. }
        | m::TerminatorKind::FalseUnwind { real_target: target, .. }
        | m::TerminatorKind::Drop { target, .. }
        | m::TerminatorKind::DropAndReplace { target, .. }
        | m::TerminatorKind::Assert { target, .. } => vec![*target],
        m::TerminatorKind::SwitchInt { targets, .. } => targets.all_targets().to_vec(),
        m::TerminatorKind::Call { target, .. } => target.iter().copied().collect(),
        _ => Vec::new(),
    }
}

/// The structured MIR of `instance`, from its function's built MIR (monomorphized at its
/// arguments for an instance of a generic function). The caller has checked that the function
/// type-checks and borrow-checks.
pub(crate) fn from_mir<'db>(db: &'db Db, instance: Instance<'db>) -> Result<Lowered<'db>, NotTranslated> {
    let func = instance.func;
    let unnamed = function_path(db, func).join("::");
    let refuse_unnamed = |what: &str| NotTranslated {
        function: unnamed.clone(),
        what: what.to_owned(),
        at: format!("fn {unnamed}"),
    };
    // A function with type or const parameters is translated at an instance only, and a theorem
    // at none: its statement is closed.
    let args = match is_generic(db, func) {
        false => None,
        true if AttrFlags::query(db, AttrDefId::FunctionId(func)).contains(AttrFlags::THEOREM) => {
            return Err(refuse_unnamed(GENERIC_THEOREM));
        }
        true => match instance.args {
            Some(args) => Some(args),
            None => return Err(refuse_unnamed(GENERIC_UNASKED)),
        },
    };
    let named = match instance_name(db, func, args) {
        Ok(named) => named,
        Err(why) => return Err(refuse_unnamed(&why)),
    };
    let display = named.display;
    let fn_text = format!("fn {display}");
    let refuse = |what: String| NotTranslated {
        function: display.clone(),
        what,
        at: fn_text.clone(),
    };
    let sig = FunctionSignature::of(db, func);
    if sig.is_async() {
        return Err(refuse("an `async` function (rule Async)".to_owned()));
    }
    if sig.is_unsafe() {
        return Err(refuse("an `unsafe fn` (rule Unsafe)".to_owned()));
    }
    if !sig.has_body() {
        return Err(refuse("a function without a body (rule Ffi)".to_owned()));
    }
    let def = DefWithBodyId::FunctionId(func);
    // An instance's calls of a trait's methods are resolved with no where clauses in the
    // crate its chain began at, as codegen resolves them; a function's own, in its own
    // environment.
    let instance_env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: instance.krate };
    let built = match args {
        None => db.mir_body(def.into()),
        Some(args) => db.monomorphized_mir_body(def.into(), args.store(), instance_env.store()),
    };
    let mir = match built {
        Ok(mir) => mir,
        Err(err) => return Err(refuse(format!("the body, which has no MIR ({err:?})"))),
    };
    if mir.coroutine.is_some() {
        return Err(refuse("an `async` block (rule Async)".to_owned()));
    }
    let krate = func.krate(db);
    let env = match args {
        None => ParamEnvAndCrate { param_env: db.trait_environment(mir.owner.generic_def(db)), krate },
        Some(_) => instance_env,
    };
    let flag = cfg::CfgAtom::Flag(intern::sym::overflow_checks);
    let checks_overflow = mir.inherits_overflow_checks
        || (!krate.data(db).origin.is_lang() && krate.cfg_options(db).check_atom(&flag));
    let store = ExpressionStore::of(db, mir.owner.expression_store_owner(db));
    let mut cx = Cx {
        db,
        env,
        interner: DbInterner::new_with(db, krate),
        display: display.clone(),
        mir,
        store,
        locals: Vec::new(),
        alias: BTreeMap::new(),
        adts: Vec::new(),
        callees: Vec::new(),
        checks_overflow,
        unusable: BTreeMap::new(),
        duration: None,
        derives: BTreeSet::new(),
    };
    let mut names: BTreeMap<Local, String> = BTreeMap::new();
    for (binding, &local) in mir.binding_locals.iter() {
        names.insert(index(local), store[binding].name.as_str().to_owned());
    }
    let params: BTreeSet<Local> = mir.param_locals.iter().map(|it| index(*it)).collect();
    for (local, decl) in mir.locals.iter() {
        let ty = match cx.ty(decl.ty.as_ref(), true, &fn_text) {
            Ok(ty) => ty,
            // The return place and the parameters are the model's signature: refused here.
            Err(err) if index(local) == 0 || params.contains(&index(local)) => return Err(err),
            Err(err) => {
                cx.unusable.insert(index(local), err.what);
                // Never read: `place` refuses the local first.
                Ty::Unit
            }
        };
        debug_assert_eq!(index(local) as usize, cx.locals.len());
        cx.locals.push(LocalDecl { name: names.get(&index(local)).cloned(), ty });
    }
    // Every closure is built into a local of its type, which `place` refuses where it is built.
    if !mir.closures.is_empty() && !cx.unusable.values().any(|it| it == "a closure") {
        return Err(refuse("a closure".to_owned()));
    }
    if matches!(cx.locals[0].ty, Ty::RefMut(_)) {
        return Err(refuse(
            "a function that returns a `&mut` reference (it needs a backward function)".to_owned(),
        ));
    }
    // The blocks reached without unwinding, in the order first reached.
    let mut number: BTreeMap<u32, usize> = BTreeMap::new();
    let mut reached = Vec::new();
    let mut queue = VecDeque::from([mir.start_block]);
    number.insert(mir.start_block.into_raw().into_u32(), 0);
    while let Some(block) = queue.pop_front() {
        reached.push(block);
        let Some(terminator) = &mir.basic_blocks[block].terminator else {
            return Err(refuse("a block without a terminator".to_owned()));
        };
        for next in normal_successors(&terminator.kind) {
            let raw = next.into_raw().into_u32();
            if !number.contains_key(&raw) {
                number.insert(raw, number.len());
                queue.push_back(next);
            }
        }
    }
    cx.find_aliases(&reached);
    let to = |block: m::BasicBlockId| number[&block.into_raw().into_u32()];
    let mut blocks: Vec<CfgBlock<Stmt, Cond>> = Vec::with_capacity(reached.len());
    for &block in &reached {
        let data = &mir.basic_blocks[block];
        let mut statements = Vec::new();
        for statement in &data.statements {
            cx.statement(statement, &mut statements)?;
        }
        let terminator = data.terminator.as_ref().expect("checked above");
        let at = cx.at(terminator.span);
        // A type with a destructor is refused with the locals, so a drop does nothing, except
        // of a local whose refusal was put off to where it is named: that is refused here.
        if let m::TerminatorKind::Drop { place, .. } = &terminator.kind
            && let Some(what) = cx.unusable.get(&index(place.local))
        {
            return Err(cx.refuse(what.clone(), &at));
        }
        let exit = match &terminator.kind {
            m::TerminatorKind::Goto { target }
            | m::TerminatorKind::FalseEdge { real_target: target, .. }
            | m::TerminatorKind::FalseUnwind { real_target: target, .. }
            | m::TerminatorKind::Drop { target, .. } => Exit::Goto(to(*target)),
            m::TerminatorKind::SwitchInt { discr, targets } => {
                let (on, ty) = cx.operand(discr, &at)?;
                let ty = cx.op_ty(&ty, &at)?;
                Exit::Switch {
                    on: Cond::Operand(on, ty),
                    targets: targets.iter().map(|(value, target)| (value, to(target))).collect(),
                    otherwise: to(targets.otherwise()),
                }
            }
            m::TerminatorKind::Return => Exit::Return,
            m::TerminatorKind::Unreachable => Exit::End(End::Unreachable),
            m::TerminatorKind::Call { func, args, destination, target, intrinsic, .. } => {
                if intrinsic.is_some() {
                    return Err(cx.refuse("a call of an intrinsic", &at));
                }
                cx.call(func, args, destination, (*target).map(&to), &at, &mut statements)?
            }
            m::TerminatorKind::Assert { cond, expected, msg, target, .. } => {
                let check = match msg {
                    m::AssertKind::Overflow(..) | m::AssertKind::OverflowNeg(_) => Check::Overflow,
                    m::AssertKind::BoundsCheck { .. } => Check::Bounds,
                    m::AssertKind::DivisionByZero(_) | m::AssertKind::RemainderByZero(_) => {
                        Check::DivisionByZero
                    }
                    m::AssertKind::MisalignedPointerDereference { .. }
                    | m::AssertKind::NullPointerDereference
                    | m::AssertKind::NullReferenceConstructed => {
                        return Err(cx.refuse("a check of a raw pointer (rule RawPointer)", &at));
                    }
                    m::AssertKind::InvalidEnumConstruction(_) => Check::Other,
                };
                let (cond, _) = cx.operand(cond, &at)?;
                statements.push(Stmt::Assert { cond, expected: *expected, check, at });
                Exit::Goto(to(*target))
            }
            m::TerminatorKind::UnwindResume | m::TerminatorKind::Abort => {
                return Err(cx.refuse("unwinding reached without a panic", &at));
            }
            m::TerminatorKind::DropAndReplace { .. } => {
                return Err(cx.refuse("a `DropAndReplace` terminator", &at));
            }
            m::TerminatorKind::TailCall { .. } => {
                return Err(cx.refuse("a tail call (`become`)", &at));
            }
            m::TerminatorKind::Yield { .. } | m::TerminatorKind::CoroutineDrop => {
                return Err(cx.refuse("a coroutine (rule Async)", &at));
            }
            m::TerminatorKind::ComptimeFor { .. } | m::TerminatorKind::ComptimeNext { .. } => {
                return Err(cx.refuse("a `for const` loop", &at));
            }
        };
        blocks.push(CfgBlock { statements, exit });
    }
    let cfg = Cfg { blocks, entry: 0 };
    let nodes = match structured::reconstruct(&cfg) {
        Ok(nodes) => fold(nodes),
        Err(err) => return Err(refuse(err.to_string())),
    };
    let body = Body {
        name: named.lean,
        display,
        locals: cx.locals,
        params: mir.param_locals.iter().map(|it| index(*it)).collect(),
        adts: cx.adts,
        nodes,
    };
    Ok(Lowered { body, callees: cx.callees, derives: cx.derives })
}
