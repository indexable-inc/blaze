//! A body printed the way rustc's `-Zunpretty=mir` prints one, so that the two can be put
//! side by side: `fn name(_1: T) -> R { let mut _2: U; bb0: { ... } }`. Types are spelled as
//! rustc's `type_name` spells them where [`rustc_type_name`] knows how, and with `?` where
//! it does not. Nothing reads this but people and `harness/mirparity.py`.

use std::fmt::Write;

use base_db::Db;
use blaze_mem::{Tag, frame};
use hir_def::{Lookup, VariantId, expr_store::ExpressionStore, signatures::FunctionSignature};
use rustc_type_ir::inherent::IntoKind;

use crate::{
    CallableDefId, MemoryMap,
    db::{HirDatabase, InternedClosureId},
    next_solver::{Ty, TyKind},
};

use super::{
    AggregateKind, AsmOperand, AssertKind, BasicBlockId, BinOp, BorrowKind, CastKind, LocalId,
    MirBody, MutBorrowKind, Operand, OperandKind, PlaceTy, ProjectionElem, Rvalue, StatementKind,
    StoredPlace, TerminatorKind, UnOp, transform::Ctx, type_name::rustc_type_name,
};

/// `body` as rustc prints the MIR of the function `name`.
pub fn print_mir_body<'db>(db: &'db Db, body: &MirBody<'db>, name: &str) -> String {
    frame::task(Tag::Mir, |frame| {
        let ctx = Ctx::new(db, body.owner, frame);
        // alloc: printing MIR for people, the edge
        let mut p = Printer { db, body, ctx: &ctx, out: String::new() };
        p.body(name);
        p.out
    })
}

struct Printer<'a, 'f, 'db> {
    db: &'db Db,
    body: &'a MirBody<'db>,
    ctx: &'a Ctx<'f, 'db>,
    out: String,
}

impl<'db> Printer<'_, '_, 'db> {
    fn ty(&self, ty: Ty<'db>) -> String {
        rustc_type_name(self.db, ty).unwrap_or_else(|| match ty.kind() {
            // alloc: printing MIR for people, the edge
            TyKind::Param(param) => format!("{param:?}"),
            // alloc: printing MIR for people, the edge
            _ => "?".to_owned(),
        })
    }

    fn local(&self, local: LocalId) -> String {
        // alloc: printing MIR for people, the edge
        format!("_{}", local.into_raw().into_u32())
    }

    fn block(&self, block: BasicBlockId) -> String {
        // alloc: printing MIR for people, the edge
        format!("bb{}", block.into_raw().into_u32())
    }

    fn body(&mut self, name: &str) {
        let body = self.body;
        let params: Vec<String> = body
            .param_locals
            .iter()
            // alloc: printing MIR for people, the edge
            .map(|&it| format!("{}: {}", self.local(it), self.ty(body.locals[it].ty.as_ref())))
            // alloc: printing MIR for people, the edge
            .collect();
        let ret = self.ty(body.locals[super::return_slot()].ty.as_ref());
        _ = writeln!(self.out, "fn {name}({}) -> {ret} {{", params.join(", "));
        let store = ExpressionStore::of(self.db, body.owner.expression_store_owner(self.db));
        for (binding, &local) in body.binding_locals.iter() {
            _ = writeln!(
                self.out,
                "    debug {} => {};",
                store[binding].name.as_str(),
                self.local(local)
            );
        }
        for (local, decl) in body.locals.iter() {
            if body.param_locals.contains(&local) {
                continue;
            }
            _ = writeln!(
                self.out,
                "    let mut {}: {};",
                self.local(local),
                self.ty(decl.ty.as_ref())
            );
        }
        for (id, block) in body.basic_blocks.iter() {
            let cleanup = if block.is_cleanup { " (cleanup)" } else { "" };
            _ = writeln!(self.out, "\n    {}{cleanup}: {{", self.block(id));
            for statement in &block.statements {
                let text = self.statement(&statement.kind);
                _ = writeln!(self.out, "        {text}");
            }
            if let Some(terminator) = &block.terminator {
                let text = self.terminator(&terminator.kind, block.is_cleanup);
                _ = writeln!(self.out, "        {text}");
            }
            self.out.push_str("    }\n");
        }
        self.out.push_str("}\n");
    }

