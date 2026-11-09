//! The `for const` expansion (`#![feature(blaze_comptime)]`, docs/v2/plan/design/comptime.md,
//! "MIR: one generic body, unrolled at monomorphization"): before the fill, each
//! [`TerminatorKind::ComptimeFor`] loop is unrolled once per member of its domain, in
//! declaration order. An iteration is the blocks its entry dominates (lowering builds it so);
//! each copy has the binder replaced by the member, `base.[f]` turned into the field's
//! projection, its own copy of every local whose type names the binder, and its
//! [`TerminatorKind::ComptimeNext`] going on to the next copy (the last to `after`). A loop is
//! expanded once its domain names no binder, so an outer loop goes before the loops inside it.

use blaze_mem::{Tag, frame};
use hir_def::{AdtId, FieldId, VariantId};
use hir_ty::comptime::{ReflectedMember, reflect_members_checked, reflected_member};
use hir_ty::mir::{ComptimeBinder, ComptimeUnexpanded, MirComptimeForm};
use rustc_type_ir::{
    TypeFlags, TypeFoldable, TypeFolder, TypeSuperFoldable, TypeVisitableExt,
    inherent::{GenericArgs as _, IntoKind, Ty as _},
    reflect::ReflectedDomain,
};
use smallvec::SmallVec;

use super::super::{
    BasicBlockId, FieldIndex, LocalId, MirBody, MirLowerError, Operand, OperandKind,
    ProjectionElem, Rvalue, StatementKind, StoredPlace, Terminator, TerminatorKind,
    framed::{block_id, block_index},
    optimize::{Dominators, successors_mut},
};
use super::Filler;
use crate::next_solver::{
    AliasConstKind, AnonConstIdWrapper, Const, ConstKind, DbInterner, GenericArgs, Ty, TyKind,
};
use hir_ty::db::HirDatabase as _;

/// How many loops one body may expand, the copies of inner loops counted: the bound of the
/// expansion (rule 18). A body past it is refused, naming it.
const EXPANSION_FUEL: usize = 1 << 16;

/// The fold that puts `member` in place of the binder `binder`, and nothing else.
pub(super) struct ReplaceBinder<'db> {
    pub(super) interner: DbInterner<'db>,
    pub(super) binder: ComptimeBinder,
    pub(super) member: Ty<'db>,
}

impl<'db> TypeFolder<DbInterner<'db>> for ReplaceBinder<'db> {
    fn cx(&self) -> DbInterner<'db> {
        self.interner
    }

    fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        if !ty.has_type_flags(TypeFlags::HAS_TY_PLACEHOLDER) {
            return ty;
        }
        if ComptimeBinder::of(ty) == Some(self.binder) {
            return self.member;
        }
        ty.super_fold_with(self)
    }
}

/// Whether `t` names the binder `binder`.
fn mentions<'db, T>(interner: DbInterner<'db>, binder: ComptimeBinder, t: T) -> bool
where
    T: TypeFoldable<DbInterner<'db>> + PartialEq + Copy,
{
    // Any type that is no placeholder tells a replaced one from the binder.
    let member = Ty::new_unit(interner);
    t.fold_with(&mut ReplaceBinder { interner, binder, member }) != t
}

/// Whether `ty` names the binder inside a closure's or coroutine's type, whose body is MIR of
/// its own that no copy reaches.
fn closure_names(interner: DbInterner<'_>, binder: ComptimeBinder, ty: Ty<'_>) -> bool {
    ty.walk().any(|arg| {
        arg.ty().is_some_and(|it| {
            matches!(
                it.kind(),
                TyKind::Closure(..) | TyKind::Coroutine(..) | TyKind::CoroutineClosure(..)
            ) && mentions(interner, binder, it)
        })
    })
}

fn unexpanded<'db>(form: MirComptimeForm, owner: impl std::fmt::Debug) -> MirLowerError<'db> {
    // alloc: error path
    MirLowerError::NotSupported(format!(
        "{} (in {owner:?})",
        ComptimeUnexpanded::new(form, "monomorphization's expansion")
    ))
}

