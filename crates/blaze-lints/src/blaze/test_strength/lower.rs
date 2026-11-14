//! MIR to [`flow`]'s IR: places by path (references are seen through, indices are one
//! element), and every call by what the analysis knows of it ([`Effect`]).
//!
//! A call is, in this order: one that diverges ([`Effect::Fail`]); a method of a built-in
//! derive (`PartialEq` compares every field, `Clone` copies); a function of core, alloc or std
//! by its name ([`Lower::known`]: `unwrap` checks the variant and returns the payload, `is_ok`
//! and `len` are shadows of the value, `next` and `index` an element, `eq` a comparison, the
//! rest made from their arguments whole); a function of a workspace crate, summarized to a
//! bounded depth ([`super::summary`]) and, outside test code, a call of the code under test; or
//! a function of another crate, which may check what it is handed (a snapshot, a property
//! runner) and so observes all of it.

use base_db::Db;
use hir::AsAssocItem as _;
use hir_def::{
    BuiltinDeriveImplId, DefWithBodyId, FunctionId, HasModule as _, ItemContainerId, Lookup as _,
    VariantId, builtin_derive::BuiltinDeriveImplMethod, hir::ExprId,
    signatures::FunctionSignature,
};
use hir_ty::{
    InferBodyId, ParamEnvAndCrate,
    db::HirDatabase as _,
    mir::{
        AggregateKind, BasicBlockId, BinOp, MirBody, MirSpan, Operand, OperandKind, PlaceTy,
        ProjectionElem, Rvalue, Statement, StatementKind, StoredPlace, Terminator,
        TerminatorKind,
    },
    next_solver::{
        DbInterner, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt as _, InferCtxt},
    },
};
use rustc_type_ir::inherent::IntoKind as _;

use super::flow::{self, Block, Call, Effect, Place, Step, Stmt, Subject, Summary, Term};
use crate::blaze::{
    alloc::{self, Resolved},
    facts, test_code,
};

/// How many levels of workspace functions a test's calls are summarized through.
pub(crate) const ROOT_DEPTH: u32 = 3;

/// How many closure bodies of one test are followed; past it the test is not followed.
pub(crate) const CLOSURES: usize = 64;

/// How many references a type is peeled of to see what it is.
const REFS: usize = 4;

/// The crates whose calls make a test a property test: blaze's own engine (a plain `#[test]`
/// whose body calls `blaze_sim::check`, `check_given`, `search` or `control`), and the property
/// crates other people's projects use.
const PROPERTY_CRATES: &[&str] = &["blaze_sim", "hegel", "hegeltest", "proptest", "quickcheck"];

/// What a body is lowered as.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// A test or a helper of tests: its calls of the code under test are sources. With
    /// `residual_fails`, a `?` that returns its error fails it (a `#[test]` that returns a
    /// `Result`).
    Root { residual_fails: bool },
    /// A body summarized for its callers: its parameters are the sources.
    Summary,
}

/// A call of the code under test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Site {
    pub(crate) f: FunctionId,
    pub(crate) at: Option<ExprId>,
}

/// A shadow call: a check of one property of a value (`is_ok`, `len`, `contains`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ShadowSite {
    pub(crate) at: Option<ExprId>,
    pub(crate) name: String,
}

/// The lowering of the bodies of one definition.
pub(crate) struct Lower<'db> {
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    infcx: InferCtxt<'db>,
    mode: Mode,
    depth: u32,
    pub(crate) sites: Vec<Site>,
    pub(crate) shadows: Vec<ShadowSite>,
    /// Where each comparison is, by mark.
    pub(crate) marks: Vec<Option<ExprId>>,
    /// Whether the body calls a property-testing crate.
    pub(crate) property: bool,
    /// The workspace functions outside test code it calls or names.
    pub(crate) reached: Vec<FunctionId>,
    /// The summaries of the body being lowered, by callee.
    summaries: Vec<(FunctionId, Summary)>,
}

