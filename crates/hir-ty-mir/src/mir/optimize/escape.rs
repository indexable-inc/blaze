//! `HeapToStack`, blaze's own pass (rustc has none): an allocation that never leaves its
//! function lives in the function's frame instead of on the heap. It runs at both levels that
//! optimize, where [`MirOptions::heap_to_stack`] is set, before the UB checks (a value it moves
//! is read through no pointer, and the checks of a box's pointer would name the box) and
//! before `BoxInPlace` ([`super::box_in_place`]), which takes only a `Box::new` of at least
//! [`super::BOX_IN_PLACE_MIN`] bytes: far past [`HEAP_TO_STACK_MAX`], so the two never want the
//! same call. A zero-sized `Box` or `vec!` stays: it allocates nothing already.
//!
//! What it moves, each only in the shapes below and refused (left as it is, with the reason
//! under the `heap_to_stack` span of target `blaze`) in any other:
//! - `Box::new(value)` into a local `_b` that is read and written only through `*_b`, dropped,
//!   and freed: `_b` becomes the value. The constructor becomes `_b = value`, `(*_b).f` becomes
//!   `_b.f`, a drop of `_b` or `*_b` drops the value, and the free (`<Box<T> as Drop>::drop`
//!   after the value's drop) goes. Moving the value out (`let v = *b;`) is a read of `_b`.
//! - `Rc::new(value)` into a local `_r` that is only dereferenced (`_t = &_r` read once by
//!   `Deref::deref`) and dropped: never cloned, counted, moved or downgraded, so its count is
//!   one when it is dropped and the drop drops the value and frees. `_r` becomes the value, each
//!   `deref` call a copy of the borrow, which now points at the value.
//! - `vec![a, b, ..]` of elements with nothing to drop, std's chain
//!   `box_assume_init_into_vec_unsafe(write_box_via_move(Box::new_uninit(), [a, b, ..]))`, into
//!   a `Vec` local `_v` that is only borrowed into a temporary read once by `Deref::deref`,
//!   `DerefMut::deref_mut`, `Index::index`, `IndexMut::index_mut`, `IntoIterator::into_iter`
//!   (of `&Vec` and `&mut Vec`) or `Vec`'s `len`, `is_empty`, `capacity`, `as_slice` and
//!   `as_mut_slice`, and dropped. None of those grows or reallocates the buffer or hands out the
//!   `Vec` itself (`as_ptr` would let `ptr::read` copy the `Vec`, and the copy grow), so its
//!   capacity stays the array's. `Box::new_uninit()` becomes a pointer to a new local of the
//!   array's `MaybeUninit`, the rest of the chain runs as it did and builds the `Vec` on that
//!   memory, and the drops of the chain's boxes and of the `Vec` go: they would free it and
//!   drop nothing.
//!
//! `String`, `Vec::with_capacity`, `Arc` (whose drop's `Acquire` fence another thread can
//! see) and a `Vec` pushed to are not taken.
//!
//! # What it relies on
//!
//! `core::alloc::GlobalAlloc`'s contract (`core/src/alloc/global.rs` at the pinned nightly):
//! "you cannot rely on global allocations actually happening, even if there are explicit
//! global allocations in the source. The optimizer may detect unused global allocations that it
//! can either eliminate entirely or move to the stack and thus never invoke the global
//! allocator. The optimizer may further assume that allocation is infallible [..] Whether
//! allocations happen or not is not part of the program behavior, even if it could be detected
//! via an allocator that tracks allocations by printing or otherwise having side effects."
//! LLVM already removes the allocations of rustc's code it can see through.
//!
//! Blaze holds itself to more than that: [`MirOptions::heap_to_stack`] is set only for a body of
//! a program whose global allocator is std's (no `#[global_allocator]`, or one that is a
//! `std::alloc::System`), as `hir-ty-codegen`'s `allocations_unobserved` decides per program,
//! never for a library unit, never for the library's own bodies (which are one for every
//! program; the pass checks the body's crate itself too, for the interpreter's) and never for a
//! body built into another (the inliner's callees and the code generator's always-inlined
//! bodies). A `#[global_allocator]` that counts, logs or fails on purpose is a program that
//! reads its allocations, and keeps every one. Not seen: a C `malloc` put in place under
//! `System` (`LD_PRELOAD`, an unprefixed jemalloc) whose statistics the program reads; the
//! contract above already lets rustc's LLVM remove those calls.
//!
//! What stays the same: the value, where it is built (by the same operand), and what drops when
//! and in what order, unwinding included. A drop of the box or `Rc` was the drop of the value
//! then a free; it is the drop of the value. A call that unwinds runs the same cleanup blocks,
//! whose frees are gone. The constructors' own unwind edges go: their one way to unwind was a
//! failed allocation, which no longer happens (the contract's "infallible").
//!
//! # Stack
//!
//! An allocation moves when its value is at most [`HEAP_TO_STACK_MAX`] bytes and aligned to at
//! most [`ALIGN_MAX`], and while the body's moved values add up to at most
//! [`HEAP_TO_STACK_FRAME_MAX`], taken in the order of their first constructor's block: the
//! frame of one call grows by at most that. One local per allocation site, so a loop reuses it.
//!
//! The frame never recurs while it is live: the pass moves nothing in a body that could call
//! itself again. Every call of such a body is of a constant function item, and each one is a
//! constructor, one of the calls the pass rewrites (`Box::new`, `Rc::new`, `Box::new_uninit`,
//! `write_box_via_move`, `box_assume_init_into_vec_unsafe`, a box's free, an `Rc`'s `deref`),
//! or a function of the library (`core`, `alloc`, `std`, ..) whose generic arguments and
//! argument types are closed ([`Closed`]): made of the library's own types and nothing a
//! program can put code behind (no parameter, closure, `dyn`, function pointer, function of the
//! program, or type of the program). By the orphan rule, every trait impl such a call can reach
//! is the library's, so it cannot run the program's code. Each value the body drops is one
//! whose drop runs only the library's code. No `asm!`, no tail call. So the frame is the deepest
//! of the program's on its thread for as long as it lives: a thread holds at most one of them,
//! but for code the program registered with std that std runs from inside it: a panic hook (a
//! panic in the hook aborts, so one level), an allocation-error hook, `atexit` handlers and
//! signal handlers, each of which needs a panic, a failed allocation, an exit or a signal per
//! level.
//!
//! A body built into another loses its moves (the inliner's callees and always-inlined bodies
//! are built with [`MirOptions::heap_to_stack`] off): its slot would be the caller's frame,
//! which may recur. So a function `Inline` takes into its caller allocates there again. A body
//! that keeps any records their bytes in `Inlined::moved_to_frame`, and the code generator
//! makes it one LLVM never imports into another unit's function (`hir-ty-codegen`'s
//! `v2/release.rs`), for the same reason: LLVM's inliner would put the slot in the importer's
//! frame.
//!
//! [`MirOptions::heap_to_stack`]: crate::mir::MirOptions::heap_to_stack

