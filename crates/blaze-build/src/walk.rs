//! The walk: one work stack of tasks (an expression to evaluate, or the continuation of one
//! whose operands are done) and one stack of the values evaluated, so a body of any depth is
//! built in constant native stack.

use core::alloc::Allocator;

use blaze_lir::builder::{MemAttrs, Param, Variadic};
use blaze_lir::func::{BlockKind, Conv, Ext, RefKind, SlotId, SlotKind};
use blaze_lir::{Block, Builder, Flags, Full, IntCond, Op, Ty, Value};
use blaze_typeck::{
    AdjustKind, AdtKind, BinOp, BodyView, Checked, Expr, ExprId, FnSig, IntTy, ItemKey, Lit, Name, PNode, PTy,
    PTys, Pat, PatId, Stmt, StmtId, UintTy, UnOp,
};

use crate::{Adts, BuildError, Built};

/// The target's pointer width: the ladder runs on AArch64.
const PTR_BITS: u32 = 64;

/// How a Rust type is held.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Repr {
    /// `()`, `!`, a fn item: no value.
    None,
    Int { ty: Ty, signed: bool },
    /// An `i8` of 0 or 1.
    Bool,
    Ptr,
    /// A struct, by its row of `layouts`.
    Struct(u32),
}

impl Repr {
    /// The LIR type of a scalar.
    fn scalar(self) -> Option<Ty> {
        match self {
            Repr::Int { ty, .. } => Some(ty),
            Repr::Bool => Some(Ty::I8),
            Repr::Ptr => Some(Ty::PTR),
            Repr::None | Repr::Struct(_) => None,
        }
    }
}

/// An evaluated expression.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum V {
    /// No value: a type without one, or an expression that diverged.
    None,
    Scalar(Value),
    /// A place: a pointer and a byte offset from it.
    Place(Value, i32),
}