    fn place(&self, place: &StoredPlace) -> String {
        let place = place.as_ref();
        let mut text = self.local(place.local);
        let mut ty = PlaceTy::from_ty(self.body.locals[place.local].ty.as_ref());
        for elem in place.projection.as_slice() {
            let next = ty.projection_ty(&self.ctx.infcx, elem, self.ctx.env);
            text = match elem {
                // alloc: printing MIR for people, the edge
                ProjectionElem::Deref => format!("(*{text})"),
                ProjectionElem::Field(field) => {
                    // alloc: printing MIR for people, the edge
                    format!("({text}.{}: {})", field.0, self.ty(next.ty))
                }
                // alloc: printing MIR for people, the edge
                ProjectionElem::Index(local) => format!("{text}[{}]", self.local(*local)),
                ProjectionElem::ConstantIndex { offset, from_end: false, .. } => {
                    // alloc: printing MIR for people, the edge
                    format!("{text}[{offset} of ?]")
                }
                ProjectionElem::ConstantIndex { offset, from_end: true, .. } => {
                    // alloc: printing MIR for people, the edge
                    format!("{text}[-{offset} of ?]")
                }
                // alloc: printing MIR for people, the edge
                ProjectionElem::Subslice { from, to } => format!("{text}[{from}:-{to}]"),
                ProjectionElem::Downcast(variant) => {
                    // alloc: printing MIR for people, the edge
                    format!("({text} as {})", self.variant_name(*variant))
                }
            };
            ty = next;
        }
        text
    }

    fn variant_name(&self, variant: VariantId) -> String {
        match variant {
            VariantId::EnumVariantId(it) => {
                let loc = it.lookup(self.db);
                loc.parent
                    .enum_variants(self.db)
                    .variant_name_by_id(it)
                    // alloc: printing MIR for people, the edge
                    .map_or_else(|| "?".to_owned(), |it| it.as_str().to_owned())
            }
            VariantId::StructId(it) => {
                // alloc: printing MIR for people, the edge
                hir_def::signatures::StructSignature::of(self.db, it).name.as_str().to_owned()
            }
            VariantId::UnionId(it) => {
                // alloc: printing MIR for people, the edge
                hir_def::signatures::UnionSignature::of(self.db, it).name.as_str().to_owned()
            }
        }
    }

    fn operand(&self, operand: &Operand) -> String {
        match &operand.kind {
            // alloc: printing MIR for people, the edge
            OperandKind::Copy(place) => format!("copy {}", self.place(place)),
            // alloc: printing MIR for people, the edge
            OperandKind::Move(place) => format!("move {}", self.place(place)),
            OperandKind::Constant { konst, ty } => {
                let ty = ty.as_ref();
                let leaf = match konst.as_ref().kind() {
                    crate::next_solver::ConstKind::Value(value) => value.try_to_leaf(),
                    _ => None,
                };
                match (leaf, scalar_width(ty)) {
                    (Some(leaf), Some(width)) => {
                        let bits = leaf.to_bits_unchecked().to_le_bytes();
                        self.constant(&bits[..width], &MemoryMap::default(), ty)
                    }
                    // alloc: printing MIR for people, the edge
                    _ => format!("const _: {}", self.ty(ty)),
                }
            }
            OperandKind::Allocation { allocation } => {
                let ty = allocation.as_ref().ty;
                self.constant(&allocation.as_ref().memory, &allocation.as_ref().memory_map, ty)
            }
            // alloc: printing MIR for people, the edge
            OperandKind::Static(_) => "const {static}".to_owned(),
        }
    }

