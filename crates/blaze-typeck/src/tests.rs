//! The driver's properties, through `blaze_sim::check` on generated bodies over generated toy
//! worlds (typeck.md, "Testing"):
//!
//! - determinism: two runs of one body over one world give equal records, bytes and read list;
//! - the read-list property: changing an answer the read list does not name changes nothing,
//!   with a planted control (`Plant::PeekFields`, a field lookup that reads its ADT unrecorded)
//!   that the oracle must catch;
//! - re-entry: a run stopped by `Need`, fetched and run again with the needs as hints, ends
//!   equal to a run that never stopped;
//! - depth: 100,000 nested parentheses and 100,000 nested binary operators check without
//!   growing the native stack.

use blaze_own::{Alloc, Vec};
use blaze_sim::{Broke, Config, Tape};
use std::alloc::Global;
use std::format;
use std::string::String;

use crate::body::{BinOp, BindAnn, BodyView, Expr, IntSuffix, Lit, Pat, Stmt};
use crate::columns::Checked;
use crate::error::CheckError;
use crate::ids::{Edition, ExprId, IntTy, ItemKey, List, Name, PatId, Range, ScopeId, StmtId};
use crate::machine::Plant;
use crate::portable::PTys;
use crate::infer::InferCtx;
use crate::toy::{FieldTy, ToyStruct, ToyWorld, struct_key};
use crate::world::{Needs, Question};
use crate::{Outcome, check_with};

/// A body under construction: owned columns a `BodyView` borrows.
struct BodyBuf {
    exprs: Vec<Expr, Global>,
    expr_ranges: Vec<Range, Global>,
    pats: Vec<Pat, Global>,
    pat_ranges: Vec<Range, Global>,
    stmts: Vec<Stmt, Global>,
    stmt_lists: Vec<StmtId, Global>,
    params: Vec<PatId, Global>,
    edition: Edition,
}

impl BodyBuf {
    fn new(edition: Edition) -> BodyBuf {
        BodyBuf {
            exprs: Vec::new_in(Global),
            expr_ranges: Vec::new_in(Global),
            pats: Vec::new_in(Global),
            pat_ranges: Vec::new_in(Global),
            stmts: Vec::new_in(Global),
            stmt_lists: Vec::new_in(Global),
            params: Vec::new_in(Global),
            edition,
        }
    }

    fn expr(&mut self, e: Expr) -> ExprId {
        let at = self.exprs.len() as u32;
        self.exprs.push(e);
        self.expr_ranges.push(Range { start: at, end: at + 1 });
        ExprId(at)
    }

    fn bind(&mut self, name: u64) -> PatId {
        let at = self.pats.len() as u32;
        self.pats.push(Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name(name), sub: None });
        self.pat_ranges.push(Range { start: at, end: at + 1 });
        PatId(at)
    }

    fn view(&self, owner: ItemKey, root: ExprId) -> BodyView<'_> {
        BodyView {
            owner,
            edition: self.edition,
            params: &self.params,
            root,
            exprs: &self.exprs,
            expr_ranges: &self.expr_ranges,
            pats: &self.pats,
            pat_ranges: &self.pat_ranges,
            stmts: &self.stmts,
            expr_lists: &[],
            pat_lists: &[],
            stmt_lists: &self.stmt_lists,
            arms: &[],
            field_inits: &[],
            field_pats: &[],
            paths: &[],
            segments: &[],
            types: PTys::EMPTY,
            type_lists: &[],
            opt_types: &[],
            fmt_uses: &[],
            coroutine: None,
        }
    }
}

/// A generated case: the structs, the owner's parameters, and the body.
struct Case {
    structs: Vec<ToyStruct, Global>,
    params: Vec<FieldTy, Global>,
    body: BodyBuf,
    root: ExprId,
}

fn field_name(j: u32) -> Name {
    Name(100 + u64::from(j))
}

