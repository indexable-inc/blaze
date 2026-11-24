//! The walk: one work stack of tasks (an expression to evaluate, or the continuation of one
//! whose operands are done) and one stack of the values evaluated, so a body of any depth is
//! built in constant native stack.

use core::alloc::Allocator;

use std::alloc::Global;

use blaze_lir::builder::{MemAttrs, Param, Variadic};
use blaze_lir::func::{BlockKind, Conv, Ext, RefId, RefKind, SlotId, SlotKind};
use blaze_lir::{Block, Builder, Flags, Full, IntCond, Op, Ty, Value};
use blaze_code::record::{SUBJECT_SHOWN, Subject, TrapKind, TrapSource};
use blaze_own::Alloc;
use blaze_typeck::{
    AdjustKind, AdtKind, BinOp, BodyView, Checked, CtorKind, Def, Edition, Expr, ExprId, FnSig, IntTy, ItemKey, LangItem, List, Lit, Mutbl,
    Name, PNode, PTy, PTys, Pat, PatId, PortableTys, Stmt, StmtId, UintTy, UnOp,
};

use crate::abi::Pass;
use crate::cabi::{self, CAbiError, Class};
use crate::data::DataRecord;
use crate::intrinsics::{self, Arg, Call, GenericArg, IntrinsicCx, IntrinsicError, Out, Which};
use crate::layout::{LayoutCx, LayoutError, LayoutId, Layouts, Prim, Refusal, Tail, Variants};
use crate::vtable::ObjectRead;
use crate::{Abi, Adts, BuildError, Built, Instance, Intrinsic, Subst, ground};

#[path = "closure.rs"]
mod closure;
pub(crate) use closure::{build_builtin_clone, build_builtin_from, build_ctor_fn, build_fn_shim, build_reify_shim, closures_in};
#[cfg(test)]
pub(crate) use closure::{empty_record as empty_record_for_tests, empty_view as empty_view_for_tests};
#[path = "coroutine.rs"]
pub(crate) mod coroutine;

pub(crate) mod arith_checks;
mod asm;
mod c_variadic;
mod casts;
mod const_scalar;
mod fmt;
#[cfg(test)]
pub(crate) use fmt::encode_text as fmt_encode_text_for_tests;
mod discard;
mod index;
mod lets;
mod matches;
mod halfquad;
mod numbers;
mod operators;
mod promote;
mod moves;
mod slice_glue;
mod strs;
mod super_let;
mod target_intr;
mod temps;
mod unsize_nested;
mod unsized_tail;
mod nounwind;
mod coroutine_panics;
mod unwind;
mod unwind_ladder;
mod val_stack;
mod await_spill;
mod state_names;
mod validity;
mod wide_cmp;
mod track_caller;

/// The target's pointer width: the ladder runs on AArch64.
const PTR_BITS: u32 = 64;

/// The lang item an index past an array's end calls.
const BOUNDS_FN: &str = "panic_bounds_check";

/// The lang item fn a drop calls; its instance at `T` is built as `T`'s drop glue.
const DROP_FN: &str = "drop_in_place";

/// No parent expression, or no declaring block (a parameter), in the flag scan.
const NO_EXPR: u32 = u32::MAX;

/// A record type not grounded yet, in `Walk::grounded`.
const UNGROUNDED: PTy = PTy(u32::MAX);

/// How a Rust type is held.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Repr {
    /// `()`, `!`, a fn item, a zero-sized aggregate with no drop glue in it: no value. Never a
    /// type that needs drop (`Layout::placed`).
    None,
    Int { ty: Ty, signed: bool },
    /// An `f32` or `f64` (walk/numbers.rs).
    Float(Ty),
    /// An `i8` of 0 or 1.
    Bool,
    Ptr,
    /// An aggregate held in memory (a struct, a tuple, an array), by its row of `layouts`: one
    /// of a non-zero size, or a zero-sized one that is `Layout::placed`.
    Agg(u32),
}

impl Repr {
    /// The LIR type of a scalar.
    fn scalar(self) -> Option<Ty> {
        match self.kept() {
            Kept::Scalar(ty) => Some(ty),
            Kept::None | Kept::Agg(_) => None,
        }
    }

    /// How a value of this repr is kept, three ways: a match on it has the scalar's LIR type
    /// in hand, with no default for "not a scalar" (rule 3).
    fn kept(self) -> Kept {
        match self {
            Repr::None => Kept::None,
            Repr::Agg(l) => Kept::Agg(l),
            Repr::Int { ty, .. } | Repr::Float(ty) => Kept::Scalar(ty),
            Repr::Bool => Kept::Scalar(Ty::I8),
            Repr::Ptr => Kept::Scalar(Ty::PTR),
        }
    }
}

/// A [`Repr`] by how it is kept ([`Repr::kept`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kept {
    None,
    /// An aggregate in memory, by its row of `layouts`.
    Agg(u32),
    /// A scalar of this LIR type.
    Scalar(Ty),
}

/// At most this many scalar leaves are copied unrolled; an array of more is copied in a loop.
const UNROLL_LEAVES: u64 = 16;

/// An evaluated expression.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum V {
    /// No value: a type without one, or an expression that diverged.
    None,
    Scalar(Value),
    /// A place: a pointer and a byte offset from it.
    Place(Value, i32),
    /// The place of an unsized value (`*s` of `s: &[T]`, `&str`, `*const dyn Tr`), held as the
    /// memory of the wide pointer that reaches it, a pointer and a byte offset: the Reference,
    /// "Dynamically Sized Types": a pointer to one carries its metadata, and a place of one is
    /// only borrowed (`&*s` is the pointer again) or indexed, never copied.
    Unsized(Value, i32),
}

/// A type outside the subset, or a type the record should not hold, before it is placed.
enum TyErr {
    Unsupported(&'static str),
    Malformed(&'static str),
    /// Grounding refused it with an error of its own.
    Other(BuildError),
}

/// Grounding's refusals as type errors, to be placed where the type was met.
fn ground_err(error: BuildError) -> TyErr {
    match error {
        BuildError::UnsupportedSig { what } => TyErr::Unsupported(what),
        BuildError::Malformed { what } => TyErr::Malformed(what),
        other => TyErr::Other(other),
    }
}

fn signature_error(error: TyErr) -> BuildError {
    match error {
        TyErr::Unsupported(what) => BuildError::UnsupportedSig { what },
        TyErr::Malformed(what) => malformed(what),
        TyErr::Other(error) => error,
    }
}

/// Layout's refusals as type errors (`Layouts` is the one layout: rule 30).
fn layout_err(error: LayoutError) -> TyErr {
    match error.why {
        Refusal::Unsupported(what) => TyErr::Unsupported(what),
        Refusal::Malformed(what) => TyErr::Malformed(what),
        Refusal::Recursive => TyErr::Malformed("an aggregate that contains itself by value"),
        Refusal::TooLarge => TyErr::Unsupported("a type larger than `isize::MAX` bytes"),
        Refusal::NoAdt(adt) => TyErr::Other(BuildError::NoAdt { adt }),
        Refusal::Ground(error) | Refusal::FieldGround { error, .. } => ground_err(error),
        Refusal::Missing(m) => TyErr::Other(BuildError::MissingRecord { item: m.item }),
        // A clean check never holds an extern type by value (it is not `Sized`).
        Refusal::NoSize(_) => TyErr::Malformed("an extern type held by value: it has no size"),
    }
}

/// The LIR type of a layout primitive (a pointer niche is a `ptr`).
fn prim_ty(prim: Prim) -> Ty {
    crate::intrinsics::lir(prim).0
}

/// An aggregate's walk row at its arguments: what `Layouts` says of it (size, alignment,
/// offsets, stride), with how each part is held (`Repr`) and whether it needs drop.
struct Layout {
    id: LayoutId,
    /// The aggregate's type, of the ground table.
    ty: PTy,
    size: u32,
    /// log2 of the alignment.
    align: u8,
    /// How many scalars a copy moves (saturating).
    leaves: u64,
    shape: Shape,
    /// It needs drop: it has a `Drop` impl, or a part that needs drop.
    needs_drop: bool,
    /// It is held in memory whatever its size: it needs drop, or a part of it is `placed` (a
    /// `ManuallyDrop<Guard>`, a union of one). Whether a value needs drop is a fact of its type,
    /// never of its size (the Reference, `[destructors.intro]`: "When an initialized variable
    /// or temporary goes out of scope, its destructor is run"), and a destructor takes the
    /// value's address: so a zero-sized type with drop glue is an aggregate of no bytes
    /// (`Repr::Agg`), which every path that carries an aggregate's drop carries too.
    placed: bool,
    /// Its `Drop` impl's fn, as the `Drop` trait's `drop` (a call at `[ty]` runs the impl's).
    drop_fn: Option<ItemKey>,
    /// A union: its `Fields` overlap at offset 0, and its glue drops none of them.
    is_union: bool,
}

/// A binding whose type needs drop: its ground type, its drop flag if it has one, and its
/// field flags if a `match` may move fields out of it (codegen.md, "A `match` by value on an
/// enum that needs drop"): a 64-bit word, bit `field_bit` set while that field is held.
#[derive(Clone, Copy)]
struct Owned {
    ty: PTy,
    flag: Option<SlotId>,
    rest: Option<SlotId>,
}

/// Ownership state of a binding in this body: dropping bindings live in `scope_pats`, while
/// non-dropping owned bindings (`!needs_drop`, e.g. `ManuallyDrop<T>`) never drop at scope end
/// yet own their place and may hold a field-flags word for `assign_over` when a dropping part
/// moves out (`partial`).
#[derive(Clone, Copy)]
enum LocalOwn {
    /// Not owned by this body (not yet bound, or a borrowed / non-`FnOnce` capture).
    None,
    /// Owned by this body and its type needs drop (lives in `scope_pats`).
    Drop(Owned),
    /// Owned by this body, its type needs no drop, no field flags needed.
    Kept,
    /// Owned by this body, its type needs no drop, with field-flags word `SlotId`.
    KeptFlags(SlotId),
}

impl LocalOwn {
    fn as_drop(self) -> Option<Owned> {
        match self {
            LocalOwn::Drop(o) => Some(o),
            LocalOwn::None | LocalOwn::Kept | LocalOwn::KeptFlags(_) => None,
        }
    }
}

/// An entry of a drop scope: a binding, or what an arm of a `match` by value on a temporary
/// left of it (`variant` the arm's, `moved` the `field_bit`s it moved out; `None`: nothing
/// moved, the whole value), dropped at the arm's end, after the arm's bindings.
#[derive(Clone, Copy)]
enum Held {
    Pat(PatId),
    Rest { at: (Value, i32), l: u32, variant: Option<u32>, moved: u64 },
    /// A temporary scrutinee of a match by tests that moves out of it (walk/moves.rs): of
    /// ground type `g` at `at`, what its word `mask` says is held, dropped at its statement's
    /// end like `Rest`.
    Moved { at: (Value, i32), g: PTy, mask: SlotId },
    /// An extended temporary (walk/temps.rs `hold_temp`): its `let`'s block's, dropped with its
    /// locals; made under a branch, `flag` is its drop flag and the slot holding its address.
    Temp { at: (Value, i32), l: u32, flag: Option<(SlotId, SlotId)> },
    /// A by-value field of the environment an `FnOnce` body or an `async` block owns, a place
    /// below its binding whose type needs drop (walk/moves/env_places.rs `bind_env_place`): the
    /// field's index, dropped with the body's outermost scope unless moved out.
    EnvPlace(u32),
}

/// Who owns a `match`'s scrutinee while its arms move out of it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// Nothing to own: no drop, or matched through a reference.
    None,
    /// A local, matched in place: its moves clear its field flags.
    Local(PatId),
    /// A temporary: each arm holds its rest (`Held::Rest`).
    Temp,
}

#[derive(Clone, Copy)]
enum Shape {
    /// A struct's or tuple's fields (a wide pointer's address and length), in
    /// `layout_fields[first..first + len]`.
    Fields { first: u32, len: u32 },
    /// `len` elements of `elem`, `stride` bytes apart.
    Array { elem: Repr, len: u32, stride: u32 },
    /// An enum (lib.rs, "Enums"): its layout `id` in `ls` (the tag, its encoding, the
    /// discriminants), its variants' fields in `layout_variants[first..first + len]`, each a
    /// range of `layout_fields` at their offsets in the enum.
    Enum { id: LayoutId, first: u32, len: u32 },
}

/// Where a binding lives: its own slot, or memory it was given (a caller's copy of an
/// aggregate argument, the temporary an aggregate expression was built in).
#[derive(Clone, Copy)]
enum Home {
    Slot(SlotId),
    At(Value, i32),
}

/// One step of an aggregate copy (`Walk::copy`).
#[derive(Clone, Copy)]
enum CopyStep {
    Copy { repr: Repr, from: (Value, i32), to: (Value, i32) },
    /// The end of a loop's body over an array: the next element, or out.
    Next { i: Value, header: Block, exit: Block },
    /// Start building into a block (an enum variant's copy, or the copy's join).
    Begin(Block),
    /// The end of an enum variant's copy: on to the join.
    Jump(Block),
    /// Write variant `variant`'s tag of enum layout `id` at `to` (after its fields).
    Tag { id: LayoutId, variant: u32, to: (Value, i32) },
}

/// What `break` and `continue` reach: a loop, or a block a `break` leaves.
#[derive(Clone, Copy)]
struct Target {
    expr: ExprId,
    /// The loop's head; `None` for a block.
    header: Option<Block>,
    exit: Block,
    repr: Repr,
    /// The drop scopes open outside it: leaving it drops every scope from this depth on.
    depth: u32,
}

/// A match being built: its arms' blocks are `arm_blocks[first..first + arms + 1]`, the last
/// the block no arm matched (unreachable: the checker proved the arms exhaustive).
#[derive(Clone, Copy)]
struct MatchState {
    expr: ExprId,
    /// The value switched on: the scrutinee, or an enum scrutinee's tag.
    scrutinee: Value,
    width: u32,
    /// An enum scrutinee: its place and its walk row, which its arms' variant patterns bind
    /// from, and its layout in `ls`, whose discriminants the arms' variants are.
    of_enum: Option<((Value, i32), u32, LayoutId)>,
    join: Block,
    repr: Repr,
    first: u32,
    arms: u32,
    /// Some arm has a guard: the arms are tested in turn rather than by one `switch`.
    guarded: bool,
    /// The scrutinee was a reference the arms' patterns matched through (the reference's
    /// "Binding modes": the default binding mode becomes `ref`): each binding takes its
    /// part's address, nothing is moved or dropped.
    by_ref: bool,
    /// By value on an enum that needs drop: who holds what the arms do not move.
    owner: Owner,
    /// A match by tests (walk/matches.rs): the scrutinee's place each arm tests and binds from.
    tested: Option<matches::At>,
}

#[derive(Clone, Copy)]
enum Task {
    /// Evaluate, then apply the expression's adjustments.
    Eval(ExprId),
    /// Evaluate without the adjustments.
    Raw(ExprId),
    /// Apply `e`'s value adjustments from its `n`th on (a coercion: an unsizing, a reified
    /// fn, `!` to any) to the value on top.
    Adjust(ExprId, u32),
    /// Step `n` of `e`'s adjustments, an overloaded dereference: the pointer on top (to the
    /// `Self` of `Deref`) is replaced by what `Deref::deref` (or `deref_mut`) returns.
    DerefCall(ExprId, u32),
    /// Load the value of the type after `e`'s adjustment `n` from the place on top.
    LoadAdjusted(ExprId, u32),
    /// Keep `e`'s adjusted value on top in a temporary of its own, as a place.
    SpillAdjusted(ExprId),
    /// Evaluate as a place.
    Place(ExprId),
    /// Evaluate as a place, after `n` builtin dereferences of its value and no other
    /// adjustment (the place an autoref borrows).
    PlaceDerefs(ExprId, u32),
    /// Build intrinsic call `e` in place, its arguments on top.
    Intrinsic(ExprId, Intrinsic),
    Stmt(StmtId),
    Discard,
    /// Discard the value on top, of expression `e`: dropped when its type needs drop (the end
    /// of an expression statement, `let _ = <value>`).
    DropValue(ExprId),
    /// The end of block `e`'s drop scope: its bindings dropped under its value.
    ScopeEnd(ExprId),
    /// The end of a statement: its borrowed temporaries dropped (walk/temps.rs).
    TempEnd,
    /// The end of temporary scope `e` (an `if` condition, a lazy operand, an arm's body...):
    /// its temporaries dropped under its value (walk/temps.rs `temp_close`).
    TempScopeEnd(ExprId),
    PushNone,
    /// Bind the value on top to the pattern; the expression it is of, for an aggregate built
    /// in a temporary of its own, which becomes the binding's home.
    Let(PatId, Option<ExprId>),
    BlockExit(ExprId),
    If { expr: ExprId, then: ExprId, else_: Option<ExprId> },
    IfThen { expr: ExprId, else_: Option<ExprId>, else_block: Block, join: Block },
    IfElse { expr: ExprId, join: Block },
    /// A `let` link of an `if` condition (walk/lets.rs): `let` expression `expr`, its
    /// scrutinee on top; a failed test to `fail`.
    LetLink { expr: ExprId, fail: Block },
    /// A `bool` link of an `if` condition holding a `let`, its value on top; false to the
    /// block given.
    CondLink(Block),
    /// `let`-`else` statement (walk/lets.rs), its scrutinee on top.
    LetElse(StmtId),
    /// The end of a `let`-`else`'s diverging block: the statement goes on in the block given,
    /// its entries set aside for the `else` block (`let_else_held` from the index) put back;
    /// the `else` block last.
    LetElseEnd(Block, u32, ExprId),
    LoopEnd(ExprId),
    Break(ExprId, bool),
    Return(bool),
    Match(ExprId),
    Arm { m: u32, i: u32 },
    Guard { m: u32, i: u32 },
    /// The guard of an arm whose pattern holds or-patterns (walk/matches/alts.rs): a false
    /// guard goes on at the alternative after the one that matched, numbered in `word`.
    GuardAlts { word: SlotId, chains: u32, n: u32 },
    /// A guarded arm's guard passed: the moves its bindings held from `from` on are made
    /// (walk/moves.rs `Pending`).
    ArmMoves { from: u32 },
    ArmEnd { m: u32 },
    MatchEnd,
    Call(ExprId),
    /// The call of an operator the check recorded a callee for, its operands on the value
    /// stack (walk/operators.rs).
    OpCall(ExprId),
    Unary(ExprId, UnOp),
    Deref(ExprId),
    Binary(ExprId, BinOp),
    Short { expr: ExprId, op: BinOp, rhs: ExprId },
    ShortEnd(Block),
    Assign(ExprId),
    AssignOp(ExprId, BinOp),
    Cast(ExprId),
    /// Store the value on top into the aggregate place under it, `off` bytes in.
    InitAt { off: u32, repr: Repr },
    /// Every part of the aggregate literal on top (laid out as row `l`) is stored: it is one
    /// value from now on, owned whole (walk/val_stack.rs `literal_built`).
    LitBuilt { l: u32 },
    /// Base-less struct expression `e` (`S { a: 1, .. }`, RFC 3681), its place on top, every
    /// written field stored: each unwritten field stored from its default's value.
    InitDefaults(ExprId),
    /// Struct update `e`'s base place on top, its new value's place under it: the fields it
    /// does not name taken from the base (walk/moves/struct_update.rs).
    StructBase(ExprId),
    Field(ExprId),
    FieldPlace(ExprId),
    /// The place under the index on top: the element, after the bounds check.
    IndexPlace(ExprId),
    /// Call `Index::index` for overloaded index `e` with `&base` and the index on top
    /// (walk/index.rs); leaves the reference it returns.
    IndexCall(ExprId),
    /// Load the value of expression `e` from the place on top.
    LoadPlace(ExprId),
    DerefPlace,
    Spill(ExprId),
    /// Copy the aggregate argument on top into the caller's copy for the call.
    ArgCopy(ExprId),
    /// Fill the array literal `[x; N]` from the value on top.
    Repeat(ExprId),
    Ref,
    /// Build `format_args!` `e` from its arguments' references on top (walk/fmt.rs).
    FormatArgs(ExprId),
    /// The poll loop of `.await` `e`, its operand's value on top (coroutine.rs `await_poll`).
    Await(ExprId),
    /// Build `asm!` `e` from its operands' values and places on top (walk/asm.rs `asm_done`).
    Asm(ExprId),
    /// An `asm!`'s `label` block `block`: its body `body`, then a jump to `join`.
    AsmLabel { block: Block, body: ExprId, join: Block },
    /// The end of an `asm!`'s label block: its value dropped, a jump to the join.
    AsmLabelEnd(Block),
    /// The code after an `asm!` with `label` operands, at its join.
    AsmJoin(Block),
}

struct Walk<'x, 'c, 't, A: Allocator + Copy> {
    body: &'x BodyView<'x>,
    checked: &'x Checked<'c>,
    /// The record's types: generic, read only to be grounded (and for the shape of a node,
    /// which the check fixed for every instance).
    types: PTys<'x>,
    /// The instance's arguments, types of `ground`.
    args: &'x [PTy],
    /// The program's ground table: every type the walk lays out is one of it.
    ground: &'x mut PortableTys<'t>,
    scratch: Alloc<'x>,
    /// Per record type: its ground type, or `UNGROUNDED`.
    grounded: Vec<PTy, A>,
    adts: &'x dyn Adts,
    alloc: A,
    b: Builder<A, Full>,
    refs: Vec<Instance, A>,
    /// By `Static` reference place: the static item.
    statics: Vec<ItemKey, A>,
    /// By `Tls` reference place: the thread-local static item.
    thread_locals: Vec<ItemKey, A>,
    /// By `Vtable` reference place: the table.
    vtables: Vec<crate::vtable::VtableRef, A>,
    /// Each `unreachable` written to end a construct ([`Walk::unreachable_of`]), in order.
    trap_marks: Vec<TrapMark, A>,
    /// The LIR instructions [`Walk::copy`] wrote (`Built::copy_insts`).
    copy_insts: u64,
    /// The expression of the task being stepped (`walk_root`), the body's root after the last:
    /// the site of an `unreachable` a step writes with no expression of its own (an enum
    /// copy's no-variant arm, `copy`), so a trap row names where in the text the copy is.
    task_at: Option<ExprId>,
    /// The one layout of every type (rule 30): the walk's rows read their sizes, alignments,
    /// offsets, strides, tags and niches from it.
    ls: Layouts<A>,
    layouts: Vec<Layout, A>,
    layout_fields: Vec<(Name, u32, Repr), A>,
    /// Row for row with `layout_fields`: each field's ground type at the aggregate's arguments
    /// (walk/matches.rs follows a pattern into a field by it).
    layout_field_tys: Vec<PTy, A>,
    /// Per enum variant laid out: its fields' range of `layout_fields` (first, len).
    layout_variants: Vec<(u32, u32), A>,
    /// The rows of `layouts` that are a struct or tuple whose last field is unsized (a slice
    /// or `str` tail: walk/unsized_tail.rs); never held by value.
    unsized_tails: Vec<u32, A>,
    /// Per pattern: the binding's home and how its value is held.
    slots: Vec<Option<(Home, Repr)>, A>,
    /// Per block made: whether a reachable edge leads to it.
    reached: Vec<bool, A>,
    /// Whether the code being built is reachable.
    live: bool,
    /// Per expression: whether it is a block some `break` leaves.
    break_target: Vec<bool, A>,
    work: Vec<Task, A>,
    vals: Vec<V, A>,
    /// Beside each of `vals`, pushed and popped with it (walk/val_stack.rs): what the walk owns
    /// of it that needs drop.
    val_tys: Vec<val_stack::Owns, A>,
    /// The parts stored so far that need drop of the aggregate literals being built, each
    /// `(offset in the literal, ground type)`, one LIFO list for all (walk/val_stack.rs).
    lit_parts: Vec<(u32, PTy), A>,
    targets: Vec<Target, A>,
    matches: Vec<MatchState, A>,
    arm_blocks: Vec<Block, A>,
    ret: Repr,
    /// How the result leaves: in registers, or through `sret`.
    ret_pass: Pass,
    /// The hidden result pointer of a fn returning an aggregate.
    sret: Option<Value>,
    /// Per pattern: the binding's state changes under a branch or loop of its scope, so it
    /// gets a drop flag if its type needs drop (`scan_flags`).
    needs_flag: Vec<bool, A>,
    /// Per pattern: a `match` on the binding may move fields out of it, so it gets field
    /// flags if its type needs drop (`scan_flags`).
    partial: Vec<bool, A>,
    /// Per expression: its parent (`NO_EXPR` for none) and whether the edge to it is
    /// conditional, from `scan_flags` (walk/temps.rs finds a temporary's scope by it).
    expr_parent: Vec<(u32, bool), A>,
    /// Per statement being built: where its temporaries start in `scope_pats`.
    temp_marks: Vec<u32, A>,
    /// A `let`-`else` statement's own entries of `scope_pats` (its temporaries, its bindings),
    /// set aside while its `else` block is built (walk/lets.rs `let_else_test`), innermost last.
    let_else_held: Vec<Held, A>,
    /// Per expression: whether it is a temporary scope of its own, and entered
    /// (walk/temps.rs `scan_temp_scopes`).
    temp_here: Vec<u8, A>,
    /// Per open temporary scope, beside `temp_marks`: the expression it is of and how many
    /// drop scopes were open when it began (walk/temps.rs).
    temp_scopes: Vec<(u32, u32), A>,
    /// Per extending borrow under a branch, of a `let` being built: its temporary's drop flag
    /// (walk/temps.rs `ext_flags_for`).
    ext_flags: Vec<(u32, SlotId), A>,
    /// `super let` bindings owned by a temporary scope (not extended): `temp_end` drops them
    /// (walk/super_let.rs).
    super_temps: Vec<PatId, A>,
    /// Per pattern: ownership state of the binding (`Drop` when its type needs drop, `Kept*`
    /// when owned by this body without needing drop).
    owned: Vec<LocalOwn, A>,
    /// Per pattern: whether a binding without a flag is initialized here.
    init: Vec<bool, A>,
    /// What needs drop, in declaration order, of every open drop scope.
    scope_pats: Vec<Held, A>,
    /// Per open drop scope (a fn's parameters first, then each block): where its bindings
    /// start in `scope_pats`.
    scope_marks: Vec<u32, A>,
    /// The parts matches by tests moved out of their scrutinees (walk/moves.rs).
    moves: moves::Moves<A>,
    /// The fields of the environment of the closure being built when one holds a captured
    /// place below its binding (walk/moves/env_places.rs).
    env_places: moves::env_places::EnvPlaces<A>,
    /// Each whole binding the closure or coroutine being built captures, bound at its
    /// capture's home ([`Walk::bind_capture_at`]): a binding of an enclosing body, which this
    /// body owns only when `own` says so (a by-value capture of an `FnOnce` body).
    captured: Vec<PatId, A>,
    /// `Some` while a coroutine's body is built (coroutine.rs `run_coroutine`).
    co: Option<coroutine::Co>,
    /// `Some` while a coroutine's body is built into a state laid from types: the flow's table
    /// of identities and what the walk named (walk/state_names.rs; ASYNC.md P8).
    names: Option<state_names::Names<'x, A>>,
    /// While a landing block's drops are built (walk/unwind.rs): their calls are plain.
    cleanup: bool,
    /// Whether the body may not unwind (walk/nounwind.rs `mark_nounwind`).
    nounwind: bool,
    /// The body's one `terminate` landing block, once a call needed it (walk/nounwind.rs).
    terminate: Option<Block>,
    /// The drops a ladder's landing built now owes after the current step, performed last
    /// pushed first (walk/unwind_ladder.rs).
    owed: Vec<unwind_ladder::DropStep, A>,
    /// Where a drop call made now goes (walk/unwind_ladder.rs `drop_call`).
    rung: unwind_ladder::Rung,
    /// What the body was given of its caller's place (walk/track_caller.rs).
    track: track_caller::Track,
}

fn unsupported(expr: ExprId, what: &'static str) -> BuildError {
    BuildError::Unsupported { expr, what }
}

fn malformed(what: &'static str) -> BuildError {
    BuildError::Malformed { what }
}

/// Unwinding permission is separate from the machine signature of a C-family pointer.
fn pointer_abi(abi: blaze_typeck::Abi) -> Option<Abi> {
    match abi {
        blaze_typeck::Abi::Rust => Some(Abi::Rust),
        blaze_typeck::Abi::C | blaze_typeck::Abi::CUnwind => Some(Abi::C),
        blaze_typeck::Abi::RustCall => Some(Abi::RustCall),
        _ => None,
    }
}

fn int_ty(i: IntTy) -> Result<Ty, TyErr> {
    Ok(match i {
        IntTy::I8 => Ty::I8,
        IntTy::I16 => Ty::I16,
        IntTy::I32 => Ty::I32,
        IntTy::I64 | IntTy::Isize => Ty::I64,
        IntTy::I128 => Ty::I128,
    })
}

fn uint_ty(u: UintTy) -> Result<Ty, TyErr> {
    Ok(match u {
        UintTy::U8 => Ty::I8,
        UintTy::U16 => Ty::I16,
        UintTy::U32 => Ty::I32,
        UintTy::U64 | UintTy::Usize => Ty::I64,
        UintTy::U128 => Ty::I128,
    })
}

fn align_up(at: u32, align: u8) -> Option<u32> {
    let a = 1u32 << align;
    at.checked_add(a - 1).map(|it| it & !(a - 1))
}

fn mask(value: u128, bits: u32) -> u128 {
    if bits >= 128 { value } else { value & ((1u128 << bits) - 1) }
}

/// The integer types of 1, 2, 4 and 8 bytes, by log2 of the size.
const WIDTHS: [Ty; 4] = [Ty::I8, Ty::I16, Ty::I32, Ty::I64];


/// What a call calls.
#[derive(Clone, Copy)]
enum Callee {
    Direct(RefId),
    /// A fn pointer's value.
    Pointer(Value),
}

/// What a build makes of a fn.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Its written body.
    Body,
    /// The drop glue of its one type argument (`drop_in_place`'s instance).
    DropGlue,
    /// The instance of trait method `FnTrait` at the closure at this expression (closure.rs):
    /// its body when the trait is the closure's kind, else a thin entry over the body; the
    /// body view's root and parameters are the closure's.
    Closure(ExprId, crate::FnTrait),
}

pub(crate) fn build<A: Allocator + Copy>(
    body: &BodyView<'_>,
    checked: &Checked<'_>,
    sig: &FnSig<'_>,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    kind: Kind,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    let mut w = Walk::new(body, checked, subst, adts, alloc);
    match kind {
        Kind::Body => {
            w.scan_flags()?;
            w.run(sig, kind)?;
        }
        Kind::DropGlue => w.run(sig, kind)?,
        Kind::Closure(closure, which) => {
            w.scan_flags()?;
            w.run_closure(closure, which)?;
        }
    }
    w.into_built()
}

/// One `unreachable` a walk wrote to end a construct: its instruction, kind, site and subject
/// (the enum an enum copy's switch read the discriminant of).
type TrapMark = (u32, TrapKind, Option<(u128, u32)>, Option<Subject>);

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// An `unreachable` ending the construct of `kind` at expression `at`, and its row of the
    /// trap table: its site is `at`'s first byte from its item's (`Adts::caller_site`, for one
    /// in a macro's expansion its outermost call's; rule 71), `None` when the world does not
    /// say or there is no expression.
    pub(crate) fn unreachable_of(&mut self, kind: TrapKind, at: Option<ExprId>) {
        let inst = self.b.unreachable_inst();
        self.mark_trap(inst.0, kind, at, None);
    }

    /// [`Walk::unreachable_of`] for the no-variant arm of an enum copy's switch on the
    /// discriminant of enum layout `id`: its row also names the enum and its discriminants as
    /// the switch compares them ([`Walk::subject_of`]).
    fn unreachable_of_enum(&mut self, id: LayoutId, len: u32, at: Option<ExprId>) -> Result<(), BuildError> {
        let subject = self.subject_of(id, len)?;
        let inst = self.b.unreachable_inst();
        self.mark_trap(inst.0, TrapKind::Discriminant, at, subject);
        Ok(())
    }

    /// The enum of layout `id` as a trap row names it: its item and the first discriminants of
    /// the `len` variants the copy's switch compares (`discriminant_case`, the same values).
    /// `None` when its type is no ADT (an enum layout of another type has no item to name).
    fn subject_of(&self, id: LayoutId, len: u32) -> Result<Option<Subject>, BuildError> {
        let Ok(PNode::Adt(item, _)) = self.ground_node(self.ls.get(id).ty) else { return Ok(None) };
        let mut discriminants = [0u64; SUBJECT_SHOWN];
        for (v, d) in discriminants.iter_mut().enumerate().take(len as usize) {
            *d = self.discriminant_case(id, v as u32)? as u64;
        }
        Ok(Some(Subject { item: item.0, count: len, discriminants }))
    }

    /// [`Walk::unreachable_of`] for the `unreachable` just written by another lowering (the
    /// intrinsic's, intrinsics.rs): the function's last instruction must be it.
    pub(crate) fn mark_last_unreachable(&mut self, kind: TrapKind, at: Option<ExprId>) -> Result<(), BuildError> {
        let ops = &self.b.func().ops;
        let last = ops.len().checked_sub(1).filter(|&i| ops[i] == Op::Unreachable as u8);
        let inst = last.ok_or(malformed("a diverging lowering that did not end in `unreachable` (a build bug)"))?;
        self.mark_trap(inst as u32, kind, at, None);
        Ok(())
    }

    fn mark_trap(&mut self, inst: u32, kind: TrapKind, at: Option<ExprId>, subject: Option<Subject>) {
        let site = at.and_then(|e| self.adts.expr_site(self.body.owner, e)).map(|offset| (self.body.owner.0, offset));
        self.trap_marks.push((inst, kind, site, subject));
    }

    /// The walk's function, finished and verified, with its tables: every walk ends here.
    pub(crate) fn into_built(self) -> Result<Built<A>, BuildError> {
        let Walk { b, refs, statics, thread_locals, vtables, trap_marks, alloc, copy_insts, .. } = self;
        let func = b.finish().map_err(|error| match error {
            blaze_lir::BuildError::Verify(v) => BuildError::Verify(v),
            other => BuildError::Lir(other),
        })?;
        let traps = trap_sources(&func, &trap_marks, alloc)?;
        // The one verification of the walk's function: its proof travels in `Built` to the
        // emitter, which takes it and checks nothing again (D163).
        let func = blaze_lir::VerifiedFunc::new(func).map_err(BuildError::Verify)?;
        Ok(Built { func, refs, statics, thread_locals, vtables, traps, copy_insts })
    }
}

/// The walk's marks as the trap table: each `unreachable`'s ordinal among the function's
/// `unreachable` instructions in layout order (what the emitter counts them by).
fn trap_sources<A: Allocator + Copy>(func: &blaze_lir::Func<A>, marks: &[TrapMark], alloc: A) -> Result<Vec<TrapSource, A>, BuildError> {
    let unreachable = Op::Unreachable as u8;
    let mut out = Vec::with_capacity_in(marks.len(), alloc);
    let (mut ordinal, mut next) = (0u32, 0usize);
    for &(inst, kind, site, subject) in marks {
        let at = inst as usize;
        if at < next || func.ops.get(at) != Some(&unreachable) {
            return Err(BuildError::Malformed { what: "a trap mark that is not the function's next `unreachable`" });
        }
        ordinal += func.ops[next..at].iter().filter(|&&op| op == unreachable).count() as u32;
        out.push(TrapSource { ordinal, kind, site, subject });
        ordinal += 1;
        next = at + 1;
    }
    Ok(out)
}

/// An unsizing adjustment's types: the pointer before and after, its pointee before and after.
#[derive(Clone, Copy)]
struct UnsizeEnds {
    from: PTy,
    to: PTy,
}

/// What the instance `subst` names of `body` reads of trait shapes, in expression order: the
/// part of its record's key the world's trait facts give ([`crate::vtable::record_key`]). Walks
/// the checked body's calls and unsizings without building code.
pub(crate) fn object_reads<A: Allocator + Copy>(
    body: &BodyView<'_>,
    checked: &Checked<'_>,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Vec<ObjectRead, A>, BuildError> {
    let mut out = Vec::new_in(alloc);
    let mut w = Walk::new(body, checked, subst, adts, alloc);
    w.object_reads(&mut out)?;
    Ok(out)
}