use base_db::{Crate, CrateOrigin, Db, LangCrateOrigin};
use blaze_mem::Frame;
use hir_def::{
    AdtId, CallableDefId, FunctionId, HasModule, ItemContainerId, Lookup, VariantId,
    signatures::{FunctionSignature, StructSignature},
};
use rustc_ast_ir::Mutability;
use rustc_type_ir::inherent::{AdtDef as _, GenericArgs as _, IntoKind, Ty as _};

use crate::{
    db::HirDatabase,
    layout::enum_variant_ids,
    mir::{
        BasicBlockId, BorrowKind, CastKind, Local, LocalId, MirBody, MirLowerError, MutBorrowKind,
        Operand, OperandKind, Place, Projection, ProjectionElem, Rvalue, Statement, StatementKind,
        StoredPlace, Terminator, TerminatorKind,
        framed::{FrameMap, map_in},
        return_slot,
        transform::Ctx,
    },
    next_solver::{GenericArgs, Region, RegionExt, Ty, TyKind},
};

use super::{Mention, Result, block_index, box_in_place::is_box_new, local_index, visit_body};

/// The largest value whose allocation moves to the stack, in bytes.
const HEAP_TO_STACK_MAX: u64 = 512;

/// The most the frame of one body grows by, over every allocation it moves, in bytes.
const HEAP_TO_STACK_FRAME_MAX: u64 = 2 << 10;

/// The largest alignment of a value that moves: what every target's frame has without
/// realigning its stack pointer.
const ALIGN_MAX: u64 = 16;

/// How many types [`Closed`] may look at for one body. Past it a type counts as not closed and
/// the body keeps its allocations.
const CLOSED_FUEL: u32 = 4096;

/// Why an allocation stays where it is.
type Refusal = &'static str;

/// What makes an allocation, and so how it moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `Box::new(value)`.
    Box,
    /// `Rc::new(value)`.
    Rc,
    /// `vec![..]`, from its `Box::new_uninit()`.
    Vec,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Box => "Box::new",
            Kind::Rc => "Rc::new",
            Kind::Vec => "vec!",
        }
    }
}

/// A local one of the [`Kind`]s of call returns into, with the blocks of those calls.
struct Candidate<'p> {
    kind: Kind,
    local: LocalId,
    /// The blocks whose calls build it, in block order.
    ctors: Vec<BasicBlockId, &'p Frame>,
    /// Calls of different kinds return into it.
    mixed: bool,
}

/// What moving one allocation changes, as [`Check`] found it.
struct Plan<'p, 'db> {
    kind: Kind,
    /// The box, the `Rc`, or the box `vec!` starts from.
    local: LocalId,
    /// What the frame holds: the box's or `Rc`'s value, which the local holds from now on, or
    /// the `MaybeUninit` of `vec!`'s array.
    held: Ty<'db>,
    ctors: Vec<BasicBlockId, &'p Frame>,
    /// Statements that go: the borrow of a box for its free.
    nops: Vec<(BasicBlockId, usize), &'p Frame>,
    /// Terminators that become a jump to where they went: a box's free, a drop of a box of
    /// `vec!` or of its `Vec`.
    jumps: Vec<BasicBlockId, &'p Frame>,
    /// The temporaries that borrow an `Rc`, which borrow its value from now on.
    borrows: Vec<LocalId, &'p Frame>,
    /// The blocks whose `deref` of an `Rc` becomes a copy of the borrow.
    derefs: Vec<BasicBlockId, &'p Frame>,
}

pub(super) fn heap_to_stack<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    frame: &Frame,
) -> Result<'db, ()> {
    // The library's bodies are one for every program.
    if is_lang(ctx.db, ctx.krate()) {
        return Ok(());
    }
    let candidates = candidates(ctx, body, frame);
    if candidates.is_empty() {
        return Ok(());
    }
    let _span = tracing::info_span!(
        target: "blaze",
        "heap_to_stack",
        body = ?body.owner,
        candidates = candidates.len(),
    )
    .entered();
    if let Err((at, reason)) = closed_body(ctx, body, frame) {
        for it in &candidates {
            tracing::info!(
                target: "blaze",
                block = block_index(it.ctors[0]),
                kind = it.kind.name(),
                outcome = "kept",
                reason,
                at = block_index(at),
            );
        }
        return Ok(());
    }
    let mentions = frame.alloc_slice_fill_with(body.locals.len(), |_| 0u32);
    visit_body(body, |local, mention, _| {
        if mention != Mention::Storage {
            mentions[local_index(local)] += 1;
        }
    });
    let check = Check { ctx, body, mentions, frame };
    let mut plans = Vec::new_in(frame);
    let mut grown = 0u64;
    for candidate in candidates {
        let (block, kind) = (candidate.ctors[0], candidate.kind);
        let kept = |reason: Refusal| {
            tracing::info!(
                target: "blaze",
                block = block_index(block),
                kind = kind.name(),
                outcome = "kept",
                reason,
            )
        };
        let plan = match check.check(candidate) {
            Ok(it) => it,
            Err(reason) => {
                kept(reason);
                continue;
            }
        };
        let Some(layout) = ctx.layout_of(plan.held) else {
            kept("the size of its value is not known where the body is generic");
            continue;
        };
        let (size, align) = (layout.size.bytes(), layout.align.abi.bytes());
        // A zero-sized `Box` or `vec!` never allocates; its pointer is the alignment, which
        // the program can read and which a frame slot's address would change.
        if size == 0 && kind != Kind::Rc {
            kept("its value is zero-sized, which allocates nothing already");
            continue;
        }
        if size > HEAP_TO_STACK_MAX {
            kept("its value is larger than HEAP_TO_STACK_MAX (512 bytes)");
            continue;
        }
        if align > ALIGN_MAX {
            kept("its value is aligned to more than 16 bytes");
            continue;
        }
        if grown + size > HEAP_TO_STACK_FRAME_MAX {
            kept("the body's frame would grow past HEAP_TO_STACK_FRAME_MAX (2 KiB)");
            continue;
        }
        grown += size;
        tracing::info!(
            target: "blaze",
            block = block_index(block),
            kind = kind.name(),
            outcome = "moved",
            size,
        );
        plans.push(plan);
    }
    for plan in &plans {
        rewrite(ctx, body, plan)?;
    }
    // At most `HEAP_TO_STACK_FRAME_MAX`, so it fits.
    let grown = u32::try_from(grown).unwrap_or(u32::MAX);
    body.inlined.moved_to_frame = body.inlined.moved_to_frame.saturating_add(grown);
    Ok(())
}