fn draw_case(tape: &Tape) -> Case {
    let n_structs = tape.range(1..4, "structs") as u32;
    let mut structs = Vec::new_in(Global);
    for i in 0..n_structs {
        let n_fields = tape.range(1..4, "fields") as u32;
        let mut fields = Vec::new_in(Global);
        for j in 0..n_fields {
            let ty = match tape.below(if i == 0 { 3 } else { 4 }, "field type") {
                0 => FieldTy::I32,
                1 => FieldTy::U8,
                2 => FieldTy::Bool,
                _ => FieldTy::Struct(tape.below(u64::from(i), "inner struct") as u32),
            };
            fields.push((field_name(j), ty));
        }
        structs.push(ToyStruct { fields });
    }
    let edition = if tape.coin("edition 2024") { Edition::E2024 } else { Edition::E2021 };
    let mut body = BodyBuf::new(edition);
    let n_params = tape.range(1..3, "params") as u32;
    let mut params = Vec::new_in(Global);
    // Each binding and, for a struct-typed one, its struct.
    let mut locals: Vec<(PatId, Option<u32>), Global> = Vec::new_in(Global);
    for k in 0..n_params {
        let s = tape.below(u64::from(n_structs), "param struct") as u32;
        params.push(FieldTy::Struct(s));
        let p = body.bind(u64::from(k));
        body.params.push(p);
        locals.push((p, Some(s)));
    }
    let mut stmt_ids: Vec<StmtId, Global> = Vec::new_in(Global);
    let mut last = None;
    let lets = tape.below(8, "lets");
    for k in 0..lets {
        let (e, s) = draw_expr(tape, &mut body, &locals, &structs);
        let p = body.bind(1000 + k);
        let st = StmtId(body.stmts.len() as u32);
        body.stmts.push(Stmt::Let { pat: p, ty: None, init: Some(e), else_: None });
        stmt_ids.push(st);
        locals.push((p, s));
        last = Some(p);
    }
    let start = body.stmt_lists.len() as u32;
    body.stmt_lists.extend_from_slice(&stmt_ids);
    let tail = match last {
        Some(p) if tape.coin("tail is the last let") => Some(body.expr(Expr::Local(p))),
        _ => Some(body.expr(Expr::Lit(Lit::Int(None)))),
    };
    let root = body.expr(Expr::Block {
        stmts: List { start, len: stmt_ids.len() as u32 },
        tail,
        scope: ScopeId(0),
    });
    Case { structs, params, body, root }
}

/// One expression over the locals so far, and the struct it has when it is a struct.
fn draw_expr(
    tape: &Tape,
    body: &mut BodyBuf,
    locals: &[(PatId, Option<u32>)],
    structs: &[ToyStruct],
) -> (ExprId, Option<u32>) {
    let (p, s) = locals[tape.index(locals.len(), "local")];
    match tape.below(6, "expr") {
        0 => {
            let suffix = if tape.coin("suffix") { Some(IntSuffix::Int(IntTy::I64)) } else { None };
            (body.expr(Expr::Lit(Lit::Int(suffix))), None)
        }
        1 => (body.expr(Expr::Lit(Lit::Bool(tape.coin("bool")))), None),
        2 => (body.expr(Expr::Local(p)), s),
        3 => {
            // A field of the local's struct (a name it may lack), then perhaps a field of that.
            let mut base = body.expr(Expr::Local(p));
            let mut at = s;
            for _ in 0..1 + tape.below(2, "field depth") {
                let j = tape.below(4, "field") as u32;
                base = body.expr(Expr::Field { base, name: field_name(j) });
                at = at.and_then(|i| {
                    structs[i as usize].fields.iter().find(|(n, _)| *n == field_name(j)).and_then(|(_, t)| match *t {
                        FieldTy::Struct(k) => Some(k),
                        _ => None,
                    })
                });
            }
            (base, at)
        }
        4 => {
            let lhs = body.expr(Expr::Local(p));
            let rhs = body.expr(Expr::Lit(Lit::Int(None)));
            let op = if tape.coin("eq") { BinOp::Eq } else { BinOp::Add };
            (body.expr(Expr::Binary { op, lhs, rhs }), None)
        }
        _ => {
            let inner = body.expr(Expr::Local(p));
            (body.expr(Expr::Paren(inner)), s)
        }
    }
}

fn world(case: &Case) -> ToyWorld {
    match ToyWorld::new(&case.structs, &case.params, FieldTy::I32) {
        Ok(w) => w,
        Err(e) => panic!("the toy world could not be built: {e}"),
    }
}

fn run(case: &Case, w: &ToyWorld, hints: &Needs<'_>, plant: Plant) -> Result<Outcome<'static>, CheckError> {
    let mut ty = InferCtx::new_in(Alloc::global());
    let body = case.body.view(w.owner, case.root);
    check_with(&body, w, &mut ty, hints, plant, Alloc::global())
}

fn done(case: &Case, w: &ToyWorld, plant: Plant) -> Result<Checked<'static>, Broke> {
    match run(case, w, &Needs::new_in(Alloc::global()), plant) {
        Ok(Outcome::Done(c)) => Ok(c),
        Ok(Outcome::Need(n)) => Err(Broke::Harness(format!("a full world was missing {:?}", n.questions))),
        Err(e) => Err(Broke::Oracle(format!("the check failed: {e}"))),
    }
}