/// `member`, about to be the binder of a `for const` loop in `owner`, when it is zero-sized: the
/// binder's value is its member's one value, made from no bytes where lowering binds the loop's
/// pattern (`lower/comptime.rs`, also in an inline constant that names it), which core's field
/// and variant representing types are (`PhantomData`). A member with bytes is refused naming
/// it, never given a value of no bytes.
fn zero_sized_member<'db>(
    filler: &Filler<'db>,
    member: Ty<'db>,
    owner: impl std::fmt::Debug,
) -> Result<(), MirLowerError<'db>> {
    let db = filler.infcx.interner.db;
    let layout = db.layout_of_ty(member.store(), filler.trait_env.store()).map_err(|e| {
        // alloc: error path
        MirLowerError::NotSupported(format!(
            "the member {member:?} of a `for const` loop in {owner:?} has no layout: {e:?}"
        ))
    })?;
    if layout.size.bytes() == 0 {
        return Ok(());
    }
    // alloc: error path
    Err(MirLowerError::NotSupported(format!(
        "the member {member:?} of a `for const` loop in {owner:?} is {} bytes: the loop's \
         binding is the member's one value, which only a zero-sized field or variant \
         representing type has",
        layout.size.bytes()
    )))
}

/// The body of an inline constant inside `for const` loops with each loop's binder replaced by
/// its member: the constant's arguments past its item's own, one per loop around it, outermost
/// first (MIR lowering's `inline_const_args`). Any other body is left as it is.
pub(super) fn bind_inline_const<'db>(
    db: &'db base_db::Db,
    filler: &mut Filler<'db>,
    body: &mut MirBody<'db>,
) -> Result<(), MirLowerError<'db>> {
    let crate::InferBodyId::AnonConstId(konst) = body.owner else { return Ok(()) };
    let binders = hir_ty::mir::comptime_binders_of_anon_const(db, konst);
    if binders.is_empty() {
        return Ok(());
    }
    let interner = filler.infcx.interner;
    let args = filler.subst;
    let own = GenericArgs::identity_for_item(interner, body.owner.generic_def(db).into()).len();
    if args.len() != own + binders.len() {
        // alloc: error path
        return Err(MirLowerError::NotSupported(format!(
            "the inline constant {konst:?} inside {} `for const` loop(s) evaluated with {} \
             arguments, not its item's {own} and one member per loop",
            binders.len(),
            args.len()
        )));
    }
    for (k, &binder) in binders.iter().enumerate() {
        let member = args.as_slice()[own + k].ty();
        let Some(member) = member.filter(|it| !it.has_type_flags(TypeFlags::HAS_TY_PLACEHOLDER))
        else {
            // alloc: error path
            return Err(MirLowerError::NotSupported(format!(
                "the inline constant {konst:?} evaluated before its `for const` loop was \
                 expanded: argument {} is {:?}, no member",
                own + k,
                args.as_slice()[own + k]
            )));
        };
        zero_sized_member(filler, member, konst)?;
        if let Some((_, local)) =
            body.locals.iter().find(|(_, it)| closure_names(interner, binder, it.ty.as_ref()))
        {
            // alloc: error path
            return Err(MirLowerError::NotSupported(format!(
                "a closure or `async` block of type {:?} in the inline constant {konst:?} names \
                 the binder of a `for const` loop: its body would need the member too",
                local.ty
            )));
        }
        filler.replace = Some((binder, member));
        let filled = filler.fill_body(body);
        filler.replace = None;
        filled?;
    }
    Ok(())
}

/// Every `for const` loop of `body` unrolled: `body` itself when it has none, so a body
/// without the feature is filled exactly as before.
pub(super) fn expand<'db>(
    filler: &mut Filler<'db>,
    body: &mut MirBody<'db>,
) -> Result<(), MirLowerError<'db>> {
    if !body.has_comptime_loop() {
        return Ok(());
    }
    let mut fuel = EXPANSION_FUEL;
    while let Some(head) = next_head(body) {
        if fuel == 0 {
            // alloc: error path
            return Err(MirLowerError::NotSupported(format!(
                "more than {EXPANSION_FUEL} `for const` loops to expand in {:?}",
                body.owner
            )));
        }
        fuel -= 1;
        expand_one(filler, body, head)?;
    }
    // A head left names a binder no expansion replaced: one of no enclosing loop.
    for (_, block) in body.basic_blocks.iter() {
        if let Some(form) =
            block.terminator.as_ref().and_then(|it| MirComptimeForm::of_terminator(&it.kind))
        {
            return Err(unexpanded(form, body.owner));
        }
    }
    retire_binder_locals(filler.infcx.interner, body)
}

