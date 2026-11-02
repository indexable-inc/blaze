//! What each local of one monomorphized body may hold, as an [`Abs`] (design section 6).
//!
//! Flow-insensitive: a local holds the join of everything ever assigned to it, so passes in
//! block order until nothing changes solve a body; each local only rises in the lattice, so
//! the passes are bounded by its height.
//!
//! - A parameter starts as `Param(i)`, every other local as `Unset`.
//! - A `&str` constant is `Const` of its text, read through its memory map; any other constant
//!   or static is `Unknown`.
//! - A place is its local's value, whatever its projection: a field of a path is the path.
//! - A write through `*r` joins into the local `r` borrows mutably (its root); a write through
//!   a pointer whose root is not known makes every local whose address is taken `Unknown`.
//! - A local whose `&mut` escapes (passed on, stored, borrowed again as a whole, borrowed from
//!   two places) or that is shared while not `Freeze` is `Unknown`.
//! - A call's value is its [`Flow`]: a walked callee's returned value substituted with the
//!   call's, a model's transfer (whose `update` sets the local its argument borrows), or an
//!   environment read.

use base_db::Db;
use blaze_hash::HashMap;
use hir_ty::{
    MemoryMap, PointerCast,
    db::HirDatabase,
    drop::is_freeze,
    method_resolution::lookup_impl_const,
    next_solver::{
        AliasConst, AliasConstKind, Allocation, AllocationData, Const, ConstKind, FreeConstAliasId,
        InherentAssocConstId, TraitAssocConstId, Ty, TyKind, infer::InferCtxt,
    },
    traits::ParamEnvAndCrate,
};
use hir_ty_mir::mir::{
    BorrowKind, CastKind, LocalId, MirBody, Operand, OperandKind, ProjectionElem, Rvalue,
    StatementKind, StoredPlace, TerminatorKind, return_slot,
};
use intern::Symbol;
use rustc_type_ir::inherent::IntoKind;
use smallvec::SmallVec;

use crate::{
    Abs,
    models::{Applied, Leaf, Transfer, apply_leaf, apply_transfer},
};

/// How a callee's parameters take a call's operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Spread {
    /// Parameter `i` is operand `i`.
    Direct,
    /// A closure called through `Fn*`: parameter 0 is operand 0 and every later one is a field
    /// of operand 1, the arguments' tuple.
    Closure,
    /// A function called through `Fn*` (`from` 1) or `const_eval_select` (`from` 0): every
    /// parameter is a field of operand `from`.
    Tuple { from: u8 },
    /// Not called here (made a pointer of, put in a vtable, run by a drop): no parameter is
    /// known.
    Opaque,
}

/// How many parameters a spread tuple gives; a parameter past them is `Unknown`.
const SPREAD: usize = 8;

impl Spread {
    pub(crate) fn params(self, operands: &[Abs]) -> SmallVec<[Abs; SPREAD]> {
        let at = |i: usize| operands.get(i).copied().unwrap_or(Abs::Unknown);
        match self {
            Spread::Direct => operands.iter().copied().collect(), // alloc: inline up to eight
            Spread::Closure => {
                let mut params = SmallVec::new();
                params.push(at(0));
                params.extend(std::iter::repeat_n(at(1), SPREAD - 1));
                params
            }
            Spread::Tuple { from } => std::iter::repeat_n(at(from as usize), SPREAD).collect(), // alloc: inline, eight
            Spread::Opaque => std::iter::repeat_n(Abs::Unknown, SPREAD).collect(), // alloc: inline, eight
        }
    }
}

/// What a call's destination gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flow {
    /// Nothing known of it.
    Unknown,
    /// A constructor: its operands, joined.
    Join,
    /// A walked callee's returned value, substituted with the parameters `spread` gives it.
    Ret { ret: Abs, spread: Spread },
    /// A leaf of the models: an environment read gives the variable, any other leaf nothing
    /// known.
    Leaf { leaf: Leaf, spread: Spread },
    /// A transfer of the models.
    Transfer { transfer: Transfer, spread: Spread },
}

/// What a local holding a mutable borrow points to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Root {
    None,
    Of(LocalId),
    Many,
}