    /// A constant as rustc prints the scalars: `const 3_u32`, `const true`, `const ()`; the
    /// name of the function a function item stands for; and `const _` for the rest.
    fn constant(&self, bytes: &[u8], _memory: &MemoryMap<'db>, ty: Ty<'db>) -> String {
        let int = |signed: bool| {
            let mut padded = [0u8; 16];
            let len = bytes.len().min(16);
            padded[..len].copy_from_slice(&bytes[..len]);
            if signed && len > 0 && bytes[len - 1] & 0x80 != 0 {
                padded[len..].fill(0xff);
            }
            (u128::from_le_bytes(padded), i128::from_le_bytes(padded))
        };
        match ty.kind() {
            // alloc: printing MIR for people, the edge
            TyKind::Bool if bytes.len() == 1 => format!("const {}", bytes[0] != 0),
            TyKind::Uint(uint) => {
                // alloc: printing MIR for people, the edge
                format!("const {}_{}", int(false).0, uint.name_str())
            }
            // alloc: printing MIR for people, the edge
            TyKind::Int(it) => format!("const {}_{}", int(true).1, it.name_str()),
            TyKind::Char => match char::from_u32(int(false).0 as u32) {
                // alloc: printing MIR for people, the edge
                Some(c) => format!("const {c:?}"),
                // alloc: printing MIR for people, the edge
                None => "const _: char".to_owned(),
            },
            // alloc: printing MIR for people, the edge
            TyKind::Tuple(tys) if tys.is_empty() => "const ()".to_owned(),
            TyKind::FnDef(def, _) => match def.0 {
                CallableDefId::FunctionId(func) => self.function_name(func),
                CallableDefId::StructId(it) => self.variant_name(VariantId::StructId(it)),
                CallableDefId::EnumVariantId(it) => self.variant_name(VariantId::EnumVariantId(it)),
            },
            // alloc: printing MIR for people, the edge
            _ => format!("const _: {}", self.ty(ty)),
        }
    }

    fn function_name(&self, func: hir_def::FunctionId) -> String {
        // alloc: printing MIR for people, the edge
        FunctionSignature::of(self.db, func).name.as_str().to_owned()
    }

    fn callee(&self, func: &Operand) -> String {
        match &func.kind {
            OperandKind::Allocation { allocation } => {
                let allocation = allocation.as_ref();
                match allocation.ty.kind() {
                    TyKind::FnDef(..) => {
                        self.constant(&allocation.memory, &allocation.memory_map, allocation.ty)
                    }
                    _ => self.operand(func),
                }
            }
            _ => self.operand(func),
        }
    }

    fn operands(&self, operands: &[Operand]) -> String {
        // alloc: printing MIR for people, the edge
        operands.iter().map(|it| self.operand(it)).collect::<Vec<_>>().join(", ")
    }

    fn rvalue(&self, rvalue: &Rvalue) -> String {
        match rvalue {
            Rvalue::Use(it) => self.operand(it),
            Rvalue::Repeat(it, count) => {
                let count = crate::consteval::try_const_usize(self.db, count.as_ref())
                    // alloc: printing MIR for people, the edge
                    .map_or_else(|| "?".to_owned(), |it| it.to_string());
                // alloc: printing MIR for people, the edge
                format!("[{}; {count}]", self.operand(it))
            }
            Rvalue::Ref(kind, place) => {
                let kind = match kind {
                    BorrowKind::Shared => "&",
                    BorrowKind::Shallow => "&fake shallow ",
                    BorrowKind::Mut { kind: MutBorrowKind::ClosureCapture } => "&uniq ",
                    BorrowKind::Mut { .. } => "&mut ",
                };
                // alloc: printing MIR for people, the edge
                format!("{kind}{}", self.place(place))
            }
            // alloc: printing MIR for people, the edge
            Rvalue::Len(place) => format!("Len({})", self.place(place)),
            Rvalue::Cast(kind, operand, ty) => {
                let kind = match kind {
                    // alloc: printing MIR for people, the edge
                    CastKind::PointerExposeAddress => "PointerExposeProvenance".to_owned(),
                    CastKind::PointerFromExposedAddress => {
                        // alloc: printing MIR for people, the edge
                        "PointerWithExposedProvenance".to_owned()
                    }
                    // alloc: printing MIR for people, the edge
                    CastKind::PtrToPtr => "PtrToPtr".to_owned(),
                    // alloc: printing MIR for people, the edge
                    CastKind::PointerCoercion(it) => format!("PointerCoercion({it:?})"),
                    // alloc: printing MIR for people, the edge
                    CastKind::DynStar => "DynStar".to_owned(),
                    // alloc: printing MIR for people, the edge
                    CastKind::IntToInt => "IntToInt".to_owned(),
                    // alloc: printing MIR for people, the edge
                    CastKind::FloatToInt => "FloatToInt".to_owned(),
                    // alloc: printing MIR for people, the edge
                    CastKind::FloatToFloat => "FloatToFloat".to_owned(),
                    // alloc: printing MIR for people, the edge
                    CastKind::IntToFloat => "IntToFloat".to_owned(),
                    // alloc: printing MIR for people, the edge
                    CastKind::FnPtrToPtr => "FnPtrToPtr".to_owned(),
                    // alloc: printing MIR for people, the edge
                    CastKind::Transmute => "Transmute".to_owned(),
                };
                // alloc: printing MIR for people, the edge
                format!("{} as {} ({kind})", self.operand(operand), self.ty(ty.as_ref()))
            }
            Rvalue::BinaryOp(op, a, b) => {
                // alloc: printing MIR for people, the edge
                format!("{}({}, {})", bin_op_name(op), self.operand(a), self.operand(b))
            }
            Rvalue::CheckedBinaryOp(op, a, b) => {
                // alloc: printing MIR for people, the edge
                format!("Checked{}({}, {})", bin_op_name(op), self.operand(a), self.operand(b))
            }
            Rvalue::UnaryOp(op, a) => {
                let op = match op {
                    UnOp::Not => "Not",
                    UnOp::Neg => "Neg",
                };
                // alloc: printing MIR for people, the edge
                format!("{op}({})", self.operand(a))
            }
            // alloc: printing MIR for people, the edge
            Rvalue::Discriminant(place) => format!("discriminant({})", self.place(place)),
            Rvalue::Aggregate(kind, operands) => self.aggregate(kind, operands),
            // rustc's `EraseDerefTemps` makes these plain copies.
            // alloc: printing MIR for people, the edge
            Rvalue::CopyForDeref(place) => format!("copy {}", self.place(place)),
            Rvalue::ThreadLocalRef(n) | Rvalue::AddressOf(n) | Rvalue::NullaryOp(n) => match *n {},
        }
    }

