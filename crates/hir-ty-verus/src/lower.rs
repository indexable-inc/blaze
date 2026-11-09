//! blaze's HIR, typed by blaze's inference, lowered into Verus's VIR: the port of
//! `rust_verify`'s `rust_to_vir` (`rust_to_vir_func.rs`'s `check_item_fn` and
//! `rust_to_vir_expr.rs`'s `expr_to_vir`), rule by rule, for the slice
//! docs/v2/plan/design/verus.md lists: a free, non-generic function over integers and booleans,
//! with `requires`, `ensures` (and a named return), `decreases`, `let`, assignment, `if`,
//! `loop` and `while` with `invariant`, `ensures` and `decreases`, `break`, `continue` and
//! `return`. Anything else is an [`Unsupported`] naming the construct and where it is, never a
//! guess.
//!
//! Each clause is a ghost group at the head of a block (`hir_def::expr_store::lower::spec`);
//! it becomes a VIR header statement (`ExprX::Header`), which `vir::headers::read_header`
//! takes off the block, as `rust_verify` does with `builtin::requires(..)` calls. A clause's
//! expressions are spec code: arithmetic on them is over mathematical integers (`int`) and `==`
//! is spec equality, as `builtin_macros` makes them under Verus. Body code is exec: arithmetic
//! is checked for overflow in the type's range and division for a zero divisor.

use std::sync::Arc;

use base_db::Db;
use hir_def::{
    DefWithBodyId, FunctionId, HasModule as _, ItemContainerId, Lookup as _,
    expr_store::{Body, ExpressionStore, SpecClauseKind, path::Path, pretty::print_expr_hir},
    hir::{self as h, BindingAnnotation, BindingId, ExprId, LabelId, LoopSource, PatId, Statement},
    resolver::{HasResolver as _, ResolveValueResult, Resolver, ValueNs},
    signatures::FunctionSignature,
};
use hir_ty::{
    InferenceResult,
    db::HirDatabase as _,
    next_solver::{Ty, TyKind},
};
use num_bigint::BigInt;
use rustc_type_ir::{IntTy, UintTy, inherent::IntoKind as _};
use vir::{
    ast::{
        self as v, ArithOp, BodyVisibility, ByRef, Constant, CrateId, DeclProph, Div0Behavior,
        Expr, ExprX, FunctionAttrsX, FunctionKind, FunctionX, HeaderExprX, InequalityOp, IntRange,
        IntegerTypeBitwidth, ItemKind, KrateX, Label, Mode, ModuleX, Opaqueness, OverflowBehavior,
        ParamX, PathX, PatternBinding, PatternX, PlaceX, ReadKind, Segment,
        SignedDivEdgeCaseBehavior, StmtX, Typ, TypDecoration, TypX, UnaryOp, UnfinalizedReadKind,
        VarIdent, VarIdentDisambiguate, Visibility,
    },
    ast_util::{air_unique_var, bool_typ, int_typ, unit_typ},
    def::{RETURN_VALUE, Spanned},
    messages::Span,
};

use crate::{ClauseRef, Unsupported, VerifyError};

/// Why a function could not be lowered.
pub(crate) enum Refused {
    Unsupported(Unsupported),
    Error(VerifyError),
}

/// Where a VIR node comes from, kept in its span's `raw_span`, so a failed query names the
/// clause or the code it is about.
#[derive(Clone, Debug)]
pub(crate) enum Origin {
    /// The function as a whole (its signature).
    Function,
    /// A node lowering makes that no source expression is (a `let`'s temporary).
    Synthesized,
    /// Body code, printed from HIR.
    Code(String),
    /// A clause's expression, or a part of one.
    Clause(ClauseRef),
}

/// A parameter: its name in the source and its VIR name.
pub(crate) struct LoweredParam {
    pub(crate) name: String,
    pub(crate) var: VarIdent,
}

/// A function lowered into a VIR crate of its own.
pub(crate) struct Lowered {
    /// `crate::path::name`.
    pub(crate) display: String,
    /// The crate: the function and its module.
    pub(crate) krate: v::Krate,
    /// The function's VIR name.
    pub(crate) fun: v::Fun,
    /// The module the function is in, whose bucket proves it.
    pub(crate) module: v::Path,
    /// The crate's VIR id.
    pub(crate) crate_id: CrateId,
    /// The parameters, in order, for a counterexample.
    pub(crate) params: Vec<LoweredParam>,
    /// The span of the function as a whole.
    pub(crate) span: Span,
}

/// `crate::path::name` of `func`.
pub(crate) fn function_display(db: &Db, func: FunctionId) -> String {
    let (krate, mut segments) = module_segments(db, func);
    segments.insert(0, krate);
    segments.push(FunctionSignature::of(db, func).name.as_str().to_owned()); // alloc: a name
    segments.join("::")
}

