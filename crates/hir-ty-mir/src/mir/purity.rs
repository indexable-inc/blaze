//! Whether a function is const-pure (#860): what a call of it gives depends on its arguments
//! alone, as a `const fn`'s value in const eval does, and if not, why and where.
//!
//! A `const fn` gets most of the way by being one: it calls only `const fn`s, takes no address
//! as a number and runs no assembly, which the const checker holds it to. What it can still do
//! at run time is read the state of a static that changes (a `static mut`, one of a type that
//! is not `Freeze`, a thread-local or a foreign static) or go through a raw pointer, and so can
//! the `const fn`s it calls; that is what this looks for. A `const fn` of the toolchain is taken
//! as pure, as fn-cache takes core and alloc. What an argument with interior mutability holds
//! is not looked at here: what keeps a result by its arguments refuses those itself.

use base_db::Db;
use either::Either;
use hir_def::{
    CallableDefId, FunctionId, HasModule, ItemContainerId, Lookup, StaticId,
    attrs::AttrFlags,
    signatures::{FunctionSignature, StaticFlags, StaticSignature},
};
use rustc_type_ir::inherent::IntoKind;

use hir_ty::{
    InferBodyId,
    db::HirDatabase,
    next_solver::{
        DbInterner, GenericArgs, ParamEnv, Ty, TyKind, TypingMode, infer::DbInternerInferExt,
    },
    traits::ParamEnvAndCrate,
};

use super::{
    CastKind, MirBody, MirSpan, Operand, OperandKind, ProjectionElem, Rvalue, StatementKind,
    StoredPlace, TerminatorKind, const_check::of_a_trait,
};

/// Why a function is not const-pure.
#[derive(Debug, PartialEq, Eq, blaze_query::Value)]
pub struct Impure {
    /// What the function that does it does, said of it: "reads or writes mutable state:
    /// `CALLS` is `AtomicU64`, which is not `Freeze`".
    pub what: String,
    /// The functions called on the way from this one to the one that does it, by name.
    pub through: Vec<String>,
    /// Where in this function's body: what does it, or the call on the way to it.
    pub span: MirSpan,
    /// It is a fact of the code to tell its writer, and not only what blaze can't tell is
    /// pure: a generic call, a body it can't read.
    pub told: bool,
}

impl Impure {
    /// Said of the function: "reaches `a` → `b`, which reads the foreign static `environ`".
    pub fn said(&self) -> String {
        match self.through.as_slice() {
            // alloc: the text said of the function, made when it is reported
            [] => self.what.clone(),
            names => {
                let shown = match names.len() {
                    // alloc: the text said of the function, made when it is reported
                    ..=3 => names.join(" → "),
                    // alloc: the text said of the function, made when it is reported
                    n => format!("{} → … → {}", names[0], names[n - 1]),
                };
                // alloc: the text said of the function, made when it is reported
                format!("reaches {shown}, which {}", self.what)
            }
        }
    }
}

/// Whether `func` is a `const fn`, not one of a trait, that is const-pure: what fn-cache needs
/// not look past.
pub fn is_const_pure(db: &Db, func: FunctionId) -> bool {
    FunctionSignature::of(db, func).is_const()
        && !of_a_trait(db, func)
        && const_purity(db, func).is_none()
}