    fn aggregate(&self, kind: &AggregateKind, operands: &[Operand]) -> String {
        match kind {
            // alloc: printing MIR for people, the edge
            AggregateKind::Array(_) => format!("[{}]", self.operands(operands)),
            AggregateKind::Tuple(_) if operands.len() == 1 => {
                // alloc: printing MIR for people, the edge
                format!("({},)", self.operand(&operands[0]))
            }
            // alloc: printing MIR for people, the edge
            AggregateKind::Tuple(_) => format!("({})", self.operands(operands)),
            AggregateKind::Adt(variant, _) => {
                let name = match variant {
                    VariantId::EnumVariantId(it) => {
                        let parent = it.lookup(self.db).parent;
                        let enum_name = hir_def::signatures::EnumSignature::of(self.db, parent)
                            .name
                            .as_str()
                            // alloc: printing MIR for people, the edge
                            .to_owned();
                        // alloc: printing MIR for people, the edge
                        format!("{enum_name}::{}", self.variant_name(*variant))
                    }
                    _ => self.variant_name(*variant),
                };
                let fields = variant.fields(self.db);
                let named = matches!(fields.shape, hir_def::item_tree::FieldsShape::Record);
                if operands.is_empty() {
                    name
                } else if named {
                    let parts: Vec<String> = fields
                        .fields()
                        .iter()
                        .zip(operands)
                        .map(|((_, field), op)| {
                            // alloc: printing MIR for people, the edge
                            format!("{}: {}", field.name.as_str(), self.operand(op))
                        })
                        // alloc: printing MIR for people, the edge
                        .collect();
                    // alloc: printing MIR for people, the edge
                    format!("{name} {{ {} }}", parts.join(", "))
                } else {
                    // alloc: printing MIR for people, the edge
                    format!("{name}({})", self.operands(operands))
                }
            }
            AggregateKind::Union(union, field) => {
                let name = self.variant_name(VariantId::UnionId(*union));
                let fields = VariantId::UnionId(*union).fields(self.db);
                // alloc: printing MIR for people, the edge
                let field_name = fields.fields()[field.local_id].name.as_str().to_owned();
                // alloc: printing MIR for people, the edge
                let value = operands.first().map_or_else(String::new, |it| self.operand(it));
                // alloc: printing MIR for people, the edge
                format!("{name} {{ {field_name}: {value} }}")
            }
            // alloc: printing MIR for people, the edge
            AggregateKind::Closure(_) => format!("{{closure}} {{ {} }}", self.operands(operands)),
        }
    }