/// The solved values of one body, and what reading an operand gives.
pub(crate) struct Values<'a, 'db> {
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    infcx: &'a InferCtxt<'db>,
    body: &'a MirBody<'db>,
    abs: Box<[Abs]>,
    /// Each constant operand read so far, by its address in `body`.
    texts: HashMap<usize, Abs>,
}

fn index(local: LocalId) -> usize {
    local.into_raw().into_u32() as usize
}

/// The local whose own memory `place` names, when it is not behind a pointer.
fn own(place: &StoredPlace) -> Option<LocalId> {
    let deref = place.projection.as_slice().iter().any(|it| matches!(it, ProjectionElem::Deref));
    (!deref).then_some(place.local)
}

/// The pointer local `place` reads through when its one `Deref` comes first (`(*r).f`).
fn through(place: &StoredPlace) -> Option<LocalId> {
    let [ProjectionElem::Deref, rest @ ..] = place.projection.as_slice() else { return None };
    (!rest.iter().any(|it| matches!(it, ProjectionElem::Deref))).then_some(place.local)
}

/// The local an operand reads whole, not through a pointer.
fn whole(operand: &Operand) -> Option<LocalId> {
    match &operand.kind {
        OperandKind::Copy(place) | OperandKind::Move(place) => own(place),
        _ => None,
    }
}

/// Whether a write to `place` lands in memory whose root is not known.
fn wild(roots: &[Root], place: &StoredPlace) -> bool {
    own(place).is_none()
        && !matches!(through(place).map(|it| roots[index(it)]), Some(Root::Of(_) | Root::Many))
}

/// Marks the root of a `&mut` passed on whole as changing unseen.
fn escape(roots: &[Root], escaped: &mut [bool], operand: &Operand) {
    if let Some(local) = whole(operand)
        && let Root::Of(root) = roots[index(local)]
    {
        escaped[index(root)] = true;
    }
}

/// What a borrow of `place` points into: a local's own memory, or what the pointer it reads
/// through borrows.
fn target(roots: &[Root], place: &StoredPlace) -> Root {
    if let Some(local) = own(place) {
        return Root::Of(local);
    }
    through(place).map_or(Root::None, |pointer| roots[index(pointer)])
}

/// Points `to` at `root` too; a second root makes it `Many` and both roots change unseen.
fn point(roots: &mut [Root], escaped: &mut [bool], to: LocalId, root: Root) -> bool {
    let slot = &mut roots[index(to)];
    let next = match (*slot, root) {
        (_, Root::None) => return false,
        (old, new) if old == new => return false,
        (Root::None, new) => new,
        (old, new) => {
            for gone in [old, new] {
                if let Root::Of(local) = gone {
                    escaped[index(local)] = true;
                }
            }
            Root::Many
        }
    };
    let changed = *slot != next;
    *slot = next;
    changed
}

/// The `Push` target of a transfer the call's flow applies in place, which is not an escape.
fn updated(flow: Flow) -> Option<usize> {
    match flow {
        Flow::Transfer { transfer: Transfer::Push { target, .. }, spread: Spread::Direct } => {
            Some(target as usize)
        }
        _ => None,
    }
}

impl<'a, 'db> Values<'a, 'db> {
    /// Solves `body`, whose call ending block `b` flows as `flows[b]` (anything for a block
    /// that does not end in a call).
    pub(crate) fn solve(
        db: &'db Db,
        env: ParamEnvAndCrate<'db>,
        infcx: &'a InferCtxt<'db>,
        body: &'a MirBody<'db>,
        flows: &[Flow],
    ) -> Values<'a, 'db> {
        let locals = body.locals.len();
        let mut values = Values {
            db,
            env,
            infcx,
            body,
            abs: vec![Abs::Unset; locals].into_boxed_slice(), // alloc: one slot per local, once per body
            texts: HashMap::default(),
        };
        let (roots, escaped) = values.borrows(flows);
        for (i, &param) in body.param_locals.iter().enumerate() {
            values.abs[index(param)] = u8::try_from(i).map_or(Abs::Unknown, Abs::Param);
        }
        for (slot, &gone) in values.abs.iter_mut().zip(&escaped) {
            if gone {
                *slot = Abs::Unknown;
            }
        }
        let mut changed = true;
        while changed {
            changed = false;
            for (id, block) in body.basic_blocks.iter() {
                for statement in &block.statements {
                    if let StatementKind::Assign(place, rvalue) = &statement.kind {
                        let value = values.rvalue(rvalue);
                        changed |= values.write(&roots, place, value);
                    }
                }
                let Some(terminator) = &block.terminator else { continue };
                match &terminator.kind {
                    TerminatorKind::Call { args, destination, .. } => {
                        let flow = flows[id.into_raw().into_u32() as usize];
                        let value = values.call(&roots, flow, args, &mut changed);
                        changed |= values.write(&roots, destination, value);
                    }
                    TerminatorKind::TailCall { .. } => {
                        changed |= values.set(return_slot(), Abs::Unknown);
                    }
                    TerminatorKind::DropAndReplace { place, value, .. } => {
                        let value = values.operand(value);
                        changed |= values.write(&roots, place, value);
                    }
                    _ => {}
                }
            }
        }
        values
    }

