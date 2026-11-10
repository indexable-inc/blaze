//! Spec lowering: a function's clause groups as Lean propositions over its model (step 2 of
//! the pipeline).
//!
//! The clauses have one home, the source, and one reading of where they are: the groups
//! hir-def lowers each clause to (`ExpressionStore::spec_clauses`), typed by inference. A clause's arithmetic is
//! over mathematical integers, so an integer is its model's `.val : Int` and a `bool` is the
//! proposition that it is `true`.
//!
//! v0 takes what the grammar on main has: `requires` and `ensures` (with a named return)
//! whose expressions are literals, parameters, the named return, `!`, `-`, `+`, `-`, `*`,
//! comparisons, `&&`, `||`, a field, and `len()` of a slice. `forall`, `==>`, `old` and
//! `spec fn` are not parsed on main yet; when they are, they lower to [`Term::Forall`],
//! [`Infix::Implies`], the entry value's variable and a Lean `def`. A loop's `invariant` is a
//! hint to a solver (the Z3 tier, retired 2026-11-10) and states nothing of the function, so it is not lowered: a
//! Lean proof brings its own induction. `decreases` (termination), `recommends` and a loop's
//! `ensures` are refused, naming the clause: lowering the rest without them would state less
//! than the function's spec says.

use base_db::Db;
use hir_def::{
    DefWithBodyId, FunctionId, HasModule as _, ItemContainerId, Lookup as _,
    expr_store::{Body, ExpressionStore, SpecClauseKind, path::Path, pretty::print_expr_hir},
    hir::{self as h, BindingId, ExprId},
    resolver::{HasResolver as _, Resolver, ValueNs},
    signatures::FunctionSignature,
};
use hir_ty::{InferenceResult, db::HirDatabase as _, next_solver::TyKind};
use rustc_type_ir::inherent::IntoKind as _;

use crate::{
    NotTranslated,
    lean::{Infix, Term, app, atom, infix, name},
};

/// A function's clauses over its model's parameters and result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    pub requires: Vec<Term>,
    pub ensures: Vec<Term>,
    /// The variable the `ensures` call the result: the named return, else `r`.
    pub result: String,
    /// Each lowered clause as written (`requires x < 10`), in source order: what a proof key
    /// and a refusal name.
    pub clauses: Vec<String>,
}

/// What a clause's expression is, in Lean.
enum Value {
    Prop(Term),
    /// A mathematical integer.
    Int(Term),
    /// A value of the model.
    Model(Term),
}

struct Lower<'db> {
    db: &'db Db,
    def: DefWithBodyId,
    display: String,
    store: &'db ExpressionStore,
    infer: &'db InferenceResult<'db>,
    resolver: Resolver<'db>,
}

type Refused<T> = Result<T, NotTranslated>;

impl<'db> Lower<'db> {
    fn text(&self, expr: ExprId) -> String {
        print_expr_hir(self.db, self.store, self.def.into(), expr, syntax::Edition::CURRENT)
    }

    fn refuse(&self, what: impl Into<String>, expr: ExprId) -> NotTranslated {
        NotTranslated { function: self.display.clone(), what: what.into(), at: self.text(expr) }
    }

    fn local(&mut self, expr: ExprId, path: Path<'_>) -> Option<BindingId> {
        let guard = self.resolver.update_to_inner_scope(self.db, self.def, expr);
        let hygiene = self.store.expr_path_hygiene(expr);
        let found = self.resolver.resolve_path_in_value_ns_fully(self.db, path, hygiene);
        self.resolver.reset_to_guard(guard);
        match found {
            Some(ValueNs::LocalBinding(binding)) => Some(binding),
            _ => None,
        }
    }

    /// `term`, a value of the model of `expr`'s type, as a clause reads it.
    fn of_type(&self, term: Term, expr: ExprId) -> Value {
        let mut ty = self.infer.expr_ty(expr);
        while let TyKind::Ref(_, inner, _) = ty.kind() {
            ty = inner;
        }
        match ty.kind() {
            TyKind::Int(_) | TyKind::Uint(_) => Value::Int(Term::Proj(Box::new(term), "val".to_owned())),
            TyKind::Bool => Value::Prop(infix(Infix::Eq, term, atom("true"))),
            _ => Value::Model(term),
        }
    }