/// The crate name of `func`, and the names of the modules from the crate root to it.
fn module_segments(db: &Db, func: FunctionId) -> (String, Vec<String>) {
    let krate = func.krate(db);
    let krate_name = krate
        .extra_data(db)
        .display_name
        .as_ref()
        .map(|it| it.crate_name().as_str().to_owned()) // alloc: a name
        .unwrap_or_else(|| "crate".to_owned()); // alloc: a name
    let mut segments = Vec::new(); // alloc: a function's module path
    if let ItemContainerId::ModuleId(module) = func.lookup(db).container {
        let mut at = Some(module.to_db(db));
        while let Some(module) = at {
            if let Some(name) = module.name(db) {
                segments.push(name.as_str().to_owned()); // alloc: a name
            }
            at = module.containing_module(db);
        }
    }
    segments.reverse();
    (krate_name, segments)
}

fn unsupported(function: &str, what: impl Into<String>, at: impl Into<String>) -> Refused {
    Refused::Unsupported(Unsupported {
        function: function.to_owned(), // alloc: error message
        what: what.into(),
        at: at.into(),
    })
}

/// A VIR error's text: its note, then each label's.
pub(crate) fn vir_err_text(err: &v::VirErr) -> String {
    let mut text = err.note.clone(); // alloc: error message
    for span in &err.spans {
        if !span.as_string.is_empty() {
            text.push_str(&format!(" at `{}`", span.as_string));
        }
    }
    for label in &err.labels {
        text.push_str(&format!("; {} (`{}`)", label.note, label.span.as_string));
    }
    text
}

/// Lowers `func` to a VIR crate holding it alone.
pub(crate) fn lower_function(db: &Db, func: FunctionId) -> Result<Lowered, Refused> {
    let display = function_display(db, func);
    let sig = FunctionSignature::of(db, func);
    let fn_text = format!("fn {}", sig.name.as_str());
    if !matches!(func.lookup(db).container, ItemContainerId::ModuleId(_)) {
        return Err(unsupported(&display, "a method or an associated function", fn_text));
    }
    if !sig.generic_params.is_empty() {
        return Err(unsupported(&display, "a generic function", fn_text));
    }
    if sig.is_async() || sig.is_gen() {
        return Err(unsupported(&display, "an `async` or `gen` function", fn_text));
    }
    if !sig.has_body() {
        return Err(unsupported(&display, "a function without a body", fn_text));
    }
    let def = DefWithBodyId::FunctionId(func);
    let body = Body::of(db, def);
    let infer = InferenceResult::of(db, def);
    if infer.is_erroneous() || infer.has_type_mismatches() {
        return Err(Refused::Error(VerifyError {
            function: display,
            stage: "types",
            message: "the function does not type-check; it is proved once it does".to_owned(), // alloc: error message
        }));
    }
    let (krate_name, modules) = module_segments(db, func);
    // The `u64` is rustc's stable crate id, which only tells crates apart in a merged VIR
    // crate; this slice lowers one crate at a time.
    let crate_id = CrateId::Id(Arc::new(krate_name), 0);
    let module = Arc::new(PathX {
        krate: crate_id.clone(),
        segments: Arc::new(modules.iter().map(|it| Segment::T(Arc::new(it.clone()), 0)).collect()), // alloc: a path
    });
    let mut fn_segments = (*module.segments).clone(); // alloc: a path
    fn_segments.push(Segment::V(Arc::new(sig.name.as_str().to_owned()), 0)); // alloc: a name
    let fun = Arc::new(v::FunX {
        path: Arc::new(PathX { krate: crate_id.clone(), segments: Arc::new(fn_segments) }),
    });

    let mut lower = Lower {
        db,
        def,
        display: display.clone(),
        store: &body.store,
        infer,
        resolver: def.resolver(db),
        next_span: 1,
        next_read: 1,
        loops: Vec::new(), // alloc: a function's loop stack
        next_label: 0,
        clause: None,
        groups: body.store.spec_clauses().iter().map(|(group, _)| *group).collect(), // alloc: a function's clause list
        ret_bindings: Vec::new(), // alloc: a function's named returns
        ret: None,
    };
    let span = lower.span(Origin::Function, fn_text.clone());

    // The parameters (`check_item_fn`'s `params`): each a plain binding, typed by inference.
    let mut params = Vec::new(); // alloc: a function's parameters
    let mut lowered_params = Vec::new(); // alloc: a function's parameters
    if body.self_param.is_some() {
        return Err(unsupported(&display, "a `self` parameter", fn_text));
    }
    for param in body.params.iter() {
        let pat = param.formal;
        let h::Pat::Bind { id, subpat: None } = body.store.pat(pat) else {
            return Err(unsupported(&display, "a parameter that is not a plain name", fn_text));
        };
        let binding = &body.store[id];
        if !matches!(binding.mode, BindingAnnotation::Unannotated) {
            return Err(unsupported(&display, "a `mut` or `ref` parameter", fn_text));
        }
        let typ = lower.typ(infer.binding_ty(id), || binding.name.as_str().to_owned())?; // alloc: error message
        let var = lower.var(id);
        params.push(Spanned::new(
            span.clone(),
            ParamX {
                name: var.clone(),
                typ,
                mode: Mode::Exec,
                user_mut: false,
                unwrapped_info: None,
            },
        ));
        lowered_params.push(LoweredParam { name: binding.name.as_str().to_owned(), var }); // alloc: a name
    }

    // The return: named by the `ensures` group's closure parameter, if the return is named.
    let output = db.callable_item_signature(func.into()).skip_binder().skip_binder().output();
    let ret_typ = lower.typ(output, || fn_text.clone())?;
    for &(group, kind) in body.store.spec_clauses() {
        if kind == SpecClauseKind::Ensures
            && let h::Expr::Closure { args, .. } = body.store.expr(group)
        {
            for &arg in args {
                let h::Pat::Bind { id, subpat: None } = body.store.pat(arg) else {
                    return Err(unsupported(
                        &display,
                        "a named return that is not a plain name",
                        fn_text,
                    ));
                };
                lower.ret_bindings.push(id);
            }
        }
    }
    let ret_name = match lower.ret_bindings.first() {
        Some(&first) => lower.var(first),
        None => air_unique_var(RETURN_VALUE),
    };
    let ret = Spanned::new(
        span.clone(),
        ParamX {
            name: ret_name,
            typ: ret_typ.clone(),
            mode: Mode::Exec,
            user_mut: false,
            unwrapped_info: None,
        },
    );
    lower.ret = Some(ret_typ);

    // The body, whose head holds the function's clause groups as header statements.
    let mut vir_body = lower.expr(body.root_expr())?;
    let header = vir::headers::read_header(&mut vir_body, &vir::headers::HeaderAllows::All)
        .map_err(|err| lower.vir_error("vir", &err))?;
    if !header.recommend.is_empty() {
        return Err(lower.vir_error_text("vir", "non-spec functions cannot have `recommends`"));
    }

    let module_visibility = Visibility { restricted_to: Some(module.clone()) };
    let function = Spanned::new(
        span.clone(),
        FunctionX {
            name: fun.clone(),
            proxy: None,
            kind: FunctionKind::Static,
            visibility: Visibility { restricted_to: None },
            body_visibility: BodyVisibility::Visibility(module_visibility),
            opaqueness: Opaqueness::Opaque,
            owning_module: Some(module.clone()),
            mode: Mode::Exec,
            typ_params: Arc::new(Vec::new()), // alloc: VIR
            typ_bounds: Arc::new(Vec::new()), // alloc: VIR
            params: Arc::new(params),
            outer_ret: ret.clone(),
            inner_ret: ret,
            require: header.require,
            ensure: header.ensure,
            returns: header.returns,
            decrease: header.decrease,
            decrease_when: header.decrease_when,
            decrease_by: header.decrease_by,
            fndef_axioms: None,
            mask_spec: header.invariant_mask,
            atomic_update: None,
            unwind_spec: header.unwind_spec,
            item_kind: ItemKind::Function,
            attrs: Arc::new(FunctionAttrsX::default()),
            body: Some(vir_body),
            return_opaque_type_eqs: Arc::new(Vec::new()), // alloc: VIR
            extra_dependencies: Vec::new(),               // alloc: VIR
            hidden: Arc::new(Vec::new()),                 // alloc: VIR
        },
    );
    let krate = Arc::new(KrateX {
        functions: vec![function],    // alloc: VIR
        reveal_groups: Vec::new(),    // alloc: VIR
        datatypes: Vec::new(),        // alloc: VIR
        traits: Vec::new(),           // alloc: VIR
        trait_impls: Vec::new(),      // alloc: VIR
        assoc_type_impls: Vec::new(), // alloc: VIR
        modules: vec![Spanned::new(span.clone(), ModuleX { path: module.clone(), reveals: None })], // alloc: VIR
        external_fns: Vec::new(),       // alloc: VIR
        external_types: Vec::new(),     // alloc: VIR
        path_as_rust_names: Vec::new(), // alloc: VIR
        // Every target blaze builds for has 64-bit `usize`; this becomes the target's once
        // `blaze check --verify` passes the target in (docs/v2/plan/design/verus.md).
        arch: v::Arch { word_bits: v::ArchWordBits::Exactly(64) },
        opaque_types: Vec::new(), // alloc: VIR
    });
    Ok(Lowered { display, krate, fun, module, crate_id, params: lowered_params, span })
}

