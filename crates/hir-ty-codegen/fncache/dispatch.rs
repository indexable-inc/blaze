//! A function's address a crate keeps in a static to choose its code once, by the CPU it runs
//! on: memchr's runtime dispatch, and any written as it is. For [`super::tests_reach`] such a
//! static is no state other tests change and a call through it no function pointer made of
//! data: what it holds is one of the functions the code stores in it, which the test's hash
//! has, as it has std's own choice of code by the CPU.
//!
//! A static is one when it is a `static` (not `mut`) of core's `AtomicPtr`, declared in a
//! block of a body of a module (a block in a block of it too), whose value is a function's address
//! made into a pointer, and every body that can name it (the one its block is in, the
//! closures of that body, and every item declared in a block of it, through their blocks)
//! only `load`s it and `store`s a function's address made into a pointer in it. A body there
//! of an `async` block or a coroutine, whose code is not gone through, makes it none.

use base_db::Db;
use blaze_hash::HashSet;
use hir_def::{
    AdtId, AssocItemId, DefWithBodyId, HasModule, ItemContainerId, Lookup, ModuleDefId, StaticId,
    expr_store::Body,
    hir::{ClosureKind, Expr},
    item_scope::ItemScope,
    signatures::{FunctionSignature, StaticFlags, StaticSignature, StructSignature},
};
use hir_ty::{
    InferBodyId, PointerCast,
    db::HirDatabase,
    mir::{
        CastKind, LocalId, MirBody, Operand, OperandKind, ProjectionElem, Rvalue, StatementKind,
        StoredPlace, TerminatorKind, return_slot,
    },
    next_solver::TyKind,
};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind};

/// Whether `id` is a static that chooses a function, as the module says.
pub(super) fn is_cache(db: &Db, id: StaticId) -> bool {
    let not = why_not(db, id);
    if let Some(why) = not
        && *super::TRACE
    {
        eprintln!("fn-cache: static {id:?} chooses no function: {why}");
    }
    not.is_none()
}

/// Why `id` is not a static that chooses a function.
fn why_not(db: &Db, id: StaticId) -> Option<&'static str> {
    if StaticSignature::of(db, id).flags.contains(StaticFlags::MUTABLE)
        || !is_atomic_ptr(db, db.type_for_static(id).skip_binder())
    {
        return Some("mutable or not an AtomicPtr");
    }
    let ItemContainerId::ModuleId(module) = id.lookup(db).container else {
        return Some("not declared in a block");
    };
    let Some(block) = module.block(db) else { return Some("not of a module's block") };
    // A block in a block is in the same body: a body with a macro call has a block of its own.
    let mut outer = block.module(db);
    while let Some(it) = outer.block(db) {
        outer = it.module(db);
    }
    // alloc: one worklist of bodies a static, filled with the owners first
    let mut bodies = Vec::new();
    owners(db, outer, &mut bodies);
    let Some(owner) = bodies
        .iter()
        .copied()
        .find(|&it| Body::of(db, it).blocks(db).any(|(found, _)| found == block))
    else {
        return Some("no body has its block");
    };
    let Ok(init) = db.mir_body(InferBodyId::DefWithBodyId(id.into())) else {
        return Some("its initializer has no MIR");
    };
    if !made_of_fn(db, init, return_slot(), &mut HashSet::default()) {
        if *super::TRACE {
            eprintln!("fn-cache: {:#?}", init.basic_blocks);
        }
        return Some("its first value is no function's address");
    }
    bodies.clear();
    bodies.push(owner);
    let mut seen = HashSet::from_iter([owner]);
    // alloc: reused for every module of a block, a static
    let mut found = Vec::new();
    // alloc: reused for every body, a static
    let mut mir = Vec::new();
    while let Some(at) = bodies.pop() {
        let store = Body::of(db, at);
        let coroutine = store.exprs().any(|(_, expr)| {
            matches!(expr, Expr::Closure { closure_kind, .. } if closure_kind != ClosureKind::Closure)
        });
        if coroutine {
            return Some("a coroutine can name it");
        }
        for (_, def_map) in store.blocks(db) {
            for (_, data) in def_map.modules(db) {
                declared(db, data.scope(db), &mut found);
                bodies.extend(found.drain(..).filter(|&it| seen.insert(it)));
            }
        }
        let Ok(body) = db.mir_body(InferBodyId::DefWithBodyId(at)) else {
            return Some("a body that can name it has no MIR");
        };
        mir.clear();
        mir.push(body);
        while let Some(body) = mir.pop() {
            if !only_dispatches(db, body, id) {
                if *super::TRACE {
                    eprintln!("fn-cache: {:?}: {:#?}", body.owner, body.basic_blocks);
                }
                return Some("a body uses it otherwise");
            }
            for &closure in &body.closures {
                let Ok(it) = db.mir_body_for_closure(closure) else {
                    return Some("a closure that can name it has no MIR");
                };
                mir.push(it);
            }
        }
    }
    None
}