    /// The value a call gives its destination; a transfer's update is joined into the local
    /// its argument borrows.
    fn call(&mut self, roots: &[Root], flow: Flow, args: &[Operand], changed: &mut bool) -> Abs {
        let operands: SmallVec<[Abs; SPREAD]> = args.iter().map(|arg| self.operand(arg)).collect(); // alloc: inline up to eight
        match flow {
            Flow::Unknown => Abs::Unknown,
            Flow::Join => operands.iter().fold(Abs::Unset, |all, &it| all.join(it)),
            Flow::Ret { ret, spread } => ret.substitute(&spread.params(&operands)),
            Flow::Leaf { leaf: leaf @ Leaf::EnvVar, spread } => {
                match apply_leaf(leaf, &spread.params(&operands)) {
                    Applied::Done { ret, .. } => ret,
                    Applied::Deferred | Applied::Refused(_) => Abs::Unknown,
                }
            }
            Flow::Leaf { .. } => Abs::Unknown,
            Flow::Transfer { transfer, spread } => {
                let moved = apply_transfer(transfer, &spread.params(&operands));
                if let (Spread::Direct, Some((at, update))) = (spread, moved.update)
                    && let Some(local) = args.get(at as usize).and_then(whole)
                    && let Root::Of(root) = roots[index(local)]
                {
                    *changed |= self.set(root, update);
                }
                moved.ret
            }
        }
    }

    /// The root of each local holding a mutable borrow, and the locals whose value can change
    /// unseen (made `Unknown`). Neither depends on values: the roots are a fixpoint over the
    /// body, the escapes one pass after it.
    fn borrows(&self, flows: &[Flow]) -> (Box<[Root]>, Box<[bool]>) {
        let body = self.body;
        let locals = body.locals.len();
        let mut roots = vec![Root::None; locals].into_boxed_slice(); // alloc: one slot per local, once per body
        let mut escaped = vec![false; locals].into_boxed_slice(); // alloc: one slot per local, once per body
        let mut changed = true;
        while changed {
            changed = false;
            for (_, block) in body.basic_blocks.iter() {
                for statement in &block.statements {
                    let StatementKind::Assign(dest, rvalue) = &statement.kind else { continue };
                    let root = match rvalue {
                        Rvalue::Ref(BorrowKind::Mut { .. }, place) => target(&roots, place),
                        Rvalue::Use(operand) => match whole(operand) {
                            Some(local) => roots[index(local)],
                            None => continue,
                        },
                        _ => continue,
                    };
                    if dest.projection.as_slice().is_empty() {
                        changed |= point(&mut roots, &mut escaped, dest.local, root);
                    }
                }
            }
        }
        let mut taken = vec![false; locals].into_boxed_slice(); // alloc: one slot per local, once per body
        let mut written = false;
        for (id, block) in body.basic_blocks.iter() {
            for statement in &block.statements {
                match &statement.kind {
                    StatementKind::Assign(dest, rvalue) => {
                        written |= wild(&roots, dest);
                        self.escapes(&roots, &mut escaped, &mut taken, dest, rvalue);
                    }
                    StatementKind::InlineAsm(_) => written = true,
                    _ => {}
                }
            }
            let Some(terminator) = &block.terminator else { continue };
            match &terminator.kind {
                TerminatorKind::Call { args, destination, .. } => {
                    let update = updated(flows[id.into_raw().into_u32() as usize]);
                    for (at, arg) in args.iter().enumerate() {
                        if Some(at) != update {
                            escape(&roots, &mut escaped, arg);
                        }
                    }
                    written |= wild(&roots, destination);
                }
                TerminatorKind::TailCall { args, .. } => {
                    args.iter().for_each(|arg| escape(&roots, &mut escaped, arg));
                }
                TerminatorKind::Yield { value, resume_arg, .. } => {
                    escape(&roots, &mut escaped, value);
                    escaped[index(resume_arg.local)] = true;
                }
                TerminatorKind::DropAndReplace { place, value, .. } => {
                    escape(&roots, &mut escaped, value);
                    written |= wild(&roots, place);
                }
                _ => {}
            }
        }
        if written {
            for (gone, &it) in escaped.iter_mut().zip(&taken) {
                *gone |= it;
            }
        }
        for &(local, _) in &body.upvars_in_place {
            escaped[index(local)] = true;
        }
        (roots, escaped)
    }