fn ix(block: &BasicBlockId) -> u32 {
    block.into_raw().into_u32()
}

/// The expression a MIR span is of.
pub(crate) fn expr_of(span: MirSpan) -> Option<ExprId> {
    match span {
        MirSpan::ExprId(it) | MirSpan::ExprEnd(it) => Some(it),
        MirSpan::PatId(_)
        | MirSpan::BindingId(_)
        | MirSpan::SelfParam
        | MirSpan::Unknown
        | MirSpan::Inlined(_) => None,
    }
}

/// Whether `krate` is core, alloc or std.
pub(crate) fn is_std(db: &Db, krate: hir::Crate) -> bool {
    krate.is_builtin(db) || ["core", "alloc", "std"].iter().any(|it| facts::crate_is(db, krate, it))
}

/// Whether `f` is a function of a trait's declaration (a call not resolved to an impl).
pub(crate) fn is_trait_decl(db: &Db, f: FunctionId) -> bool {
    matches!(f.lookup(db).container, ItemContainerId::TraitId(_))
}

/// The trait `f` implements a method of, if it is in an impl of one.
fn implemented(db: &Db, f: FunctionId) -> Option<hir::Trait> {
    hir::Function::from(f).as_assoc_item(db)?.implemented_trait(db)
}

/// Whether `f` is a method of an impl of a trait of core, alloc or std (`Clone`, `Display`).
pub(crate) fn std_trait_impl(db: &Db, f: FunctionId) -> bool {
    implemented(db, f).is_some_and(|it| is_std(db, it.module(db).krate(db)))
}

/// Whether `f` is `eq` or `ne` of a hand-written `PartialEq` impl.
fn is_partial_eq(db: &Db, f: FunctionId) -> bool {
    let name = FunctionSignature::of(db, f).name.as_str();
    matches!(name, "eq" | "ne")
        && implemented(db, f).is_some_and(|it| {
            it.name(db).as_str() == "PartialEq" && is_std(db, it.module(db).krate(db))
        })
}