    fn prop(&mut self, expr: ExprId) -> Refused<Term> {
        match self.value(expr)? {
            Value::Prop(term) => Ok(term),
            _ => Err(self.refuse("a clause that is not a `bool`", expr)),
        }
    }

    fn int(&mut self, expr: ExprId) -> Refused<Term> {
        match self.value(expr)? {
            Value::Int(term) => Ok(term),
            _ => Err(self.refuse("arithmetic on what is not an integer, in a clause", expr)),
        }
    }

    fn value(&mut self, expr: ExprId) -> Refused<Value> {
        let store = self.store;
        Ok(match store.expr(expr) {
            h::Expr::Literal(literal) => match literal {
                h::Literal::Bool(value) => Value::Prop(atom(if *value { "True" } else { "False" })),
                h::Literal::Int(value, _) => Value::Int(atom(value.to_string())),
                h::Literal::Uint(value, _) => Value::Int(atom(value.to_string())),
                _ => return Err(self.refuse("a literal that is not an integer or a `bool`", expr)),
            },
            h::Expr::Path(path) => {
                let Some(binding) = self.local(expr, path) else {
                    return Err(self.refuse("a path to an item, in a clause", expr));
                };
                self.of_type(name(store[binding].name.as_str().to_owned()), expr)
            }
            h::Expr::UnaryOp { expr: operand, op: h::UnaryOp::Not } => {
                Value::Prop(Term::Not(Box::new(self.prop(operand)?)))
            }
            h::Expr::UnaryOp { expr: operand, op: h::UnaryOp::Neg } => {
                Value::Int(infix(Infix::Sub, atom("0"), self.int(operand)?))
            }
            h::Expr::BinaryOp { lhs, rhs, op: Some(op) } => match op {
                h::BinaryOp::LogicOp(op) => {
                    let op = match op {
                        h::LogicOp::And => Infix::And,
                        h::LogicOp::Or => Infix::Or,
                    };
                    Value::Prop(infix(op, self.prop(lhs)?, self.prop(rhs)?))
                }
                h::BinaryOp::ArithOp(op) => {
                    let op = match op {
                        h::ArithOp::Add => Infix::Add,
                        h::ArithOp::Sub => Infix::Sub,
                        h::ArithOp::Mul => Infix::Mul,
                        other => {
                            return Err(self.refuse(format!("`{other:?}` in a clause"), expr));
                        }
                    };
                    Value::Int(infix(op, self.int(lhs)?, self.int(rhs)?))
                }
                h::BinaryOp::CmpOp(h::CmpOp::Eq { negated }) => {
                    let op = if negated { Infix::Ne } else { Infix::Eq };
                    let term = match (self.value(lhs)?, self.value(rhs)?) {
                        (Value::Int(left), Value::Int(right))
                        | (Value::Prop(left), Value::Prop(right))
                        | (Value::Model(left), Value::Model(right)) => infix(op, left, right),
                        _ => return Err(self.refuse("a comparison of two kinds of value", expr)),
                    };
                    Value::Prop(term)
                }
                h::BinaryOp::CmpOp(h::CmpOp::Ord { ordering, strict }) => {
                    let (left, right) = (self.int(lhs)?, self.int(rhs)?);
                    let (left, right) = match ordering {
                        h::Ordering::Less => (left, right),
                        h::Ordering::Greater => (right, left),
                    };
                    Value::Prop(infix(if strict { Infix::Lt } else { Infix::Le }, left, right))
                }
                h::BinaryOp::Assignment { .. } => {
                    return Err(self.refuse("an assignment in a clause", expr));
                }
            },
            h::Expr::Field { expr: base, name: field } => match self.value(base)? {
                Value::Model(term) => {
                    self.of_type(Term::Proj(Box::new(term), field.as_str().to_owned()), expr)
                }
                _ => return Err(self.refuse("a field of what is not a struct, in a clause", expr)),
            },
            h::Expr::MethodCall { receiver, method_name, args, .. }
                if method_name.as_str() == "len" && args.is_empty() =>
            {
                let db = self.db;
                let is_slice_len = self.infer.method_resolution(expr).is_some_and(|(callee, _)| {
                    callee.krate(db).data(db).origin.is_lang()
                        && match callee.lookup(db).container {
                            ItemContainerId::ImplId(imp) => {
                                matches!(db.impl_self_ty(imp).skip_binder().kind(), TyKind::Slice(_))
                            }
                            _ => false,
                        }
                });
                match (is_slice_len, self.value(receiver)?) {
                    (true, Value::Model(term)) => Value::Int(Term::Proj(
                        Box::new(app("Slice.len", vec![term])),
                        "val".to_owned(),
                    )),
                    _ => return Err(self.refuse("a `len()` that is not a slice's, in a clause", expr)),
                }
            }
            _ => return Err(self.refuse("an expression v0 does not lower in a clause", expr)),
        })
    }
}