/// The locals a `Box::new`, `Rc::new` or `Box::new_uninit` call returns into, in the order of
/// the first such call's block. A call that returns into a place other than a whole local (a
/// field, through a pointer) stores what it allocates, which so leaves the local: not one.
fn candidates<'p>(
    ctx: &Ctx<'_, '_>,
    body: &MirBody<'_>,
    frame: &'p Frame,
) -> Vec<Candidate<'p>, &'p Frame> {
    let mut found: Vec<Candidate<'p>, &'p Frame> = Vec::new_in(frame);
    let mut by_local: FrameMap<'_, LocalId, usize> = map_in(frame);
    for (block, data) in body.basic_blocks.iter() {
        let Some(Terminator { kind: TerminatorKind::Call { func, destination, .. }, .. }) =
            &data.terminator
        else {
            continue;
        };
        let Some((func, _)) = func.fn_item() else { continue };
        let Some(kind) = constructs(ctx, func) else { continue };
        let Some(local) = destination.as_ref().as_local() else { continue };
        match by_local.get(&local) {
            Some(&at) => {
                let it = &mut found[at];
                it.mixed |= it.kind != kind;
                it.ctors.push(block);
            }
            None => {
                by_local.insert(local, found.len());
                let mut ctors = Vec::new_in(frame);
                ctors.push(block);
                found.push(Candidate { kind, local, ctors, mixed: false });
            }
        }
    }
    found
}

/// The kind of allocation a call of `func` makes, if it is one the pass moves.
fn constructs(ctx: &Ctx<'_, '_>, func: FunctionId) -> Option<Kind> {
    let db = ctx.db;
    if is_box_new(ctx, func) {
        return Some(Kind::Box);
    }
    if inherent_of(db, func, "new").is_some_and(|it| is_alloc_struct(db, it, "Rc")) {
        return Some(Kind::Rc);
    }
    if inherent_of(db, func, "new_uninit").is_some_and(|it| is_owned_box(ctx, it)) {
        return Some(Kind::Vec);
    }
    None
}

/// Whether the body can call itself again, through anything but code the program registered
/// with std: `Err` with the first block that may, and how.
fn closed_body<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    frame: &Frame,
) -> std::result::Result<(), (BasicBlockId, Refusal)> {
    let db = ctx.db;
    let mut closed = Closed::new(frame);
    for (block, data) in body.basic_blocks.iter() {
        if data.statements.iter().any(|it| matches!(it.kind, StatementKind::InlineAsm(_))) {
            return Err((block, "the body has an `asm!`, which may call anything"));
        }
        let Some(terminator) = &data.terminator else { continue };
        match &terminator.kind {
            TerminatorKind::Call { func, args, .. } => {
                let Some((def, generic)) = func.fn_def() else {
                    return Err((
                        block,
                        "the body calls through a function pointer, which may be its own function",
                    ));
                };
                // A constructor runs no code.
                let CallableDefId::FunctionId(callee) = def else { continue };
                if exempt(ctx, &mut closed, callee, generic) {
                    continue;
                }
                if !is_lang(db, callee.krate(db)) {
                    return Err((
                        block,
                        "the body calls a function of the program, which may call it again",
                    ));
                }
                let closed_types = generic.types().all(|it| closed.closed(ctx, it))
                    && args.iter().all(|it| closed.closed(ctx, ctx.operand_ty(body, it)));
                if !closed_types {
                    return Err((
                        block,
                        "the body calls a library function with a parameter, closure, `dyn`, \
                         function or type of the program, whose code that call may run",
                    ));
                }
            }
            TerminatorKind::Drop { place, .. } | TerminatorKind::DropAndReplace { place, .. } => {
                let ty = place.as_ref().ty(body, &ctx.infcx, ctx.env).ty;
                if !closed.drop_closed(ctx, ty) {
                    return Err((
                        block,
                        "the body drops a value whose drop may run the program's code",
                    ));
                }
            }
            TerminatorKind::TailCall { .. } => {
                return Err((block, "the body ends in a tail call, which may be of itself"));
            }
            _ => {}
        }
    }
    Ok(())
}

/// A call the closed-body rule takes whatever its generic arguments: one this pass rewrites, or
/// one of the library's whose code never reaches a value of its type arguments' but to move it.
/// `Box::new`, `Rc::new`, `Box::new_uninit`, `write_box_via_move` and
/// `box_assume_init_into_vec_unsafe` allocate and move; `<Box<T, A> as Drop>::drop` frees with
/// `A` and `<Rc<T, A> as Deref>::deref` returns a borrow, so `A` must be closed.
fn exempt<'db>(
    ctx: &Ctx<'_, 'db>,
    closed: &mut Closed<'_, 'db>,
    callee: FunctionId,
    generic: GenericArgs<'db>,
) -> bool {
    let db = ctx.db;
    let lang = ctx.infcx.interner.lang_items();
    if constructs(ctx, callee).is_some()
        || is_alloc_fn(db, callee, "write_box_via_move")
        || is_alloc_fn(db, callee, "box_assume_init_into_vec_unsafe")
    {
        return true;
    }
    let holder = |is: &dyn Fn(AdtId) -> bool, closed: &mut Closed<'_, 'db>| {
        let Some(self_ty) = generic.types().next() else { return false };
        match ctx.normalize(self_ty).kind() {
            TyKind::Adt(adt, args) if is(adt.def_id()) => {
                args.types().skip(1).all(|it| closed.closed(ctx, it))
            }
            _ => false,
        }
    };
    if Some(callee) == lang.Drop_drop {
        return holder(&|it| is_owned_box(ctx, it), closed);
    }
    if Some(callee) == lang.Deref_deref {
        return holder(&|it| is_alloc_struct(db, it, "Rc"), closed);
    }
    false
}