fn peel(ty: Ty<'_>) -> Ty<'_> {
    let mut ty = ty;
    for _ in 0..REFS {
        match ty.kind() {
            TyKind::Ref(_, inner, _) => ty = inner,
            _ => break,
        }
    }
    ty
}

/// Whether `ty` is `&mut _`.
fn is_mut_ref(ty: Ty<'_>) -> bool {
    matches!(ty.kind(), TyKind::Ref(_, _, mutability) if mutability.is_mut())
}

/// Whether `ty`, its references peeled, is something a callee may call: a closure, a function,
/// a trait object.
fn callable(ty: Ty<'_>) -> bool {
    matches!(
        peel(ty).kind(),
        TyKind::Closure(..) | TyKind::FnPtr(..) | TyKind::FnDef(..) | TyKind::Dynamic(..)
    )
}

impl<'db> Lower<'db> {
    pub(crate) fn new(db: &'db Db, def: DefWithBodyId, mode: Mode, depth: u32) -> Lower<'db> {
        let krate = def.krate(db);
        Lower {
            db,
            env: ParamEnvAndCrate { param_env: db.trait_environment(def.generic_def(db)), krate },
            infcx: DbInterner::new_with(db, krate)
                .with_regions()
                .infer_ctxt()
                .build(TypingMode::non_body_analysis()),
            mode,
            depth,
            sites: Vec::new(),   
            shadows: Vec::new(), 
            marks: Vec::new(),   
            property: false,
            reached: Vec::new(),   
            summaries: Vec::new(), 
        }
    }

    fn is_root(&self) -> bool {
        matches!(self.mode, Mode::Root { .. })
    }

    /// Lowers what comes next as a closure of a test: a `?` in it returns from the closure,
    /// not from the test, so it does not fail the test.
    pub(crate) fn enter_closure(&mut self) {
        if let Mode::Root { .. } = self.mode {
            self.mode = Mode::Root { residual_fails: false };
        }
    }

    /// The IR of `mir`.
    pub(crate) fn body(&mut self, mir: &MirBody<'db>, ret_escapes: bool, seed_params: bool) -> flow::Body {
        self.summaries.clear();
        let n = mir.basic_blocks.len();
        
        let mut blocks = vec![Block { stmts: Vec::new(), term: Term::Stop }; n];
        for (id, block) in mir.basic_blocks.iter() {
            let at = id.into_raw().into_u32() as usize;
            if at >= n || block.is_cleanup {
                continue;
            }
            let mut stmts = Vec::new(); 
            for statement in &block.statements {
                self.stmt(statement, &mut stmts);
            }
            let term = match &block.terminator {
                Some(it) => self.term(mir, it, &mut stmts),
                None => Term::Stop,
            };
            blocks[at] = Block { stmts, term };
        }
        let summaries = std::mem::take(&mut self.summaries);
        flow::Body {
            locals: mir.locals.len() as u32,
            blocks,
            params: mir.param_locals.iter().map(|&it| it.into_raw().into_u32()).collect(),
            seed_params,
            ret: 0,
            ret_escapes,
            summaries: summaries.into_iter().map(|(_, it)| it).collect(),
        }
    }

    /// The indices of the parameters of `mir` that are `&mut`.
    pub(crate) fn mut_params(&self, mir: &MirBody<'db>) -> Vec<u32> {
        mir.param_locals
            .iter()
            .enumerate()
            .filter(|(_, it)| is_mut_ref(mir.locals[**it].ty.as_ref()))
            .map(|(at, _)| at as u32)
            .collect() 
    }

    fn place(&self, place: &StoredPlace) -> Place {
        let mut path = Vec::new(); 
        for elem in place.projection.as_slice() {
            match elem {
                ProjectionElem::Deref | ProjectionElem::Subslice { .. } => {}
                ProjectionElem::Field(it) => path.push(Step::Field(it.0)),
                ProjectionElem::Index(_) | ProjectionElem::ConstantIndex { .. } => {
                    path.push(Step::Elem)
                }
                ProjectionElem::Downcast(it) => path.push(Step::Variant(self.variant(*it))),
                // A field a `for const` loop names: not one the path can say; read from here.
                ProjectionElem::ReflectedField(_) => break,
            }
        }
        Place { local: place.local.into_raw().into_u32(), path }
    }

    fn variant(&self, variant: VariantId) -> u32 {
        match variant {
            VariantId::EnumVariantId(it) => it.index(self.db) as u32,
            VariantId::StructId(_) | VariantId::UnionId(_) => 0,
        }
    }

    /// Notes a workspace function outside test code as reached.
    fn note(&mut self, f: FunctionId) {
        let db = self.db;
        let local = hir::Function::from(f).module(db).krate(db).origin(db).is_local();
        if local && !self.reached.contains(&f) && !test_code::is_test_def(db, DefWithBodyId::FunctionId(f)) {
            self.reached.push(f);
        }
    }

    fn operand(&mut self, operand: &Operand) -> Option<Place> {
        if let Some((f, _)) = operand.fn_item() {
            self.note(f);
        }
        match &operand.kind {
            OperandKind::Copy(it) | OperandKind::Move(it) => Some(self.place(it)),
            OperandKind::Constant { .. } | OperandKind::Allocation { .. } | OperandKind::Static(_) => {
                None
            }
        }
    }

    fn operand_ty(&self, mir: &MirBody<'db>, operand: &Operand) -> Option<Ty<'db>> {
        match &operand.kind {
            OperandKind::Copy(it) | OperandKind::Move(it) => Some(
                PlaceTy::from_ty(mir.locals[it.local].ty.as_ref())
                    .multi_projection_ty(&self.infcx, self.env.param_env, it.projection.as_slice())
                    .ty,
            ),
            OperandKind::Constant { ty, .. } => Some(ty.as_ref()),
            OperandKind::Allocation { .. } | OperandKind::Static(_) => None,
        }
    }

    /// The indices of the arguments that are `&mut`.
    fn mut_args(&self, mir: &MirBody<'db>, args: &[Operand]) -> Vec<u32> {
        args.iter()
            .enumerate()
            .filter(|(_, it)| self.operand_ty(mir, it).is_some_and(is_mut_ref))
            .map(|(at, _)| at as u32)
            .collect() 
    }

    fn any_callable(&self, mir: &MirBody<'db>, args: &[Operand]) -> bool {
        args.iter().any(|it| self.operand_ty(mir, it).is_some_and(callable))
    }

    fn mark_at(&mut self, at: Option<ExprId>) -> u32 {
        self.marks.push(at);
        (self.marks.len() - 1) as u32
    }

    fn shadow(&mut self, step: Step, name: &str, at: Option<ExprId>) -> Effect {
        self.shadows.push(ShadowSite { at, name: name.to_owned() }); 
        Effect::Shadow { step, site: (self.shadows.len() - 1) as u32 }
    }

    fn stmt(&mut self, statement: &Statement, out: &mut Vec<Stmt>) {
        // Fake reads, storage markers, `Deinit` and inline assembly carry no value the analysis
        // follows.
        let StatementKind::Assign(to, value) = &statement.kind else { return };
        let to = self.place(to);
        let made = match value {
            Rvalue::Use(it) | Rvalue::Cast(_, it, _) => {
                self.operand(it).map(|from| Stmt::Copy { to, from })
            }
            Rvalue::Repeat(it, _) => self
                .operand(it)
                .map(|from| Stmt::Aggregate { to, fields: vec![(vec![Step::Elem], from)] }), 
            Rvalue::Ref(_, it) => Some(Stmt::Borrow { to, of: self.place(it) }),
            Rvalue::CopyForDeref(it) => Some(Stmt::Copy { to, from: self.place(it) }),
            Rvalue::Len(it) => Some(Stmt::Read { to, from: self.place(it), step: Step::Len }),
            Rvalue::Discriminant(it) => {
                Some(Stmt::Read { to, from: self.place(it), step: Step::Discr })
            }
            Rvalue::BinaryOp(op, left, right) | Rvalue::CheckedBinaryOp(op, left, right) => {
                let compares = matches!(op, BinOp::Eq | BinOp::Ne) && self.is_root();
                let mark = compares.then(|| self.mark_at(expr_of(statement.span)));
                let from = [left, right].into_iter().filter_map(|it| self.operand(it)).collect();
                Some(Stmt::Derive { to, from, mark })
            }
            Rvalue::UnaryOp(_, it) => {
                Some(Stmt::Derive { to, from: self.operand(it).into_iter().collect(), mark: None })
            }
            Rvalue::Aggregate(kind, operands) => {
                let mut fields = Vec::new(); 
                for (at, it) in operands.iter().enumerate() {
                    if let Some(from) = self.operand(it) {
                        fields.push((self.prefix(kind, at as u32), from));
                    }
                }
                Some(Stmt::Aggregate { to, fields })
            }
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => {
                match *never {}
            }
        };
        out.extend(made);
    }

    /// Where the operand `at` of an aggregate of `kind` goes.
    fn prefix(&self, kind: &AggregateKind, at: u32) -> Vec<Step> {
        match kind {
            AggregateKind::Adt(VariantId::EnumVariantId(it), _) => {
                vec![Step::Variant(it.index(self.db) as u32), Step::Field(at)] 
            }
            AggregateKind::Adt(VariantId::StructId(_) | VariantId::UnionId(_), _)
            | AggregateKind::Tuple(_)
            | AggregateKind::Closure(_) => vec![Step::Field(at)], 
            AggregateKind::Array(_) => vec![Step::Elem], 
            AggregateKind::Union(_, field) => vec![Step::Field(field.local_id.into_raw().into_u32())], 
        }
    }

    fn term(&mut self, mir: &MirBody<'db>, terminator: &Terminator, out: &mut Vec<Stmt>) -> Term {
        match &terminator.kind {
            TerminatorKind::Goto { target } => Term::Goto(vec![ix(target)]), 
            TerminatorKind::SwitchInt { discr, targets } => Term::Switch {
                on: self.operand(discr),
                targets: targets.all_targets().iter().map(ix).collect(),
            },
            // An arm the match can't take, the end of an unwind: no path the test takes.
            TerminatorKind::UnwindResume
            | TerminatorKind::CoroutineDrop
            | TerminatorKind::Unreachable => Term::Stop,
            TerminatorKind::Abort => Term::Fail,
            TerminatorKind::Return => Term::Return,
            TerminatorKind::Drop { target, .. } => Term::Goto(vec![ix(target)]), 
            TerminatorKind::DropAndReplace { place, value, target, .. } => {
                if let Some(from) = self.operand(value) {
                    out.push(Stmt::Copy { to: self.place(place), from });
                }
                Term::Goto(vec![ix(target)]) 
            }
            TerminatorKind::Call { func, args, destination, target, .. } => {
                self.call(mir, func, args, destination, *target, terminator.span)
            }
            // What a tail call is handed goes where the body does not follow it.
            TerminatorKind::TailCall { func, args, .. } => {
                self.operand(func);
                for it in args.iter() {
                    if let Some(from) = self.operand(it) {
                        out.push(Stmt::Escape { from });
                    }
                }
                Term::Return
            }
            TerminatorKind::Assert { cond, target, .. } => {
                Term::Assert { cond: self.operand(cond), target: ix(target) }
            }
            TerminatorKind::Yield { value, resume, .. } => {
                if let Some(from) = self.operand(value) {
                    out.push(Stmt::Escape { from });
                }
                Term::Goto(vec![ix(resume)]) 
            }
            TerminatorKind::FalseEdge { real_target, imaginary_target } => {
                Term::Goto(vec![ix(real_target), ix(imaginary_target)]) 
            }
            TerminatorKind::FalseUnwind { real_target, .. } => Term::Goto(vec![ix(real_target)]), 
            TerminatorKind::ComptimeFor { body, after, .. } => {
                Term::Goto(vec![ix(body), ix(after)]) 
            }
            TerminatorKind::ComptimeNext { head } => Term::Goto(vec![ix(head)]), 
        }
    }

    fn call(
        &mut self,
        mir: &MirBody<'db>,
        func: &Operand,
        args: &[Operand],
        destination: &StoredPlace,
        target: Option<BasicBlockId>,
        span: MirSpan,
    ) -> Term {
        let places: Vec<Option<Place>> = args.iter().map(|it| self.operand(it)).collect(); 
        let dest = self.place(destination);
        let at = expr_of(span);
        let mut subject = None;
        let effect = match func.fn_item() {
            // A closure, function pointer or trait object held in a local.
            None => {
                self.operand(func);
                Effect::Opaque { observes: true, writes: self.mut_args(mir, args) }
            }
            Some((f, generic)) => {
                self.note(f);
                match alloc::resolve(self.db, self.env, f, generic) {
                    Resolved::Derived(impl_, method, _) => self.derived(impl_, method, at),
                    Resolved::Function(g, _) => {
                        let (effect, called) = self.function(mir, g, args, at);
                        subject = called;
                        effect
                    }
                }
            }
        };
        let effect = match target {
            Some(_) => effect,
            None => Effect::Fail,
        };
        Term::Call(Call { effect, args: places, dest, target: target.as_ref().map(ix), subject })
    }

    fn derived(&mut self, impl_: BuiltinDeriveImplId, method: BuiltinDeriveImplMethod, at: Option<ExprId>) -> Effect {
        let Some(f) = method.trait_method(self.db, impl_) else {
            return Effect::Opaque { observes: false, writes: Vec::new() }; 
        };
        match FunctionSignature::of(self.db, f).name.as_str() {
            "eq" | "ne" if self.is_root() => Effect::Compare { mark: self.mark_at(at) },
            "clone" => Effect::Identity,
            // `Debug`, `Hash`, `PartialOrd`, `Ord`, `Default`: made from every field.
            _ => Effect::Opaque { observes: false, writes: Vec::new() }, 
        }
    }

    fn function(
        &mut self,
        mir: &MirBody<'db>,
        g: FunctionId,
        args: &[Operand],
        at: Option<ExprId>,
    ) -> (Effect, Option<Subject>) {
        let db = self.db;
        let krate = hir::Function::from(g).module(db).krate(db);
        // Before the locality test: a property crate may be a workspace member (`blaze-sim` is
        // one of blaze's, and a fixture's crates are all local). Its functions are the runner,
        // not code under test: they observe what they are handed. A test is a property test by
        // this call alone, never by its attribute.
        if PROPERTY_CRATES.iter().any(|it| facts::crate_is(db, krate, it)) {
            self.property = true;
            return (Effect::Opaque { observes: true, writes: self.mut_args(mir, args) }, None);
        }
        if is_std(db, krate) {
            return (self.known(mir, g, args, at), None);
        }
        if !krate.origin(db).is_local() {
            // A dependency's function may check what it is handed (a snapshot, a
            // `pretty_assertions` comparison, a property runner): it observes all of it.
            return (Effect::Opaque { observes: true, writes: self.mut_args(mir, args) }, None);
        }
        self.note(g);
        if is_trait_decl(db, g) {
            // Not resolved to an impl: any impl may run.
            return (Effect::Opaque { observes: true, writes: self.mut_args(mir, args) }, None);
        }
        let test = test_code::is_test_def(db, DefWithBodyId::FunctionId(g));
        let mark = (!test && self.is_root() && is_partial_eq(db, g)).then(|| self.mark_at(at));
        let effect = if self.depth == 0 {
            Effect::Opaque { observes: true, writes: self.mut_args(mir, args) }
        } else {
            let index = match self.summaries.iter().position(|(it, _)| *it == g) {
                Some(it) => it,
                None => {
                    // clone: one callee's summary, into the body that calls it
                    let summary = super::summary(db, g, self.depth - 1).clone();
                    self.summaries.push((g, summary));
                    self.summaries.len() - 1
                }
            };
            Effect::Summary { index: index as u32, mark }
        };
        let subject = (self.is_root() && !test && !std_trait_impl(db, g)).then(|| {
            self.sites.push(Site { f: g, at });
            Subject { site: (self.sites.len() - 1) as u32, mut_args: self.mut_args(mir, args) }
        });
        (effect, subject)
    }

    /// A function of core, alloc or std, by its name and its impl's type.
    fn known(&mut self, mir: &MirBody<'db>, g: FunctionId, args: &[Operand], at: Option<ExprId>) -> Effect {
        let db = self.db;
        let name = FunctionSignature::of(db, g).name.as_str();
        let adt = facts::impl_adt(db, g);
        let is = |it: &str| adt.is_some_and(|adt| adt.name(db).as_str() == it);
        let (option, result) = (is("Option"), is("Result"));
        let payload = if option { 1 } else { 0 };
        match name {
            "clone" | "to_owned" | "to_vec" | "as_ref" | "as_mut" | "as_deref" | "as_deref_mut"
            | "deref" | "deref_mut" | "borrow" | "borrow_mut" | "as_slice" | "as_mut_slice"
            | "as_str" | "iter" | "iter_mut" | "into_iter" | "by_ref" | "copied" | "cloned" => {
                Effect::Identity
            }
            "unwrap" | "expect" if option || result => Effect::Unwrap { variant: payload, checks: true },
            "unwrap_err" | "expect_err" if result => Effect::Unwrap { variant: 1, checks: true },
            "unwrap_or" | "unwrap_or_default" | "unwrap_or_else" | "unwrap_unchecked"
                if option || result =>
            {
                Effect::Unwrap { variant: payload, checks: false }
            }
            "is_some" | "is_none" if option => self.shadow(Step::Discr, name, at),
            "is_ok" | "is_err" if result => self.shadow(Step::Discr, name, at),
            "len" | "is_empty" => self.shadow(Step::Len, name, at),
            "contains" | "contains_key" | "starts_with" | "ends_with" => {
                self.shadow(Step::Probe, name, at)
            }
            "next" | "next_back" | "nth" | "get" | "get_mut" | "first" | "first_mut" | "last"
            | "last_mut" => Effect::Element { wrap: Some(1) },
            "index" | "index_mut" => Effect::Element { wrap: None },
            "branch" if option => Effect::Branch { ok: 1, err_field: false },
            "branch" if result => Effect::Branch { ok: 0, err_field: true },
            "eq" | "ne" if self.is_root() => Effect::Compare { mark: self.mark_at(at) },
            "from_residual" => match self.mode {
                Mode::Root { residual_fails: true } => Effect::Fail,
                Mode::Root { residual_fails: false } | Mode::Summary => {
                    Effect::Opaque { observes: false, writes: Vec::new() } 
                }
            },
            "drop" | "forget" => Effect::Ignore,
            _ => Effect::Opaque { observes: self.any_callable(mir, args), writes: self.mut_args(mir, args) },
        }
    }
}

/// What a function's body calls and names: how `unreached_by_tests` walks the call graph.
/// A trait method no impl was resolved for is kept as the trait's: every impl method of a
/// workspace trait impl is a root of the walk anyway (a test that hands a value to `format!`
/// or a `dyn` runs its `Display` without naming it), so a dispatched call loses nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Callees {
    /// The functions it calls or names, resolved to their impls where the types say which.
    pub(crate) fns: Vec<FunctionId>,
    /// The MIR of it or of one of its closures could not be built: what it calls is not known.
    pub(crate) unread: bool,
}

/// Notes the function `operand` names, resolved where the caller's types say which impl.
fn callee(db: &Db, env: ParamEnvAndCrate<'_>, operand: &Operand, out: &mut Callees) {
    let Some((g, generic)) = operand.fn_item() else { return };
    let h = match alloc::resolve(db, env, g, generic) {
        Resolved::Function(h, _) => h,
        // A built-in derive's method has no body: nothing further to reach.
        Resolved::Derived(..) => return,
    };
    if !out.fns.contains(&h) {
        out.fns.push(h);
    }
}

/// [`Callees`] of the body `def` (a function's, a const's, a static's), from its MIR and its
/// closures'.
pub(crate) fn callees_of(db: &Db, def: DefWithBodyId) -> Callees {
    let mut out = Callees::default();
    let Ok(mir) = db.mir_body(InferBodyId::DefWithBodyId(def)) else {
        out.unread = true;
        return out;
    };
    let env = ParamEnvAndCrate {
        param_env: db.trait_environment(def.generic_def(db)),
        krate: def.krate(db),
    };
    
    let mut bodies: Vec<&MirBody<'_>> = vec![mir];
    let mut next = 0;
    while next < bodies.len() {
        if bodies.len() >= CLOSURES {
            out.unread = true;
            break;
        }
        for &closure in &bodies[next].closures {
            match db.mir_body_for_closure(closure) {
                Ok(it) => bodies.push(it),
                Err(_) => out.unread = true,
            }
        }
        next += 1;
    }
    for mir in bodies {
        for (_, block) in mir.basic_blocks.iter() {
            for statement in &block.statements {
                if let StatementKind::Assign(_, value) = &statement.kind {
                    value.for_each_operand(&mut |it| callee(db, env, it, &mut out));
                }
            }
            if let Some(terminator) = &block.terminator {
                terminator.kind.for_each_operand(&mut |it| callee(db, env, it, &mut out));
            }
        }
    }
    out
}