/// Why `func`, a function const code may call (a `const fn`, or a method of a trait or of an
/// `impl` of one), is not const-pure, if it is not.
#[blaze_query::tracked(returns(ref), cycle_result = const_purity_cycle_result)]
pub fn const_purity<'db>(db: &'db Db, func: FunctionId) -> Option<Impure> {
    let unknown =
        // alloc: the impurity the query keeps, only when there is one
        |what: String, span: MirSpan| Impure { what, through: Vec::new(), span, told: false };
    let signature = FunctionSignature::of(db, func);
    let krate = func.krate(db);
    if FunctionSignature::is_intrinsic(db, func)
        || (signature.is_const() && krate.data(db).origin.is_lang())
    {
        return None;
    }
    if !signature.is_const() && !of_a_trait(db, func) {
        // alloc: error path
        return Some(unknown("is not a `const fn`".to_owned(), MirSpan::Unknown));
    }
    let Ok(body) = db.mir_body(InferBodyId::DefWithBodyId(func.into())) else {
        // alloc: error path
        return Some(unknown("has MIR blaze can't read".to_owned(), MirSpan::Unknown));
    };
    let own = krate.data(db).origin.is_local();
    // What is told wins over what is only not known, which a later block may come after.
    let mut first_unknown = None;
    let mut infcx = None;
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            let found = match &statement.kind {
                // alloc: the impurity the query keeps
                StatementKind::InlineAsm(_) => Some(("has assembly in it".to_owned(), false)),
                StatementKind::Assign(place, rvalue) => {
                    assigned(db, krate, body, own, place, rvalue)
                }
                _ => None,
            };
            match found {
                Some((what, true)) => {
                    return Some(Impure {
                        what,
                        // alloc: empty, no allocation
                        through: Vec::new(),
                        span: statement.span,
                        told: true,
                    });
                }
                Some((what, false)) => {
                    first_unknown.get_or_insert_with(|| unknown(what, statement.span));
                }
                None => {}
            }
        }
        let Some(terminator) = &block.terminator else { continue };
        let (read, callee): (&[Operand], _) = match &terminator.kind {
            TerminatorKind::Call { func, args, .. }
            | TerminatorKind::TailCall { func, args, .. } => (&args[..], Some(func)),
            TerminatorKind::SwitchInt { discr, .. } => (std::slice::from_ref(discr), None),
            TerminatorKind::Drop { place, .. } | TerminatorKind::DropAndReplace { place, .. } => {
                // What a `Drop` impl does is not looked into.
                let glue = !place.projection.as_slice().is_empty() || {
                    let infcx = infcx.get_or_insert_with(|| {
                        DbInterner::new_with(db, krate)
                            .with_regions()
                            .infer_ctxt()
                            .build(TypingMode::non_body_analysis())
                    });
                    let ty = body.locals[place.local].ty.as_ref();
                    hir_ty::drop::has_drop_glue(infcx, ty, ParamEnv::empty())
                        != hir_ty::drop::DropGlue::None
                };
                if glue {
                    first_unknown.get_or_insert_with(|| {
                        // alloc: error path
                        unknown("drops a value with drop glue".to_owned(), terminator.span)
                    });
                }
                (&[], None)
            }
            _ => (&[], None),
        };
        for operand in read {
            if let Some(what) = state_read(db, krate, operand) {
                return Some(Impure {
                    what,
                    // alloc: empty, no allocation
                    through: Vec::new(),
                    span: terminator.span,
                    told: true,
                });
            }
        }
        let Some(callee) = callee else { continue };
        if let Some(mut impure) = called(db, krate, callee) {
            impure.span = terminator.span;
            if impure.told {
                return Some(impure);
            }
            first_unknown.get_or_insert(impure);
        }
    }
    first_unknown
}

/// Every function of a cycle of calls gets what this gives, so it can't say pure for one that
/// is not.
fn const_purity_cycle_result(
    _db: &Db,
    _: blaze_query::db::Id,
    _func: FunctionId,
) -> Option<Impure> {
    Some(Impure {
        // alloc: the impurity the query keeps
        what: "calls itself back".to_owned(),
        // alloc: empty, no allocation
        through: Vec::new(),
        span: MirSpan::Unknown,
        told: false,
    })
}