/// Whether a type is closed (made only of the library's types, so nothing in it lets a call of
/// the library run the program's code) and whether its drop runs only the library's code,
/// remembered for the body. A type within itself counts as closed for as long as it is being
/// looked at (co-inductively); an answer found under that assumption is kept only once the type
/// that was asked about is closed too, and every step is a conjunction, so then each was.
struct Closed<'p, 'db> {
    /// By type and whether its drop is asked about.
    known: FrameMap<'p, (Ty<'db>, bool), bool>,
    walking: Vec<(Ty<'db>, bool), &'p Frame>,
    /// Closed under the assumptions of `walking`, kept once its outermost is.
    pending: Vec<(Ty<'db>, bool), &'p Frame>,
    fuel: u32,
}

impl<'p, 'db> Closed<'p, 'db> {
    fn new(frame: &'p Frame) -> Self {
        Closed {
            known: map_in(frame),
            walking: Vec::new_in(frame),
            pending: Vec::new_in(frame),
            fuel: CLOSED_FUEL,
        }
    }

    fn closed(&mut self, ctx: &Ctx<'_, 'db>, ty: Ty<'db>) -> bool {
        self.ask(ctx, ty, false)
    }

    fn drop_closed(&mut self, ctx: &Ctx<'_, 'db>, ty: Ty<'db>) -> bool {
        self.ask(ctx, ty, true)
    }

    fn ask(&mut self, ctx: &Ctx<'_, 'db>, ty: Ty<'db>, drop: bool) -> bool {
        let key = (ctx.normalize(ty), drop);
        if let Some(&known) = self.known.get(&key) {
            return known;
        }
        if self.walking.contains(&key) {
            return true;
        }
        if self.fuel == 0 {
            return false;
        }
        self.fuel -= 1;
        self.walking.push(key);
        let answer = match drop {
            false => self.walk(ctx, key.0),
            true => self.walk_drop(ctx, key.0),
        };
        self.walking.pop();
        let outermost = self.walking.is_empty();
        if !answer {
            self.known.insert(key, false);
            if outermost {
                self.pending.clear();
            }
        } else if outermost {
            for it in self.pending.drain(..) {
                self.known.insert(it, true);
            }
            self.known.insert(key, true);
        } else {
            self.pending.push(key);
        }
        answer
    }

    fn walk(&mut self, ctx: &Ctx<'_, 'db>, ty: Ty<'db>) -> bool {
        let db = ctx.db;
        match ty.kind() {
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Str
            | TyKind::Never => true,
            TyKind::Ref(_, inner, _)
            | TyKind::RawPtr(inner, _)
            | TyKind::Slice(inner)
            | TyKind::Array(inner, _)
            | TyKind::Pat(inner, _) => self.closed(ctx, inner),
            TyKind::Tuple(fields) => fields.iter().all(|it| self.closed(ctx, it)),
            TyKind::Adt(adt, args) => {
                is_lang(db, adt.def_id().krate(db))
                    && args.types().all(|it| self.closed(ctx, it))
                    && self.fields(ctx, adt.def_id(), args, false)
            }
            TyKind::FnDef(def, args) => {
                is_lang(db, def.0.krate(db))
                    && args.skip_binder().types().all(|it| self.closed(ctx, it))
            }
            // A parameter, a closure, a `dyn`, a function pointer, an alias left unnormalized,
            // an extern type: what code it stands for is the program's.
            _ => false,
        }
    }

    fn walk_drop(&mut self, ctx: &Ctx<'_, 'db>, ty: Ty<'db>) -> bool {
        if !ctx.needs_drop(ty) {
            return true;
        }
        let db = ctx.db;
        match ty.kind() {
            TyKind::Adt(adt, args) => {
                let id = adt.def_id();
                if adt.is_box() || is_alloc_struct(db, id, "Rc") || is_alloc_struct(db, id, "Vec") {
                    // Dropping one drops what it holds, then frees with its allocator.
                    let mut types = args.types();
                    let Some(held) = types.next() else { return false };
                    return types.all(|it| self.closed(ctx, it)) && self.drop_closed(ctx, held);
                }
                if adt.destructor(ctx.infcx.interner).is_some() {
                    return self.closed(ctx, ty);
                }
                self.fields(ctx, id, args, true)
            }
            TyKind::Tuple(fields) => fields.iter().all(|it| self.drop_closed(ctx, it)),
            TyKind::Array(inner, _) | TyKind::Slice(inner) => self.drop_closed(ctx, inner),
            TyKind::Closure(_, args) => self.drop_closed(ctx, args.as_closure().tupled_upvars_ty()),
            _ => self.closed(ctx, ty),
        }
    }

    /// Whether every field of every variant of `adt` with `args` is closed (`drop`: its drop is).
    fn fields(
        &mut self,
        ctx: &Ctx<'_, 'db>,
        adt: AdtId,
        args: GenericArgs<'db>,
        drop: bool,
    ) -> bool {
        let db = ctx.db;
        let interner = ctx.infcx.interner;
        let variant = |this: &mut Self, variant: VariantId| {
            db.field_types(variant).iter().all(|(_, field)| {
                let ty = field.ty().instantiate(interner, args).skip_norm_wip();
                this.ask(ctx, ty, drop)
            })
        };
        match adt {
            AdtId::StructId(id) => variant(self, id.into()),
            AdtId::UnionId(id) => variant(self, id.into()),
            AdtId::EnumId(id) => {
                enum_variant_ids(db, id).iter().all(|&it| variant(self, it.into()))
            }
        }
    }
}

/// What the checks of one candidate read.
struct Check<'c, 'p, 'f, 'db> {
    ctx: &'c Ctx<'f, 'db>,
    body: &'c MirBody<'db>,
    /// By local: how many places name it, storage markers not counted.
    mentions: &'c [u32],
    frame: &'p Frame,
}

impl<'c, 'p, 'f, 'db> Check<'c, 'p, 'f, 'db> {
    fn check(&self, candidate: Candidate<'p>) -> std::result::Result<Plan<'p, 'db>, Refusal> {
        if candidate.mixed {
            return Err("calls of different kinds return into it");
        }
        let local = candidate.local;
        if !is_own(self.body, local) {
            return Err(
                "it is the return place, a parameter, a capture, a guard's reference or a \
                        promoted constant",
            );
        }
        for &block in &candidate.ctors {
            let Some(TerminatorKind::Call { func, args, target, .. }) = self.terminator(block)
            else {
                return Err("its constructor is not a call");
            };
            if target.is_none() {
                return Err("a call that builds it does not return");
            }
            // What `call_to_copy` makes of a `Box::new` or `Rc::new`: one argument, the value.
            if candidate.kind != Kind::Vec && args.len() != 1 {
                return Err("its constructor does not take one argument");
            }
            if operand_names(func, local) || args.iter().any(|it| operand_names(it, local)) {
                return Err("a call that builds it reads it too");
            }
        }
        let ty = self.ctx.normalize(self.body.locals[local].ty.as_ref());
        match candidate.kind {
            Kind::Box => self.check_box(candidate, ty),
            Kind::Rc => self.check_rc(candidate, ty),
            Kind::Vec => self.check_vec(candidate, ty),
        }
    }

    fn terminator(&self, block: BasicBlockId) -> Option<&'c TerminatorKind> {
        self.body.basic_blocks[block].terminator.as_ref().map(|it| &it.kind)
    }

    /// The local `place` is, if it is a temporary that holds one borrow and is read once: named
    /// by exactly two places.
    fn borrow_temp(&self, place: &StoredPlace) -> Option<LocalId> {
        let local = place.as_ref().as_local()?;
        (is_temp(self.body, local) && self.mentions[local_index(local)] == 2).then_some(local)
    }

    fn plan(
        &self,
        kind: Kind,
        local: LocalId,
        held: Ty<'db>,
        ctors: Vec<BasicBlockId, &'p Frame>,
    ) -> Plan<'p, 'db> {
        Plan {
            kind,
            local,
            held,
            ctors,
            nops: Vec::new_in(self.frame),
            jumps: Vec::new_in(self.frame),
            borrows: Vec::new_in(self.frame),
            derefs: Vec::new_in(self.frame),
        }
    }

    /// `_b` of type `Box<T>`: read and written only through `*_b`, dropped, and freed.
    fn check_box(
        &self,
        candidate: Candidate<'p>,
        ty: Ty<'db>,
    ) -> std::result::Result<Plan<'p, 'db>, Refusal> {
        let b = candidate.local;
        let held = match ty.kind() {
            TyKind::Adt(adt, args) if adt.is_box() => args.type_at(0),
            _ => return Err("`Box::new` returns into a local that is not a `Box`"),
        };
        let mut plan = self.plan(Kind::Box, b, held, candidate.ctors);
        for (block, data) in self.body.basic_blocks.iter() {
            for (index, statement) in data.statements.iter().enumerate() {
                match &statement.kind {
                    StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::Nop => {}
                    StatementKind::FakeRead(place) if is_whole(place, b) => {}
                    StatementKind::Assign(
                        to,
                        Rvalue::Ref(BorrowKind::Mut { kind: MutBorrowKind::Default }, place),
                    ) if is_whole(place, b) => {
                        // The free after the value's drop: `_r = &mut _b; <Box<T> as Drop>::drop(move _r)`.
                        let freed = self.borrow_temp(to).is_some_and(|r| self.frees(block, r, ty));
                        if !freed {
                            return Err("the box is borrowed whole, other than to free it");
                        }
                        plan.nops.push((block, index));
                        plan.jumps.push(block);
                    }
                    other => {
                        if !statement_places(other, |it| through_deref(it, b)) {
                            return Err(
                                "the box itself is moved, copied, borrowed or written, not only \
                                        what it holds",
                            );
                        }
                    }
                }
            }
            let Some(terminator) = &data.terminator else { continue };
            match &terminator.kind {
                // Checked: the constructors, and the frees, whose call names only the borrow.
                _ if plan.ctors.contains(&block) || plan.jumps.contains(&block) => {}
                // The box whole or what it holds: after the move, both drop the value.
                TerminatorKind::Drop { place, .. }
                    if is_whole(place, b) || (place.local == b && through_deref(place, b)) => {}
                other => {
                    if !terminator_places(other, |it| through_deref(it, b)) {
                        return Err(
                            "the box itself is moved, copied, returned into or dropped in \
                                    part, not only what it holds",
                        );
                    }
                }
            }
        }
        Ok(plan)
    }

    /// Whether `block` ends by freeing the box of type `box_ty` that `r` borrows:
    /// `<Box<T, A> as Drop>::drop(move r)`, its one other mention.
    fn frees(&self, block: BasicBlockId, r: LocalId, box_ty: Ty<'db>) -> bool {
        let Some(TerminatorKind::Call { func, args, target: Some(_), .. }) = self.terminator(block)
        else {
            return false;
        };
        let Some((callee, generic)) = func.fn_item() else { return false };
        Some(callee) == self.ctx.infcx.interner.lang_items().Drop_drop
            && generic.types().next().is_some_and(|it| self.ctx.normalize(it) == box_ty)
            && matches!(&args[..], [arg] if whole_use(arg) == Some(r))
    }

    /// `_r` of type `Rc<T>`: only borrowed for `deref`, and dropped.
    fn check_rc(
        &self,
        candidate: Candidate<'p>,
        ty: Ty<'db>,
    ) -> std::result::Result<Plan<'p, 'db>, Refusal> {
        let db = self.ctx.db;
        let r = candidate.local;
        let held = match ty.kind() {
            TyKind::Adt(adt, args) if is_alloc_struct(db, adt.def_id(), "Rc") => args.type_at(0),
            _ => return Err("`Rc::new` returns into a local that is not an `Rc`"),
        };
        let mut plan = self.plan(Kind::Rc, r, held, candidate.ctors);
        for (_, data) in self.body.basic_blocks.iter() {
            for statement in &data.statements {
                match &statement.kind {
                    StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::Nop => {}
                    StatementKind::FakeRead(place) if is_whole(place, r) => {}
                    StatementKind::Assign(to, Rvalue::Ref(BorrowKind::Shared, place))
                        if is_whole(place, r) =>
                    {
                        let Some(t) = self.borrow_temp(to) else {
                            return Err(
                                "the `Rc` is borrowed into a place that is read more than once",
                            );
                        };
                        plan.borrows.push(t);
                    }
                    other => {
                        if !statement_places(other, |it| !names(it, r)) {
                            return Err("the `Rc` is used other than through `*`: moved, copied, \
                                        cloned, counted or borrowed mutably");
                        }
                    }
                }
            }
        }
        let deref = self.ctx.infcx.interner.lang_items().Deref_deref;
        for (block, data) in self.body.basic_blocks.iter() {
            let Some(terminator) = &data.terminator else { continue };
            match &terminator.kind {
                _ if plan.ctors.contains(&block) => {}
                TerminatorKind::Drop { place, .. } if is_whole(place, r) => {}
                TerminatorKind::Call { func, args, target: Some(_), .. }
                    if args
                        .first()
                        .and_then(whole_use)
                        .is_some_and(|t| plan.borrows.contains(&t)) =>
                {
                    // Its destination is of `&T`, so it is no place of the `Rc`, and `deref`
                    // never unwinds, so its cleanup edge goes with the call.
                    let is_deref = func.fn_item().is_some_and(|(it, _)| Some(it) == deref);
                    if !is_deref || args.len() != 1 {
                        return Err(
                            "a borrow of the `Rc` is given to something other than `Deref::deref`",
                        );
                    }
                    plan.derefs.push(block);
                }
                other => {
                    if !terminator_places(other, |it| !names(it, r)) {
                        return Err(
                            "the `Rc` is used other than through `*`: moved, returned into, \
                                    or given to a call",
                        );
                    }
                }
            }
        }
        if plan.derefs.len() != plan.borrows.len() {
            return Err("a borrow of the `Rc` is read other than by `Deref::deref`");
        }
        Ok(plan)
    }

    /// `_u` of type `Box<MaybeUninit<[T; N]>>`, the start of `vec!`'s chain: `_w =
    /// write_box_via_move(move _u, [..])`, `_v = box_assume_init_into_vec_unsafe(move _w)`, and
    /// `_v` only borrowed for what never grows it, and dropped.
    fn check_vec(
        &self,
        candidate: Candidate<'p>,
        ty: Ty<'db>,
    ) -> std::result::Result<Plan<'p, 'db>, Refusal> {
        let ctx = self.ctx;
        let lang = ctx.infcx.interner.lang_items();
        let u = candidate.local;
        if candidate.ctors.len() != 1 {
            return Err("more than one `Box::new_uninit` returns into it");
        }
        let slot = match ty.kind() {
            TyKind::Adt(adt, args) if adt.is_box() => ctx.normalize(args.type_at(0)),
            _ => return Err("`Box::new_uninit` returns into a local that is not a `Box`"),
        };
        let array = match slot.kind() {
            TyKind::Adt(adt, args)
                if lang.MaybeUninit.map(AdtId::UnionId) == Some(adt.def_id()) =>
            {
                ctx.normalize(args.type_at(0))
            }
            _ => return Err("its box is not of a `MaybeUninit`"),
        };
        if !matches!(array.kind(), TyKind::Array(..)) {
            return Err("its box is not of an array: a `Box::new_uninit` that is not `vec!`'s");
        }
        if ctx.needs_drop(array) {
            return Err("the `Vec`'s elements have something to drop");
        }
        let Some((written_at, w)) = self.consumer(u, "write_box_via_move") else {
            return Err("its box is not written once by `write_box_via_move`: not `vec!`'s");
        };
        let Some((made_at, v)) = self.consumer(w, "box_assume_init_into_vec_unsafe") else {
            return Err(
                "its written box is not made a `Vec` once by `box_assume_init_into_vec_unsafe`",
            );
        };
        if !is_temp(self.body, w) || !is_own(self.body, v) {
            return Err("a local of `vec!`'s chain is a parameter, a capture or the return place");
        }
        let built = candidate.ctors[0];
        let links = [u, w, v];
        let named = |place: &StoredPlace| links.iter().any(|&it| names(place, it));
        let mut plan = self.plan(Kind::Vec, u, slot, candidate.ctors);
        // The borrows of `_v`, and whether each is shared.
        let mut borrows: Vec<(LocalId, bool), &Frame> = Vec::new_in(self.frame);
        for (_, data) in self.body.basic_blocks.iter() {
            for statement in &data.statements {
                match &statement.kind {
                    StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::Nop => {}
                    StatementKind::FakeRead(place)
                        if links.iter().any(|&it| is_whole(place, it)) => {}
                    StatementKind::Assign(to, Rvalue::Ref(kind, place)) if is_whole(place, v) => {
                        let shared = match kind {
                            BorrowKind::Shared | BorrowKind::Shallow => true,
                            BorrowKind::Mut {
                                kind: MutBorrowKind::Default | MutBorrowKind::TwoPhasedBorrow,
                            } => false,
                            BorrowKind::Mut { kind: MutBorrowKind::ClosureCapture } => {
                                return Err("the `Vec` is captured by a closure");
                            }
                        };
                        let Some(x) = self.borrow_temp(to) else {
                            return Err(
                                "the `Vec` is borrowed into a place that is read more than once",
                            );
                        };
                        borrows.push((x, shared));
                    }
                    other => {
                        if !statement_places(other, |it| !named(it)) {
                            return Err("the `Vec` or a box of `vec!` is moved, copied or written");
                        }
                    }
                }
            }
        }
        let mut used = 0;
        for (block, data) in self.body.basic_blocks.iter() {
            let Some(terminator) = &data.terminator else { continue };
            match &terminator.kind {
                _ if block == built => {}
                // The chain's calls: the link they read is their first argument, the next the
                // whole place they return into, and neither is named anywhere else in them.
                TerminatorKind::Call { func, args, .. }
                    if block == written_at || block == made_at =>
                {
                    if operand_names_any(func, &links)
                        || args.iter().skip(1).any(|it| operand_names_any(it, &links))
                    {
                        return Err("a call of `vec!`'s chain reads a link of it twice");
                    }
                }
                TerminatorKind::Drop { place, .. }
                    if links.iter().any(|&it| is_whole(place, it)) =>
                {
                    plan.jumps.push(block);
                }
                TerminatorKind::Call { func, args, destination, target: Some(_), .. }
                    if let Some(&(_, shared)) = args
                        .first()
                        .and_then(whole_use)
                        .and_then(|x| borrows.iter().find(|it| it.0 == x)) =>
                {
                    if !self.never_grows(func, shared) {
                        return Err(
                            "a borrow of the `Vec` is given to a call that may grow it or hand it out",
                        );
                    }
                    if named(destination)
                        || args.iter().skip(1).any(|it| operand_names_any(it, &links))
                    {
                        return Err("a call given a borrow of the `Vec` names it again");
                    }
                    used += 1;
                }
                other => {
                    if !terminator_places(other, |it| !named(it)) {
                        return Err(
                            "the `Vec` or a box of `vec!` is moved, returned into or given to a call",
                        );
                    }
                }
            }
        }
        if used != borrows.len() {
            return Err("a borrow of the `Vec` is read other than by a call that never grows it");
        }
        Ok(plan)
    }

    /// The one call of the `alloc` function `name` that reads `local` whole as its first
    /// argument, with the whole local it returns into, if there is exactly one and it returns.
    fn consumer(&self, local: LocalId, name: &str) -> Option<(BasicBlockId, LocalId)> {
        let mut found = None;
        for (block, data) in self.body.basic_blocks.iter() {
            let Some(TerminatorKind::Call { func, args, destination, target, .. }) =
                data.terminator.as_ref().map(|it| &it.kind)
            else {
                continue;
            };
            if args.first().and_then(whole_use) != Some(local) {
                continue;
            }
            let is = func.fn_item().is_some_and(|(it, _)| is_alloc_fn(self.ctx.db, it, name));
            let into = destination.as_ref().as_local();
            match (is, into, target, found) {
                (true, Some(into), Some(_), None) => found = Some((block, into)),
                _ => return None,
            }
        }
        found
    }

    /// Whether `func`, given a borrow of the `Vec` (`shared` or mutable) as its first argument,
    /// neither grows nor reallocates nor frees its buffer, nor hands out the `Vec` itself.
    fn never_grows(&self, func: &Operand, shared: bool) -> bool {
        let db = self.ctx.db;
        let lang = self.ctx.infcx.interner.lang_items();
        let Some((callee, _)) = func.fn_item() else { return false };
        let of_vec = |name: &str| {
            inherent_of(db, callee, name).is_some_and(|it| is_alloc_struct(db, it, "Vec"))
        };
        let trait_methods = match shared {
            true => [lang.Deref_deref, lang.Index_index, lang.IntoIterIntoIter],
            false => [lang.DerefMut_deref_mut, lang.IndexMut_index_mut, lang.IntoIterIntoIter],
        };
        if trait_methods.contains(&Some(callee)) {
            return true;
        }
        match shared {
            true => ["len", "is_empty", "capacity", "as_slice"].into_iter().any(of_vec),
            false => of_vec("as_mut_slice"),
        }
    }
}