#[test]
fn a_check_is_deterministic() {
    blaze_sim::check(Config::cases(300), |tape: &Tape| -> Result<(), Broke> {
        let case = draw_case(tape);
        let w = world(&case);
        let a = done(&case, &w, Plant::None)?;
        let b = done(&case, &w, Plant::None)?;
        if a != b {
            return Err(Broke::Oracle(format!("two runs differ:\n{a:?}\n{b:?}")));
        }
        Ok(())
    });
}

/// Changes each struct the read list does not name and checks again: the result must not move.
fn read_list_property(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let case = draw_case(tape);
    let w = world(&case);
    let first = done(&case, &w, plant)?;
    for i in 0..case.structs.len() as u32 {
        if first.reads.holds(&Question::Adt(struct_key(i))) {
            continue;
        }
        let mut changed = world(&case);
        changed.perturb(i);
        let again = done(&case, &changed, plant)?;
        if !first.same_result(&again) {
            return Err(Broke::Oracle(format!(
                "the read list missed struct {i}: changing it changed the result\n{first:?}\n{again:?}"
            )));
        }
    }
    Ok(())
}

#[test]
fn an_answer_not_read_changes_nothing() {
    blaze_sim::check(Config::cases(300), |tape: &Tape| read_list_property(tape, Plant::None));
}

#[test]
fn the_oracle_catches_an_unrecorded_read() {
    blaze_sim::control(Config::cases(300), Plant::None, Plant::PeekFields, "read list missed", read_list_property);
}

#[test]
fn a_need_then_reentry_equals_one_run() {
    blaze_sim::check(Config::cases(300), |tape: &Tape| -> Result<(), Broke> {
        let case = draw_case(tape);
        let full = done(&case, &world(&case), Plant::None)?;
        let mut w = world(&case);
        for &q in full.reads.questions() {
            if tape.coin("hold back") {
                w.missing.push(q);
            }
        }
        let mut hints = Needs::new_in(Alloc::global());
        // Each stop fetches at least the question it missed.
        for _ in 0..=full.reads.questions().len() + 1 {
            match run(&case, &w, &hints, Plant::None) {
                Ok(Outcome::Done(c)) => {
                    return if c == full {
                        Ok(())
                    } else {
                        Err(Broke::Oracle(format!("re-entry differs from one run:\n{full:?}\n{c:?}")))
                    };
                }
                Ok(Outcome::Need(n)) => {
                    let missed = n.questions.iter().filter(|q| w.missing.contains(q)).count();
                    if missed == 0 {
                        return Err(Broke::Oracle(format!("a need named nothing missing: {:?}", n.questions)));
                    }
                    let mut kept = Vec::new_in(Global);
                    for &q in w.missing.iter() {
                        if !n.questions.contains(&q) {
                            kept.push(q);
                        }
                    }
                    w.missing = kept;
                    hints = n;
                }
                Err(e) => return Err(Broke::Oracle(format!("a re-entered run failed: {e}"))),
            }
        }
        Err(Broke::Oracle(String::from("re-entry did not end within its bound")))
    });
}

/// A clock that counts its reads: deterministic nanoseconds for the ask log.
struct Ticks(core::cell::Cell<u64>);

impl crate::asks::Clock for Ticks {
    fn now_ns(&self) -> u64 {
        let t = self.0.get();
        self.0.set(t + 1);
        t
    }
}

/// The re-entry loop's fetch: every question a need names (`Kept`), or, planted, the first
/// round's missed question lost once (a fetch that did not land).
#[derive(Clone, Copy, Debug)]
enum Hints {
    Kept,
    LoseFirstFetch,
}

