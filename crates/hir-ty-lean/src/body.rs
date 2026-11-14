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
    AdtId, CallableDefId, DefWithBodyId, FunctionId, HasModule as _, ItemContainerId, Lookup as _,
    ModuleId, TraitId, VariantId,
    expr_store::{ExpressionStore, pretty::print_expr_hir},
    item_tree::FieldsShape,
    signatures::{EnumSignature, FunctionSignature, StructSignature, TraitSignature},
};
use hir_ty::{
    db::HirDatabase as _,
    mir as m,
    next_solver::{
        AliasConstKind, ConstKind, DbInterner, FreeConstAliasId, GenericArgs, InherentAssocConstId,
        Ty as HTy, TyKind,
    },
};
use rustc_type_ir::{
    IntTy as RIntTy, UintTy,
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
            Ty::Bool | Ty::Int(_) | Ty::Unit | Ty::Never | Ty::TryFromIntError => {}
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
    pub name: String,
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
            _ => None,
        }
    }
}

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

/// What [`from_mir`] gives: the body, and the functions it calls by their Lean names.
pub(crate) struct Lowered {
    pub(crate) body: Body,
    pub(crate) callees: Vec<(String, FunctionId)>,
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

/// Where a function of the standard library is, which tells its model.
enum Home {
    /// An inherent method of this integer type.
    Int(IntTy),
    Option,
    Result,
    Slice,
    /// An inherent method of `alloc::vec::Vec`.
    Vec,
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

/// `func`'s path, its segments: the crate, the modules, the type for a method or an associated
/// function of an inherent impl, the name.
pub(crate) fn function_path(db: &Db, func: FunctionId) -> Vec<String> {
    let mut path = match owner(db, func) {
        Ok(Some((path, _))) => path,
        // A function refused for its impl is named by its module: the refusal says the rest.
        Ok(None) | Err(_) => module_path(db, func.module(db)),
    };
    path.push(FunctionSignature::of(db, func).name.as_str().to_owned());
    path
}

/// `func`'s name in Lean: its path, dotted. A method lives in its type's namespace
/// (`krate.Counter.bump`), where Lean has already put the type's fields, constructors and
/// what it derives from them; a method of one of those names is `<name>_fn`.
pub(crate) fn function_lean_name(db: &Db, func: FunctionId) -> String {
    let mut path = function_path(db, func);
    if let Ok(Some((_, taken))) = owner(db, func) {
        let name = path.last_mut().expect("a path ends in the function's name");
        if taken.contains(name) || LEAN_DERIVED.contains(&name.as_str()) {
            name.push_str("_fn");
        }
    }
    path.join(".")
}

/// The names Lean declares in the namespace of every structure and inductive type.
const LEAN_DERIVED: &[&str] = &[
    "mk", "rec", "recOn", "casesOn", "noConfusion", "noConfusionType", "below", "brecOn",
    "binductionOn", "ibelow", "sizeOf", "ctorIdx", "toCtorIdx", "ctorElim", "inj", "injEq", "ext",
    "ext_iff",
];

/// The type whose inherent impl holds `func`: its path and the names Lean's declaration of
/// it takes (a struct's fields, an enum's variants). `None` for a function of a module; `Err`
/// names the kind of impl whose functions are not translated.
fn owner(db: &Db, func: FunctionId) -> Result<Option<(Vec<String>, BTreeSet<String>)>, &'static str> {
    let imp = match func.lookup(db).container {
        ItemContainerId::ModuleId(_) => return Ok(None),
        ItemContainerId::ImplId(imp) => imp,
        ItemContainerId::ExternBlockId(_) => return Err("a function without a body (rule Ffi)"),
        _ => return Err("a method of a trait"),
    };
    blaze_query::db::attach(db, || {
        if db.impl_trait(imp).is_some() {
            return Err("a method of a trait impl");
        }
        let TyKind::Adt(def, args) = db.impl_self_ty(imp).skip_binder().kind() else {
            return Err("a method of a type that is not a struct or an enum");
        };
        if !args.is_empty() {
            return Err("a method of a generic type");
        }
        let adt = def.def_id();
        let mut taken = BTreeSet::new();
        let own = match adt {
            AdtId::StructId(it) => {
                let data = VariantId::StructId(it).fields(db);
                let positional = matches!(data.shape, FieldsShape::Tuple);
                for (at, (_, field)) in data.fields().iter().enumerate() {
                    taken.insert(match positional {
                        true => format!("f{at}"),
                        false => field.name.as_str().to_owned(),
                    });
                }
                StructSignature::of(db, it).name.as_str().to_owned()
            }
            AdtId::EnumId(it) => {
                for (variant, _) in it.enum_variants(db).variants.iter() {
                    taken.insert(variant.as_str().to_owned());
                }
                EnumSignature::of(db, it).name.as_str().to_owned()
            }
            AdtId::UnionId(_) => return Err("a method of a union"),
        };
        let mut path = module_path(db, adt.module(db));
        path.push(own);
        Ok(Some((path, taken)))
    })
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
    interner: DbInterner<'db>,
    display: String,
    mir: &'db m::MirBody<'db>,
    store: &'db ExpressionStore,
    locals: Vec<LocalDecl>,
    /// A `&mut` temporary and the place it stands for.
    alias: BTreeMap<Local, Alias>,
    adts: Vec<AdtDecl>,
    callees: Vec<(String, FunctionId)>,
    checks_overflow: bool,
    /// The temporaries and bindings of a type outside the subset, and why: each is refused
    /// where a place names it (with the statement or call that does), not where it is declared,
    /// so one the model never reads (`expect`'s message) is no refusal.
    unusable: BTreeMap<Local, String>,
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
                if !args.is_empty() {
                    return Err(self.refuse("a generic struct or enum", at));
                }
                if def.destructor(self.interner).is_some() {
                    return Err(self.refuse("a type with a destructor (`impl Drop`)", at));
                }
                Ty::Adt(self.adt(adt, at)?)
            }
            TyKind::RawPtr(..) => return Err(self.refuse("a raw pointer (rule RawPointer)", at)),
            TyKind::Float(_) => return Err(self.refuse("a floating-point number", at)),
            TyKind::Char => return Err(self.refuse("the type `char`", at)),
            TyKind::Str => return Err(self.refuse("a string (`str`)", at)),
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

