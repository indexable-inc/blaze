//! MIR, embedded: a function's `MirBody` as a term of the Lean types in
//! `lean/Blaze/Mir/Syntax.lean`, which `Blaze.Mir.Step` gives a meaning
//! (docs/v2/plan/design/formal-verification.md, "The semantics of MIR").
//!
//! Unlike structured MIR ([`super`]), this keeps the body as blaze has it: its blocks, its
//! locals, its references, its moves, its storage markers. What it changes is small and each
//! is listed in the doc as trusted:
//!
//! - Only the blocks reached without unwinding are written, numbered in the order a
//!   breadth-first walk from the entry reaches them. A panic ends the run, so no cleanup block
//!   runs.
//! - `FalseEdge` and `FalseUnwind` are the `goto` they are once borrow checking has read them;
//!   a `FakeRead` is a `nop`; `CopyForDeref` is a copy.
//! - A call of one of core's panic entry points is the intrinsic `panic` (its message is not
//!   kept); core's `wrapping_*`, `saturating_*` on integers and `len` on slices are intrinsics,
//!   as the functional models have them; a tuple struct's or a variant's constructor function
//!   is the intrinsic `construct`.
//! - A tuple type is declared like a struct, `Option` and `Result` like enums, `!` as an enum
//!   of no variant.
//!
//! The program written is the function at index 0, then each function it calls (built MIR),
//! in the order first called. Anything outside the subset is a [`NotTranslated`] naming it.
//! Writing a function twice gives the same bytes: nothing here iterates a hash map.

use hir_def::{HasModule as _, Lookup as _};
use hir_ty::db::HirDatabase as _;
use rustc_type_ir::inherent::IntoKind as _;

use super::*;

/// Core's panic entry points: a call of one is the intrinsic `panic`.
const PANIC_ENTRIES: &[&str] = &[
    "panic",
    "panic_fmt",
    "const_panic_fmt",
    "panic_nounwind",
    "panic_explicit",
    "panic_display",
    "panic_default_msg",
    "panic_str_2015",
    "unreachable_display",
    "panic_bounds_check",
];

/// The methods of core's integers the semantics has as intrinsics.
const INT_INTRINSICS: &[(&str, &str)] = &[
    ("wrapping_add", "Intrinsic.wrappingAdd"),
    ("wrapping_sub", "Intrinsic.wrappingSub"),
    ("wrapping_mul", "Intrinsic.wrappingMul"),
    ("saturating_add", "Intrinsic.saturatingAdd"),
    ("saturating_sub", "Intrinsic.saturatingSub"),
];

/// A function's MIR as a Lean module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Embedded {
    /// The function, `krate::module::name`.
    pub function: String,
    /// The Lean constant, `krate.module.name.mir : Blaze.Mir.Program`.
    pub name: String,
    /// The module, `BlazeMir.krate.module.name`.
    pub module: String,
    /// The term alone, a `Blaze.Mir.Program`.
    pub term: String,
    /// The module's text.
    pub text: String,
}

impl Embedded {
    /// The module's path under the directory embedded bodies are written in.
    pub fn path(&self) -> String {
        format!("{}.lean", self.module.replace('.', "/"))
    }
}

/// `term` as an argument: in parentheses unless it is one word.
fn arg(term: &str) -> String {
    if term.contains(' ') { format!("({term})") } else { term.to_owned() }
}

/// A number as an argument or a pair's part: a negative one in parentheses.
fn number(text: &str) -> String {
    if text.starts_with('-') { format!("({text})") } else { text.to_owned() }
}

fn list(items: &[String]) -> String {
    format!("[{}]", items.join(", "))
}

fn scalar(ty: IntTy) -> String {
    format!("ScalarTy.{}", ty.lean())
}

/// The types and functions of one program, as they are found.
struct Program {
    /// Each declared type by its key, and its Lean declaration: an index here is an `adt` id.
    adts: Vec<(String, String)>,
    /// The functions, the one asked for first: an index here is a function's id.
    functions: Vec<FunctionId>,
}