/// The first loop head whose domain names no binder: an outermost one left.
fn next_head(body: &MirBody<'_>) -> Option<BasicBlockId> {
    body.basic_blocks.iter().find_map(|(id, block)| match &block.terminator.as_ref()?.kind {
        TerminatorKind::ComptimeFor { domain, .. }
            if !domain.as_ref().has_type_flags(TypeFlags::HAS_TY_PLACEHOLDER) =>
        {
            Some(id)
        }
        _ => None,
    })
}

/// The domain `Fields<B, ()>` or `Variants<B>` as its kind and container `B`.
fn decode_domain<'db>(
    interner: DbInterner<'db>,
    domain: Ty<'db>,
) -> Option<(ReflectedDomain, Ty<'db>)> {
    let lang = interner.lang_items();
    let TyKind::Adt(def, args) = domain.kind() else { return None };
    let AdtId::StructId(id) = def.def_id() else { return None };
    let container = args.type_at(0);
    if Some(id) == lang.ReflectFields {
        // A filter (`P != ()`) is refused by inference; nothing filtered reaches here.
        let unfiltered = args.len() < 2
            || matches!(args.type_at(1).kind(), TyKind::Tuple(tys) if tys.is_empty());
        return unfiltered.then_some((ReflectedDomain::Fields, container));
    }
    (Some(id) == lang.ReflectVariants).then_some((ReflectedDomain::Variants, container))
}

/// The projection `base.[f]` is once `f` is `member`: `Field(i)`, or `Field(0), Downcast(v),
/// Field(i)` through the `value` of a `VariantOf<E, v>`. `None` for a variant, no field.
fn member_projection(
    interner: DbInterner<'_>,
    member: Ty<'_>,
) -> Option<SmallVec<[ProjectionElem<LocalId>; 3]>> {
    let (named, _) = reflected_member(interner, member)?;
    let mut elems = SmallVec::new();
    match named {
        ReflectedMember::TupleField(i) => elems.push(ProjectionElem::Field(FieldIndex(i))),
        ReflectedMember::Field(FieldId { parent: VariantId::EnumVariantId(v), local_id }) => {
            elems.push(ProjectionElem::Field(FieldIndex(0)));
            elems.push(ProjectionElem::Downcast(VariantId::EnumVariantId(v)));
            elems.push(ProjectionElem::Field(local_id.into()));
        }
        ReflectedMember::Field(FieldId { local_id, .. }) => {
            elems.push(ProjectionElem::Field(local_id.into()));
        }
        ReflectedMember::Variant(_) => return None,
    }
    Some(elems)
}