struct Lower<'db> {
    db: &'db Db,
    def: DefWithBodyId,
    display: String,
    store: &'db ExpressionStore,
    infer: &'db InferenceResult<'db>,
    resolver: Resolver<'db>,
    next_span: u64,
    next_read: u64,
    /// The loops around the expression being lowered, innermost last: the HIR label and the
    /// VIR label.
    loops: Vec<(Option<LabelId>, Label)>,
    next_label: usize,
    /// The clause item being lowered, when in one: its expressions are spec code.
    clause: Option<ClauseRef>,
    /// The clause groups, in source order (a group's index is its `ClauseRef::group`).
    groups: Vec<ExprId>,
    /// The named return's bindings (one per `ensures` group), which are the return value.
    ret_bindings: Vec<BindingId>,
    ret: Option<Typ>,
}

impl<'db> Lower<'db> {
    fn span(&mut self, origin: Origin, text: String) -> Span {
        let id = self.next_span;
        self.next_span += 1;
        Span { raw_span: Arc::new(origin), id, data: Vec::new(), as_string: text } // alloc: VIR
    }

    fn text(&self, expr: ExprId) -> String {
        print_expr_hir(self.db, self.store, self.def.into(), expr, syntax::Edition::CURRENT)
    }

    fn expr_span(&mut self, expr: ExprId) -> Span {
        let text = self.text(expr);
        match &self.clause {
            Some(clause) => self.span(Origin::Clause(clause.clone()), text), // clone: a clause's origin
            None => self.span(Origin::Code(text.clone()), text), // clone: a span's two texts
        }
    }