/// Whether `operand`, which a body of a crate outside the toolchain makes a function pointer
/// of, is what it loaded from a static `is_cache` says chooses a function.
pub(super) fn loaded_from(
    db: &Db,
    body: &MirBody<'_>,
    operand: &Operand,
    is_cache: &impl Fn(StaticId) -> bool,
) -> bool {
    let Some(local) = whole_local(operand) else { return false };
    let mut seen = HashSet::default();
    loaded(db, body, local, is_cache, &mut seen)
}

fn loaded(
    db: &Db,
    body: &MirBody<'_>,
    local: LocalId,
    is_cache: &impl Fn(StaticId) -> bool,
    seen: &mut HashSet<LocalId>,
) -> bool {
    if !seen.insert(local) {
        return true;
    }
    if body.param_locals.contains(&local) || borrowed(body, local) {
        return false;
    }
    let mut any = false;
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            let StatementKind::Assign(place, rvalue) = &statement.kind else { continue };
            if place.local != local {
                continue;
            }
            any = true;
            let from = match rvalue {
                Rvalue::Use(it) if place.projection.as_slice().is_empty() => whole_local(it),
                _ => None,
            };
            match from {
                Some(from) if loaded(db, body, from, is_cache, seen) => {}
                _ => return false,
            }
        }
        let Some(terminator) = &block.terminator else { continue };
        let TerminatorKind::Call { func, args, destination, .. } = &terminator.kind else {
            continue;
        };
        if destination.local != local {
            continue;
        }
        any = true;
        let loads = destination.projection.as_slice().is_empty()
            && atomic_ptr_method(db, func) == Some("load")
            && args.first().and_then(|it| static_of(body, it)).is_some_and(is_cache);
        if !loads {
            return false;
        }
    }
    any
}

/// The bodies of what `scope` declares.
fn declared(db: &Db, scope: &ItemScope, bodies: &mut Vec<DefWithBodyId>) {
    let assoc = |bodies: &mut Vec<DefWithBodyId>, item: AssocItemId| match item {
        AssocItemId::FunctionId(id) => bodies.push(id.into()),
        AssocItemId::ConstId(id) => bodies.push(id.into()),
        AssocItemId::TypeAliasId(_) => {}
    };
    for def in scope.declarations() {
        match def {
            ModuleDefId::FunctionId(id) => bodies.push(id.into()),
            ModuleDefId::ConstId(id) => bodies.push(id.into()),
            ModuleDefId::StaticId(id) => bodies.push(id.into()),
            ModuleDefId::TraitId(id) => {
                for &(_, item) in id.trait_items(db).items.iter() {
                    assoc(bodies, item);
                }
            }
            _ => {}
        }
    }
    bodies.extend(scope.unnamed_consts().map(DefWithBodyId::from));
    for imp in scope.impls() {
        for &(_, item) in imp.impl_items(db).items.iter() {
            assoc(bodies, item);
        }
    }
}

/// Pushes to `bodies` the bodies of what `module`, of no block, declares: one of them is the
/// body a block of the module is in.
fn owners<'db>(db: &'db Db, module: hir_def::ModuleIdLt<'db>, bodies: &mut Vec<DefWithBodyId>) {
    let def_map = module.def_map(db);
    if let Some((_, data)) = def_map.modules(db).find(|(it, _)| *it == module) {
        declared(db, data.scope(db), bodies);
    }
}