/// `plan` carried out on `body`.
fn rewrite<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    plan: &Plan<'_, 'db>,
) -> Result<'db, ()> {
    let interner = ctx.infcx.interner;
    let local = plan.local;
    match plan.kind {
        Kind::Box | Kind::Rc => {
            body.locals[local].ty = plan.held.store();
            for &block in &plan.ctors {
                call_to_copy(body, block, plan.kind)?;
            }
            if plan.kind == Kind::Box {
                body.walk_places(|place| {
                    if place.local == local
                        && matches!(
                            place.projection.as_slice().first(),
                            Some(ProjectionElem::Deref)
                        )
                    {
                        let rest = place.projection.as_slice()[1..].iter().copied();
                        place.projection = Projection::new_from_iter(rest).store();
                    }
                });
            }
        }
        Kind::Vec => {
            // `_u = Box::new_uninit()` becomes `_p = &mut _slot; _u = transmute(_p)`: the box
            // points at the frame's slot, which lives (no storage markers) as long as the frame.
            let slot = body.locals.alloc(Local { ty: plan.held.store() });
            let pointer_ty =
                Ty::new_ref(interner, Region::new_erased(interner), plan.held, Mutability::Mut);
            let pointer = body.locals.alloc(Local { ty: pointer_ty.store() });
            let boxed = body.locals[local].ty.clone(); // clone: a refcount
            let block = plan.ctors[0];
            let owner = body.owner;
            let data = &mut body.basic_blocks[block];
            let Some(Terminator {
                span,
                kind: TerminatorKind::Call { destination, target: Some(target), .. },
            }) = data.terminator.take()
            else {
                return Err(changed(owner, block, "`vec!`'s `Box::new_uninit` call"));
            };
            let borrow = Rvalue::Ref(
                BorrowKind::Mut { kind: MutBorrowKind::Default },
                Place::from(slot).store(),
            );
            let to_box = Rvalue::Cast(
                CastKind::Transmute,
                Operand { kind: OperandKind::Move(Place::from(pointer).store()), span: None },
                boxed,
            );
            data.statements.push(Statement {
                kind: StatementKind::Assign(Place::from(pointer).store(), borrow),
                span,
            });
            data.statements
                .push(Statement { kind: StatementKind::Assign(destination, to_box), span });
            data.terminator = Some(Terminator { span, kind: TerminatorKind::Goto { target } });
        }
    }
    for &(block, index) in &plan.nops {
        body.basic_blocks[block].statements[index].kind = StatementKind::Nop;
    }
    for &block in &plan.jumps {
        let owner = body.owner;
        let terminator = body.basic_blocks[block].terminator.as_mut();
        let target = match terminator.as_ref().map(|it| &it.kind) {
            Some(TerminatorKind::Drop { target, .. })
            | Some(TerminatorKind::Call { target: Some(target), .. }) => *target,
            _ => return Err(changed(owner, block, "a drop or free of the moved allocation")),
        };
        if let Some(terminator) = terminator {
            terminator.kind = TerminatorKind::Goto { target };
        }
    }
    for &t in &plan.borrows {
        let ty = body.locals[t].ty.as_ref();
        let TyKind::Ref(region, _, mutbl) = ty.kind() else {
            return Err(MirLowerError::ImplementationError(format!(
                "HeapToStack: the borrow _{} of an `Rc` in {:?} is of type `{ty:?}`, not a reference",
                local_index(t),
                body.owner
            )));
        };
        body.locals[t].ty = Ty::new_ref(interner, region, plan.held, mutbl).store();
    }
    for &block in &plan.derefs {
        call_to_copy(body, block, plan.kind)?;
    }
    Ok(())
}