    fn in_spec(&self) -> bool {
        self.clause.is_some()
    }

    fn unsupported(&self, what: impl Into<String>, expr: ExprId) -> Refused {
        unsupported(&self.display, what, self.text(expr))
    }

    fn vir_error(&self, stage: &'static str, err: &v::VirErr) -> Refused {
        self.vir_error_text(stage, &vir_err_text(err))
    }

    fn vir_error_text(&self, stage: &'static str, message: &str) -> Refused {
        Refused::Error(VerifyError {
            function: self.display.clone(), // alloc: error message
            stage,
            message: message.to_owned(), // alloc: error message
        })
    }

    /// The VIR name of a binding: its name, told apart by its index (`local_to_var`).
    fn var(&self, binding: BindingId) -> VarIdent {
        let binding = match self.ret_bindings.first() {
            Some(&first) if self.ret_bindings.contains(&binding) => first,
            _ => binding,
        };
        let name = self.store[binding].name.as_str().to_owned(); // alloc: a name
        VarIdent(
            Arc::new(name),
            VarIdentDisambiguate::RustcId(u32::from(binding.into_raw()) as usize),
        )
    }

    /// `mid_ty_to_vir` for the types this slice takes.
    fn typ(&self, ty: Ty<'db>, at: impl FnOnce() -> String) -> Result<Typ, Refused> {
        let range = match ty.kind() {
            TyKind::Bool => return Ok(bool_typ()),
            TyKind::Never => {
                return Ok(Arc::new(TypX::Decorate(TypDecoration::Never, None, unit_typ())));
            }
            TyKind::Tuple(tys) if tys.is_empty() => return Ok(unit_typ()),
            TyKind::Int(IntTy::I8) => IntRange::I(8),
            TyKind::Int(IntTy::I16) => IntRange::I(16),
            TyKind::Int(IntTy::I32) => IntRange::I(32),
            TyKind::Int(IntTy::I64) => IntRange::I(64),
            TyKind::Int(IntTy::I128) => IntRange::I(128),
            TyKind::Int(IntTy::Isize) => IntRange::ISize,
            TyKind::Uint(UintTy::U8) => IntRange::U(8),
            TyKind::Uint(UintTy::U16) => IntRange::U(16),
            TyKind::Uint(UintTy::U32) => IntRange::U(32),
            TyKind::Uint(UintTy::U64) => IntRange::U(64),
            TyKind::Uint(UintTy::U128) => IntRange::U(128),
            TyKind::Uint(UintTy::Usize) => IntRange::USize,
            _ => {
                // `Debug` names the type only through the attached database.
                let what = blaze_query::db::attach(self.db, || format!("a value of type `{ty:?}`")); // alloc: error message
                return Err(unsupported(&self.display, what, at()));
            }
        };
        Ok(Arc::new(TypX::Int(range)))
    }

    fn expr_typ(&self, expr: ExprId) -> Result<Typ, Refused> {
        self.typ(self.infer.expr_ty(expr), || self.text(expr))
    }

    fn mk(&self, span: &Span, typ: &Typ, x: ExprX) -> Expr {
        v::SpannedTyped::new(span, typ, x)
    }

    /// `expr_to_vir`, with a `!`-typed expression coerced to the type it is used at
    /// (`NeverToAny`).
    fn expr(&mut self, expr: ExprId) -> Result<Expr, Refused> {
        let lowered = self.expr_inner(expr)?;
        let ty = self.infer.expr_ty(expr);
        let adjusted = self.infer.expr_ty_after_adjustments(expr);
        if matches!(ty.kind(), TyKind::Never) && !matches!(adjusted.kind(), TyKind::Never) {
            let typ = self.typ(adjusted, || self.text(expr))?;
            let span = lowered.span.clone();
            return Ok(self.mk(&span, &typ, ExprX::NeverToAny(lowered)));
        }
        Ok(lowered)
    }

    fn read_local(&mut self, span: &Span, typ: &Typ, var: VarIdent) -> Expr {
        let place = v::SpannedTyped::new(span, typ, PlaceX::Local(var));
        let id = self.next_read;
        self.next_read += 1;
        let kind = UnfinalizedReadKind { preliminary_kind: ReadKind::Copy, id };
        self.mk(span, typ, ExprX::ReadPlace(place, kind))
    }

    /// The local `path` at `expr` names.
    fn resolve_local(&mut self, expr: ExprId, path: Path<'_>) -> Option<BindingId> {
        let guard = self.resolver.update_to_inner_scope(self.db, self.def, expr);
        let hygiene = self.store.expr_path_hygiene(expr);
        let found = self.resolver.resolve_path_in_value_ns_fully(self.db, path, hygiene);
        self.resolver.reset_to_guard(guard);
        match found {
            Some(ValueNs::LocalBinding(binding)) => Some(binding),
            _ => None,
        }
    }