/// Whether `body` uses the static `id` only as the module says: copies of where it is in
/// locals, `load` of it, and `store` in it of a function's address.
fn only_dispatches(db: &Db, body: &MirBody<'_>, id: StaticId) -> bool {
    // The locals that hold where the static is.
    let mut at: HashSet<LocalId> = HashSet::default();
    loop {
        let before = at.len();
        for (_, block) in body.basic_blocks.iter() {
            for statement in &block.statements {
                if let StatementKind::Assign(place, rvalue) = &statement.kind
                    && place.projection.as_slice().is_empty()
                    && copies(rvalue, id, &at)
                {
                    at.insert(place.local);
                }
            }
        }
        if at.len() == before {
            break;
        }
    }
    let place_names = |place: &StoredPlace| {
        at.contains(&place.local)
            || place
                .projection
                .as_slice()
                .iter()
                .any(|it| matches!(it, ProjectionElem::Index(local) if at.contains(local)))
    };
    let names = |operand: &Operand| match &operand.kind {
        OperandKind::Static(it) => *it == id,
        OperandKind::Copy(place) | OperandKind::Move(place) => place_names(place),
        _ => false,
    };
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            match &statement.kind {
                StatementKind::Assign(place, rvalue) => {
                    if place_names(place) && !at.contains(&place.local) {
                        return false;
                    }
                    if place.projection.as_slice().is_empty() && copies(rvalue, id, &at) {
                        continue;
                    }
                    if place_names(place) {
                        return false;
                    }
                    let mut named = false;
                    rvalue.for_each_operand(&mut |it| named |= names(it));
                    let read = match rvalue {
                        Rvalue::Ref(_, it)
                        | Rvalue::Len(it)
                        | Rvalue::Discriminant(it)
                        | Rvalue::CopyForDeref(it) => place_names(it),
                        _ => false,
                    };
                    if named || read {
                        return false;
                    }
                }
                StatementKind::InlineAsm(_) if !at.is_empty() => return false,
                StatementKind::InlineAsm(_)
                | StatementKind::FakeRead(_)
                | StatementKind::Deinit(_)
                | StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::Nop => {}
            }
        }
        let Some(terminator) = &block.terminator else { continue };
        if let TerminatorKind::Call { func, args, destination, .. } = &terminator.kind
            && let Some(method @ ("load" | "store")) = atomic_ptr_method(db, func)
            && args.first().is_some_and(|it| names(it))
        {
            if place_names(destination) || args[1..].iter().any(|it| names(it)) {
                return false;
            }
            if method == "store"
                && !args.get(1).is_some_and(|it| fn_address(body, it, &mut HashSet::default()))
            {
                return false;
            }
            continue;
        }
        let mut named = false;
        terminator.kind.for_each_operand(&mut |it| named |= names(it));
        let place = match &terminator.kind {
            TerminatorKind::Call { destination, .. } => place_names(destination),
            TerminatorKind::DropAndReplace { place, .. } => place_names(place),
            _ => false,
        };
        if named || place {
            return false;
        }
    }
    true
}

/// Whether `rvalue` is where the static `id` is, or a copy of what a local of `at` holds.
fn copies(rvalue: &Rvalue, id: StaticId, at: &HashSet<LocalId>) -> bool {
    match rvalue {
        Rvalue::Use(Operand { kind: OperandKind::Static(it), .. }) => *it == id,
        Rvalue::Use(it) => whole_local(it).is_some_and(|it| at.contains(&it)),
        Rvalue::CopyForDeref(place) => {
            place.projection.as_slice().is_empty() && at.contains(&place.local)
        }
        Rvalue::Ref(_, place) => {
            matches!(place.projection.as_slice(), [ProjectionElem::Deref])
                && at.contains(&place.local)
        }
        _ => false,
    }
}

/// The static an operand is where it is, by the locals it is copied through.
fn static_of(body: &MirBody<'_>, operand: &Operand) -> Option<StaticId> {
    if let OperandKind::Static(id) = &operand.kind {
        return Some(*id);
    }
    let mut local = whole_local(operand)?;
    let mut seen = HashSet::default();
    loop {
        if !seen.insert(local) || body.param_locals.contains(&local) {
            return None;
        }
        let mut defs = body.basic_blocks.iter().flat_map(|(_, block)| {
            block.statements.iter().filter_map(move |it| match &it.kind {
                StatementKind::Assign(place, rvalue) if place.local == local => Some(rvalue),
                _ => None,
            })
        });
        let (Some(def), None) = (defs.next(), defs.next()) else { return None };
        match def {
            Rvalue::Use(Operand { kind: OperandKind::Static(id), .. }) => return Some(*id),
            Rvalue::Use(it) => local = whole_local(it)?,
            Rvalue::CopyForDeref(place) if place.projection.as_slice().is_empty() => {
                local = place.local
            }
            Rvalue::Ref(_, place)
                if matches!(place.projection.as_slice(), [ProjectionElem::Deref]) =>
            {
                local = place.local
            }
            _ => return None,
        }
    }
}

