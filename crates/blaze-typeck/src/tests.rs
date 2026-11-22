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
use crate::world::{HolePlant, Needs, Question};
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
            segment_ranges: &[],
            types: PTys::EMPTY,
            type_lists: &[],
            type_list_ranges: &[],
            method_name_ranges: &[],
            opt_types: &[],
            fmt_uses: &[],
            coroutine: None,
            super_lets: &[],
            asm_pieces: &[],
            asm_operands: &[],
            asm_clobbers: &[],
            asm_text: "",
            call_arg_ints: &[],
            unsafe_blocks: &[],
            expr_editions: &[],
            pat_editions: &[],
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

/// `run` with a planted bug of the hole tests.
fn run_holes(case: &Case, w: &ToyWorld, hints: &Needs<'_>, plant: HolePlant) -> Result<Outcome<'static>, CheckError> {
    let mut ty = InferCtx::new_in(Alloc::global());
    let body = case.body.view(w.owner, case.root);
    crate::check_hole_plant(&body, w, &mut ty, hints, plant, Alloc::global())
}

#[test]
fn a_need_then_reentry_equals_one_run() {
    blaze_sim::check(Config::cases(300), |tape: &Tape| reentry_property(tape, HolePlant::None));
}

/// The oracle of rounds: a record made after N rounds over a world that withholds records is
/// byte for byte the record of one run over the world that had them all. Its control lets the
/// hints (an earlier round's questions) into the read list.
#[test]
fn the_rounds_oracle_catches_an_earlier_read_kept() {
    blaze_sim::control(Config::cases(300), HolePlant::None, HolePlant::KeepEarlierReads, "re-entry differs", reentry_property);
}

/// A run that met a hole makes no result. Its control makes a result from the stand-ins.
#[test]
fn the_rounds_oracle_catches_an_answer_through_a_hole() {
    blaze_sim::control(Config::cases(300), HolePlant::None, HolePlant::AnswerThroughHoles, "re-entry differs", reentry_property);
}

fn reentry_property(tape: &Tape, plant: HolePlant) -> Result<(), Broke> {
    {
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
            match run_holes(&case, &w, &hints, plant) {
                Ok(Outcome::Done(c)) => {
                    return if c == full {
                        Ok(())
                    } else {
                        Err(Broke::Oracle(format!("re-entry differs from one run:\n{full:?}\n{c:?}")))
                    };
                }
                Ok(Outcome::Need(n)) => {
                    let missed = n.missing.iter().filter(|q| w.missing.contains(q)).count();
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
    }
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
    // Room for the control's lost rounds (at most the prediction, itself at most this).
    let bound = 2 * (full.reads.questions().len() as u32 + 2);
    for entries in 1..=bound {
        match run(&case, &w, &hints, Plant::None) {
            Ok(Outcome::Done(_)) => {
                // The log models a run that stops at its first missing answer (asks.rs
                // `Order::Pull`). A run goes on past a hole (world.rs `Hole`): it is the same run
                // up to its first hole and fetches at least what the model fetches, so the model
                // is a bound, never exceeded.
                return if entries <= predicted.entries {
                    Ok(())
                } else {
                    Err(Broke::Oracle(format!(
                        "the log predicted at most {} entries, re-entry took {entries}",
                        predicted.entries
                    )))
                };
            }
            Ok(Outcome::Need(n)) => {
                // The control loses the fetches of as many rounds as the bound allows: the next
                // round past them still misses, so the bound is exceeded.
                let lose = matches!(hints_mode, Hints::LoseFirstFetch) && entries <= predicted.entries;
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
        m.diag(DiagCode::Mismatch, At::Body, crate::diag::About::found(var));
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
        let unit = m.common.unit;
        m.diag(code, At::Body, crate::diag::About::found(unit));
        match m.writeback() {
            Ok(crate::Outcome::Done(c)) => Ok(c.diags.len()),
            Ok(crate::Outcome::Need(n)) => panic!("writeback met a hole: {:?}", n.missing),
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

/// Five structs, a parameter of each, and `let a_k = p_k.f0;` for each: five independent
/// records the body reads (typeck.md, "Speed on real code": rounds).
fn five_fields() -> Case {
    let mut structs = Vec::new_in(Global);
    let mut params = Vec::new_in(Global);
    let mut body = BodyBuf::new(Edition::E2021);
    let mut stmt_ids: Vec<StmtId, Global> = Vec::new_in(Global);
    for k in 0..5u32 {
        let mut fields = Vec::new_in(Global);
        fields.push((field_name(0), FieldTy::I32));
        structs.push(ToyStruct { fields });
        params.push(FieldTy::Struct(k));
        let p = body.bind(u64::from(k));
        body.params.push(p);
    }
    for k in 0..5u32 {
        let base = body.expr(Expr::Local(body.params[k as usize]));
        let e = body.expr(Expr::Field { base, name: field_name(0) });
        let a = body.bind(1000 + u64::from(k));
        stmt_ids.push(StmtId(body.stmts.len() as u32));
        body.stmts.push(Stmt::Let { pat: a, ty: None, init: Some(e), else_: None });
    }
    let start = body.stmt_lists.len() as u32;
    body.stmt_lists.extend_from_slice(&stmt_ids);
    let tail = Some(body.expr(Expr::Lit(Lit::Int(None))));
    let root = body.expr(Expr::Block { stmts: List { start, len: stmt_ids.len() as u32 }, tail, scope: ScopeId(0) });
    Case { structs, params, body, root }
}

/// The world of `five_fields` with the five structs' records withheld.
fn five_withheld(case: &Case) -> ToyWorld {
    let mut w = world(case);
    for k in 0..5u32 {
        w.missing.push(Question::Adt(struct_key(k)));
    }
    w
}

/// Runs `case` over `w` until it is done, fetching each round's missing records; the rounds.
fn rounds_to_done(case: &Case, w: &mut ToyWorld, plant: Plant) -> Result<(Checked<'static>, u32), String> {
    let mut hints = Needs::new_in(Alloc::global());
    for round in 1..=8u32 {
        match run(case, w, &hints, plant) {
            Ok(Outcome::Done(c)) => return Ok((c, round)),
            Ok(Outcome::Need(n)) => {
                let before = w.missing.len();
                w.missing.retain(|q| !n.questions.contains(q));
                if w.missing.len() == before {
                    return Err(format!("round {round} needed nothing withheld: {:?}", n.questions));
                }
                hints = n;
            }
            Err(e) => return Err(format!("round {round} failed: {e}")),
        }
    }
    Err(String::from("not done in 8 rounds"))
}

#[test]
fn five_missing_records_come_back_in_one_round() {
    let case = five_fields();
    let w = five_withheld(&case);
    let n = match run(&case, &w, &Needs::new_in(Alloc::global()), Plant::None) {
        Ok(Outcome::Need(n)) => n,
        Ok(Outcome::Done(c)) => panic!("a check over a world missing five records made a result: {c:?}"),
        Err(e) => panic!("the first round failed: {e}"),
    };
    let named: Vec<Question, Global> = {
        let mut v = Vec::new_in(Global);
        v.extend(n.questions.iter().copied().filter(|q| w.missing.contains(q)));
        v
    };
    assert_eq!(named.len(), 5, "the first round named {} of the 5 missing records: {:?}", named.len(), n.questions);
    assert_eq!(&n.missing[..], &w.missing[..], "the holes are the five withheld records, sorted");
    assert_eq!(n.first, Some(Question::Adt(struct_key(0))), "the first hole is the first field's struct");
    let mut w = five_withheld(&case);
    let (c, rounds) = match rounds_to_done(&case, &mut w, Plant::None) {
        Ok(v) => v,
        Err(e) => panic!("{e}"),
    };
    assert_eq!(rounds, 2, "five independent records took {rounds} rounds");
    assert!(c.diags.is_empty(), "the second round was not clean: {:?}", c.diags);
}

/// No diagnostic leaves a run that met a hole; the control answers through the stand-ins,
/// where a field of a struct whose record is a hole reads as no field.
#[test]
fn a_hole_never_makes_a_diagnostic() {
    let case = five_fields();
    let w = five_withheld(&case);
    let first = |plant| run_holes(&case, &w, &Needs::new_in(Alloc::global()), plant);
    match first(HolePlant::None) {
        Ok(Outcome::Need(n)) => assert_eq!(n.missing.len(), 5),
        Ok(Outcome::Done(c)) => panic!("a holed run made a result: {c:?}"),
        Err(e) => panic!("a holed run failed: {e}"),
    }
    match first(HolePlant::AnswerThroughHoles) {
        Ok(Outcome::Done(c)) => assert!(!c.diags.is_empty(), "the control made no diagnostic: the test shows nothing"),
        Ok(Outcome::Need(n)) => panic!("the control still needed {:?}", n.missing),
        Err(e) => panic!("the control failed: {e}"),
    }
}

/// Rule 20 (infer/mod.rs `unreadable`): a handle the context cannot read is viewed as the error
/// type and noted, taken once by `take_malformed`, where it was a panic of the process ("node 6
/// is not a type", tempo g090627: 744 bodies lost with it). The control: a handle the context
/// made reads clean and notes nothing.
#[test]
fn an_unreadable_handle_fails_its_body_not_the_process() {
    use crate::error::{Malformed, Unreadable};
    use crate::ty::{Ty, TyView, TypeCtx};
    let mut cx = InferCtx::new_in(Alloc::global());
    let empty = cx.mk_list(&[]).expect("the empty list");
    let unit = cx.mk(TyView::Tuple(empty)).expect("the unit type");
    assert_ne!(cx.view(unit), TyView::Error);
    assert_eq!(cx.take_malformed(), None, "a handle the context made noted a failure");
    let past = Ty(u32::MAX >> 3);
    assert_eq!(cx.view(past), TyView::Error);
    assert_eq!(cx.take_malformed(), Some(Malformed::Unreadable { node: past.0, what: Unreadable::NotInArena }));
    assert_eq!(cx.take_malformed(), None, "the note is taken once");
}