    /// The local an assignment at `expr` assigns to, from its target pattern.
    fn assigned_local(&mut self, expr: ExprId, target: PatId) -> Option<BindingId> {
        match self.store.pat(target) {
            h::Pat::Path(path) => {
                let guard = self.resolver.update_to_inner_scope(self.db, self.def, expr);
                let hygiene = self.store.pat_path_hygiene(target);
                let found = self.resolver.resolve_path_in_value_ns(self.db, path, hygiene);
                self.resolver.reset_to_guard(guard);
                match found {
                    Some(ResolveValueResult::ValueNs(ValueNs::LocalBinding(binding))) => {
                        Some(binding)
                    }
                    _ => None,
                }
            }
            h::Pat::Expr(place) => self.place_local(place),
            _ => None,
        }
    }

    /// The local `expr` is, when it is a path to one.
    fn place_local(&mut self, expr: ExprId) -> Option<BindingId> {
        match self.store.expr(expr) {
            h::Expr::Path(path) => self.resolve_local(expr, path),
            _ => None,
        }
    }

    fn int_range(&self, typ: &Typ) -> Option<IntRange> {
        match &**typ {
            TypX::Int(range) => Some(*range),
            _ => None,
        }
    }

    /// `binopkind_to_binaryop` for an arithmetic operator: spec code is over `int`, exec code
    /// checks overflow in the operands' range. The type is the result's.
    fn arith(
        &self,
        op: h::ArithOp,
        operand: &Typ,
        expr: ExprId,
    ) -> Result<(v::BinaryOp, Typ, bool), Refused> {
        let Some(range) = self.int_range(operand) else {
            return Err(self.unsupported(format!("`{op:?}` on a non-integer"), expr));
        };
        let spec = self.in_spec();
        let overflow = if spec { OverflowBehavior::Allow } else { OverflowBehavior::Error(range) };
        let div0 = if spec { Div0Behavior::Allow } else { Div0Behavior::Error };
        let signed_width = match range {
            IntRange::I(width) => Some(IntegerTypeBitwidth::Width(width)),
            IntRange::ISize => Some(IntegerTypeBitwidth::ArchWordSize),
            _ => None,
        };
        let result = if spec { int_typ() } else { operand.clone() };
        let op = match op {
            h::ArithOp::Add => ArithOp::Add(overflow),
            h::ArithOp::Sub => ArithOp::Sub(overflow),
            h::ArithOp::Mul => ArithOp::Mul(overflow),
            // Spec `/` and `%` are Euclidean on `int`, as `builtin`'s `spec_euclidean_div`.
            h::ArithOp::Div if spec => ArithOp::EuclideanDiv(div0),
            h::ArithOp::Rem if spec => ArithOp::EuclideanMod(div0),
            h::ArithOp::Div => match signed_width {
                None => ArithOp::EuclideanDiv(div0),
                Some(width) => {
                    ArithOp::TruncatingDiv(div0, SignedDivEdgeCaseBehavior::Error(width))
                }
            },
            h::ArithOp::Rem => match signed_width {
                None => ArithOp::EuclideanMod(div0),
                Some(width) => {
                    ArithOp::TruncatingMod(div0, SignedDivEdgeCaseBehavior::Error(width))
                }
            },
            h::ArithOp::Shl
            | h::ArithOp::Shr
            | h::ArithOp::BitXor
            | h::ArithOp::BitOr
            | h::ArithOp::BitAnd => {
                return Err(self.unsupported(format!("the bit operator `{op:?}`"), expr));
            }
        };
        let clip = !spec
            && matches!(
                op,
                ArithOp::EuclideanDiv(_)
                    | ArithOp::EuclideanMod(_)
                    | ArithOp::TruncatingDiv(..)
                    | ArithOp::TruncatingMod(..)
            );
        Ok((v::BinaryOp::Arith(op), result, clip))
    }

    fn label_for(&self, label: Option<LabelId>, expr: ExprId) -> Result<Label, Refused> {
        let found = match label {
            None => self.loops.last(),
            Some(label) => self.loops.iter().rev().find(|(it, _)| *it == Some(label)),
        };
        match found {
            Some((_, label)) => Ok(label.clone()), // clone: a VIR label
            None => Err(self.unsupported("a `break` or `continue` out of a labelled block", expr)),
        }
    }