impl<'x, 'c, 't, A: Allocator + Copy> Walk<'x, 'c, 't, A> {
    fn new(body: &'x BodyView<'x>, checked: &'x Checked<'c>, subst: &'x mut Subst<'_, 't>, adts: &'x dyn Adts, alloc: A) -> Self {
    let mut grounded = Vec::with_capacity_in(checked.types.len(), alloc);
    grounded.resize(checked.types.len(), UNGROUNDED);
    let mut break_target = Vec::with_capacity_in(body.exprs.len(), alloc);
    break_target.resize(body.exprs.len(), false);
    for e in body.exprs {
        if let Expr::Break { target, .. } = *e
            && let Some(Expr::Block { .. }) = body.exprs.get(target.ix())
        {
            break_target[target.ix()] = true;
        }
    }
    let mut slots = Vec::with_capacity_in(body.pats.len(), alloc);
    slots.resize(body.pats.len(), None);
    let mut needs_flag = Vec::with_capacity_in(body.pats.len(), alloc);
    needs_flag.resize(body.pats.len(), false);
    let mut partial = Vec::with_capacity_in(body.pats.len(), alloc);
    partial.resize(body.pats.len(), false);
    let mut owned = Vec::with_capacity_in(body.pats.len(), alloc);
    owned.resize(body.pats.len(), LocalOwn::None);
    let mut init = Vec::with_capacity_in(body.pats.len(), alloc);
    init.resize(body.pats.len(), false);
    Walk {
        body,
        checked,
        types: checked.types.view(),
        args: subst.args,
        ground: &mut *subst.ground,
        scratch: subst.scratch,
        grounded,
        adts,
        alloc,
        b: Builder::new_in(PTR_BITS, alloc),
        refs: Vec::new_in(alloc),
        statics: Vec::new_in(alloc),
        thread_locals: Vec::new_in(alloc),
        vtables: Vec::new_in(alloc),
        trap_marks: Vec::new_in(alloc),
        copy_insts: 0,
        task_at: None,
        ls: Layouts::new_in(alloc),
        layouts: Vec::new_in(alloc),
        layout_fields: Vec::new_in(alloc),
        layout_field_tys: Vec::new_in(alloc),
        layout_variants: Vec::new_in(alloc),
        unsized_tails: Vec::new_in(alloc),
        slots,
        reached: Vec::new_in(alloc),
        live: true,
        break_target,
        work: Vec::new_in(alloc),
        vals: Vec::new_in(alloc),
        val_tys: Vec::new_in(alloc),
        lit_parts: Vec::new_in(alloc),
        targets: Vec::new_in(alloc),
        matches: Vec::new_in(alloc),
        arm_blocks: Vec::new_in(alloc),
        ret: Repr::None,
        ret_pass: Pass::Ignore,
        sret: None,
        needs_flag,
        partial,
        expr_parent: Vec::new_in(alloc),
        temp_marks: Vec::new_in(alloc),
        let_else_held: Vec::new_in(alloc),
        temp_here: {
            let mut here = Vec::with_capacity_in(body.exprs.len(), alloc);
            here.resize(body.exprs.len(), temps::NOT_SCOPE);
            here
        },
        temp_scopes: Vec::new_in(alloc),
        ext_flags: Vec::new_in(alloc),
        super_temps: Vec::new_in(alloc),
        owned,
        init,
        scope_pats: Vec::new_in(alloc),
        scope_marks: Vec::new_in(alloc),
        moves: moves::Moves::new_in(alloc),
        env_places: moves::env_places::EnvPlaces::new_in(alloc),
        captured: Vec::new_in(alloc),
        co: None,
        names: None,
        cleanup: false,
        nounwind: false,
        terminate: None,
        owed: Vec::new_in(alloc),
        rung: unwind_ladder::Rung::Off,
        track: track_caller::Track::No,
    }
    }

    /// Pushes onto `out` each [`ObjectRead`] of the body, in expression order: a dynamic call's
    /// (principal, method, slot), then the expression's upcasts off the first supertrait chain
    /// (or along it). The same facts `call` and `unsize` read, asked before the build.
    fn object_reads(&mut self, out: &mut Vec<ObjectRead, A>) -> Result<(), BuildError> {
        for i in 0..self.body.exprs.len() {
            let e = ExprId(u32::try_from(i).map_err(|_| malformed("an expression index past u32"))?);
            let instance = match self.expr(e)? {
                Expr::Call { callee, .. } => {
                    let t = self.adjusted_ty(callee)?;
                    match self.node_at(callee, t)? {
                        PNode::FnDef(item, args) => Some(Instance { item, args }),
                        _ => None,
                    }
                }
                Expr::MethodCall { .. } => Some(self.method_callee(e)?),
                _ => None,
            };
            if let Some(instance) = instance
                && let Some((principal, slot)) = self.dyn_slot(e, instance)?
            {
                out.push(ObjectRead::Call { principal, method: instance.item, slot });
            }
            let rows = self.adjust_rows(e);
            for n in 0..rows.len() {
                if !matches!(self.checked.adjusts.column::<1>()[rows.start + n], AdjustKind::Unsize) {
                    continue;
                }
                let UnsizeEnds { from, to, .. } = self.unsize_ends(e, n)?;
                let node = |w: &Self, t: PTy| w.ground_node(t).map_err(|error| Self::ty_err(e, error));
                if !matches!((node(self, from)?, node(self, to)?), (PNode::Dynamic(_), PNode::Dynamic(_))) {
                    continue;
                }
                let (Some((a, _)), Some((b, _))) = (self.principal(e, from)?, self.principal(e, to)?) else {
                    continue;
                };
                if a == b {
                    continue;
                }
                let shape = crate::vtable::Shape::of(a, |t| self.adts.object_trait(t), self.scratch)
                    .map_err(|error| BuildError::Vtable { at: Some(e), error })?;
                let slot = match shape.upcast(b) {
                    Some(crate::vtable::Upcast::Prefix) => None,
                    Some(crate::vtable::Upcast::Load(slot)) => Some(slot),
                    None => return Err(malformed("an upcast to a trait that is not a supertrait")),
                };
                out.push(ObjectRead::Upcast { from: a, to: b, slot });
            }
        }
        Ok(())
    }
}

impl<'x, 'c, 't, A: Allocator + Copy> Walk<'x, 'c, 't, A> {
    // Types.

    /// `t` of table `from` (a signature's, a struct's), grounded at the instance's arguments.
    fn ground_in(&mut self, from: PTys<'_>, t: PTy) -> Result<PTy, TyErr> {
        crate::instance::ground(from, t, self.args, self.ground, self.scratch, self.adts.facts()).map_err(ground_err)
    }

    /// Type `t` of the record, grounded at the instance's arguments, once per type.
    fn ground_ty(&mut self, t: PTy) -> Result<PTy, TyErr> {
        let at = t.0 as usize;
        let found = *self.grounded.get(at).ok_or(TyErr::Malformed("a type past its table"))?;
        if found != UNGROUNDED {
            return Ok(found);
        }
        let types = self.types;
        // An `async` block of this body, still named by its expression: its identity here
        // (coroutine.rs `own_block`), at the top or inside another type (a callee's `Fut`
        // argument, a closure returning the block); every other node as `ground` maps it.
        let g = match coroutine::own_block(types, t) {
            Some(block) => self.own_coroutine_ty(block)?,
            None => crate::instance::ground_own(types, t, self.args, self.body, self.ground, self.scratch, self.adts.facts()).map_err(ground_err)?,
        };
        self.grounded[at] = g;
        Ok(g)
    }

    /// The node of ground type `g`.
    fn ground_node(&self, g: PTy) -> Result<PNode, TyErr> {
        self.ground.view().node(g).map_err(|_| TyErr::Malformed("a type past the ground table"))
    }

    /// The value of ground array length `n` (a `ConstValue` once grounded).
    fn ground_len(&self, n: PTy) -> Result<u64, TyErr> {
        self.ground.view().value_u64(n).ok_or(TyErr::Malformed("an array length that is not a value"))
    }

    /// The ground node of record type `t`, met at `e`.
    fn node_at(&mut self, e: ExprId, t: PTy) -> Result<PNode, BuildError> {
        let g = self.ground_ty(t).map_err(|error| Self::ty_err(e, error))?;
        self.ground_node(g).map_err(|error| Self::ty_err(e, error))
    }

    fn find_layout(&self, ty: PTy) -> Option<u32> {
        self.layouts.iter().position(|l| l.ty == ty).map(|it| it as u32)
    }

    fn pattern_base(&self, mut g: PTy) -> Result<PTy, TyErr> {
        for _ in 0..=self.ground.view().len() {
            match self.ground_node(g)? {
                PNode::Pat(base, _) => g = base,
                _ => return Ok(g),
            }
        }
        Err(TyErr::Malformed("a cycle in the base types of a pattern type"))
    }