    fn statement(&self, statement: &StatementKind) -> String {
        match statement {
            StatementKind::Assign(place, rvalue) => {
                // alloc: printing MIR for people, the edge
                format!("{} = {};", self.place(place), self.rvalue(rvalue))
            }
            StatementKind::FakeRead(place) => {
                // alloc: printing MIR for people, the edge
                format!("FakeRead(ForLet(None), {});", self.place(place))
            }
            // alloc: printing MIR for people, the edge
            StatementKind::Deinit(place) => format!("Deinit({});", self.place(place)),
            // alloc: printing MIR for people, the edge
            StatementKind::StorageLive(local) => format!("StorageLive({});", self.local(*local)),
            // alloc: printing MIR for people, the edge
            StatementKind::StorageDead(local) => format!("StorageDead({});", self.local(*local)),
            StatementKind::InlineAsm(asm) => {
                let operands: Vec<String> = asm
                    .operands
                    .iter()
                    .map(|it| match it {
                        // alloc: printing MIR for people, the edge
                        AsmOperand::Reg { input, output, .. } => format!(
                            "reg({}, {})",
                            // alloc: printing MIR for people, the edge
                            input.as_ref().map_or_else(|| "_".to_owned(), |it| self.operand(it)),
                            // alloc: printing MIR for people, the edge
                            output.as_ref().map_or_else(|| "_".to_owned(), |it| self.place(it))
                        ),
                        // alloc: printing MIR for people, the edge
                        AsmOperand::Const(it) => format!("const {}", self.operand(it)),
                        // alloc: printing MIR for people, the edge
                        AsmOperand::Sym(it) => format!("sym {}", self.operand(it)),
                        // alloc: printing MIR for people, the edge
                        AsmOperand::Label => "label".to_owned(),
                    })
                    // alloc: printing MIR for people, the edge
                    .collect();
                // alloc: printing MIR for people, the edge
                format!("asm!({});", operands.join(", "))
            }
            // alloc: printing MIR for people, the edge
            StatementKind::Nop => "nop;".to_owned(),
        }
    }

    fn unwind(&self, unwind: Option<BasicBlockId>, in_cleanup: bool) -> String {
        match unwind {
            // alloc: printing MIR for people, the edge
            Some(block) => format!("unwind: {}", self.block(block)),
            // alloc: printing MIR for people, the edge
            None if in_cleanup => "unwind terminate(cleanup)".to_owned(),
            // alloc: printing MIR for people, the edge
            None => "unwind continue".to_owned(),
        }
    }