    fn expr_inner(&mut self, expr: ExprId) -> Result<Expr, Refused> {
        let span = self.expr_span(expr);
        let store = self.store;
        match store.expr(expr) {
            h::Expr::Literal(lit) => {
                let typ = self.expr_typ(expr)?;
                let constant = match lit {
                    h::Literal::Bool(b) => Constant::Bool(*b),
                    h::Literal::Int(value, _) => Constant::Int(BigInt::from(*value)),
                    h::Literal::Uint(value, _) => Constant::Int(BigInt::from(*value)),
                    _ => {
                        return Err(
                            self.unsupported("a literal that is not an integer or a `bool`", expr)
                        );
                    }
                };
                Ok(self.mk(&span, &typ, ExprX::Const(constant)))
            }
            h::Expr::Path(path) => {
                let Some(binding) = self.resolve_local(expr, path) else {
                    return Err(self.unsupported(
                        "a path to an item (a constant, a static, a function)",
                        expr,
                    ));
                };
                let typ = self.typ(self.infer.binding_ty(binding), || self.text(expr))?;
                let var = self.var(binding);
                Ok(self.read_local(&span, &typ, var))
            }
            h::Expr::UnaryOp { expr: operand, op } => {
                let inner = self.expr(operand)?;
                match op {
                    h::UnaryOp::Not if matches!(&*inner.typ, TypX::Bool) => {
                        Ok(self.mk(&span, &bool_typ(), ExprX::Unary(UnaryOp::Not, inner)))
                    }
                    h::UnaryOp::Neg => {
                        // `-e` is `0 - e`: in exec code it overflows exactly when `0 - e` does.
                        let (op, typ, _) = self.arith(h::ArithOp::Sub, &inner.typ, expr)?;
                        let zero = self.mk(
                            &span,
                            &inner.typ,
                            ExprX::Const(Constant::Int(BigInt::from(0))),
                        );
                        Ok(self.mk(&span, &typ, ExprX::Binary(op, zero, inner)))
                    }
                    _ => Err(self.unsupported(format!("the unary operator `{op:?}` here"), expr)),
                }
            }
            h::Expr::BinaryOp { lhs, rhs, op } => {
                let Some(op) = op else {
                    return Err(self.unsupported("a binary expression with no operator", expr));
                };
                match op {
                    h::BinaryOp::Assignment { op } => {
                        let Some(binding) = self.place_local(lhs) else {
                            return Err(self.unsupported(
                                "an assignment to something other than a local",
                                expr,
                            ));
                        };
                        self.assign(expr, &span, binding, rhs, op)
                    }
                    h::BinaryOp::LogicOp(op) => {
                        let (l, r) = (self.expr(lhs)?, self.expr(rhs)?);
                        let op = match op {
                            h::LogicOp::And => v::LogicalOp::And,
                            h::LogicOp::Or => v::LogicalOp::Or,
                        };
                        Ok(self.mk(&span, &bool_typ(), ExprX::Logical(op, l, r)))
                    }
                    h::BinaryOp::CmpOp(op) => {
                        let (l, r) = (self.expr(lhs)?, self.expr(rhs)?);
                        if !matches!(&*l.typ, TypX::Int(_) | TypX::Bool) {
                            return Err(self.unsupported(
                                "a comparison of values that are not integers or `bool`s",
                                expr,
                            ));
                        }
                        let mode = if self.in_spec() { Mode::Spec } else { Mode::Exec };
                        let op = match op {
                            h::CmpOp::Eq { negated: false } => v::BinaryOp::Eq(mode),
                            h::CmpOp::Eq { negated: true } => v::BinaryOp::Ne,
                            h::CmpOp::Ord { ordering, strict } => {
                                if !matches!(&*l.typ, TypX::Int(_)) {
                                    return Err(self.unsupported("an ordering of `bool`s", expr));
                                }
                                v::BinaryOp::Inequality(match (ordering, strict) {
                                    (h::Ordering::Less, true) => InequalityOp::Lt,
                                    (h::Ordering::Less, false) => InequalityOp::Le,
                                    (h::Ordering::Greater, true) => InequalityOp::Gt,
                                    (h::Ordering::Greater, false) => InequalityOp::Ge,
                                })
                            }
                        };
                        Ok(self.mk(&span, &bool_typ(), ExprX::Binary(op, l, r)))
                    }
                    h::BinaryOp::ArithOp(op) => {
                        let (l, r) = (self.expr(lhs)?, self.expr(rhs)?);
                        let (op, typ, clip) = self.arith(op, &l.typ, expr)?;
                        let e = self.mk(&span, &typ, ExprX::Binary(op, l, r));
                        if !clip {
                            return Ok(e);
                        }
                        // The result of `/` and `%` is in range; the clip tells the solver
                        // (`mk_ty_clip`).
                        let range = self.int_range(&typ).unwrap_or(IntRange::Int);
                        Ok(self.mk(
                            &span,
                            &typ,
                            ExprX::Unary(UnaryOp::Clip { range, truncate: true }, e),
                        ))
                    }
                }
            }
            h::Expr::Assignment { target, value } => {
                let Some(binding) = self.assigned_local(expr, target) else {
                    return Err(
                        self.unsupported("an assignment to something other than a local", expr)
                    );
                };
                self.assign(expr, &span, binding, value, None)
            }
            h::Expr::If { condition, then_branch, else_branch } => {
                if matches!(store.expr(condition), h::Expr::Let { .. }) {
                    return Err(self.unsupported("an `if let`", expr));
                }
                let typ = self.expr_typ(expr)?;
                let c = self.expr(condition)?;
                let t = self.expr(then_branch)?;
                let e = match else_branch {
                    Some(it) => Some(self.expr(it)?),
                    None => None,
                };
                Ok(self.mk(&span, &typ, ExprX::If(c, t, e)))
            }
            h::Expr::Block { id: _, statements, tail, label, unsafe_: _ } => {
                if label.is_some() {
                    return Err(self.unsupported("a labelled block", expr));
                }
                let typ = self.expr_typ(expr)?;
                let mut stmts = Vec::new(); // alloc: VIR
                for statement in statements.iter() {
                    match statement {
                        Statement::Expr { expr: group, has_semi: _ }
                            if store.spec_clause(group).is_some() =>
                        {
                            let header = self.header(group)?;
                            stmts.push(Spanned::new(header.span.clone(), StmtX::Expr(header)));
                        }
                        Statement::Expr { expr: it, has_semi: _ } => {
                            let e = self.expr(it)?;
                            stmts.push(Spanned::new(e.span.clone(), StmtX::Expr(e)));
                        }
                        Statement::Let { pat, type_ref: _, initializer, else_branch, is_super } => {
                            if else_branch.is_some() || is_super {
                                return Err(self.unsupported("a `let else` or a `super let`", expr));
                            }
                            stmts.push(self.let_stmt(expr, pat, initializer)?);
                        }
                        // Nested items are lowered (and proved) as their own functions.
                        Statement::Item(_) => {}
                    }
                }
                let tail = match tail {
                    Some(it) => Some(self.expr(it)?),
                    None => None,
                };
                Ok(self.mk(&span, &typ, ExprX::Block(Arc::new(stmts), tail)))
            }
            h::Expr::Loop { body, label, source } => {
                let (cond, body_expr) = match source {
                    LoopSource::Loop => (None, body),
                    LoopSource::While => match store.expr(body) {
                        h::Expr::If { condition, then_branch, else_branch: Some(_) } => {
                            if matches!(store.expr(condition), h::Expr::Let { .. }) {
                                return Err(self.unsupported("a `while let` loop", expr));
                            }
                            (Some(condition), then_branch)
                        }
                        _ => {
                            return Err(
                                self.unsupported("a `while` loop of an unexpected shape", expr)
                            );
                        }
                    },
                    LoopSource::ForLoop => return Err(self.unsupported("a `for` loop", expr)),
                };
                let typ = self.expr_typ(expr)?;
                let vir_label = Label {
                    id: self.next_label,
                    name: label.map(|it| store[it].name.as_str().to_owned()), // alloc: a label
                };
                self.next_label += 1;
                let cond = match cond {
                    Some(it) => Some(self.expr(it)?),
                    None => None,
                };
                self.loops.push((label, vir_label.clone())); // clone: a VIR label
                let lowered = self.expr(body_expr);
                self.loops.pop();
                let mut body = lowered?;
                let header =
                    vir::headers::read_header(&mut body, &vir::headers::HeaderAllows::Loop)
                        .map_err(|err| self.vir_error("vir", &err))?;
                Ok(self.mk(
                    &span,
                    &typ,
                    ExprX::Loop {
                        loop_isolation: true,
                        allow_complex_invariants: false,
                        is_for_loop: false,
                        assume_termination: false,
                        label: vir_label,
                        cond,
                        body,
                        invs: header.loop_invariants(),
                        decrease: header.decrease,
                        atomic_call: false,
                        known_no_back_edge: false,
                        source: v::LoopSource::Loop,
                    },
                ))
            }
            h::Expr::Break { expr: value, label } => {
                if value.is_some() {
                    return Err(self.unsupported("a `break` with a value", expr));
                }
                let label = self.label_for(label, expr)?;
                let typ = self.expr_typ(expr)?;
                Ok(self.mk(
                    &span,
                    &typ,
                    ExprX::BreakOrContinue { label, is_break: true, value: None },
                ))
            }
            h::Expr::Continue { label } => {
                let label = self.label_for(label, expr)?;
                let typ = self.expr_typ(expr)?;
                Ok(self.mk(
                    &span,
                    &typ,
                    ExprX::BreakOrContinue { label, is_break: false, value: None },
                ))
            }
            h::Expr::Return { expr: value } => {
                let typ = self.expr_typ(expr)?;
                let value = match value {
                    Some(it) => Some(self.expr(it)?),
                    None => None,
                };
                Ok(self.mk(&span, &typ, ExprX::Return(value)))
            }
            h::Expr::Call { .. } | h::Expr::MethodCall { .. } => {
                Err(self.unsupported("a call (callees' specifications are the next slice)", expr))
            }
            other => {
                Err(self
                    .unsupported(format!("the expression kind `{}`", expr_kind_name(&other)), expr))
            }
        }
    }