/// What an assignment does that is not pure, and whether that is told: a static's state read,
/// or in code of the workspace a raw pointer gone through or an address taken as a number.
fn assigned(
    db: &Db,
    krate: base_db::Crate,
    body: &MirBody<'_>,
    own: bool,
    place: &StoredPlace,
    rvalue: &Rvalue,
) -> Option<(String, bool)> {
    for operand in operands(rvalue) {
        if let Some(what) = state_read(db, krate, operand) {
            return Some((what, true));
        }
    }
    if !own {
        return None;
    }
    if let Rvalue::Cast(CastKind::PointerExposeAddress, ..) = rvalue {
        // alloc: the impurity the query keeps
        return Some(("exposes a pointer address".to_owned(), true));
    }
    // alloc: the impurity the query keeps, only when there is one
    raw_use_in(body, place, rvalue).map(|what| (what.to_owned(), false))
}

/// What a function called by `callee` does that is not pure, said with the way to it.
fn called(db: &Db, krate: base_db::Crate, callee: &Operand) -> Option<Impure> {
    let unknown =
        // alloc: the impurity the query keeps, only when there is one
        |what: String| Impure { what, through: Vec::new(), span: MirSpan::Unknown, told: false };
    let ty: Ty<'_> = match &callee.kind {
        OperandKind::Constant { ty, .. } => ty.as_ref(),
        OperandKind::Allocation { allocation } => allocation.as_ref().ty,
        // alloc: error path
        _ => return Some(unknown("calls a function pointer or a closure".to_owned())),
    };
    let TyKind::FnDef(def, args) = ty.kind() else {
        // alloc: error path
        return Some(unknown("calls a function pointer or a closure".to_owned()));
    };
    let CallableDefId::FunctionId(func) = def.0 else { return None };
    let args = args.skip_binder();
    if FunctionSignature::is_intrinsic(db, func) || diverges(db, func) {
        // A panic: what it prints is no result.
        return None;
    }
    let func = match func.lookup(db).container {
        ItemContainerId::TraitId(_) => match resolved(db, krate, func, args) {
            Some(it) => it,
            None => {
                // alloc: error path
                let name = FunctionSignature::of(db, func).name.as_str().to_owned();
                // alloc: error path
                return Some(unknown(format!("calls `{name}` of a trait, for a type not known")));
            }
        },
        _ => func,
    };
    let inner = const_purity(db, func).as_ref()?;
    // alloc: the impurity the query keeps, with the way to it
    let mut through = Vec::with_capacity(1 + inner.through.len());
    // alloc: the impurity the query keeps, with the way to it
    through.push(format!("`{}`", FunctionSignature::of(db, func).name.as_str()));
    // clone: the callee's way, kept in this function's result
    through.extend(inner.through.iter().cloned());
    // clone: the callee's text, kept in this function's result
    Some(Impure { what: inner.what.clone(), through, span: MirSpan::Unknown, told: inner.told })
}

/// The body a call of a trait's method `func` runs, when its arguments tell it.
fn resolved<'db>(
    db: &'db Db,
    krate: base_db::Crate,
    func: FunctionId,
    args: GenericArgs<'db>,
) -> Option<FunctionId> {
    if rustc_type_ir::TypeVisitableExt::has_param(&args) {
        return None;
    }
    let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate };
    match db.lookup_impl_method(env, func, args) {
        (Either::Left(it), _) if FunctionSignature::of(db, it).has_body() => Some(it),
        _ => None,
    }
}

fn diverges(db: &Db, func: FunctionId) -> bool {
    let output = db.callable_item_signature(func.into()).skip_binder().skip_binder().output();
    matches!(output.kind(), TyKind::Never)
}

/// What reading `operand` reads of a static that changes: "reads or writes mutable state:
/// `X` is a `static mut`".
fn state_read(db: &Db, krate: base_db::Crate, operand: &Operand) -> Option<String> {
    let OperandKind::Static(id) = &operand.kind else { return None };
    static_state(db, krate, *id)
}