    fn terminator(&self, terminator: &TerminatorKind, in_cleanup: bool) -> String {
        match terminator {
            // alloc: printing MIR for people, the edge
            TerminatorKind::Goto { target } => format!("goto -> {};", self.block(*target)),
            TerminatorKind::SwitchInt { discr, targets } => {
                let mut arms: Vec<String> = targets
                    .iter()
                    // alloc: printing MIR for people, the edge
                    .map(|(value, target)| format!("{value}: {}", self.block(target)))
                    // alloc: printing MIR for people, the edge
                    .collect();
                // alloc: printing MIR for people, the edge
                arms.push(format!("otherwise: {}", self.block(targets.otherwise())));
                // alloc: printing MIR for people, the edge
                format!("switchInt({}) -> [{}];", self.operand(discr), arms.join(", "))
            }
            // alloc: printing MIR for people, the edge
            TerminatorKind::UnwindResume => "resume;".to_owned(),
            // alloc: printing MIR for people, the edge
            TerminatorKind::Abort => "abort;".to_owned(),
            // alloc: printing MIR for people, the edge
            TerminatorKind::Return => "return;".to_owned(),
            // alloc: printing MIR for people, the edge
            TerminatorKind::Unreachable => "unreachable;".to_owned(),
            // alloc: printing MIR for people, the edge
            TerminatorKind::Drop { place, target, unwind } => format!(
                "drop({}) -> [return: {}, {}];",
                self.place(place),
                self.block(*target),
                self.unwind(*unwind, in_cleanup)
            ),
            // alloc: printing MIR for people, the edge
            TerminatorKind::DropAndReplace { place, value, target, unwind } => format!(
                "replace({} <- {}) -> [return: {}, {}];",
                self.place(place),
                self.operand(value),
                self.block(*target),
                self.unwind(*unwind, in_cleanup)
            ),
            TerminatorKind::Call { func, args, destination, target, cleanup, .. } => {
                // alloc: printing MIR for people, the edge
                let call = format!(
                    "{} = {}({})",
                    self.place(destination),
                    self.callee(func),
                    self.operands(args)
                );
                match target {
                    // alloc: printing MIR for people, the edge
                    Some(target) => format!(
                        "{call} -> [return: {}, {}];",
                        self.block(*target),
                        self.unwind(*cleanup, in_cleanup)
                    ),
                    // alloc: printing MIR for people, the edge
                    None => format!("{call} -> {};", self.unwind(*cleanup, in_cleanup)),
                }
            }
            TerminatorKind::TailCall { func, args, .. } => {
                // alloc: printing MIR for people, the edge
                format!("tailcall {}({});", self.callee(func), self.operands(args))
            }
            TerminatorKind::Assert { cond, expected, msg, target, cleanup } => {
                let cond = match expected {
                    true => self.operand(cond),
                    // alloc: printing MIR for people, the edge
                    false => format!("!{}", self.operand(cond)),
                };
                // alloc: printing MIR for people, the edge
                format!(
                    "assert({cond}, {}) -> [success: {}, {}];",
                    self.assert_message(msg),
                    self.block(*target),
                    self.unwind(*cleanup, in_cleanup)
                )
            }
            // alloc: printing MIR for people, the edge
            TerminatorKind::Yield { value, resume, resume_arg, drop } => format!(
                "{} = yield({}) -> [resume: {}, drop: {}];",
                self.place(resume_arg),
                self.operand(value),
                self.block(*resume),
                // alloc: printing MIR for people, the edge
                drop.map_or_else(|| "?".to_owned(), |it| self.block(it))
            ),
            // alloc: printing MIR for people, the edge
            TerminatorKind::CoroutineDrop => "coroutine_drop;".to_owned(),
            // alloc: printing MIR for people, the edge
            TerminatorKind::FalseEdge { real_target, imaginary_target } => format!(
                "falseEdge -> [real: {}, imaginary: {}];",
                self.block(*real_target),
                self.block(*imaginary_target)
            ),
            // alloc: printing MIR for people, the edge
            TerminatorKind::FalseUnwind { real_target, unwind } => format!(
                "falseUnwind -> [real: {}, {}];",
                self.block(*real_target),
                self.unwind(*unwind, in_cleanup)
            ),
        }
    }

    /// rustc's `AssertKind::fmt_assert_args`.
    fn assert_message(&self, msg: &AssertKind) -> String {
        match msg {
            // alloc: printing MIR for people, the edge
            AssertKind::BoundsCheck { len, index } => format!(
                "\"index out of bounds: the length is {{}} but the index is {{}}\", {}, {}",
                self.operand(len),
                self.operand(index)
            ),
            // alloc: printing MIR for people, the edge
            AssertKind::Overflow(BinOp::Shl, _, b) => format!(
                "\"attempt to shift left by `{{}}`, which would overflow\", {}",
                self.operand(b)
            ),
            // alloc: printing MIR for people, the edge
            AssertKind::Overflow(BinOp::Shr, _, b) => format!(
                "\"attempt to shift right by `{{}}`, which would overflow\", {}",
                self.operand(b)
            ),
            // alloc: printing MIR for people, the edge
            AssertKind::Overflow(BinOp::Rem, a, b) => format!(
                "\"attempt to compute the remainder of `{{}} % {{}}`, which would overflow\", {}, {}",
                self.operand(a),
                self.operand(b)
            ),
            // alloc: printing MIR for people, the edge
            AssertKind::Overflow(op, a, b) => format!(
                "\"attempt to compute `{{}} {op} {{}}`, which would overflow\", {}, {}",
                self.operand(a),
                self.operand(b)
            ),
            AssertKind::OverflowNeg(a) => {
                // alloc: printing MIR for people, the edge
                format!("\"attempt to negate `{{}}`, which would overflow\", {}", self.operand(a))
            }
            AssertKind::DivisionByZero(a) => {
                // alloc: printing MIR for people, the edge
                format!("\"attempt to divide `{{}}` by zero\", {}", self.operand(a))
            }
            // alloc: printing MIR for people, the edge
            AssertKind::RemainderByZero(a) => format!(
                "\"attempt to calculate the remainder of `{{}}` with a divisor of zero\", {}",
                self.operand(a)
            ),
            // alloc: printing MIR for people, the edge
            AssertKind::MisalignedPointerDereference { required, found } => format!(
                "\"misaligned pointer dereference: address must be a multiple of {{}} but is {{}}\", {}, {}",
                self.operand(required),
                self.operand(found)
            ),
            // alloc: printing MIR for people, the edge
            AssertKind::NullPointerDereference => "\"null pointer dereference occurred\"".to_owned(),
            // alloc: printing MIR for people, the edge
            AssertKind::NullReferenceConstructed => "\"null reference produced\"".to_owned(),
            // alloc: printing MIR for people, the edge
            AssertKind::InvalidEnumConstruction(source) => format!(
                "\"trying to construct an enum from an invalid value {{}}\", {}",
                self.operand(source)
            ),
        }
    }
}