    /// Marks what assigning `rvalue` to `dest` lets change unseen: a `&mut` passed on other
    /// than to a local, a pointer borrowed mutably, a local shared while not `Freeze`; and
    /// the locals whose address it takes.
    fn escapes(
        &self,
        roots: &[Root],
        escaped: &mut [bool],
        taken: &mut [bool],
        dest: &StoredPlace,
        rvalue: &Rvalue,
    ) {
        let to_local = dest.projection.as_slice().is_empty();
        match rvalue {
            Rvalue::Ref(kind, place) => {
                let root = target(roots, place);
                if let Root::Of(local) = root {
                    taken[index(local)] = true;
                }
                match kind {
                    BorrowKind::Mut { .. } => {
                        if !to_local && let Root::Of(local) = root {
                            escaped[index(local)] = true;
                        }
                        if own(place).is_some()
                            && let Root::Of(local) = roots[index(place.local)]
                        {
                            escaped[index(local)] = true;
                        }
                    }
                    BorrowKind::Shared | BorrowKind::Shallow => {
                        if let Root::Of(local) = root
                            && !escaped[index(local)]
                            && !self.freeze(local)
                        {
                            escaped[index(local)] = true;
                        }
                    }
                }
            }
            // A `&mut` moved whole to a local gave the local its root.
            Rvalue::Use(_) if to_local => {}
            Rvalue::Use(operand)
            | Rvalue::Repeat(operand, _)
            | Rvalue::Cast(_, operand, _)
            | Rvalue::UnaryOp(_, operand) => escape(roots, escaped, operand),
            Rvalue::BinaryOp(_, a, b) | Rvalue::CheckedBinaryOp(_, a, b) => {
                escape(roots, escaped, a);
                escape(roots, escaped, b);
            }
            Rvalue::Aggregate(_, operands) => {
                operands.iter().for_each(|it| escape(roots, escaped, it));
            }
            Rvalue::Len(_) | Rvalue::Discriminant(_) | Rvalue::CopyForDeref(_) => {}
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => {
                match *never {}
            }
        }
    }

    fn freeze(&self, local: LocalId) -> bool {
        is_freeze(self.infcx, self.body.locals[local].ty.as_ref(), self.env.param_env)
    }

    /// Joins `value` into where a write to `place` lands, when that is a local of the body.
    fn write(&mut self, roots: &[Root], place: &StoredPlace, value: Abs) -> bool {
        if let Some(local) = own(place) {
            return self.set(local, value);
        }
        match through(place).map(|pointer| roots[index(pointer)]) {
            Some(Root::Of(local)) => self.set(local, value),
            _ => false,
        }
    }

    fn set(&mut self, local: LocalId, value: Abs) -> bool {
        let slot = &mut self.abs[index(local)];
        let joined = slot.join(value);
        let changed = joined != *slot;
        *slot = joined;
        changed
    }

    pub(crate) fn local(&self, local: LocalId) -> Abs {
        self.abs[index(local)]
    }