impl Program {
    fn function(&mut self, func: FunctionId) -> usize {
        match self.functions.iter().position(|it| *it == func) {
            Some(at) => at,
            None => {
                self.functions.push(func);
                self.functions.len() - 1
            }
        }
    }

    /// The id of the declared type `key`, declared on first use with `variants`: each a
    /// discriminant and its fields' types.
    fn declare(&mut self, key: String, variants: Vec<(String, Vec<String>)>) -> usize {
        if let Some(at) = self.adts.iter().position(|(it, _)| *it == key) {
            return at;
        }
        let variants: Vec<String> = variants
            .into_iter()
            .map(|(discriminant, fields)| format!("({}, {})", discriminant, list(&fields)))
            .collect();
        let decl = format!(
            "{{ name := \"{}\", discr := ScalarTy.Isize, variants := {} }}",
            key.replace('\\', "\\\\").replace('"', "\\\""),
            list(&variants)
        );
        self.adts.push((key, decl));
        self.adts.len() - 1
    }
}

/// One body being written.
struct Writer<'p, 'db> {
    cx: Cx<'db>,
    program: &'p mut Program,
    /// The body's phase is built: its checked operations and its index panic.
    built: bool,
}

impl<'db> Writer<'_, 'db> {
    /// A name for `ty`, for the declaration of a type that holds it.
    fn key(ty: &Ty) -> String {
        match ty {
            Ty::Bool => "bool".to_owned(),
            Ty::Int(int) => int.lean().to_lowercase(),
            Ty::Unit => "()".to_owned(),
            Ty::Never => "!".to_owned(),
            Ty::Tuple(fields) => {
                format!("({})", fields.iter().map(Self::key).collect::<Vec<_>>().join(", "))
            }
            Ty::Adt(name) => name.replace('.', "::"),
            Ty::Option(inner) => format!("Option<{}>", Self::key(inner)),
            Ty::Result(ok, err) => format!("Result<{}, {}>", Self::key(ok), Self::key(err)),
            Ty::Ref(inner) => format!("&{}", Self::key(inner)),
            Ty::RefMut(inner) => format!("&mut {}", Self::key(inner)),
            Ty::Slice(elem) => format!("[{}]", Self::key(elem)),
            Ty::Array(elem, len) => format!("[{}; {len}]", Self::key(elem)),
        }
    }

    /// The id of the declared type `ty` is: a struct, an enum, a tuple, `Option`, `Result`
    /// or `!`.
    fn adt(&mut self, ty: &Ty, at: &str) -> Refused<usize> {
        let key = Self::key(ty);
        if let Some(found) = self.program.adts.iter().position(|(it, _)| *it == key) {
            return Ok(found);
        }
        let variants: Vec<(String, Vec<Ty>)> = match ty {
            Ty::Never => Vec::new(),
            Ty::Tuple(fields) => vec![("0".to_owned(), fields.clone())],
            Ty::Adt(name) => {
                match self.cx.adts.iter().find(|it| it.name == *name).map(|it| it.kind.clone()) {
                    Some(AdtKind::Struct(fields)) => {
                        vec![("0".to_owned(), fields.into_iter().map(|it| it.ty).collect())]
                    }
                    Some(AdtKind::Enum(variants)) => variants
                        .into_iter()
                        .map(|it| {
                            let fields = it.fields.into_iter().map(|field| field.ty).collect();
                            (number(&(it.discriminant as i128).to_string()), fields)
                        })
                        .collect(),
                    None => return Err(self.cx.refuse(format!("the type `{key}`, not declared"), at)),
                }
            }
            Ty::Option(_) | Ty::Result(..) => match self.cx.variants(ty) {
                Some(variants) => variants
                    .into_iter()
                    .map(|it| {
                        let fields = it.fields.into_iter().map(|field| field.ty).collect();
                        (it.discriminant.to_string(), fields)
                    })
                    .collect(),
                None => return Err(self.cx.refuse(format!("the type `{key}`"), at)),
            },
            other => {
                return Err(self.cx.refuse(format!("`{other:?}` used as a struct or an enum"), at));
            }
        };
        // The fields' types first, so a type is declared after those it holds.
        let mut written = Vec::with_capacity(variants.len());
        for (discriminant, fields) in variants {
            let mut tys = Vec::with_capacity(fields.len());
            for field in &fields {
                tys.push(self.ty(field, at)?);
            }
            written.push((discriminant, tys));
        }
        Ok(self.program.declare(key, written))
    }

    fn ty(&mut self, ty: &Ty, at: &str) -> Refused<String> {
        Ok(match ty {
            Ty::Bool => "Ty.bool".to_owned(),
            Ty::Int(int) => format!("Ty.int {}", scalar(*int)),
            Ty::Unit => "Ty.unit".to_owned(),
            Ty::Never | Ty::Tuple(_) | Ty::Adt(_) | Ty::Option(_) | Ty::Result(..) => {
                format!("Ty.adt {}", self.adt(ty, at)?)
            }
            Ty::Ref(inner) => format!("Ty.ref false {}", arg(&self.ty(inner, at)?)),
            Ty::RefMut(inner) => format!("Ty.ref true {}", arg(&self.ty(inner, at)?)),
            Ty::Slice(elem) => format!("Ty.slice {}", arg(&self.ty(elem, at)?)),
            Ty::Array(elem, len) => format!("Ty.array {} {len}", arg(&self.ty(elem, at)?)),
        })
    }

    /// The place as MIR has it, and its type.
    fn place(&mut self, stored: &m::StoredPlace, at: &str) -> Refused<(String, Ty)> {
        // The structured reading refuses what the subset has no projection for, and types it.
        let (_, ty) = self.cx.place(stored, at)?;
        let mut elems = Vec::new();
        for elem in stored.projection.as_slice() {
            elems.push(match *elem {
                m::ProjectionElem::Deref => "ProjElem.deref".to_owned(),
                m::ProjectionElem::Field(field) => format!("ProjElem.field {}", field.0),
                m::ProjectionElem::Downcast(VariantId::EnumVariantId(variant)) => {
                    format!("ProjElem.downcast {}", variant.index(self.cx.db))
                }
                m::ProjectionElem::Index(by) => format!("ProjElem.index {}", index(by)),
                m::ProjectionElem::ConstantIndex { offset, from_end: false, .. } => {
                    format!("ProjElem.constIndex {offset}")
                }
                other => {
                    return Err(self.cx.refuse(format!("the projection `{other:?}`"), at));
                }
            });
        }
        Ok((format!("(Place.mk {} {})", index(stored.local), list(&elems)), ty))
    }

    fn operand(&mut self, operand: &m::Operand, at: &str) -> Refused<(String, Ty)> {
        match &operand.kind {
            m::OperandKind::Copy(place) => {
                let (place, ty) = self.place(place, at)?;
                Ok((format!("Operand.copy {place}"), ty))
            }
            m::OperandKind::Move(place) => {
                let (place, ty) = self.place(place, at)?;
                Ok((format!("Operand.move {place}"), ty))
            }
            _ => {
                let (value, ty) = self.cx.operand(operand, at)?;
                let constant = match value {
                    Operand::Int(int, bits) => {
                        format!("Const.int {} {}", scalar(int), number(&int.decimal(bits)))
                    }
                    Operand::Bool(value) => format!("Const.bool {value}"),
                    Operand::Unit => "Const.unit".to_owned(),
                    Operand::Place(_) => {
                        return Err(self.cx.refuse("a constant that is a place", at));
                    }
                };
                Ok((format!("Operand.const {}", arg(&constant)), ty))
            }
        }
    }

    fn operands(&mut self, operands: &[m::Operand], at: &str) -> Refused<String> {
        let mut out = Vec::with_capacity(operands.len());
        for operand in operands {
            out.push(self.operand(operand, at)?.0);
        }
        Ok(list(&out))
    }

    /// `rvalue` assigned to a place of type `dest`.
    fn rvalue(&mut self, rvalue: &m::Rvalue, dest: &Ty, at: &str) -> Refused<String> {
        Ok(match rvalue {
            m::Rvalue::Use(operand) => format!("Rvalue.use {}", arg(&self.operand(operand, at)?.0)),
            m::Rvalue::CopyForDeref(place) => {
                format!("Rvalue.use (Operand.copy {})", self.place(place, at)?.0)
            }
            m::Rvalue::Ref(kind, place) => {
                let mutable = matches!(kind, m::BorrowKind::Mut { .. });
                format!("Rvalue.ref {mutable} {}", self.place(place, at)?.0)
            }
            m::Rvalue::BinaryOp(op, left, right) | m::Rvalue::CheckedBinaryOp(op, left, right) => {
                let (left, left_ty) = self.operand(left, at)?;
                let (right, _) = self.operand(right, at)?;
                let on = self.cx.op_ty(&left_ty, at)?;
                let (name, overflowing) = match op {
                    m::BinOp::Add => ("add", false),
                    m::BinOp::Sub => ("sub", false),
                    m::BinOp::Mul => ("mul", false),
                    m::BinOp::Div => ("div", false),
                    m::BinOp::Rem => ("rem", false),
                    m::BinOp::BitXor => ("bitXor", false),
                    m::BinOp::BitAnd => ("bitAnd", false),
                    m::BinOp::BitOr => ("bitOr", false),
                    m::BinOp::Shl => ("shl", false),
                    m::BinOp::Shr => ("shr", false),
                    m::BinOp::Eq => ("eq", false),
                    m::BinOp::Lt => ("lt", false),
                    m::BinOp::Le => ("le", false),
                    m::BinOp::Ne => ("ne", false),
                    m::BinOp::Ge => ("ge", false),
                    m::BinOp::Gt => ("gt", false),
                    m::BinOp::AddWithOverflow => ("add", true),
                    m::BinOp::SubWithOverflow => ("sub", true),
                    m::BinOp::MulWithOverflow => ("mul", true),
                    m::BinOp::Offset => {
                        return Err(self.cx.refuse("pointer arithmetic (rule RawPointer)", at));
                    }
                };
                if on == OpTy::Bool
                    && (overflowing || !matches!(name, "eq" | "ne" | "bitXor" | "bitAnd" | "bitOr"))
                {
                    return Err(self.cx.refuse(format!("the operator `{op:?}` on `bool`s"), at));
                }
                let checked = matches!(rvalue, m::Rvalue::CheckedBinaryOp(..))
                    && self.cx.checks_overflow
                    && matches!(name, "add" | "sub" | "mul" | "shl" | "shr");
                let form = if overflowing {
                    "withOverflow"
                } else if checked {
                    "checked"
                } else {
                    "binary"
                };
                format!("Rvalue.{form} BinOp.{name} {} {}", arg(&left), arg(&right))
            }
            m::Rvalue::UnaryOp(op, of) => {
                let (of, of_ty) = self.operand(of, at)?;
                let name = match (op, self.cx.op_ty(&of_ty, at)?) {
                    (m::UnOp::Not, _) => "not",
                    (m::UnOp::Neg, OpTy::Int(_)) if self.built && self.cx.checks_overflow => {
                        "negChecked"
                    }
                    (m::UnOp::Neg, OpTy::Int(_)) => "neg",
                    (m::UnOp::Neg, OpTy::Bool) => {
                        return Err(self.cx.refuse("the negation of a `bool`", at));
                    }
                };
                format!("Rvalue.unary UnOp.{name} {}", arg(&of))
            }
            m::Rvalue::Cast(m::CastKind::IntToInt, of, to) => {
                let (of, of_ty) = self.operand(of, at)?;
                self.cx.op_ty(&of_ty, at)?;
                match self.cx.ty(to.as_ref(), false, at)? {
                    Ty::Int(to) => format!("Rvalue.cast {} {}", arg(&of), scalar(to)),
                    other => return Err(self.cx.refuse(format!("a cast to `{other:?}`"), at)),
                }
            }
            m::Rvalue::Cast(kind, ..) => {
                return Err(self.cx.refuse(format!("a `{kind:?}` cast"), at));
            }
            m::Rvalue::Len(place) => {
                let (place, ty) = self.place(place, at)?;
                match ty.value() {
                    Ty::Slice(_) | Ty::Array(..) => format!("Rvalue.len {place}"),
                    other => {
                        return Err(self.cx.refuse(format!("the length of a `{other:?}`"), at));
                    }
                }
            }
            m::Rvalue::Discriminant(place) => {
                let (place, ty) = self.place(place, at)?;
                let ty = ty.value().clone();
                if self.cx.variants(&ty).is_none() {
                    return Err(self.cx.refuse(format!("the discriminant of a `{ty:?}`"), at));
                }
                if *dest != Ty::Int(IntTy::Isize) {
                    return Err(self.cx.refuse(
                        format!("a discriminant read as `{dest:?}` (an enum with a `repr`)"),
                        at,
                    ));
                }
                format!("Rvalue.discriminant {} {place}", self.adt(&ty, at)?)
            }
            m::Rvalue::Aggregate(kind, operands) => {
                let fields = self.operands(operands, at)?;
                match kind {
                    m::AggregateKind::Tuple(_) if operands.is_empty() => {
                        "Rvalue.use (Operand.const Const.unit)".to_owned()
                    }
                    m::AggregateKind::Tuple(_) => format!("Rvalue.aggregate AggKind.tuple {fields}"),
                    m::AggregateKind::Array(_) => format!("Rvalue.aggregate AggKind.array {fields}"),
                    m::AggregateKind::Adt(variant, _) => {
                        let variant = match variant {
                            VariantId::StructId(_) => 0,
                            VariantId::EnumVariantId(variant) => variant.index(self.cx.db),
                            VariantId::UnionId(_) => return Err(self.cx.refuse("a union", at)),
                        };
                        let adt = self.adt(dest.value(), at)?;
                        format!("Rvalue.aggregate (AggKind.adt {adt} {variant}) {fields}")
                    }
                    m::AggregateKind::Union(..) => return Err(self.cx.refuse("a union", at)),
                    m::AggregateKind::Closure(_) => return Err(self.cx.refuse("a closure", at)),
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
                    return Err(self.cx.refuse("an array whose length is not a number", at));
                };
                format!("Rvalue.replicate {} {len}", arg(&self.operand(of, at)?.0))
            }
            m::Rvalue::ThreadLocalRef(never)
            | m::Rvalue::AddressOf(never)
            | m::Rvalue::NullaryOp(never) => match *never {},
        })
    }

    fn statement(&mut self, statement: &m::Statement) -> Refused<String> {
        let at = self.cx.at(statement.span);
        Ok(match &statement.kind {
            m::StatementKind::Assign(place, rvalue) => {
                let (place, ty) = self.place(place, &at)?;
                format!("Statement.assign {place} ({})", self.rvalue(rvalue, &ty, &at)?)
            }
            m::StatementKind::StorageLive(local) => format!("Statement.storageLive {}", index(*local)),
            m::StatementKind::StorageDead(local) => format!("Statement.storageDead {}", index(*local)),
            // A read only the borrow checker makes.
            m::StatementKind::FakeRead(_) | m::StatementKind::Nop => "Statement.nop".to_owned(),
            m::StatementKind::Deinit(_) => {
                return Err(self.cx.refuse("a `Deinit` statement", &at));
            }
            m::StatementKind::InlineAsm(_) => {
                return Err(self.cx.refuse("inline assembly (rule InlineAsm)", &at));
            }
        })
    }

    /// What a call calls: a function of the program, or an intrinsic.
    fn callee(&mut self, func: &m::Operand, at: &str) -> Refused<String> {
        let Some((callee, generics)) = func.fn_def() else {
            return Err(self.cx.refuse("a call of a function pointer or a closure", at));
        };
        let db = self.cx.db;
        let callee = match callee {
            CallableDefId::FunctionId(callee) => callee,
            CallableDefId::StructId(_) => return Ok("Callee.intrinsic (Intrinsic.construct 0)".to_owned()),
            CallableDefId::EnumVariantId(variant) => {
                return Ok(format!(
                    "Callee.intrinsic (Intrinsic.construct {})",
                    variant.index(db)
                ));
            }
        };
        let name = FunctionSignature::of(db, callee).name.as_str().to_owned();
        let container = callee.lookup(db).container;
        if callee.krate(db).data(db).origin.is_lang() {
            if PANIC_ENTRIES.contains(&name.as_str()) {
                return Ok("Callee.intrinsic Intrinsic.panic".to_owned());
            }
            let intrinsic = match container {
                ItemContainerId::ImplId(imp) => match db.impl_self_ty(imp).skip_binder().kind() {
                    TyKind::Int(_) | TyKind::Uint(_) => {
                        INT_INTRINSICS.iter().find(|(method, _)| *method == name).map(|it| it.1)
                    }
                    TyKind::Slice(_) if name == "len" => Some("Intrinsic.sliceLen"),
                    _ => None,
                },
                _ => None,
            };
            return match intrinsic {
                Some(intrinsic) => Ok(format!("Callee.intrinsic {intrinsic}")),
                None => Err(self.cx.refuse(
                    format!(
                        "a call of the standard library's `{name}`, which the semantics has no \
                         intrinsic for (rule NoModel)"
                    ),
                    at,
                )),
            };
        }
        if !generics.is_empty() {
            return Err(self.cx.refuse(format!("a call of the generic function `{name}`"), at));
        }
        match container {
            ItemContainerId::ModuleId(_) => {}
            ItemContainerId::ExternBlockId(_) => {
                return Err(self.cx.refuse(
                    format!("a call of the extern function `{name}` (rule Ffi)"),
                    at,
                ));
            }
            _ => return Err(self.cx.refuse(format!("a call of the method `{name}`"), at)),
        }
        Ok(format!("Callee.fn {}", self.program.function(callee)))
    }

    fn terminator(
        &mut self,
        terminator: &m::Terminator,
        to: &dyn Fn(m::BasicBlockId) -> usize,
    ) -> Refused<String> {
        let at = self.cx.at(terminator.span);
        Ok(match &terminator.kind {
            m::TerminatorKind::Goto { target }
            | m::TerminatorKind::FalseEdge { real_target: target, .. }
            | m::TerminatorKind::FalseUnwind { real_target: target, .. } => {
                format!("Terminator.goto {}", to(*target))
            }
            // A type with a destructor was refused with the locals.
            m::TerminatorKind::Drop { place, target, .. } => {
                format!("Terminator.drop {} {}", self.place(place, &at)?.0, to(*target))
            }
            m::TerminatorKind::SwitchInt { discr, targets } => {
                let (on, ty) = self.operand(discr, &at)?;
                let ty = self.cx.op_ty(&ty, &at)?;
                let arms: Vec<String> = targets
                    .iter()
                    .map(|(value, target)| {
                        let value = match ty {
                            OpTy::Bool => value.to_string(),
                            OpTy::Int(int) => number(&int.decimal(value)),
                        };
                        format!("({value}, {})", to(target))
                    })
                    .collect();
                format!(
                    "Terminator.switchInt {} {} {}",
                    arg(&on),
                    list(&arms),
                    to(targets.otherwise())
                )
            }
            m::TerminatorKind::Return => "Terminator.ret".to_owned(),
            m::TerminatorKind::Unreachable => "Terminator.unreachable".to_owned(),
            m::TerminatorKind::Call { func, args, destination, target, intrinsic, .. } => {
                if intrinsic.is_some() {
                    return Err(self.cx.refuse("a call of a compiler intrinsic", &at));
                }
                let callee = self.callee(func, &at)?;
                // A panic's message is not kept: its operands are strings and formatters.
                let args = if callee == "Callee.intrinsic Intrinsic.panic" {
                    "[]".to_owned()
                } else {
                    self.operands(args, &at)?
                };
                let target = match target {
                    Some(target) => format!("(some {})", to(*target)),
                    None => "none".to_owned(),
                };
                let (dest, _) = self.place(destination, &at)?;
                format!("Terminator.call {} {args} {dest} {target}", arg(&callee))
            }
            m::TerminatorKind::Assert { cond, expected, msg, target, .. } => {
                let kind = match msg {
                    m::AssertKind::Overflow(..)
                    | m::AssertKind::OverflowNeg(_)
                    | m::AssertKind::DivisionByZero(_)
                    | m::AssertKind::RemainderByZero(_) => "Error.arithmetic",
                    m::AssertKind::BoundsCheck { .. } => "Error.bounds",
                    m::AssertKind::InvalidEnumConstruction(_) => "Error.panic",
                    m::AssertKind::MisalignedPointerDereference { .. }
                    | m::AssertKind::NullPointerDereference
                    | m::AssertKind::NullReferenceConstructed => {
                        return Err(self.cx.refuse("a check of a raw pointer (rule RawPointer)", &at));
                    }
                };
                let (cond, _) = self.operand(cond, &at)?;
                format!("Terminator.assert {} {expected} {kind} {}", arg(&cond), to(*target))
            }
            m::TerminatorKind::UnwindResume | m::TerminatorKind::Abort => {
                return Err(self.cx.refuse("unwinding reached without a panic", &at));
            }
            m::TerminatorKind::DropAndReplace { .. } => {
                return Err(self.cx.refuse("a `DropAndReplace` terminator", &at));
            }
            m::TerminatorKind::TailCall { .. } => {
                return Err(self.cx.refuse("a tail call (`become`)", &at));
            }
            m::TerminatorKind::Yield { .. } | m::TerminatorKind::CoroutineDrop => {
                return Err(self.cx.refuse("a coroutine (rule Async)", &at));
            }
            m::TerminatorKind::ComptimeFor { .. } | m::TerminatorKind::ComptimeNext { .. } => {
                return Err(self.cx.refuse("a `for const` loop", &at));
            }
        })
    }
}