    /// How ground type `g` is held.
    fn repr(&mut self, mut g: PTy) -> Result<Repr, TyErr> {
        let node = loop {
            g = self.pattern_base(g)?;
            match self.ground_node(g)? {
                PNode::Union(l) if l.len == 1 => {
                    let only = self
                        .ground
                        .view()
                        .list(l)
                        .ok()
                        .and_then(|xs| xs.first().copied())
                        .ok_or(TyErr::Malformed("a union list past the ground table"))?;
                    if only.0 >= g.0 {
                        return Err(TyErr::Malformed("a cycle in a single-member union"));
                    }
                    g = only;
                }
                other => break other,
            }
        };
        Ok(match node {
            PNode::Pat(..) => return Err(TyErr::Malformed("a pattern type still wrapped after finding its base")),
            PNode::Bool => Repr::Bool,
            PNode::Int(i) => Repr::Int { ty: int_ty(i)?, signed: true },
            PNode::Uint(u) => Repr::Int { ty: uint_ty(u)?, signed: false },
            PNode::Never | PNode::FnDef(..) => Repr::None,
            PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) if l.len == 0 => Repr::None,
            PNode::Future(..) => {
                return Err(TyErr::Unsupported(
                    "an async block's future named by its expression in a ground type (only its identity, `PNode::Coroutine`, crosses a body: coroutine.rs)",
                ));
            }
            // A closure: a struct of one field, its environment (closure.rs). A coroutine: its
            // state, whose first bytes are its upvars (coroutine.rs).
            PNode::Adt(..) | PNode::Tuple(_) | PNode::Record(..) | PNode::Union(_) | PNode::Array(..) | PNode::Closure { .. } | PNode::Coroutine { .. } => {
                let l = self.layout(g)?;
                // An unsized tail's struct is only ever behind a pointer (walk/unsized_tail.rs).
                if self.has_unsized_tail(l) {
                    return Err(TyErr::Unsupported(unsized_tail::UNSIZED_BY_VALUE));
                }
                // No bytes and no destructor to hand an address to: no value. A zero-sized
                // type that is `placed` (it, or a part of it, has drop glue) stays an aggregate.
                let row = &self.layouts[l as usize];
                if row.size == 0 && !row.placed { Repr::None } else { Repr::Agg(l) }
            }
            // A wide pointer (lib.rs, "Wide pointers") when its pointee is unsized, by `ls`.
            PNode::Ref(..) | PNode::Ptr(..) => {
                if self.is_agg(g)? {
                    Repr::Agg(self.layout(g)?)
                } else {
                    Repr::Ptr
                }
            }
            // A fn pointer is the code's address (lib.rs, "Fn pointers").
            PNode::FnPtr(..) => Repr::Ptr,
            // strs.rs: "`char` ... has the same size and alignment as `u32`" (the Reference).
            PNode::Char => Repr::Int { ty: Ty::I32, signed: false },
            PNode::Float(f) => numbers::float_repr(f)?,
            PNode::Str => return Err(TyErr::Unsupported("str")),
            PNode::Slice(_) => return Err(TyErr::Unsupported("a slice")),
            PNode::Dynamic(_) => return Err(TyErr::Unsupported("a trait object (`dyn`)")),
            // Unsized with no metadata: only ever behind a (thin) pointer.
            PNode::Foreign(_) => return Err(TyErr::Malformed("an extern type held by value: it has no size")),
            PNode::ExTrait(..) | PNode::ExProj { .. } | PNode::ExAuto(_) => {
                return Err(TyErr::Malformed("a trait object's predicate row as a type"));
            }
            PNode::Param(_) | PNode::Alias { .. } | PNode::Opaque(..) | PNode::Infer | PNode::Error => {
                return Err(TyErr::Malformed("a type the ground table does not hold"));
            }
            PNode::ConstValue { .. } | PNode::ConstParam(_) => {
                return Err(TyErr::Malformed("a const generic argument where a value's type is"));
            }
        })
    }

    /// Size and log2 alignment.
    fn size_align(&self, repr: Repr) -> (u32, u8) {
        match repr {
            Repr::None => (0, 0),
            Repr::Bool => (1, 0),
            Repr::Ptr => (PTR_BITS / 8, 3),
            // "Type layout": each primitive's alignment is its size on this target (`u128` and
            // `i128` 16 since the 1.77 toolchains, `f64` 8).
            Repr::Int { ty, .. } | Repr::Float(ty) => {
                let bytes = ty.bits(PTR_BITS) / 8;
                (bytes, bytes.trailing_zeros() as u8)
            }
            Repr::Agg(l) => {
                let l = &self.layouts[l as usize];
                (l.size, l.align)
            }
        }
    }

    /// How many scalars a copy of `repr` moves.
    fn leaves(&self, repr: Repr) -> u64 {
        match repr {
            Repr::None => 0,
            Repr::Agg(l) => self.layouts[l as usize].leaves,
            Repr::Int { .. } | Repr::Float(_) | Repr::Bool | Repr::Ptr => 1,
        }
    }

    /// Ground type `g`'s layout in `ls`, the one layout (rule 30).
    fn ls_of(&mut self, g: PTy) -> Result<LayoutId, TyErr> {
        let adts = self.adts;
        let mut cx = LayoutCx { ground: &mut *self.ground, facts: adts.facts(), scratch: self.scratch };
        self.ls.of(g, &mut cx).map_err(layout_err)
    }

    /// Whether ground type `g` is laid out as an aggregate: an ADT, an array, a non-empty
    /// tuple, or a wide pointer (a pointer `ls` lays out as an address and a length).
    fn is_agg(&mut self, mut g: PTy) -> Result<bool, TyErr> {
        let node = loop {
            match self.ground_node(g)? {
                PNode::Union(l) if l.len == 1 => {
                    let only = self
                        .ground
                        .view()
                        .list(l)
                        .ok()
                        .and_then(|xs| xs.first().copied())
                        .ok_or(TyErr::Malformed("a union list past the ground table"))?;
                    if only.0 >= g.0 {
                        return Err(TyErr::Malformed("a cycle in a single-member union"));
                    }
                    g = only;
                }
                other => break other,
            }
        };
        Ok(match node {
            // A coroutine's state holds its tag at least (coroutine.rs).
            PNode::Adt(..) | PNode::Array(..) | PNode::Coroutine { .. } => true,
            PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) => l.len > 0,
            // Its environment is a tuple: an aggregate when it captures something.
            PNode::Closure { .. } => {
                let env = self.closure_parts(g)?.env;
                match self.ground_node(env)? {
                    PNode::Tuple(l) => l.len > 0,
                    _ => return Err(TyErr::Malformed("a closure's environment that is not a tuple")),
                }
            }
            PNode::Ref(..) | PNode::Ptr(..) => {
                let id = self.ls_of(g)?;
                self.ls.get(id).pair.is_some()
            }
            _ => false,
        })
    }

    /// The layout of an enum variant `variant` of enum layout `l`: its fields' range of
    /// `layout_fields`, and the enum's layout in `ls` (whose `tag_write` makes the variant). A
    /// struct or tuple is its own variant 0, with no tag.
    fn variant_rows(&self, l: u32, variant: u32) -> Result<(u32, u32, Option<LayoutId>), BuildError> {
        match self.layouts[l as usize].shape {
            Shape::Fields { first, len } if variant == 0 => Ok((first, len, None)),
            Shape::Enum { id, first, len } if variant < len => {
                let (f, n) = self.layout_variants[(first + variant) as usize];
                Ok((f, n, Some(id)))
            }
            _ => Err(malformed("a variant past its type's variants")),
        }
    }

    /// Field `j` (by DECLARATION index) of variant `variant` of layout `l`: its offset, its row
    /// in `layout_fields` and the enum's id when `l` is an enum. The one lookup of a field by
    /// index. THE PROMISE it rests on, checked here on every lookup: `layout` pushes the rows
    /// in declaration order (`for (j, ..) in parts.iter().enumerate()`), each at `ls`'s offset
    /// for index `j` (a `repr(Rust)` aggregate's OFFSETS are reordered by alignment, never its
    /// rows), so the row is `first + j` and carries the declared field's name: an ADT's field
    /// `j` of that variant, and `Name(j)` for a tuple, a closure's environment, a coroutine's
    /// one field and a wide pointer's two words. A row of another name is
    /// [`BuildError::FieldOrder`], never an offset read from the wrong field.
    fn decl_field(&self, l: u32, variant: u32, j: u32) -> Result<(u32, u32, Option<LayoutId>), BuildError> {
        let (first, len, id) = self.variant_rows(l, variant)?;
        if j >= len {
            return Err(malformed("a field past its aggregate's fields"));
        }
        let row = first + j;
        let &(found, off, _) = self.layout_fields.get(row as usize).ok_or(malformed("a layout's fields past their table"))?;
        let node = self.ground.view().node(self.layouts[l as usize].ty).map_err(|_| malformed("a layout's type past the ground table"))?;
        let declared = match node {
            PNode::Adt(k, _) => {
                let adt = self.adts.adt(k).ok_or(BuildError::NoAdt { adt: k })?;
                let v = adt.variants.get(variant as usize).ok_or(malformed("a variant past its ADT's variants"))?;
                let fields = adt.fields.get(v.fields.range()).ok_or(malformed("an ADT's fields past their column"))?;
                fields.get(j as usize).ok_or(malformed("a field past its ADT's fields"))?.name
            }
            PNode::Record(schema, _) => {
                let idx = usize::try_from(j).ok();
                self.ground
                    .view()
                    .record_names(schema)
                    .or_else(|| self.checked.types.view().record_names(schema))
                    .and_then(|ns| idx.and_then(|i| ns.get(i).copied()))
                    .unwrap_or(Name(u64::from(j)))
            }
            _ => Name(u64::from(j)),
        };
        if found != declared {
            return Err(BuildError::FieldOrder { ty: node, field: j, found });
        }
        Ok((off, row, id))
    }

    /// Makes variant `variant` of the enum of layout `id` at `at`: stores what `ls.tag_write`
    /// says (nothing for a struct, an enum of one present variant, or a niche encoding's
    /// untagged variant, whose own fields hold valid values there).
    fn write_tag(&mut self, id: Option<LayoutId>, variant: u32, at: (Value, i32)) {
        let Some(w) = id.and_then(|id| self.ls.tag_write(id, variant)) else { return };
        let v = match w.scalar.prim {
            // A pointer niche: null, or an address that is no allocation's.
            Prim::Ptr if w.value == 0 => self.b.null(),
            Prim::Ptr => {
                let x = self.b.iconst(Ty::I64, w.value);
                self.b.convert(Op::FromExposed, Flags::NONE, Ty::PTR, x)
            }
            p => self.b.iconst(prim_ty(p), w.value),
        };
        let attrs = MemAttrs::aligned(w.scalar.prim.align());
        self.b.store(Flags::NONE, v, at.0, at.1 + w.offset as i32, attrs);
    }

    /// The discriminant of the enum of layout `id` at `at`, as an `i64` (the tag decoded by
    /// the shared decoder, `intrinsics::read_discriminant`).
    fn read_discriminant(&mut self, id: LayoutId, at: (Value, i32)) -> Result<Value, BuildError> {
        crate::intrinsics::read_discriminant(&mut self.b, &self.ls, id, Some(at), Ty::I64)
            .ok_or(malformed("an enum's tag past an `i32` offset"))
    }

    /// The value a `switch` on `read_discriminant` matches for variant `v` of enum layout `id`.
    fn discriminant_case(&self, id: LayoutId, v: u32) -> Result<u128, BuildError> {
        let d = self.ls.discriminant(id, v).ok_or(malformed("a variant past its enum's discriminants"))?;
        Ok(mask(d as u128, 64))
    }

    /// The layout of ground aggregate type `g` (a struct, a tuple, an array, an enum, a wide
    /// pointer) at its arguments, the aggregates it holds first, by a work list. Its size,
    /// alignment, offsets and stride are `ls`'s; the row adds each part's `Repr` and whether
    /// it needs drop.
    fn layout(&mut self, g: PTy) -> Result<u32, TyErr> {
        if let Some(found) = self.find_layout(g) {
            return Ok(found);
        }
        let too_big = || TyErr::Unsupported("an aggregate of 2 GiB or more");
        let mut todo = Vec::new_in(self.alloc);
        let mut args: Vec<PTy, A> = Vec::new_in(self.alloc);
        // The parts: a struct's or tuple's fields, or an array's element.
        let mut parts: Vec<(Name, PTy), A> = Vec::new_in(self.alloc);
        // An enum's variants: where each one's fields end in `parts`.
        let mut variant_ends: Vec<u32, A> = Vec::new_in(self.alloc);
        todo.push(g);
        while let Some(&t) = todo.last() {
            if self.find_layout(t).is_some() {
                todo.pop();
                continue;
            }
            parts.clear();
            variant_ends.clear();
            let mut is_enum = false;
            let mut is_union = false;
            let mut array_len = None;
            let mut drop_fn = None;
            // `ManuallyDrop<T>` (core's `#[lang = "manually_drop"]`): its field is never dropped.
            let mut keeps_fields = false;
            // `repr(packed(n))`'s log2 of `n`: the most alignment a part's access may claim.
            let mut packed: Option<u8> = None;
            match self.ground_node(t)? {
                PNode::Adt(k, l) => {
                    args.clear();
                    args.extend_from_slice(
                        self.ground.view().list(l).map_err(|_| TyErr::Malformed("a list past the ground table"))?,
                    );
                    let adts = self.adts;
                    // A packed struct's or union's parts may sit below their alignment: `ls`
                    // places them (layout.rs `univariant`), and once the row is made every
                    // plain access of this function claims at most the cap (below).
                    packed = adts.facts().repr(k).map_err(|m| TyErr::Other(BuildError::NoAdt { adt: m.item }))?.packed;
                    drop_fn = adts.drop_impl(t, self.ground).map_err(TyErr::Other)?;
                    keeps_fields = adts.lang_adt(LangItem::ManuallyDrop) == Some(k);
                    let adt = adts.adt(k).ok_or(TyErr::Other(BuildError::NoAdt { adt: k }))?;
                    match adt.kind {
                        AdtKind::Struct => {
                            if adt.variants.len() != 1 {
                                return Err(TyErr::Malformed("a struct without exactly one variant"));
                            }
                        }
                        // The Reference, `[layout.repr.alignment.intro]`: "The align and packed
                        // modifiers can be used to respectively raise or lower the alignment of
                        // structs and unions": a packed enum is no Rust program's.
                        AdtKind::Enum if packed.is_some() => return Err(TyErr::Malformed("a `repr(packed)` enum")),
                        AdtKind::Enum => is_enum = true,
                        // Every field at offset 0 (`ls`'s); a copy copies each field's leaves,
                        // so every byte some field holds; its fields are never dropped.
                        AdtKind::Union => {
                            if adt.variants.len() != 1 {
                                return Err(TyErr::Malformed("a union without exactly one variant"));
                            }
                            is_union = true;
                        }
                    }
                    if adt.generics as usize != args.len() {
                        return Err(TyErr::Malformed("an ADT type whose argument count is not its ADT's generic count"));
                    }
                    for variant in adt.variants {
                        let fields = adt
                            .fields
                            .get(variant.fields.range())
                            .ok_or(TyErr::Malformed("an ADT's fields past their column"))?;
                        for f in fields {
                            let ft = ground(adt.types, f.ty, &args, self.ground, self.scratch, self.adts.facts()).map_err(ground_err)?;
                            parts.push((f.name, ft));
                        }
                        variant_ends.push(parts.len() as u32);
                    }
                }
                // A wide pointer: its address and its length (lib.rs, "Wide pointers").
                PNode::Ref(..) | PNode::Ptr(..) => {
                    let byte = self.ground.intern(PNode::Uint(UintTy::U8)).map_err(|_| TyErr::Malformed("the ground table is full"))?;
                    let ptr = self.ground.intern(PNode::Ptr(Mutbl::Not, byte)).map_err(|_| TyErr::Malformed("the ground table is full"))?;
                    let len = self.ground.intern(PNode::Uint(UintTy::Usize)).map_err(|_| TyErr::Malformed("the ground table is full"))?;
                    parts.push((Name(0), ptr));
                    parts.push((Name(1), len));
                }
                PNode::Tuple(l) => {
                    let elems = self.ground.view().list(l).map_err(|_| TyErr::Malformed("a list past the ground table"))?;
                    for (i, &et) in elems.iter().enumerate() {
                        parts.push((Name(u64::try_from(i).map_err(|_| TyErr::Malformed("too many fields"))?), et));
                    }
                }
                PNode::Record(schema, l) => {
                    let elems = self.ground.view().list(l).map_err(|_| TyErr::Malformed("a list past the ground table"))?;
                    let names = self
                        .ground
                        .view()
                        .record_names(schema)
                        .or_else(|| self.checked.types.view().record_names(schema));
                    for (i, &et) in elems.iter().enumerate() {
                        let nm = match names.and_then(|ns| ns.get(i).copied()) {
                            Some(nm) => nm,
                            None => Name(u64::try_from(i).map_err(|_| TyErr::Malformed("too many fields"))?),
                        };
                        parts.push((nm, et));
                    }
                }
                PNode::Union(l) => {
                    let elems = self.ground.view().list(l).map_err(|_| TyErr::Malformed("a list past the ground table"))?;
                    is_enum = true;
                    for &et in elems.iter() {
                        parts.push((Name(0), et));
                        let end = u32::try_from(parts.len()).map_err(|_| TyErr::Malformed("too many union variants"))?;
                        variant_ends.push(end);
                    }
                }
                PNode::Array(et, n) => {
                    parts.push((Name(0), et));
                    array_len = Some(self.ground_len(n)?);
                }
                // A closure: a struct of one field, its environment (closure.rs).
                PNode::Closure { .. } => {
                    let env = self.closure_parts(t)?.env;
                    parts.push((Name(0), env));
                }
                // A coroutine: one field, its upvars, in the state's size (coroutine.rs).
                PNode::Coroutine { .. } => {
                    let env = self.coroutine_parts(t)?.env;
                    parts.push((Name(0), env));
                }
                _ => return Err(TyErr::Malformed("a layout asked of other than an aggregate")),
            }
            let mut missing = None;
            for &(_, pt) in parts.iter() {
                // A pattern type is held as its base (`repr`): a wide pointer under one
                // (core's `NonNull<dyn T>` holds `pattern_type!(*const dyn T is !null)`) is laid
                // out first too, so no row is made between this row's fields below.
                let held = self.pattern_base(pt)?;
                // Every part whose `repr` makes a layout row is laid out first: an aggregate,
                // and a closure that captures nothing (not an aggregate, but `repr` lays it out
                // to see it is zero-sized). One laid out below, between this row's fields, would
                // put its own fields inside this row's (core's `read_separator`: a `move`
                // closure holding a captureless closure, census class 66).
                let lays_out = self.is_agg(held)? || matches!(self.ground_node(held)?, PNode::Closure { .. });
                if lays_out && self.find_layout(held).is_none() {
                    missing = Some(held);
                    break;
                }
            }
            if let Some(inner) = missing {
                if todo.contains(&inner) {
                    return Err(TyErr::Malformed("an aggregate that contains itself by value"));
                }
                todo.push(inner);
                continue;
            }
            // Every aggregate part is laid out by now: whether one needs drop is known.
            // A coroutine always needs drop (core's `needs_drop`: it "may return `true` for types
            // that don't actually need to be dropped"): its drop is the pass's `drop`, which
            // reads the tag (coroutine.rs).
            let mut needs_drop = drop_fn.is_some() || matches!(self.ground_node(t)?, PNode::Coroutine { .. });
            // A union's fields are never dropped (the reference, "Unions"): only its own
            // `Drop` impl runs. Nor are `ManuallyDrop`'s.
            for &(_, pt) in parts.iter().filter(|_| !is_union && !keeps_fields) {
                if !self.is_agg(pt)? {
                    continue;
                }
                let at = self.find_layout(pt).ok_or(TyErr::Malformed("a part whose layout was not made"))?;
                let part = &self.layouts[at as usize];
                needs_drop |= part.needs_drop;
            }
            // Held in memory (`Layout::placed`): by its own drop, or by a part's, kept or not.
            let mut placed = needs_drop;
            for &(_, pt) in parts.iter() {
                if placed || !self.is_agg(pt)? {
                    continue;
                }
                let at = self.find_layout(pt).ok_or(TyErr::Malformed("a part whose layout was not made"))?;
                placed = self.layouts[at as usize].placed;
            }
            // A packed struct or union (the Reference, `[layout.repr.alignment.packed-fields]`:
            // "The alignments of each field, for the purpose of positioning fields, is the
            // smaller of the specified alignment and the alignment of the field's type"): when
            // some part's alignment is above the cap, a part may sit below its own alignment,
            // so from here on every plain access this function makes claims at most the cap
            // (`Builder::cap_access_align`). The carrier is the function, not the place: places
            // are `(Value, i32)` everywhere and offsets are read from rows at dozens of sites; a
            // per-place cap one of them forgot would claim too much (a miscompile), while a
            // lowered claim is never wrong. No access through such a part can come before
            // this: its offset is read from this row, made here.
            if let Some(cap) = packed {
                let mut most = 0u8;
                for &(_, pt) in parts.iter() {
                    let align = if self.is_agg(pt)? {
                        let at = self.find_layout(pt).ok_or(TyErr::Malformed("a part whose layout was not made"))?;
                        let part = &self.layouts[at as usize];
                        // Its drop takes `&mut` to the part (the Reference, "Destructors"), a
                        // reference that must be aligned: rustc moves such a part to an aligned
                        // place first; the walk does not yet.
                        if part.needs_drop && part.align > cap && !is_union {
                            return Err(TyErr::Unsupported(
                                "a `repr(packed)` struct with a part that needs drop above the packed alignment",
                            ));
                        }
                        part.align
                    } else {
                        if matches!(self.ground_node(pt)?, PNode::Slice(_) | PNode::Str | PNode::Dynamic(_)) {
                            return Err(TyErr::Unsupported("a `repr(packed)` struct with an unsized tail"));
                        }
                        let repr = self.repr(pt)?;
                        self.size_align(repr).1
                    };
                    most = most.max(align);
                }
                if most > cap {
                    self.b.cap_access_align(cap);
                }
            }
            // The size, alignment and every offset are `ls`'s (rule 30: one layout).
            let id = self.ls_of(t)?;
            let lt = *self.ls.get(id);
            // A struct or tuple ending in a slice or `str` (walk/unsized_tail.rs): a row of its
            // fields at `ls`'s offsets, the tail's with no representation; `repr` refuses it by
            // value. A `dyn` tail's row holds the prefix's end, rounded up to the vtable's
            // alignment where the tail is reached (crate::dyn_tail).
            let unsized_tail = match lt.tail {
                Tail::Sized => false,
                Tail::Slice { .. } | Tail::Dyn if !is_enum && !is_union && array_len.is_none() => true,
                Tail::Slice { .. } | Tail::Dyn => return Err(TyErr::Malformed("an enum, union or array with an unsized tail")),
            };
            let size = u32::try_from(lt.size).ok().filter(|&s| s <= i32::MAX as u32).ok_or_else(too_big)?;
            let align = lt.align;
            let offset = |at: Option<u64>| -> Result<u32, TyErr> {
                u32::try_from(at.ok_or(TyErr::Malformed("a part `ls` gives no offset"))?).map_err(|_| too_big())
            };
            let layout = match array_len {
                // An enum (lib.rs, "Enums"): each variant's fields at their offsets in the enum,
                // as `ls` placed them (its tag, niche and field order are its own).
                None if is_enum => {
                    let n = variant_ends.len();
                    let vfirst = self.layout_variants.len() as u32;
                    if let Variants::Multiple { .. } = lt.variants {
                        // The walk switches on the discriminant's 64-bit pattern (`read_discriminant`
                        // reads an `i64`; `discriminant_case` masks each case to 64 bits), so any
                        // discriminant of a primitive representation of 64 bits or less fits, signed
                        // or unsigned: the Reference, `[layout.repr.primitive.enum]`, "primitive
                        // representations set the size and alignment to be the same as the primitive
                        // type of the same name": a `repr(usize)` discriminant is any `usize`, e.g.
                        // core's `AlignmentEnum::_Align1Shl63 = 1 << 63`.
                        for v in 0..n as u32 {
                            let d = self.ls.discriminant(id, v).ok_or(TyErr::Malformed("an enum without its discriminants"))?;
                            if i64::try_from(d).is_err() && u64::try_from(d).is_err() {
                                return Err(TyErr::Unsupported(
                                    "an enum discriminant that needs more than 64 bits (a `repr(u128)` or `repr(i128)` value)",
                                ));
                            }
                        }
                    }
                    let mut most = 0u64;
                    let mut from = 0usize;
                    for vi in 0..n {
                        let to = variant_ends[vi] as usize;
                        let first = self.layout_fields.len() as u32;
                        // No offset: a variant `ls` dropped (uninhabited, of zero-sized fields
                        // only, which no value is ever of).
                        let vid = self.ls.variant(id, vi as u32);
                        let mut leaves = 0u64;
                        for (j, &(name, pt)) in parts[from..to].iter().enumerate() {
                            let repr = self.repr(pt)?;
                            let at = match (vid.and_then(|v| self.ls.field_offset(v, j as u32)), repr) {
                                (Some(o), _) => offset(Some(o))?,
                                (None, Repr::None) => 0,
                                (None, _) => return Err(TyErr::Malformed("a variant `ls` dropped with a part that has a value")),
                            };
                            self.layout_fields.push((name, at, repr));
                            self.layout_field_tys.push(pt);
                            leaves = leaves.saturating_add(self.leaves(repr));
                        }
                        self.layout_variants.push((first, (to - from) as u32));
                        most = most.max(leaves);
                        from = to;
                    }
                    // Uninhabited: it never has a value to copy.
                    let leaves = if lt.uninhabited { 0 } else { most.saturating_add(1) };
                    let shape = Shape::Enum { id, first: vfirst, len: n as u32 };
                    Layout { id, ty: t, size, align, leaves, shape, needs_drop, placed, drop_fn, is_union }
                }
                Some(n) => {
                    let elem = self.repr(parts[0].1)?;
                    let crate::layout::Fields::Array { stride, .. } = lt.fields else {
                        return Err(TyErr::Malformed("an array `ls` does not lay out as one"));
                    };
                    let stride = u32::try_from(stride).map_err(|_| too_big())?;
                    let len = u32::try_from(n).map_err(|_| TyErr::Unsupported("an array of 2^32 elements or more"))?;
                    let leaves = self.leaves(elem).saturating_mul(n);
                    Layout { id, ty: t, size, align, leaves, shape: Shape::Array { elem, len, stride }, needs_drop, placed, drop_fn, is_union }
                }
                // A struct's or tuple's fields at `ls`'s offsets (a `repr(Rust)` one reordered by
                // decreasing alignment), by declaration index.
                None => {
                    let first = self.layout_fields.len() as u32;
                    let mut leaves = 0u64;
                    for (j, &(name, pt)) in parts.iter().enumerate() {
                        // The unsized tail has no representation of its own: it is reached
                        // only as an unsized place (walk/unsized_tail.rs).
                        let repr = if unsized_tail && j + 1 == parts.len() { Repr::None } else { self.repr(pt)? };
                        let at = offset(self.ls.field_offset(id, j as u32))?;
                        self.layout_fields.push((name, at, repr));
                        self.layout_field_tys.push(pt);
                        leaves = leaves.saturating_add(self.leaves(repr));
                    }
                    let len = parts.len() as u32;
                    // A row's fields are contiguous: no part's `repr` made a row between them
                    // (the pre-pass above lays out every part that has one).
                    if self.layout_fields.len() as u32 != first + len {
                        return Err(TyErr::Malformed("a part laid out between its aggregate's fields (a row's fields not contiguous)"));
                    }
                    if unsized_tail {
                        self.unsized_tails.push(self.layouts.len() as u32);
                    }
                    Layout { id, ty: t, size, align, leaves, shape: Shape::Fields { first, len }, needs_drop, placed, drop_fn, is_union }
                }
            };
            self.layouts.push(layout);
            todo.pop();
        }
        self.find_layout(g).ok_or(TyErr::Malformed("an aggregate whose layout was not made"))
    }

    fn ty_err(e: ExprId, error: TyErr) -> BuildError {
        match error {
            TyErr::Unsupported(what) => BuildError::UnsupportedType { expr: e, what },
            TyErr::Malformed(what) => malformed(what),
            TyErr::Other(error) => error,
        }
    }

    /// How a type of the record is held in this instance, met at `e`.
    fn repr_at(&mut self, e: ExprId, t: PTy) -> Result<Repr, BuildError> {
        self.ground_ty(t).and_then(|g| self.repr(g)).map_err(|error| Self::ty_err(e, error))
    }

    fn expr(&self, e: ExprId) -> Result<Expr, BuildError> {
        self.body.exprs.get(e.ix()).copied().ok_or(malformed("an expression past `exprs`"))
    }

    fn pat(&self, p: PatId) -> Result<Pat, BuildError> {
        self.body.pats.get(p.ix()).copied().ok_or(malformed("a pattern past `pats`"))
    }

    /// What path or struct expression `e` resolved to, as the check recorded it.
    fn expr_def(&self, e: ExprId) -> Option<Def> {
        let col = self.checked.expr_defs.column::<0>();
        col.binary_search(&e).ok().map(|at| self.checked.expr_defs.column::<1>()[at])
    }

    /// What path, tuple-struct or struct pattern `p` resolved to, as the check recorded it.
    fn pat_def(&self, p: PatId) -> Option<Def> {
        let col = self.checked.pat_defs.column::<0>();
        col.binary_search(&p).ok().map(|at| self.checked.pat_defs.column::<1>()[at])
    }

    /// The variant a definition makes: a variant's index, a struct's 0.
    fn def_variant(def: Option<Def>) -> Option<u32> {
        match def? {
            Def::Ctor { variant, .. } => Some(variant),
            Def::Adt(_) => Some(0),
            _ => None,
        }
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

    /// Whether `e`'s last adjustment is an overloaded dereference: the operand of an overloaded
    /// `*e`, where the check records the dereference on the operand.
    fn deref_on_operand(&self, e: ExprId) -> bool {
        let rows = self.adjust_rows(e);
        matches!(self.checked.adjusts.column::<1>()[rows].last(), Some(AdjustKind::Deref { overloaded: Some(_) }))
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
        if self.vals.is_empty() {
            return Err(malformed("the value stack ran out (a build bug)"));
        }
        Ok(self.pop_val())
    }

    /// The scalar of `v`, of type `ty`; in unreachable code a value that diverged is `uninit`.
    fn scalar(&mut self, v: V, ty: Ty) -> Result<Value, BuildError> {
        match v {
            V::Scalar(x) => Ok(x),
            V::None if !self.live => Ok(self.b.uninit(ty)),
            V::None => Err(malformed("an expression with no value where its type has one")),
            V::Place(..) => Err(malformed("a place where a scalar was expected")),
            V::Unsized(..) => Err(malformed("an unsized place where a scalar was expected")),
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
            V::Unsized(..) => Err(malformed("an unsized place where a sized one was expected")),
        }
    }

    fn address(&mut self, (p, off): (Value, i32)) -> Value {
        if off == 0 {
            return p;
        }
        let by = self.b.iconst(Ty::I64, off as i64 as u128);
        self.b.ptr_add(Flags::NONE, p, by)
    }

    /// What an edge passes for `v` of `repr`: nothing, the scalar, or the aggregate's address.
    fn arg(&mut self, v: V, repr: Repr) -> Result<Option<Value>, BuildError> {
        Ok(match repr {
            Repr::None => None,
            Repr::Agg(_) => {
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
        match repr.kept() {
            Kept::None => V::None,
            Kept::Agg(_) => V::Place(self.b.param(Ty::PTR), 0),
            Kept::Scalar(ty) => V::Scalar(self.b.param(ty)),
        }
    }

    fn mem(&self, repr: Repr) -> MemAttrs {
        MemAttrs::aligned(self.size_align(repr).1)
    }

    fn load(&mut self, repr: Repr, (p, off): (Value, i32)) -> V {
        match repr.kept() {
            Kept::None => V::None,
            Kept::Agg(_) => V::Place(p, off),
            Kept::Scalar(ty) => {
                let attrs = self.mem(repr);
                V::Scalar(self.b.load(ty, Flags::NONE, p, off, attrs))
            }
        }
    }

    fn store(&mut self, v: V, repr: Repr, to: (Value, i32)) -> Result<(), BuildError> {
        match repr {
            Repr::None => Ok(()),
            Repr::Agg(_) => {
                let from = self.place(v, repr)?;
                if from != to {
                    self.copy(repr, from, to)?;
                }
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

    /// Copies layout `l`'s `size` bytes from one place to another, whatever they hold, as one
    /// `memcpy`. A union's bytes may be uninitialised past the field written, and may carry a
    /// pointer's provenance: LIR's `memcpy` keeps both ("Bytes keep their provenance"), while
    /// an integer load of an uninitialised byte is undefined and an integer drops provenance.
    /// The backends lower a small `memcpy` of a known length to loads and stores themselves.
    fn copy_bytes(&mut self, l: u32, from: (Value, i32), to: (Value, i32)) {
        // The walk's `Layout::size` is a `u32`; its `align` and `MemAttrs::align` are both log2
        // of the alignment in bytes.
        let (size, align) = (self.layouts[l as usize].size, self.layouts[l as usize].align);
        if size == 0 {
            return;
        }
        let (src, dst) = (self.address(from), self.address(to));
        let len = self.b.iconst(Ty::I64, u128::from(size));
        let mut attrs = MemAttrs::aligned(align);
        attrs.src_align = align;
        self.b.bulk(Op::Memcpy, Flags::NONE, dst, src, len, attrs);
    }

    /// Copies a value of `repr` from one place to another, scalar leaf by scalar leaf (never
    /// the padding), by a work list: a struct's or tuple's fields in turn, an array's elements
    /// unrolled up to `UNROLL_LEAVES` leaves and in a loop beyond.
    fn copy(&mut self, repr: Repr, from: (Value, i32), to: (Value, i32)) -> Result<(), BuildError> {
        let before = self.b.func().ops.len();
        let copied = self.copy_leaves(repr, from, to);
        self.copy_insts += (self.b.func().ops.len() - before) as u64;
        copied
    }

    /// [`Walk::copy`]'s work, counted by it.
    fn copy_leaves(&mut self, repr: Repr, from: (Value, i32), to: (Value, i32)) -> Result<(), BuildError> {
        let mut stack = Vec::new_in(self.alloc);
        stack.push(CopyStep::Copy { repr, from, to });
        while let Some(step) = stack.pop() {
            let (repr, from, to) = match step {
                CopyStep::Copy { repr, from, to } => (repr, from, to),
                CopyStep::Next { i, header, exit } => {
                    let one = self.b.iconst(Ty::I64, 1);
                    let next = self.b.binary(Op::Add, Flags::NONE, i, one);
                    self.jump(header, &[next]);
                    self.begin(exit);
                    continue;
                }
                CopyStep::Begin(block) => {
                    self.begin(block);
                    continue;
                }
                CopyStep::Jump(block) => {
                    self.jump(block, &[]);
                    continue;
                }
                CopyStep::Tag { id, variant, to } => {
                    self.write_tag(Some(id), variant, to);
                    continue;
                }
            };
            let l = match repr.kept() {
                Kept::None => continue,
                Kept::Agg(l) => l,
                Kept::Scalar(ty) => {
                    let attrs = self.mem(repr);
                    let x = self.b.load(ty, Flags::NONE, from.0, from.1, attrs);
                    self.b.store(Flags::NONE, x, to.0, to.1, attrs);
                    continue;
                }
            };
            // A coroutine's state is copied whole: what the pass keeps in it and its tag are no
            // field of its row (coroutine.rs `copy_state`).
            if self.copy_state(l, from, to) {
                continue;
            }
            // A union is copied as its bytes, never by its fields and never switching on a tag:
            // the layout does not know which field is live (the Reference, "Unions": reading a
            // field reinterprets the bytes; the copy keeps every byte, uninitialised ones too).
            // `MaybeUninit<T>` is one: `uninit()` leaves `value: ManuallyDrop<T>`'s bytes unwritten.
            if self.layouts[l as usize].is_union {
                self.copy_bytes(l, from, to);
                continue;
            }
            match self.layouts[l as usize].shape {
                Shape::Fields { first, len } => {
                    for i in (first..first + len).rev() {
                        let (_, off, repr) = self.layout_fields[i as usize];
                        let off = off as i32;
                        stack.push(CopyStep::Copy { repr, from: (from.0, from.1 + off), to: (to.0, to.1 + off) });
                    }
                }
                Shape::Array { elem, len, stride } => {
                    if self.layouts[l as usize].leaves <= UNROLL_LEAVES {
                        for i in (0..len).rev() {
                            let off = (i * stride) as i32;
                            stack.push(CopyStep::Copy { repr: elem, from: (from.0, from.1 + off), to: (to.0, to.1 + off) });
                        }
                        continue;
                    }
                    let (i, fp, tp, header, exit) = self.array_loop(len, stride, from, to);
                    stack.push(CopyStep::Next { i, header, exit });
                    stack.push(CopyStep::Copy { repr: elem, from: (fp, 0), to: (tp, 0) });
                }
                // Switch on the variant, copy its fields alone, write its tag (lib.rs, "Enums"):
                // a copy never reads a byte the value's variant leaves undefined.
                Shape::Enum { id, first, len } => {
                    if self.layouts[l as usize].leaves == 0 {
                        continue;
                    }
                    let fields = |w: &Self, v: u32, stack: &mut Vec<CopyStep, A>| {
                        let (f, n) = w.layout_variants[(first + v) as usize];
                        for i in (f..f + n).rev() {
                            let (_, off, repr) = w.layout_fields[i as usize];
                            let off = off as i32;
                            stack.push(CopyStep::Copy { repr, from: (from.0, from.1 + off), to: (to.0, to.1 + off) });
                        }
                    };
                    match self.ls.get(id).variants {
                        // One present variant and no tag: its fields.
                        Variants::Single { index } if index < len => fields(self, index, &mut stack),
                        Variants::Single { .. } => {}
                        Variants::Multiple { .. } => {
                            let d = self.read_discriminant(id, from)?;
                            let join = self.create();
                            let none = self.create();
                            stack.push(CopyStep::Begin(join));
                            let mut cases = Vec::new_in(self.alloc);
                            for v in (0..len).rev() {
                                let case = self.discriminant_case(id, v)?;
                                let block = self.create();
                                cases.push((case, block));
                                stack.push(CopyStep::Jump(join));
                                stack.push(CopyStep::Tag { id, variant: v, to });
                                fields(self, v, &mut stack);
                                stack.push(CopyStep::Begin(block));
                            }
                            self.switch(d, none, &cases);
                            // No variant has another discriminant: holding one is UB.
                            self.begin(none);
                            self.unreachable_of_enum(id, len, self.task_at)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Ends the block with a switch on `x`: to each case's block on its value, else `default`.
    fn switch(&mut self, x: Value, default: Block, cases: &[(u128, Block)]) {
        let mut tos = Vec::with_capacity_in(cases.len(), self.alloc);
        for &(v, block) in cases {
            self.reach(block);
            tos.push((v, (block, &[] as &[Value])));
        }
        self.reach(default);
        self.b.switch(x, (default, &[]), &tos);
    }

    /// Opens a loop over the `len` elements of two arrays, `stride` apart: the index, the two
    /// elements' addresses, the loop's head and its exit. The body is built next; it ends by
    /// adding 1 to the index and jumping to the head, after which the exit is begun.
    fn array_loop(
        &mut self,
        len: u32,
        stride: u32,
        from: (Value, i32),
        to: (Value, i32),
    ) -> (Value, Value, Value, Block, Block) {
        let header = self.create();
        let body = self.create();
        let exit = self.create();
        let zero = self.b.iconst(Ty::I64, 0);
        let (from, to) = (self.address(from), self.address(to));
        self.jump(header, &[zero]);
        self.begin(header);
        let i = self.b.param(Ty::I64);
        let n = self.b.iconst(Ty::I64, u128::from(len));
        let more = self.b.icmp(IntCond::Ult, i, n);
        self.br(more, (body, &[]), (exit, &[]));
        self.begin(body);
        let s = self.b.iconst(Ty::I64, u128::from(stride));
        let off = self.b.binary(Op::Mul, Flags::NONE, i, s);
        let fp = self.b.ptr_add(Flags::NONE, from, off);
        let tp = self.b.ptr_add(Flags::NONE, to, off);
        (i, fp, tp, header, exit)
    }

    /// A fresh slot for `repr`.
    fn temp(&mut self, repr: Repr) -> (Value, i32) {
        let (size, align) = self.size_align(repr);
        let slot = self.b.slot(size, align, SlotKind::Temp);
        self.name_temp(slot);
        (self.b.slot_addr(slot), 0)
    }

    /// A `bool` (`i8`) as an `i1` to branch on.
    fn cond(&mut self, v: V) -> Result<Value, BuildError> {
        let x = self.scalar(v, Ty::I8)?;
        let zero = self.b.iconst(Ty::I8, 0);
        Ok(self.b.icmp(IntCond::Ne, x, zero))
    }

    // Bindings.

    /// The place a binding's home is.
    fn home(&mut self, home: Home) -> (Value, i32) {
        match home {
            Home::Slot(slot) => (self.b.slot_addr(slot), 0),
            Home::At(p, off) => (p, off),
        }
    }

    /// How a binding's type is held.
    fn local_repr(&mut self, p: PatId) -> Result<Repr, BuildError> {
        let t = *self.checked.local_ty.get(p.ix()).ok_or(malformed("a binding without a type in the record"))?;
        self.ground_ty(t).and_then(|g| self.repr(g)).map_err(|error| Self::pat_ty_err(p, error))
    }

    fn pat_ty_err(p: PatId, error: TyErr) -> BuildError {
        match error {
            TyErr::Unsupported(what) => BuildError::UnsupportedPat { pat: p, what },
            TyErr::Malformed(what) => malformed(what),
            TyErr::Other(error) => error,
        }
    }

    /// Checks `p` is a plain binding (no `ref`, no subpattern).
    fn plain_binding(&self, p: PatId) -> Result<(), BuildError> {
        match self.pat(p)? {
            Pat::Bind { ann, sub: None, .. } if ann.by_ref.is_none() => Ok(()),
            Pat::Bind { .. } => Err(BuildError::UnsupportedPat { pat: p, what: "a `ref` binding or a binding with a subpattern" }),
            _ => Err(BuildError::UnsupportedPat { pat: p, what: "a pattern other than a binding or `_`" }),
        }
    }

    /// Binding pattern `p` lives at `at`, memory it owns (a caller's copy, a temporary).
    fn bind_at(&mut self, p: PatId, at: (Value, i32)) -> Result<(), BuildError> {
        self.plain_binding(p)?;
        if self.slots[p.ix()].is_some() {
            return Err(malformed("a binding bound twice"));
        }
        let repr = self.local_repr(p)?;
        self.slots[p.ix()] = Some((Home::At(at.0, at.1), repr));
        Ok(())
    }

    /// Captured variable `p` (a closure's or an async block's upvar) lives at `at`. Any
    /// binding: a capture names the variable, whatever its pattern was in the enclosing body
    /// (`ref x`, `r @ 0..=99`: chrono's `Parsed::to_naive_date::resolve_year` captures
    /// `Some(r @ 0..=99)`'s `r` in `|v| v.checked_add(r)`); its subpattern was tested there.
    fn bind_capture_at(&mut self, p: PatId, at: (Value, i32)) -> Result<(), BuildError> {
        if !matches!(self.pat(p)?, Pat::Bind { .. }) {
            return Err(malformed("a capture of other than a binding"));
        }
        if self.slots[p.ix()].is_some() {
            return Err(malformed("a binding bound twice"));
        }
        let repr = self.local_repr(p)?;
        self.slots[p.ix()] = Some((Home::At(at.0, at.1), repr));
        self.captured.push(p);
        Ok(())
    }

    /// The slot of binding pattern `p`, made on first use.
    fn bind(&mut self, p: PatId) -> Result<(Home, Repr), BuildError> {
        self.plain_binding(p)?;
        if let Some(found) = self.slots[p.ix()] {
            return Ok(found);
        }
        let repr = self.local_repr(p)?;
        let (size, align) = self.size_align(repr);
        let slot = self.b.slot(size, align, SlotKind::Named);
        self.name_slot(slot, blaze_flow_lower::identity::Identity::Binding(p));
        self.slots[p.ix()] = Some((Home::Slot(slot), repr));
        Ok((Home::Slot(slot), repr))
    }

    /// Whether irrefutable pattern `p` peels references off the type it is matched against
    /// (the Reference, "Binding modes": "each time a reference is matched using a
    /// non-reference pattern, it will automatically dereference the value"): a tuple, a
    /// struct or tuple-struct, a slice or a path pattern. The check records such a pattern at
    /// its peeled type (blaze-typeck rules/pat.rs `step_pat`).
    fn peels_refs(&self, p: PatId) -> Result<bool, BuildError> {
        Ok(matches!(self.pat(p)?, Pat::Tuple { .. } | Pat::TupleStruct { .. } | Pat::Struct { .. } | Pat::Slice { .. } | Pat::Path(_)))
    }

    /// Binds `v`, a value of ground type `g` (its binder's: a `let` initializer's adjusted
    /// type, a parameter's, a field's), to pattern `p`. A pattern that peels references off a
    /// reference `g` is the irrefutable binder's at `g` (`let Ending { fw, stream } = &*p;`,
    /// `let World { driver, .. } = &mut *world;`): its recorded type is the peeled one, which
    /// is not the value's. Every other pattern's recorded type is `g`: [`Self::bind_value`].
    fn bind_value_of(&mut self, p: PatId, v: V, g: PTy) -> Result<(), BuildError> {
        if self.peels_refs(p)? && matches!(self.ground_node(g).map_err(|error| Self::pat_ty_err(p, error))?, PNode::Ref(..)) {
            return self.bind_irrefutable_at(p, v, g);
        }
        self.bind_value(p, v)
    }

    /// Binds `v` to pattern `p`: a binding (the value stored into its slot), `_` (dropped), a
    /// tuple pattern or an array pattern (`[a, _, b]`) taken apart from the aggregate's place,
    /// by a work list; every other irrefutable pattern (a struct or only-variant pattern, `&p`,
    /// an array with `..`, a tuple over a reference) by walk/matches/irrefutable.rs.
    fn bind_value(&mut self, p: PatId, v: V) -> Result<(), BuildError> {
        let mut todo = Vec::new_in(self.alloc);
        let mut parts = Vec::new_in(self.alloc);
        todo.push((p, v));
        while let Some((p, v)) = todo.pop() {
            let (elems, rest) = match self.pat(p)? {
                Pat::Wild => {
                    let t = *self.checked.pat_ty.get(p.ix()).ok_or(malformed("a pattern without a type in the record"))?;
                    let g = self.ground_ty(t).map_err(|error| Self::pat_ty_err(p, error))?;
                    if self.needs_drop(g).map_err(|error| Self::pat_ty_err(p, error))? {
                        return Err(BuildError::UnsupportedPat { pat: p, what: "a `_` pattern over a value that needs drop" });
                    }
                    continue;
                }
                Pat::Bind { .. } => {
                    let (home, repr) = self.bind(p)?;
                    let at = self.home(home);
                    self.store(v, repr, at)?;
                    continue;
                }
                Pat::Tuple { elems, rest } => (elems, rest),
                Pat::Slice { elems, rest: None } => (elems, None),
                // An array with `..`, a struct or only-variant pattern, `&p`: the irrefutable
                // binder, by the match builder's projection.
                Pat::Slice { rest: Some(_), .. }
                | Pat::Struct { .. }
                | Pat::Record { .. }
                | Pat::TupleStruct { .. }
                | Pat::Ref { .. }
                | Pat::Path(_) => {
                    self.bind_irrefutable(p, v)?;
                    continue;
                }
                _ => {
                    return Err(BuildError::UnsupportedPat {
                        pat: p,
                        what: "a pattern other than a binding, `_`, a tuple, an array, a struct, a variant, a path or a `&` pattern",
                    });
                }
            };
            let subs = self.body.pat_lists.get(elems.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?;
            let t = *self.checked.pat_ty.get(p.ix()).ok_or(malformed("a pattern without a type in the record"))?;
            let g = self.ground_ty(t).map_err(|error| Self::pat_ty_err(p, error))?;
            let node = self.ground_node(g).map_err(|error| Self::pat_ty_err(p, error))?;
            let repr = self.repr(g).map_err(|error| Self::pat_ty_err(p, error))?;
            let tuple = matches!(self.pat(p)?, Pat::Tuple { .. });
            match (node, tuple) {
                (PNode::Tuple(_), true) | (PNode::Array(..), false) => {}
                (PNode::Slice(_), false) => {
                    return Err(BuildError::UnsupportedPat { pat: p, what: "a slice pattern of a slice" });
                }
                // Over a reference ("Binding modes": dereferenced, bindings by `ref`).
                (PNode::Ref(..), _) => {
                    self.bind_irrefutable(p, v)?;
                    continue;
                }
                _ => return Err(malformed("a tuple or array pattern of a type of another shape")),
            }
            // Each part's offset and representation, in element order.
            parts.clear();
            match repr {
                // A zero-sized aggregate: no part has a value.
                Repr::None => {
                    let n = match node {
                        PNode::Tuple(l) => l.len,
                        PNode::Array(_, n) => {
                            let n = self.ground_len(n).map_err(|error| Self::pat_ty_err(p, error))?;
                            u32::try_from(n).map_err(|_| malformed("an array of 2^32 elements or more"))?
                        }
                        // The match above let through only a tuple or an array.
                        _ => return Err(malformed("a zero-sized tuple or array pattern of another type")),
                    };
                    parts.resize(n as usize, (0u32, Repr::None));
                }
                Repr::Agg(l) => match self.layouts[l as usize].shape {
                    Shape::Fields { first, len } => {
                        for i in first..first + len {
                            let (_, off, r) = self.layout_fields[i as usize];
                            parts.push((off, r));
                        }
                    }
                    Shape::Array { elem, len, stride } => {
                        for i in 0..len {
                            parts.push((i * stride, elem));
                        }
                    }
                    Shape::Enum { .. } => return Err(malformed("a tuple or array pattern of an enum")),
                },
                _ => return Err(malformed("a tuple or array pattern of a scalar")),
            }
            let n = parts.len();
            let fits = match rest {
                Some(_) => subs.len() <= n,
                None => subs.len() == n,
            };
            if !fits {
                return Err(match (tuple, rest) {
                    (false, _) => BuildError::UnsupportedPat { pat: p, what: "an array pattern with `..` or of another length" },
                    _ => malformed("a tuple pattern of another arity than its type"),
                });
            }
            if let Some(r) = rest {
                let skipped = r as usize..r as usize + (n - subs.len());
                if parts[skipped].iter().any(|&(_, r)| self.repr_needs_drop(r)) {
                    return Err(BuildError::UnsupportedPat { pat: p, what: "a `..` pattern over a part that needs drop" });
                }
            }
            let at = match repr {
                Repr::Agg(_) => Some(self.place(v, repr)?),
                _ => None,
            };
            for (j, &sub) in subs.iter().enumerate().rev() {
                let k = match rest {
                    Some(r) if j >= r as usize => n - (subs.len() - j),
                    _ => j,
                };
                let (off, r) = parts[k];
                let part = match at {
                    Some((base, at)) => {
                        let at = (base, at + off as i32);
                        self.load(r, at)
                    }
                    None => V::None,
                };
                // A part whose pattern peels references off it (`let (Foo { a }, n) = (&foo, 1);`)
                // is bound at the part's own type (`bind_value_of`).
                if r == Repr::Ptr && self.peels_refs(sub)? {
                    let pg = match (repr, node) {
                        (Repr::Agg(l), PNode::Tuple(_)) => match self.layouts[l as usize].shape {
                            Shape::Fields { first, .. } => self.layout_field_tys[first as usize + k],
                            _ => return Err(malformed("a tuple not laid out as fields")),
                        },
                        (_, PNode::Array(elem, _)) => elem,
                        _ => return Err(malformed("a tuple or array pattern of a type of another shape")),
                    };
                    self.bind_value_of(sub, part, pg)?;
                    continue;
                }
                todo.push((sub, part));
            }
        }
        Ok(())
    }

    // Drops (lib.rs, "Drops").

    /// Whether ground type `g` needs drop.
    fn needs_drop(&mut self, g: PTy) -> Result<bool, TyErr> {
        if !self.is_agg(g)? {
            return Ok(false);
        }
        let l = self.layout(g)?;
        Ok(self.layouts[l as usize].needs_drop)
    }

    /// Whether a value of `repr` needs drop: its type's answer, read from the type's own row.
    /// `Repr::None` is never a type that needs drop (`repr` holds those as `Agg`, whatever
    /// their size), so this equals `needs_drop` of the type for every type `repr` accepts.
    fn repr_needs_drop(&self, repr: Repr) -> bool {
        match repr {
            Repr::Agg(l) => self.layouts[l as usize].needs_drop,
            _ => false,
        }
    }

    /// Calls the drop glue of ground type `g` on the value at `at`: `drop_in_place::<g>`.
    fn drop_at(&mut self, g: PTy, at: (Value, i32)) -> Result<(), BuildError> {
        let item = self.adts.lang_fn(DROP_FN).ok_or(BuildError::NoLangFn { expr: None, name: DROP_FN })?;
        let args = self.ground.intern_list(&[g]).map_err(|_| malformed("the ground table is full"))?;
        let p = self.address(at);
        self.call_ptr(Instance { item, args }, p)
    }

    /// A drop call of `instance`, a fn of one pointer parameter and no result, with `p`: plain,
    /// or an `invoke` when an unwind ladder says so (walk/unwind_ladder.rs `drop_call`).
    fn call_ptr(&mut self, instance: Instance, p: Value) -> Result<(), BuildError> {
        // A tracked `Drop::drop` has no call expression to be the place: refused by name.
        let (sig, values) = self.lang_call_sig(None, instance, &[Param::of(Ty::PTR)], &[p])?;
        let place = self.ref_place(instance);
        let reference = self.b.reference(RefKind::Func, place);
        self.drop_call(Callee::Direct(reference), sig, &values)
    }

    /// Binding `p`, just bound (`init`) or declared without a value, joins the innermost drop
    /// scope if its type needs drop; a binding with a flag has it set here, at its `let`,
    /// which every use of the binding comes after.
    fn own(&mut self, p: PatId, init: bool) -> Result<(), BuildError> {
        let t = *self.checked.local_ty.get(p.ix()).ok_or(malformed("a binding without a type in the record"))?;
        let g = self.ground_ty(t).map_err(|error| Self::pat_ty_err(p, error))?;
        if !self.needs_drop(g).map_err(|error| Self::pat_ty_err(p, error))? {
            self.own_kept(p, init);
            return Ok(());
        }
        let (_, repr) = self.slots[p.ix()].ok_or(malformed("a binding owned before it has a home"))?;
        // `repr` holds every type that needs drop in memory (`Layout::placed`), whatever its size.
        if repr == Repr::None {
            return Err(malformed("a binding whose type needs drop held as no value"));
        }
        if self.owned.get(p.ix()).copied().and_then(LocalOwn::as_drop).is_some() {
            return Err(malformed("a binding owned twice"));
        }
        let flag = if self.needs_flag[p.ix()] {
            let slot = self.b.slot(1, 0, SlotKind::Named);
            self.name_flag(slot, blaze_flow_lower::identity::Identity::Binding(p));
            let at = self.b.slot_addr(slot);
            let v = self.b.iconst(Ty::I8, u128::from(init));
            self.b.store(Flags::NONE, v, at, 0, MemAttrs::aligned(0));
            Some(slot)
        } else {
            None
        };
        let rest = if self.partial[p.ix()] {
            let slot = self.b.slot(8, 3, SlotKind::Named);
            Some(slot)
        } else {
            None
        };
        self.owned[p.ix()] = LocalOwn::Drop(Owned { ty: g, flag, rest });
        self.hold_all(p);
        self.init[p.ix()] = init;
        self.scope_pats.push(Held::Pat(p));
        Ok(())
    }

    /// Marks binding `p` whose type needs no drop (e.g. `ManuallyDrop<T>`) as owned by this body,
    /// allocating its field-flags word when a dropping part moves out of it (`partial`).
    fn own_kept(&mut self, p: PatId, init: bool) {
        let Some(entry) = self.owned.get_mut(p.ix()) else { return };
        if !self.partial.get(p.ix()).copied().unwrap_or(false) {
            *entry = LocalOwn::Kept;
            return;
        }
        let slot = match *entry {
            LocalOwn::KeptFlags(slot) => slot,
            LocalOwn::None | LocalOwn::Kept | LocalOwn::Drop(_) => {
                let slot = self.b.slot(8, 3, SlotKind::Named);
                *entry = LocalOwn::KeptFlags(slot);
                slot
            }
        };
        let at = self.b.slot_addr(slot);
        let bits = self.b.iconst(Ty::I64, if init { u128::from(u64::MAX) } else { 0 });
        self.b.store(Flags::NONE, bits, at, 0, MemAttrs::aligned(3));
    }

    /// The field-flags word of binding `p`, whether its type needs drop (`Owned::rest`) or not
    /// (`LocalOwn::KeptFlags`, e.g. `ManuallyDrop<T>`).
    fn field_mask(&self, p: PatId) -> Option<SlotId> {
        match self.owned.get(p.ix()).copied() {
            Some(LocalOwn::Drop(o)) => o.rest,
            Some(LocalOwn::KeptFlags(slot)) => Some(slot),
            Some(LocalOwn::None | LocalOwn::Kept) | None => None,
        }
    }

    /// Whether binding `p` is owned by this body (not a borrowed or non-`FnOnce` capture),
    /// whether its own type needs drop or not (`ManuallyDrop<T>`).
    fn local_is_owned(&self, p: PatId) -> bool {
        !matches!(self.owned.get(p.ix()).copied(), None | Some(LocalOwn::None))
    }

    /// Whether binding `p` is owned by this body and its type needs drop.
    fn local_drops(&self, p: PatId) -> bool {
        self.owned.get(p.ix()).copied().and_then(LocalOwn::as_drop).is_some()
    }

    /// Sets every field flag of binding `p`, if it has them: it holds all of its value again
    /// (its `let`, an assignment over it).
    fn hold_all(&mut self, p: PatId) {
        if let Some(slot) = self.field_mask(p) {
            let at = self.b.slot_addr(slot);
            let all = self.b.iconst(Ty::I64, u128::from(u64::MAX));
            self.b.store(Flags::NONE, all, at, 0, MemAttrs::aligned(3));
        }
    }

    /// The bit of field `j` of variant `variant` of enum layout `l` in a field-flag word: the
    /// fields of every variant before it, then `j`; `None` past 64 fields.
    fn field_bit(&self, l: u32, variant: u32, j: u32) -> Result<Option<u32>, BuildError> {
        let Shape::Enum { first, .. } = self.layouts[l as usize].shape else {
            return Err(malformed("field flags of other than an enum"));
        };
        let before: u32 = (0..variant).map(|v| self.layout_variants[(first + v) as usize].1).sum();
        Ok(Some(before + j).filter(|&b| b < 64))
    }

    /// Drops what the binding of ground type `g` at `at` holds: all of it, or with field flags
    /// `rest`, each field of the live variant whose flag is set, in declaration order.
    fn drop_owned(&mut self, g: PTy, at: (Value, i32), rest: Option<SlotId>) -> Result<(), BuildError> {
        let Some(slot) = rest else { return self.drop_at(g, at) };
        // A match by tests moved parts out of it (walk/moves.rs), or it is not an enum.
        if self.moves_take(g, slot)? {
            return self.drop_moved(g, at, slot, true);
        }
        let l = self.layout(g).map_err(|_| malformed("a binding with field flags and no layout"))?;
        let Shape::Enum { id, first, len } = self.layouts[l as usize].shape else {
            return Err(malformed("field flags on other than an enum"));
        };
        let fa = self.b.slot_addr(slot);
        let mask = self.b.load(Ty::I64, Flags::NONE, fa, 0, MemAttrs::aligned(3));
        let exit = self.create();
        let mut cases = Vec::with_capacity_in(len as usize, self.alloc);
        let mut bodies = Vec::with_capacity_in(len as usize, self.alloc);
        for v in 0..len {
            let (ff, fl) = self.layout_variants[(first + v) as usize];
            if !self.fields_need_drop(ff, fl) {
                continue;
            }
            let block = self.create();
            cases.push((self.discriminant_case(id, v)?, block));
            bodies.push((block, v, ff, fl));
        }
        let d = self.read_discriminant(id, at)?;
        self.switch(d, exit, &cases);
        for &(block, v, ff, fl) in bodies.iter() {
            self.begin(block);
            for j in 0..fl {
                let (_, off, repr) = self.layout_fields[(ff + j) as usize];
                let Repr::Agg(fr) = repr else { continue };
                if !self.layouts[fr as usize].needs_drop {
                    continue;
                }
                let ft = self.layouts[fr as usize].ty;
                // A field past 64 is never moved (`arm` refuses it), so always held.
                let Some(bit) = self.field_bit(l, v, j)? else {
                    self.drop_at(ft, (at.0, at.1 + off as i32))?;
                    continue;
                };
                let one = self.b.iconst(Ty::I64, 1u128 << bit);
                let held = self.b.binary(Op::And, Flags::NONE, mask, one);
                let zero = self.b.iconst(Ty::I64, 0);
                let set = self.b.icmp(IntCond::Ne, held, zero);
                let yes = self.create();
                let next = self.create();
                self.br(set, (yes, &[]), (next, &[]));
                self.begin(yes);
                self.drop_at(ft, (at.0, at.1 + off as i32))?;
                self.jump(next, &[]);
                self.begin(next);
            }
            self.jump(exit, &[]);
        }
        self.begin(exit);
        Ok(())
    }

    /// Drops what an arm left of a temporary scrutinee (`Held::Rest`): its variant is known,
    /// so its fields not moved out are dropped with no flag; nothing moved, the whole value.
    fn drop_rest(&mut self, at: (Value, i32), l: u32, variant: Option<u32>, moved: u64) -> Result<(), BuildError> {
        let Some(v) = variant else {
            let g = self.layouts[l as usize].ty;
            return self.drop_at(g, at);
        };
        let (ff, fl, _) = self.variant_rows(l, v)?;
        for j in 0..fl {
            let (_, off, repr) = self.layout_fields[(ff + j) as usize];
            let Repr::Agg(fr) = repr else { continue };
            // A field past 64 is never moved (`arm` refuses it).
            let gone = self.field_bit(l, v, j)?.is_some_and(|bit| moved & (1 << bit) != 0);
            if self.layouts[fr as usize].needs_drop && !gone {
                let ft = self.layouts[fr as usize].ty;
                self.drop_at(ft, (at.0, at.1 + off as i32))?;
            }
        }
        Ok(())
    }

    /// Drops drop-scope entry `h`: a binding (`drop_local`), or an arm's rest.
    fn drop_held(&mut self, h: Held, kill: bool) -> Result<(), BuildError> {
        match h {
            Held::Pat(p) => self.drop_local(p, kill),
            Held::Rest { at, l, variant, moved } => self.drop_rest(at, l, variant, moved),
            Held::Moved { at, g, mask } => self.drop_moved(g, at, mask, false),
            Held::Temp { at, l, flag } => self.drop_temp(at, l, flag),
            Held::EnvPlace(j) => self.drop_env_place(j, kill),
        }
    }

    /// Every binding of pattern `p`, just bound, joins the innermost drop scope, in order.
    fn own_pat(&mut self, p: PatId) -> Result<(), BuildError> {
        let mut todo = Vec::new_in(self.alloc);
        todo.push(p);
        while let Some(p) = todo.pop() {
            let subs = match self.pat(p)? {
                Pat::Bind { sub, .. } => {
                    self.own(p, true)?;
                    todo.extend(sub);
                    continue;
                }
                Pat::Ref { inner, .. } => {
                    todo.push(inner);
                    continue;
                }
                Pat::Struct { fields, .. } => {
                    let fps = self.body.field_pats.get(fields.range()).ok_or(malformed("a struct pattern's fields past `field_pats`"))?;
                    todo.extend(fps.iter().rev().map(|fp| fp.pat));
                    continue;
                }
                Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Slice { elems, .. } => elems,
                _ => continue,
            };
            let subs = self.body.pat_lists.get(subs.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?;
            todo.extend(subs.iter().rev().copied());
        }
        Ok(())
    }

    /// Stores `on` into binding `p`'s flag, if it has one.
    fn set_flag(&mut self, p: PatId, on: bool) {
        if let Some(Owned { flag: Some(slot), .. }) = self.owned.get(p.ix()).copied().and_then(LocalOwn::as_drop) {
            let at = self.b.slot_addr(slot);
            let v = self.b.iconst(Ty::I8, u128::from(on));
            self.b.store(Flags::NONE, v, at, 0, MemAttrs::aligned(0));
        }
    }

    /// Drops binding `p` if it is initialized: statically, or by its flag (cleared on the way,
    /// so a scope entered again starts clean). `kill`: its scope ends, it is not initialized
    /// after (an early exit leaves the state of the code that follows it as it was).
    fn drop_local(&mut self, p: PatId, kill: bool) -> Result<(), BuildError> {
        let Some(o) = self.owned.get(p.ix()).copied().and_then(LocalOwn::as_drop) else { return Ok(()) };
        let (home, _) = self.slots[p.ix()].ok_or(malformed("an owned binding without a home"))?;
        match o.flag {
            None => {
                if self.init[p.ix()] {
                    let at = self.home(home);
                    self.drop_owned(o.ty, at, o.rest)?;
                }
                if kill {
                    self.init[p.ix()] = false;
                }
            }
            Some(slot) => {
                let fa = self.b.slot_addr(slot);
                let f = self.b.load(Ty::I8, Flags::NONE, fa, 0, MemAttrs::aligned(0));
                let zero = self.b.iconst(Ty::I8, 0);
                let set = self.b.icmp(IntCond::Ne, f, zero);
                let yes = self.create();
                let join = self.create();
                self.br(set, (yes, &[]), (join, &[]));
                self.begin(yes);
                let at = self.home(home);
                self.drop_owned(o.ty, at, o.rest)?;
                self.set_flag(p, false);
                self.jump(join, &[]);
                self.begin(join);
            }
        }
        Ok(())
    }

    /// Where drop scope `depth` starts in `scope_pats` (its end when it is not open).
    fn scope_start(&self, depth: u32) -> usize {
        self.scope_marks.get(depth as usize).map_or(self.scope_pats.len(), |&m| m as usize)
    }

    /// Whether a binding from `start` of `scope_pats` on may be dropped here.
    fn pending_from(&self, start: usize) -> bool {
        self.scope_pats[start..].iter().any(|&h| self.held_pending(h))
    }

    /// Drops the entries from `start` of `scope_pats` on, last declared first, for an exit
    /// that leaves their scopes (`break`, `continue`, `return`), each with its unwind ladder
    /// (walk/unwind_ladder.rs `drop_scope`).
    fn drop_from(&mut self, start: usize) -> Result<(), BuildError> {
        self.drop_scope(start, false)
    }

    /// A value about to outlive the drops of the bindings from `start` on: an aggregate place is
    /// copied out first, as it may be one of theirs (a field of a binding the drop changes).
    fn settle(&mut self, v: V, repr: Repr, start: usize) -> Result<V, BuildError> {
        if !self.pending_from(start) {
            return Ok(v);
        }
        match (v, repr) {
            (V::Place(..), Repr::Agg(_)) => {
                let at = self.temp(repr);
                self.store(v, repr, at)?;
                Ok(V::Place(at.0, at.1))
            }
            _ => Ok(v),
        }
    }

    /// Binding `p` is used by value: a move, when its type needs drop.
    fn moved(&mut self, e: ExprId, p: PatId) -> Result<(), BuildError> {
        let Some(o) = self.owned.get(p.ix()).copied().and_then(LocalOwn::as_drop) else { return Ok(()) };
        if o.flag.is_some() {
            self.set_flag(p, false);
            return Ok(());
        }
        if !self.init[p.ix()] && self.live {
            return Err(unsupported(e, "a use of a value already moved or never assigned (borrowck's E0382)"));
        }
        self.init[p.ix()] = false;
        Ok(())
    }

    /// The binding place expression `e` names, through parentheses.
    fn local_of(&self, e: ExprId) -> Result<Option<PatId>, BuildError> {
        let mut e = e;
        loop {
            match self.expr(e)? {
                Expr::Paren(inner) => e = inner,
                Expr::Local(p) => return Ok(Some(p)),
                _ => return Ok(None),
            }
        }
    }

    /// Whether binding `p` is a WHOLE binding the closure or coroutine this walk builds
    /// captures (an environment field with no projection, bound by [`Walk::bind_capture_at`]),
    /// so its home is the capture's, not a slot of this body.
    fn captured_whole(&self, p: PatId) -> bool {
        self.captured.contains(&p)
    }

    /// Whether `e` is a place expression (a local, a field, an index, a dereference), through
    /// parentheses.
    fn is_place_expr(&self, e: ExprId) -> Result<bool, BuildError> {
        let mut e = e;
        loop {
            match self.expr(e)? {
                Expr::Paren(inner) => e = inner,
                Expr::Local(_) | Expr::Field { .. } | Expr::TupleField { .. } | Expr::Index { .. } => return Ok(true),
                Expr::Unary { op: UnOp::Deref, .. } => return Ok(true),
                Expr::Path(_) => return Ok(matches!(self.expr_def(e), Some(Def::Static(..)))),
                _ => return Ok(false),
            }
        }
    }

    /// Which bindings get a drop flag: those with a state change (a use by value, an
    /// assignment) under a branch or a loop between it and the block that declares the
    /// binding. A use is by value unless its context is a place's (a borrow, a field's or an
    /// index's base, a compound assignment's left side, an autoref'd receiver); a context not
    /// known to be a place's counts, so the scan errs toward a flag, never away from one.
    fn scan_flags(&mut self) -> Result<(), BuildError> {
        let body = self.body;
        let n = body.exprs.len();
        // Per expression: its parent and whether the edge to it is conditional.
        let mut parent: Vec<(u32, bool), A> = Vec::with_capacity_in(n, self.alloc);
        parent.resize(n, (NO_EXPR, false));
        let mut decl: Vec<u32, A> = Vec::with_capacity_in(body.pats.len(), self.alloc);
        decl.resize(body.pats.len(), NO_EXPR);
        let mut kids: Vec<(ExprId, bool), A> = Vec::new_in(self.alloc);
        let mut pats: Vec<PatId, A> = Vec::new_in(self.alloc);
        let list = |l: List| body.expr_lists.get(l.range()).ok_or(malformed("a list past `expr_lists`"));
        for (i, x) in body.exprs.iter().enumerate() {
            kids.clear();
            pats.clear();
            match *x {
                Expr::Missing | Expr::Lit(_) | Expr::Local(_) | Expr::Path(_) | Expr::Continue { .. } => {}
                Expr::Paren(c) | Expr::Loop { body: c } => kids.push((c, matches!(x, Expr::Loop { .. }))),
                Expr::Block { stmts, tail, .. } => {
                    let ss = body.stmt_lists.get(stmts.range()).ok_or(malformed("a block's statements past `stmt_lists`"))?;
                    for &s in ss {
                        match body.stmts.get(s.ix()).copied().ok_or(malformed("a statement past `stmts`"))? {
                            Stmt::Let { pat, init, else_, .. } => {
                                kids.extend(init.map(|c| (c, false)));
                                kids.extend(else_.map(|c| (c, true)));
                                pats.push(pat);
                                // A `let`-`else`, and a `let` that owns its initializer's parts
                                // (matches/irrefutable.rs `let_owns_parts`), owns its scrutinee as
                                // a match does: a local, or a field of one, gets the local's field
                                // flags (walk/moves.rs `scan_local_scrutinee`,
                                // walk/moves/field_scrutinee.rs).
                                if let Some(i) = init
                                    && (else_.is_some() || self.let_owns_parts(pat, i)?)
                                {
                                    self.scan_field_scrutinee(i, pat)?;
                                    self.scan_local_scrutinee(i, pat)?;
                                }
                            }
                            Stmt::Expr { expr, .. } => kids.push((expr, false)),
                            Stmt::Item => {}
                        }
                    }
                    kids.extend(tail.map(|c| (c, false)));
                }
                Expr::If { cond, then, else_ } => {
                    kids.push((cond, false));
                    kids.push((then, true));
                    kids.extend(else_.map(|c| (c, true)));
                }
                // As a `let` statement: its initializer, then its pattern's bindings. A field
                // of an owned local as its scrutinee gets the local's field flags, as a
                // `let`-`else`'s does (walk/moves/field_scrutinee.rs; walk/lets.rs `let_link`).
                Expr::Let { pat, init } => {
                    kids.push((init, false));
                    pats.push(pat);
                    self.scan_field_scrutinee(init, pat)?;
                    self.scan_local_scrutinee(init, pat)?;
                }
                Expr::Match { scrutinee, arms } => {
                    kids.push((scrutinee, false));
                    // A local scrutinee: an arm binding all of it moves it (a flag); an arm
                    // binding a variant's field by value moves that field (field flags).
                    let mut s = scrutinee;
                    while let Some(&Expr::Paren(inner)) = body.exprs.get(s.ix()) {
                        s = inner;
                    }
                    let local = match body.exprs.get(s.ix()) {
                        Some(&Expr::Local(p)) => Some(p),
                        _ => None,
                    };
                    for arm in body.arms.get(arms.range()).ok_or(malformed("a match's arms past `arms`"))? {
                        if let Some(x) = local {
                            let by_value = |p: PatId| matches!(body.pats.get(p.ix()), Some(Pat::Bind { ann, .. }) if ann.by_ref.is_none());
                            let subs: &[PatId] = match body.pats.get(arm.pat.ix()) {
                                Some(Pat::Bind { ann, .. }) if ann.by_ref.is_none() => {
                                    self.needs_flag[x.ix()] = true;
                                    &[]
                                }
                                Some(&Pat::TupleStruct { elems, .. }) => {
                                    body.pat_lists.get(elems.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?
                                }
                                _ => &[],
                            };
                            let fields_by_value = match body.pats.get(arm.pat.ix()) {
                                Some(&Pat::Struct { fields, .. }) => body
                                    .field_pats
                                    .get(fields.range())
                                    .ok_or(malformed("a struct pattern's fields past `field_pats`"))?
                                    .iter()
                                    .any(|fp| by_value(fp.pat)),
                                _ => false,
                            };
                            // A deeper by-value binding moves through a match by tests
                            // (walk/moves.rs), by the same field flags.
                            if fields_by_value || subs.iter().any(|&p| by_value(p)) || self.moves_deep(arm.pat)? {
                                self.partial[x.ix()] = true;
                            }
                        }
                        // A field of a local (`match self.failed`): field flags when an arm
                        // may move a part out (walk/moves/field_scrutinee.rs).
                        self.scan_field_scrutinee(scrutinee, arm.pat)?;
                        kids.extend(arm.guard.map(|g| (g, true)));
                        kids.push((arm.body, true));
                        // An arm's bindings are declared by its body (a use in its guard
                        // crosses the arm's edge and counts as conditional: a flag, safely).
                        self.mark_decl(arm.pat, arm.body.0, &mut decl)?;
                    }
                }
                Expr::Break { value, .. } | Expr::Return { value } => kids.extend(value.map(|c| (c, false))),
                Expr::Call { callee, args } => {
                    kids.push((callee, false));
                    kids.extend(list(args)?.iter().map(|&c| (c, false)));
                }
                Expr::MethodCall { receiver, args, .. } => {
                    kids.push((receiver, false));
                    kids.extend(list(args)?.iter().map(|&c| (c, false)));
                }
                Expr::Field { base: c, .. }
                | Expr::TupleField { base: c, .. }
                | Expr::Unary { operand: c, .. }
                | Expr::Ref { operand: c, .. }
                | Expr::Cast { operand: c, .. }
                | Expr::Repeat { elem: c, .. } => kids.push((c, false)),
                Expr::Binary { op, lhs, rhs } => {
                    kids.push((lhs, false));
                    kids.push((rhs, matches!(op, BinOp::And | BinOp::Or)));
                }
                Expr::Assign { lhs, rhs } | Expr::AssignOp { lhs, rhs, .. } | Expr::Index { base: lhs, index: rhs } => {
                    kids.push((lhs, false));
                    kids.push((rhs, false));
                }
                Expr::Tuple { elems } | Expr::Array { elems } => kids.extend(list(elems)?.iter().map(|&c| (c, false))),
                Expr::Struct { fields, base, .. } | Expr::Record { fields, base } => {
                    let inits = body.field_inits.get(fields.range()).ok_or(malformed("a struct literal's fields past `field_inits`"))?;
                    kids.extend(inits.iter().map(|f| (f.expr, false)));
                    kids.extend(base.expr().map(|c| (c, false)));
                }
                Expr::Closure { body: c, .. } => kids.push((c, true)),
                Expr::FormatArgs { args, .. } => kids.extend(list(args)?.iter().map(|&c| (c, false))),
                Expr::Asm { operands, .. } => asm::asm_kids(body, operands, &mut kids)?,
                Expr::Await(c) => kids.push((c, false)),
                Expr::AsyncBlock { body: c, .. } => kids.push((c, true)),
                Expr::Underscore | Expr::OffsetOf { .. } => {}
                Expr::RawRef { operand: c, .. } => kids.push((c, false)),
                // An inline constant is its own body: its value, not its code, is this one's.
                Expr::ConstBlock { .. } => {}
            }
            for &(c, cond) in kids.iter() {
                *parent.get_mut(c.ix()).ok_or(malformed("an expression past `exprs`"))? = (i as u32, cond);
            }
            for &p in pats.iter() {
                self.mark_decl(p, i as u32, &mut decl)?;
            }
        }
        for (i, x) in body.exprs.iter().enumerate() {
            let Expr::Local(p) = *x else { continue };
            // The context: the parent of the outermost parenthesis around the use.
            let mut c = i as u32;
            while let (up, _) = parent[c as usize]
                && up != NO_EXPR
                && let Expr::Paren(_) = body.exprs[up as usize]
            {
                c = up;
            }
            let up = parent[c as usize].0;
            if self.use_keeps_state(c, up) {
                continue;
            }
            let home = *decl.get(p.ix()).ok_or(malformed("a local past `pats`"))?;
            let mut c = i as u32;
            while c != home {
                let (up, cond) = parent[c as usize];
                if cond {
                    self.needs_flag[p.ix()] = true;
                    break;
                }
                if up == NO_EXPR {
                    break;
                }
                c = up;
            }
        }
        // A part moved out by a place expression (a field, a box's contents) gives its local
        // field flags (walk/moves/boxes.rs).
        self.scan_part_moves(&parent)?;
        // A part moved out by a closure's by-value capture of a place below its binding
        // likewise (walk/moves/env_places.rs).
        self.scan_env_moves()?;
        // A field taken out of a struct update's base likewise (walk/moves/struct_update.rs).
        self.scan_update_moves()?;
        self.expr_parent = parent;
        self.scan_temp_scopes()
    }

    /// Whether the use of place expression `c` (the outermost parenthesis around it), whose
    /// parent is `up` (`NO_EXPR`: none), keeps the place's drop state: a first adjustment that
    /// borrows it (an autoref, an overloaded dereference's `&self`), wherever it stands, or a
    /// parent that takes a place (a borrow, a field's or an index's base, a compound
    /// assignment's left side, a comparison's operand, a `match` or `let` scrutinee, a struct
    /// update's base). Anything else changes it: a move, and an assignment's left side (which
    /// makes it held). The rule of a drop flag: `scan_flags` (a local's) and
    /// walk/moves/env_places.rs (an owned environment field's). Not walk/moves/boxes.rs
    /// `used_in_place`, which asks whether a use MOVES a part (an assignment's left side does
    /// not; adjustments are its caller's).
    fn use_keeps_state(&self, c: u32, up: u32) -> bool {
        let rows = self.adjust_rows(ExprId(c));
        let borrowed_first = matches!(
            self.checked.adjusts.column::<1>().get(rows.start).filter(|_| !rows.is_empty()),
            Some(AdjustKind::Borrow { .. } | AdjustKind::Deref { overloaded: Some(_) })
        );
        borrowed_first
            || up != NO_EXPR
                && match self.body.exprs[up as usize] {
                    Expr::Ref { operand, .. } | Expr::RawRef { operand, .. } => operand.0 == c,
                    Expr::Field { base, .. } | Expr::TupleField { base, .. } | Expr::Index { base, .. } => base.0 == c,
                    Expr::AssignOp { lhs, .. } => lhs.0 == c,
                    // [expr.cmp.place]: a comparison's operands are borrowed in place.
                    Expr::Binary { op, lhs, rhs } if operators::comparison(op) => lhs.0 == c || rhs.0 == c,
                    // A place expression: matched in place, its arms' moves are marked below.
                    Expr::Match { scrutinee, .. } => scrutinee.0 == c,
                    // A `let` condition's scrutinee likewise (walk/lets.rs: nothing moves out).
                    Expr::Let { init, .. } => init.0 == c,
                    // A struct update's base: a place, only its taken fields move
                    // (walk/moves/struct_update.rs).
                    Expr::Struct { base: blaze_typeck::StructBase::Expr(b), .. } => b.0 == c,
                    _ => false,
                }
    }

    /// Every binding of pattern `p` is declared by expression `by`.
    fn mark_decl(&self, p: PatId, by: u32, decl: &mut Vec<u32, A>) -> Result<(), BuildError> {
        let mut todo: Vec<PatId, A> = Vec::new_in(self.alloc);
        todo.push(p);
        while let Some(p) = todo.pop() {
            *decl.get_mut(p.ix()).ok_or(malformed("a pattern past `pats`"))? = by;
            let subs = match self.pat(p)? {
                Pat::Bind { sub, .. } => {
                    todo.extend(sub);
                    continue;
                }
                Pat::Ref { inner, .. } => {
                    todo.push(inner);
                    continue;
                }
                Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Slice { elems, .. } | Pat::Or { alts: elems } => elems,
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                    let fps = self.body.field_pats.get(fields.range()).ok_or(malformed("a struct pattern's fields past `field_pats`"))?;
                    todo.extend(fps.iter().map(|f| f.pat));
                    continue;
                }
                Pat::Missing | Pat::Wild | Pat::Lit(_) | Pat::NegLit(_) | Pat::Path(_) | Pat::Range { .. } => continue,
            };
            todo.extend_from_slice(self.body.pat_lists.get(subs.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?);
        }
        Ok(())
    }

    /// The body of `drop_in_place::<T>`, `T` the instance's argument, its parameter `p`: `T`'s
    /// drop glue (lib.rs, "Drops").
    fn glue(&mut self, p: Value) -> Result<(), BuildError> {
        let g = *self.args.first().ok_or(malformed("`drop_in_place` without its type argument"))?;
        let sig_err = |error: TyErr| match error {
            TyErr::Unsupported(what) => BuildError::UnsupportedSig { what },
            TyErr::Malformed(what) => malformed(what),
            TyErr::Other(error) => error,
        };
        // A coroutine's drop is the pass's `drop` of its split body, never glue over fields.
        if coroutine::coroutine_of(self.ground.view(), g).is_some() {
            return Err(BuildError::UnsupportedSig { what: "a coroutine's drop glue asked of `build_drop_glue` (it is `build_coroutine`'s `drop`)" });
        }
        if !self.needs_drop(g).map_err(sig_err)? {
            return Ok(());
        }
        let l = self.layout(g).map_err(sig_err)? as usize;
        // A `Box<T>`'s contents go first, then its `Drop` impl (the library's, which frees)
        // and its own fields: that second half alone is the drop of a box whose contents moved
        // out (walk/moves/boxes.rs `drop_box_free`).
        if let PNode::Adt(k, list) = self.ground_node(g).map_err(sig_err)?
            && self.adts.lang_adt(LangItem::OwnedBox) == Some(k)
        {
            // The contents, then the free: the free is owed if the contents' glue unwinds
            // (walk/unwind_ladder.rs).
            return self.drops_with_ladder(
                &[unwind_ladder::DropStep::BoxContents { list, p }, unwind_ladder::DropStep::BoxFree { g, p }],
                false,
            );
        }
        // Its own `Drop`, then its parts (fields; the live variant's fields; elements), each
        // with its unwind ladder (walk/unwind_ladder.rs `drop_impl_then_parts`, `drop_parts`).
        self.drop_impl_then_parts(g, l, p)
    }

    /// The contents of the `Box<T>` at `p` (`list` its arguments), dropped by its glue before
    /// the `Box`'s own `Drop` impl, which frees the allocation (std's `Box`: "the compiler drops
    /// the contents, then calls `Drop::drop`", which deallocates by `Layout::for_value_raw`, so
    /// a zero-sized `T`, never allocated, is never freed). The box's first word is the data
    /// pointer (its pointer field, nested or not, is at offset 0); for `T = dyn Tr` the second
    /// is the table, whose word 0 is `T`'s glue ([`Self::dyn_glue`]). For `T = [U]` or `str`
    /// the box is the wide pointer itself (data at 0, length at 8): its elements' glue, first to
    /// last (walk/slice_glue.rs; nothing for `str` or a `U` that needs no drop); the box's
    /// `Drop` then frees `len * size_of::<U>()` bytes, as std's does by `Layout::for_value_raw`.
    fn drop_box_contents(&mut self, list: List, p: Value) -> Result<(), BuildError> {
        let sig_err = |error: TyErr| match error {
            TyErr::Unsupported(what) => BuildError::UnsupportedSig { what },
            TyErr::Malformed(what) => malformed(what),
            TyErr::Other(error) => error,
        };
        let t = *self
            .ground
            .view()
            .list(list)
            .map_err(|_| malformed("a `Box`'s arguments past the ground table"))?
            .first()
            .ok_or(malformed("a `Box` without its type argument"))?;
        match self.ground_node(t).map_err(sig_err)? {
            PNode::Dynamic(_) => {
                let data = self.b.load(Ty::PTR, Flags::NONE, p, 0, MemAttrs::aligned(3));
                let vt = self.b.load(Ty::PTR, Flags::NONE, p, 8, MemAttrs::aligned(3));
                self.dyn_glue(data, vt)?;
            }
            PNode::Slice(_) | PNode::Str => self.slice_glue(t, p)?,
            _ => {
                if self.needs_drop(t).map_err(sig_err)? {
                    let data = self.b.load(Ty::PTR, Flags::NONE, p, 0, MemAttrs::aligned(3));
                    self.drop_at(t, (data, 0))?;
                }
            }
        }
        Ok(())
    }

    /// The drop of the trait object at `data` whose table is `vt` (vtable.rs: word 0 is the
    /// concrete type's `drop_in_place`, 0 when it needs none): a call of word 0 with `data`,
    /// skipped when the word is 0.
    fn dyn_glue(&mut self, data: Value, vt: Value) -> Result<(), BuildError> {
        let word = self.b.load(Ty::I64, Flags::NONE, vt, 0, MemAttrs::aligned(3));
        let zero = self.b.iconst(Ty::I64, 0);
        let set = self.b.icmp(IntCond::Ne, word, zero);
        let yes = self.create();
        let next = self.create();
        self.br(set, (yes, &[]), (next, &[]));
        self.begin(yes);
        let f = self.b.load(Ty::PTR, Flags::NONE, vt, 0, MemAttrs::aligned(3));
        let sig = self.b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
        self.drop_call(Callee::Pointer(f), sig, &[data])?;
        self.jump(next, &[]);
        self.begin(next);
        Ok(())
    }

    /// Whether any of `layout_fields[first..first + len]` needs drop.
    fn fields_need_drop(&self, first: u32, len: u32) -> bool {
        self.layout_fields[first as usize..(first + len) as usize].iter().any(|&(_, _, repr)| self.repr_needs_drop(repr))
    }

    // The function.

    fn run(&mut self, sig: &FnSig<'_>, kind: Kind) -> Result<(), BuildError> {
        // A method's `self` is its first input and its body's first parameter, like any other.
        if sig.inputs.len() != self.body.params.len() {
            return Err(malformed("a signature and a body with other numbers of parameters"));
        }
        let sig_err = |error: TyErr| match error {
            TyErr::Unsupported(what) => BuildError::UnsupportedSig { what },
            TyErr::Malformed(what) => malformed(what),
            TyErr::Other(error) => error,
        };
        // Drop glue is of the Rust ABI; a body is of its fn's.
        let owner = self.body.owner;
        let abi = match kind {
            Kind::DropGlue => Abi::Rust,
            Kind::Body => self.adts.abi(owner).ok_or(BuildError::NoAbi { item: owner })?,
            Kind::Closure(..) => return Err(malformed("a closure's body built as a fn's (closure.rs builds it)")),
        };
        let mut reprs = Vec::with_capacity_in(sig.inputs.len(), self.alloc);
        let mut passes = Vec::with_capacity_in(sig.inputs.len(), self.alloc);
        // "rust-call": the last parameter, a tuple, arrives as its fields (lib.rs `Abi`).
        let mut spread = Vec::new_in(self.alloc);
        for (i, &t) in sig.inputs.iter().enumerate() {
            let g = self.ground_in(sig.types, t).map_err(sig_err)?;
            let repr = self.repr(g).map_err(sig_err)?;
            reprs.push(repr);
            if abi == Abi::RustCall && i + 1 == sig.inputs.len() && kind == Kind::Body {
                spread = self.rust_call_fields(repr)?;
                for &(_, frepr) in spread.iter() {
                    passes.push(self.rust_pass(frepr)?);
                }
            } else {
                passes.push(self.pass(g, repr, abi, Some(owner))?);
            }
        }
        // An `async fn` returns its coroutine, whatever its signature writes (coroutine.rs).
        let g = match self.async_fn_output()? {
            Some(g) => g,
            None => self.ground_in(sig.types, sig.output).map_err(sig_err)?,
        };
        self.ret = self.repr(g).map_err(sig_err)?;
        self.ret_pass = self.pass(g, self.ret, abi, Some(owner))?;
        // A `#[track_caller]` fn's hidden last parameter, its caller's `Location`
        // (walk/track_caller.rs); no parameter of the body binds it.
        let tracked = kind == Kind::Body && self.own_tracked()?;
        if tracked {
            passes.push(track_caller::PASS);
        }
        let (params, rets) = self.sig_of(&passes, self.ret_pass);
        let own = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        self.b.set_signature(own);
        self.mark_nounwind(owner, abi, kind == Kind::Body)?;
        let entry = self.create();
        self.reached[entry.index()] = true;
        self.begin(entry);
        let mut values = Vec::with_capacity_in(params.len(), self.alloc);
        for p in params.iter() {
            values.push(self.b.param(p.ty));
        }
        if tracked {
            let loc = *values.last().ok_or(malformed("a `#[track_caller]` fn without its location parameter"))?;
            self.track = track_caller::Track::Passed(loc);
        }
        let mut next = values.iter().copied();
        if let Pass::Ref | Pass::Memory { .. } = self.ret_pass {
            self.sret = Some(next.next().ok_or(malformed("a result pointer without its value"))?);
        }
        if kind == Kind::DropGlue {
            // Reconstitute the wide pointer exactly as for an ordinary parameter.
            let g = *self.args.first().ok_or(malformed("`drop_in_place` without its type argument"))?;
            if let (&[repr @ Repr::Agg(_)], &[pass], Repr::None) = (reprs.as_slice(), passes.as_slice(), self.ret) {
                let value = self.take_passed(repr, pass, &mut next)?;
                let at = self.place(value, repr)?;
                let q = self.address(at);
                if matches!(self.ground_node(g).map_err(sig_err)?, PNode::Dynamic(_)) {
                    let data = self.b.load(Ty::PTR, Flags::NONE, q, 0, MemAttrs::aligned(3));
                    let vt = self.b.load(Ty::PTR, Flags::NONE, q, 8, MemAttrs::aligned(3));
                    self.dyn_glue(data, vt)?;
                } else if self.is_slice_pointee(g)? {
                    self.slice_glue(g, q)?;
                } else {
                    let l = self.layout(g).map_err(sig_err)?;
                    if !self.has_unsized_tail(l) {
                        return Err(malformed("a wide drop-glue parameter without an unsized pointee"));
                    }
                    if self.needs_drop(g).map_err(sig_err)? {
                        return Err(BuildError::UnsupportedSig {
                            what: "the drop glue of a struct or tuple with an unsized tail that needs drop (its prefix fields, then its slice tail's elements or its `dyn` tail at the vtable's alignment)",
                        });
                    }
                }
                self.b.ret(&[]);
                if let Some(error) = self.b.error() {
                    return Err(BuildError::Lir(error.clone()));
                }
                return Ok(());
            }
            let (&[Repr::Ptr], Repr::None, &[p]) = (reprs.as_slice(), self.ret, values.as_slice()) else {
                return Err(malformed("a `drop_glue` parameter that is neither a thin pointer, a slice, `str` nor `dyn`"));
            };
            self.glue(p)?;
            self.b.ret(&[]);
            if let Some(error) = self.b.error() {
                return Err(BuildError::Lir(error.clone()));
            }
            return Ok(());
        }
        // The parameters' drop scope, the outermost.
        self.scope_marks.push(0);
        for (i, &repr) in reprs.iter().enumerate() {
            let p = self.body.params[i];
            if abi == Abi::RustCall && i + 1 == reprs.len() {
                // The tuple is put back together in a slot of its own from its fields: each
                // field's register is stored, an aggregate field copied from the caller's copy.
                let v = if spread.is_empty() {
                    V::None
                } else {
                    let at = self.temp(repr);
                    for (j, &(off, frepr)) in spread.iter().enumerate() {
                        let fv = self.take_passed(frepr, passes[i + j], &mut next)?;
                        self.store(fv, frepr, (at.0, at.1 + off as i32))?;
                    }
                    V::Place(at.0, at.1)
                };
                match v {
                    V::Place(at, off) if matches!(self.pat(p)?, Pat::Bind { .. }) => {
                        self.bind_at(p, (at, off))?;
                        self.own_pat(p)?;
                    }
                    // walk/matches/irrefutable.rs `bind_param`: binds and owns.
                    v => {
                        let g = self.ground_in(sig.types, sig.inputs[i]).map_err(sig_err)?;
                        self.bind_param(p, v, g)?
                    }
                }
                continue;
            }
            // The caller's copy (or the one a C parameter's registers are written to) is the
            // callee's to keep: a binding lives in it.
            match self.take_passed(repr, passes[i], &mut next)? {
                V::Place(at, off) if matches!(self.pat(p)?, Pat::Bind { .. }) => {
                    self.bind_at(p, (at, off))?;
                    self.own_pat(p)?;
                }
                v => {
                    // The parameter's own type: a pattern that peels a reference
                    // (`fn f(Foo { a }: &Foo)`) is recorded at the peeled one. Bound and
                    // owned by walk/matches/irrefutable.rs `bind_param` (`_: String` too).
                    let g = self.ground_in(sig.types, sig.inputs[i]).map_err(sig_err)?;
                    self.bind_param(p, v, g)?
                }
            }
        }
        self.walk_root()
    }

    /// Walks the body's root to its value and returns it: the rest of a fn after its parameters
    /// are bound (`run`, closure.rs `run_closure`).
    fn walk_root(&mut self) -> Result<(), BuildError> {
        self.work.push(Task::Eval(self.body.root));
        while let Some(task) = self.work.pop() {
            self.note_task(&task);
            if let Some(e) = state_names::task_expr(&task) {
                self.task_at = Some(e);
            }
            self.step(task)?;
        }
        // The result's copy out is the root's.
        self.task_at = Some(self.body.root);
        let v = self.pop()?;
        if !self.vals.is_empty() {
            return Err(malformed("values left on the stack at the end (a build bug)"));
        }
        if self.live {
            self.ret_value(v)?;
        } else {
            self.unreachable_of(TrapKind::BodyEnd, Some(self.body.root));
        }
        if let Some(error) = self.b.error() {
            return Err(BuildError::Lir(error.clone()));
        }
        Ok(())
    }

    /// Adapt the walk's value storage to the layout's call convention.
    fn rust_pass(&self, repr: Repr) -> Result<Pass, BuildError> {
        Ok(match repr {
            Repr::None => Pass::Ignore,
            Repr::Agg(row) => {
                let row = &self.layouts[row as usize];
                self.ls.get(row.id).rust_pass(self.ground.view())
                    .map_err(|why| signature_error(layout_err(LayoutError { ty: row.ty, why })))?
            }
            _ => {
                let ty = repr.scalar().ok_or(malformed("a scalar of no type"))?;
                crate::layout::rust_scalar(ty, matches!(repr, Repr::Int { signed: true, .. }), false)
            }
        })
    }

    /// The fields of a "rust-call" fn's last parameter, a tuple held as `repr`, by their
    /// offsets and reprs (codegen.md, "Closures and the Fn traits"). A zero-sized tuple has
    /// none: it crosses the call as nothing.
    fn rust_call_fields(&self, repr: Repr) -> Result<Vec<(u32, Repr), A>, BuildError> {
        let mut out = Vec::new_in(self.alloc);
        let l = match repr {
            Repr::None => return Ok(out),
            Repr::Agg(l) => l,
            _ => return Err(malformed("a \"rust-call\" fn's last parameter is not a tuple")),
        };
        let Shape::Fields { first, len } = self.layouts[l as usize].shape else {
            return Err(malformed("a \"rust-call\" fn's last parameter is not a tuple"));
        };
        out.reserve(len as usize);
        for j in first..first + len {
            let (_, off, repr) = *self.layout_fields.get(j as usize).ok_or(malformed("a tuple's field past its layout"))?;
            out.push((off, repr));
        }
        Ok(out)
    }

    /// A call of a "rust-call" fn whose last argument is written as one tuple value (core's
    /// `(**self).call(args)`): that value, on top of the stack, is replaced by its fields, and
    /// its repr and pass in `reprs` and `passes` by theirs. A field of an aggregate is passed by
    /// its address inside the tuple: the tuple is the caller's copy already.
    fn spread_tuple(&mut self, reprs: &mut Vec<Repr, A>, passes: &mut Vec<Pass, A>) -> Result<(), BuildError> {
        let lost = || malformed("a \"rust-call\" call without its tuple of arguments");
        let repr = reprs.pop().ok_or_else(lost)?;
        passes.pop().ok_or_else(lost)?;
        if self.vals.is_empty() {
            return Err(lost());
        }
        let v = self.pop_val();
        let fields = self.rust_call_fields(repr)?;
        if fields.is_empty() {
            return Ok(());
        }
        let at = self.place(v, repr)?;
        for &(off, frepr) in fields.iter() {
            let to = (at.0, at.1 + off as i32);
            let fv = match frepr {
                Repr::None => V::None,
                Repr::Agg(_) => V::Place(to.0, to.1),
                _ => self.load(frepr, to),
            };
            self.push_val(fv, None);
            reprs.push(frepr);
            passes.push(self.rust_pass(frepr)?);
        }
        Ok(())
    }

    /// How a value of ground type `g`, held as `repr`, crosses a call of `abi` (lib.rs, "Rust
    /// ABI" and "The C ABI"); `item` is the fn a C refusal names (`None` through a pointer).
    fn pass(&mut self, g: PTy, repr: Repr, abi: Abi, item: Option<ItemKey>) -> Result<Pass, BuildError> {
        // "rust-call" passes each parameter (its tuple's fields too) as the Rust ABI does.
        if let Abi::Rust | Abi::RustCall = abi {
            return self.rust_pass(repr);
        }
        let refuse = |why| BuildError::CAbi { item, why };
        let adts = self.adts;
        let class = {
            let mut cx = LayoutCx { ground: &mut *self.ground, facts: adts.facts(), scratch: self.scratch };
            self.ls.of(g, &mut cx).map_err(CAbiError::Layout).and_then(|id| {
                let base = self.ls.get(id).call_ty;
                cabi::class_of(&mut self.ls, base, &mut cx, self.alloc)
            })
        };
        let class = class.map_err(|error| match error {
            CAbiError::Layout(error) => match layout_err(error) {
                TyErr::Unsupported(why) | TyErr::Malformed(why) => refuse(why),
                TyErr::Other(error) => error,
            },
            CAbiError::Refused { why, .. } => refuse(why),
            CAbiError::TooLarge => refuse("more than 2 GiB of stacked arguments"),
        })?;
        Ok(match class {
            Class::Ignore => Pass::Ignore,
            Class::Int { size: 16, .. } => return Err(refuse("a 128-bit integer (a register pair)")),
            // A scalar, or a one-scalar aggregate (a newtype) read and written at offset 0;
            // Apple widens an integer under 32 bits (`cabi::Ext`).
            Class::Int { size, signed } => {
                let ext = match (size < 4, signed) {
                    (true, true) => Ext::Sext,
                    (true, false) => Ext::Zext,
                    (false, _) => Ext::None,
                };
                match repr {
                    Repr::Agg(_) => {
                        let id = self.ls_of(g).map_err(|error| match error {
                            TyErr::Unsupported(why) | TyErr::Malformed(why) => refuse(why),
                            TyErr::Other(error) => error,
                        })?;
                        let s = self.ls.get(id).scalar.ok_or(malformed("a C integer class of a type with no scalar"))?;
                        Pass::Scalar { ty: prim_ty(s.prim), ext, agg: true }
                    }
                    _ => {
                        let ty = repr.scalar().ok_or(malformed("a scalar of no type"))?;
                        Pass::Scalar { ty, ext, agg: false }
                    }
                }
            }
            // AAPCS64 6.8 C.1 and 6.9: a float (half, single, double or quad precision) takes
            // one SIMD register, `v[NSRN]`, which the LIR float scalar of its type stands for
            // at the call; a one-float newtype is its float, read and written at offset 0.
            Class::Float { .. } => match repr {
                Repr::Agg(_) => {
                    let id = self.ls_of(g).map_err(|error| match error {
                        TyErr::Unsupported(why) | TyErr::Malformed(why) => refuse(why),
                        TyErr::Other(error) => error,
                    })?;
                    let s = self.ls.get(id).scalar.ok_or(malformed("a C float class of a type with no scalar"))?;
                    Pass::Scalar { ty: prim_ty(s.prim), ext: Ext::None, agg: true }
                }
                _ => {
                    let ty = repr.scalar().ok_or(malformed("a scalar of no type"))?;
                    Pass::Scalar { ty, ext: Ext::None, agg: false }
                }
            },
            Class::Hfa { .. } => return Err(refuse("a homogeneous float aggregate (the SIMD registers are not built)")),
            Class::Composite { size, align } if size > 16 => {
                let size = u32::try_from(size).map_err(|_| refuse("a composite past 4 GiB"))?;
                Pass::Memory { size, align }
            }
            // AAPCS64 C.8 starts it at an even register, which a run of `i64`s cannot say.
            Class::Composite { align: 4.., .. } => return Err(refuse("a 16-byte-aligned composite (an even register pair)")),
            Class::Composite { size, .. } => match self.rust_pass(repr)? {
                wide @ Pass::Wide { .. } => wide,
                _ => Pass::Words { size: size as u32 },
            },
        })
    }

    /// The lowered signature of passes `inputs` and `ret`: a result pointer first (Rust's plain
    /// pointer, C's `sret`), then each input's registers.
    fn sig_of(&self, inputs: &[Pass], ret: Pass) -> (Vec<Param, A>, Vec<Param, A>) {
        crate::abi::sig_of_in(inputs, ret, self.alloc)
    }

    /// The `size` (1 to 8) bytes at `at`, `start` bytes into a value aligned to `align`, as a
    /// zero-extended `i64`: loads of falling powers of two inside the size, so no byte past
    /// the value is read (12 bytes are an `i64`, then an `i32`).
    fn read_word(&mut self, at: (Value, i32), start: u32, size: u32, align: u8) -> Result<Value, BuildError> {
        let mut word = None;
        let mut off = 0u32;
        while off < size {
            let log = (31 - (size - off).leading_zeros()).min(3);
            let ty = WIDTHS[log as usize];
            let a = align.min(log as u8).min((start + off).trailing_zeros().min(7) as u8);
            let x = self.b.load(ty, Flags::NONE, at.0, at.1 + (start + off) as i32, MemAttrs::aligned(a));
            let mut x = self.resize(x, ty, Ty::I64, false);
            if off > 0 {
                let by = self.b.iconst(Ty::I64, u128::from(off * 8));
                x = self.b.binary(Op::Shl, Flags::NONE, x, by);
            }
            word = Some(match word {
                None => x,
                Some(w) => self.b.binary(Op::Or, Flags::NONE, w, x),
            });
            off += 1 << log;
        }
        word.ok_or(malformed("a C register of no bytes"))
    }

    /// Writes the low `size` (1 to 8) bytes of `i64` `word` at `at`, `start` bytes into a value
    /// aligned to `align`, in stores of falling powers of two inside the size.
    fn write_word(&mut self, word: Value, at: (Value, i32), start: u32, size: u32, align: u8) {
        let mut off = 0u32;
        while off < size {
            let log = (31 - (size - off).leading_zeros()).min(3);
            let ty = WIDTHS[log as usize];
            let a = align.min(log as u8).min((start + off).trailing_zeros().min(7) as u8);
            let mut x = word;
            if off > 0 {
                let by = self.b.iconst(Ty::I64, u128::from(off * 8));
                x = self.b.binary(Op::Lshr, Flags::NONE, x, by);
            }
            let x = self.resize(x, Ty::I64, ty, false);
            self.b.store(Flags::NONE, x, at.0, at.1 + (start + off) as i32, MemAttrs::aligned(a));
            off += 1 << log;
        }
    }

    /// The argument values of `v`, held as `repr`, passed as `p`, into `out`.
    fn pass_arg(&mut self, v: V, repr: Repr, p: Pass, out: &mut Vec<Value, A>) -> Result<(), BuildError> {
        match p {
            Pass::Ignore => {}
            Pass::Scalar { ty, agg: false, .. } => out.push(self.scalar(v, ty)?),
            Pass::Scalar { ty, agg: true, .. } => {
                let at = self.place(v, repr)?;
                let attrs = self.mem(repr);
                out.push(self.b.load(ty, Flags::NONE, at.0, at.1, attrs));
            }
            // `ArgCopy` made the caller's copy; a `byval` one is copied again by the call.
            Pass::Ref | Pass::Memory { .. } => {
                let at = self.place(v, repr)?;
                out.push(self.address(at));
            }
            Pass::Wide { metadata } => {
                let (ptr, off) = self.place(v, repr)?;
                let attrs = self.mem(repr);
                out.push(self.b.load(Ty::PTR, Flags::NONE, ptr, off, attrs));
                out.push(self.b.load(metadata, Flags::NONE, ptr, off + 8, attrs));
            }
            Pass::Words { size } => {
                let at = self.place(v, repr)?;
                let align = self.size_align(repr).1;
                for k in 0..size.div_ceil(8) {
                    out.push(self.read_word(at, k * 8, (size - k * 8).min(8), align)?);
                }
            }
        }
        Ok(())
    }

    /// A value of `repr`, passed as `p`, from the values `next` yields: a parameter in the
    /// callee, or a call's results in the caller.
    fn take_passed(&mut self, repr: Repr, p: Pass, next: &mut dyn Iterator<Item = Value>) -> Result<V, BuildError> {
        let mut one = || next.next().ok_or(malformed("a passed value without its register"));
        Ok(match p {
            Pass::Ignore if matches!(repr, Repr::Agg(_)) => {
                // A ZST with a destructor still needs an aligned place in this frame.
                let (ptr, off) = self.temp(repr);
                V::Place(ptr, off)
            }
            Pass::Ignore => V::None,
            Pass::Scalar { agg: false, .. } => V::Scalar(one()?),
            Pass::Scalar { agg: true, .. } => {
                let x = one()?;
                let at = self.temp(repr);
                let attrs = self.mem(repr);
                self.b.store(Flags::NONE, x, at.0, at.1, attrs);
                V::Place(at.0, at.1)
            }
            Pass::Ref | Pass::Memory { .. } => V::Place(one()?, 0),
            Pass::Wide { .. } => {
                let (ptr, off) = self.temp(repr);
                let attrs = self.mem(repr);
                self.b.store(Flags::NONE, one()?, ptr, off, attrs);
                self.b.store(Flags::NONE, one()?, ptr, off + 8, attrs);
                V::Place(ptr, off)
            }
            Pass::Words { size } => {
                let at = self.temp(repr);
                let align = self.size_align(repr).1;
                for k in 0..size.div_ceil(8) {
                    let w = one()?;
                    self.write_word(w, at, k * 8, (size - k * 8).min(8), align);
                }
                V::Place(at.0, at.1)
            }
        })
    }

    /// Calls `callee` (a fn reference, or a pointer value) with the values `n` on top of the
    /// stack, of `reprs` passed as `passes`, for a result of `rrepr` passed as `rpass`; pushes
    /// the result.
    fn emit_call(&mut self, callee: Callee, reprs: &[Repr], passes: &[Pass], rrepr: Repr, rpass: Pass) -> Result<(), BuildError> {
        self.emit_call_from(callee, reprs, passes, None, false, rrepr, rpass)
    }

    /// Returns `v` from the function: as the one result, through the result pointer, or as
    /// nothing; the value is made first, then every open scope is dropped, parameters last.
    fn ret_value(&mut self, v: V) -> Result<(), BuildError> {
        // A coroutine's body returns `Poll::Ready(v)` (coroutine.rs).
        if self.co.is_some() {
            return self.coroutine_return(v);
        }
        let ret = self.ret;
        let mut xs = Vec::with_capacity_in(2, self.alloc);
        match self.ret_pass {
            Pass::Ref | Pass::Memory { .. } => {
                let to = self.sret.ok_or(malformed("an aggregate result without its pointer"))?;
                self.store(v, ret, (to, 0))?;
            }
            p => self.pass_arg(v, ret, p, &mut xs)?,
        }
        self.drop_from(0)?;
        self.b.ret(&xs);
        Ok(())
    }

    /// Whether building place `e` runs no code that could write memory: locals, fields,
    /// dereferences and indexes by a local or a literal.
    fn effect_free_place(&self, e: ExprId) -> Result<bool, BuildError> {
        let mut todo = Vec::new_in(self.alloc);
        todo.push(e);
        while let Some(e) = todo.pop() {
            match self.expr(e)? {
                Expr::Local(_) | Expr::Lit(_) => {}
                Expr::Paren(x) | Expr::Field { base: x, .. } | Expr::TupleField { base: x, .. } => todo.push(x),
                Expr::Unary { op: UnOp::Deref, operand } => todo.push(operand),
                // An overloaded index calls `Index::index`, which may write (walk/index.rs).
                Expr::Index { .. } if !self.index_builtin(e)? => return Ok(false),
                Expr::Index { base, index } => {
                    todo.push(base);
                    todo.push(index);
                }
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    /// Whether `e` builds its aggregate value in a temporary of its own, which a binding or a
    /// call may then take as is: a call (a method call too), a literal.
    fn is_fresh(&self, e: ExprId) -> Result<bool, BuildError> {
        let mut e = e;
        loop {
            if !self.adjust_rows(e).is_empty() {
                return Ok(false);
            }
            match self.expr(e)? {
                Expr::Paren(inner) => e = inner,
                // A method call is a call (`raw`'s `Expr::MethodCall` arm: `Task::Call`), its
                // result in the slot the call made: the Reference, "Place Expressions and Value
                // Expressions", a value expression as a call is. Not here: an `Index` (a place,
                // `*index(&a, i)`), a `?` (its value is a place in the matched temporary), and,
                // until a test shows the slot left alive, an `.await`'s result and an overloaded
                // operator's call.
                Expr::Call { .. } | Expr::MethodCall { .. } | Expr::Tuple { .. } | Expr::Array { .. } | Expr::Repeat { .. } | Expr::Struct { .. } => {
                    return Ok(true);
                }
                _ => return Ok(false),
            }
        }
    }

    fn step(&mut self, task: Task) -> Result<(), BuildError> {
        match task {
            Task::Eval(e) => {
                // A temporary scope of its own: opened around its evaluation (walk/temps.rs).
                if self.temp_enter(e)? {
                    return Ok(());
                }
                let rows = self.adjust_rows(e);
                if rows.is_empty() {
                    return self.raw(e);
                }
                self.eval_adjusted(e, rows)?;
            }
            Task::Raw(e) => self.raw(e)?,
            Task::Adjust(e, from) => self.adjust(e, from as usize)?,
            Task::DerefCall(e, n) => self.deref_call(e, n as usize)?,
            Task::LoadAdjusted(e, n) => {
                let v = self.pop()?;
                let at = self.place_any(v)?;
                let t = self.checked.adjusts.column::<2>()[self.adjust_rows(e).start + n as usize];
                let repr = self.repr_at(e, t)?;
                if self.repr_needs_drop(repr) {
                    // A move out of a `Box`'s contents (walk/moves/boxes.rs); any other
                    // dereference is refused there by name.
                    let v = self.move_box_contents(e, n, at, repr)?;
                    self.push_val(v, None);
                    return Ok(());
                }
                let v = self.load(repr, at);
                self.push_val(v, None);
            }
            Task::SpillAdjusted(e) => {
                let v = self.pop()?;
                let repr = self.adjusted_repr(e)?;
                let drop = self.repr_needs_drop(repr);
                let at = match v {
                    V::Place(p, off) if !drop || self.is_fresh(e)? => (p, off),
                    _ => {
                        let at = self.temp(repr);
                        self.store(v, repr, at)?;
                        at
                    }
                };
                // Dropped at its statement's end (walk/temps.rs).
                if drop && self.live {
                    self.hold_temp(e, at, repr)?;
                }
                self.push_val(V::Place(at.0, at.1), None);
            }
            Task::Place(e) => self.place_of(e)?,
            Task::PlaceDerefs(e, n) => self.place_derefs(e, n)?,
            Task::Intrinsic(e, which) => self.intrinsic(e, which)?,
            Task::Stmt(s) => self.stmt(s)?,
            Task::Discard => {
                self.pop()?;
            }
            Task::DropValue(e) => {
                let v = self.pop()?;
                let repr = self.adjusted_repr(e)?;
                if self.live && self.repr_needs_drop(repr) {
                    let Repr::Agg(l) = repr else { return Err(malformed("a value that needs drop not held as an aggregate")) };
                    let at = self.place(v, repr)?;
                    let g = self.layouts[l as usize].ty;
                    self.drop_at(g, at)?;
                }
            }
            Task::ScopeEnd(e) => {
                let v = self.pop()?;
                let mark = self.scope_marks.pop().ok_or(malformed("a block's drop scope out of order"))? as usize;
                let repr = self.expr_repr(e)?;
                let v = self.settle(v, repr, mark)?;
                // Last declared first, each with its unwind ladder (walk/unwind_ladder.rs); each
                // binding's own drop reads only its own `owned`, so they are cleared after.
                self.drop_scope(mark, true)?;
                for i in mark..self.scope_pats.len() {
                    if let Held::Pat(p) = self.scope_pats[i] {
                        self.owned[p.ix()] = LocalOwn::None;
                    }
                }
                self.scope_pats.truncate(mark);
                self.push_val(v, None);
            }
            Task::TempEnd => self.temp_end()?,
            Task::TempScopeEnd(e) => self.temp_close(e)?,
            Task::PushNone => self.push_val(V::None, None),
            // The same answer `stmt` had: `v` is the initializer as a scrutinee, owned with its
            // parts (walk/matches/irrefutable.rs `let_owns_parts`); the bindings own themselves.
            Task::Let(p, Some(init)) if self.let_owns_parts(p, init)? => {
                let v = self.pop()?;
                let from = self.bind_let_owned(p, init, v)?;
                self.place_super_let(p, from)?;
            }
            Task::Let(p, init) => {
                let v = self.pop()?;
                // The same answer `stmt` had: `v` is the initializer's place (walk/matches/ref_bind.rs).
                let borrows = match init {
                    Some(e) => self.let_borrows_place(p, e)?,
                    None => false,
                };
                let fresh = match (init, v, self.pat(p)?) {
                    _ if borrows => None,
                    (Some(e), V::Place(at, off), Pat::Bind { .. }) if self.live && self.is_fresh(e)? => Some((at, off)),
                    _ => None,
                };
                match fresh {
                    Some(at) => self.bind_at(p, at)?,
                    None if borrows => self.bind_let_place(p, v)?,
                    // The initializer's own type: a pattern that peels a reference is recorded
                    // at the peeled one (`bind_value_of`).
                    None => match init {
                        Some(e) => {
                            let t = self.adjusted_ty(e)?;
                            let g = self.ground_ty(t).map_err(|error| Self::ty_err(e, error))?;
                            self.bind_value_of(p, v, g)?
                        }
                        None => self.bind_value(p, v)?,
                    },
                }
                let from = self.scope_pats.len();
                self.own_pat(p)?;
                self.place_super_let(p, from)?;
            }
            Task::BlockExit(e) => {
                let v = self.pop()?;
                let t = self.targets.pop().filter(|t| t.expr == e).ok_or(malformed("a block's break target out of order"))?;
                let arg = self.arg(v, t.repr)?;
                self.jump(t.exit, arg.as_slice());
                self.begin(t.exit);
                let v = self.join_param(t.repr);
                self.push_val(v, None);
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
                        self.push_val(V::None, None);
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
                self.push_val(v, None);
            }
            Task::LetLink { expr, fail } => self.let_link(expr, fail)?,
            Task::CondLink(fail) => self.cond_link(fail)?,
            Task::LetElse(s) => self.let_else_test(s)?,
            Task::LetElseEnd(cont, from, els) => self.let_else_end(cont, from, els)?,
            Task::LoopEnd(e) => {
                let t = self.targets.pop().filter(|t| t.expr == e).ok_or(malformed("a loop's target out of order"))?;
                let header = t.header.ok_or(malformed("a loop without its head"))?;
                self.jump(header, &[]);
                self.begin(t.exit);
                let v = self.join_param(t.repr);
                self.push_val(v, None);
            }
            Task::Break(target, has_value) => {
                let v = if has_value { self.pop()? } else { V::None };
                let t = *self
                    .targets
                    .iter()
                    .rev()
                    .find(|t| t.expr == target)
                    .ok_or(malformed("a `break` whose target is not around it"))?;
                let start = self.scope_start(t.depth);
                let v = self.settle(v, t.repr, start)?;
                let arg = self.arg(v, t.repr)?;
                self.drop_from(start)?;
                self.jump(t.exit, arg.as_slice());
                self.dead_block();
                self.push_val(V::None, None);
            }
            Task::Return(has_value) => {
                let v = if has_value { self.pop()? } else { V::None };
                self.ret_value(v)?;
                self.dead_block();
                self.push_val(V::None, None);
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
            Task::GuardAlts { word, chains, n } => self.guard_alts(word, chains, n)?,
            Task::ArmMoves { from } => self.arm_moves(from)?,
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
                self.unreachable_of(TrapKind::MatchNoArm, Some(st.expr));
                self.arm_blocks.truncate(st.first as usize);
                self.begin(st.join);
                let v = self.join_param(st.repr);
                self.push_val(v, None);
            }
            Task::Call(e) => self.call(e)?,
            Task::OpCall(e) => self.operator_call(e)?,
            Task::Unary(e, op) => {
                let v = self.pop()?;
                let repr = self.expr_repr(e)?;
                let r = match (op, repr) {
                    (UnOp::Neg, Repr::Int { ty, signed }) => {
                        let x = self.scalar(v, ty)?;
                        // [expr.operator.int-overflow]: `-MIN` overflows "unless the operand
                        // is a literal expression (or a literal expression standing alone
                        // inside one or more grouped expressions)".
                        let Expr::Unary { mut operand, .. } = self.expr(e)? else {
                            return Err(malformed("a unary task on another expression"));
                        };
                        while let Expr::Paren(inner) = self.expr(operand)? {
                            operand = inner;
                        }
                        if signed && self.adts.overflow_checks().on() && !matches!(self.expr(operand)?, Expr::Lit(_)) {
                            self.checked_neg(e, x, ty)?
                        } else {
                            self.b.unary(Op::Neg, Flags::NONE, x)
                        }
                    }
                    (UnOp::Not, Repr::Int { ty, .. }) => {
                        let x = self.scalar(v, ty)?;
                        self.b.unary(Op::Not, Flags::NONE, x)
                    }
                    (UnOp::Neg, Repr::Float(ty)) => {
                        let x = self.scalar(v, ty)?;
                        self.b.unary(Op::Fneg, Flags::NONE, x)
                    }
                    (UnOp::Not, Repr::Bool) => {
                        let x = self.scalar(v, Ty::I8)?;
                        let one = self.b.iconst(Ty::I8, 1);
                        self.b.binary(Op::Xor, Flags::NONE, x, one)
                    }
                    _ => return Err(unsupported(e, "this unary operator on this type")),
                };
                self.push_val(V::Scalar(r), None);
            }
            Task::Deref(e) => {
                let v = self.pop()?;
                let p = self.scalar(v, Ty::PTR)?;
                let repr = self.expr_repr(e)?;
                // A builtin `*` of a reference or raw pointer read as a value. Every place
                // expression context (a scrutinee, a borrow's or field's operand, an assignment's
                // left side, a `let` initializer bound by `ref`) builds `*p` by `Task::Place`; a
                // `Box`'s `*` is the check's overloaded dereference (walk/moves/boxes.rs), never
                // this task. So a needs-drop value here is a move out of a place behind a pointer,
                // which the check refuses (E0507; the Reference, [expr.move.movable-place]).
                if self.repr_needs_drop(repr) {
                    return Err(malformed("a move out of a dereference whose type needs drop, which the check refuses (E0507)"));
                }
                let v = self.load(repr, (p, 0));
                self.push_val(v, None);
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
                self.push_val(V::Scalar(p), None);
            }
            Task::Assign(e) => {
                let to = self.pop()?;
                let v = self.pop()?;
                let Expr::Assign { lhs, .. } = self.expr(e)? else { return Err(malformed("an assignment's task on another expression")) };
                let repr = self.expr_repr(lhs)?;
                let at = self.place(to, repr)?;
                if self.live && self.repr_needs_drop(repr) {
                    // The old value is dropped before the store, the new one held apart from
                    // anything the drop could write.
                    let v = match v {
                        V::Place(..) => {
                            let tmp = self.temp(repr);
                            self.store(v, repr, tmp)?;
                            V::Place(tmp.0, tmp.1)
                        }
                        other => other,
                    };
                    match self.local_of(lhs)? {
                        Some(p) if self.local_drops(p) => {
                            self.note_whole_assign(e, p);
                            self.drop_local(p, false)?;
                            self.store(v, repr, at)?;
                            self.set_flag(p, true);
                            self.hold_all(p);
                            self.init[p.ix()] = true;
                        }
                        // A whole binding this closure captures and does not own: by reference
                        // (its home is the enclosing body's binding, behind the field's pointer:
                        // closure.rs `bind_capture_at`), or by value in an `Fn`/`FnMut` body
                        // (the environment's field, borrowed with the environment). The
                        // Reference, "Assignment expressions": "It then has the effect of first
                        // dropping the value at the assigned place, unless the place is an
                        // uninitialized local variable or an uninitialized field of a local
                        // variable", then moving the value in. A captured binding is
                        // initialized (a capture reads it: E0381), and its owner, the
                        // enclosing body or the environment, drops what this store leaves
                        // there. tempo consensus-config `read_secret_inner`'s closure 0
                        // (`read_result = Err(..)` through a `&mut` capture), t1530.
                        Some(p) if self.captured_whole(p) => {
                            let Repr::Agg(l) = repr else { return Err(malformed("a value that needs drop not held as an aggregate")) };
                            let g = self.layouts[l as usize].ty;
                            self.drop_at(g, at)?;
                            self.store(v, repr, at)?;
                        }
                        Some(_) => return Err(malformed("an assignment to a binding that needs drop and is not owned")),
                        None => {
                            let Repr::Agg(l) = repr else { return Err(malformed("a value that needs drop not held as an aggregate")) };
                            let g = self.layouts[l as usize].ty;
                            // An owned field of a closure body's environment drops by its state
                            // (walk/moves/env_places.rs); a field of a local with field flags by
                            // its bits (walk/moves.rs).
                            if !self.assign_env_place(lhs, at, v, repr)? {
                                self.assign_over(lhs, g, at, v, repr)?;
                            }
                        }
                    }
                } else {
                    self.store(v, repr, at)?;
                    if self.live {
                        if let Some(p) = self.local_of(lhs)? {
                            self.note_whole_assign(e, p);
                            self.hold_all(p);
                        } else if let Repr::Agg(l) = repr
                            && let Ok(ix) = usize::try_from(l)
                            && self.layouts.get(ix).is_some_and(|row| row.placed)
                        {
                            self.reinit_field_bits(lhs)?;
                        }
                    }
                }
                self.push_val(V::None, None);
            }
            Task::AssignOp(e, op) => {
                let to = self.pop()?;
                let r = self.pop()?;
                let Expr::AssignOp { lhs, rhs, .. } = self.expr(e)? else {
                    return Err(malformed("an assignment's task on another expression"));
                };
                let lrepr = self.expr_repr(lhs)?;
                let rrepr = self.adjusted_repr(rhs)?;
                let ty = lrepr.scalar().ok_or(unsupported(e, "a compound assignment to a non-scalar"))?;
                let (p, off) = self.place(to, lrepr)?;
                let l = self.load(lrepr, (p, off));
                let l = self.scalar(l, ty)?;
                let rty = rrepr.scalar().ok_or(unsupported(e, "a compound assignment of a non-scalar"))?;
                let r = self.scalar(r, rty)?;
                // The Reference, "Compound assignment expressions": for primitive types `a op= b`
                // is `a = a op b` once, so a float's is its IEEE operator (walk/numbers.rs), as
                // core's `impl DivAssign for f32` (`*self /= other`) needs.
                let x = match lrepr {
                    Repr::Float(_) => self.float_binary(e, op, l, r)?,
                    _ => self.arith(e, op, l, r, lrepr, rrepr)?,
                };
                self.store(V::Scalar(x), lrepr, (p, off))?;
                self.push_val(V::None, None);
            }
            Task::Cast(e) => self.cast(e)?,
            Task::InitAt { off, repr } => {
                let v = self.pop()?;
                let V::Place(p, at) = *self.vals.last().ok_or(malformed("an aggregate literal without its place"))? else {
                    return Err(malformed("an aggregate literal without its place"));
                };
                self.store(v, repr, (p, at + off as i32))?;
                // The literal owns it now (walk/val_stack.rs).
                self.stored_part(off, repr)?;
            }
            Task::LitBuilt { l } => self.literal_built(l)?,
            Task::StructBase(e) => self.struct_update_base(e)?,
            Task::InitDefaults(e) => self.init_defaults(e)?,
            // A field of an unsized place: of a struct ending in a slice or `str`, reached
            // through its wide pointer (walk/unsized_tail.rs).
            Task::Field(e) | Task::FieldPlace(e) if matches!(self.vals.last(), Some(V::Unsized(..))) => {
                self.unsized_field(e, matches!(task, Task::FieldPlace(_)))?;
            }
            Task::Field(e) | Task::FieldPlace(e) => {
                let (field_off, repr) = self.field(e)?;
                let v = self.pop()?;
                let (p, off) = self.place_any(v)?;
                let at = (p, off + field_off as i32);
                let v = match task {
                    // A partial move out of a local (walk/moves.rs).
                    Task::Field(_) if self.repr_needs_drop(repr) => self.move_field(e, at, repr)?,
                    Task::Field(_) => self.load(repr, at),
                    _ => V::Place(at.0, at.1),
                };
                self.push_val(v, None);
            }
            Task::IndexPlace(e) => self.index_place(e)?,
            Task::IndexCall(e) => self.index_call(e)?,
            Task::LoadPlace(e) => {
                let v = self.pop()?;
                let at = self.place_any(v)?;
                let repr = self.expr_repr(e)?;
                if self.repr_needs_drop(repr) {
                    return Err(unsupported(e, "a move out of an indexed element whose type needs drop"));
                }
                let v = self.load(repr, at);
                self.push_val(v, None);
            }
            Task::DerefPlace => {
                // A thin pointer is a scalar (`repr`); a wide one is an aggregate whose
                // dereference is an unsized place, kept as the pointer's own memory.
                let v = match self.pop()? {
                    V::Place(p, off) => V::Unsized(p, off),
                    v => V::Place(self.scalar(v, Ty::PTR)?, 0),
                };
                self.push_val(v, None);
            }
            Task::Spill(e) => {
                let v = self.pop()?;
                let repr = self.expr_repr(e)?;
                let drop = self.repr_needs_drop(repr);
                let at = match v {
                    V::Place(p, off) if !drop || self.is_fresh(e)? => (p, off),
                    _ => {
                        let at = self.temp(repr);
                        self.store(v, repr, at)?;
                        at
                    }
                };
                // Dropped at its statement's end (walk/temps.rs).
                if drop && self.live {
                    self.hold_temp(e, at, repr)?;
                }
                self.push_val(V::Place(at.0, at.1), None);
            }
            Task::ArgCopy(a) => {
                let v = self.pop()?;
                let repr = self.adjusted_repr(a)?;
                let v = match v {
                    V::Place(..) if self.is_fresh(a)? => v,
                    _ => {
                        let at = self.temp(repr);
                        self.store(v, repr, at)?;
                        V::Place(at.0, at.1)
                    }
                };
                // The caller's copy is the walk's own until the call takes it: a call that
                // unwinds before then drops it (walk/val_stack.rs).
                let owned = self.owned_arg(repr);
                self.push_val(v, owned);
            }
            Task::Repeat(e) => self.repeat(e)?,
            Task::FormatArgs(e) => self.format_args_done(e)?,
            Task::Await(e) => self.await_poll(e)?,
            Task::Asm(e) => self.asm_done(e)?,
            Task::AsmLabel { block, body, join } => self.asm_label(block, body, join),
            Task::AsmLabelEnd(join) => self.asm_label_end(join)?,
            Task::AsmJoin(join) => self.asm_join(join),
            Task::Ref => {
                let v = match self.pop()? {
                    // A borrow of an unsized place is the wide pointer that reached it.
                    V::Unsized(p, off) => V::Place(p, off),
                    v => {
                        let at = self.place_any(v)?;
                        V::Scalar(self.address(at))
                    }
                };
                self.push_val(v, None);
            }
        }
        Ok(())
    }

    /// Pushes the tasks that evaluate `e` and apply its adjustments `rows` (lib.rs,
    /// "Adjustments"): the leading dereferences and the borrow as a pointer to the place they
    /// reach (never a copy of the value), a dereference with no borrow as a load from that
    /// place, then the coercions as value steps.
    fn eval_adjusted(&mut self, e: ExprId, rows: core::ops::Range<usize>) -> Result<(), BuildError> {
        let kinds = &self.checked.adjusts.column::<1>()[rows];
        let lead = kinds.iter().take_while(|k| matches!(k, AdjustKind::Deref { .. })).count();
        let borrow = matches!(kinds.get(lead), Some(AdjustKind::Borrow { .. }));
        // A `&mut` autoref or reborrow is a mutable place context (walk/index.rs).
        if matches!(kinds.get(lead), Some(AdjustKind::Borrow { mutbl: Mutbl::Mut, .. })) {
            self.refuse_mut_through_overloaded_index(e)?;
        }
        let from = lead + usize::from(borrow);
        if kinds[from..].iter().any(|k| matches!(k, AdjustKind::Deref { .. } | AdjustKind::Borrow { .. })) {
            return Err(unsupported(e, "a dereference or a borrow after a coercion in one adjustment list"));
        }
        if from < kinds.len() {
            self.work.push(Task::Adjust(e, from as u32));
        }
        match (lead, borrow) {
            (0, false) => self.raw(e)?,
            // A shared autoref promoted to a `'static` slot (walk/promote.rs): the address of
            // `e`'s constant row, never a frame temporary; any coercions after it (e.g. `Unsize`)
            // were already pushed above as `Task::Adjust(e, 1)`. An `Expr::Ref` listed in
            // `Checked::promoted` promotes its own `&operand`, not a further autoref of it.
            (0, true)
                if !matches!(self.expr(e)?, Expr::Ref { .. })
                    && matches!(kinds[0], AdjustKind::Borrow { mutbl: Mutbl::Not, raw: false, .. })
                    && self.is_promoted(e) =>
            {
                self.promoted_ref(e)?;
            }
            // An autoref: the address of `e`'s own place.
            (0, true) => {
                self.work.push(Task::Ref);
                self.work.push(Task::PlaceDerefs(e, 0));
            }
            // A reborrow: the pointer to the place the dereferences reach is the borrow.
            (_, true) => self.push_deref_chain(e, lead),
            // A dereference with no borrow: a copy out of the place.
            (_, false) => {
                self.work.push(Task::LoadAdjusted(e, (lead - 1) as u32));
                self.work.push(Task::DerefPlace);
                self.push_deref_chain(e, lead);
            }
        }
        Ok(())
    }

    /// Pushes the tasks that leave a pointer to the place `e`'s first `lead` adjustments (all
    /// dereferences) reach: a builtin one loads the next pointer from the place, an overloaded
    /// one calls `Deref::deref` with the pointer to its `Self` (`DerefCall`).
    fn push_deref_chain(&mut self, e: ExprId, lead: usize) {
        let rows = self.adjust_rows(e);
        let kinds = &self.checked.adjusts.column::<1>()[rows.start..rows.start + lead];
        let mut builtin_n = 0usize;
        let mut builtin_u32 = 0u32;
        for k in kinds {
            if matches!(*k, AdjustKind::Deref { overloaded: None }) {
                builtin_n += 1;
                builtin_u32 += 1;
            } else {
                break;
            }
        }
        // The tasks run first to last, so they are pushed last to first.
        for i in (builtin_n.max(1)..lead).rev() {
            match kinds[i] {
                AdjustKind::Deref { overloaded: Some(_) } => self.work.push(Task::DerefCall(e, i as u32)),
                // The pointer step `i` dereferences is the place step `i - 1` reached, loaded
                // at its own type: a wide pointer whole (rule 76: never as a thin one).
                _ => {
                    self.work.push(Task::LoadAdjusted(e, (i - 1) as u32));
                    self.work.push(Task::DerefPlace);
                }
            }
        }
        if builtin_n > 0 {
            // Leading builtin dereferences go through `PlaceDerefs` so a closure body whose
            // environment holds a place below its binding (`*r`, `**r`: env_places.rs) reads
            // the captured place rather than the un-dereferenced root.
            self.work.push(Task::Ref);
            self.work.push(Task::PlaceDerefs(e, builtin_u32));
        } else {
            // `e` is the `Self`: borrowed in place.
            self.work.push(Task::DerefCall(e, 0));
            self.work.push(Task::Ref);
            self.work.push(Task::PlaceDerefs(e, 0));
        }
    }

    /// The type `e` has before its adjustment `n`: its own for the first, else the one the
    /// adjustment before it made.
    fn ty_before(&self, e: ExprId, n: usize) -> Result<PTy, BuildError> {
        match n {
            0 => self.expr_ty(e),
            _ => Ok(self.checked.adjusts.column::<2>()[self.adjust_rows(e).start + n - 1]),
        }
    }

    /// Adjustment `n` of `e`, an overloaded dereference: calls `Deref::deref` (or
    /// `DerefMut::deref_mut`) at `[Self]` with the pointer on top, and leaves the reference it
    /// returns. A reference to an unsized target (`String` to `str`, `W` to `[u8]`) is a wide
    /// pointer, left as the place of its own memory (`emit_call` puts an aggregate result in a
    /// temporary); the `DerefPlace` after makes that the target's unsized place (`V::Unsized`),
    /// exactly as for the builtin `*r` of `r: &[T]`. The Reference, "The dereference operator":
    /// "On non-pointer types *x is equivalent to *std::ops::Deref::deref(&x) in an immutable
    /// place expression context". Its consumers are the unsized place's: a borrow, a field of
    /// an unsized tail, an index (`push_index_base`) and a pattern (walk/matches.rs).
    fn deref_call(&mut self, e: ExprId, n: usize) -> Result<(), BuildError> {
        let rows = self.adjust_rows(e);
        let kinds = &self.checked.adjusts.column::<1>()[rows.start..rows.end];
        let AdjustKind::Deref { overloaded: Some(m) } = kinds[n] else {
            return Err(malformed("a deref call on an adjustment that is not an overloaded dereference"));
        };
        let mut_after = kinds[n + 1..].iter().any(|k| matches!(k, AdjustKind::Borrow { mutbl: Mutbl::Mut, .. }));
        let self_t = self.ty_before(e, n)?;
        let target_t = self.checked.adjusts.column::<2>()[rows.start + n];
        let g_self = self.ground_ty(self_t).map_err(|error| Self::ty_err(e, error))?;
        let g_target = self.ground_ty(target_t).map_err(|error| Self::ty_err(e, error))?;
        // A `Box`'s dereference is the builtin one (walk/moves/boxes.rs): alloc's
        // `Deref::deref` for a box is `&**self`, which would call itself.
        if self.box_arg(g_self).map_err(|error| Self::ty_err(e, error))?.is_some() {
            return self.box_deref_builtin(e, m, g_self, g_target);
        }
        // A place borrowed `&mut` through `Deref::deref` would be written through a shared
        // reference: the check must record `DerefMut` there (rustc's
        // `convert_place_derefs_to_mutable`), and the build never upgrades it on its own.
        if m == Mutbl::Not && mut_after {
            return Err(unsupported(e, "a `&mut` borrow through an overloaded dereference the check recorded as `Deref::deref`, not `DerefMut::deref_mut`"));
        }
        let name = match m {
            Mutbl::Not => "deref",
            Mutbl::Mut => "deref_mut",
        };
        let item = self.adts.deref_fn(m).ok_or(BuildError::NoLangFn { expr: Some(e), name })?;
        let g_arg =self.ground.intern(PNode::Ref(m, g_self)).map_err(|_| malformed("the ground table is full"))?;
        let g_ret = self.ground.intern(PNode::Ref(m, g_target)).map_err(|_| malformed("the ground table is full"))?;
        let arepr = self.repr(g_arg).map_err(|error| Self::ty_err(e, error))?;
        // An unsized `Self` (`struct Bytes([u8])` with `impl Deref for Bytes`, winnow's
        // `SliceLen for &Bytes`): `&Self` is a wide pointer, an aggregate, on top as the place of
        // its memory (`Task::Ref` of the unsized place, or the loaded pointer of the step
        // before). It is passed as every aggregate argument is, from the caller's own copy
        // (`Task::ArgCopy`'s rule), so the callee's parameter is never the place it came from.
        match arepr {
            Repr::Ptr => {}
            Repr::Agg(_) => {
                let v = self.pop()?;
                let at = self.temp(arepr);
                self.store(v, arepr, at)?;
                self.push_val(V::Place(at.0, at.1), None);
            }
            _ => return Err(malformed("a reference to a `Deref` `Self` that is neither a thin nor a wide pointer")),
        }
        // A wide `rrepr` (an aggregate) comes back as `V::Place` of its temporary: the memory
        // `DerefPlace` keeps as the unsized place (`take_passed`, every aggregate pass).
        let rrepr = self.repr(g_ret).map_err(|error| Self::ty_err(e, error))?;
        let abi = self.adts.abi(item).ok_or(BuildError::NoAbi { item })?;
        let apass = self.pass(g_arg, arepr, abi, Some(item))?;
        let rpass = self.pass(g_ret, rrepr, abi, Some(item))?;
        let args = self.ground.intern_list(&[g_self]).map_err(|_| malformed("the ground table is full"))?;
        self.call_instance(Some(e), Instance { item, args }, &[arepr], &[apass], None, rrepr, rpass)
    }

    /// Applies `e`'s adjustments from its `from`th on, all coercions, to the value on top, in
    /// order. A step the build has no form for is refused by name, never skipped.
    fn adjust(&mut self, e: ExprId, from: usize) -> Result<(), BuildError> {
        let rows = self.adjust_rows(e);
        for n in from..rows.len() {
            let kind = self.checked.adjusts.column::<1>()[rows.start + n];
            let t = self.checked.adjusts.column::<2>()[rows.start + n];
            match kind {
                AdjustKind::NeverToAny => {
                    let repr = self.repr_at(e, t)?;
                    let v = self.pop()?;
                    // core's primitive_docs.rs, the `!` type: "`!` represents the type of
                    // diverging computations -- computations which never resolve to any value",
                    // and "since `!` has no values (since it is uninhabited), it is a marker for
                    // unreachable code". A `!` value reached in live code (a read of a `!`
                    // place: `*self` of `self: &!`, core's `impl Debug for !`) means control
                    // never gets here: the block ends, and the coerced value below is built in
                    // the dead block after it (a call returning `!` already ended its block).
                    if self.live {
                        self.unreachable_of(TrapKind::NeverValue, Some(e));
                        self.dead_block();
                    }
                    let v = match repr.kept() {
                        Kept::None => V::None,
                        Kept::Agg(_) => {
                            let (p, off) = self.place(v, repr)?;
                            V::Place(p, off)
                        }
                        Kept::Scalar(ty) => V::Scalar(self.scalar(v, ty)?),
                    };
                    self.push_val(v, None);
                }
                // A raw pointer's mutability, a fn pointer's `unsafe`: the same bits.
                AdjustKind::MutToConstPointer | AdjustKind::UnsafeFnPointer => {}
                // A fn item to a pointer (lib.rs, "Fn pointers"): the address of its instance.
                AdjustKind::ReifyFnPointer => {
                    self.pop()?;
                    let before = self.ty_before(e, n)?;
                    let PNode::FnDef(item, args) = self.node_at(e, before)? else {
                        return Err(malformed("a fn pointer made of other than a fn item"));
                    };
                    let p = self.reify_addr(e, Instance { item, args })?;
                    self.push_val(V::Scalar(p), None);
                }
                AdjustKind::Unsize => self.unsize(e, n)?,
                // A closure that captures nothing to a pointer: its `call_once` (closure.rs
                // `closure_fn_addr`).
                AdjustKind::ClosureFnPointer => {
                    self.pop()?;
                    let before = self.ty_before(e, n)?;
                    let p = self.closure_fn_addr(e, before)?;
                    self.push_val(V::Scalar(p), None);
                }
                AdjustKind::Deref { .. } | AdjustKind::Borrow { .. } => {
                    return Err(malformed("a dereference or a borrow among the value adjustments"));
                }
                AdjustKind::UnionInject { variant } => self.union_inject(e, t, variant)?,
                AdjustKind::UnionWiden => self.union_widen(e, n, t)?,
                AdjustKind::RecordProject => self.record_project(e, n, t)?,
            }
        }
        Ok(())
    }

    /// Projects a wider structural record on top to a narrower structural record `dst_t`
    /// (`{ host, port, tls } -> { host }`), copying retained fields by canonical field name and
    /// dropping any unused source fields that need drop.
    fn record_project(&mut self, e: ExprId, n: usize, dst_t: PTy) -> Result<(), BuildError> {
        let src_t = self.ty_before(e, n)?;
        let src_g = self.ground_ty(src_t).map_err(|error| Self::ty_err(e, error))?;
        let dst_g = self.ground_ty(dst_t).map_err(|error| Self::ty_err(e, error))?;
        if src_g == dst_g {
            return Ok(());
        }
        let PNode::Record(src_schema, src_list) = self.ground_node(src_g).map_err(|error| Self::ty_err(e, error))? else {
            return Err(malformed("a record projection from other than a structural record"));
        };
        let PNode::Record(dst_schema, dst_list) = self.ground_node(dst_g).map_err(|error| Self::ty_err(e, error))? else {
            return Err(malformed("a record projection to other than a structural record"));
        };
        let mut src_names: blaze_own::Vec<Name, A> = blaze_own::Vec::new_in(self.alloc);
        let mut dst_names: blaze_own::Vec<Name, A> = blaze_own::Vec::new_in(self.alloc);
        let mut src_fields: blaze_own::Vec<PTy, A> = blaze_own::Vec::new_in(self.alloc);
        let mut dst_fields: blaze_own::Vec<PTy, A> = blaze_own::Vec::new_in(self.alloc);
        {
            let gv = self.ground.view();
            let cv = self.checked.types.view();
            let s_ns = gv
                .record_names(src_schema)
                .or_else(|| cv.record_names(src_schema))
                .ok_or(malformed("a projected source record missing schema names"))?;
            let d_ns = gv
                .record_names(dst_schema)
                .or_else(|| cv.record_names(dst_schema))
                .ok_or(malformed("a projected target record missing schema names"))?;
            src_names.extend_from_slice(s_ns);
            dst_names.extend_from_slice(d_ns);
            src_fields.extend_from_slice(gv.list(src_list).map_err(|_| malformed("a source record's fields past the ground table"))?);
            dst_fields.extend_from_slice(gv.list(dst_list).map_err(|_| malformed("a target record's fields past the ground table"))?);
        }
        let v = self.pop()?;
        let src_repr = self.repr(src_g).map_err(|error| Self::ty_err(e, error))?;
        let dst_repr = self.repr(dst_g).map_err(|error| Self::ty_err(e, error))?;
        if !self.live {
            match dst_repr {
                Repr::None => self.push_val(V::None, None),
                Repr::Agg(_) => {
                    let dst_at = self.temp(dst_repr);
                    self.push_val(V::Place(dst_at.0, dst_at.1), None);
                }
                _ => return Err(malformed("a structural record not held as an aggregate")),
            }
            return Ok(());
        }
        let src_at = match src_repr {
            Repr::None => None,
            Repr::Agg(src_l) => Some((src_l, self.place(v, src_repr)?)),
            _ => return Err(malformed("a structural record not held as an aggregate")),
        };
        let out_val = match dst_repr {
            Repr::None => V::None,
            Repr::Agg(dst_l) => {
                let dst_at = self.temp(dst_repr);
                let Some((src_l, sat)) = src_at else {
                    return Err(malformed("a non-ZST record projected from a ZST record"));
                };
                for (di, &nm) in dst_names.iter().enumerate() {
                    let si = src_names
                        .iter()
                        .position(|&sn| sn == nm)
                        .ok_or(malformed("a projected record missing a target field in source"))?;
                    let ft = *dst_fields.get(di).ok_or(malformed("a target record field past its list"))?;
                    let frepr = self.repr(ft).map_err(|error| Self::ty_err(e, error))?;
                    if frepr == Repr::None {
                        continue;
                    }
                    let soff = self.field_offset(src_l, si)?;
                    let doff = self.field_offset(dst_l, di)?;
                    let soff_i32 = i32::try_from(soff).map_err(|_| malformed("a record field offset past `i32`"))?;
                    let doff_i32 = i32::try_from(doff).map_err(|_| malformed("a record field offset past `i32`"))?;
                    let fv = self.load(frepr, (sat.0, sat.1 + soff_i32));
                    self.store(fv, frepr, (dst_at.0, dst_at.1 + doff_i32))?;
                }
                V::Place(dst_at.0, dst_at.1)
            }
            _ => return Err(malformed("a structural record not held as an aggregate")),
        };
        for (si, &sn) in src_names.iter().enumerate() {
            if dst_names.iter().any(|&dn| dn == sn) {
                continue;
            }
            let ft = *src_fields.get(si).ok_or(malformed("a source record field past its list"))?;
            if !self.needs_drop(ft).map_err(|error| Self::ty_err(e, error))? {
                continue;
            }
            let at = match src_at {
                Some((src_l, sat)) => {
                    let soff = self.field_offset(src_l, si)?;
                    let soff_i32 = i32::try_from(soff).map_err(|_| malformed("a record field offset past `i32`"))?;
                    (sat.0, sat.1 + soff_i32)
                }
                None => {
                    let slot = self.b.slot(0, 1, SlotKind::Temp);
                    (self.b.slot_addr(slot), 0)
                }
            };
            self.drop_at(ft, at)?;
        }
        let owns = self.repr_needs_drop(dst_repr).then_some(dst_g);
        self.push_val(out_val, owns);
        Ok(())
    }

    /// Injects the value on top into structural union type `dst_t` at member `variant`.
    fn union_inject(&mut self, e: ExprId, dst_t: PTy, variant: u32) -> Result<(), BuildError> {
        let v = self.pop()?;
        let dst_g = self.ground_ty(dst_t).map_err(|error| Self::ty_err(e, error))?;
        if let PNode::Union(ul) = self.ground_node(dst_g).map_err(|error| Self::ty_err(e, error))?
            && ul.len == 1
        {
            if variant != 0 {
                return Err(malformed("a single-member union injection at a non-zero variant"));
            }
            let owns = self.needs_drop(dst_g).map_err(|error| Self::ty_err(e, error))?.then_some(dst_g);
            self.push_val(v, owns);
            return Ok(());
        }
        let dst_repr = self.repr(dst_g).map_err(|error| Self::ty_err(e, error))?;
        match dst_repr {
            Repr::None => {
                self.push_val(V::None, None);
                Ok(())
            }
            Repr::Agg(dst_l) => {
                let (first, len, tag) = self.variant_rows(dst_l, variant)?;
                if len != 1 {
                    return Err(malformed("a union variant without exactly one payload field"));
                }
                let first_idx = usize::try_from(first).map_err(|_| malformed("a union field index past `usize`"))?;
                let &(_, field_off, field_repr) = self
                    .layout_fields
                    .get(first_idx)
                    .ok_or(malformed("a union variant's field past `layout_fields`"))?;
                let field_off_i32 = i32::try_from(field_off).map_err(|_| malformed("a union field offset past `i32`"))?;
                let dst_at = self.temp(dst_repr);
                self.write_tag(tag, variant, dst_at);
                self.store(v, field_repr, (dst_at.0, dst_at.1 + field_off_i32))?;
                let owns = self.repr_needs_drop(dst_repr).then_some(dst_g);
                self.push_val(V::Place(dst_at.0, dst_at.1), owns);
                Ok(())
            }
            _ => Err(malformed("a multi-member union not held as an aggregate")),
        }
    }

    /// Widens the structural union value on top (`A | B -> A | B | C`).
    fn union_widen(&mut self, e: ExprId, n: usize, dst_t: PTy) -> Result<(), BuildError> {
        let src_t = self.ty_before(e, n)?;
        let src_g = self.ground_ty(src_t).map_err(|error| Self::ty_err(e, error))?;
        let dst_g = self.ground_ty(dst_t).map_err(|error| Self::ty_err(e, error))?;
        if src_g == dst_g {
            return Ok(());
        }
        let PNode::Union(src_l_list) = self.ground_node(src_g).map_err(|error| Self::ty_err(e, error))? else {
            return Err(malformed("a union widening from other than a structural union"));
        };
        let PNode::Union(dst_l_list) = self.ground_node(dst_g).map_err(|error| Self::ty_err(e, error))? else {
            return Err(malformed("a union widening to other than a structural union"));
        };
        let mut src_members: blaze_own::Vec<PTy, A> = blaze_own::Vec::new_in(self.alloc);
        src_members.extend_from_slice(
            self.ground
                .view()
                .list(src_l_list)
                .map_err(|_| malformed("a source union's members past the ground table"))?,
        );
        let mut dst_members: blaze_own::Vec<PTy, A> = blaze_own::Vec::new_in(self.alloc);
        dst_members.extend_from_slice(
            self.ground
                .view()
                .list(dst_l_list)
                .map_err(|_| malformed("a target union's members past the ground table"))?,
        );
        if let [only] = src_members.as_slice() {
            let j = dst_members
                .iter()
                .position(|dm| dm == only)
                .ok_or(malformed("a widened union missing a source member"))?;
            let j = u32::try_from(j).map_err(|_| malformed("a union variant index past `u32`"))?;
            return self.union_inject(e, dst_t, j);
        }
        let v = self.pop()?;
        let src_repr = self.repr(src_g).map_err(|error| Self::ty_err(e, error))?;
        let dst_repr = self.repr(dst_g).map_err(|error| Self::ty_err(e, error))?;
        if dst_repr == Repr::None {
            self.push_val(V::None, None);
            return Ok(());
        }
        let Repr::Agg(dst_l) = dst_repr else {
            return Err(malformed("a widened union not held as an aggregate"));
        };
        let dst_at = self.temp(dst_repr);
        if !self.live {
            self.push_val(V::Place(dst_at.0, dst_at.1), None);
            return Ok(());
        }
        let Repr::Agg(src_l) = src_repr else {
            return Err(malformed("a multi-member source union not held as an aggregate"));
        };
        let src_at = self.place(v, src_repr)?;
        self.remap_union(Some(e), src_l, src_at, &src_members, dst_l, dst_at, &dst_members)?;
        let owns = self.repr_needs_drop(dst_repr).then_some(dst_g);
        self.push_val(V::Place(dst_at.0, dst_at.1), owns);
        Ok(())
    }

    /// Copies the active member of source union `src_l` at `src_at` into target union `dst_l` at
    /// `dst_at`, switching on the source discriminant across every source member that appears in
    /// `dst_members` (shared by `AdjustKind::UnionWiden` and `UnionPat::Residual`).
    pub(super) fn remap_union(
        &mut self,
        at_expr: Option<ExprId>,
        src_l: u32,
        src_at: (Value, i32),
        src_members: &[PTy],
        dst_l: u32,
        dst_at: (Value, i32),
        dst_members: &[PTy],
    ) -> Result<(), BuildError> {
        let src_l_idx = usize::try_from(src_l).map_err(|_| malformed("a layout index past `usize`"))?;
        let Some(&Shape::Enum { id: src_id, len: src_len, .. }) = self.layouts.get(src_l_idx).map(|row| &row.shape) else {
            return Err(malformed("a multi-member union not laid out as an enum"));
        };
        let copy_one = |w: &mut Self, i: u32, j: u32| -> Result<(), BuildError> {
            let (sfirst, slen, _) = w.variant_rows(src_l, i)?;
            let (dfirst, dlen, dtag) = w.variant_rows(dst_l, j)?;
            if slen != 1 || dlen != 1 {
                return Err(malformed("a union variant without exactly one payload field"));
            }
            let sfirst_idx = usize::try_from(sfirst).map_err(|_| malformed("a union field index past `usize`"))?;
            let dfirst_idx = usize::try_from(dfirst).map_err(|_| malformed("a union field index past `usize`"))?;
            let &(_, soff, srepr) = w
                .layout_fields
                .get(sfirst_idx)
                .ok_or(malformed("a source union variant's field past `layout_fields`"))?;
            let &(_, doff, drepr) = w
                .layout_fields
                .get(dfirst_idx)
                .ok_or(malformed("a target union variant's field past `layout_fields`"))?;
            let soff_i32 = i32::try_from(soff).map_err(|_| malformed("a union field offset past `i32`"))?;
            let doff_i32 = i32::try_from(doff).map_err(|_| malformed("a union field offset past `i32`"))?;
            w.write_tag(dtag, j, dst_at);
            let pv = w.load(srepr, (src_at.0, src_at.1 + soff_i32));
            w.store(pv, drepr, (dst_at.0, dst_at.1 + doff_i32))
        };
        match self.ls.get(src_id).variants {
            Variants::Single { index } if index < src_len => {
                let idx = usize::try_from(index).map_err(|_| malformed("a union variant past `usize`"))?;
                let sm = *src_members.get(idx).ok_or(malformed("a union variant past its members"))?;
                let j = dst_members
                    .iter()
                    .position(|&dm| dm == sm)
                    .ok_or(malformed("a remapped union missing the single present source member"))?;
                let j = u32::try_from(j).map_err(|_| malformed("a union variant index past `u32`"))?;
                copy_one(self, index, j)?;
                Ok(())
            }
            Variants::Single { .. } => Ok(()),
            Variants::Multiple { .. } => {
                let d = self.read_discriminant(src_id, src_at)?;
                let join = self.create();
                let none = self.create();
                let mut cases: blaze_own::Vec<(u128, Block), A> = blaze_own::Vec::new_in(self.alloc);
                let mut arms: blaze_own::Vec<(Block, u32, u32), A> = blaze_own::Vec::new_in(self.alloc);
                for (i_idx, &sm) in src_members.iter().enumerate() {
                    let Some(j_idx) = dst_members.iter().position(|&dm| dm == sm) else {
                        continue;
                    };
                    let i = u32::try_from(i_idx).map_err(|_| malformed("a union variant index past `u32`"))?;
                    let j = u32::try_from(j_idx).map_err(|_| malformed("a union variant index past `u32`"))?;
                    let case = self.discriminant_case(src_id, i)?;
                    let block = self.create();
                    cases.push((case, block));
                    arms.push((block, i, j));
                }
                self.switch(d, none, &cases);
                for &(block, i, j) in arms.iter() {
                    self.begin(block);
                    copy_one(self, i, j)?;
                    self.jump(join, &[]);
                }
                self.begin(none);
                self.unreachable_of_enum(src_id, src_len, at_expr)?;
                self.begin(join);
                Ok(())
            }
        }
    }

    /// Adjustment `n` of `e`, an unsizing of the value on top (lib.rs, "Wide pointers";
    /// codegen.md, "Trait objects"): a pointer, or a struct holding one at any depth (`Box<T>`,
    /// `Pin<Box<T>>`, a user `Wrap<T>(u32, Box<T>)`), followed by structure with the other
    /// fields moved to their offsets in the target: walk/unsize_nested.rs.
    fn unsize(&mut self, e: ExprId, n: usize) -> Result<(), BuildError> {
        self.unsize_nested(e, n)
    }

    /// Adjustment `n` of `e`, an unsizing: the pointee before and after, past every struct
    /// around the pointer and every struct tail of the pointee (walk/unsize_nested.rs).
    fn unsize_ends(&mut self, e: ExprId, n: usize) -> Result<UnsizeEnds, BuildError> {
        let before = self.ty_before(e, n)?;
        let after = self.checked.adjusts.column::<2>()[self.adjust_rows(e).start + n];
        let gb = self.ground_ty(before).map_err(|error| Self::ty_err(e, error))?;
        let ga = self.ground_ty(after).map_err(|error| Self::ty_err(e, error))?;
        let unsize_nested::UnsizeWalk { from, to, .. } = self.unsize_walk(e, gb, ga, None)?;
        Ok(UnsizeEnds { from, to })
    }

    /// The principal trait of ground trait object `t` and its arguments after `Self` (its first
    /// predicate when that is an `ExTrait`: the one form puts it first), `None` for `dyn Send`.
    fn principal(&self, e: ExprId, t: PTy) -> Result<Option<(ItemKey, List)>, BuildError> {
        let view = self.ground.view();
        let PNode::Dynamic(l) = view.node(t).map_err(|_| malformed("a type past the ground table"))? else {
            return Err(malformed("a trait object's principal asked of another type"));
        };
        let preds = view.list(l).map_err(|_| malformed("a trait object's predicates past the ground table"))?;
        let Some(&first) = preds.first() else {
            return Err(unsupported(e, "a trait object with no predicates"));
        };
        Ok(match view.node(first).map_err(|_| malformed("a type past the ground table"))? {
            PNode::ExTrait(k, args) => Some((k, args)),
            _ => None,
        })
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
        let l = match repr {
            Repr::Agg(l) => l,
            // A field of a zero-sized aggregate is zero-sized.
            Repr::None => return Ok((0, Repr::None)),
            _ => return Err(unsupported(e, "a field of a value that is not a struct or a tuple")),
        };
        let Shape::Fields { first, len } = self.layouts[l as usize].shape else {
            return Err(malformed("a field of an array"));
        };
        if at >= len {
            return Err(malformed("a field index past its aggregate's fields"));
        }
        let (_, off, repr) = self.layout_fields[(first + at) as usize];
        Ok((off, repr))
    }

    /// The place `v` is; in unreachable code a value that diverged is any pointer.
    fn place_any(&mut self, v: V) -> Result<(Value, i32), BuildError> {
        match v {
            V::Place(p, off) => Ok((p, off)),
            V::None if !self.live => Ok((self.b.uninit(Ty::PTR), 0)),
            V::None => Err(malformed("an expression with no value where a place was expected")),
            V::Scalar(_) => Err(malformed("a scalar where a place was expected")),
            V::Unsized(..) => Err(malformed("an unsized place where a sized one was expected")),
        }
    }

    /// The element place of index expression `e`, from the index on top and the array's place
    /// under it: the index is compared with the length, and out of range the program's
    /// `panic_bounds_check` is called with both (it never returns).
    /// Pushes the tasks that leave the base of index expression `base[..]`: an array's place,
    /// or for a slice reached through a reference (`w[i]` with `w: &[T]`, autoderef's last
    /// step builtin) the wide pointer itself, whose address and length `index_place` reads
    /// (lib.rs, "Wide pointers": a place carries no length, the pointer does).
    fn push_index_base(&mut self, base: ExprId) -> Result<(), BuildError> {
        let t = self.adjusted_ty(base)?;
        if !matches!(self.node_at(base, t)?, PNode::Slice(_)) {
            self.work.push(Task::Place(base));
            return Ok(());
        }
        let rows = self.adjust_rows(base);
        let kinds = &self.checked.adjusts.column::<1>()[rows];
        // A slice place written out with no adjustment: a struct's slice tail (`p.body[i]`,
        // `body: [u8]` last; walk/unsized_tail.rs), or an explicit dereference of a wide pointer
        // (`(*slice)[i]` with `slice: &mut [T]`, core's `SliceIndex<[T]> for usize`). An
        // unsized type is never a value, so the base is a place expression (the Reference,
        // "Index expressions": "For other types an index expression `a[b]` is equivalent to
        // `*std::ops::Index::index(&a, b)`" applies only past the builtin `[T]`), and its place
        // is held as its wide pointer (`V::Unsized`).
        if kinds.is_empty() && self.is_place_expr(base)? {
            self.work.push(Task::Place(base));
            return Ok(());
        }
        let lead = kinds.iter().take_while(|k| matches!(k, AdjustKind::Deref { .. })).count();
        if lead == 0 || lead != kinds.len() {
            return Err(unsupported(base, "indexing a slice not reached by a dereference of a wide pointer"));
        }
        // `w[i]` with `W: Deref<Target = [T]>`: the last step calls `Deref::deref`, and the
        // slice's place is the unsized place behind the wide reference it returns (`place_of`:
        // the chain, then `DerefPlace`; `deref_call`).
        if matches!(kinds[lead - 1], AdjustKind::Deref { overloaded: Some(_) }) {
            self.work.push(Task::Place(base));
            return Ok(());
        }
        if lead == 1 {
            self.raw(base)?;
        } else {
            self.work.push(Task::LoadAdjusted(base, (lead - 2) as u32));
            self.work.push(Task::DerefPlace);
            self.push_deref_chain(base, lead - 1);
        }
        Ok(())
    }

    fn index_place(&mut self, e: ExprId) -> Result<(), BuildError> {
        let Expr::Index { base, .. } = self.expr(e)? else { return Err(malformed("an index task on another expression")) };
        let i = self.pop()?;
        let i = self.scalar(i, Ty::I64)?;
        let v = self.pop()?;
        let t = self.adjusted_ty(base)?;
        let elem = self.expr_repr(e)?;
        let (at, n) = match self.node_at(base, t)? {
            PNode::Array(_, len) => {
                let len = self.ground_len(len).map_err(|error| Self::ty_err(base, error))?;
                (self.place_any(v)?, self.b.iconst(Ty::I64, u128::from(len)))
            }
            // `v` is the wide pointer (`push_index_base`), or the slice's place, which is held
            // as its wide pointer (`V::Unsized`): its address at 0, length at 8.
            PNode::Slice(_) => {
                let w = match v {
                    V::Unsized(p, off) => (p, off),
                    v => self.place_any(v)?,
                };
                let p = self.b.load(Ty::PTR, Flags::NONE, w.0, w.1, MemAttrs::aligned(3));
                let len = self.b.load(Ty::I64, Flags::NONE, w.0, w.1 + 8, MemAttrs::aligned(3));
                ((p, 0), len)
            }
            _ => return Err(unsupported(e, "an overloaded index (`Index::index`)")),
        };
        let bounds = self.adts.lang_fn(BOUNDS_FN).ok_or(BuildError::NoLangFn { expr: Some(e), name: BOUNDS_FN })?;
        let ok = self.create();
        let fail = self.create();
        let inside = self.b.icmp(IntCond::Ult, i, n);
        self.br(inside, (ok, &[]), (fail, &[]));
        self.begin(fail);
        let callee = Instance { item: bounds, args: List::EMPTY };
        let usize_ = Param::of(Ty::I64);
        // core's `panic_bounds_check` is `#[track_caller]`: the location is passed last.
        let (sig, values) = self.lang_call_sig(Some(e), callee, &[usize_, usize_], &[i, n])?;
        let place = self.ref_place(callee);
        let reference = self.b.reference(RefKind::Func, place);
        // In this block `i` and `n` are the values the branch compared. The panic may unwind:
        // called as every unwinding call is, so the live values are dropped on the way out
        // (walk/unwind.rs; the Reference, "Panic": unwinding runs destructors).
        self.call_unwinding(Callee::Direct(reference), sig, &values)?;
        self.unreachable_of(TrapKind::AfterPanic, Some(e));
        self.begin(ok);
        let (stride, _) = self.size_align(elem);
        let v = if stride == 0 {
            V::Place(at.0, at.1)
        } else {
            let s = self.b.iconst(Ty::I64, u128::from(stride));
            let off = self.b.binary(Op::Mul, Flags::NONE, i, s);
            let p = self.b.ptr_add(Flags::NONE, at.0, off);
            V::Place(p, at.1)
        };
        self.push_val(v, None);
        Ok(())
    }

    /// Pushes the tasks of ctor call `e` of variant `variant` with arguments `args`: the tag,
    /// then each field from its argument, in a temporary (lib.rs, "Enums").
    fn construct(&mut self, e: ExprId, variant: u32, args: &[ExprId]) -> Result<(), BuildError> {
        match self.expr_repr(e)? {
            // Zero-sized: the arguments run for their effects.
            Repr::None => {
                self.work.push(Task::PushNone);
                for &a in args.iter().rev() {
                    self.work.push(Task::Discard);
                    self.work.push(Task::Eval(a));
                }
            }
            repr @ Repr::Agg(l) => {
                let (first, len, tag) = self.variant_rows(l, variant)?;
                if args.len() != len as usize {
                    return Err(malformed("a ctor call whose arguments are not its variant's fields"));
                }
                let at = self.temp(repr);
                self.write_tag(tag, variant, at);
                // Partly built while its arguments run, owned part by part (walk/val_stack.rs).
                self.push_literal(at);
                self.work.push(Task::LitBuilt { l });
                for (i, &a) in args.iter().enumerate().rev() {
                    let (_, off, repr) = self.layout_fields[first as usize + i];
                    self.work.push(Task::InitAt { off, repr });
                    self.work.push(Task::Eval(a));
                }
            }
            _ => return Err(malformed("a ctor call of a type held as a scalar")),
        }
        Ok(())
    }

    /// `const_eval_select::<ARG, F, G, RET>(arg, const_fn, rt_fn)` at run time: a call of
    /// `G`'s fn item with the fields of the tuple `arg`, in order (`gl` the call's generic
    /// arguments, its `n` arguments on top of the value stack).
    fn const_eval_select(&mut self, e: ExprId, gl: List, n: usize) -> Result<(), BuildError> {
        let gs = self.ground.view().list(gl).map_err(|_| malformed("a list past the ground table"))?;
        let (&[g_arg, _, g_rt, g_ret], 3) = (gs, n) else {
            return Err(malformed("a `const_eval_select` call not of four generic arguments and three arguments"));
        };
        let PNode::FnDef(item, args) = self.ground_node(g_rt).map_err(|error| Self::ty_err(e, error))? else {
            return Err(unsupported(e, "`const_eval_select` of a run-time fn that is not a fn item"));
        };
        let PNode::Tuple(fl) = self.ground_node(g_arg).map_err(|error| Self::ty_err(e, error))? else {
            return Err(malformed("`const_eval_select`'s argument is not a tuple"));
        };
        let base = self.vals.len().checked_sub(n).ok_or(malformed("an intrinsic's arguments missing from the value stack (a build bug)"))?;
        let tuple = self.vals[base];
        self.truncate_vals(base);
        let trepr = self.repr(g_arg).map_err(|error| Self::ty_err(e, error))?;
        let at = match trepr {
            Repr::Agg(_) => Some(self.place(tuple, trepr)?),
            _ => None,
        };
        let fields = self.ground.view().list(fl).map_err(|_| malformed("a list past the ground table"))?.len();
        let abi = self.adts.abi(item).ok_or(BuildError::NoAbi { item })?;
        let mut reprs = Vec::with_capacity_in(fields, self.alloc);
        let mut passes = Vec::with_capacity_in(fields, self.alloc);
        for i in 0..fields {
            let ft = self.ground.view().list(fl).map_err(|_| malformed("a list past the ground table"))?[i];
            let repr = self.repr(ft).map_err(|error| Self::ty_err(e, error))?;
            let v = match (trepr, at) {
                (Repr::Agg(l), Some(at)) => {
                    let Shape::Fields { first, .. } = self.layouts[l as usize].shape else {
                        return Err(malformed("a tuple not laid out as fields"));
                    };
                    let (_, off, frepr) = self.layout_fields[first as usize + i];
                    self.load(frepr, (at.0, at.1 + off as i32))
                }
                // A zero-sized tuple: no field holds a value. A tuple is never a scalar (`repr`).
                (Repr::None, _) => V::None,
                _ => return Err(malformed("`const_eval_select`'s argument tuple held as a scalar")),
            };
            self.push_val(v, None);
            passes.push(self.pass(ft, repr, abi, Some(item))?);
            reprs.push(repr);
        }
        let rrepr = self.repr(g_ret).map_err(|error| Self::ty_err(e, error))?;
        let rpass = self.pass(g_ret, rrepr, abi, Some(item))?;
        self.call_instance(Some(e), Instance { item, args }, &reprs, &passes, None, rrepr, rpass)
    }

    /// Intrinsic call `e`, its arguments on top (lib.rs, "Intrinsics").
    fn intrinsic(&mut self, e: ExprId, which: Intrinsic) -> Result<(), BuildError> {
        let Expr::Call { callee, args } = self.expr(e)? else { return Err(malformed("an intrinsic's task on another expression")) };
        let list = self.body.expr_lists.get(args.range()).ok_or(malformed("a call's arguments past `expr_lists`"))?;
        let which = match which {
            Intrinsic::Table(which) => which,
            // A target intrinsic (THREADS.md D1, INTRINSICS.md P7b): a barrier or hint, or a
            // generic float function (walk/target_intr.rs).
            Intrinsic::Target(t) => return self.target_intrinsic(e, list, t),
        };
        // The generic arguments, grounded: a type stays a type; a const argument is a
        // `ConstValue` (portable.rs) and reaches `build` as its value (intrinsics.rs, "The
        // seam"): a `bool`, integer or `char` as its bits; a fieldless enum's value (under
        // `adt_const_params`, core's `AtomicOrdering`: "These values must match the compiler's
        // `AtomicOrdering`") is its VARIANT INDEX in the ground table and becomes the variant's
        // discriminant through the one discriminant table (`ls`, rule 30).
        let t = self.expr_ty(callee)?;
        let PNode::FnDef(_, gl) = self.node_at(callee, t)? else { return Err(malformed("an intrinsic call whose callee is not a fn item")) };
        if which == Which::ConstEvalSelect {
            return self.const_eval_select(e, gl, list.len());
        }
        if which == Which::CatchUnwind {
            return self.catch_unwind(e, list.len());
        }
        // Decided here by `T`'s layout: nothing, or the `panic_nounwind` call (walk/validity.rs).
        if let Some(check) = intrinsics::validity::Check::of(which) {
            return self.validity_check(e, which, check, gl, list.len());
        }
        let mut generics = Vec::new_in(self.alloc);
        let n_generics = self.ground.view().list(gl).map_err(|_| malformed("a list past the ground table"))?.len();
        for i in 0..n_generics {
            let g = *self.ground.view().list(gl).map_err(|_| malformed("a list past the ground table"))?.get(i).ok_or(malformed("a list past the ground table"))?;
            let arg = match self.ground_node(g).map_err(|error| Self::ty_err(callee, error))? {
                PNode::ConstValue { ty, bits } => match self.ground_node(ty).map_err(|error| Self::ty_err(callee, error))? {
                    PNode::Bool | PNode::Int(_) | PNode::Uint(_) | PNode::Char => GenericArg::Const(bits),
                    PNode::Adt(..) => {
                        let id = self.ls_of(ty).map_err(|error| Self::ty_err(callee, error))?;
                        let variant = u32::try_from(bits).map_err(|_| malformed("an enum const argument whose variant index passes 32 bits"))?;
                        let d = self.ls.discriminant(id, variant).ok_or(BuildError::Intrinsic {
                            expr: e,
                            which,
                            why: "a const generic argument of an ADT type that is not a fieldless enum's variant",
                            unsupported: true,
                        })?;
                        // Two's complement in 128 bits: `build` reads an ordering as 0..=4 and
                        // refuses any other value by name.
                        GenericArg::Const(d as u128)
                    }
                    _ => {
                        return Err(BuildError::Intrinsic {
                            expr: e,
                            which,
                            why: "a const generic argument of a type that is not `bool`, an integer, `char` or a fieldless enum",
                            unsupported: true,
                        });
                    }
                },
                PNode::ConstParam(_) => return Err(malformed("a const generic parameter left in an intrinsic call's grounded arguments")),
                _ => GenericArg::Ty(g),
            };
            generics.push(arg);
        }
        // The arguments as `build` takes them: a value `ls` lays out as one scalar is that
        // scalar (a newtype's too), any other aggregate its address.
        let n = list.len();
        let base = self.vals.len().checked_sub(n).ok_or(malformed("an intrinsic's arguments missing from the value stack (a build bug)"))?;
        let mut cargs = Vec::with_capacity_in(n, self.alloc);
        for (i, &a) in list.iter().enumerate() {
            let v = self.vals[base + i];
            let at = self.adjusted_ty(a)?;
            let g = self.ground_ty(at).map_err(|error| Self::ty_err(a, error))?;
            let repr = self.repr_at(a, at)?;
            // `simd_shuffle`'s indices ("`U` must be a **const** vector of `u32`s", stdarch's
            // `const { SimdShuffleIdx(..) }`): the address of the constant's own data row, which
            // intrinsics/simd_vec.rs reads them from, never the copy the operand was built as.
            if which == Which::Simd(intrinsics::simd::SimdOp::Shuffle) && i == 2 {
                if let Some(row) = self.const_operand_row(a)? {
                    cargs.push(Arg::Place(row));
                    continue;
                }
            }
            cargs.push(match repr {
                Repr::None => Arg::None,
                Repr::Agg(_) => {
                    let place = self.place(v, repr)?;
                    let id = self.ls_of(g).map_err(|error| Self::ty_err(a, error))?;
                    let l = *self.ls.get(id);
                    match l.scalar {
                        Some(s) => Arg::Scalar(self.b.load(prim_ty(s.prim), Flags::NONE, place.0, place.1, MemAttrs::aligned(l.align))),
                        None => Arg::Place(self.address(place)),
                    }
                }
                scalar => Arg::Scalar(self.scalar(v, scalar.scalar().ok_or(malformed("a scalar of no type"))?)?),
            });
        }
        self.truncate_vals(base);
        let rt = self.expr_ty(e)?;
        let ret = self.ground_ty(rt).map_err(|error| Self::ty_err(e, error))?;
        let rrepr = self.repr_at(e, rt)?;
        // Only `needs_drop` asks: answered before the call, by the walk's drop rules.
        let answer = match (which, generics.first()) {
            (Which::NeedsDrop, Some(&GenericArg::Ty(t0))) => self.needs_drop(t0).map_err(|error| Self::ty_err(e, error))?,
            _ => false,
        };
        let needs_drop = move |_: PTy| answer;
        // `caller_location`: the location this fn was passed, or one of this call's place
        // (walk/track_caller.rs); its result is `&'static Location<'static>`.
        let site = match which {
            Which::CallerLocation => match self.ground_node(ret).map_err(|error| Self::ty_err(e, error))? {
                PNode::Ref(_, loc_ty) => Some((self.caller_location_site(e)?, loc_ty)),
                _ => return Err(BuildError::Intrinsic { expr: e, which, why: "`caller_location` whose result is not a reference", unsupported: false }),
            },
            _ => None,
        };
        let call = Call { which, generics: &generics, args: &cargs, ret, site };
        let adts = self.adts;
        let built = {
            let mut lcx = LayoutCx { ground: &mut *self.ground, facts: adts.facts(), scratch: self.scratch };
            let mut icx = IntrinsicCx { layouts: &mut self.ls, cx: &mut lcx, needs_drop: &needs_drop, ub_checks: adts.ub_checks(), overflow_checks: adts.overflow_checks() };
            intrinsics::build(&mut self.b, &call, &mut icx)
        };
        let out = built.map_err(|error| match error {
            IntrinsicError::Shape { which, why } => BuildError::Intrinsic { expr: e, which, why, unsupported: false },
            IntrinsicError::Unsupported { which, why } => BuildError::Intrinsic { expr: e, which, why, unsupported: true },
            IntrinsicError::Layout(error) => Self::ty_err(e, layout_err(error)),
        })?;
        if which == Which::Unreachable {
            self.mark_last_unreachable(TrapKind::Intrinsic, Some(e))?;
        }
        let v = match out {
            // No value (intrinsics.rs `Out::None`: `()`, or a zero-sized result). A zero-sized
            // result that `repr` holds in memory (`Layout::placed`: a part needs drop, as core's
            // `transmute_unchecked::<[T; 0], [MaybeUninit<T>; 0]>` in `<[T; N] as
            // IntoIterator>::into_iter` at a `T` that needs drop, clap's `Id`, t1545) is still a
            // value the body binds and moves: a fresh zero-sized temporary, of which the
            // intrinsic writes no byte (it has none). A result with bytes, or a scalar, with no
            // value in live code is refused here, by name, not at the later read (rule 3).
            Out::None => match rrepr {
                Repr::None => V::None,
                _ if !self.live => V::None,
                Repr::Agg(l) if self.layouts[l as usize].size == 0 => {
                    let at = self.temp(rrepr);
                    V::Place(at.0, at.1)
                }
                Repr::Agg(_) => return Err(malformed("an intrinsic with no value whose result type has bytes")),
                _ => return Err(malformed("an intrinsic with no value whose result type is a scalar")),
            },
            Out::Diverges => {
                self.dead_block();
                V::None
            }
            Out::Scalar(x) => match rrepr {
                Repr::None => V::None,
                // A newtype of a scalar: the scalar at its offset 0.
                Repr::Agg(_) => {
                    let at = self.temp(rrepr);
                    let attrs = self.mem(rrepr);
                    self.b.store(Flags::NONE, x, at.0, at.1, attrs);
                    V::Place(at.0, at.1)
                }
                _ => V::Scalar(x),
            },
            // A `(T, bool)`: written at the tuple's offsets.
            Out::Pair(x, flag) => {
                let Repr::Agg(l) = rrepr else { return Err(malformed("an intrinsic's pair result of a type not held as a tuple")) };
                let Shape::Fields { first, len: 2 } = self.layouts[l as usize].shape else {
                    return Err(malformed("an intrinsic's pair result of a type not of two fields"));
                };
                let at = self.temp(rrepr);
                for (i, part) in [x, flag].into_iter().enumerate() {
                    let (_, off, repr) = self.layout_fields[first as usize + i];
                    self.store(V::Scalar(part), repr, (at.0, at.1 + off as i32))?;
                }
                V::Place(at.0, at.1)
            }
            // The value at an address (a transmuted argument's own memory): copied into a
            // temporary of its own, which a binding may take as its home.
            Out::At(addr) => match rrepr {
                Repr::None => V::None,
                Repr::Agg(_) => {
                    let to = self.temp(rrepr);
                    self.copy(rrepr, (addr, 0), to)?;
                    V::Place(to.0, to.1)
                }
                scalar => self.load(scalar, (addr, 0)),
            },
        };
        self.push_val(v, None);
        Ok(())
    }

    /// The value of const `item`, named by path `e`, at the arguments the check recorded for the
    /// path (`Checked::const_args_of`: `[Self, trait args..]` of `<T as Tr>::C`, the impl's of an
    /// inherent impl's constant, none for a free `const`) grounded at this instance's: the
    /// Reference, "Associated Items", "Associated Constants": `<T as Tr>::C` at `T = u8` is
    /// `u8`'s impl's value, so `size_of::<u8>` and `size_of::<u64>` read different records.
    fn const_value(&mut self, e: ExprId, item: ItemKey) -> Result<V, BuildError> {
        let args = self.const_path_args(e)?;
        let adts = self.adts;
        let record = adts.const_value(item, &args[..]).ok_or(BuildError::NoConstValue { expr: e, item })?;
        self.const_record_value(e, record)
    }

    /// The value of const generic parameter `index` of the body's item, named by path `e` (the
    /// Reference, "Const generics": "const parameters may be used ... as a standalone
    /// expression"; core's `as_chunks::<N>` reads `N != 0`). It is the instance's argument at
    /// that index, a `ConstValue` of the ground table: a `bool`, integer or `char` is its bits
    /// as a scalar constant. A fieldless enum's value (`adt_const_params`) is refused by name.
    fn const_param_value(&mut self, e: ExprId, index: u32) -> Result<V, BuildError> {
        let g = *self.args.get(index as usize).ok_or(malformed("a const parameter past the instance's arguments"))?;
        let PNode::ConstValue { bits, .. } = self.ground_node(g).map_err(|error| Self::ty_err(e, error))? else {
            return Err(malformed("a const parameter's argument that is not a value in the ground table"));
        };
        match self.expr_repr(e)? {
            Repr::None => Ok(V::None),
            Repr::Agg(_) => Err(unsupported(e, "a const generic parameter of an enum type (`adt_const_params`) as a value")),
            scalar => {
                let ty = scalar.scalar().ok_or(malformed("a scalar of no type"))?;
                // The bits past the type's width are 0 (`const_scalar`): a negative value
                // written sign-extended is cut to the type.
                let width = ty.bits(PTR_BITS);
                let value = if width >= 128 { bits } else { bits & ((1u128 << width) - 1) };
                Ok(V::Scalar(self.const_scalar(ty, value)?))
            }
        }
    }

    /// The generic arguments the check recorded for constant path `e`, grounded at this
    /// instance's arguments (as [`Self::method_callee`] grounds a callee's); empty for a path
    /// with no row (a constant with no generics).
    fn const_path_args(&mut self, e: ExprId) -> Result<Vec<PTy, A>, BuildError> {
        let Some(list) = self.checked.const_args_of(e) else {
            return Ok(Vec::new_in(self.alloc));
        };
        let tys = self.types.list(list).map_err(|_| malformed("a constant path's arguments past the record's types"))?;
        let mut grounded = Vec::with_capacity_in(tys.len(), self.alloc);
        for &t in tys {
            grounded.push(self.ground_ty(t).map_err(|error| Self::ty_err(e, error))?);
        }
        Ok(grounded)
    }

    /// The address of the data row of the constant operand `e` (parentheses peeled): a
    /// `const { .. }` block's record or a constant path's, as `Expr::ConstBlock` and
    /// [`Self::const_value`] read them. `None` for any other expression.
    fn const_operand_row(&mut self, e: ExprId) -> Result<Option<Value>, BuildError> {
        let mut x = e;
        while let Expr::Paren(inner) = self.expr(x)? {
            x = inner;
        }
        let adts = self.adts;
        let record = match self.expr(x)? {
            Expr::ConstBlock { .. } => {
                let owner = self.body.owner;
                adts.const_block_value(owner, x, self.args).ok_or(BuildError::NoConstBlockValue { expr: x, owner })?
            }
            Expr::Path(_) => match self.expr_def(x) {
                Some(Def::Const(item)) => {
                    let args = self.const_path_args(x)?;
                    adts.const_value(item, &args[..]).ok_or(BuildError::NoConstValue { expr: x, item })?
                }
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        Ok(Some(self.data_rows(x, record)?))
    }

    /// The value of expression `e` from evaluated record `record` (lib.rs, "Consts"): a constant
    /// path's or a `const { .. }` block's (one home for both): a scalar is a constant of the
    /// root's bytes, little-endian at the type's width; anything else is the place of the root's
    /// data row, its relocations to the record's other allocations (each a data row of its own),
    /// to statics and to fns.
    fn const_record_value(&mut self, e: ExprId, record: &DataRecord<Global>) -> Result<V, BuildError> {
        let repr = self.expr_repr(e)?;
        self.const_record_value_as(e, repr, record)
    }

    /// [`Self::const_record_value`] of a value held as `repr` (a field default's, the field's
    /// representation), read for expression `e`.
    fn const_record_value_as(&mut self, e: ExprId, repr: Repr, record: &DataRecord<Global>) -> Result<V, BuildError> {
        let bad = |why| BuildError::Malformed { what: why };
        record.check().map_err(|_| bad("a const's record that breaks the data form"))?;
        let root = record.allocs.first().ok_or(bad("a const's record with no allocation"))?;
        match repr {
            Repr::None => return Ok(V::None),
            Repr::Agg(_) => {}
            scalar => {
                let ty = scalar.scalar().ok_or(malformed("a scalar of no type"))?;
                if !root.relocs.is_empty() {
                    // A pointer to data: the root's own row holds it, read as any place.
                    let at = self.data_rows(e, record)?;
                    return Ok(self.load(scalar, (at, 0)));
                }
                let width = (ty.bits(PTR_BITS) / 8) as usize;
                let bytes = root.bytes.get(..width).ok_or(bad("a const's bytes shorter than its type"))?;
                let mut value = 0u128;
                for (i, &byte) in bytes.iter().enumerate() {
                    value |= u128::from(byte) << (8 * i);
                }
                return Ok(V::Scalar(self.const_scalar(ty, value)?));
            }
        }
        let at = self.data_rows(e, record)?;
        // A copy, as any use of a const is a fresh value: its row is never written.
        let to = self.temp(repr);
        self.copy(repr, (at, 0), to)?;
        Ok(V::Place(to.0, to.1))
    }

    /// `Task::InitDefaults(e)`: the place of base-less struct expression `e` (RFC 3681) on
    /// top, its written fields stored. Each field it does not write (`Checked::default_fields`)
    /// is stored from its default: a constant over the ADT's generics, whose value at the
    /// expression's type's ground arguments was pulled before the walk (front2 const_at.rs
    /// `needs_of`) and is read as a constant path's is ([`Adts::const_value`]).
    fn init_defaults(&mut self, e: ExprId) -> Result<(), BuildError> {
        let Some(&V::Place(p, at)) = self.vals.last() else {
            return Err(malformed("a struct expression's defaults without its place"));
        };
        let Repr::Agg(l) = self.expr_repr(e)? else {
            return Err(malformed("a struct expression's defaults of a value held as other than an aggregate"));
        };
        let t = self.expr_ty(e)?;
        let g = self.ground_ty(t).map_err(|error| Self::ty_err(e, error))?;
        let mut args: Vec<PTy, A> = Vec::new_in(self.alloc);
        let variant = match self.ground_node(g).map_err(|error| Self::ty_err(e, error))? {
            PNode::Adt(_, list) => {
                args.extend_from_slice(self.ground.view().list(list).map_err(|_| malformed("an ADT's arguments past the ground table"))?);
                Self::def_variant(self.expr_def(e)).ok_or(malformed("a struct literal the check did not resolve to a struct or variant"))?
            }
            PNode::Record(..) => 0,
            _ => return Err(malformed("a struct expression with defaults of a type that is not an ADT or record")),
        };
        let (first, len, _) = self.variant_rows(l, variant)?;
        let checked: &'x Checked<'c> = self.checked;
        let adts = self.adts;
        let (idx, keys) = checked.default_fields_of(e);
        for (&i, &key) in idx.iter().zip(keys.iter()) {
            if i >= len {
                return Err(malformed("a field default past its variant's fields"));
            }
            let (_, off, frepr) = self.layout_fields[(first + i) as usize];
            let record = adts.const_value(key, &args[..]).ok_or(BuildError::NoConstValue { expr: e, item: key })?;
            let v = self.const_record_value_as(e, frepr, record)?;
            self.store(v, frepr, (p, at + off as i32))?;
        }
        Ok(())
    }

    /// The address of the static path `e` names (lib.rs, "Statics"): `data_addr` of its
    /// `Static` reference, or `tls_addr` of its `Tls` one for a `#[thread_local]`.
    fn static_addr(&mut self, e: ExprId) -> Result<Value, BuildError> {
        let Some(Def::Static(item, _)) = self.expr_def(e) else { return Err(malformed("a static's address of a path to other than a static")) };
        let s = self.adts.static_item(item).ok_or(BuildError::NoStatic { expr: e, item })?;
        let place = self.static_place(item, s.thread_local)?;
        Ok(match s.thread_local {
            true => {
                let r = self.b.reference(RefKind::Tls, place);
                self.b.tls_addr(r)
            }
            false => {
                let r = self.b.reference(RefKind::Static, place);
                self.b.data_addr(r)
            }
        })
    }

    /// Static `item`'s reference place: its index in `thread_locals` or `statics`.
    fn static_place(&mut self, item: ItemKey, thread_local: bool) -> Result<u32, BuildError> {
        let list = match thread_local {
            true => &mut self.thread_locals,
            false => &mut self.statics,
        };
        let place = match list.iter().position(|&k| k == item) {
            Some(at) => at,
            None => {
                list.push(item);
                list.len() - 1
            }
        };
        u32::try_from(place).map_err(|_| malformed("more than 2^32 statics"))
    }

    /// The address of `instance`'s code: `func_addr` of its `Func` reference.
    fn fn_addr(&mut self, instance: Instance) -> Value {
        let place = self.ref_place(instance);
        let r = self.b.reference(RefKind::Func, place);
        self.b.func_addr(r)
    }

    /// The address of `table`: `data_addr` of its `Vtable` reference, one place per table. The
    /// unsizing to `dyn` (codegen.md, "Trait objects") pairs it with the data pointer.
    fn vtable_addr(&mut self, table: crate::vtable::VtableRef) -> Result<Value, BuildError> {
        let place = match self.vtables.iter().position(|&t| t == table) {
            Some(at) => at,
            None => {
                self.vtables.push(table);
                self.vtables.len() - 1
            }
        };
        let place = u32::try_from(place).map_err(|_| malformed("more than 2^32 vtables"))?;
        let r = self.b.reference(RefKind::Vtable, place);
        Ok(self.b.data_addr(r))
    }

    fn ref_place(&mut self, instance: Instance) -> u32 {
        match self.refs.iter().position(|&k| k == instance) {
            Some(at) => at as u32,
            None => {
                self.refs.push(instance);
                (self.refs.len() - 1) as u32
            }
        }
    }

    /// `[x; N]`: the value on top stored `N` times into the array's temporary under it,
    /// unrolled up to `UNROLL_LEAVES` leaves and in a loop beyond.
    fn repeat(&mut self, e: ExprId) -> Result<(), BuildError> {
        let v = self.pop()?;
        let repr = self.expr_repr(e)?;
        let Repr::Agg(l) = repr else {
            // A zero-sized array: the element was evaluated, and has nowhere to go.
            self.push_val(V::None, None);
            return Ok(());
        };
        let Shape::Array { elem, len, stride } = self.layouts[l as usize].shape else {
            return Err(malformed("an array literal of a type that is not an array"));
        };
        let to = self.temp(repr);
        if self.layouts[l as usize].leaves <= UNROLL_LEAVES {
            for i in 0..len {
                self.store(v, elem, (to.0, to.1 + (i * stride) as i32))?;
            }
        } else {
            // The loop's source is the element itself: the same place at every index.
            let (i, _, tp, header, exit) = self.array_loop(len, stride, to, to);
            self.store(v, elem, (tp, 0))?;
            let one = self.b.iconst(Ty::I64, 1);
            let next = self.b.binary(Op::Add, Flags::NONE, i, one);
            self.jump(header, &[next]);
            self.begin(exit);
        }
        self.push_val(V::Place(to.0, to.1), None);
        Ok(())
    }

    /// Pushes the tasks that evaluate `e` as a place.
    fn place_of(&mut self, e: ExprId) -> Result<(), BuildError> {
        let rows = self.adjust_rows(e);
        let kinds = &self.checked.adjusts.column::<1>()[rows.start..rows.end];
        // An autoderef of builtin steps only: `*`, once per step.
        if kinds.iter().all(|k| *k == AdjustKind::Deref { overloaded: None }) {
            let n = kinds.len() as u32;
            return self.place_derefs(e, n);
        }
        // Dereferences, some overloaded: the place the pointer they leave points to.
        if kinds.iter().all(|k| matches!(k, AdjustKind::Deref { .. })) {
            self.work.push(Task::DerefPlace);
            self.push_deref_chain(e, kinds.len());
            return Ok(());
        }
        // A borrow or a coercion makes a new value: its place is a temporary of its own.
        self.work.push(Task::SpillAdjusted(e));
        self.eval_adjusted(e, rows)
    }

    /// Pushes the tasks that evaluate `e` as a place after `n` builtin dereferences of its
    /// value; its own adjustments are the caller's.
    fn place_derefs(&mut self, e: ExprId, n: u32) -> Result<(), BuildError> {
        // A closure body's use of a place its environment holds (walk/moves/env_places.rs).
        if self.env_use(e, n, false)? {
            return Ok(());
        }
        if n > 0 {
            // Step down one builtin dereference at a time so `env_use` is checked at every
            // intermediate deref depth (e.g. `r: &mut &S` captured as `*r` with `n = 2`).
            self.work.push(Task::DerefPlace);
            if n == 1 {
                self.work.push(Task::Raw(e));
            } else {
                self.work.push(Task::LoadAdjusted(e, n - 2));
                self.work.push(Task::PlaceDerefs(e, n - 1));
            }
            return Ok(());
        }
        match self.expr(e)? {
            Expr::Local(p) => {
                let (home, _) = self.slots.get(p.ix()).copied().flatten().ok_or(malformed("a local used before its binding"))?;
                let at = self.home(home);
                self.push_val(V::Place(at.0, at.1), None);
            }
            Expr::Path(_) if matches!(self.expr_def(e), Some(Def::Static(..))) => {
                let at = self.static_addr(e)?;
                self.push_val(V::Place(at, 0), None);
            }
            Expr::Paren(inner) => self.work.push(Task::Place(inner)),
            Expr::Field { base, .. } | Expr::TupleField { base, .. } => {
                self.work.push(Task::FieldPlace(e));
                self.work.push(Task::Place(base));
            }
            // The builtin element or an overloaded index's place (walk/index.rs).
            Expr::Index { .. } => self.push_index(e, false)?,
            // `*x` overloaded: the check put the dereference on `x` itself, so `x`'s adjusted
            // place is the place.
            Expr::Unary { op: UnOp::Deref, operand } if self.deref_on_operand(operand) => {
                self.work.push(Task::Place(operand));
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
        self.super_let_form(s, stmt)?;
        // A statement is a temporary scope: its end drops its borrowed temporaries, after its
        // own value and its `let`'s binding (walk/temps.rs).
        self.stmt_temps(stmt)?;
        match stmt {
            // `let P = e else { .. };`: walk/lets.rs.
            Stmt::Let { init: Some(init), else_: Some(_), .. } => self.let_else(s, init)?,
            Stmt::Let { init: None, else_: Some(_), .. } => return Err(malformed("a `let`-`else` without an initializer")),
            // `let _ = <place>` reads nothing and moves nothing; `let _ = <value>` drops the
            // value at once.
            Stmt::Let { pat, init: Some(init), else_: None, .. } if matches!(self.pat(pat)?, Pat::Wild) => {
                self.discard(init)?;
            }
            Stmt::Let { pat, init: Some(init), else_: None, .. } => {
                self.work.push(Task::Let(pat, Some(init)));
                // `let ref x = <place>;`: a `let` initializer is a place expression context
                // (the Reference, "Place Expressions and Value Expressions"): its place, not its
                // value (walk/matches/ref_bind.rs).
                if self.let_borrows_place(pat, init)? {
                    self.work.push(Task::Place(init));
                } else if self.let_owns_parts(pat, init)? {
                    // A pattern leaving a part that needs drop unbound (`let (a, _) = pair;`):
                    // the initializer as a scrutinee, a local in place (walk/lets.rs
                    // `push_scrutinee`; walk/matches/irrefutable.rs `bind_let_owned`).
                    self.push_scrutinee(init)?;
                } else {
                    self.work.push(Task::Eval(init));
                }
            }
            // `let x;`, `let (a, b);`: walk/matches/irrefutable.rs `declare_let`.
            Stmt::Let { pat, init: None, else_: None, .. } => {
                let from = self.scope_pats.len();
                self.declare_let(pat)?;
                self.place_super_let(pat, from)?;
            }
            // An expression statement's value is a temporary dropped at its end.
            Stmt::Expr { expr, .. } => {
                self.work.push(Task::DropValue(expr));
                self.work.push(Task::Eval(expr));
            }
            Stmt::Item => {}
        }
        Ok(())
    }

    /// Pushes the tasks of `e` (its adjustments are the caller's), or evaluates a leaf.
    fn raw(&mut self, e: ExprId) -> Result<(), BuildError> {
        // A closure body's use of a place its environment holds (walk/moves/env_places.rs).
        if self.env_use(e, 0, true)? {
            return Ok(());
        }
        match self.expr(e)? {
            Expr::Missing => return Err(unsupported(e, "a missing expression")),
            Expr::Lit(lit) => self.lit(e, lit)?,
            Expr::Local(p) => {
                let (home, repr) = self.slots.get(p.ix()).copied().flatten().ok_or(malformed("a local used before its binding"))?;
                // A use by value: a move, when its type needs drop (its memory is read where
                // the value lands, before anything else runs that could drop it).
                self.moved(e, p)?;
                let at = self.home(home);
                let v = self.load(repr, at);
                self.push_val(v, None);
            }
            Expr::Path(_) => {
                let t = self.expr_ty(e)?;
                match (self.node_at(e, t)?, self.expr_def(e)) {
                    (PNode::FnDef(..), _) => self.push_val(V::None, None),
                    (_, Some(Def::Const(item))) => {
                        let v = self.const_value(e, item)?;
                        self.push_val(v, None);
                    }
                    // A const generic parameter: the instance's argument.
                    (_, Some(Def::TyParam(index))) => {
                        let v = self.const_param_value(e, index)?;
                        self.push_val(v, None);
                    }
                    // A static by value: a copy read from its memory.
                    (_, Some(Def::Static(..))) => {
                        let at = self.static_addr(e)?;
                        let repr = self.expr_repr(e)?;
                        let v = match repr {
                            Repr::Agg(_) => {
                                let to = self.temp(repr);
                                self.copy(repr, (at, 0), to)?;
                                V::Place(to.0, to.1)
                            }
                            _ => self.load(repr, (at, 0)),
                        };
                        self.push_val(v, None);
                    }
                    // A unit struct or unit variant: its tag, if it has one, in a temporary.
                    (PNode::Adt(..), Some(def @ (Def::Ctor { kind: CtorKind::Unit, .. } | Def::Adt(_)))) => {
                        let variant = match def {
                            Def::Ctor { variant, .. } => variant,
                            // A unit struct: its only variant.
                            Def::Adt(_) => 0,
                            _ => return Err(malformed("a unit value's path naming neither a ctor nor an ADT")),
                        };
                        match self.expr_repr(e)? {
                            Repr::Agg(l) => {
                                let (_, _, tag) = self.variant_rows(l, variant)?;
                                let at = self.temp(Repr::Agg(l));
                                self.write_tag(tag, variant, at);
                                self.push_val(V::Place(at.0, at.1), None);
                            }
                            Repr::None => self.push_val(V::None, None),
                            _ => return Err(malformed("a unit struct or variant held as a scalar")),
                        }
                    }
                    _ => return Err(unsupported(e, "a path to other than a fn item, a unit struct or a unit variant (a const, a static)")),
                }
            }
            Expr::Paren(inner) => self.work.push(Task::Eval(inner)),
            Expr::Block { stmts, tail, .. } => {
                let list = self.body.stmt_lists.get(stmts.range()).ok_or(malformed("a block's statements past `stmt_lists`"))?;
                let depth = self.scope_marks.len() as u32;
                if self.break_target[e.ix()] {
                    let repr = self.expr_repr(e)?;
                    let exit = self.create();
                    self.targets.push(Target { expr: e, header: None, exit, repr, depth });
                    self.work.push(Task::BlockExit(e));
                }
                // The block's drop scope: its bindings are dropped after its value is made.
                self.scope_marks.push(self.scope_pats.len() as u32);
                self.work.push(Task::ScopeEnd(e));
                match tail {
                    Some(t) => self.work.push(Task::Eval(t)),
                    None => self.work.push(Task::PushNone),
                }
                for &s in list.iter().rev() {
                    self.work.push(Task::Stmt(s));
                }
            }
            // A condition holding a `let` (`if let`, `while let`, a let chain): walk/lets.rs.
            Expr::If { cond, then, else_ } if self.cond_has_let(cond)? => self.if_let(e, cond, then, else_)?,
            Expr::If { cond, then, else_ } => {
                self.work.push(Task::If { expr: e, then, else_ });
                self.work.push(Task::Eval(cond));
            }
            // A `let` is built as a link of its `if` condition or its match guard (walk/lets.rs
            // `if_let`, `let_guard`), never evaluated alone; reaching one here (a `let` under
            // `||`, `!` or parentheses, which Rust refuses) is refused by name.
            Expr::Let { .. } => return Err(unsupported(e, "`let` outside an `if` or `while` condition")),
            // walk/fmt.rs: the arguments, then `Task::FormatArgs`.
            Expr::FormatArgs { args, uses } => self.format_args(e, args, uses)?,
            // coroutine.rs: the operand, then `Task::Await`; the state, its tag `Unresumed`.
            Expr::Await(operand) => self.await_operand(e, operand)?,
            Expr::AsyncBlock { .. } => self.coroutine_value(e)?,
            // walk/asm.rs: the operands' values and places, then `Task::Asm`.
            Expr::Asm { .. } => self.asm_operands(e)?,
            // `_ = e` is built by its assignment (walk/discard.rs); `_` anywhere else is the
            // check's `UnderscoreExpr` error.
            Expr::Underscore => return Err(malformed("`_` outside the whole left side of an assignment (the check's `UnderscoreExpr`)")),
            Expr::RawRef { mutbl, operand } => self.push_raw_ref(e, mutbl, operand)?,
            // The Reference, "Block expressions", "Const blocks": the block is evaluated at compile
            // time with the enclosing generics in scope, its value the expression's; one value
            // per instance (`size_of::<T>()` in it differs at `u8` and `u64`): the front's
            // record of it at this instance's ground arguments, read as a constant path's is.
            Expr::ConstBlock { .. } => {
                let adts = self.adts;
                let owner = self.body.owner;
                let record = adts.const_block_value(owner, e, self.args).ok_or(BuildError::NoConstBlockValue { expr: e, owner })?;
                let v = self.const_record_value(e, record)?;
                self.push_val(v, None);
            }
            // Its value is the container's layout: a field's offset, which the code builder
            // does not compute yet.
            Expr::OffsetOf { ty, fields } => {
                let v = self.offset_of(e, ty, fields)?;
                self.push_val(v, None);
            }
            Expr::Match { scrutinee, .. } => {
                self.work.push(Task::Match(e));
                // A local that needs drop is a place expression: matched in place, not moved
                // (codegen.md, "A `match` by value on an enum that needs drop").
                let mut s = scrutinee;
                while let Expr::Paren(inner) = self.expr(s)? {
                    s = inner;
                }
                match self.expr(s)? {
                    Expr::Local(p) if self.local_drops(p) && self.adjust_rows(scrutinee).is_empty() => {
                        self.work.push(Task::Place(s));
                    }
                    // An unsized scrutinee (`match *w { [a, .., d] => .. }`, `w: Deref<Target =
                    // [u8]>`, or `*s` of `s: &[u8]`): the Reference, "Match expressions": "A
                    // match behaves differently depending on whether or not the scrutinee
                    // expression is a place expression"; an unsized value is never copied, so
                    // its place (`V::Unsized`, the wide pointer's memory) is what is tested.
                    _ if self.is_place_expr(scrutinee)?
                        && matches!(self.node_at(scrutinee, self.adjusted_ty(scrutinee)?)?, PNode::Slice(_) | PNode::Str) =>
                    {
                        self.work.push(Task::Place(scrutinee));
                    }
                    // A place behind a pointer whose type needs drop (`matches!(*self, Ok(_))`,
                    // `match self.kind { .. }` with `self: &S`): matched in place; only a by-value
                    // binding would move (walk/moves.rs `scrutinee_in_place`).
                    _ if self.scrutinee_in_place(scrutinee)? => self.work.push(Task::Place(scrutinee)),
                    // A field of an owned local that needs drop (`match self.failed`, `self:
                    // Held`): a place the local owns, tested where it is; a by-value binding
                    // moves its part by the local's field flags (walk/moves/field_scrutinee.rs).
                    _ if self.owned_field_scrutinee(scrutinee)?.is_some() => self.work.push(Task::Place(scrutinee)),
                    _ => self.work.push(Task::Eval(scrutinee)),
                }
            }
            Expr::Loop { body } => {
                let repr = self.expr_repr(e)?;
                let header = self.create();
                let exit = self.create();
                self.jump(header, &[]);
                self.begin(header);
                let depth = self.scope_marks.len() as u32;
                self.targets.push(Target { expr: e, header: Some(header), exit, repr, depth });
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
                let start = self.scope_start(t.depth);
                self.drop_from(start)?;
                self.jump(header, &[]);
                self.dead_block();
                self.push_val(V::None, None);
            }
            Expr::Return { value } => {
                self.work.push(Task::Return(value.is_some()));
                if let Some(v) = value {
                    self.work.push(Task::Eval(v));
                }
            }
            Expr::Call { callee, args } => {
                let list = self.body.expr_lists.get(args.range()).ok_or(malformed("a call's arguments past `expr_lists`"))?;
                let t = self.adjusted_ty(callee)?;
                let item = match self.node_at(callee, t)? {
                    PNode::FnDef(item, _) => item,
                    // Through a fn pointer: its value first, then the arguments, then the call
                    // (lib.rs, "Fn pointers").
                    PNode::FnPtr(..) => {
                        self.work.push(Task::Call(e));
                        for &a in list.iter().rev() {
                            if let Repr::Agg(_) = self.adjusted_repr(a)? {
                                self.work.push(Task::ArgCopy(a));
                            }
                            self.work.push(Task::Eval(a));
                        }
                        self.work.push(Task::Eval(callee));
                        return Ok(());
                    }
                    // A closure called directly: a call of its body, the instance of its kind's
                    // `Fn*` method, with its environment as `self` (closure.rs).
                    PNode::Closure { .. } if !self.through_fn_trait(e) => return self.push_closure_call(e, callee, list),
                    // An overloaded call through `Fn*` (the checker's callee row): the callee is
                    // `self`, evaluated with its adjustments (the borrow of `call`/`call_mut`),
                    // then the arguments, each a field of the method's tuple (lib.rs
                    // `Abi::RustCall`).
                    _ if self.checked.callees.column::<0>().contains(&e) => {
                        self.work.push(Task::Call(e));
                        for &a in list.iter().rev().chain([&callee]) {
                            if let Repr::Agg(_) = self.adjusted_repr(a)? {
                                self.work.push(Task::ArgCopy(a));
                            }
                            self.work.push(Task::Eval(a));
                        }
                        return Ok(());
                    }
                    _ => return Err(unsupported(e, "a call of other than a fn item, a fn pointer or a `Fn*` impl")),
                };
                // A tuple struct's or tuple variant's ctor: the value built in place.
                if let Some(Def::Ctor { variant, kind: CtorKind::Tuple, .. }) = self.expr_def(callee) {
                    return self.construct(e, variant, list);
                }
                if let Some(which) = self.adts.intrinsic(item) {
                    self.work.push(Task::Intrinsic(e, which));
                    for &a in list.iter().rev() {
                        self.work.push(Task::Eval(a));
                    }
                    return Ok(());
                }
                // A `#[rustc_legacy_const_generics]` argument is the callee's const generic, not a
                // value it is passed (`Self::legacy_listed`).
                let listed = self.legacy_listed(callee, item, list.len())?;
                self.work.push(Task::Call(e));
                for (k, &a) in list.iter().enumerate().rev() {
                    if listed.contains(&(k as u32)) {
                        continue;
                    }
                    // An aggregate argument gets its caller's copy at once, before the next
                    // argument runs.
                    if let Repr::Agg(_) = self.adjusted_repr(a)? {
                        self.work.push(Task::ArgCopy(a));
                    }
                    self.work.push(Task::Eval(a));
                }
                self.work.push(Task::Discard);
                self.work.push(Task::Eval(callee));
            }
            // A method call is a call of the method the check picked, the receiver (with its
            // adjustments: the autoderef, the autoref) its first argument.
            Expr::MethodCall { receiver, args, .. } => {
                let list = self.body.expr_lists.get(args.range()).ok_or(malformed("a call's arguments past `expr_lists`"))?;
                self.work.push(Task::Call(e));
                for &a in list.iter().rev().chain([&receiver]) {
                    if let Repr::Agg(_) = self.adjusted_repr(a)? {
                        self.work.push(Task::ArgCopy(a));
                    }
                    self.work.push(Task::Eval(a));
                }
            }
            Expr::Field { base, .. } | Expr::TupleField { base, .. } => {
                self.work.push(Task::Field(e));
                self.work.push(Task::Place(base));
            }
            // `*x` overloaded: the dereference is `x`'s own last adjustment, so the value of `*x`
            // is `x`'s adjusted value.
            Expr::Unary { op: UnOp::Deref, operand } if self.deref_on_operand(operand) => {
                self.work.push(Task::Eval(operand));
            }
            Expr::Unary { op: UnOp::Deref, operand } => {
                let t = self.adjusted_ty(operand)?;
                match self.node_at(operand, t)? {
                    PNode::Ptr(..) | PNode::Ref(..) => {}
                    _ => return Err(malformed("a dereference of other than a pointer with no overloaded dereference recorded")),
                }
                self.work.push(Task::Deref(e));
                self.work.push(Task::Eval(operand));
            }
            // walk/operators.rs: an operator with the check's callee row is that call; one
            // with none is the builtin operator, on its builtin scalar types only.
            Expr::Unary { op, operand } => {
                if self.push_overloaded(e)? {
                    return Ok(());
                }
                self.builtin_unary(e, op, operand)?;
                self.work.push(Task::Unary(e, op));
                self.work.push(Task::Eval(operand));
            }
            Expr::Binary { op: op @ (BinOp::And | BinOp::Or), lhs, rhs } => {
                self.work.push(Task::Short { expr: e, op, rhs });
                self.work.push(Task::Eval(lhs));
            }
            Expr::Binary { op, lhs, rhs } => {
                if self.push_overloaded(e)? {
                    return Ok(());
                }
                self.builtin_binary(e, op, lhs, rhs, false)?;
                self.work.push(Task::Binary(e, op));
                self.work.push(Task::Eval(rhs));
                self.work.push(Task::Eval(lhs));
            }
            // `_ = rhs` is `{ let _ = rhs; }` (the Reference, "Destructuring assignments";
            // walk/discard.rs), its value `()`.
            Expr::Assign { lhs, rhs } if self.assigns_to_underscore(lhs)? => {
                self.work.push(Task::PushNone);
                self.discard(rhs)?;
            }
            // The right side first, then the place: Rust's order for a primitive assignment.
            Expr::Assign { lhs, rhs } => {
                self.refuse_mut_through_overloaded_index(lhs)?;
                self.work.push(Task::Assign(e));
                self.work.push(Task::Place(lhs));
                // An aggregate right side is a place read late, at the store: copied first when
                // building the left side's place could write it (`a[f()] = b`).
                if let Repr::Agg(_) = self.adjusted_repr(rhs)?
                    && !self.effect_free_place(lhs)?
                {
                    self.work.push(Task::ArgCopy(rhs));
                }
                self.work.push(Task::Eval(rhs));
            }
            Expr::AssignOp { op, lhs, rhs } => {
                self.refuse_mut_through_overloaded_index(lhs)?;
                if self.push_overloaded(e)? {
                    return Ok(());
                }
                self.builtin_binary(e, op, lhs, rhs, true)?;
                self.work.push(Task::AssignOp(e, op));
                self.work.push(Task::Place(lhs));
                self.work.push(Task::Eval(rhs));
            }
            // The Reference, "Constant promotion": a borrow the checker promoted is of a
            // `'static` slot: the address of the value's constant row (walk/promote.rs).
            Expr::Ref { mutbl: Mutbl::Not, .. } if self.is_promoted(e) => self.promoted_ref(e)?,
            Expr::Ref { operand, mutbl } => {
                if mutbl == Mutbl::Mut {
                    self.refuse_mut_through_overloaded_index(operand)?;
                }
                self.work.push(Task::Ref);
                self.work.push(Task::Place(operand));
            }
            Expr::Cast { operand, .. } => {
                self.work.push(Task::Cast(e));
                self.work.push(Task::Eval(operand));
            }
            Expr::Struct { base: blaze_typeck::StructBase::Expr(_), .. }
            | Expr::Record { base: blaze_typeck::StructBase::Expr(_), .. } => self.struct_update(e)?,
            // `S { a: 1, .. }` (RFC 3681) is built as `S { a: 1, b: B_DEFAULT }` would be: the
            // written fields in written order, then each unwritten field from its default's
            // value at this instance (`Task::InitDefaults`: `Checked::default_fields`, each a
            // constant pulled before the walk by front2 const_at.rs `needs_of`).
            Expr::Struct { fields, base: base @ (blaze_typeck::StructBase::None | blaze_typeck::StructBase::Defaults), .. }
            | Expr::Record { fields, base: base @ (blaze_typeck::StructBase::None | blaze_typeck::StructBase::Defaults) } => {
                let defaults = base == blaze_typeck::StructBase::Defaults
                    || !self.checked.default_fields_of(e).0.is_empty();
                let is_record = matches!(self.expr(e)?, Expr::Record { .. });
                let repr = self.expr_repr(e)?;
                let inits = self.body.field_inits.get(fields.range()).ok_or(malformed("a struct literal's fields past `field_inits`"))?;
                match repr {
                    // A zero-sized struct: its fields are evaluated, in order, for their effects.
                    Repr::None => {
                        self.work.push(Task::PushNone);
                        for init in inits.iter().rev() {
                            self.work.push(Task::Discard);
                            self.work.push(Task::Eval(init.expr));
                        }
                    }
                    Repr::Agg(l) => {
                        let variant = if is_record {
                            0
                        } else {
                            Self::def_variant(self.expr_def(e))
                                .ok_or(malformed("a struct literal the check did not resolve to a struct or variant"))?
                        };
                        let (first, len, tag) = self.variant_rows(l, variant)?;
                        // A union literal names exactly one field (the reference, "Unions").
                        let named = if self.layouts[l as usize].is_union { 1 } else { len as usize };
                        // With defaults: the fields written and those taken from defaults.
                        let taken = if defaults { self.checked.default_fields_of(e).0.len() } else { 0 };
                        if inits.len() + taken != named {
                            return Err(malformed("a struct literal that does not name every field, or a union literal not one"));
                        }
                        let at = self.temp(repr);
                        self.write_tag(tag, variant, at);
                        // Partly built while its fields run, owned part by part
                        // (walk/val_stack.rs); one value once the last is stored.
                        self.push_literal(at);
                        self.work.push(Task::LitBuilt { l });
                        // Runs last: the defaults are constants, stored once every written
                        // field's expression has run (as their operands are in MIR).
                        if defaults {
                            self.work.push(Task::InitDefaults(e));
                        }
                        for init in inits.iter().rev() {
                            let row = (first..first + len)
                                .find(|&i| self.layout_fields[i as usize].0 == init.name)
                                .ok_or(malformed("a struct literal naming a field its struct lacks"))?;
                            let (_, off, repr) = self.layout_fields[row as usize];
                            self.work.push(Task::InitAt { off, repr });
                            self.work.push(Task::Eval(init.expr));
                        }
                    }
                    _ => return Err(unsupported(e, "a struct literal of other than a struct")),
                }
            }
            Expr::Tuple { elems } | Expr::Array { elems } => {
                let list = self.body.expr_lists.get(elems.range()).ok_or(malformed("a literal's elements past `expr_lists`"))?;
                let repr = self.expr_repr(e)?;
                match repr {
                    // `()` or a zero-sized aggregate: the elements are evaluated for their effects.
                    Repr::None => {
                        self.work.push(Task::PushNone);
                        for &x in list.iter().rev() {
                            self.work.push(Task::Discard);
                            self.work.push(Task::Eval(x));
                        }
                    }
                    Repr::Agg(l) => {
                        let shape = self.layouts[l as usize].shape;
                        let at = self.temp(repr);
                        // Partly built while its elements run, owned element by element
                        // (walk/val_stack.rs).
                        self.push_literal(at);
                        self.work.push(Task::LitBuilt { l });
                        for (i, &x) in list.iter().enumerate().rev() {
                            let (off, repr) = match shape {
                                Shape::Fields { first, len } if (i as u32) < len => {
                                    let (_, off, repr) = self.layout_fields[(first + i as u32) as usize];
                                    (off, repr)
                                }
                                Shape::Array { elem, len, stride } if (i as u32) < len => (i as u32 * stride, elem),
                                _ => return Err(malformed("a literal with more elements than its type")),
                            };
                            self.work.push(Task::InitAt { off, repr });
                            self.work.push(Task::Eval(x));
                        }
                    }
                    _ => return Err(malformed("a tuple or array literal of a scalar type")),
                }
            }
            Expr::Repeat { elem, .. } => {
                self.work.push(Task::Repeat(e));
                self.work.push(Task::Eval(elem));
            }
            Expr::Index { .. } => self.push_index(e, true)?,
            // Its environment, built from the captured bindings (closure.rs).
            Expr::Closure { .. } => self.closure_value(e)?,
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
            // Its bytes as data, and the pair of their address and length (lib.rs, "Wide pointers").
            Lit::Str => {
                let text = self.adts.str_lit(self.body.owner, e).ok_or(BuildError::NoLiteralValue { expr: e })?;
                let len = text.len() as u128;
                let data = self.b.data(text.as_bytes(), 0, &[]);
                let p = self.b.data_addr(data);
                let repr = self.expr_repr(e)?;
                let at = self.temp(repr);
                self.b.store(Flags::NONE, p, at.0, at.1, MemAttrs::aligned(3));
                let n = self.b.iconst(Ty::I64, len);
                self.b.store(Flags::NONE, n, at.0, at.1 + 8, MemAttrs::aligned(3));
                self.push_val(V::Place(at.0, at.1), None);
                return Ok(());
            }
            // strs.rs: a wide pointer over the bytes and their nul; a `char` as its `u32`.
            Lit::CStr | Lit::Char => {
                let v = if matches!(lit, Lit::CStr) { self.c_str_lit(e)? } else { self.char_lit(e)? };
                self.push_val(v, None);
                return Ok(());
            }
            Lit::Float(_) => {
                let v = self.float_lit(e)?;
                self.push_val(v, None);
                return Ok(());
            }
        };
        self.push_val(V::Scalar(v), None);
        Ok(())
    }

    /// An arithmetic or bitwise operation by the Reference (walk/arith_checks.rs): division and
    /// remainder checked in every build, `+ - *` and shifts checked when
    /// [`Adts::overflow_checks`] says so and wrapping otherwise; a shift's amount is brought to
    /// the left side's width first.
    fn arith(&mut self, e: ExprId, op: BinOp, l: Value, r: Value, lrepr: Repr, rrepr: Repr) -> Result<Value, BuildError> {
        let (ty, signed) = match lrepr {
            Repr::Int { ty, signed } => (ty, signed),
            Repr::Bool if matches!(op, BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor) => (Ty::I8, false),
            _ => return Err(unsupported(e, "arithmetic on this type (a pointer, a bool)")),
        };
        let checks = matches!(lrepr, Repr::Int { .. }) && self.adts.overflow_checks().on();
        match op {
            BinOp::Div | BinOp::Rem if matches!(lrepr, Repr::Int { .. }) => return self.checked_div(e, op, l, r, ty, signed),
            BinOp::Add | BinOp::Sub | BinOp::Mul if checks => return self.checked_add_sub_mul(e, op, l, r, signed),
            BinOp::Shl | BinOp::Shr if checks => {
                let Repr::Int { ty: rty, .. } = rrepr else { return Err(unsupported(e, "a shift by a non-integer")) };
                self.check_shift(e, op, r, rty, ty)?;
            }
            _ => {}
        }
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
        // Of the operands a builtin binary operator takes, only a wide raw pointer is an
        // aggregate (walk/wide_cmp.rs: `==` and `!=` word by word).
        if matches!(lrepr, Repr::Agg(_)) || matches!(rrepr, Repr::Agg(_)) {
            let v = self.wide_ptr_binary(e, op, lhs, (l, lrepr), (r, rrepr))?;
            self.push_val(v, None);
            return Ok(());
        }
        let lty = lrepr.scalar().ok_or(unsupported(e, "a binary operator on an operand of no value"))?;
        let rty = rrepr.scalar().ok_or(unsupported(e, "a binary operator on an operand of no value"))?;
        let l = self.scalar(l, lty)?;
        let r = self.scalar(r, rty)?;
        // walk/numbers.rs: IEEE comparisons and arithmetic.
        if let Repr::Float(_) = lrepr {
            let v = self.float_binary(e, op, l, r)?;
            self.push_val(V::Scalar(v), None);
            return Ok(());
        }
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
        self.push_val(V::Scalar(v), None);
        Ok(())
    }

    fn cast(&mut self, e: ExprId) -> Result<(), BuildError> {
        let v = self.pop()?;
        let Expr::Cast { operand, .. } = self.expr(e)? else { return Err(malformed("a cast task on another expression")) };
        let from = self.adjusted_repr(operand)?;
        let to = self.expr_repr(e)?;
        let out = match (from, to) {
            (Repr::None, Repr::None) => V::None,
            (Repr::Int { ty: fty, signed }, Repr::Int { ty, .. }) => {
                let x = self.scalar(v, fty)?;
                V::Scalar(self.resize(x, fty, ty, signed))
            }
            // `bool as int` is 0 or 1, zero-extended (the Reference, "Numeric cast"): an `i8`.
            (Repr::Bool, Repr::Int { ty, .. }) => {
                let x = self.scalar(v, Ty::I8)?;
                V::Scalar(self.resize(x, Ty::I8, ty, false))
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
            (Repr::Float(_), _) | (_, Repr::Float(_)) => self.float_cast(e, v, from, to)?,
            // A wide pointer, an enum, a fn item (walk/casts.rs).
            _ => self.cast_other(e, operand, v, from, to)?,
        };
        self.push_val(out, None);
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

    /// The positions of the arguments of call `callee(..)` (`n` of them, of fn item `item`)
    /// that are the callee's own const generic arguments under
    /// `#[rustc_legacy_const_generics(i, ..)]` (`Adts::legacy_const_generics`): the checker's
    /// test (blaze-typeck rules/legacy_const.rs `legacy_sig`) on the callee as written, a path
    /// with no qualified-self anchor and no lang item, naming `item` itself, whose last segment
    /// writes no generic arguments, with every listed position below `n`. Empty when the rule
    /// does not apply: the call then passes every argument. A listed argument is a constant
    /// (a literal, a const parameter, a constant item), so leaving it unevaluated drops no
    /// effect.
    fn legacy_listed(&self, callee: ExprId, item: ItemKey, n: usize) -> Result<&'x [u32], BuildError> {
        let adts: &'x dyn Adts = self.adts;
        let listed = adts.legacy_const_generics(item);
        if listed.is_empty() {
            return Ok(&[]);
        }
        let Expr::Path(p) = self.expr(callee)? else { return Ok(&[]) };
        let path = *self.body.paths.get(p.0 as usize).ok_or(malformed("a callee's path past `paths`"))?;
        if path.anchor.is_some() || path.lang.is_some() || self.expr_def(callee) != Some(Def::Fn(item)) {
            return Ok(&[]);
        }
        let segs = self.body.segments.get(path.segments.range()).ok_or(malformed("a callee's path segments past `segments`"))?;
        match segs.last() {
            Some(last) if !last.has_args => {}
            _ => return Ok(&[]),
        }
        if listed.iter().any(|&i| i as usize >= n) {
            return Ok(&[]);
        }
        Ok(listed)
    }

    fn call(&mut self, e: ExprId) -> Result<(), BuildError> {
        // The arguments in order (a method's receiver first) and the callee instance: its item
        // and its call's arguments, grounded at this instance's.
        let mut list = Vec::new_in(self.alloc);
        // An overloaded call (`f(x)` through `Fn*`): its arguments are written apart already,
        // the fields of the trait method's tuple, so none is spread.
        let mut overloaded = false;
        let instance = match self.expr(e)? {
            Expr::Call { callee, args } => {
                let written = self.body.expr_lists.get(args.range()).ok_or(malformed("a call's arguments past `expr_lists`"))?;
                let t = self.adjusted_ty(callee)?;
                match self.node_at(callee, t)? {
                    PNode::FnDef(item, args) => {
                        // Only the value arguments: the listed ones were not evaluated (`expr`'s
                        // `Expr::Call` arm), they are in `args` as const generics.
                        let listed = self.legacy_listed(callee, item, written.len())?;
                        list.extend(written.iter().enumerate().filter(|&(k, _)| !listed.contains(&(k as u32))).map(|(_, &a)| a));
                        Instance { item, args }
                    }
                    PNode::FnPtr(_, header) => {
                        list.extend_from_slice(written);
                        return self.call_pointer(e, &list, header.abi);
                    }
                    // A closure called directly (closure.rs `push_closure_call`).
                    PNode::Closure { .. } if !self.through_fn_trait(e) => return self.closure_call(e, callee, written),
                    // The checker's `Fn*::call*` at `[callee's type, tuple of the arguments]`,
                    // the callee (adjusted: borrowed for `call` and `call_mut`) its `self`.
                    _ => {
                        overloaded = true;
                        list.push(callee);
                        list.extend_from_slice(written);
                        self.method_callee(e)?
                    }
                }
            }
            Expr::MethodCall { receiver, args, .. } => {
                list.push(receiver);
                list.extend_from_slice(self.body.expr_lists.get(args.range()).ok_or(malformed("a call's arguments past `expr_lists`"))?);
                self.method_callee(e)?
            }
            _ => return Err(malformed("a call task on another expression")),
        };
        if let Some((_, slot)) = self.dyn_slot(e, instance)? {
            self.refuse_tracked_slot(e, instance)?;
            // A "rust-call" method called with its tuple written whole (`(**self).call_mut(args)`
            // in core's `impl FnMut<A> for &mut F` at `F = dyn FnMut(..)`): the slot's fn takes
            // the tuple's fields apart, as a direct call does (`spread_tuple`).
            let spread = !overloaded && self.adts.abi(instance.item) == Some(Abi::RustCall);
            return self.dyn_call(e, instance, &list, slot, spread);
        }
        let item = instance.item;
        let abi = self.adts.abi(item).ok_or(BuildError::NoAbi { item })?;
        let n = list.len();
        let mut reprs = Vec::with_capacity_in(n, self.alloc);
        let mut passes = Vec::with_capacity_in(n, self.alloc);
        // A C-variadic callee: its declared count splits the arguments (walk/c_variadic.rs).
        let declared = self.adts.c_variadic(item).map(|d| d as usize);
        if declared.is_some() {
            Self::c_variadic_abi(e, abi, self.adts.nounwind(item))?;
        }
        for (k, &a) in list.iter().enumerate() {
            let t = self.adjusted_ty(a)?;
            let g = self.ground_ty(t).map_err(|error| Self::ty_err(a, error))?;
            let repr = self.repr_at(a, t)?;
            reprs.push(repr);
            passes.push(match declared {
                Some(d) if k >= d => self.unnamed_c_pass(a, g, repr)?,
                _ => self.pass(g, repr, abi, Some(item))?,
            });
        }
        if abi == Abi::RustCall && !overloaded {
            self.spread_tuple(&mut reprs, &mut passes)?;
        }
        let ty = self.expr_ty(e)?;
        let g = self.ground_ty(ty).map_err(|error| Self::ty_err(e, error))?;
        let rrepr = self.repr_at(e, ty)?;
        let rpass = self.pass(g, rrepr, abi, Some(item))?;
        self.call_instance(Some(e), instance, &reprs, &passes, declared, rrepr, rpass)?;
        if let PNode::Never = self.node_at(e, ty)? {
            self.pop_val();
            self.unreachable_of(TrapKind::AfterDivergingCall, Some(e));
            self.dead_block();
            self.push_val(V::None, None);
        }
        Ok(())
    }

    /// The slot of `instance` when it is a dynamic call: a method of the object's principal or
    /// of a trait above it, with `Self` (its first argument) a trait object. The checker records
    /// no other mark (codegen.md, "Trait objects"). A trait implemented for a `dyn` type by an
    /// impl, or a generic fn at `T = dyn Tr`, has no slot there: an ordinary call.
    fn dyn_slot(&mut self, e: ExprId, instance: Instance) -> Result<Option<(ItemKey, u32)>, BuildError> {
        let view = self.ground.view();
        let Some(&self_ty) = view.list(instance.args).map_err(|_| malformed("a callee's arguments past the ground table"))?.first() else {
            return Ok(None);
        };
        if !matches!(self.ground_node(self_ty).map_err(|error| Self::ty_err(e, error))?, PNode::Dynamic(_)) {
            return Ok(None);
        }
        let Some((principal, _)) = self.principal(e, self_ty)? else {
            return Ok(None);
        };
        let adts = self.adts;
        let shape = crate::vtable::Shape::of(principal, |t| adts.object_trait(t), self.scratch)
            .map_err(|error| BuildError::Vtable { at: Some(e), error })?;
        if let Some(slot) = shape.slot(instance.item) {
            return Ok(Some((principal, slot)));
        }
        // No slot. A method of a trait on the object's own tree (the principal or a supertrait)
        // with `Self` the object is still a dynamic call by the Reference ("Trait objects"; no
        // impl of such a trait for the object type can be written, E0371): built as a direct
        // call it would run the trait's default body, not the concrete type's. Such a method
        // has no slot when the world marks it not dispatchable (`where Self: Sized`, generic,
        // or `self` by value, which `unsized_fn_params` can call on `Box<dyn FnOnce>`): refused
        // by name. A method of another trait (`impl Other for dyn Tr`) is an ordinary call.
        let traits = shape.prefix.iter().copied().chain(shape.words.iter().filter_map(|w| match *w {
            crate::vtable::Word::Super { trait_, .. } => Some(trait_),
            crate::vtable::Word::Method { .. } => None,
        }));
        for t in traits {
            let o = adts.object_trait(t).ok_or(BuildError::Vtable { at: Some(e), error: crate::vtable::VtableError::NoTrait(t) })?;
            if o.methods.iter().any(|m| m.item == instance.item) {
                return Err(unsupported(e, "a method of the object's own trait with no vtable slot (`self` by value, `where Self: Sized` or generic) called on a trait object"));
            }
        }
        Ok(None)
    }

    /// Dynamic call `e` of `instance` at vtable `slot` (codegen.md, "Trait objects"): the
    /// receiver, a wide pointer, is the `list.len()`-th value from the top; `f = load [vtable +
    /// 8 * (3 + slot)]`, then `f(data, args..)`. The callee is the impl's method at the concrete
    /// type, so the receiver is passed in its thin form (`DispatchFromDyn`): `&self` and `&mut
    /// self` as a pointer, `self: Box<Self>` as a `Box` of a sized type. With `spread`, the
    /// last argument is a "rust-call" method's tuple written whole, passed as its fields.
    fn dyn_call(&mut self, e: ExprId, instance: Instance, list: &[ExprId], slot: u32, spread: bool) -> Result<(), BuildError> {
        let item = instance.item;
        let abi = self.adts.abi(item).ok_or(BuildError::NoAbi { item })?;
        let n = list.len();
        let mut reprs = Vec::with_capacity_in(n, self.alloc);
        let mut passes = Vec::with_capacity_in(n, self.alloc);
        let mut wide = Repr::None;
        for (i, &a) in list.iter().enumerate() {
            let t = self.adjusted_ty(a)?;
            let mut g = self.ground_ty(t).map_err(|error| Self::ty_err(a, error))?;
            if i == 0 {
                wide = self.repr_at(a, t)?;
                g = self.thin_receiver(a, g)?;
            }
            let repr = self.repr(g).map_err(|error| Self::ty_err(a, error))?;
            reprs.push(repr);
            passes.push(self.pass(g, repr, abi, Some(item))?);
        }
        let ty = self.expr_ty(e)?;
        let g = self.ground_ty(ty).map_err(|error| Self::ty_err(e, error))?;
        let rrepr = self.repr_at(e, ty)?;
        let rpass = self.pass(g, rrepr, abi, Some(item))?;
        let at = self.vals.len().checked_sub(n).ok_or(malformed("a dynamic call's receiver missing from the value stack (a build bug)"))?;
        if spread {
            if n < 2 {
                return Err(malformed("a \"rust-call\" method of a trait object called without its tuple of arguments"));
            }
            self.spread_tuple(&mut reprs, &mut passes)?;
        }
        let v = core::mem::replace(&mut self.vals[at], V::None);
        let place = self.place(v, wide)?;
        let data = self.load(Repr::Ptr, place);
        let data = self.scalar(data, Ty::PTR)?;
        let vt = self.load(Repr::Ptr, (place.0, place.1 + 8));
        let vt = self.scalar(vt, Ty::PTR)?;
        self.vals[at] = match reprs[0] {
            Repr::Ptr => V::Scalar(data),
            Repr::Agg(_) => {
                let to = self.temp(reprs[0]);
                self.b.store(Flags::NONE, data, to.0, to.1, MemAttrs::aligned(3));
                V::Place(to.0, to.1)
            }
            _ => return Err(malformed("a thin receiver held as other than a pointer or an aggregate")),
        };
        let off = 8 * (crate::vtable::HEADER_WORDS + slot);
        let off = i32::try_from(off).map_err(|_| malformed("a vtable slot past 2^31 bytes"))?;
        let f = self.b.load(Ty::PTR, Flags::NONE, vt, off, MemAttrs::aligned(3));
        self.emit_call(Callee::Pointer(f), &reprs, &passes, rrepr, rpass)?;
        if let PNode::Never = self.node_at(e, ty)? {
            self.pop_val();
            self.unreachable_of(TrapKind::AfterDivergingCall, Some(e));
            self.dead_block();
            self.push_val(V::None, None);
        }
        Ok(())
    }

    /// The thin ABI type of a dynamic receiver. A transparent receiver such as
    /// `Pin<&mut Self>` inherits its pointer field before metadata is removed.
    fn thin_receiver(&mut self, a: ExprId, g: PTy) -> Result<PTy, BuildError> {
        let full = |_| malformed("the ground table is full");
        let unit = self.ground.intern(PNode::Tuple(List::EMPTY)).map_err(full)?;
        let id = self.ls_of(g).map_err(|error| Self::ty_err(a, error))?;
        let g = self.ls.get(id).call_ty;
        let node = match self.ground_node(g).map_err(|error| Self::ty_err(a, error))? {
            PNode::Ref(m, _) => PNode::Ref(m, unit),
            PNode::Ptr(m, _) => PNode::Ptr(m, unit),
            PNode::Adt(k, l) if self.adts.lang_adt(LangItem::OwnedBox) == Some(k) => {
                let view = self.ground.view();
                let mut args = Vec::new_in(self.alloc);
                for &t in view.list(l).map_err(|_| malformed("a type's arguments past the ground table"))? {
                    args.push(match view.node(t) {
                        Ok(PNode::Dynamic(_)) => unit,
                        _ => t,
                    });
                }
                PNode::Adt(k, self.ground.intern_list(&args).map_err(full)?)
            }
            _ => return Err(unsupported(a, "a dynamic receiver whose ABI field is neither a pointer nor the owned box")),
        };
        self.ground.intern(node).map_err(full)
    }

    /// The declared C parameters use `pass`. The ellipsis requires a promoted
    /// scalar, including one stored at offset zero in a transparent argument.
    fn unnamed_c_pass(&mut self, e: ExprId, ty: PTy, repr: Repr) -> Result<Pass, BuildError> {
        if let Repr::Agg(_) = repr {
            let id = self.ls_of(ty).map_err(|error| Self::ty_err(e, error))?;
            let base = self.ls.get(id).call_ty;
            if base != ty {
                let inner = self.repr(base).map_err(|error| Self::ty_err(e, error))?;
                let Pass::Scalar { ty, ext, .. } = Self::vararg_pass(e, inner)? else {
                    return Err(malformed("a promoted C variable argument without a scalar pass"));
                };
                return Ok(Pass::Scalar { ty, ext, agg: true });
            }
        }
        Self::vararg_pass(e, repr)
    }

    /// Call `e` through a fn pointer of ABI `abi`: the pointer under the arguments `list` on
    /// the stack; the signature lowered from the arguments' and the result's types, by the
    /// pointer's ABI (lib.rs, "Fn pointers").
    fn call_pointer(&mut self, e: ExprId, list: &[ExprId], abi: blaze_typeck::Abi) -> Result<(), BuildError> {
        let abi = pointer_abi(abi).ok_or_else(|| unsupported(e, "a fn pointer outside the Rust, rust-call and C-family calling conventions"))?;
        let n = list.len();
        let mut reprs = Vec::with_capacity_in(n, self.alloc);
        let mut passes = Vec::with_capacity_in(n, self.alloc);
        for &a in list.iter() {
            let t = self.adjusted_ty(a)?;
            let g = self.ground_ty(t).map_err(|error| Self::ty_err(a, error))?;
            let repr = self.repr_at(a, t)?;
            reprs.push(repr);
            passes.push(self.pass(g, repr, abi, None)?);
        }
        if abi == Abi::RustCall {
            self.spread_tuple(&mut reprs, &mut passes)?;
        }
        let ty = self.expr_ty(e)?;
        let g = self.ground_ty(ty).map_err(|error| Self::ty_err(e, error))?;
        let rrepr = self.repr_at(e, ty)?;
        let rpass = self.pass(g, rrepr, abi, None)?;
        // The pointer is under the arguments: taken out, so the arguments are the top ones.
        let at = self.vals.len().checked_sub(reprs.len() + 1).ok_or(malformed("a call's pointer missing from the value stack (a build bug)"))?;
        let p = self.remove_val(at);
        let p = self.scalar(p, Ty::PTR)?;
        self.emit_call(Callee::Pointer(p), &reprs, &passes, rrepr, rpass)?;
        if let PNode::Never = self.node_at(e, ty)? {
            self.pop_val();
            self.unreachable_of(TrapKind::AfterDivergingCall, Some(e));
            self.dead_block();
            self.push_val(V::None, None);
        }
        Ok(())
    }

    /// The method that method call `e` calls, as the check resolved it, its arguments grounded at
    /// this instance's.
    fn method_callee(&mut self, e: ExprId) -> Result<Instance, BuildError> {
        let at = self.checked.callees.column::<0>().iter().position(|&x| x == e).ok_or(malformed("a method call with no callee in the record"))?;
        let item = self.checked.callees.column::<1>()[at];
        let generic = self.checked.callees.column::<2>()[at];
        let tys = self.types.list(generic).map_err(|_| malformed("a callee's arguments past the record's types"))?;
        let mut grounded = Vec::with_capacity_in(tys.len(), self.alloc);
        for &t in tys {
            grounded.push(self.ground_ty(t).map_err(|error| Self::ty_err(e, error))?);
        }
        let args = self.ground.intern_list(&grounded).map_err(|_| malformed("the ground table is full"))?;
        Ok(Instance { item, args })
    }

    // Match.

    /// The values pattern `p` matches, into `out`; `true` when it matches every value. Over an
    /// enum of layout `of_enum`, a variant is its discriminant, as `read_discriminant` reads it.
    fn pat_values(&mut self, p: PatId, width: u32, of_enum: Option<LayoutId>, out: &mut Vec<u128, A>) -> Result<bool, BuildError> {
        let refuse = |what| BuildError::UnsupportedPat { pat: p, what };
        let one = |w: &mut Self, alt: PatId, out: &mut Vec<u128, A>| -> Result<bool, BuildError> {
            match w.pat(alt)? {
                Pat::Wild => Ok(true),
                Pat::Lit(Lit::Bool(b)) => {
                    out.push(u128::from(b));
                    Ok(false)
                }
                Pat::Lit(Lit::Int(_) | Lit::Char) | Pat::NegLit(Lit::Int(_)) => {
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
                // A variant: its discriminant (lib.rs, "Enums").
                Pat::TupleStruct { .. } | Pat::Path(_) | Pat::Struct { .. } => {
                    let v = Self::def_variant(w.pat_def(alt)).ok_or(BuildError::UnsupportedPat {
                        pat: alt,
                        what: "a path pattern to other than a struct or a variant (a const)",
                    })?;
                    let id = of_enum.ok_or(BuildError::UnsupportedPat { pat: alt, what: "a variant pattern over other than an enum" })?;
                    out.push(w.discriminant_case(id, v)?);
                    Ok(false)
                }
                _ => Err(BuildError::UnsupportedPat {
                    pat: alt,
                    what: "a pattern other than a literal, a variant, a binding or `_`",
                }),
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

    /// Binds the fields of variant pattern `p` (`V(a, b)`, `V { x, .. }`) from the enum of layout
    /// `l` at `at`, each by copy. Its subpatterns must be irrefutable (`bind_value` refuses the
    /// rest by name). A field that needs drop is moved out by a binding (its `field_bit` in
    /// the result) and left in place by `_`; nothing deeper is built.
    /// `own`: each by-value binding joins the drop scope here; else the caller owns them
    /// (a guarded arm, once its guard passed: walk/moves.rs `Pending`).
    fn bind_variant(&mut self, p: PatId, at: (Value, i32), l: u32, by_ref: bool, own: bool) -> Result<u64, BuildError> {
        let variant = Self::def_variant(self.pat_def(p)).ok_or(malformed("a variant pattern the check did not resolve"))?;
        let (first, len, _) = self.variant_rows(l, variant)?;
        let n = len as usize;
        // (field row, subpattern) pairs.
        let mut binds = Vec::new_in(self.alloc);
        let mut moved = 0u64;
        match self.pat(p)? {
            Pat::TupleStruct { elems, rest, .. } => {
                let subs = self.body.pat_lists.get(elems.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?;
                let fits = match rest {
                    Some(_) => subs.len() <= n,
                    None => subs.len() == n,
                };
                if !fits {
                    return Err(malformed("a variant pattern of another arity than its variant"));
                }
                for (j, &sub) in subs.iter().enumerate() {
                    let k = match rest {
                        Some(r) if j >= r as usize => n - (subs.len() - j),
                        _ => j,
                    };
                    binds.push((first as usize + k, sub));
                }
            }
            Pat::Struct { fields, .. } => {
                let fps = self.body.field_pats.get(fields.range()).ok_or(malformed("a struct pattern's fields past `field_pats`"))?;
                for fp in fps {
                    let row = (first..first + len)
                        .find(|&i| self.layout_fields[i as usize].0 == fp.name)
                        .ok_or(malformed("a struct pattern naming a field its variant lacks"))?;
                    binds.push((row as usize, fp.pat));
                }
            }
            _ => return Err(malformed("a variant's fields bound from a pattern of another kind")),
        }
        for &(row, sub) in binds.iter() {
            let (_, off, repr) = self.layout_fields[row];
            let part = (at.0, at.1 + off as i32);
            if by_ref {
                // Through a reference: a binding is `&field` (its address), `_` reads nothing.
                match self.pat(sub)? {
                    Pat::Wild => {}
                    Pat::Bind { sub: None, .. } => {
                        let a = self.address(part);
                        self.bind_value(sub, V::Scalar(a))?;
                    }
                    _ => {
                        return Err(BuildError::UnsupportedPat {
                            pat: sub,
                            what: "a pattern other than a binding or `_` inside a variant matched through a reference",
                        });
                    }
                }
                continue;
            }
            if self.repr_needs_drop(repr) {
                match self.pat(sub)? {
                    // Left in place: the scrutinee's owner drops it.
                    Pat::Wild => continue,
                    Pat::Bind { ann, sub: None, .. } if ann.by_ref.is_none() => {
                        let bit = self.field_bit(l, variant, row as u32 - first)?.ok_or(BuildError::UnsupportedPat {
                            pat: sub,
                            what: "a move out of a field past an enum's 64th (its field flags are one word)",
                        })?;
                        moved |= 1 << bit;
                    }
                    _ => {
                        return Err(BuildError::UnsupportedPat {
                            pat: sub,
                            what: "a pattern other than a binding or `_` over a field that needs drop, in a `match` by value (a move path deeper than one field)",
                        });
                    }
                }
            }
            let v = self.load(repr, part);
            // The field's own type: a subpattern that peels a reference field
            // (`Some(Foo { a })` over `Option<&Foo>`) is recorded at the peeled one.
            let g = self.layout_field_tys[row];
            self.bind_value_of(sub, v, g)?;
            if own {
                self.own_pat(sub)?;
            }
        }
        Ok(moved)
    }

    fn match_dispatch(&mut self, e: ExprId) -> Result<(), BuildError> {
        let Expr::Match { scrutinee, arms } = self.expr(e)? else { return Err(malformed("a match task on another expression")) };
        let mut s = self.pop()?;
        // An unsized scrutinee's place (`match *w`, pushed as a place by `Expr::Match`) has no
        // repr of its own: it is tested in place by its ground type (`tested_place` takes a
        // `V::Unsized` as the wide pointer's memory, walk/matches.rs reads a slice's elements
        // and length through it), never held as a value.
        if let V::Unsized(..) = s {
            return self.match_tested(e, scrutinee, arms, s, Repr::None);
        }
        let mut srepr = self.adjusted_repr(scrutinee)?;
        // A place behind a pointer that needs drop, matched in place (`Expr::Match` pushed its
        // place): by tests, whose bindings refuse a move out with no owner (walk/moves.rs
        // `scrutinee_in_place`); the switch below would own it as a temporary.
        // A field of an owned local that needs drop alike: the match by tests owns it by the
        // local's field flags (walk/moves.rs `own_scrutinee`); the switch would not.
        if !self.switch_takes(scrutinee, arms, srepr)?
            || self.scrutinee_in_place(scrutinee)?
            || self.owned_field_scrutinee(scrutinee)?.is_some()
        {
            return self.match_tested(e, scrutinee, arms, s, srepr);
        }
        // A reference to an enum, matched by variant patterns: the default binding mode (the
        // reference, "Binding modes"): the references are followed to the enum's place.
        let mut by_ref = false;
        // The enum's ground type when the references were followed to it.
        let mut pointee: Option<PTy> = None;
        let mut owner = Owner::None;
        if srepr == Repr::Ptr {
            let t = self.adjusted_ty(scrutinee)?;
            let mut g = self.ground_ty(t).map_err(|error| Self::ty_err(scrutinee, error))?;
            let mut depth = 0u32;
            while let PNode::Ref(_, inner) = self.ground_node(g).map_err(|error| Self::ty_err(scrutinee, error))? {
                g = inner;
                depth += 1;
            }
            let list = self.body.arms.get(arms.range()).ok_or(malformed("a match's arms past `arms`"))?;
            let mut variant_pats = false;
            for arm in list.iter() {
                match self.pat(arm.pat)? {
                    Pat::Ref { .. } => {
                        return Err(BuildError::UnsupportedPat { pat: arm.pat, what: "a `&` pattern in a match on a reference" });
                    }
                    Pat::TupleStruct { .. } | Pat::Struct { .. } | Pat::Path(..) | Pat::Or { .. } => variant_pats = true,
                    _ => {}
                }
            }
            let is_enum = match self.ground_node(g).map_err(|error| Self::ty_err(scrutinee, error))? {
                PNode::Adt(..) => true,
                PNode::Union(l) => l.len != 1,
                _ => false,
            } && self.is_agg(g).map_err(|error| Self::ty_err(scrutinee, error))?;
            if variant_pats && depth > 0 && is_enum {
                let mut p = self.scalar(s, Ty::PTR)?;
                for _ in 1..depth {
                    p = self.b.load(Ty::PTR, Flags::NONE, p, 0, MemAttrs::aligned(3));
                }
                srepr = self.repr(g).map_err(|error| Self::ty_err(scrutinee, error))?;
                s = V::Place(p, 0);
                by_ref = true;
                pointee = Some(g);
            }
        }
        // An enum's walk row, a zero-sized one's too (no place, a constant discriminant): by
        // value, or through references (`match self` in a derived `Debug` of `enum E { A }`).
        let enum_row = match srepr {
            Repr::Agg(l) => Some(l),
            Repr::None => {
                let g = match pointee {
                    Some(g) => g,
                    None => {
                        let t = self.adjusted_ty(scrutinee)?;
                        self.ground_ty(t).map_err(|error| Self::ty_err(scrutinee, error))?
                    }
                };
                match self.ground_node(g).map_err(|error| Self::ty_err(scrutinee, error))? {
                    PNode::Adt(..) => Some(self.layout(g).map_err(|error| Self::ty_err(scrutinee, error))?),
                    PNode::Union(l) if l.len != 1 => Some(self.layout(g).map_err(|error| Self::ty_err(scrutinee, error))?),
                    _ => None,
                }
            }
            _ => None,
        }
        .filter(|&l| matches!(self.layouts[l as usize].shape, Shape::Enum { .. }));
        let (x, width, of_enum) = match (srepr, enum_row) {
            (Repr::Int { ty, .. }, _) => (self.scalar(s, ty)?, ty.bits(PTR_BITS), None),
            (Repr::Bool, _) => (self.scalar(s, Ty::I8)?, 8, None),
            // An enum: its discriminant, decoded once from the tag by `ls`'s encoding, as an
            // `i64` (lib.rs, "Enums").
            (_, Some(l)) => {
                let Shape::Enum { id, .. } = self.layouts[l as usize].shape else {
                    return Err(malformed("an enum's layout changed shape"));
                };
                // By value, what no arm moves is dropped at the scrutinee's end: a local's
                // field flags, or a temporary's rest per arm (codegen.md, "A `match` by value
                // on an enum that needs drop"); through a reference nothing moves.
                if self.layouts[l as usize].needs_drop && !by_ref {
                    owner = match self.local_of(scrutinee)? {
                        // As `Expr::Match` evaluated it: in place.
                        Some(p) if self.local_drops(p) && self.adjust_rows(scrutinee).is_empty() => Owner::Local(p),
                        // A temporary: held in its temporary scope like every temporary
                        // (walk/temps.rs `hold_temp`, as `Held::Moved` with its word: "The
                        // scrutinee of a `match` expression is not a temporary scope"), wherever
                        // the match stands (a statement, or inside a larger expression such as
                        // `a()?.b()?`), and dropped at that scope's end minus what an arm moved.
                        // One drop point for every temporary: the match by tests owns it.
                        _ => return self.match_tested(e, scrutinee, arms, s, srepr),
                    };
                }
                let at = match srepr {
                    Repr::Agg(_) => self.place(s, srepr)?,
                    // Zero-sized: nothing is read through the place.
                    _ => (self.b.uninit(Ty::PTR), 0),
                };
                let d = self.read_discriminant(id, at)?;
                (d, 64, Some((at, l, id)))
            }
            _ => return Err(unsupported(e, "a match on other than an integer, a bool or an enum")),
        };
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
            let mut values = Vec::new_in(self.alloc);
            let mut cases: Vec<(u128, Block), A> = Vec::new_in(self.alloc);
            let mut default = self.arm_blocks[(first + n) as usize];
            for (i, arm) in list.iter().enumerate() {
                values.clear();
                let always = self.pat_values(arm.pat, width, of_enum.map(|o| o.2), &mut values)?;
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
            self.switch(x, default, &cases);
        }
        self.matches.push(MatchState { expr: e, scrutinee: x, width, of_enum, join, repr, first, arms: n, guarded, by_ref, owner, tested: None });
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
        if let Some(at) = st.tested {
            return self.arm_tested(m, i, arm, at, block, next);
        }
        self.begin(block);
        if st.guarded {
            let mut values = Vec::new_in(self.alloc);
            let always = self.pat_values(arm.pat, st.width, st.of_enum.map(|o| o.2), &mut values)?;
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
        // The arm's drop scope: its bindings drop at its end. (A temporary scrutinee that needs
        // drop is never the switch's: the match by tests holds it in its temporary scope.)
        self.scope_marks.push(self.scope_pats.len() as u32);
        let mut moved = 0u64;
        let mut whole = false;
        // A guarded arm's bindings are stored before its guard, which reads them, but own
        // their parts and take them from the scrutinee only once it passed (walk/moves.rs
        // `Pending`, applied by `Task::ArmMoves`): a false guard leaves the scrutinee whole.
        let hold = arm.guard.is_some();
        let from = self.pending_mark();
        match (self.pat(arm.pat)?, st.of_enum) {
            (Pat::Bind { .. }, None) => self.bind_value(arm.pat, V::Scalar(st.scrutinee))?,
            (Pat::Bind { .. }, Some((at, _, _))) => {
                self.bind_value(arm.pat, V::Place(at.0, at.1))?;
                if st.owner != Owner::None {
                    self.arm_effect(moves::Pending::OwnPat(arm.pat), hold)?;
                    whole = true;
                }
            }
            (Pat::TupleStruct { .. } | Pat::Struct { .. }, Some((at, l, _))) => {
                moved = self.bind_variant(arm.pat, at, l, st.by_ref, !hold)?;
                if hold && !st.by_ref {
                    self.arm_effect(moves::Pending::OwnPat(arm.pat), true)?;
                }
                if moved != 0 {
                    let _variant = Self::def_variant(self.pat_def(arm.pat)).ok_or(malformed("a variant pattern the check did not resolve"))?;
                    if self.layouts[l as usize].drop_fn.is_some() {
                        return Err(BuildError::UnsupportedPat {
                            pat: arm.pat,
                            what: "a move out of an enum that implements `Drop` (E0509)",
                        });
                    }
                    match st.owner {
                        Owner::Local(x) => {
                            let slot = self
                                .field_mask(x)
                                .ok_or(malformed("a match moving fields out of a local without field flags"))?;
                            self.arm_effect(moves::Pending::Clear { mask: slot, bits: moved }, hold)?;
                        }
                        Owner::Temp | Owner::None => return Err(malformed("a field that needs drop moved out of a scrutinee the switch does not own")),
                    }
                }
            }
            (Pat::Or { alts }, Some(_)) => {
                let list = self.body.pat_lists.get(alts.range()).ok_or(malformed("an or-pattern past `pat_lists`"))?;
                for &alt in list {
                    if let Pat::TupleStruct { elems, .. } = self.pat(alt)?
                        && elems.len > 0
                    {
                        return Err(BuildError::UnsupportedPat { pat: alt, what: "an or-pattern of variants with fields" });
                    }
                    if let Pat::Struct { .. } = self.pat(alt)? {
                        return Err(BuildError::UnsupportedPat { pat: alt, what: "an or-pattern of variants with fields" });
                    }
                }
            }
            _ => {}
        }
        if whole {
            match st.owner {
                Owner::Local(x) => self.arm_effect(moves::Pending::Moved { expr: st.expr, x }, hold)?,
                Owner::Temp => return Err(malformed("a switch owning a temporary scrutinee (the match by tests owns those)")),
                Owner::None => {}
            }
        }
        self.work.push(Task::ArmEnd { m });
        self.work.push(Task::ScopeEnd(arm.body));
        // An `if let` guard (walk/lets.rs `let_guard`): its links, each failure to `next`.
        if let Some(g) = arm.guard
            && self.cond_has_let(g)?
        {
            return self.let_guard(g, arm.body, next);
        }
        self.work.push(Task::Eval(arm.body));
        if let Some(g) = arm.guard {
            self.work.push(Task::ArmMoves { from });
            self.work.push(Task::Guard { m, i });
            self.work.push(Task::Eval(g));
        }
        Ok(())
    }
}