    /// `x = value`, or `x op= value` (`AssignOp`), to a local.
    fn assign(
        &mut self,
        expr: ExprId,
        span: &Span,
        binding: BindingId,
        value: ExprId,
        op: Option<h::ArithOp>,
    ) -> Result<Expr, Refused> {
        if self.in_spec() {
            return Err(self.unsupported("an assignment in a clause", expr));
        }
        let typ = self.typ(self.infer.binding_ty(binding), || self.text(expr))?;
        let rhs = self.expr(value)?;
        let op = match op {
            Some(op) => Some(self.arith(op, &typ, expr)?.0),
            None => None,
        };
        let place = v::SpannedTyped::new(span, &typ, PlaceX::Local(self.var(binding)));
        Ok(self.mk(span, &unit_typ(), ExprX::Assign { place, rhs, op, typ, resolve: false }))
    }

    /// `let x = init;` (`StmtX::Decl` with the initializer as a temporary place).
    fn let_stmt(
        &mut self,
        block: ExprId,
        pat: PatId,
        initializer: Option<ExprId>,
    ) -> Result<v::Stmt, Refused> {
        let h::Pat::Bind { id, subpat: None } = self.store.pat(pat) else {
            return Err(self.unsupported("a `let` with a pattern that is not a plain name", block));
        };
        let binding = &self.store[id];
        let user_mut = match binding.mode {
            BindingAnnotation::Unannotated => false,
            BindingAnnotation::Mutable => true,
            _ => return Err(self.unsupported("a `ref` binding", block)),
        };
        let typ = self.typ(self.infer.binding_ty(id), || self.text(block))?;
        let name = binding.name.as_str().to_owned(); // alloc: a name
        let span = self.span(Origin::Code(format!("let {name}")), format!("let {name}"));
        let init = match initializer {
            Some(init) => {
                let e = self.expr(init)?;
                let temp_span = self.span(Origin::Synthesized, e.span.as_string.clone()); // clone: a span's text
                Some(v::SpannedTyped::new(&temp_span, &e.typ.clone(), PlaceX::Temporary(e)))
            }
            None => None,
        };
        let pattern = v::SpannedTyped::new(
            &span,
            &typ,
            PatternX::Var(PatternBinding {
                name: self.var(id),
                by_ref: ByRef::No,
                typ: typ.clone(),
                user_mut: Some(user_mut),
                copy: true,
            }),
        );
        Ok(Spanned::new(
            span,
            StmtX::Decl {
                pattern,
                mode: Some((Mode::Exec, DeclProph::Default)),
                init,
                els: None,
                assert_irrefutable: false,
            },
        ))
    }