/// `func`'s body `mir` (its built MIR when `None`) as a Lean `Body`, 6 columns in.
fn body<'db>(
    db: &'db Db,
    program: &mut Program,
    func: FunctionId,
    mir: Option<&m::MirBody<'db>>,
) -> Refused<String> {
    let display = function_path(db, func).join("::");
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
    let built = match db.mir_body(def.into()) {
        Ok(mir) => mir,
        Err(err) => return Err(refuse(format!("the body, which has no MIR ({err:?})"))),
    };
    let mir = mir.unwrap_or(built);
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
    let store = ExpressionStore::of(db, built.owner.expression_store_owner(db));
    let cx = Cx {
        db,
        interner: DbInterner::new_with(db, krate),
        display: display.clone(),
        // Spans are read through the built body's owner, which every phase shares.
        mir: built,
        store,
        locals: Vec::new(),
        alias: BTreeMap::new(),
        adts: Vec::new(),
        callees: Vec::new(),
        checks_overflow,
    };
    let mut writer = Writer { cx, program, built: mir.phase == m::MirPhase::Built };
    let mut locals = Vec::with_capacity(mir.locals.len());
    for (local, decl) in mir.locals.iter() {
        let ty = writer.cx.ty(decl.ty.as_ref(), true, &fn_text)?;
        debug_assert_eq!(index(local) as usize, writer.cx.locals.len());
        locals.push(writer.ty(&ty, &fn_text)?);
        writer.cx.locals.push(LocalDecl { name: None, ty });
    }
    // The semantics takes the parameters as locals 1 to n, in order.
    let params: Vec<Local> = mir.param_locals.iter().map(|it| index(*it)).collect();
    if params.iter().enumerate().any(|(at, local)| *local as usize != at + 1) {
        return Err(refuse(format!("parameters that are not locals 1 to n, in order ({params:?})")));
    }
    // The blocks reached without unwinding, in the order first reached.
    let mut numbers: BTreeMap<u32, usize> = BTreeMap::new();
    let mut reached = Vec::new();
    let mut queue = VecDeque::from([mir.start_block]);
    numbers.insert(mir.start_block.into_raw().into_u32(), 0);
    while let Some(block) = queue.pop_front() {
        reached.push(block);
        let Some(terminator) = &mir.basic_blocks[block].terminator else {
            return Err(refuse("a block without a terminator".to_owned()));
        };
        for next in normal_successors(&terminator.kind) {
            let raw = next.into_raw().into_u32();
            if !numbers.contains_key(&raw) {
                numbers.insert(raw, numbers.len());
                queue.push_back(next);
            }
        }
    }
    let to = |block: m::BasicBlockId| numbers[&block.into_raw().into_u32()];
    let mut blocks = Vec::with_capacity(reached.len());
    for &block in &reached {
        let data = &mir.basic_blocks[block];
        let mut statements = Vec::with_capacity(data.statements.len());
        for statement in &data.statements {
            statements.push(writer.statement(statement)?);
        }
        let terminator = writer.terminator(data.terminator.as_ref().expect("checked above"), &to)?;
        let statements = if statements.is_empty() {
            "[]".to_owned()
        } else {
            let lines: Vec<String> = statements.iter().map(|it| format!("              {it}")).collect();
            format!("[\n{}]", lines.join(",\n"))
        };
        blocks.push(format!(
            "          {{ stmts := {statements},\n            terminator := {terminator} }}"
        ));
    }
    Ok(format!(
        "      {{ name := \"{display}\",\n        args := {},\n        locals := {},\n        blocks := [\n{}] }}",
        params.len(),
        list(&locals),
        blocks.join(",\n")
    ))
}