    fn rvalue(&mut self, rvalue: &Rvalue) -> Abs {
        match rvalue {
            Rvalue::Use(operand) | Rvalue::Repeat(operand, _) => self.operand(operand),
            Rvalue::Ref(_, place) | Rvalue::CopyForDeref(place) => self.local(place.local),
            Rvalue::Cast(kind, operand, _) => match kind {
                CastKind::PtrToPtr
                | CastKind::PointerCoercion(
                    PointerCast::Unsize
                    | PointerCast::MutToConstPointer
                    | PointerCast::ArrayToPointer
                    | PointerCast::UnsafeFnPointer,
                ) => self.operand(operand),
                _ => Abs::Unknown,
            },
            Rvalue::Aggregate(_, operands) => {
                operands.iter().fold(Abs::Unset, |all, it| all.join(self.operand(it)))
            }
            Rvalue::Len(_)
            | Rvalue::BinaryOp(..)
            | Rvalue::CheckedBinaryOp(..)
            | Rvalue::UnaryOp(..)
            | Rvalue::Discriminant(_) => Abs::Unknown,
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => {
                match *never {}
            }
        }
    }

    /// What reading `operand` gives.
    pub(crate) fn operand(&mut self, operand: &Operand) -> Abs {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => self.local(place.local),
            OperandKind::Static(_) => Abs::Unknown,
            OperandKind::Allocation { .. } | OperandKind::Constant { .. } => {
                let at = std::ptr::from_ref(operand) as usize;
                if let Some(&text) = self.texts.get(&at) {
                    return text;
                }
                let text = self.text(operand).map_or(Abs::Unknown, Abs::Const);
                self.texts.insert(at, text);
                text
            }
        }
    }

    /// The text of a `&str` constant.
    fn text(&self, operand: &Operand) -> Option<Symbol> {
        match &operand.kind {
            OperandKind::Allocation { allocation } => str_of(&allocation.as_ref()),
            OperandKind::Constant { konst, ty } if is_str_ref(ty.as_ref()) => {
                let value = evaluate(self.db, self.infcx, self.env, konst.as_ref()).ok()?;
                str_of(&*value)
            }
            _ => None,
        }
    }
}

fn is_str_ref(ty: Ty<'_>) -> bool {
    matches!(ty.kind(), TyKind::Ref(_, inner, _) if matches!(inner.kind(), TyKind::Str))
}

/// The text an evaluated `&str` points to: its bytes are an address word and a length word.
fn str_of(allocation: &AllocationData<'_>) -> Option<Symbol> {
    if !is_str_ref(allocation.ty) {
        return None;
    }
    let memory = &allocation.memory[..];
    let word = memory.len() / 2;
    if !matches!(word, 4 | 8) {
        return None;
    }
    let read = |bytes: &[u8]| {
        let mut le = [0u8; 8];
        le[..bytes.len()].copy_from_slice(bytes);
        usize::try_from(u64::from_le_bytes(le)).ok()
    };
    let addr = read(&memory[..word])?;
    let len = read(&memory[word..])?;
    if len != 0 && matches!(allocation.memory_map, MemoryMap::Empty) {
        return None;
    }
    let bytes = allocation.memory_map.get(addr, len)?;
    (bytes.len() == len).then_some(())?;
    std::str::from_utf8(bytes).ok().map(Symbol::intern)
}

/// A named constant's value, evaluated as the code generator evaluates it.
pub(crate) fn evaluate<'db>(
    db: &'db Db,
    infcx: &InferCtxt<'db>,
    env: ParamEnvAndCrate<'db>,
    konst: Const<'db>,
) -> Result<Allocation<'db>, String> {
    let ConstKind::Alias(_, AliasConst { kind, args, .. }) = konst.kind() else {
        return Err(format!("`{konst:?}` is not a named constant")); // alloc: error path
    };
    match kind {
        AliasConstKind::Projection { def_id: TraitAssocConstId(id) }
        | AliasConstKind::InherentImpl { def_id: InherentAssocConstId(id) }
        | AliasConstKind::Free { def_id: FreeConstAliasId(id) } => {
            let (id, args) = lookup_impl_const(infcx, env.param_env, id, args);
            db.const_eval(id, args, Some(env))
        }
        AliasConstKind::Anon { def_id } => db.anon_const_eval(def_id.0, args, Some(env)),
        AliasConstKind::InherentSelf { .. } => {
            return Err(format!("`{konst:?}`: an inherent const in Self form after type check")); // alloc: error path
        }
    }
    .map_err(|error| format!("`{konst:?}` does not evaluate: {error:?}")) // alloc: error path
}