fn expand_one<'db>(
    filler: &mut Filler<'db>,
    body: &mut MirBody<'db>,
    head: BasicBlockId,
) -> Result<(), MirLowerError<'db>> {
    let interner = filler.infcx.interner;
    let owner = body.owner;
    let Some(Terminator {
        span,
        kind: TerminatorKind::ComptimeFor { binder, domain, body: first, after },
    }) = body.basic_blocks[head].terminator.clone()
    // clone: the head's parts, before it is rewritten
    else {
        return Err(unexpanded(MirComptimeForm::ForHead, owner));
    };
    let domain = filler.fill(domain.as_ref())?;
    let Some((kind, container)) = decode_domain(interner, domain) else {
        // alloc: error path
        return Err(MirLowerError::NotSupported(format!(
            "`for const` over {domain:?} in {owner:?}: no `Fields<T>` or `Variants<T>`"
        )));
    };
    let Some(members) = reflect_members_checked(interner, container, kind) else {
        // alloc: error path
        return Err(MirLowerError::NotSupported(format!(
            "`for const` over the members of {container:?} in {owner:?}: no reflected \
             container (a struct that is not `Struct`, a union, or no enum for `Variants`)"
        )));
    };
    if let Some(callee) =
        body.inlined.callees.iter().find(|it| mentions(interner, binder, it.args.as_ref()))
    {
        // alloc: error path
        return Err(MirLowerError::NotSupported(format!(
            "a callee inlined into {owner:?} with arguments {:?} that name the binder of a \
             `for const` loop",
            callee.args
        )));
    }

    // The iteration: what its entry dominates. The head and `after` are outside it.
    // alloc: one list per loop expanded, kept across the copies
    let region: Vec<BasicBlockId> = frame::task(Tag::Mir, |frame| {
        let dominators = Dominators::new(body, frame);
        body.basic_blocks
            .iter()
            .map(|(id, _)| id)
            .filter(|&b| dominators.dominates(first, b))
            .collect()
    });
    let mut in_region: SmallVec<[Option<usize>; 64]> =
        SmallVec::from_elem(None, body.basic_blocks.len());
    for (at, &block) in region.iter().enumerate() {
        in_region[block_index(block)] = Some(at);
    }
    let entry_at = in_region[block_index(first)].expect("the entry dominates itself");

    // alloc: the locals each copy gets its own of
    let own: Vec<LocalId> = body
        .locals
        .iter()
        .filter(|(_, local)| mentions(interner, binder, local.ty.as_ref()))
        .map(|(id, _)| id)
        .collect();
    if let Some(&local) =
        own.iter().find(|&&it| closure_names(interner, binder, body.locals[it].ty.as_ref()))
    {
        // alloc: error path
        return Err(MirLowerError::NotSupported(format!(
            "a closure or `async` block of type {:?} in {owner:?} names the binder of a \
             `for const` loop: its body would need a copy per member",
            body.locals[local].ty
        )));
    }

    let base = body.basic_blocks.len();
    let count = members.len();
    let entry_of = |copy: usize| block_id(base + copy * region.len() + entry_at);
    for (copy, member) in members.iter().enumerate() {
        zero_sized_member(filler, member, owner)?;
        let offset = base + copy * region.len();
        let next = if copy + 1 < count { entry_of(copy + 1) } else { after };
        let mut map_local: SmallVec<[(LocalId, LocalId); 8]> = SmallVec::new();
        for &local in &own {
            // clone: the declaration, its type replaced below
            let mut decl = body.locals[local].clone();
            decl.ty = ReplaceBinder { interner, binder, member }.fold_ty(decl.ty.as_ref()).store();
            map_local.push((local, body.locals.alloc(decl)));
        }
        let local =
            |it: LocalId| map_local.iter().find(|(from, _)| *from == it).map_or(it, |&(_, to)| to);
        let projection = member_projection(interner, member);
        let mut no_field = false;
        let mut fix_place = |place: &mut StoredPlace| {
            place.local = local(place.local);
            let elems = place.projection.as_slice();
            let touches = elems.iter().any(|it| {
                matches!(it, ProjectionElem::Index(_))
                    || *it == ProjectionElem::ReflectedField(binder)
            });
            if !touches {
                return;
            }
            let mut out: SmallVec<[ProjectionElem<LocalId>; 8]> = SmallVec::new();
            for &elem in elems {
                match elem {
                    ProjectionElem::Index(it) => out.push(ProjectionElem::Index(local(it))),
                    ProjectionElem::ReflectedField(it) if it == binder => match &projection {
                        Some(field) => out.extend(field.iter().copied()),
                        None => no_field = true,
                    },
                    other => out.push(other),
                }
            }
            place.projection = super::super::Projection::new_from_iter(out).store();
        };
        filler.replace = Some((binder, member));
        for (at, &block) in region.iter().enumerate() {
            // clone: the copy of the iteration's block for this member
            let mut data = body.basic_blocks[block].clone();
            let filled = filler.fill_block(&mut data);
            if let Err(error) = filled {
                filler.replace = None;
                return Err(error);
            }
            for statement in &mut data.statements {
                MirBody::walk_statement_places(&mut statement.kind, &mut fix_place);
                if let StatementKind::StorageLive(it) | StatementKind::StorageDead(it) =
                    &mut statement.kind
                {
                    *it = local(*it);
                }
            }
            if let Some(terminator) = &mut data.terminator {
                MirBody::walk_terminator_places(&mut terminator.kind, &mut fix_place);
                if matches!(terminator.kind, TerminatorKind::ComptimeNext { head: it } if it == head)
                {
                    terminator.kind = TerminatorKind::Goto { target: next };
                } else {
                    successors_mut(&mut terminator.kind, |it| {
                        if let Some(Some(at)) = in_region.get(block_index(*it)) {
                            *it = block_id(offset + at);
                        }
                    });
                }
            }
            let id = body.basic_blocks.alloc(data);
            debug_assert_eq!(id, block_id(offset + at));
        }
        filler.replace = None;
        if no_field {
            // alloc: error path
            return Err(MirLowerError::NotSupported(format!(
                "`base.[f]` in {owner:?} with `f` the variant {member:?}, no field"
            )));
        }
        // What the body says of the iteration's locals and blocks, for each copy.
        let unreached: SmallVec<[BasicBlockId; 4]> = body
            .unreached
            .iter()
            .filter_map(|it| in_region.get(block_index(*it)).copied().flatten())
            .map(|at| block_id(offset + at))
            .collect();
        body.unreached.extend(unreached);
        for &(from, to) in &map_local {
            if body.promoted_constants.contains(&from) {
                body.promoted_constants.push(to);
            }
        }
        // A copy's locals end their scopes where the originals do: the last copy's first, as
        // a later iteration's would.
        let mut ends: SmallVec<[LocalId; 16]> = SmallVec::new();
        for &it in &body.scope_ends {
            if let Some(&(_, to)) = map_local.iter().find(|(from, _)| *from == it) {
                ends.push(to);
            }
            ends.push(it);
        }
        if ends.len() != body.scope_ends.len() {
            body.scope_ends.clear();
            body.scope_ends.extend(ends);
        }
    }

    // The head goes into the first copy; the original iteration is reached no more.
    let target = if count == 0 { after } else { entry_of(0) };
    body.basic_blocks[head].terminator =
        Some(Terminator { span, kind: TerminatorKind::Goto { target } });
    for &block in &region {
        let data = &mut body.basic_blocks[block];
        data.statements.clear();
        let span = data.terminator.as_ref().map_or(span, |it| it.span);
        data.terminator = Some(Terminator { span, kind: TerminatorKind::Unreachable });
    }
    body.unreached.retain(|it| in_region.get(block_index(*it)).copied().flatten().is_none());
    Ok(())
}