/// A type outside the subset, or a type the record should not hold, before it is placed.
enum TyErr {
    Unsupported(&'static str),
    Malformed(&'static str),
}

/// A struct's layout: its fields in `layout_fields[first..first + len]`.
struct Layout {
    key: ItemKey,
    size: u32,
    /// log2 of the alignment.
    align: u8,
    first: u32,
    len: u32,
}

/// What `break` and `continue` reach: a loop, or a block a `break` leaves.
#[derive(Clone, Copy)]
struct Target {
    expr: ExprId,
    /// The loop's head; `None` for a block.
    header: Option<Block>,
    exit: Block,
    repr: Repr,
}

/// A match being built: its arms' blocks are `arm_blocks[first..first + arms + 1]`, the last
/// the block no arm matched (unreachable: the checker proved the arms exhaustive).
#[derive(Clone, Copy)]
struct MatchState {
    expr: ExprId,
    scrutinee: Value,
    width: u32,
    join: Block,
    repr: Repr,
    first: u32,
    arms: u32,
    /// Some arm has a guard: the arms are tested in turn rather than by one `switch`.
    guarded: bool,
}

#[derive(Clone, Copy)]
enum Task {
    /// Evaluate, then apply the expression's adjustments.
    Eval(ExprId),
    /// Evaluate without the adjustments.
    Raw(ExprId),
    Adjust(ExprId),
    /// Evaluate as a place.
    Place(ExprId),
    Stmt(StmtId),
    Discard,
    PushNone,
    Let(PatId),
    BlockExit(ExprId),
    If { expr: ExprId, then: ExprId, else_: Option<ExprId> },
    IfThen { expr: ExprId, else_: Option<ExprId>, else_block: Block, join: Block },
    IfElse { expr: ExprId, join: Block },
    LoopEnd(ExprId),
    Break(ExprId, bool),
    Return(bool),
    Match(ExprId),
    Arm { m: u32, i: u32 },
    Guard { m: u32, i: u32 },
    ArmEnd { m: u32 },
    MatchEnd,
    Call(ExprId),
    Unary(ExprId, UnOp),
    Deref(ExprId),
    Binary(ExprId, BinOp),
    Short { expr: ExprId, op: BinOp, rhs: ExprId },
    ShortEnd(Block),
    Assign(ExprId),
    AssignOp(ExprId, BinOp),
    Cast(ExprId),
    /// Store the value on top into the struct place under it, at `layout_fields[at]`.
    StructInit(u32),
    Field(ExprId),
    FieldPlace(ExprId),
    DerefPlace,
    Spill(ExprId),
    Ref,
}

struct Walk<'x, 'c, A: Allocator + Clone> {
    body: &'x BodyView<'x>,
    checked: &'x Checked<'c>,
    types: PTys<'x>,
    adts: &'x dyn Adts,
    alloc: A,
    b: Builder<A, Full>,
    refs: Vec<ItemKey, A>,
    layouts: Vec<Layout, A>,
    layout_fields: Vec<(Name, u32, Repr), A>,
    /// Per pattern: the binding's slot and how its value is held.
    slots: Vec<Option<(SlotId, Repr)>, A>,
    /// Per block made: whether a reachable edge leads to it.
    reached: Vec<bool, A>,
    /// Whether the code being built is reachable.
    live: bool,
    /// Per expression: whether it is a block some `break` leaves.
    break_target: Vec<bool, A>,
    work: Vec<Task, A>,
    vals: Vec<V, A>,
    targets: Vec<Target, A>,
    matches: Vec<MatchState, A>,
    arm_blocks: Vec<Block, A>,
    ret: Repr,
}

fn unsupported(expr: ExprId, what: &'static str) -> BuildError {
    BuildError::Unsupported { expr, what }
}

fn malformed(what: &'static str) -> BuildError {
    BuildError::Malformed { what }
}

fn int_ty(i: IntTy) -> Result<Ty, TyErr> {
    Ok(match i {
        IntTy::I8 => Ty::I8,
        IntTy::I16 => Ty::I16,
        IntTy::I32 => Ty::I32,
        IntTy::I64 | IntTy::Isize => Ty::I64,
        IntTy::I128 => return Err(TyErr::Unsupported("i128")),
    })
}

fn uint_ty(u: UintTy) -> Result<Ty, TyErr> {
    Ok(match u {
        UintTy::U8 => Ty::I8,
        UintTy::U16 => Ty::I16,
        UintTy::U32 => Ty::I32,
        UintTy::U64 | UintTy::Usize => Ty::I64,
        UintTy::U128 => return Err(TyErr::Unsupported("u128")),
    })
}

fn align_up(at: u32, align: u8) -> Option<u32> {
    let a = 1u32 << align;
    at.checked_add(a - 1).map(|it| it & !(a - 1))
}

fn mask(value: u128, bits: u32) -> u128 {
    if bits >= 128 { value } else { value & ((1u128 << bits) - 1) }
}

/// A signature's parameter or return of a scalar: a `bool` and the integers under 32 bits
/// widened by their signedness.
fn abi_param(repr: Repr, ty: Ty) -> Param {
    let ext = match repr {
        Repr::Bool => Ext::Zext,
        Repr::Int { ty, signed } if ty.bits(PTR_BITS) < 32 => {
            if signed {
                Ext::Sext
            } else {
                Ext::Zext
            }
        }
        Repr::Int { .. } | Repr::Ptr | Repr::None | Repr::Struct(_) => Ext::None,
    };
    Param { ext, ..Param::of(ty) }
}

pub(crate) fn build<A: Allocator + Clone>(
    body: &BodyView<'_>,
    checked: &Checked<'_>,
    sig: &FnSig<'_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    let mut break_target = Vec::with_capacity_in(body.exprs.len(), alloc.clone());
    break_target.resize(body.exprs.len(), false);
    for e in body.exprs {
        if let Expr::Break { target, .. } = *e
            && let Some(Expr::Block { .. }) = body.exprs.get(target.ix())
        {
            break_target[target.ix()] = true;
        }
    }
    let mut slots = Vec::with_capacity_in(body.pats.len(), alloc.clone());
    slots.resize(body.pats.len(), None);
    let mut w = Walk {
        body,
        checked,
        types: checked.types.view(),
        adts,
        alloc: alloc.clone(),
        b: Builder::new_in(PTR_BITS, alloc.clone()),
        refs: Vec::new_in(alloc.clone()),
        layouts: Vec::new_in(alloc.clone()),
        layout_fields: Vec::new_in(alloc.clone()),
        slots,
        reached: Vec::new_in(alloc.clone()),
        live: true,
        break_target,
        work: Vec::new_in(alloc.clone()),
        vals: Vec::new_in(alloc.clone()),
        targets: Vec::new_in(alloc.clone()),
        matches: Vec::new_in(alloc.clone()),
        arm_blocks: Vec::new_in(alloc.clone()),
        ret: Repr::None,
    };
    w.run(sig)?;
    let Walk { b, refs, .. } = w;
    let func = b.finish().map_err(|error| match error {
        blaze_lir::BuildError::Verify(v) => BuildError::Verify(v),
        other => BuildError::Lir(other),
    })?;
    blaze_lir::verify(func.view()).map_err(BuildError::Verify)?;
    Ok(Built { func, refs })
}

impl<'x, 'c, A: Allocator + Clone> Walk<'x, 'c, A> {
    // Types.

    fn find_layout(&self, key: ItemKey) -> Option<u32> {
        self.layouts.iter().position(|l| l.key == key).map(|it| it as u32)
    }

    /// How a type of table `types` is held.
    fn repr(&mut self, types: PTys<'_>, t: PTy) -> Result<Repr, TyErr> {
        let node = types.node(t).map_err(|_| TyErr::Malformed("a type past its table"))?;
        Ok(match node {
            PNode::Bool => Repr::Bool,
            PNode::Int(i) => Repr::Int { ty: int_ty(i)?, signed: true },
            PNode::Uint(u) => Repr::Int { ty: uint_ty(u)?, signed: false },
            PNode::Never | PNode::FnDef(..) => Repr::None,
            PNode::Tuple(l) if l.len == 0 => Repr::None,
            PNode::Tuple(_) => return Err(TyErr::Unsupported("a tuple")),
            PNode::Ref(_, inner) | PNode::Ptr(_, inner) => {
                match types.node(inner).map_err(|_| TyErr::Malformed("a type past its table"))? {
                    PNode::Str | PNode::Slice(_) => {
                        return Err(TyErr::Unsupported("a pointer to an unsized type (a wide pointer)"));
                    }
                    PNode::Param(_) | PNode::Alias { .. } | PNode::Opaque(..) => {
                        return Err(TyErr::Unsupported("a pointer to a generic type"));
                    }
                    PNode::Infer | PNode::Error => return Err(TyErr::Malformed("an unresolved type in the record")),
                    _ => Repr::Ptr,
                }
            }
            PNode::Adt(key, args) => {
                if args.len != 0 {
                    return Err(TyErr::Unsupported("a generic struct"));
                }
                Repr::Struct(self.layout(key)?)
            }
            PNode::Char => return Err(TyErr::Unsupported("char")),
            PNode::Float(_) => return Err(TyErr::Unsupported("a float")),
            PNode::Str => return Err(TyErr::Unsupported("str")),
            PNode::Array(..) => return Err(TyErr::Unsupported("an array by value")),
            PNode::Slice(_) => return Err(TyErr::Unsupported("a slice")),
            PNode::FnPtr(_) => return Err(TyErr::Unsupported("a fn pointer")),
            PNode::Closure(..) => return Err(TyErr::Unsupported("a closure")),
            PNode::Param(_) => return Err(TyErr::Unsupported("a generic parameter")),
            PNode::Alias { .. } => return Err(TyErr::Unsupported("an associated type")),
            PNode::Opaque(..) => return Err(TyErr::Unsupported("an opaque type")),
            PNode::Infer | PNode::Error => return Err(TyErr::Malformed("an unresolved type in the record")),
        })
    }

    /// Size and log2 alignment.
    fn size_align(&self, repr: Repr) -> (u32, u8) {
        match repr {
            Repr::None => (0, 0),
            Repr::Bool => (1, 0),
            Repr::Ptr => (PTR_BITS / 8, 3),
            Repr::Int { ty, .. } => {
                let bytes = ty.bits(PTR_BITS) / 8;
                (bytes, bytes.trailing_zeros() as u8)
            }
            Repr::Struct(l) => {
                let l = &self.layouts[l as usize];
                (l.size, l.align)
            }
        }
    }

    /// The layout of struct `key`, its nested structs first, by a work list.
    fn layout(&mut self, key: ItemKey) -> Result<u32, TyErr> {
        if let Some(found) = self.find_layout(key) {
            return Ok(found);
        }
        let mut todo = Vec::new_in(self.alloc.clone());
        todo.push(key);
        while let Some(&k) = todo.last() {
            if self.find_layout(k).is_some() {
                todo.pop();
                continue;
            }
            let adt = self.adts.adt(k).ok_or(TyErr::Malformed("a struct the build's `Adts` does not know"))?;
            match adt.kind {
                AdtKind::Struct => {}
                AdtKind::Enum => return Err(TyErr::Unsupported("an enum")),
                AdtKind::Union => return Err(TyErr::Unsupported("a union")),
            }
            if adt.generics != 0 {
                return Err(TyErr::Unsupported("a generic struct"));
            }
            let [variant] = adt.variants else {
                return Err(TyErr::Malformed("a struct without exactly one variant"));
            };
            let fields =
                adt.fields.get(variant.fields.range()).ok_or(TyErr::Malformed("a struct's fields past their column"))?;
            let mut missing = None;
            for f in fields {
                if let Ok(PNode::Adt(inner, _)) = adt.types.node(f.ty)
                    && self.find_layout(inner).is_none()
                {
                    missing = Some(inner);
                    break;
                }
            }
            if let Some(inner) = missing {
                if todo.contains(&inner) {
                    return Err(TyErr::Malformed("a struct that contains itself by value"));
                }
                todo.push(inner);
                continue;
            }
            let first = self.layout_fields.len() as u32;
            let (mut size, mut align) = (0u32, 0u8);
            for f in fields {
                let repr = self.repr(adt.types, f.ty)?;
                let (s, a) = self.size_align(repr);
                let at = align_up(size, a).ok_or(TyErr::Unsupported("a struct of 4 GiB or more"))?;
                self.layout_fields.push((f.name, at, repr));
                size = at.checked_add(s).ok_or(TyErr::Unsupported("a struct of 4 GiB or more"))?;
                align = align.max(a);
            }
            let size = align_up(size, align)
                .filter(|&s| s <= i32::MAX as u32)
                .ok_or(TyErr::Unsupported("a struct of 2 GiB or more"))?;
            self.layouts.push(Layout { key: k, size, align, first, len: fields.len() as u32 });
            todo.pop();
        }
        self.find_layout(key).ok_or(TyErr::Malformed("a struct whose layout was not made"))
    }

    fn ty_err(e: ExprId, error: TyErr) -> BuildError {
        match error {
            TyErr::Unsupported(what) => BuildError::UnsupportedType { expr: e, what },
            TyErr::Malformed(what) => malformed(what),
        }
    }

    /// How a type of the record is held, met at `e`.
    fn repr_at(&mut self, e: ExprId, t: PTy) -> Result<Repr, BuildError> {
        let types = self.types;
        self.repr(types, t).map_err(|error| Self::ty_err(e, error))
    }

    fn expr(&self, e: ExprId) -> Result<Expr, BuildError> {
        self.body.exprs.get(e.ix()).copied().ok_or(malformed("an expression past `exprs`"))
    }

    fn pat(&self, p: PatId) -> Result<Pat, BuildError> {
        self.body.pats.get(p.ix()).copied().ok_or(malformed("a pattern past `pats`"))
    }

    fn expr_ty(&self, e: ExprId) -> Result<PTy, BuildError> {
        self.checked.expr_ty.get(e.ix()).copied().ok_or(malformed("an expression without a type in the record"))
    }

    /// The rows of `e`'s adjustments: `start..end` of the record's `adjusts`.
    fn adjust_rows(&self, e: ExprId) -> core::ops::Range<usize> {
        let col = self.checked.adjusts.column::<0>();
        let start = col.partition_point(|&x| x < e);
        let len = col[start..].iter().take_while(|&&x| x == e).count();
        start..start + len
    }

    /// `e`'s type after its adjustments.
    fn adjusted_ty(&self, e: ExprId) -> Result<PTy, BuildError> {
        let rows = self.adjust_rows(e);
        match rows.end.checked_sub(1).filter(|&last| last >= rows.start) {
            Some(last) => Ok(self.checked.adjusts.column::<2>()[last]),
            None => self.expr_ty(e),
        }
    }

    fn expr_repr(&mut self, e: ExprId) -> Result<Repr, BuildError> {
        let t = self.expr_ty(e)?;
        self.repr_at(e, t)
    }

    fn adjusted_repr(&mut self, e: ExprId) -> Result<Repr, BuildError> {
        let t = self.adjusted_ty(e)?;
        self.repr_at(e, t)
    }

    // Blocks.

    fn create(&mut self) -> Block {
        let block = self.b.create_block(BlockKind::Normal);
        self.reached.push(false);
        block
    }

    fn begin(&mut self, block: Block) {
        self.b.begin(block);
        self.live = self.reached[block.index()];
    }

    fn reach(&mut self, block: Block) {
        if self.live {
            self.reached[block.index()] = true;
        }
    }

    fn jump(&mut self, to: Block, args: &[Value]) {
        self.reach(to);
        self.b.jump((to, args));
    }

    fn br(&mut self, cond: Value, then: (Block, &[Value]), otherwise: (Block, &[Value])) {
        self.reach(then.0);
        self.reach(otherwise.0);
        self.b.br(Flags::NONE, cond, then, otherwise);
    }

    /// After a terminator that leaves the expression: a block no edge reaches, for the code
    /// that follows it.
    fn dead_block(&mut self) {
        let block = self.create();
        self.begin(block);
    }

    // Values.

    fn pop(&mut self) -> Result<V, BuildError> {
        self.vals.pop().ok_or(malformed("the value stack ran out (a build bug)"))
    }

    /// The scalar of `v`, of type `ty`; in unreachable code a value that diverged is `uninit`.
    fn scalar(&mut self, v: V, ty: Ty) -> Result<Value, BuildError> {
        match v {
            V::Scalar(x) => Ok(x),
            V::None if !self.live => Ok(self.b.uninit(ty)),
            V::None => Err(malformed("an expression with no value where its type has one")),
            V::Place(..) => Err(malformed("a place where a scalar was expected")),
        }
    }

    /// The place of `v`, a struct; in unreachable code a value that diverged is a fresh slot.
    fn place(&mut self, v: V, repr: Repr) -> Result<(Value, i32), BuildError> {
        match v {
            V::Place(p, off) => Ok((p, off)),
            V::None if !self.live => {
                let (size, align) = self.size_align(repr);
                let slot = self.b.slot(size, align, SlotKind::Temp);
                Ok((self.b.slot_addr(slot), 0))
            }
            V::None => Err(malformed("an expression with no value where its type has one")),
            V::Scalar(_) => Err(malformed("a scalar where a place was expected")),
        }
    }

    fn address(&mut self, (p, off): (Value, i32)) -> Value {
        if off == 0 {
            return p;
        }
        let by = self.b.iconst(Ty::I64, off as i64 as u128);
        self.b.ptr_add(Flags::NONE, p, by)
    }

    /// What an edge passes for `v` of `repr`: nothing, the scalar, or the struct's address.
    fn arg(&mut self, v: V, repr: Repr) -> Result<Option<Value>, BuildError> {
        Ok(match repr {
            Repr::None => None,
            Repr::Struct(_) => {
                let at = self.place(v, repr)?;
                Some(self.address(at))
            }
            scalar => {
                let ty = scalar.scalar().ok_or(malformed("a scalar of no type"))?;
                Some(self.scalar(v, ty)?)
            }
        })
    }

    /// A join's parameter for a value of `repr`, as a value.
    fn join_param(&mut self, repr: Repr) -> V {
        match repr {
            Repr::None => V::None,
            Repr::Struct(_) => V::Place(self.b.param(Ty::PTR), 0),
            scalar => V::Scalar(self.b.param(scalar.scalar().unwrap_or(Ty::I8))),
        }
    }

    fn mem(&self, repr: Repr) -> MemAttrs {
        MemAttrs::aligned(self.size_align(repr).1)
    }

    fn load(&mut self, repr: Repr, (p, off): (Value, i32)) -> V {
        match repr {
            Repr::None => V::None,
            Repr::Struct(_) => V::Place(p, off),
            scalar => {
                let ty = scalar.scalar().unwrap_or(Ty::I8);
                let attrs = self.mem(scalar);
                V::Scalar(self.b.load(ty, Flags::NONE, p, off, attrs))
            }
        }
    }

    fn store(&mut self, v: V, repr: Repr, to: (Value, i32)) -> Result<(), BuildError> {
        match repr {
            Repr::None => Ok(()),
            Repr::Struct(l) => {
                let from = self.place(v, repr)?;
                self.copy(l, from, to);
                Ok(())
            }
            scalar => {
                let ty = scalar.scalar().ok_or(malformed("a scalar of no type"))?;
                let x = self.scalar(v, ty)?;
                let attrs = self.mem(scalar);
                self.b.store(Flags::NONE, x, to.0, to.1, attrs);
                Ok(())
            }
        }
    }

    /// Copies struct `layout` field by field, nested structs by a work list.
    fn copy(&mut self, layout: u32, from: (Value, i32), to: (Value, i32)) {
        let mut stack = Vec::new_in(self.alloc.clone());
        stack.push((layout, 0i32));
        while let Some((l, base)) = stack.pop() {
            let (first, len) = (self.layouts[l as usize].first, self.layouts[l as usize].len);
            for i in first..first + len {
                let (_, off, repr) = self.layout_fields[i as usize];
                let at = base + off as i32;
                match repr {
                    Repr::None => {}
                    Repr::Struct(inner) => stack.push((inner, at)),
                    scalar => {
                        let ty = scalar.scalar().unwrap_or(Ty::I8);
                        let attrs = self.mem(scalar);
                        let x = self.b.load(ty, Flags::NONE, from.0, from.1 + at, attrs);
                        self.b.store(Flags::NONE, x, to.0, to.1 + at, attrs);
                    }
                }
            }
        }
    }

    /// A fresh slot for `repr`.
    fn temp(&mut self, repr: Repr) -> (Value, i32) {
        let (size, align) = self.size_align(repr);
        let slot = self.b.slot(size, align, SlotKind::Temp);
        (self.b.slot_addr(slot), 0)
    }

    /// A `bool` (`i8`) as an `i1` to branch on.
    fn cond(&mut self, v: V) -> Result<Value, BuildError> {
        let x = self.scalar(v, Ty::I8)?;
        let zero = self.b.iconst(Ty::I8, 0);
        Ok(self.b.icmp(IntCond::Ne, x, zero))
    }

    // Bindings.

    /// The slot of binding pattern `p`, made on first use.
    fn bind(&mut self, p: PatId) -> Result<(SlotId, Repr), BuildError> {
        match self.pat(p)? {
            Pat::Bind { ann, sub: None, .. } if ann.by_ref.is_none() => {}
            Pat::Bind { .. } => {
                return Err(BuildError::UnsupportedPat { pat: p, what: "a `ref` binding or a binding with a subpattern" });
            }
            _ => return Err(BuildError::UnsupportedPat { pat: p, what: "a pattern other than a binding or `_`" }),
        }
        if let Some(found) = self.slots[p.ix()] {
            return Ok(found);
        }
        let t = *self.checked.local_ty.get(p.ix()).ok_or(malformed("a binding without a type in the record"))?;
        let types = self.types;
        let repr = self.repr(types, t).map_err(|error| match error {
            TyErr::Unsupported(what) => BuildError::UnsupportedPat { pat: p, what },
            TyErr::Malformed(what) => malformed(what),
        })?;
        let (size, align) = self.size_align(repr);
        let slot = self.b.slot(size, align, SlotKind::Named);
        self.slots[p.ix()] = Some((slot, repr));
        Ok((slot, repr))
    }

    /// Stores `v` into the binding `p` is, or drops it for `_`.
    fn bind_value(&mut self, p: PatId, v: V) -> Result<(), BuildError> {
        if let Pat::Wild = self.pat(p)? {
            return Ok(());
        }
        let (slot, repr) = self.bind(p)?;
        let at = self.b.slot_addr(slot);
        self.store(v, repr, (at, 0))
    }

    // The function.

    fn run(&mut self, sig: &FnSig<'_>) -> Result<(), BuildError> {
        if sig.parent_count != 0 || sig.own_count != 0 {
            return Err(BuildError::UnsupportedSig { what: "a generic fn" });
        }
        if sig.self_param {
            return Err(BuildError::UnsupportedSig { what: "a method" });
        }
        if sig.inputs.len() != self.body.params.len() {
            return Err(malformed("a signature and a body with other numbers of parameters"));
        }
        let sig_err = |error: TyErr| match error {
            TyErr::Unsupported(what) => BuildError::UnsupportedSig { what },
            TyErr::Malformed(what) => malformed(what),
        };
        let mut params = Vec::with_capacity_in(sig.inputs.len(), self.alloc.clone());
        let mut reprs = Vec::with_capacity_in(sig.inputs.len(), self.alloc.clone());
        for &t in sig.inputs {
            let repr = self.repr(sig.types, t).map_err(sig_err)?;
            match repr {
                Repr::Struct(_) => return Err(BuildError::UnsupportedSig { what: "a struct passed by value" }),
                Repr::None => {}
                scalar => params.push(abi_param(scalar, scalar.scalar().unwrap_or(Ty::I8))),
            }
            reprs.push(repr);
        }
        self.ret = self.repr(sig.types, sig.output).map_err(sig_err)?;
        let mut rets = Vec::with_capacity_in(1, self.alloc.clone());
        match self.ret {
            Repr::Struct(_) => return Err(BuildError::UnsupportedSig { what: "a struct returned by value" }),
            Repr::None => {}
            scalar => rets.push(abi_param(scalar, scalar.scalar().unwrap_or(Ty::I8))),
        }
        let own = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        self.b.set_signature(own);
        let entry = self.create();
        self.reached[entry.index()] = true;
        self.begin(entry);
        let mut values = Vec::with_capacity_in(params.len(), self.alloc.clone());
        for p in params.iter() {
            values.push(self.b.param(p.ty));
        }
        let mut next = values.iter();
        for (i, &repr) in reprs.iter().enumerate() {
            let v = match repr {
                Repr::None => V::None,
                _ => V::Scalar(*next.next().ok_or(malformed("a parameter without its value"))?),
            };
            self.bind_value(self.body.params[i], v)?;
        }
        self.work.push(Task::Eval(self.body.root));
        while let Some(task) = self.work.pop() {
            self.step(task)?;
        }
        let v = self.pop()?;
        if !self.vals.is_empty() {
            return Err(malformed("values left on the stack at the end (a build bug)"));
        }
        if self.live {
            let ret = self.ret;
            match self.arg(v, ret)? {
                Some(x) => self.b.ret(&[x]),
                None => self.b.ret(&[]),
            }
        } else {
            self.b.unreachable();
        }
        if let Some(error) = self.b.error() {
            return Err(BuildError::Lir(error.clone()));
        }
        Ok(())
    }

    fn step(&mut self, task: Task) -> Result<(), BuildError> {
        match task {
            Task::Eval(e) => {
                if !self.adjust_rows(e).is_empty() {
                    self.work.push(Task::Adjust(e));
                }
                self.raw(e)?;
            }
            Task::Raw(e) => self.raw(e)?,
            Task::Adjust(e) => self.adjust(e)?,
            Task::Place(e) => self.place_of(e)?,
            Task::Stmt(s) => self.stmt(s)?,
            Task::Discard => {
                self.pop()?;
            }
            Task::PushNone => self.vals.push(V::None),
            Task::Let(p) => {
                let v = self.pop()?;
                self.bind_value(p, v)?;
            }
            Task::BlockExit(e) => {
                let v = self.pop()?;
                let t = self.targets.pop().filter(|t| t.expr == e).ok_or(malformed("a block's break target out of order"))?;
                let arg = self.arg(v, t.repr)?;
                self.jump(t.exit, arg.as_slice());
                self.begin(t.exit);
                let v = self.join_param(t.repr);
                self.vals.push(v);
            }
            Task::If { expr, then, else_ } => {
                let c = self.pop()?;
                let c = self.cond(c)?;
                let then_block = self.create();
                let else_block = self.create();
                let join = self.create();
                self.br(c, (then_block, &[]), (else_block, &[]));
                self.begin(then_block);
                self.work.push(Task::IfThen { expr, else_, else_block, join });
                self.work.push(Task::Eval(then));
            }
            Task::IfThen { expr, else_, else_block, join } => {
                let v = self.pop()?;
                let repr = self.expr_repr(expr)?;
                let arg = self.arg(v, repr)?;
                self.jump(join, arg.as_slice());
                self.begin(else_block);
                match else_ {
                    Some(x) => {
                        self.work.push(Task::IfElse { expr, join });
                        self.work.push(Task::Eval(x));
                    }
                    None => {
                        if repr != Repr::None {
                            return Err(malformed("an `if` without `else` whose type has a value"));
                        }
                        self.jump(join, &[]);
                        self.begin(join);
                        self.vals.push(V::None);
                    }
                }
            }
            Task::IfElse { expr, join } => {
                let v = self.pop()?;
                let repr = self.expr_repr(expr)?;
                let arg = self.arg(v, repr)?;
                self.jump(join, arg.as_slice());
                self.begin(join);
                let v = self.join_param(repr);
                self.vals.push(v);
            }
            Task::LoopEnd(e) => {
                let t = self.targets.pop().filter(|t| t.expr == e).ok_or(malformed("a loop's target out of order"))?;
                let header = t.header.ok_or(malformed("a loop without its head"))?;
                self.jump(header, &[]);
                self.begin(t.exit);
                let v = self.join_param(t.repr);
                self.vals.push(v);
            }
            Task::Break(target, has_value) => {
                let v = if has_value { self.pop()? } else { V::None };
                let t = *self
                    .targets
                    .iter()
                    .rev()
                    .find(|t| t.expr == target)
                    .ok_or(malformed("a `break` whose target is not around it"))?;
                let arg = self.arg(v, t.repr)?;
                self.jump(t.exit, arg.as_slice());
                self.dead_block();
                self.vals.push(V::None);
            }
            Task::Return(has_value) => {
                let v = if has_value { self.pop()? } else { V::None };
                let ret = self.ret;
                match self.arg(v, ret)? {
                    Some(x) => self.b.ret(&[x]),
                    None => self.b.ret(&[]),
                }
                self.dead_block();
                self.vals.push(V::None);
            }
            Task::Match(e) => self.match_dispatch(e)?,
            Task::Arm { m, i } => self.arm(m, i)?,
            Task::Guard { m, i } => {
                let g = self.pop()?;
                let c = self.cond(g)?;
                let st = self.matches[m as usize];
                let next = self.arm_blocks[(st.first + i + 1) as usize];
                let body = self.create();
                self.br(c, (body, &[]), (next, &[]));
                self.begin(body);
            }
            Task::ArmEnd { m } => {
                let v = self.pop()?;
                let st = self.matches[m as usize];
                let arg = self.arg(v, st.repr)?;
                self.jump(st.join, arg.as_slice());
            }
            Task::MatchEnd => {
                let st = self.matches.pop().ok_or(malformed("a match's state out of order"))?;
                let none = self.arm_blocks[(st.first + st.arms) as usize];
                self.begin(none);
                self.b.unreachable();
                self.arm_blocks.truncate(st.first as usize);
                self.begin(st.join);
                let v = self.join_param(st.repr);
                self.vals.push(v);
            }
            Task::Call(e) => self.call(e)?,
            Task::Unary(e, op) => {
                let v = self.pop()?;
                let repr = self.expr_repr(e)?;
                let r = match (op, repr) {
                    (UnOp::Neg, Repr::Int { ty, .. }) => {
                        let x = self.scalar(v, ty)?;
                        self.b.unary(Op::Neg, Flags::NONE, x)
                    }
                    (UnOp::Not, Repr::Int { ty, .. }) => {
                        let x = self.scalar(v, ty)?;
                        self.b.unary(Op::Not, Flags::NONE, x)
                    }
                    (UnOp::Not, Repr::Bool) => {
                        let x = self.scalar(v, Ty::I8)?;
                        let one = self.b.iconst(Ty::I8, 1);
                        self.b.binary(Op::Xor, Flags::NONE, x, one)
                    }
                    _ => return Err(unsupported(e, "this unary operator on this type")),
                };
                self.vals.push(V::Scalar(r));
            }
            Task::Deref(e) => {
                let v = self.pop()?;
                let p = self.scalar(v, Ty::PTR)?;
                let repr = self.expr_repr(e)?;
                let v = self.load(repr, (p, 0));
                self.vals.push(v);
            }
            Task::Binary(e, op) => self.binary(e, op)?,
            Task::Short { expr, op, rhs } => {
                let l = self.pop()?;
                let c = self.cond(l)?;
                let rhs_block = self.create();
                let join = self.create();
                match op {
                    BinOp::And => {
                        let no = self.b.iconst(Ty::I8, 0);
                        self.br(c, (rhs_block, &[]), (join, &[no]));
                    }
                    BinOp::Or => {
                        let yes = self.b.iconst(Ty::I8, 1);
                        self.br(c, (join, &[yes]), (rhs_block, &[]));
                    }
                    _ => return Err(unsupported(expr, "a short-circuit operator other than `&&` and `||`")),
                }
                self.begin(rhs_block);
                self.work.push(Task::ShortEnd(join));
                self.work.push(Task::Eval(rhs));
            }
            Task::ShortEnd(join) => {
                let r = self.pop()?;
                let r = self.scalar(r, Ty::I8)?;
                self.jump(join, &[r]);
                self.begin(join);
                let p = self.b.param(Ty::I8);
                self.vals.push(V::Scalar(p));
            }
            Task::Assign(e) => {
                let to = self.pop()?;
                let v = self.pop()?;
                let Expr::Assign { lhs, .. } = self.expr(e)? else { return Err(malformed("an assignment's task on another expression")) };
                let repr = self.expr_repr(lhs)?;
                let V::Place(p, off) = to else { return Err(malformed("an assignment to no place")) };
                self.store(v, repr, (p, off))?;
                self.vals.push(V::None);
            }
            Task::AssignOp(e, op) => {
                let to = self.pop()?;
                let r = self.pop()?;
                let Expr::AssignOp { lhs, rhs, .. } = self.expr(e)? else {
                    return Err(malformed("an assignment's task on another expression"));
                };
                let lrepr = self.expr_repr(lhs)?;
                let rrepr = self.adjusted_repr(rhs)?;
                let V::Place(p, off) = to else { return Err(malformed("an assignment to no place")) };
                let ty = lrepr.scalar().ok_or(unsupported(e, "a compound assignment to a non-scalar"))?;
                let l = self.load(lrepr, (p, off));
                let l = self.scalar(l, ty)?;
                let rty = rrepr.scalar().ok_or(unsupported(e, "a compound assignment of a non-scalar"))?;
                let r = self.scalar(r, rty)?;
                let x = self.arith(e, op, l, r, lrepr, rrepr)?;
                self.store(V::Scalar(x), lrepr, (p, off))?;
                self.vals.push(V::None);
            }
            Task::Cast(e) => self.cast(e)?,
            Task::StructInit(at) => {
                let v = self.pop()?;
                let V::Place(p, off) = *self.vals.last().ok_or(malformed("a struct literal without its place"))? else {
                    return Err(malformed("a struct literal without its place"));
                };
                let (_, field_off, repr) = self.layout_fields[at as usize];
                self.store(v, repr, (p, off + field_off as i32))?;
            }
            Task::Field(e) | Task::FieldPlace(e) => {
                let v = self.pop()?;
                let V::Place(p, off) = v else { return Err(malformed("a field of a value that is not a place")) };
                let (field_off, repr) = self.field(e)?;
                let at = (p, off + field_off as i32);
                let v = match task {
                    Task::Field(_) => self.load(repr, at),
                    _ => V::Place(at.0, at.1),
                };
                self.vals.push(v);
            }
            Task::DerefPlace => {
                let v = self.pop()?;
                let p = self.scalar(v, Ty::PTR)?;
                self.vals.push(V::Place(p, 0));
            }
            Task::Spill(e) => {
                let v = self.pop()?;
                if let V::Place(..) = v {
                    self.vals.push(v);
                } else {
                    let repr = self.expr_repr(e)?;
                    let at = self.temp(repr);
                    self.store(v, repr, at)?;
                    self.vals.push(V::Place(at.0, at.1));
                }
            }
            Task::Ref => {
                let v = self.pop()?;
                let V::Place(p, off) = v else { return Err(malformed("a borrow of no place")) };
                let a = self.address((p, off));
                self.vals.push(V::Scalar(a));
            }
        }
        Ok(())
    }

    /// Applies `e`'s adjustments to the value on top: those that change no bits, and
    /// `NeverToAny` of an expression that diverged. Any other is refused, never skipped.
    fn adjust(&mut self, e: ExprId) -> Result<(), BuildError> {
        let rows = self.adjust_rows(e);
        let kinds = &self.checked.adjusts.column::<1>()[rows.clone()];
        let tys = &self.checked.adjusts.column::<2>()[rows.clone()];
        match kinds {
            [AdjustKind::NeverToAny] => {
                let t = tys[0];
                let repr = self.repr_at(e, t)?;
                let v = self.pop()?;
                let v = match repr {
                    Repr::None => V::None,
                    Repr::Struct(_) => {
                        let (p, off) = self.place(v, repr)?;
                        V::Place(p, off)
                    }
                    scalar => {
                        let ty = scalar.scalar().unwrap_or(Ty::I8);
                        V::Scalar(self.scalar(v, ty)?)
                    }
                };
                self.vals.push(v);
                Ok(())
            }
            // A raw pointer's mutability: the same bits.
            [AdjustKind::MutToConstPointer] => Ok(()),
            // A reborrow (`&*r`, `&mut *r` to `*const`/`*mut`): the same pointer.
            [AdjustKind::Deref { overloaded: None }, AdjustKind::Borrow { .. }] => {
                let t = self.expr_ty(e)?;
                match self.types.node(t) {
                    Ok(PNode::Ref(..) | PNode::Ptr(..)) => Ok(()),
                    _ => Err(unsupported(e, "a borrow of a dereference of a non-pointer")),
                }
            }
            [] => Ok(()),
            _ => Err(unsupported(e, "this adjustment (an autoref, an overloaded deref, an unsizing or a fn pointer)")),
        }
    }

    /// The offset and representation of field expression `e`'s field.
    fn field(&mut self, e: ExprId) -> Result<(u32, Repr), BuildError> {
        let base = match self.expr(e)? {
            Expr::Field { base, .. } | Expr::TupleField { base, .. } => base,
            _ => return Err(malformed("a field's task on another expression")),
        };
        let col = self.checked.fields.column::<0>();
        let row = col.binary_search(&e).map_err(|_| malformed("a field access without its record row"))?;
        let at = self.checked.fields.column::<2>()[row];
        let repr = self.adjusted_repr(base)?;
        let Repr::Struct(l) = repr else { return Err(unsupported(e, "a field of a value that is not a struct")) };
        let layout = &self.layouts[l as usize];
        if at >= layout.len {
            return Err(malformed("a field index past its struct's fields"));
        }
        let (_, off, repr) = self.layout_fields[(layout.first + at) as usize];
        Ok((off, repr))
    }

    /// Pushes the tasks that evaluate `e` as a place.
    fn place_of(&mut self, e: ExprId) -> Result<(), BuildError> {
        let rows = self.adjust_rows(e);
        let kinds = &self.checked.adjusts.column::<1>()[rows];
        match kinds {
            [] => {}
            [AdjustKind::Deref { overloaded: None }] => {
                self.work.push(Task::DerefPlace);
                self.work.push(Task::Raw(e));
                return Ok(());
            }
            _ => return Err(unsupported(e, "this adjustment of a place")),
        }
        match self.expr(e)? {
            Expr::Local(p) => {
                let (slot, _) = self.slots.get(p.ix()).copied().flatten().ok_or(malformed("a local used before its binding"))?;
                let at = self.b.slot_addr(slot);
                self.vals.push(V::Place(at, 0));
            }
            Expr::Paren(inner) => self.work.push(Task::Place(inner)),
            Expr::Field { base, .. } | Expr::TupleField { base, .. } => {
                self.work.push(Task::FieldPlace(e));
                self.work.push(Task::Place(base));
            }
            Expr::Unary { op: UnOp::Deref, operand } => {
                self.work.push(Task::DerefPlace);
                self.work.push(Task::Eval(operand));
            }
            _ => {
                self.work.push(Task::Spill(e));
                self.work.push(Task::Raw(e));
            }
        }
        Ok(())
    }

    fn stmt(&mut self, s: StmtId) -> Result<(), BuildError> {
        let stmt = self.body.stmts.get(s.ix()).copied().ok_or(malformed("a statement past `stmts`"))?;
        match stmt {
            Stmt::Let { else_: Some(e), .. } => return Err(unsupported(e, "`let`-`else`")),
            Stmt::Let { pat, init: Some(init), else_: None, .. } => {
                self.work.push(Task::Let(pat));
                self.work.push(Task::Eval(init));
            }
            Stmt::Let { pat, init: None, else_: None, .. } => {
                if !matches!(self.pat(pat)?, Pat::Wild) {
                    self.bind(pat)?;
                }
            }
            Stmt::Expr { expr, .. } => {
                self.work.push(Task::Discard);
                self.work.push(Task::Eval(expr));
            }
            Stmt::Item => {}
        }
        Ok(())
    }

    /// Pushes the tasks of `e` (its adjustments are the caller's), or evaluates a leaf.
    fn raw(&mut self, e: ExprId) -> Result<(), BuildError> {
        match self.expr(e)? {
            Expr::Missing => return Err(unsupported(e, "a missing expression")),
            Expr::Lit(lit) => self.lit(e, lit)?,
            Expr::Local(p) => {
                let (slot, repr) = self.slots.get(p.ix()).copied().flatten().ok_or(malformed("a local used before its binding"))?;
                let at = self.b.slot_addr(slot);
                let v = self.load(repr, (at, 0));
                self.vals.push(v);
            }
            Expr::Path(_) => {
                let t = self.expr_ty(e)?;
                match self.types.node(t) {
                    Ok(PNode::FnDef(..)) => self.vals.push(V::None),
                    _ => return Err(unsupported(e, "a path to other than a fn item (a const, a static, a unit struct)")),
                }
            }
            Expr::Paren(inner) => self.work.push(Task::Eval(inner)),
            Expr::Block { stmts, tail, .. } => {
                let list = self.body.stmt_lists.get(stmts.range()).ok_or(malformed("a block's statements past `stmt_lists`"))?;
                if self.break_target[e.ix()] {
                    let repr = self.expr_repr(e)?;
                    let exit = self.create();
                    self.targets.push(Target { expr: e, header: None, exit, repr });
                    self.work.push(Task::BlockExit(e));
                }
                match tail {
                    Some(t) => self.work.push(Task::Eval(t)),
                    None => self.work.push(Task::PushNone),
                }
                for &s in list.iter().rev() {
                    self.work.push(Task::Stmt(s));
                }
            }
            Expr::If { cond, then, else_ } => {
                self.work.push(Task::If { expr: e, then, else_ });
                self.work.push(Task::Eval(cond));
            }
            Expr::Match { scrutinee, .. } => {
                self.work.push(Task::Match(e));
                self.work.push(Task::Eval(scrutinee));
            }
            Expr::Loop { body } => {
                let repr = self.expr_repr(e)?;
                let header = self.create();
                let exit = self.create();
                self.jump(header, &[]);
                self.begin(header);
                self.targets.push(Target { expr: e, header: Some(header), exit, repr });
                self.work.push(Task::LoopEnd(e));
                self.work.push(Task::Discard);
                self.work.push(Task::Eval(body));
            }
            Expr::Break { value, target } => {
                self.work.push(Task::Break(target, value.is_some()));
                if let Some(v) = value {
                    self.work.push(Task::Eval(v));
                }
            }
            Expr::Continue { target } => {
                let t = *self
                    .targets
                    .iter()
                    .rev()
                    .find(|t| t.expr == target)
                    .ok_or(malformed("a `continue` whose loop is not around it"))?;
                let header = t.header.ok_or(malformed("a `continue` of a block"))?;
                self.jump(header, &[]);
                self.dead_block();
                self.vals.push(V::None);
            }
            Expr::Return { value } => {
                self.work.push(Task::Return(value.is_some()));
                if let Some(v) = value {
                    self.work.push(Task::Eval(v));
                }
            }
            Expr::Call { callee, args } => {
                let t = self.expr_ty(callee)?;
                match self.types.node(t) {
                    Ok(PNode::FnDef(_, generics)) if generics.len == 0 => {}
                    Ok(PNode::FnDef(..)) => return Err(unsupported(e, "a call of a generic fn")),
                    _ => return Err(unsupported(e, "a call of other than a fn item (a fn pointer, a closure)")),
                }
                let list = self.body.expr_lists.get(args.range()).ok_or(malformed("a call's arguments past `expr_lists`"))?;
                self.work.push(Task::Call(e));
                for &a in list.iter().rev() {
                    self.work.push(Task::Eval(a));
                }
                self.work.push(Task::Discard);
                self.work.push(Task::Eval(callee));
            }
            Expr::MethodCall { .. } => return Err(unsupported(e, "a method call")),
            Expr::Field { base, .. } | Expr::TupleField { base, .. } => {
                self.work.push(Task::Field(e));
                self.work.push(Task::Place(base));
            }
            Expr::Unary { op: UnOp::Deref, operand } => {
                let t = self.adjusted_ty(operand)?;
                match self.types.node(t) {
                    Ok(PNode::Ptr(..) | PNode::Ref(..)) => {}
                    _ => return Err(unsupported(e, "an overloaded dereference")),
                }
                self.work.push(Task::Deref(e));
                self.work.push(Task::Eval(operand));
            }
            Expr::Unary { op, operand } => {
                self.work.push(Task::Unary(e, op));
                self.work.push(Task::Eval(operand));
            }
            Expr::Binary { op: op @ (BinOp::And | BinOp::Or), lhs, rhs } => {
                self.work.push(Task::Short { expr: e, op, rhs });
                self.work.push(Task::Eval(lhs));
            }
            Expr::Binary { op, lhs, rhs } => {
                self.work.push(Task::Binary(e, op));
                self.work.push(Task::Eval(rhs));
                self.work.push(Task::Eval(lhs));
            }
            // The right side first, then the place: Rust's order for a primitive assignment.
            Expr::Assign { lhs, rhs } => {
                self.work.push(Task::Assign(e));
                self.work.push(Task::Place(lhs));
                self.work.push(Task::Eval(rhs));
            }
            Expr::AssignOp { op, lhs, rhs } => {
                self.work.push(Task::AssignOp(e, op));
                self.work.push(Task::Place(lhs));
                self.work.push(Task::Eval(rhs));
            }
            Expr::Ref { operand, .. } => {
                self.work.push(Task::Ref);
                self.work.push(Task::Place(operand));
            }
            Expr::Cast { operand, .. } => {
                self.work.push(Task::Cast(e));
                self.work.push(Task::Eval(operand));
            }
            Expr::Struct { base: Some(_), .. } => return Err(unsupported(e, "a struct update (`..base`)")),
            Expr::Struct { fields, base: None, .. } => {
                let repr = self.expr_repr(e)?;
                let Repr::Struct(l) = repr else { return Err(unsupported(e, "a struct literal of other than a struct")) };
                let inits = self.body.field_inits.get(fields.range()).ok_or(malformed("a struct literal's fields past `field_inits`"))?;
                let (first, len) = (self.layouts[l as usize].first, self.layouts[l as usize].len);
                if inits.len() != len as usize {
                    return Err(malformed("a struct literal that does not name every field"));
                }
                let at = self.temp(repr);
                self.vals.push(V::Place(at.0, at.1));
                for init in inits.iter().rev() {
                    let row = (first..first + len)
                        .find(|&i| self.layout_fields[i as usize].0 == init.name)
                        .ok_or(malformed("a struct literal naming a field its struct lacks"))?;
                    self.work.push(Task::StructInit(row));
                    self.work.push(Task::Eval(init.expr));
                }
            }
            Expr::Tuple { .. } => return Err(unsupported(e, "a tuple")),
            Expr::Array { .. } | Expr::Repeat { .. } => return Err(unsupported(e, "an array")),
            Expr::Index { .. } => return Err(unsupported(e, "indexing")),
            Expr::Closure { .. } => return Err(unsupported(e, "a closure")),
        }
        Ok(())
    }

    fn lit(&mut self, e: ExprId, lit: Lit) -> Result<(), BuildError> {
        let v = match lit {
            Lit::Bool(b) => self.b.iconst(Ty::I8, u128::from(b)),
            Lit::Int(_) => {
                let Repr::Int { ty, .. } = self.expr_repr(e)? else {
                    return Err(unsupported(e, "an integer literal of a non-integer type"));
                };
                let value = self.adts.int_lit(self.body.owner, e).ok_or(BuildError::NoLiteralValue { expr: e })?;
                if mask(value, ty.bits(PTR_BITS)) != value {
                    return Err(unsupported(e, "an integer literal past its type's width"));
                }
                self.b.iconst(ty, value)
            }
            Lit::ByteStr(len) => {
                let bytes = self.adts.byte_str(self.body.owner, e).ok_or(BuildError::NoLiteralValue { expr: e })?;
                if bytes.len() as u64 != len {
                    return Err(malformed("a byte string whose bytes are not its length"));
                }
                let data = self.b.data(bytes, 0, &[]);
                self.b.data_addr(data)
            }
            Lit::Str => return Err(unsupported(e, "a string literal (a wide pointer)")),
            Lit::Char => return Err(unsupported(e, "a char literal")),
            Lit::Float(_) => return Err(unsupported(e, "a float literal")),
        };
        self.vals.push(V::Scalar(v));
        Ok(())
    }

    /// An arithmetic or bitwise operation of Rust's wrapping semantics (overflow is not checked
    /// in this tier); a shift's amount is brought to the left side's width first.
    fn arith(&mut self, e: ExprId, op: BinOp, l: Value, r: Value, lrepr: Repr, rrepr: Repr) -> Result<Value, BuildError> {
        let (ty, signed) = match lrepr {
            Repr::Int { ty, signed } => (ty, signed),
            Repr::Bool if matches!(op, BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor) => (Ty::I8, false),
            _ => return Err(unsupported(e, "arithmetic on this type (a pointer, a bool)")),
        };
        let o = match op {
            BinOp::Add => Op::Add,
            BinOp::Sub => Op::Sub,
            BinOp::Mul => Op::Mul,
            BinOp::Div => {
                if signed {
                    Op::Sdiv
                } else {
                    Op::Udiv
                }
            }
            BinOp::Rem => {
                if signed {
                    Op::Srem
                } else {
                    Op::Urem
                }
            }
            BinOp::BitAnd => Op::And,
            BinOp::BitOr => Op::Or,
            BinOp::BitXor => Op::Xor,
            BinOp::Shl | BinOp::Shr => {
                let Repr::Int { ty: rty, .. } = rrepr else { return Err(unsupported(e, "a shift by a non-integer")) };
                let (from, to) = (rty.bits(PTR_BITS), ty.bits(PTR_BITS));
                let amount = if from > to {
                    self.b.convert(Op::Trunc, Flags::NONE, ty, r)
                } else if from < to {
                    self.b.convert(Op::Zext, Flags::NONE, ty, r)
                } else {
                    r
                };
                let o = match (op, signed) {
                    (BinOp::Shl, _) => Op::Shl,
                    (_, true) => Op::Ashr,
                    (_, false) => Op::Lshr,
                };
                return Ok(self.b.binary(o, Flags::NONE, l, amount));
            }
            _ => return Err(unsupported(e, "this operator in arithmetic")),
        };
        Ok(self.b.binary(o, Flags::NONE, l, r))
    }

    fn binary(&mut self, e: ExprId, op: BinOp) -> Result<(), BuildError> {
        let r = self.pop()?;
        let l = self.pop()?;
        let Expr::Binary { lhs, rhs, .. } = self.expr(e)? else { return Err(malformed("a binary task on another expression")) };
        let lrepr = self.adjusted_repr(lhs)?;
        let rrepr = self.adjusted_repr(rhs)?;
        let lty = lrepr.scalar().ok_or(unsupported(e, "a binary operator on a non-scalar"))?;
        let rty = rrepr.scalar().ok_or(unsupported(e, "a binary operator on a non-scalar"))?;
        let l = self.scalar(l, lty)?;
        let r = self.scalar(r, rty)?;
        let signed = matches!(lrepr, Repr::Int { signed: true, .. });
        let cond = match op {
            BinOp::Eq => Some(IntCond::Eq),
            BinOp::Ne => Some(IntCond::Ne),
            BinOp::Lt => Some(if signed { IntCond::Slt } else { IntCond::Ult }),
            BinOp::Le => Some(if signed { IntCond::Sle } else { IntCond::Ule }),
            BinOp::Gt => Some(if signed { IntCond::Sgt } else { IntCond::Ugt }),
            BinOp::Ge => Some(if signed { IntCond::Sge } else { IntCond::Uge }),
            _ => None,
        };
        let v = match cond {
            Some(c) => {
                if lty != rty {
                    return Err(malformed("a comparison of two types"));
                }
                let bit = self.b.icmp(c, l, r);
                self.b.convert(Op::Zext, Flags::NONE, Ty::I8, bit)
            }
            None => self.arith(e, op, l, r, lrepr, rrepr)?,
        };
        self.vals.push(V::Scalar(v));
        Ok(())
    }

    fn cast(&mut self, e: ExprId) -> Result<(), BuildError> {
        let v = self.pop()?;
        let Expr::Cast { operand, .. } = self.expr(e)? else { return Err(malformed("a cast task on another expression")) };
        let from = self.adjusted_repr(operand)?;
        let to = self.expr_repr(e)?;
        let out = match (from, to) {
            (Repr::None, Repr::None) => V::None,
            (Repr::Int { .. } | Repr::Bool, Repr::Int { ty, .. }) => {
                let fty = from.scalar().unwrap_or(Ty::I8);
                let x = self.scalar(v, fty)?;
                let signed = matches!(from, Repr::Int { signed: true, .. });
                V::Scalar(self.resize(x, fty, ty, signed))
            }
            (Repr::Bool, Repr::Bool) => V::Scalar(self.scalar(v, Ty::I8)?),
            (Repr::Ptr, Repr::Ptr) => V::Scalar(self.scalar(v, Ty::PTR)?),
            (Repr::Ptr, Repr::Int { ty, .. }) => {
                let x = self.scalar(v, Ty::PTR)?;
                let addr = self.b.convert(Op::Expose, Flags::NONE, Ty::I64, x);
                V::Scalar(self.resize(addr, Ty::I64, ty, false))
            }
            (Repr::Int { ty, signed }, Repr::Ptr) => {
                let x = self.scalar(v, ty)?;
                let addr = self.resize(x, ty, Ty::I64, signed);
                V::Scalar(self.b.convert(Op::FromExposed, Flags::NONE, Ty::PTR, addr))
            }
            _ => return Err(unsupported(e, "this cast (a float, a char, a fn item to a pointer)")),
        };
        self.vals.push(out);
        Ok(())
    }

    /// An integer to another width: truncated, or extended by the source's signedness.
    fn resize(&mut self, x: Value, from: Ty, to: Ty, signed: bool) -> Value {
        let (f, t) = (from.bits(PTR_BITS), to.bits(PTR_BITS));
        if f == t {
            x
        } else if f > t {
            self.b.convert(Op::Trunc, Flags::NONE, to, x)
        } else if signed {
            self.b.convert(Op::Sext, Flags::NONE, to, x)
        } else {
            self.b.convert(Op::Zext, Flags::NONE, to, x)
        }
    }

    fn call(&mut self, e: ExprId) -> Result<(), BuildError> {
        let Expr::Call { callee, args } = self.expr(e)? else { return Err(malformed("a call task on another expression")) };
        let list = self.body.expr_lists.get(args.range()).ok_or(malformed("a call's arguments past `expr_lists`"))?;
        let n = list.len();
        if self.vals.len() < n {
            return Err(malformed("a call's arguments missing from the value stack (a build bug)"));
        }
        let t = self.expr_ty(callee)?;
        let Ok(PNode::FnDef(key, _)) = self.types.node(t) else {
            return Err(malformed("a call whose callee is not a fn item"));
        };
        let mut params = Vec::with_capacity_in(n, self.alloc.clone());
        let mut values = Vec::with_capacity_in(n, self.alloc.clone());
        let base = self.vals.len() - n;
        for (i, &a) in list.iter().enumerate() {
            let repr = self.adjusted_repr(a)?;
            let v = self.vals[base + i];
            match repr {
                Repr::None => {}
                Repr::Struct(_) => return Err(unsupported(a, "a struct passed by value")),
                scalar => {
                    let ty = scalar.scalar().unwrap_or(Ty::I8);
                    values.push(self.scalar(v, ty)?);
                    params.push(abi_param(scalar, ty));
                }
            }
        }
        self.vals.truncate(base);
        let rrepr = self.expr_repr(e)?;
        let mut rets = Vec::with_capacity_in(1, self.alloc.clone());
        match rrepr {
            Repr::None => {}
            Repr::Struct(_) => return Err(unsupported(e, "a struct returned by value")),
            scalar => rets.push(abi_param(scalar, scalar.scalar().unwrap_or(Ty::I8))),
        }
        let place = match self.refs.iter().position(|&k| k == key) {
            Some(at) => at as u32,
            None => {
                self.refs.push(key);
                (self.refs.len() - 1) as u32
            }
        };
        let reference = self.b.reference(RefKind::Func, place);
        let sig = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        let r = self.b.call(Flags::NONE, reference, sig, &values);
        let ty = self.expr_ty(e)?;
        if let Ok(PNode::Never) = self.types.node(ty) {
            self.b.unreachable();
            self.dead_block();
            self.vals.push(V::None);
            return Ok(());
        }
        self.vals.push(match rrepr {
            Repr::None => V::None,
            _ => V::Scalar(r),
        });
        Ok(())
    }

    // Match.

    /// The values pattern `p` matches, into `out`; `true` when it matches every value.
    fn pat_values(&mut self, p: PatId, width: u32, out: &mut Vec<u128, A>) -> Result<bool, BuildError> {
        let refuse = |what| BuildError::UnsupportedPat { pat: p, what };
        let one = |w: &mut Self, alt: PatId, out: &mut Vec<u128, A>| -> Result<bool, BuildError> {
            match w.pat(alt)? {
                Pat::Wild => Ok(true),
                Pat::Lit(Lit::Bool(b)) => {
                    out.push(u128::from(b));
                    Ok(false)
                }
                Pat::Lit(Lit::Int(_)) | Pat::NegLit(Lit::Int(_)) => {
                    let v = w.adts.int_pat(w.body.owner, alt).ok_or(BuildError::NoPatValue { pat: alt })?;
                    if mask(v, width) != v {
                        return Err(BuildError::UnsupportedPat { pat: alt, what: "an integer literal past its type's width" });
                    }
                    let v = match w.pat(alt)? {
                        Pat::NegLit(_) => mask(0u128.wrapping_sub(v), width),
                        _ => v,
                    };
                    out.push(v);
                    Ok(false)
                }
                _ => Err(BuildError::UnsupportedPat { pat: alt, what: "a pattern other than a literal, a binding or `_`" }),
            }
        };
        match self.pat(p)? {
            Pat::Bind { ann, sub: None, .. } if ann.by_ref.is_none() => Ok(true),
            Pat::Bind { .. } => Err(refuse("a `ref` binding or a binding with a subpattern")),
            Pat::Or { alts } => {
                let list = self.body.pat_lists.get(alts.range()).ok_or(malformed("an or-pattern past `pat_lists`"))?;
                let mut always = false;
                for &alt in list {
                    always |= one(self, alt, out)?;
                }
                Ok(always)
            }
            _ => one(self, p, out),
        }
    }

    fn match_dispatch(&mut self, e: ExprId) -> Result<(), BuildError> {
        let Expr::Match { scrutinee, arms } = self.expr(e)? else { return Err(malformed("a match task on another expression")) };
        let s = self.pop()?;
        let srepr = self.adjusted_repr(scrutinee)?;
        let (sty, width) = match srepr {
            Repr::Int { ty, .. } => (ty, ty.bits(PTR_BITS)),
            Repr::Bool => (Ty::I8, 8),
            _ => return Err(unsupported(e, "a match on other than an integer or a bool")),
        };
        let x = self.scalar(s, sty)?;
        let list = self.body.arms.get(arms.range()).ok_or(malformed("a match's arms past `arms`"))?;
        let repr = self.expr_repr(e)?;
        let guarded = list.iter().any(|a| a.guard.is_some());
        let first = self.arm_blocks.len() as u32;
        for _ in 0..=list.len() {
            let block = self.create();
            self.arm_blocks.push(block);
        }
        let join = self.create();
        let n = list.len() as u32;
        if guarded {
            let to = self.arm_blocks[first as usize];
            self.jump(to, &[]);
        } else {
            let mut values = Vec::new_in(self.alloc.clone());
            let mut cases: Vec<(u128, Block), A> = Vec::new_in(self.alloc.clone());
            let mut default = self.arm_blocks[(first + n) as usize];
            for (i, arm) in list.iter().enumerate() {
                values.clear();
                let always = self.pat_values(arm.pat, width, &mut values)?;
                let block = self.arm_blocks[first as usize + i];
                for &v in values.iter() {
                    if !cases.iter().any(|&(c, _)| c == v) {
                        cases.push((v, block));
                    }
                }
                if always {
                    default = block;
                    break;
                }
            }
            let mut tos = Vec::with_capacity_in(cases.len(), self.alloc.clone());
            for &(v, block) in cases.iter() {
                self.reach(block);
                tos.push((v, (block, &[] as &[Value])));
            }
            self.reach(default);
            self.b.switch(x, (default, &[]), &tos);
        }
        self.matches.push(MatchState { expr: e, scrutinee: x, width, join, repr, first, arms: n, guarded });
        let m = (self.matches.len() - 1) as u32;
        self.work.push(Task::MatchEnd);
        for i in (0..n).rev() {
            self.work.push(Task::Arm { m, i });
        }
        Ok(())
    }

    fn arm(&mut self, m: u32, i: u32) -> Result<(), BuildError> {
        let st = self.matches[m as usize];
        let Expr::Match { arms, .. } = self.expr(st.expr)? else { return Err(malformed("a match's state on another expression")) };
        let arm = *self.body.arms.get(arms.range()).and_then(|l| l.get(i as usize)).ok_or(malformed("an arm past `arms`"))?;
        let block = self.arm_blocks[(st.first + i) as usize];
        let next = self.arm_blocks[(st.first + i + 1) as usize];
        self.begin(block);
        if st.guarded {
            let mut values = Vec::new_in(self.alloc.clone());
            let always = self.pat_values(arm.pat, st.width, &mut values)?;
            if !always {
                let ty = Ty::int(st.width).ok_or(malformed("a scrutinee of no integer width"))?;
                let mut c = None;
                for &v in values.iter() {
                    let k = self.b.iconst(ty, v);
                    let hit = self.b.icmp(IntCond::Eq, st.scrutinee, k);
                    c = Some(match c {
                        Some(prev) => self.b.binary(Op::Or, Flags::NONE, prev, hit),
                        None => hit,
                    });
                }
                let c = c.ok_or(malformed("an or-pattern of no alternatives"))?;
                let pass = self.create();
                self.br(c, (pass, &[]), (next, &[]));
                self.begin(pass);
            }
        }
        if let Pat::Bind { .. } = self.pat(arm.pat)? {
            self.bind_value(arm.pat, V::Scalar(st.scrutinee))?;
        }
        self.work.push(Task::ArmEnd { m });
        self.work.push(Task::Eval(arm.body));
        if let Some(g) = arm.guard {
            self.work.push(Task::Guard { m, i });
            self.work.push(Task::Eval(g));
        }
        Ok(())
    }
}