    /// A clause group as a VIR header expression.
    fn header(&mut self, group: ExprId) -> Result<Expr, Refused> {
        let Some(kind) = self.store.spec_clause(group) else {
            return Err(self.unsupported("a clause group lowering did not record", group));
        };
        let index =
            self.groups.iter().position(|it| *it == group).unwrap_or(self.groups.len()) as u32;
        let (named, items_expr) = match self.store.expr(group) {
            h::Expr::Closure { args, body, .. } => (args.first().copied(), body),
            _ => (None, group),
        };
        let h::Expr::Tuple { exprs } = self.store.expr(items_expr) else {
            return Err(self.unsupported("a clause group that is not a list", group));
        };
        let mut items = Vec::new(); // alloc: VIR
        for (item, &e) in exprs.iter().enumerate() {
            self.clause =
                Some(ClauseRef { kind, group: index, item: item as u32, text: self.text(e) });
            let lowered = self.expr(e);
            self.clause = None;
            let lowered = lowered?;
            if !matches!(&*lowered.typ, TypX::Bool)
                && !matches!(kind, SpecClauseKind::Decreases | SpecClauseKind::LoopDecreases)
            {
                return Err(self.unsupported("a clause that is not a `bool`", e));
            }
            items.push(lowered);
        }
        let items = Arc::new(items);
        let header = match kind {
            SpecClauseKind::Requires => HeaderExprX::Requires(items),
            SpecClauseKind::Recommends => HeaderExprX::Recommends(items),
            SpecClauseKind::Ensures => {
                let named = match named {
                    Some(pat) => match self.store.pat(pat) {
                        h::Pat::Bind { id, subpat: None } => Some((self.var(id), self.ret.clone())),
                        _ => {
                            return Err(
                                self.unsupported("a named return that is not a plain name", group)
                            );
                        }
                    },
                    None => None,
                };
                HeaderExprX::Ensures(named, (items, Arc::new(Vec::new()))) // alloc: VIR
            }
            SpecClauseKind::Decreases | SpecClauseKind::LoopDecreases => {
                HeaderExprX::Decreases(items)
            }
            SpecClauseKind::Invariant => HeaderExprX::Invariant(items),
            SpecClauseKind::LoopEnsures => {
                HeaderExprX::Ensures(None, (items, Arc::new(Vec::new())))
            } // alloc: VIR
        };
        let span = self.span(
            Origin::Clause(ClauseRef { kind, group: index, item: 0, text: self.text(items_expr) }),
            format!("{} {}", kind.keyword(), self.text(items_expr)),
        );
        Ok(self.mk(&span, &unit_typ(), ExprX::Header(Arc::new(header))))
    }
}

/// A name for an expression kind this slice does not lower.
fn expr_kind_name(expr: &h::Expr<'_>) -> &'static str {
    match expr {
        h::Expr::Missing => "missing expression",
        h::Expr::Let { .. } => "let",
        h::Expr::Const(_) => "const block",
        h::Expr::Match { .. } => "match",
        h::Expr::Become { .. } => "become",
        h::Expr::Yield { .. } => "yield",
        h::Expr::Yeet { .. } => "do yeet",
        h::Expr::RecordLit { .. } => "struct literal",
        h::Expr::Field { .. } => "field access",
        h::Expr::Await { .. } => "await",
        h::Expr::Cast { .. } => "cast",
        h::Expr::Ref { .. } => "reference",
        h::Expr::Index { .. } => "index",
        h::Expr::Closure { .. } => "closure",
        h::Expr::Tuple { .. } => "tuple",
        h::Expr::Array(_) => "array",
        h::Expr::Underscore => "_",
        h::Expr::OffsetOf(_) => "offset_of",
        h::Expr::InlineAsm(_) => "asm",
        _ => "expression",
    }
}