/// The originals of the locals the copies replaced name a binder and are used by no block
/// now: given `()`, so the fill has nothing to normalize there. One still used is a binder
/// that no loop expanded.
fn retire_binder_locals<'db>(
    interner: DbInterner<'db>,
    body: &mut MirBody<'db>,
) -> Result<(), MirLowerError<'db>> {
    let mut used: SmallVec<[bool; 64]> = SmallVec::from_elem(false, body.locals.len());
    let mut mark = |it: LocalId| used[crate::mir::framed::local_index(it)] = true;
    body.visit_places(|place| {
        mark(place.local);
        for elem in place.projection.as_slice() {
            if let ProjectionElem::Index(it) = *elem {
                mark(it);
            }
        }
    });
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::StorageLive(it) | StatementKind::StorageDead(it) = statement.kind
            {
                mark(it);
            }
        }
    }
    let unit = Ty::new_unit(interner).store();
    for (id, local) in body.locals.iter_mut() {
        if !local.ty.as_ref().has_type_flags(TypeFlags::HAS_TY_PLACEHOLDER) {
            continue;
        }
        if used[crate::mir::framed::local_index(id)] {
            return Err(unexpanded(MirComptimeForm::ForHead, body.owner));
        }
        local.ty = unit.clone(); // clone: an interned type handle
    }
    Ok(())
}