fn bin_op_name(op: &BinOp) -> &'static str {
    match op {
        BinOp::Add => "Add",
        BinOp::Sub => "Sub",
        BinOp::Mul => "Mul",
        BinOp::Div => "Div",
        BinOp::Rem => "Rem",
        BinOp::BitXor => "BitXor",
        BinOp::BitAnd => "BitAnd",
        BinOp::BitOr => "BitOr",
        BinOp::Shl => "Shl",
        BinOp::Shr => "Shr",
        BinOp::Eq => "Eq",
        BinOp::Lt => "Lt",
        BinOp::Le => "Le",
        BinOp::Ne => "Ne",
        BinOp::Ge => "Ge",
        BinOp::Gt => "Gt",
        BinOp::Offset => "Offset",
        BinOp::AddWithOverflow => "AddWithOverflow",
        BinOp::SubWithOverflow => "SubWithOverflow",
        BinOp::MulWithOverflow => "MulWithOverflow",
    }
}

/// What rustc's `-Zunpretty=mir` writes after the path of the function a closure is written
/// in: `::{closure#0}`, the closures it is in first.
pub fn closure_path_suffix(db: &Db, closure: InternedClosureId<'_>) -> Option<String> {
    let loc = closure.loc(db);
    let crate::InferBodyId::DefWithBodyId(owner) = loc.owner else { return None };
    super::type_name::rustc_closure_suffix(db, owner, loc.expr)
}

/// The size of a value of a scalar type rustc prints constants of by value.
fn scalar_width(ty: Ty<'_>) -> Option<usize> {
    use rustc_type_ir::{IntTy, UintTy};
    Some(match ty.kind() {
        TyKind::Bool => 1,
        TyKind::Char => 4,
        TyKind::Int(IntTy::I8) | TyKind::Uint(UintTy::U8) => 1,
        TyKind::Int(IntTy::I16) | TyKind::Uint(UintTy::U16) => 2,
        TyKind::Int(IntTy::I32) | TyKind::Uint(UintTy::U32) => 4,
        TyKind::Int(IntTy::I64 | IntTy::Isize) | TyKind::Uint(UintTy::U64 | UintTy::Usize) => 8,
        TyKind::Int(IntTy::I128) | TyKind::Uint(UintTy::U128) => 16,
        _ => return None,
    })
}

/// Where rustc says an `impl` is written, which it names one by in its MIR: at the derive that
/// wrote it, or where the `impl` starts. The file and the offset in it.
pub fn impl_written_at(
    db: &Db,
    id: hir_def::ImplId,
) -> (base_db::FileId, syntax::TextSize) {
    use hir_def::src::HasSource;
    use hir_expand::{InFile, MacroCallKind};
    let source: InFile<_> = id.lookup(db).ast_ptr(db);
    if let Some(call) = source.file_id.macro_file() {
        let loc = call.loc(db);
        if let MacroCallKind::Derive { .. } = loc.kind {
            let range = loc.kind.original_call_range(db, loc.krate);
            return (range.file_id.file_id(db), range.range.start());
        }
    }
    let range = InFile::new(source.file_id, source.value.syntax_node_ptr().text_range())
        .original_node_file_range_rooted(db);
    (range.file_id.file_id(db), range.range.start())
}
