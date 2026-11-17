//! blaze_verify's native contracts checked at run time (docs/v2/plan/design/contracts.md,
//! part 1).
//!
//! A function's clause groups are ghost (`ExpressionStore::spec_clause`): MIR building leaves
//! them out. When the function's crate has the cfg atom [`cfg::CONTRACT_CHECKS`] (its
//! profile's `contract-checks`, on in dev and test), its `requires` and `ensures` are also
//! built as checks:
//!
//! - each `requires` expression is tested where its group stands, at the head of the body,
//!   before any statement of the body runs;
//! - each `ensures` expression is tested once the value is in the return slot, on every way
//!   out: the body's value and each `return` (a `?` is a `return`). The named return `r` of
//!   `-> (r: T)` is the return slot itself.
//!
//! A clause that does not hold calls core's `panic_nounwind` with a message naming the
//! function, the clause and where it is written: the process aborts (exit 134), as nightly's
//! `-Zcontract-checks` does. `recommends` and `decreases` stay erased, as do a loop's clauses.
//!
//! What this does not build, each refused or documented rather than skipped:
//! - an `ensures` of a function with a `mut` parameter is refused: the clause would read the
//!   parameter as the body left it, not as it was passed;
//! - an `ensures` whose named return is not a plain binding (`-> ((a, b): T)`) is refused;
//! - an `ensures` that moves out of the returned value, a parameter or a local of the body is
//!   refused, naming what it moves: the clause runs before the caller gets the value, so it
//!   reads it, by reference (`match &r { Ok(k) => .. }`); what the clause itself makes (the
//!   value of a call, what a pattern binds out of it) is its own to move and drop;
//! - a `become` in a function whose `ensures` is checked is refused: it leaves without a return;
//! - a clause runs as the program's own code: `n + 1` is machine arithmetic that can overflow
//!   (and panic in dev), where a proof reads it as an unbounded `int`.

use hir_def::{FunctionId, expr_store::SpecClauseKind};
use intern::Symbol;

use super::*;

/// What a body that checks its clauses at run time knows of them: the function, and its first
/// parameter binding declared `mut`, which an `ensures` cannot be checked against.
#[derive(Clone, Copy)]
pub(super) struct Contracts {
    func: FunctionId,
    mut_param: Option<BindingId>,
}

impl<'a, 'f, 'db> MirLowerCtx<'a, 'f, 'db> {
    /// From `lower_root`: whether this body checks its clauses, which only the body of a
    /// function with clauses in a crate with [`cfg::CONTRACT_CHECKS`] does. A closure, a
    /// constant and every body of a crate without the atom lowers as before.
    pub(super) fn start_contract_checks(
        &mut self,
        params: impl Iterator<Item = PatId>,
        self_param: Option<BindingId>,
    ) {
        let Some(func) = self.owner.as_function() else { return };
        let store = self.store;
        if store.spec_clauses().is_empty() {
            return;
        }
        if !self.krate().cfg_options(self.db).contract_checks() {
            return;
        }
        let is_mut = |binding: BindingId| {
            matches!(store[binding].mode, BindingAnnotation::Mutable | BindingAnnotation::RefMut)
        };
        let mut mut_param = self_param.filter(|&it| is_mut(it));
        for pat in params {
            store.walk_bindings_in_pat(pat, |binding| {
                if mut_param.is_none() && is_mut(binding) {
                    mut_param = Some(binding);
                }
            });
        }
        self.contracts = Some(Contracts { func, mut_param });
    }

    /// Whether a `return` of this body tests `ensures` clauses.
    pub(super) fn checks_ensures(&self) -> bool {
        self.contracts.is_some()
            && self.store.spec_clauses().iter().any(|&(_, kind)| kind == SpecClauseKind::Ensures)
    }