/// The call that ends `block`, of one argument, becomes `destination = arg; goto target`: a
/// constructor whose value the local now is, or an `Rc`'s `deref` of a borrow that now points
/// at the value.
fn call_to_copy<'db>(body: &mut MirBody<'db>, block: BasicBlockId, kind: Kind) -> Result<'db, ()> {
    let owner = body.owner;
    let data = &mut body.basic_blocks[block];
    let fits = matches!(
        data.terminator.as_ref().map(|it| &it.kind),
        Some(TerminatorKind::Call { args, target: Some(_), .. }) if args.len() == 1
    );
    if !fits {
        return Err(changed(owner, block, kind.name()));
    }
    let Some(Terminator {
        span,
        kind: TerminatorKind::Call { args, destination, target: Some(target), .. },
    }) = data.terminator.take()
    else {
        return Err(changed(owner, block, kind.name()));
    };
    let mut args = args.into_vec();
    let Some(arg) = args.pop() else { return Err(changed(owner, block, kind.name())) };
    data.statements
        .push(Statement { kind: StatementKind::Assign(destination, Rvalue::Use(arg)), span });
    data.terminator = Some(Terminator { span, kind: TerminatorKind::Goto { target } });
    Ok(())
}

/// The error of a rewrite that finds a block other than its check did: a bug of the pass.
fn changed<'db>(
    owner: crate::InferBodyId<'db>,
    block: BasicBlockId,
    what: &str,
) -> MirLowerError<'db> {
    MirLowerError::ImplementationError(format!(
        "HeapToStack: bb{} of {owner:?} no longer ends in {what}, which its check found there",
        block_index(block)
    ))
}