/// The program of `func`, whose own body is `mir` (its built MIR when `None`), then each
/// function it calls: a `Blaze.Mir.Program`, 2 columns in.
pub(crate) fn program<'db>(
    db: &'db Db,
    func: FunctionId,
    mir: Option<&m::MirBody<'db>>,
) -> Refused<String> {
    let mut program = Program { adts: Vec::new(), functions: vec![func] };
    let mut bodies = vec![body(db, &mut program, func, mir)?];
    // Each body may add functions; the list only grows, and a program's functions bound it.
    let mut at = 1;
    while at < program.functions.len() {
        let callee = program.functions[at];
        bodies.push(body(db, &mut program, callee, None)?);
        at += 1;
    }
    let adts: Vec<String> = program.adts.iter().map(|(_, decl)| decl.clone()).collect();
    Ok(format!("  {{ adts := {},\n    fns := [\n{}] }}", list(&adts), bodies.join(",\n")))
}

/// The Lean module of `func`'s embedded program `term`.
pub(crate) fn module(db: &Db, func: FunctionId, term: String) -> Embedded {
    let path = function_path(db, func);
    let function = path.join("::");
    let name = format!("{}.mir", path.join("."));
    let module = format!("BlazeMir.{}", path.join("."));
    let text = format!(
        "-- Generated by blaze ({}) from `{function}`: its MIR, embedded.\n\
         -- Do not edit: the golden tests of crates/hir-ty-lean hold this file to what the encoder writes.\n\
         import Blaze.Mir.Syntax\n\nopen Blaze Blaze.Mir\n\ndef {} : Program :=\n{term}\n",
        crate::TRANSLATOR,
        crate::lean::ident(&name),
    );
    Embedded { function, name, module, term, text }
}