/// `func`'s clauses, `None` when it has none. `params` are the names the model gives its
/// parameters, which the clauses must name them by.
pub(crate) fn lower(db: &Db, func: FunctionId, params: &[String]) -> Refused<Option<Spec>> {
    let def = DefWithBodyId::FunctionId(func);
    let body = Body::of(db, def);
    let groups = body.store.spec_clauses();
    if groups.is_empty() {
        return Ok(None);
    }
    let display = crate::body::function_path(db, func).join("::");
    let fn_text = format!("fn {}", FunctionSignature::of(db, func).name.as_str());
    let mut lower = Lower {
        db,
        def,
        display: display.clone(),
        store: &body.store,
        infer: InferenceResult::of(db, def),
        resolver: def.resolver(db),
    };
    let refuse = |what: &str| NotTranslated {
        function: display.clone(),
        what: what.to_owned(),
        at: fn_text.clone(),
    };
    // A clause names a parameter by its binding, the model by its variable: they must agree.
    for (param, expected) in body.params.iter().zip(params) {
        let named = match body.store.pat(param.formal) {
            h::Pat::Bind { id, subpat: None } => body.store[id].name.as_str() == expected,
            _ => false,
        };
        if !named {
            return Err(refuse(
                "clauses on a function with a parameter that is a pattern, or whose name another \
                 local shares",
            ));
        }
    }
    let mut spec =
        Spec { requires: Vec::new(), ensures: Vec::new(), result: "r".to_owned(), clauses: Vec::new() };
    for &(group, kind) in groups {
        let (named, items) = match body.store.expr(group) {
            h::Expr::Closure { args, body: items, .. } => (args.first().copied(), items),
            _ => (None, group),
        };
        let h::Expr::Tuple { exprs } = body.store.expr(items) else {
            return Err(lower.refuse("a clause group that is not a list", group));
        };
        match kind {
            SpecClauseKind::Invariant => continue,
            SpecClauseKind::Requires | SpecClauseKind::Ensures => {}
            SpecClauseKind::Recommends
            | SpecClauseKind::Decreases
            | SpecClauseKind::LoopEnsures
            | SpecClauseKind::LoopDecreases => {
                return Err(lower.refuse(
                    format!("a `{}` clause, which v0 does not lower", kind.keyword()),
                    group,
                ));
            }
        }
        if let Some(named) = named {
            let h::Pat::Bind { id, subpat: None } = body.store.pat(named) else {
                return Err(lower.refuse("a named return that is not a plain name", group));
            };
            spec.result = body.store[id].name.as_str().to_owned();
        }
        for &expr in exprs {
            let term = lower.prop(expr)?;
            spec.clauses.push(format!("{} {}", kind.keyword(), lower.text(expr)));
            match kind {
                SpecClauseKind::Requires => spec.requires.push(term),
                _ => spec.ensures.push(term),
            }
        }
    }
    Ok(Some(spec))
}