    /// The clause group `group` of kind `kind` as a statement: the checks of a `requires`
    /// when this body checks its clauses, nothing otherwise.
    pub(super) fn lower_clause_group(
        &mut self,
        group: ExprId,
        kind: SpecClauseKind,
        current: BasicBlockId,
    ) -> Result<'db, Option<BasicBlockId>> {
        let Some(Contracts { func, .. }) = self.contracts else { return Ok(Some(current)) };
        if kind != SpecClauseKind::Requires {
            return Ok(Some(current));
        }
        let Expr::Tuple { exprs } = self.store.expr(group) else {
            let path = self.fn_path(func);
            implementation_error!("a `requires` group of `{path}` is not a tuple");
        };
        let mut current = current;
        for &clause in exprs {
            let Some(next) = self.check_clause(func, SpecClauseKind::Requires, clause, current)?
            else {
                return Ok(None);
            };
            current = next;
        }
        Ok(Some(current))
    }

    /// The checks of every `ensures` of this body, with its value in the return slot, at a
    /// `return` or at the end of the body.
    pub(super) fn lower_ensures_checks(
        &mut self,
        mut current: BasicBlockId,
    ) -> Result<'db, Option<BasicBlockId>> {
        let Some(Contracts { func, .. }) = self.contracts else { return Ok(Some(current)) };
        let store = self.store;
        for &(group, kind) in store.spec_clauses() {
            if kind != SpecClauseKind::Ensures {
                continue;
            }
            let exprs = match store.expr(group) {
                Expr::Tuple { exprs } => exprs,
                Expr::Closure { args, body, .. } => {
                    let path = self.fn_path(func);
                    let [named] = args else {
                        let count = args.len();
                        implementation_error!("the `ensures` of `{path}` names {count} returns");
                    };
                    let Pat::Bind { id, subpat: None } = store.pat(*named) else {
                        not_supported!(
                            "checking the `ensures` of `{path}` at run time: its named return is \
                             a pattern, not one name"
                        );
                    };
                    self.set_binding_local(id, return_slot());
                    // The group owns what its clauses bind (`Ok(k) =>`), so the body did not
                    // pick them: each is declared where the clause binds it, as the bindings
                    // of a `const { .. }` lowered in place are.
                    for (binding, _) in store.bindings() {
                        if store.binding_owner(binding) == Some(group)
                            && self.local_of_binding(binding).is_none()
                        {
                            self.undeclared.insert(binding);
                        }
                    }
                    let Expr::Tuple { exprs } = store.expr(body) else {
                        implementation_error!("an `ensures` group of `{path}` is not a tuple");
                    };
                    exprs
                }
                _ => {
                    let path = self.fn_path(func);
                    implementation_error!(
                        "an `ensures` group of `{path}` is neither a tuple nor a closure"
                    );
                }
            };
            if !exprs.is_empty() {
                self.refuse_mut_param()?;
            }
            for &clause in exprs {
                let Some(next) =
                    self.check_clause(func, SpecClauseKind::Ensures, clause, current)?
                else {
                    return Ok(None);
                };
                current = next;
            }
        }
        Ok(Some(current))
    }

    /// A `become` leaves the function without its value passing a return: refused where the
    /// function's `ensures` is checked.
    pub(super) fn refuse_become_with_ensures(&self) -> Result<'db, ()> {
        if let Some(Contracts { func, .. }) = self.contracts
            && self.checks_ensures()
        {
            let path = self.fn_path(func);
            not_supported!(
                "`become` in `{path}`, whose `ensures` is checked at run time (`contract-checks`): \
                 the tail call leaves before the clause can be tested"
            );
        }
        Ok(())
    }

    /// An `ensures` reads the parameters as they are at the return; one declared `mut` may
    /// have changed since the call, and a proof reads it as it was passed.
    fn refuse_mut_param(&self) -> Result<'db, ()> {
        let Some(Contracts { func, mut_param: Some(binding) }) = self.contracts else {
            return Ok(());
        };
        let path = self.fn_path(func);
        let name = self.store[binding].name.as_str();
        not_supported!(
            "checking the `ensures` of `{path}` at run time: its parameter `{name}` is `mut`, so \
             the clause would read it as the body left it, not as it was passed"
        );
    }

    /// Tests `clause`, of kind `kind`, in `current`: where it holds the lowering goes on,
    /// where it does not `panic_nounwind` aborts naming it.
    fn check_clause(
        &mut self,
        func: FunctionId,
        kind: SpecClauseKind,
        clause: ExprId,
        current: BasicBlockId,
    ) -> Result<'db, Option<BasicBlockId>> {
        let span = MirSpan::from(clause);
        let before = (
            self.building.blocks.len(),
            self.block(current).statements.len(),
            self.building.locals.len(),
        );
        let scope = self.push_drop_scope();
        let lowered = self.lower_let_chain(clause, current, false)?;
        if kind == SpecClauseKind::Ensures {
            self.ensures_locals.push((before.2, self.building.locals.len()));
            self.refuse_moves_out(func, clause, current, before.0, before.1)?;
        }
        let Some((holds, fails)) = lowered else {
            scope.pop_assume_dropped(self);
            return Ok(None);
        };
        let message = self.broken_message(func, kind, clause);
        self.abort_with(fails, &message, span)?;
        Ok(Some(scope.pop_and_drop(self, holds, span)))
    }

    /// An `ensures` reads what the function returns and what it was given: it runs before the
    /// caller gets the value, so a clause that moved out of the return slot (`match r { Ok(k)
    /// => .. }` of a `k` that is not `Copy`, `consume(r)`) would drop a part of it and hand
    /// the caller the rest. Refused, naming what is moved; a clause moves only what it made
    /// itself (`ensures_locals`). The statements looked at are the clause's: those of
    /// `current` from `statements` on, and every block from `blocks` on. Lowering writes
    /// `Copy` for every read and `mark_moves` says `Move` by the type alone, so a read of a
    /// place whose type is not `Copy` is a move.
    fn refuse_moves_out(
        &mut self,
        func: FunctionId,
        clause: ExprId,
        current: BasicBlockId,
        blocks: usize,
        statements: usize,
    ) -> Result<'db, ()> {
        let mut read = Vec::new_in(self.frame);
        let mut note = |operand: &Operand| {
            if let OperandKind::Copy(place) | OperandKind::Move(place) = &operand.kind {
                // clone: a `StoredPlace` is a local and an interned projection handle
                read.push(place.clone());
            }
        };
        let own = framed::block_index(current);
        for (index, block) in self.building.blocks.iter().enumerate() {
            let from = match index {
                index if index == own => statements,
                index if index >= blocks => 0,
                _ => continue,
            };
            for statement in &block.statements[from..] {
                statement.kind.for_each_operand(&mut note);
            }
            if let Some(terminator) = &block.terminator {
                terminator.kind.for_each_operand(&mut note);
            }
        }
        for place in read {
            let local = framed::local_index(place.local);
            if self.ensures_locals.iter().any(|&(from, to)| (from..to).contains(&local)) {
                continue;
            }
            let ty = self.place_ty(&place.as_ref());
            if self.is_copy(ty) {
                continue;
            }
            let path = self.fn_path(func);
            let text = hir_def::expr_store::pretty::print_expr_hir(
                self.db,
                self.store,
                self.store_owner,
                clause,
                self.edition(),
            );
            let named = self.store.bindings().find_map(|(binding, data)| {
                (self.local_of_binding(binding) == Some(place.local)).then(|| data.name.as_str())
            });
            let what = match named {
                Some(name) => format!("`{name}`"), // alloc: error path
                None if place.local == return_slot() => "the value returned".to_owned(), // alloc: error path
                None => "a value of the body".to_owned(), // alloc: error path
            };
            not_supported!(
                "checking the `ensures` of `{path}` at run time: the clause `{text}` moves out \
                 of {what}, which is still used after the clause: read it by reference (`match \
                 &r {{ .. }}`, a `ref` binding, a method that takes `&self`)"
            );
        }
        Ok(())
    }

    /// `block` ends in `panic_nounwind(message)`, which never returns and never unwinds.
    fn abort_with(&mut self, block: BasicBlockId, message: &str, span: MirSpan) -> Result<'db, ()> {
        let Some(panic) = self.lang_items().PanicNounwind else {
            not_supported!(
                "a run-time contract check: core has no `panic_nounwind` lang item to abort with"
            );
        };
        let interner = self.interner();
        let str_ref = Ty::new_ref(
            interner,
            self.types.regions.statik,
            Ty::new(interner, TyKind::Str),
            Mutability::Not,
        );
        let mut argument =
            self.lower_literal_to_operand(str_ref, &Literal::String(Symbol::intern(message)))?;
        argument.span = Some(span);
        let func = Operand::const_zst(Ty::new_fn_def(
            interner,
            CallableDefId::FunctionId(panic).into(),
            GenericArgs::new_from_slice(&[]),
        ));
        let never = self.temp(self.types.types.never, block, span)?;
        self.set_terminator(
            block,
            TerminatorKind::Call {
                func,
                // alloc: the call's argument, kept in the body
                args: Box::new([argument]),
                destination: Place::from(never).store(),
                target: None,
                cleanup: None,
                from_hir_call: false,
                intrinsic: None,
            },
            span,
        );
        Ok(())
    }

    /// What the abort of a broken `clause` says: the function, the clause as written, and
    /// where.
    #[expect(
        blaze::reads_positions,
        reason = "a broken contract's abort message carries the clause's file, line and column into the \
                  lowered MIR, so the body's MIR changes on an edit above it: the place must come from a \
                  `Location` the link fills, as `line!()`'s does (`location::slots_lines`)"
    )]
    fn broken_message(&self, func: FunctionId, kind: SpecClauseKind, clause: ExprId) -> String {
        let path = self.fn_path(func);
        let text = hir_def::expr_store::pretty::print_expr_hir(
            self.db,
            self.store,
            self.store_owner,
            clause,
            self.edition(),
        );
        let place = location::source_location_in_file(
            self.db,
            DefWithBodyId::FunctionId(func),
            MirSpan::from(clause),
        )
        .and_then(|(file, line, column)| {
            let krate = hir_def::HasModule::krate(&func, self.db);
            Some(format!(" ({}:{line}:{column})", location::source_path(self.db, krate, file)?))
        })
        .unwrap_or_default();
        match kind {
            SpecClauseKind::Requires => {
                format!("`{path}` was called with its contract broken: requires `{text}`{place}")
            }
            _ => format!("`{path}` broke its contract: ensures `{text}` on return{place}"),
        }
    }

    /// `func` as a backtrace names it, or its own name where that spelling is not built.
    fn fn_path(&self, func: FunctionId) -> String {
        hir_ty_print::rustc_function_name(self.db, func, &[]).unwrap_or_else(|| {
            hir_def::signatures::FunctionSignature::of(self.db, func).name.as_str().to_owned() // alloc: a name
        })
    }
}