/// The value a `Box<T, A>` or an `alloc::rc::Rc<T, A>` holds, `T`: what dropping the box or
/// `Rc` drops besides freeing it, and what a body that `HeapToStack` moved the allocation of
/// drops instead. `None` for any other type.
pub fn heap_held<'db>(db: &Db, ty: Ty<'db>) -> Option<Ty<'db>> {
    let TyKind::Adt(adt, args) = ty.kind() else { return None };
    if adt.is_box() || is_alloc_struct(db, adt.def_id(), "Rc") {
        return Some(args.type_at(0));
    }
    None
}

/// `adt` is the struct `name` of the `alloc` crate.
fn is_alloc_struct(db: &Db, adt: AdtId, name: &str) -> bool {
    let AdtId::StructId(id) = adt else { return false };
    matches!(id.krate(db).data(db).origin, CrateOrigin::Lang(LangCrateOrigin::Alloc))
        && StructSignature::of(db, id).name.as_str() == name
}

/// `adt` is `Box` (the `owned_box` lang item).
fn is_owned_box(ctx: &Ctx<'_, '_>, adt: AdtId) -> bool {
    ctx.infcx.interner.lang_items().OwnedBox.is_some_and(|it| AdtId::StructId(it) == adt)
}

/// The type `func` is an inherent method named `name` of, if it is one.
fn inherent_of(db: &Db, func: FunctionId, name: &str) -> Option<AdtId> {
    if FunctionSignature::of(db, func).name.as_str() != name {
        return None;
    }
    let ItemContainerId::ImplId(impl_) = func.lookup(db).container else { return None };
    if db.impl_trait(impl_).is_some() {
        return None;
    }
    match db.impl_self_ty(impl_).skip_binder().kind() {
        TyKind::Adt(adt, _) => Some(adt.def_id()),
        _ => None,
    }
}