/// The ask log's prediction equals re-entry run for real: a cold body (every question held
/// back) under pull order runs exactly `restarts(.., Order::Pull).entries` times, and the
/// logged run's outcome equals the unlogged one's.
fn restarts_property(tape: &Tape, hints_mode: Hints) -> Result<(), Broke> {
    let case = draw_case(tape);
    let full = done(&case, &world(&case), Plant::None)?;
    let w0 = world(&case);
    let clock = Ticks(core::cell::Cell::new(0));
    let view = case.body.view(w0.owner, case.root);
    let mut ty = InferCtx::new_in(Alloc::global());
    let (o, log) = match crate::check_logged(&view, &w0, &mut ty, &Needs::new_in(Alloc::global()), &clock, Alloc::global()) {
        Ok(v) => v,
        Err(e) => return Err(Broke::Oracle(format!("the logged check failed: {e}"))),
    };
    match o {
        Outcome::Done(c) if c == full => {}
        _ => return Err(Broke::Oracle(String::from("the logged run's outcome differs from the unlogged one"))),
    }
    if log.asks.iter().any(|a| a.missing) {
        return Err(Broke::Oracle(String::from("a full world's log has a missing ask")));
    }
    let predicted = crate::asks::restarts(&log, crate::asks::Order::Pull, Alloc::global());
    let mut w = world(&case);
    for &q in full.reads.questions() {
        w.missing.push(q);
    }
    let mut hints = Needs::new_in(Alloc::global());
    let bound = full.reads.questions().len() as u32 + 2;
    for entries in 1..=bound {
        match run(&case, &w, &hints, Plant::None) {
            Ok(Outcome::Done(_)) => {
                return if entries == predicted.entries {
                    Ok(())
                } else {
                    Err(Broke::Oracle(format!(
                        "the log predicted {} entries, re-entry took {entries}",
                        predicted.entries
                    )))
                };
            }
            Ok(Outcome::Need(n)) => {
                let lose = matches!(hints_mode, Hints::LoseFirstFetch) && entries == 1;
                let mut kept = Vec::new_in(Global);
                for &q in w.missing.iter() {
                    if !n.questions.contains(&q) || lose {
                        kept.push(q);
                    }
                }
                w.missing = kept;
                hints = n;
            }
            Err(e) => return Err(Broke::Oracle(format!("a re-entered run failed: {e}"))),
        }
    }
    Err(Broke::Oracle(String::from("re-entry did not end within its bound")))
}

#[test]
fn the_ask_log_predicts_reentry() {
    blaze_sim::check(Config::cases(300), |tape: &Tape| restarts_property(tape, Hints::Kept));
}

/// The control: a first fetch that did not land costs one more entry than the log predicts.
#[test]
fn the_ask_log_control_catches_a_lost_fetch() {
    blaze_sim::control(
        Config::cases(300),
        Hints::Kept,
        Hints::LoseFirstFetch,
        "the log predicted",
        restarts_property,
    );
}

/// What a probe or a failed `commit_if` may leave behind: a diagnostic, an error count, a
/// queued variable, a pending obligation, a variable's binding. Each is written inside the
/// snapshot and must be gone after; the committed case shows the test sees each write.
#[test]
fn a_rolled_back_snapshot_leaves_nothing_behind() {
    use crate::diag::{At, DiagCode};
    use crate::error::Stop;
    use crate::machine::Machine;
    use crate::ty::{Cause, Goal, Obligation, TypeCtx};
    use crate::world::Reads;

    let mut body = BodyBuf::new(Edition::E2021);
    let root = body.expr(Expr::Lit(Lit::Int(None)));
    let case = Case { structs: Vec::new_in(Global), params: Vec::new_in(Global), body, root };
    let w = world(&case);
    let view = case.body.view(w.owner, case.root);
    let mut ty = InferCtx::new_in(Alloc::global());
    let reads = Reads::new_in(&w, Alloc::global());
    let mut m = match Machine::new_in(view, reads, &mut ty, Plant::None, Alloc::global()) {
        Ok(m) => m,
        Err(e) => panic!("the machine could not be made: {e}"),
    };
    let var = match m.fresh() {
        Ok(v) => v,
        Err(e) => panic!("{e:?}"),
    };
    // Every undoable write a rule makes, then the variable's binding.
    let write = |m: &mut Machine<'_, '_, '_, '_, ToyWorld, InferCtx<'static>>| -> Result<Option<()>, Stop> {
        m.diag(DiagCode::Mismatch, At::Body, None, None, None);
        let n = m.fresh_int()?;
        let unit = m.common.unit;
        m.ty.register(Obligation { goal: Goal::WellFormed(unit), cause: Cause::Body })?;
        m.unify(var, n)?;
        Ok(Some(()))
    };
    let before = (m.diags.len(), m.errors, m.numeric.len(), m.ty.pending_len());
    let left = |m: &mut Machine<'_, '_, '_, '_, ToyWorld, InferCtx<'static>>| {
        // `var` is bound to `n`, itself an unbound integer variable, so "bound" is "resolves
        // to something else", not "resolves to a non-variable".
        let bound = m.ty.shallow_resolve(var) != var;
        (m.diags.len(), m.errors, m.numeric.len(), m.ty.pending_len(), bound)
    };

    let probed = m.probe(|m| write(m));
    assert!(matches!(probed, Ok(Some(()))));
    assert_eq!(left(&mut m), (before.0, before.1, before.2, before.3, false), "a probe left a write behind");

    let failed: Result<Option<()>, Stop> = m.commit_if(|m| {
        write(m)?;
        Ok(None)
    });
    assert!(matches!(failed, Ok(None)));
    assert_eq!(left(&mut m), (before.0, before.1, before.2, before.3, false), "a failed commit_if left a write behind");

    let kept = m.commit_if(|m| write(m));
    assert!(matches!(kept, Ok(Some(()))));
    assert_eq!(
        left(&mut m),
        (before.0 + 1, before.1 + 1, before.2 + 1, before.3 + 1, true),
        "a committed snapshot lost a write"
    );
}