    /// The Lean name of the crate's struct or enum `adt`, declared on first use.
    fn adt(&mut self, adt: AdtId, at: &str) -> Refused<String> {
        let own = match adt {
            AdtId::StructId(it) => StructSignature::of(self.db, it).name.as_str().to_owned(),
            AdtId::EnumId(it) => EnumSignature::of(self.db, it).name.as_str().to_owned(),
            AdtId::UnionId(_) => return Err(self.refuse("a union", at)),
        };
        let mut path = module_path(self.db, adt.module(self.db));
        path.push(own);
        let name = path.join(".");
        if self.adts.iter().any(|it| it.name == name) {
            return Ok(name);
        }
        let kind = match adt {
            AdtId::StructId(it) => AdtKind::Struct(self.fields(VariantId::StructId(it), at)?),
            AdtId::EnumId(it) => {
                let mut variants = Vec::new();
                for (variant_name, &(variant, _)) in it.enum_variants(self.db).variants.iter() {
                    let fields = self.fields(VariantId::EnumVariantId(variant), at)?;
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
        self.adts.push(AdtDecl { name: name.clone(), kind });
        Ok(name)
    }

    /// The fields of `variant`; a positional field `i` is named `f{i}`.
    fn fields(&mut self, variant: VariantId, at: &str) -> Refused<Vec<Field>> {
        let db = self.db;
        let data = variant.fields(db);
        let positional = matches!(data.shape, FieldsShape::Tuple);
        let mut out = Vec::new();
        for (at_field, (local, field)) in db.field_types(variant).iter().enumerate() {
            let ty = self.ty(field.ty().skip_binder(), false, at)?;
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
        let (bits, ty) = match &operand.kind {
            m::OperandKind::Copy(place) | m::OperandKind::Move(place) => {
                let (place, ty) = self.place(place, at)?;
                return Ok((Operand::Place(place), ty));
            }
            m::OperandKind::Constant { konst, ty } => {
                if operand.fn_def().is_some() {
                    return Err(self.refuse("a function used as a value", at));
                }
                let ty = self.ty(ty.as_ref(), false, at)?;
                let bits = match konst.as_ref().kind() {
                    ConstKind::Value(value) => {
                        value.try_to_leaf().map(|leaf| leaf.to_bits_unchecked())
                    }
                    ConstKind::Alias(_, alias) => Some(self.named_const(alias, at)?),
                    _ => None,
                };
                (bits, ty)
            }
            m::OperandKind::Allocation { allocation } => {
                if operand.fn_def().is_some() {
                    return Err(self.refuse("a function used as a value", at));
                }
                let allocation = allocation.as_ref();
                let ty = self.ty(allocation.ty, false, at)?;
                let memory: &[u8] = &allocation.memory;
                // Every target blaze translates for is little-endian.
                let bits = (memory.len() <= 16).then(|| {
                    let mut bytes = [0u8; 16];
                    bytes[..memory.len()].copy_from_slice(memory);
                    u128::from_le_bytes(bytes)
                });
                (bits, ty)
            }
            m::OperandKind::Static(_) => return Err(self.refuse("a static (rule Ffi)", at)),
        };
        let operand = match (&ty, bits) {
            (Ty::Unit, _) => Operand::Unit,
            (Ty::Bool, Some(bits)) => Operand::Bool(bits != 0),
            (Ty::Int(int), Some(bits)) => Operand::Int(*int, bits),
            _ => return Err(self.refuse(format!("a constant of type `{ty:?}`"), at)),
        };
        Ok((operand, ty))
    }

    /// The bits of the named constant `alias` (`u32::MAX`, a crate's `const`), by evaluating
    /// it in its own environment, as at most 16 little-endian bytes.
    fn named_const(&self, alias: hir_ty::next_solver::AliasConst<'db>, at: &str) -> Refused<u128> {
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
        let Ok(allocation) = self.db.const_eval(id, alias.args, None) else {
            return Err(self.refuse("a constant that does not evaluate", at));
        };
        let memory: &[u8] = &allocation.memory;
        if memory.len() > 16 {
            return Err(self.refuse("a constant wider than 16 bytes", at));
        }
        // Every target blaze translates for is little-endian.
        let mut bytes = [0u8; 16];
        bytes[..memory.len()].copy_from_slice(memory);
        Ok(u128::from_le_bytes(bytes))
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
            if !generics.is_empty() {
                return Err(self.refuse(format!("a call of the generic function `{name}`"), at));
            }
            match container {
                ItemContainerId::ExternBlockId(_) => {
                    return Err(self.refuse(format!("a call of the extern function `{name}` (rule Ffi)"), at));
                }
                _ => {
                    if let Err(why) = owner(db, callee) {
                        return Err(self.refuse(format!("a call of `{name}`, {why}"), at));
                    }
                }
            }
            let lean = function_lean_name(db, callee);
            if !self.callees.iter().any(|(_, it)| *it == callee) {
                self.callees.push((lean.clone(), callee));
            }
            Callee::Function { name: lean }
        };
        let (dest, _) = self.place(destination, at)?;
        let args = self.operands(args, at)?;
        out.push(Stmt::Call { dest, callee, args, at: at.to_owned() });
        self.goto(target, at)
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
                match ty.kind() {
                    TyKind::Slice(_) => Home::Slice,
                    TyKind::Adt(def, _) => match def.def_id() {
                        AdtId::EnumId(it) => match EnumSignature::of(db, it).name.as_str() {
                            "Option" => Home::Option,
                            "Result" => Home::Result,
                            _ => Home::Other,
                        },
                        adt if is_vec(db, adt) => Home::Vec,
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
            Home::Trait { name: of, module } => {
                let module: Vec<&str> = module.iter().map(String::as_str).collect();
                match (module.as_slice(), of.as_str(), name) {
                    (["cmp"], "Ord", "max" | "min" | "clamp") => self.int_order(name, args, at).map(Some),
                    (["convert"], "From", "from") | (["convert"], "Into", "into") => {
                        let operands = self.operands(args, at)?;
                        let from = self.operand(&args[0], at)?.1;
                        // A `From` between integers is lossless: the cast keeps the value.
                        let (callee, to) = match (from.value(), self.place(destination, at)?.1) {
                            (Ty::Int(_), Ty::Int(to)) => ("Scalar.cast", to),
                            (Ty::Bool, Ty::Int(to)) => ("Scalar.ofBool", to),
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
        for arg in args {
            let (operand, ty) = self.operand(arg, at)?;
            if !matches!(ty.value(), Ty::Int(_)) {
                return Err(self.refuse(
                    format!("a call of the standard library's `{name}` on a `{ty:?}`, which has no model (rule NoModel)"),
                    at,
                ));
            }
            operands.push(operand);
        }
        let (name, monadic) = match name {
            "max" => ("Scalar.max", false),
            "min" => ("Scalar.min", false),
            // `clamp` panics when its least is above its most.
            _ => ("Scalar.ord_clamp", true),
        };
        Ok((Callee::Model { name, monadic, ty: None, lends_first: false }, operands))
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

/// `func`'s structured MIR, from its built MIR. The caller has checked that the function
/// type-checks and borrow-checks.
pub(crate) fn from_mir(db: &Db, func: FunctionId) -> Result<Lowered, NotTranslated> {
    let path = function_path(db, func);
    let display = path.join("::");
    let fn_text = format!("fn {display}");
    let refuse = |what: String| NotTranslated {
        function: display.clone(),
        what,
        at: fn_text.clone(),
    };
    let sig = FunctionSignature::of(db, func);
    if let Err(why) = owner(db, func) {
        return Err(refuse(why.to_owned()));
    }
    if !sig.generic_params.is_empty() {
        return Err(refuse("a generic function".to_owned()));
    }
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
    let mir = match db.mir_body(def.into()) {
        Ok(mir) => mir,
        Err(err) => return Err(refuse(format!("the body, which has no MIR ({err:?})"))),
    };
    if mir.coroutine.is_some() {
        return Err(refuse("an `async` block (rule Async)".to_owned()));
    }
    let krate = func.krate(db);
    let flag = cfg::CfgAtom::Flag(intern::sym::overflow_checks);
    let checks_overflow = mir.inherits_overflow_checks
        || (!krate.data(db).origin.is_lang() && krate.cfg_options(db).check_atom(&flag));
    let store = ExpressionStore::of(db, mir.owner.expression_store_owner(db));
    let mut cx = Cx {
        db,
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
        name: function_lean_name(db, func),
        display,
        locals: cx.locals,
        params: mir.param_locals.iter().map(|it| index(*it)).collect(),
        adts: cx.adts,
        nodes,
    };
    Ok(Lowered { body, callees: cx.callees })
}