/// `func` is the free function `name` of the `alloc` crate.
fn is_alloc_fn(db: &Db, func: FunctionId, name: &str) -> bool {
    matches!(func.lookup(db).container, ItemContainerId::ModuleId(_))
        && matches!(func.krate(db).data(db).origin, CrateOrigin::Lang(LangCrateOrigin::Alloc))
        && FunctionSignature::of(db, func).name.as_str() == name
}

fn is_lang(db: &Db, krate: Crate) -> bool {
    krate.data(db).origin.is_lang()
}

/// A local the body makes for itself: not the return place, a parameter, a capture, a
/// promoted constant or a guard's reference. A binding is one (`let b = Box::new(..)`).
fn is_own(body: &MirBody<'_>, local: LocalId) -> bool {
    local != return_slot()
        && !body.param_locals.contains(&local)
        && !body.promoted_locals.iter().any(|it| it.0 == local)
        && !body.promoted_constants.contains(&local)
        && !body.upvars_in_place.iter().any(|it| it.0 == local)
        && !body.capture_copies.iter().any(|it| it.0 == local)
        && !body.guard_refs.iter().any(|it| it.0 == local)
}

/// [`is_own`], and not a binding.
fn is_temp(body: &MirBody<'_>, local: LocalId) -> bool {
    is_own(body, local) && !body.binding_locals.values().any(|it| *it == local)
}

/// `place` is `local`, whole.
fn is_whole(place: &StoredPlace, local: LocalId) -> bool {
    place.local == local && place.projection.as_slice().is_empty()
}

/// `place` names `local`: as its base or as an index in it.
fn names(place: &StoredPlace, local: LocalId) -> bool {
    place.local == local
        || place
            .projection
            .as_slice()
            .iter()
            .any(|it| matches!(it, ProjectionElem::Index(l) if *l == local))
}

/// `place` names the box `local` only through `*local`, if at all.
fn through_deref(place: &StoredPlace, local: LocalId) -> bool {
    let elems = place.projection.as_slice();
    if elems.iter().any(|it| matches!(it, ProjectionElem::Index(l) if *l == local)) {
        return false;
    }
    place.local != local || matches!(elems.first(), Some(ProjectionElem::Deref))
}

/// The local `operand` copies or moves whole, if it does.
fn whole_use(operand: &Operand) -> Option<LocalId> {
    match &operand.kind {
        OperandKind::Copy(place) | OperandKind::Move(place) => place.as_ref().as_local(),
        _ => None,
    }
}

fn operand_names(operand: &Operand, local: LocalId) -> bool {
    match &operand.kind {
        OperandKind::Copy(place) | OperandKind::Move(place) => names(place, local),
        _ => false,
    }
}

fn operand_names_any(operand: &Operand, locals: &[LocalId]) -> bool {
    locals.iter().any(|&it| operand_names(operand, it))
}

/// Whether every place of the statement holds to `ok`.
fn statement_places(kind: &StatementKind, mut ok: impl FnMut(&StoredPlace) -> bool) -> bool {
    let mut all = true;
    MirBody::visit_statement_places(kind, &mut |place: &StoredPlace| all &= ok(place));
    all
}

/// Whether every place of the terminator holds to `ok`.
fn terminator_places(kind: &TerminatorKind, mut ok: impl FnMut(&StoredPlace) -> bool) -> bool {
    let mut all = true;
    MirBody::visit_terminator_places(kind, &mut |place: &StoredPlace| all &= ok(place));
    all
}