/// The checker's invariant: a finding points at its place. A diagnostic that lost its place
/// (empty range) is `DiagWithoutPlace` naming it, unless its kind is about the whole item (a
/// missing lang item), which is kept.
#[test]
fn a_diagnostic_with_no_place_is_an_internal_error() {
    use crate::diag::{At, DiagCode};
    use crate::error::CheckError;
    use crate::machine::Machine;
    use crate::world::Reads;

    let mut body = BodyBuf::new(Edition::E2021);
    let root = body.expr(Expr::Lit(Lit::Int(None)));
    let case = Case { structs: Vec::new_in(Global), params: Vec::new_in(Global), body, root };
    let w = world(&case);
    let view = case.body.view(w.owner, case.root);
    let outcome = |code: DiagCode| {
        let mut ty = InferCtx::new_in(Alloc::global());
        let reads = Reads::new_in(&w, Alloc::global());
        let mut m = match Machine::new_in(view, reads, &mut ty, Plant::None, Alloc::global()) {
            Ok(m) => m,
            Err(e) => panic!("the machine could not be made: {e}"),
        };
        m.diag(code, At::Body, None, None, None);
        match m.writeback() {
            Ok(c) => Ok(c.diags.len()),
            Err(e) => Err(e),
        }
    };
    match outcome(DiagCode::Unsatisfied) {
        Err(CheckError::DiagWithoutPlace { code: DiagCode::Unsatisfied, name: None }) => {}
        other => panic!("an E0277 with an empty range passed: {other:?}"),
    }
    assert!(matches!(outcome(DiagCode::MissingLangItem), Ok(1)), "a whole-item finding was refused");
}

/// A body of `depth` nested nodes made by `wrap` around an integer literal.
fn deep(depth: u32, wrap: impl Fn(&mut BodyBuf, ExprId) -> ExprId) -> Result<(), String> {
    let mut body = BodyBuf::new(Edition::E2021);
    let mut e = body.expr(Expr::Lit(Lit::Int(None)));
    for _ in 0..depth {
        e = wrap(&mut body, e);
    }
    let structs = Vec::new_in(Global);
    let case = Case { structs, params: Vec::new_in(Global), body, root: e };
    let w = world(&case);
    match run(&case, &w, &Needs::new_in(Alloc::global()), Plant::None) {
        Ok(Outcome::Done(c)) if c.diags.is_empty() => Ok(()),
        Ok(Outcome::Done(c)) => Err(format!("unexpected diagnostics: {:?}", c.diags)),
        Ok(Outcome::Need(n)) => Err(format!("needed {:?}", n.questions)),
        Err(e) => Err(format!("{e}")),
    }
}

#[test]
fn deep_parens_do_not_grow_the_stack() {
    let got = deep(100_000, |b, e| b.expr(Expr::Paren(e)));
    assert_eq!(got, Ok(()));
}

#[test]
fn deep_binary_ops_do_not_grow_the_stack() {
    let got = deep(100_000, |b, lhs| {
        let rhs = b.expr(Expr::Lit(Lit::Int(None)));
        b.expr(Expr::Binary { op: BinOp::Add, lhs, rhs })
    });
    assert_eq!(got, Ok(()));
}