/// Whether `operand` is a function's address, as a function pointer or made into a pointer,
/// by the locals it is copied through: never one a body is handed, nor one a call returns.
fn fn_address(body: &MirBody<'_>, operand: &Operand, seen: &mut HashSet<LocalId>) -> bool {
    if operand.fn_item().is_some() {
        return true;
    }
    let Some(local) = whole_local(operand) else { return false };
    if !seen.insert(local) {
        return true;
    }
    if body.param_locals.contains(&local)
        || body.upvar_locals.values().flatten().any(|(it, _)| *it == local)
        || borrowed(body, local)
    {
        return false;
    }
    let mut any = false;
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            let StatementKind::Assign(place, rvalue) = &statement.kind else { continue };
            if place.local != local {
                continue;
            }
            any = true;
            if !place.projection.as_slice().is_empty() {
                return false;
            }
            let ok = match rvalue {
                Rvalue::Cast(
                    CastKind::PtrToPtr
                    | CastKind::FnPtrToPtr
                    | CastKind::PointerCoercion(PointerCast::ReifyFnPointer),
                    inner,
                    _,
                )
                | Rvalue::Use(inner) => fn_address(body, inner, seen),
                _ => false,
            };
            if !ok {
                return false;
            }
        }
        if let Some(terminator) = &block.terminator
            && let TerminatorKind::Call { destination, .. } = &terminator.kind
            && destination.local == local
        {
            return false;
        }
    }
    any
}

/// Whether the initializer `body`'s `local` is an `AtomicPtr` made of a function's address.
fn made_of_fn(
    db: &Db,
    body: &MirBody<'_>,
    local: LocalId,
    seen: &mut HashSet<LocalId>,
) -> bool {
    if !seen.insert(local) || borrowed(body, local) {
        return false;
    }
    let mut any = false;
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            let StatementKind::Assign(place, rvalue) = &statement.kind else { continue };
            if place.local != local {
                continue;
            }
            any = true;
            let from = match rvalue {
                Rvalue::Use(it) if place.projection.as_slice().is_empty() => whole_local(it),
                _ => None,
            };
            if !from.is_some_and(|it| made_of_fn(db, body, it, seen)) {
                return false;
            }
        }
        let Some(terminator) = &block.terminator else { continue };
        let TerminatorKind::Call { func, args, destination, .. } = &terminator.kind else {
            continue;
        };
        if destination.local != local {
            continue;
        }
        any = true;
        let made = destination.projection.as_slice().is_empty()
            && atomic_ptr_method(db, func) == Some("new")
            && args.first().is_some_and(|it| fn_address(body, it, &mut HashSet::default()));
        if !made {
            return false;
        }
    }
    any
}

/// Whether a reference to `local`, or to a part of it, is taken anywhere in `body`.
fn borrowed(body: &MirBody<'_>, local: LocalId) -> bool {
    body.basic_blocks.iter().any(|(_, block)| {
        block.statements.iter().any(|it| {
            matches!(&it.kind, StatementKind::Assign(_, Rvalue::Ref(_, place)) if place.local == local)
        })
    })
}

fn whole_local(operand: &Operand) -> Option<LocalId> {
    match &operand.kind {
        OperandKind::Copy(place) | OperandKind::Move(place)
            if place.projection.as_slice().is_empty() =>
        {
            Some(place.local)
        }
        _ => None,
    }
}

/// Which of core's `AtomicPtr::new`, `load` and `store` an operand is the function of.
fn atomic_ptr_method(db: &Db, func: &Operand) -> Option<&'static str> {
    let (func, _) = func.fn_item()?;
    if !matches!(
        func.krate(db).data(db).origin,
        base_db::CrateOrigin::Lang(base_db::LangCrateOrigin::Core)
    ) {
        return None;
    }
    let ItemContainerId::ImplId(imp) = func.lookup(db).container else { return None };
    if db.impl_trait(imp).is_some() || !is_atomic_ptr(db, db.impl_self_ty(imp).skip_binder()) {
        return None;
    }
    match FunctionSignature::of(db, func).name.as_str() {
        "new" => Some("new"),
        "load" => Some("load"),
        "store" => Some("store"),
        _ => None,
    }
}

/// Whether `ty` is core's `AtomicPtr`: a struct of its own, or `Atomic<*mut T>` it is an
/// alias of.
fn is_atomic_ptr(db: &Db, ty: hir_ty::next_solver::Ty<'_>) -> bool {
    let TyKind::Adt(def, args) = ty.kind() else { return false };
    let AdtId::StructId(id) = def.def_id() else { return false };
    if !matches!(
        id.krate(db).data(db).origin,
        base_db::CrateOrigin::Lang(base_db::LangCrateOrigin::Core)
    ) {
        return false;
    }
    match StructSignature::of(db, id).name.as_str() {
        "AtomicPtr" => true,
        "Atomic" => args
            .as_slice()
            .first()
            .and_then(|it| it.ty())
            .is_some_and(|it| matches!(it.kind(), TyKind::RawPtr(..))),
        _ => false,
    }
}
