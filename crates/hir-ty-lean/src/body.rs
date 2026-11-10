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
//!   ([`Cond::Discriminant`]).
//!
//! Anything outside v0 is a [`NotTranslated`] naming the construct and where it is.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use base_db::Db;
use hir_def::{
    AdtId, CallableDefId, DefWithBodyId, FunctionId, HasModule as _, ItemContainerId, Lookup as _,
    ModuleId, VariantId,
    expr_store::{ExpressionStore, pretty::print_expr_hir},
    item_tree::FieldsShape,
    signatures::{EnumSignature, FunctionSignature, StructSignature},
};
use hir_ty::{
    db::HirDatabase as _,
    mir as m,
    next_solver::{ConstKind, DbInterner, Ty as HTy, TyKind},
};
use rustc_type_ir::{
    IntTy as RIntTy, UintTy,
    inherent::{AdtDef as _, GenericArgs as _, IntoKind as _, SliceLike as _},
};

use crate::{
    NotTranslated,
    structured::{self, Cfg, CfgBlock, End, Exit, Node},
};

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
}

impl Ty {
    /// The type under any references.
    pub fn value(&self) -> &Ty {
        match self {
            Ty::Ref(inner) | Ty::RefMut(inner) => inner.value(),
            other => other,
        }
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
    /// `monadic` when it gives a `Result`.
    Model { name: &'static str, monadic: bool },
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
            Rvalue::Len(_) | Rvalue::Discriminant(..) => Vec::new(),
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
                        Node::Branch { on, arms, otherwise }
                    }
                    None => Node::Branch { on, arms, otherwise },
                }
            }
            Node::Block(body) => Node::Block(fold(body)),
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

/// The methods of core's integers the Lean library models, all total.
const INT_MODELS: &[(&str, &str)] = &[
    ("wrapping_add", "Scalar.wrapping_add"),
    ("wrapping_sub", "Scalar.wrapping_sub"),
    ("wrapping_mul", "Scalar.wrapping_mul"),
    ("saturating_add", "Scalar.saturating_add"),
    ("saturating_sub", "Scalar.saturating_sub"),
];

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

/// `func`'s path, its segments: the crate, the modules, the name.
pub(crate) fn function_path(db: &Db, func: FunctionId) -> Vec<String> {
    let mut path = module_path(db, func.module(db));
    path.push(FunctionSignature::of(db, func).name.as_str().to_owned());
    path
}

fn index(local: m::LocalId) -> Local {
    local.into_raw().into_u32()
}

struct Cx<'db> {
    db: &'db Db,
    interner: DbInterner<'db>,
    display: String,
    mir: &'db m::MirBody<'db>,
    store: &'db ExpressionStore,
    locals: Vec<LocalDecl>,
    /// A `&mut` temporary and the place it stands for.
    alias: BTreeMap<Local, m::StoredPlace>,
    adts: Vec<AdtDecl>,
    callees: Vec<(String, FunctionId)>,
    checks_overflow: bool,
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
        let mut local = stored.local;
        let mut elems: Vec<m::PlaceElem> = stored.projection.as_slice().to_vec();
        // Each hop goes to a local assigned before, so the locals bound the hops.
        for _ in 0..=self.locals.len() {
            let Some(target) = self.alias.get(&index(local)) else { break };
            let rest: &[m::PlaceElem] = match elems.split_first() {
                Some((m::ProjectionElem::Deref, rest)) => rest,
                None => &[],
                Some(_) => return Err(self.refuse("a projection of a `&mut` reference", at)),
            };
            let mut next = target.projection.as_slice().to_vec();
            next.extend_from_slice(rest);
            local = target.local;
            elems = next;
        }
        let mut ty = self.locals[index(local) as usize].ty.clone();
        let mut out = Vec::with_capacity(elems.len());
        let mut at_elem = 0;
        while at_elem < elems.len() {
            let elem = elems[at_elem];
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
                    let Some(m::ProjectionElem::Field(field)) = elems.get(at_elem).copied() else {
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
                (m::ProjectionElem::Index(by), Ty::Slice(elem) | Ty::Array(elem, _)) => {
                    (Elem::Index(index(by)), (**elem).clone())
                }
                (
                    m::ProjectionElem::ConstantIndex { offset, from_end: false, .. },
                    Ty::Slice(elem) | Ty::Array(elem, _),
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
        Ok((Place { local: index(local), elems: out }, ty))
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
                    Ty::Slice(_) => Rvalue::Len(place),
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
            let model = match container {
                ItemContainerId::ImplId(imp) => match db.impl_self_ty(imp).skip_binder().kind() {
                    TyKind::Int(_) | TyKind::Uint(_) => {
                        INT_MODELS.iter().find(|(method, _)| *method == name).map(|it| it.1)
                    }
                    TyKind::Slice(_) if name == "len" => Some("Slice.len"),
                    _ => None,
                },
                _ => None,
            };
            let Some(model) = model else {
                return Err(self.refuse(
                    format!(
                        "a call of the standard library's `{name}`, which has no model (rule \
                         NoModel)"
                    ),
                    at,
                ));
            };
            Callee::Model { name: model, monadic: false }
        } else {
            if !generics.is_empty() {
                return Err(self.refuse(format!("a call of the generic function `{name}`"), at));
            }
            match container {
                ItemContainerId::ModuleId(_) => {}
                ItemContainerId::ExternBlockId(_) => {
                    return Err(self.refuse(format!("a call of the extern function `{name}` (rule Ffi)"), at));
                }
                _ => {
                    return Err(self.refuse(format!("a call of the method `{name}`"), at));
                }
            }
            let lean = function_path(db, callee).join(".");
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
        let mut candidate: BTreeMap<Local, m::StoredPlace> = BTreeMap::new();
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
                    candidate.insert(local, target);
                }
            }
            if let Some(m::Terminator { kind: m::TerminatorKind::Call { destination, .. }, .. }) =
                &data.terminator
                && destination.projection.is_empty()
            {
                *assigned.entry(index(destination.local)).or_default() += 1;
            }
        }
        let params: BTreeSet<Local> = mir.param_locals.iter().map(|it| index(*it)).collect();
        for (local, target) in candidate {
            if assigned.get(&local) == Some(&1)
                && !params.contains(&local)
                && is_mut_ref(local, &self.locals)
            {
                self.alias.insert(local, target);
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
    if !matches!(func.lookup(db).container, ItemContainerId::ModuleId(_)) {
        return Err(refuse("a method or an associated function".to_owned()));
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
    if !mir.closures.is_empty() {
        return Err(refuse("a closure".to_owned()));
    }
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
    };
    let mut names: BTreeMap<Local, String> = BTreeMap::new();
    for (binding, &local) in mir.binding_locals.iter() {
        names.insert(index(local), store[binding].name.as_str().to_owned());
    }
    for (local, decl) in mir.locals.iter() {
        let ty = cx.ty(decl.ty.as_ref(), true, &fn_text)?;
        debug_assert_eq!(index(local) as usize, cx.locals.len());
        cx.locals.push(LocalDecl { name: names.get(&index(local)).cloned(), ty });
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
        let exit = match &terminator.kind {
            m::TerminatorKind::Goto { target }
            | m::TerminatorKind::FalseEdge { real_target: target, .. }
            | m::TerminatorKind::FalseUnwind { real_target: target, .. }
            // A type with a destructor was refused with the locals, so the drop does nothing.
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
        name: path.join("."),
        display,
        locals: cx.locals,
        params: mir.param_locals.iter().map(|it| index(*it)).collect(),
        adts: cx.adts,
        nodes,
    };
    Ok(Lowered { body, callees: cx.callees })
}