/// The `if const { COND }` fold (docs/v2/plan/design/comptime.md, "Syntax"): in an instance
/// whose `for const` loops are expanded and filled, each `SwitchInt` on an inline constant,
/// now monomorphic, becomes a `Goto` to the arm its value picks, and the blocks only the other
/// arm reached are emptied (no statement, `Unreachable`), so a dead arm's `const { panic!() }`
/// is never evaluated. Only an inline constant's switch is folded; any other is left to run.
pub(super) fn fold_if_const<'db>(
    db: &'db base_db::Db,
    filler: &Filler<'db>,
    body: &mut MirBody<'db>,
) -> Result<(), MirLowerError<'db>> {
    let before = reachable(body);
    let mut folded = false;
    let blocks: SmallVec<[BasicBlockId; 64]> = body.basic_blocks.iter().map(|(id, _)| id).collect();
    for block in blocks {
        let data = &body.basic_blocks[block];
        let Some(Terminator { kind: TerminatorKind::SwitchInt { discr, targets }, .. }) =
            &data.terminator
        else {
            continue;
        };
        let Some(konst) = switched_constant(db, &data.statements, discr) else { continue };
        let ConstKind::Alias(_, alias) = konst.kind() else { continue };
        let AliasConstKind::Anon { def_id: AnonConstIdWrapper(id) } = alias.kind else { continue };
        if !id.is_inline_const(db) {
            continue;
        }
        if alias.args.has_type_flags(TypeFlags::HAS_PARAM | TypeFlags::HAS_PLACEHOLDER) {
            // alloc: error path
            return Err(MirLowerError::NotSupported(format!(
                "the `if const` condition {id:?} in {:?} still generic after the expansion: \
                 {:?}",
                body.owner, alias.args
            )));
        }
        let value =
            db.anon_const_eval(id, alias.args, Some(filler.trait_env)).map_err(|error| {
                // alloc: error path
                let name = format!("the `if const` condition {id:?} in {:?}", body.owner);
                MirLowerError::ConstEvalError(name.into(), Box::new(error))
            })?;
        let memory = &value.memory;
        let Some(&byte) = memory.first().filter(|_| memory.len() == 1) else {
            // alloc: error path
            return Err(MirLowerError::NotSupported(format!(
                "the `if const` condition {id:?} in {:?} is no `bool`: {} bytes",
                body.owner,
                memory.len()
            )));
        };
        let value = u128::from(byte);
        let target = targets
            .iter()
            .find_map(|(it, target)| (it == value).then_some(target))
            .unwrap_or_else(|| targets.otherwise());
        let terminator = body.basic_blocks[block].terminator.as_mut().expect("matched above");
        terminator.kind = TerminatorKind::Goto { target };
        folded = true;
    }
    if !folded {
        return Ok(());
    }
    let after = reachable(body);
    for (id, data) in body.basic_blocks.iter_mut() {
        let at = block_index(id);
        if before[at] && !after[at] {
            data.statements.clear();
            let span = data.terminator.as_ref().map(|it| it.span);
            if let Some(span) = span {
                data.terminator = Some(Terminator { span, kind: TerminatorKind::Unreachable });
            }
        }
    }
    body.unreached.retain(|it| after[block_index(*it)]);
    Ok(())
}

/// The constant a switch's operand is: the operand itself, or the local the block's last write
/// to it (`_n = const C`) set. `None` for anything else, which the fold leaves alone.
fn switched_constant<'db>(
    _db: &'db base_db::Db,
    statements: &[super::super::Statement],
    discr: &Operand,
) -> Option<Const<'db>> {
    let constant = |op: &Operand| match &op.kind {
        OperandKind::Constant { konst, .. } => Some(konst.as_ref()),
        _ => None,
    };
    let local = match &discr.kind {
        OperandKind::Constant { .. } => return constant(discr),
        OperandKind::Copy(place) | OperandKind::Move(place)
            if place.projection.as_slice().is_empty() =>
        {
            place.local
        }
        _ => return None,
    };
    for statement in statements.iter().rev() {
        match &statement.kind {
            StatementKind::Assign(place, rvalue) if place.local == local => {
                return match rvalue {
                    Rvalue::Use(op) if place.projection.as_slice().is_empty() => constant(op),
                    _ => None,
                };
            }
            StatementKind::Deinit(place) if place.local == local => return None,
            StatementKind::StorageDead(it) | StatementKind::StorageLive(it) if *it == local => {
                return None;
            }
            StatementKind::InlineAsm(_) => return None,
            _ => {}
        }
    }
    None
}

/// Which blocks the start block reaches, by every edge (unwind and cleanup too).
fn reachable(body: &MirBody<'_>) -> SmallVec<[bool; 64]> {
    let mut seen: SmallVec<[bool; 64]> = SmallVec::from_elem(false, body.basic_blocks.len());
    let mut stack: SmallVec<[BasicBlockId; 32]> = SmallVec::new();
    stack.push(body.start_block);
    // Bounded by the blocks: each is pushed once it is first seen.
    seen[block_index(body.start_block)] = true;
    while let Some(block) = stack.pop() {
        let Some(terminator) = &body.basic_blocks[block].terminator else { continue };
        let mut kind = terminator.kind.clone(); // clone: `successors_mut` takes it mutably
        successors_mut(&mut kind, |it| {
            let at = block_index(*it);
            if !seen[at] {
                seen[at] = true;
                stack.push(*it);
            }
        });
    }
    seen
}