fn static_state(db: &Db, krate: base_db::Crate, id: StaticId) -> Option<String> {
    let signature = StaticSignature::of(db, id);
    if signature.flags.contains(StaticFlags::EXTERN) {
        let name =
            // clone: an interned symbol
            AttrFlags::link_name(db, id.into()).unwrap_or_else(|| signature.name.symbol().clone());
        let name = name.as_str();
        return (!name.starts_with("__rust_"))
            // alloc: the impurity the query keeps
            .then(|| format!("reads the foreign static `{name}`"));
    }
    let name = signature.name.as_str();
    if AttrFlags::query(db, id.into()).contains(AttrFlags::THREAD_LOCAL) {
        // alloc: the impurity the query keeps
        return Some(format!("uses the thread-local `{name}`"));
    }
    if signature.flags.contains(StaticFlags::MUTABLE) {
        // alloc: the impurity the query keeps
        return Some(format!("reads or writes mutable state: `{name}` is a `static mut`"));
    }
    let ty = db.type_for_static(id).skip_binder();
    let interner = DbInterner::new_with(db, krate).with_regions();
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    if hir_ty::drop::is_freeze(&infcx, ty, ParamEnv::empty()) {
        return None;
    }
    let shown = super::type_name::rustc_type_name(db, ty)
        .map(|it| {
            let (bare, rest) = it.split_at(it.find('<').unwrap_or(it.len()));
            // alloc: the impurity the query keeps
            format!("{}{rest}", bare.rsplit("::").next().unwrap_or(bare))
        })
        // alloc: the impurity the query keeps
        .unwrap_or_else(|| "_".to_owned());
    // alloc: the impurity the query keeps
    Some(format!("reads or writes mutable state: `{name}` is `{shown}`, which is not `Freeze`"))
}

/// The operands an rvalue reads, in order.
pub fn operands(rvalue: &Rvalue) -> impl Iterator<Item = &Operand> {
    let (one, two, many): (Option<&Operand>, Option<&Operand>, &[Operand]) = match rvalue {
        Rvalue::Use(it)
        | Rvalue::Repeat(it, _)
        | Rvalue::Cast(_, it, _)
        | Rvalue::UnaryOp(_, it) => (Some(it), None, &[]),
        Rvalue::BinaryOp(_, a, b) | Rvalue::CheckedBinaryOp(_, a, b) => (Some(a), Some(b), &[]),
        Rvalue::Aggregate(_, it) => (None, None, &it[..]),
        _ => (None, None, &[]),
    };
    one.into_iter().chain(two).chain(many)
}

/// Whether an assignment goes through a raw pointer, which makes what it gives depend on more
/// than its arguments: "dereferences a raw pointer".
fn raw_use_in(body: &MirBody<'_>, place: &StoredPlace, rvalue: &Rvalue) -> Option<&'static str> {
    let raw = |place: &StoredPlace| {
        matches!(place.projection.as_slice().first(), Some(ProjectionElem::Deref))
            && matches!(body.locals[place.local].ty.as_ref().kind(), TyKind::RawPtr(..))
    };
    let operand_raw = |operand: &Operand| match &operand.kind {
        OperandKind::Copy(place) | OperandKind::Move(place) => raw(place),
        _ => false,
    };
    let read = match rvalue {
        Rvalue::Ref(_, it)
        | Rvalue::Len(it)
        | Rvalue::Discriminant(it)
        | Rvalue::CopyForDeref(it) => raw(it),
        _ => operands(rvalue).any(operand_raw),
    };
    (raw(place) || read).then_some("dereferences a raw pointer")
}

/// The first use of a raw pointer in `body` that makes what it gives depend on more than its
/// arguments: a dereference, an address taken as a number, or assembly.
pub fn raw_use(body: &MirBody<'_>) -> Option<(&'static str, MirSpan)> {
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            let StatementKind::Assign(place, rvalue) = &statement.kind else {
                if let StatementKind::InlineAsm(_) = &statement.kind {
                    return Some(("has assembly in it", statement.span));
                }
                continue;
            };
            if let Rvalue::Cast(CastKind::PointerExposeAddress, ..) = rvalue {
                return Some(("takes an address as a number", statement.span));
            }
            if let Some(what) = raw_use_in(body, place, rvalue) {
                return Some((what, statement.span));
            }
        }
    }
    None
}
