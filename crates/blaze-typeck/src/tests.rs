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
        self.pats.push(Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name(name), sub: None, ty: None });
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
            legacy_args: &[],
            unsafe_blocks: &[],
            expr_editions: &[],
            pat_editions: &[],
            record_expr_schemas: &[],
            closure_static_params: &[],
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

/// The invariant: type checking is deterministic and changing any world answer not recorded in the read list changes nothing in the resulting record.
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

#[test]
fn record_schema_and_infer_non_unification() {
    use crate::ids::{IntTy, Name, RecordSchema, UintTy};
    use crate::ty::{TyView, TypeCtx, VarKind};

    let alloc = blaze_own::testing::heap().alloc();
    let mut cx = InferCtx::new_in(alloc);

    let schema_ab = RecordSchema::of_sorted_names(&[Name::of("a"), Name::of("b")]);
    let schema_xy = RecordSchema::of_sorted_names(&[Name::of("x"), Name::of("y")]);
    assert_eq!(RecordSchema::EMPTY, RecordSchema::of_sorted_names(&[]));
    assert_ne!(schema_ab, schema_xy);
    assert_ne!(schema_ab, RecordSchema::EMPTY);

    let i32_ty = cx.mk(TyView::Int(IntTy::I32)).expect("i32");
    let u8_ty = cx.mk(TyView::Uint(UintTy::U8)).expect("u8");
    let fields = cx.mk_list(&[i32_ty, u8_ty]).expect("fields");

    let tup = cx.mk(TyView::Tuple(fields)).expect("tuple");
    let rec_ab = cx.mk(TyView::Record(schema_ab, fields)).expect("rec_ab");
    let rec_xy = cx.mk(TyView::Record(schema_xy, fields)).expect("rec_xy");
    let uni = cx.mk(TyView::Union(fields)).expect("union");

    assert_eq!(cx.view(tup), TyView::Tuple(fields));
    assert_eq!(cx.view(rec_ab), TyView::Record(schema_ab, fields));
    assert_eq!(cx.view(rec_xy), TyView::Record(schema_xy, fields));
    assert_eq!(cx.view(uni), TyView::Union(fields));

    // Tuple, Record, and Union NEVER unify with each other; distinct RecordSchemas NEVER unify.
    assert!(!cx.unify(tup, rec_ab).expect("unify"));
    assert!(!cx.unify(tup, uni).expect("unify"));
    assert!(!cx.unify(rec_ab, uni).expect("unify"));
    assert!(!cx.unify(rec_ab, rec_xy).expect("unify"));

    // Same-schema Record unifies element-wise and binds variables.
    let v1 = cx.fresh(VarKind::Ty).expect("fresh v1");
    let rec_var_fields = cx.mk_list(&[v1, u8_ty]).expect("rec var fields");
    let rec_ab_var = cx.mk(TyView::Record(schema_ab, rec_var_fields)).expect("rec_ab_var");
    assert!(cx.unify(rec_ab, rec_ab_var).expect("unify same schema"));
    assert_eq!(cx.shallow_resolve(v1), i32_ty);

    // Union unifies element-wise and binds variables.
    let v2 = cx.fresh(VarKind::Ty).expect("fresh v2");
    let uni_var_fields = cx.mk_list(&[i32_ty, v2]).expect("uni var fields");
    let uni_var = cx.mk(TyView::Union(uni_var_fields)).expect("uni_var");
    assert!(cx.unify(uni, uni_var).expect("unify union"));
    assert_eq!(cx.shallow_resolve(v2), u8_ty);
}

/// Minimal allocator-backed world for unit-testing union coercions, union pattern narrowing,
/// and structural records without `Global`.
struct SigWorld<'a> {
    owner: ItemKey,
    types: crate::portable::PortableTys<'a>,
    inputs: blaze_own::Vec<crate::portable::PTy, Alloc<'a>>,
    output: crate::portable::PTy,
}

impl<'a> SigWorld<'a> {
    fn new(
        owner: ItemKey,
        types: crate::portable::PortableTys<'a>,
        inputs: &[crate::portable::PTy],
        output: crate::portable::PTy,
        alloc: Alloc<'a>,
    ) -> SigWorld<'a> {
        let mut ins = blaze_own::Vec::with_capacity_in(inputs.len(), alloc);
        ins.extend_from_slice(inputs);
        SigWorld { owner, types, inputs: ins, output }
    }
}

impl crate::world::World for SigWorld<'_> {
    fn fn_sig(&self, _: crate::world::Seal, item: ItemKey) -> crate::world::Fetch<Option<crate::world::FnSig<'_>>> {
        if item != self.owner {
            return Ok(None);
        }
        Ok(Some(crate::world::FnSig {
            parent_count: 0,
            own_count: 0,
            declared_count: 0,
            self_param: false,
            header: crate::ids::FnPtrHeader::RUST,
            inputs: &self.inputs,
            output: self.output,
            types: self.types.view(),
            c_variadic: false,
            legacy_const_generics: &[],
        }))
    }
    fn predicates(&self, _: crate::world::Seal, _: ItemKey) -> crate::world::Fetch<crate::world::Predicates<'_>> {
        Ok(crate::world::Predicates { clauses: &[], types: PTys::EMPTY })
    }
    fn adt(&self, _: crate::world::Seal, _: ItemKey) -> crate::world::Fetch<Option<crate::world::Adt<'_>>> {
        Ok(None)
    }
    fn impls_of(&self, _: crate::world::Seal, _: ItemKey, _: crate::world::Shape) -> crate::world::Fetch<&[ItemKey]> {
        Ok(&[])
    }
    fn impl_header(&self, _: crate::world::Seal, _: ItemKey) -> crate::world::Fetch<Option<crate::world::ImplHeader<'_>>> {
        Ok(None)
    }
    fn inherent(&self, _: crate::world::Seal, _: crate::world::Shape, _: Name) -> crate::world::Fetch<&[crate::world::AssocItem]> {
        Ok(&[])
    }
    fn assoc_item(&self, _: crate::world::Seal, _: ItemKey, _: Name) -> crate::world::Fetch<Option<crate::world::AssocItem>> {
        Ok(None)
    }
    fn traits_in_scope(&self, _: crate::world::Seal, _: ScopeId) -> crate::world::Fetch<&[ItemKey]> {
        Ok(&[])
    }
    fn resolve_path(&self, _: crate::world::Seal, _: ScopeId, _: crate::ids::PathIdx, _: &[Name]) -> crate::world::Fetch<crate::path_miss::PathAnswer> {
        Ok(Err(crate::path_miss::PathMiss::Whole))
    }
    fn lang_item(&self, _: crate::world::Seal, _: crate::ids::LangItem) -> crate::world::Fetch<Option<ItemKey>> {
        Ok(None)
    }
    fn lang_path(&self, _: crate::world::Seal, _: crate::ids::LangItem) -> crate::world::Fetch<Option<crate::world::Def>> {
        Ok(None)
    }
    fn const_value(&self, _: crate::world::Seal, _: ItemKey) -> crate::world::Fetch<Option<u64>> {
        Ok(None)
    }
    fn opaque_hidden(&self, _: crate::world::Seal, _: ItemKey) -> crate::world::Fetch<Option<crate::world::Hidden<'_>>> {
        Ok(None)
    }
    fn closure(&self, _: crate::world::Seal, _: ItemKey, _: u32) -> crate::world::Fetch<Option<crate::world::ClosureFacts<'_>>> {
        Ok(None)
    }
    fn attrs(&self, _: crate::world::Seal, _: ItemKey) -> crate::world::Fetch<crate::world::TypeAttrs> {
        Ok(crate::world::TypeAttrs(0))
    }
    fn trait_def(&self, _: crate::world::Seal, _: ItemKey) -> crate::world::Fetch<Option<crate::world::TraitDef<'_>>> {
        Ok(None)
    }
    fn assoc_parent(&self, _: crate::world::Seal, _: ItemKey) -> crate::world::Fetch<Option<crate::world::AssocParent>> {
        Ok(None)
    }
    fn const_holds(&self, _: crate::world::Seal, _: ItemKey, _: &[crate::portable::PTy], _: PTys<'_>) -> crate::world::Fetch<crate::world::ConstHolds> {
        Ok(crate::world::ConstHolds::Error)
    }
    fn const_params(&self, _: crate::world::Seal, _: ItemKey) -> crate::world::Fetch<crate::world::ConstParams<'_>> {
        Ok(crate::world::ConstParams { params: &[], types: PTys::EMPTY })
    }
}

fn run_sig_check<'a>(body: &BodyView<'_>, world: &SigWorld<'_>, alloc: Alloc<'a>) -> Checked<'a> {
    let mut cx = InferCtx::new_in(alloc);
    let hints = Needs::new_in(alloc);
    match crate::check(body, world, &mut cx, &hints, alloc).expect("check ok") {
        Outcome::Done(c) => c,
        Outcome::Need(n) => panic!("unexpected need: {:?}", n.questions),
    }
}

fn empty_body<'b>(owner: ItemKey, types: PTys<'b>, root: ExprId) -> BodyView<'b> {
    BodyView {
        owner,
        edition: Edition::E2024,
        params: &[],
        root,
        exprs: &[],
        expr_ranges: &[],
        pats: &[],
        pat_ranges: &[],
        stmts: &[],
        expr_lists: &[],
        pat_lists: &[],
        stmt_lists: &[],
        arms: &[],
        field_inits: &[],
        field_pats: &[],
        paths: &[],
        segments: &[],
        segment_ranges: &[],
        types,
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
        legacy_args: &[],
        unsafe_blocks: &[],
        expr_editions: &[],
        pat_editions: &[],
        record_expr_schemas: &[],
        closure_static_params: &[],
    }
}

#[test]
fn union_injection_coercion_and_rejection() {
    use crate::body::{IntSuffix, Lit};
    use crate::columns::AdjustKind;
    use crate::diag::DiagCode;
    use crate::ids::{IntTy, UintTy};
    use crate::portable::{PNode, PortableTys};

    let alloc = blaze_own::testing::heap().alloc();
    let owner = ItemKey(1001);
    let mut tys = PortableTys::new_in(alloc);
    let i32_t = tys.intern(PNode::Int(IntTy::I32)).expect("i32");
    let u8_t = tys.intern(PNode::Uint(UintTy::U8)).expect("u8");
    let union_l = tys.intern_list(&[i32_t, u8_t]).expect("list");
    let union_t = tys.intern(PNode::Union(union_l)).expect("union");

    // 1. `42i32` coerces to `i32 | u8` via `UnionInject { variant: 0 }`.
    let world = SigWorld::new(owner, tys, &[], union_t, alloc);
    let exprs = [Expr::Lit(Lit::Int(Some(IntSuffix::Int(IntTy::I32))))];
    let expr_ranges = [Range { start: 0, end: 5 }];
    let mut bv = empty_body(owner, PTys::EMPTY, ExprId(0));
    bv.exprs = &exprs;
    bv.expr_ranges = &expr_ranges;
    let checked = run_sig_check(&bv, &world, alloc);
    assert!(checked.diags.is_empty(), "diags: {:?}", checked.diags);
    assert_eq!(checked.adjusts.column::<0>(), &[ExprId(0)]);
    assert_eq!(checked.adjusts.column::<1>(), &[AdjustKind::UnionInject { variant: 0 }]);

    // Checked record round-trip preserves `AdjustKind::UnionInject`.
    let bytes = crate::checked_record::encode(&checked, alloc).expect("encode");
    let decoded = crate::checked_record::decode(&bytes, alloc).expect("decode");
    assert!(checked.same_result(&decoded));

    // 2. `7u8` coerces to `i32 | u8` via `UnionInject { variant: 1 }`.
    let exprs2 = [Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U8))))];
    bv.exprs = &exprs2;
    let checked2 = run_sig_check(&bv, &world, alloc);
    assert!(checked2.diags.is_empty(), "diags: {:?}", checked2.diags);
    assert_eq!(checked2.adjusts.column::<1>(), &[AdjustKind::UnionInject { variant: 1 }]);

    // 3. `true` (bool) does NOT coerce to `i32 | u8` -> Mismatch diagnostic.
    let exprs3 = [Expr::Lit(Lit::Bool(true))];
    bv.exprs = &exprs3;
    let checked3 = run_sig_check(&bv, &world, alloc);
    assert_eq!(checked3.diags.len(), 1);
    assert_eq!(checked3.diags[0].code, DiagCode::Mismatch);
}

#[test]
fn union_widening_coercion_and_rejection() {
    use crate::columns::AdjustKind;
    use crate::diag::DiagCode;
    use crate::ids::{IntTy, UintTy};
    use crate::portable::{PNode, PortableTys};

    let alloc = blaze_own::testing::heap().alloc();
    let owner = ItemKey(1002);
    let mut tys = PortableTys::new_in(alloc);
    let i32_t = tys.intern(PNode::Int(IntTy::I32)).expect("i32");
    let u8_t = tys.intern(PNode::Uint(UintTy::U8)).expect("u8");
    let bool_t = tys.intern(PNode::Bool).expect("bool");
    let char_t = tys.intern(PNode::Char).expect("char");
    let ab_l = tys.intern_list(&[i32_t, u8_t]).expect("ab");
    let ab = tys.intern(PNode::Union(ab_l)).expect("A | B");
    let abc_l = tys.intern_list(&[i32_t, u8_t, bool_t]).expect("abc");
    let abc = tys.intern(PNode::Union(abc_l)).expect("A | B | C");
    let ad_l = tys.intern_list(&[i32_t, char_t]).expect("ad");
    let ad = tys.intern(PNode::Union(ad_l)).expect("A | D");

    // 1. `fn f(x: i32 | u8) -> i32 | u8 | bool { x }` -> `UnionWiden`.
    let world = SigWorld::new(owner, tys.fork_in(alloc).expect("fork"), &[ab], abc, alloc);
    let pats = [Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of("x"), sub: None, ty: None }];
    let pat_ranges = [Range { start: 0, end: 1 }];
    let params = [PatId(0)];
    let exprs = [Expr::Local(PatId(0))];
    let expr_ranges = [Range { start: 2, end: 3 }];
    let mut bv = empty_body(owner, PTys::EMPTY, ExprId(0));
    bv.params = &params;
    bv.pats = &pats;
    bv.pat_ranges = &pat_ranges;
    bv.exprs = &exprs;
    bv.expr_ranges = &expr_ranges;
    let checked = run_sig_check(&bv, &world, alloc);
    assert!(checked.diags.is_empty(), "diags: {:?}", checked.diags);
    assert_eq!(checked.adjusts.column::<0>(), &[ExprId(0)]);
    assert_eq!(checked.adjusts.column::<1>(), &[AdjustKind::UnionWiden]);

    // Round-trip `AdjustKind::UnionWiden` through `checked_record`.
    let bytes = crate::checked_record::encode(&checked, alloc).expect("encode");
    let decoded = crate::checked_record::decode(&bytes, alloc).expect("decode");
    assert!(checked.same_result(&decoded));

    // 2. `fn g(x: i32 | char) -> i32 | u8 | bool { x }` -> Mismatch (char not in target).
    let world_bad = SigWorld::new(owner, tys, &[ad], abc, alloc);
    let checked_bad = run_sig_check(&bv, &world_bad, alloc);
    assert_eq!(checked_bad.diags.len(), 1);
    assert_eq!(checked_bad.diags[0].code, DiagCode::Mismatch);
}

#[test]
fn typed_union_pattern_and_residual_narrowing() {
    use crate::body::{Arm, IntSuffix, Lit};
    use crate::columns::UnionPat;
    use crate::ids::{IntTy, Mutbl, UintTy};
    use crate::portable::{PNode, PortableTys};

    let alloc = blaze_own::testing::heap().alloc();
    let owner = ItemKey(1003);

    // Part A: `fn f(u: i32 | u8 | bool) -> u8 | bool { match u { a: i32 => 0u8, rest => rest } }`
    let mut tys = PortableTys::new_in(alloc);
    let i32_t = tys.intern(PNode::Int(IntTy::I32)).expect("i32");
    let u8_t = tys.intern(PNode::Uint(UintTy::U8)).expect("u8");
    let bool_t = tys.intern(PNode::Bool).expect("bool");
    let u3_l = tys.intern_list(&[i32_t, u8_t, bool_t]).expect("u3");
    let u3 = tys.intern(PNode::Union(u3_l)).expect("i32 | u8 | bool");
    let u2_l = tys.intern_list(&[u8_t, bool_t]).expect("u2");
    let u2 = tys.intern(PNode::Union(u2_l)).expect("u8 | bool");

    let mut body_tys = PortableTys::new_in(alloc);
    let body_i32 = body_tys.intern(PNode::Int(IntTy::I32)).expect("body i32");

    let world = SigWorld::new(owner, tys.fork_in(alloc).expect("fork"), &[u3], u2, alloc);
    let pats = [
        Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of("u"), sub: None, ty: None },
        Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of("a"), sub: None, ty: Some(body_i32) },
        Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of("rest"), sub: None, ty: None },
    ];
    let pat_ranges = [Range { start: 0, end: 1 }, Range { start: 2, end: 8 }, Range { start: 9, end: 13 }];
    let params = [PatId(0)];
    let exprs = [
        Expr::Local(PatId(0)),
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U8)))),
        Expr::Local(PatId(2)),
        Expr::Match { scrutinee: ExprId(0), arms: List { start: 0, len: 2 } },
    ];
    let expr_ranges = [
        Range { start: 14, end: 15 },
        Range { start: 16, end: 19 },
        Range { start: 20, end: 24 },
        Range { start: 25, end: 40 },
    ];
    let arms = [
        Arm { pat: PatId(1), guard: None, body: ExprId(1) },
        Arm { pat: PatId(2), guard: None, body: ExprId(2) },
    ];
    let mut bv = empty_body(owner, body_tys.view(), ExprId(3));
    bv.params = &params;
    bv.pats = &pats;
    bv.pat_ranges = &pat_ranges;
    bv.exprs = &exprs;
    bv.expr_ranges = &expr_ranges;
    bv.arms = &arms;

    let checked = run_sig_check(&bv, &world, alloc);
    assert!(checked.diags.is_empty(), "diags: {:?}", checked.diags);
    assert_eq!(checked.union_pat_of(PatId(1)), Some(UnionPat::Variant(0)));
    assert_eq!(checked.union_pat_of(PatId(2)), Some(UnionPat::Residual));
    let view = checked.types.view();
    assert_eq!(view.node(checked.local_ty[1]), Ok(PNode::Int(IntTy::I32)));
    let PNode::Union(rest_l) = view.node(checked.local_ty[2]).expect("rest node") else {
        panic!("expected rest to narrow to a 2-member union, got {:?}", view.node(checked.local_ty[2]));
    };
    let rest_members = view.list(rest_l).expect("rest members");
    assert_eq!(rest_members.len(), 2);
    assert_eq!(view.node(rest_members[0]), Ok(PNode::Uint(UintTy::U8)));
    assert_eq!(view.node(rest_members[1]), Ok(PNode::Bool));

    // Round-trip `union_pats` through `checked_record`.
    let bytes = crate::checked_record::encode(&checked, alloc).expect("encode");
    let decoded = crate::checked_record::decode(&bytes, alloc).expect("decode");
    assert!(checked.same_result(&decoded));

    // Part B: single-member residual narrowing through `&`:
    // `fn g(u: &(i32 | bool)) -> bool { match u { a: i32 => false, b => *b } }`
    let u_ib_l = tys.intern_list(&[i32_t, bool_t]).expect("ib");
    let u_ib = tys.intern(PNode::Union(u_ib_l)).expect("i32 | bool");
    let ref_u_ib = tys.intern(PNode::Ref(Mutbl::Not, u_ib)).expect("&(i32 | bool)");
    let world_b = SigWorld::new(owner, tys, &[ref_u_ib], bool_t, alloc);
    let exprs_b = [
        Expr::Local(PatId(0)),
        Expr::Lit(Lit::Bool(false)),
        Expr::Local(PatId(2)),
        Expr::Unary { op: crate::body::UnOp::Deref, operand: ExprId(2) },
        Expr::Match { scrutinee: ExprId(0), arms: List { start: 0, len: 2 } },
    ];
    let expr_ranges_b = [
        Range { start: 14, end: 15 },
        Range { start: 16, end: 21 },
        Range { start: 22, end: 23 },
        Range { start: 24, end: 26 },
        Range { start: 27, end: 40 },
    ];
    let arms_b = [
        Arm { pat: PatId(1), guard: None, body: ExprId(1) },
        Arm { pat: PatId(2), guard: None, body: ExprId(3) },
    ];
    let mut bv_b = empty_body(owner, body_tys.view(), ExprId(4));
    bv_b.params = &params;
    bv_b.pats = &pats;
    bv_b.pat_ranges = &pat_ranges;
    bv_b.exprs = &exprs_b;
    bv_b.expr_ranges = &expr_ranges_b;
    bv_b.arms = &arms_b;
    let checked_b = run_sig_check(&bv_b, &world_b, alloc);
    assert!(checked_b.diags.is_empty(), "diags: {:?}", checked_b.diags);
    assert_eq!(checked_b.union_pat_of(PatId(1)), Some(UnionPat::Variant(0)));
    assert_eq!(checked_b.union_pat_of(PatId(2)), Some(UnionPat::Variant(1)));
    let vb = checked_b.types.view();
    let PNode::Ref(Mutbl::Not, a_inner) = vb.node(checked_b.local_ty[1]).expect("a ref") else {
        panic!("expected a: &i32, got {:?}", vb.node(checked_b.local_ty[1]));
    };
    assert_eq!(vb.node(a_inner), Ok(PNode::Int(IntTy::I32)));
    let PNode::Ref(Mutbl::Not, b_inner) = vb.node(checked_b.local_ty[2]).expect("b ref") else {
        panic!("expected b: &bool, got {:?}", vb.node(checked_b.local_ty[2]));
    };
    assert_eq!(vb.node(b_inner), Ok(PNode::Bool));
}

#[test]
fn structural_records_literal_field_update_and_destructure() {
    use crate::body::{FieldInit, FieldPat, IntSuffix, Lit, StructBase};
    use crate::ids::{IntTy, Mutbl, RecordSchema, UintTy};
    use crate::portable::{PNode, PortableTys};

    let alloc = blaze_own::testing::heap().alloc();
    let owner = ItemKey(1004);
    let mut tys = PortableTys::new_in(alloc);
    let i32_t = tys.intern(PNode::Int(IntTy::I32)).expect("i32");
    let u8_t = tys.intern(PNode::Uint(UintTy::U8)).expect("u8");
    let sorted_names = [Name::of("a"), Name::of("b")];
    let schema = RecordSchema::of_sorted_names(&sorted_names);
    tys.register_record_schema(schema, &sorted_names).expect("register schema");
    let rec_l = tys.intern_list(&[i32_t, u8_t]).expect("rec fields");
    let rec_t = tys.intern(PNode::Record(schema, rec_l)).expect("rec");

    // Body:
    // fn f() -> { a: i32, b: u8 } {
    //   let r = { b: 1u8, a: 2i32 };
    //   let rr = &r;
    //   let _x = rr.b;
    //   let { a, .. } = r;
    //   { a: a, ..r }
    // }
    let mut body_tys = PortableTys::new_in(alloc);
    body_tys.register_record_schema(schema, &sorted_names).expect("register body schema");
    let world = SigWorld::new(owner, tys, &[], rec_t, alloc);

    let pats = [
        // 0: r
        Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of("r"), sub: None, ty: None },
        // 1: rr
        Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of("rr"), sub: None, ty: None },
        // 2: _x
        Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of("_x"), sub: None, ty: None },
        // 3: a
        Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of("a"), sub: None, ty: None },
        // 4: { a, .. }
        Pat::Record { fields: List { start: 0, len: 1 }, rest: true },
    ];
    let pat_ranges = [
        Range { start: 0, end: 1 },
        Range { start: 2, end: 4 },
        Range { start: 5, end: 7 },
        Range { start: 8, end: 9 },
        Range { start: 10, end: 20 },
    ];
    let field_pats = [FieldPat { name: Name::of("a"), pat: PatId(3) }];

    let exprs = [
        // 0: 1u8
        Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U8)))),
        // 1: 2i32
        Expr::Lit(Lit::Int(Some(IntSuffix::Int(IntTy::I32)))),
        // 2: { b: 1u8, a: 2i32 }
        Expr::Record { fields: List { start: 0, len: 2 }, base: StructBase::None },
        // 3: r
        Expr::Local(PatId(0)),
        // 4: &r
        Expr::Ref { mutbl: Mutbl::Not, operand: ExprId(3) },
        // 5: rr
        Expr::Local(PatId(1)),
        // 6: rr.b
        Expr::Field { base: ExprId(5), name: Name::of("b") },
        // 7: r (for destructuring)
        Expr::Local(PatId(0)),
        // 8: a (for update)
        Expr::Local(PatId(3)),
        // 9: r (base for update)
        Expr::Local(PatId(0)),
        // 10: { a: a, ..r }
        Expr::Record { fields: List { start: 2, len: 1 }, base: StructBase::Expr(ExprId(9)) },
        // 11: block
        Expr::Block { stmts: List { start: 0, len: 4 }, tail: Some(ExprId(10)), scope: ScopeId(0) },
    ];
    let mut expr_ranges = blaze_own::Vec::with_capacity_in(exprs.len(), alloc);
    for i in 0..exprs.len() {
        expr_ranges.push(Range { start: 100 + i as u32, end: 101 + i as u32 });
    }
    let field_inits = [
        // Written in reversed order: b then a!
        FieldInit { name: Name::of("b"), expr: ExprId(0) },
        FieldInit { name: Name::of("a"), expr: ExprId(1) },
        // Update init: a: a
        FieldInit { name: Name::of("a"), expr: ExprId(8) },
    ];
    let stmts = [
        Stmt::Let { pat: PatId(0), ty: None, init: Some(ExprId(2)), else_: None },
        Stmt::Let { pat: PatId(1), ty: None, init: Some(ExprId(4)), else_: None },
        Stmt::Let { pat: PatId(2), ty: None, init: Some(ExprId(6)), else_: None },
        Stmt::Let { pat: PatId(4), ty: None, init: Some(ExprId(7)), else_: None },
    ];
    let stmt_lists = [StmtId(0), StmtId(1), StmtId(2), StmtId(3)];
    let record_expr_schemas = [(ExprId(2), schema)];

    let mut bv = empty_body(owner, body_tys.view(), ExprId(11));
    bv.pats = &pats;
    bv.pat_ranges = &pat_ranges;
    bv.field_pats = &field_pats;
    bv.exprs = &exprs;
    bv.expr_ranges = &expr_ranges;
    bv.field_inits = &field_inits;
    bv.stmts = &stmts;
    bv.stmt_lists = &stmt_lists;
    bv.record_expr_schemas = &record_expr_schemas;

    let checked = run_sig_check(&bv, &world, alloc);
    assert!(checked.diags.is_empty(), "diags: {:?}", checked.diags);

    let view = checked.types.view();
    // Canonical schema names preserved in Checked::types!
    assert_eq!(view.record_names(schema), Some(&sorted_names[..]));
    // `rr.b` has type `u8` and field index 1 (since canonical order is a=0, b=1).
    assert_eq!(view.node(checked.expr_ty[6]), Ok(PNode::Uint(UintTy::U8)));
    assert_eq!(checked.fields.column::<0>(), &[ExprId(6)]);
    assert_eq!(checked.fields.column::<2>(), &[1]);
    // Destructured `a` has type `i32`.
    assert_eq!(view.node(checked.local_ty[3]), Ok(PNode::Int(IntTy::I32)));
    // Update `{ a: a, ..r }` takes unwritten field 1 (`b: u8`) from `r`.
    let (fru_idx, fru_tys) = checked.fru_fields_of(ExprId(10));
    assert_eq!(fru_idx, &[1]);
    assert_eq!(fru_tys.len(), 1);
    assert_eq!(view.node(fru_tys[0]), Ok(PNode::Uint(UintTy::U8)));

    // Checked record round-trip preserves `record_schemas` + `record_names`!
    let bytes = crate::checked_record::encode(&checked, alloc).expect("encode");
    let decoded = crate::checked_record::decode(&bytes, alloc).expect("decode");
    assert!(checked.same_result(&decoded));
    assert_eq!(decoded.types.view().record_names(schema), Some(&sorted_names[..]));
}

#[test]
fn union_match_exhaustiveness_and_redundant_arms() {
    use crate::body::{Arm, IntSuffix, Lit};
    use crate::diag::DiagCode;
    use crate::ids::{IntTy, UintTy};
    use crate::portable::{PNode, PortableTys};

    let alloc = blaze_own::testing::heap().alloc();
    let owner = ItemKey(1005);
    let mut tys = PortableTys::new_in(alloc);
    let i32_t = tys.intern(PNode::Int(IntTy::I32)).expect("i32");
    let u8_t = tys.intern(PNode::Uint(UintTy::U8)).expect("u8");
    let ab_l = tys.intern_list(&[i32_t, u8_t]).expect("ab");
    let ab = tys.intern(PNode::Union(ab_l)).expect("i32 | u8");

    let mut body_tys = PortableTys::new_in(alloc);
    let b_i32 = body_tys.intern(PNode::Int(IntTy::I32)).expect("b i32");
    let b_u8 = body_tys.intern(PNode::Uint(UintTy::U8)).expect("b u8");

    let world = SigWorld::new(owner, tys, &[ab], i32_t, alloc);
    let params = [PatId(0)];
    let p_x = Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of("x"), sub: None, ty: None };
    let p_a = Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of("a"), sub: None, ty: Some(b_i32) };
    let p_b = Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of("b"), sub: None, ty: Some(b_u8) };
    let p_rest = Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of("rest"), sub: None, ty: None };
    let p_a2 = Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of("a2"), sub: None, ty: Some(b_i32) };
    let e_x = Expr::Local(PatId(0));
    let e_1 = Expr::Lit(Lit::Int(Some(IntSuffix::Int(IntTy::I32))));
    let e_2 = Expr::Lit(Lit::Int(Some(IntSuffix::Int(IntTy::I32))));
    let e_3 = Expr::Lit(Lit::Int(Some(IntSuffix::Int(IntTy::I32))));
    let e_true = Expr::Lit(Lit::Bool(true));

    // 1. Non-exhaustive: `match x { a: i32 => 1 }` (missing `u8`).
    let pats1 = [p_x, p_a];
    let pat_r1 = [Range { start: 0, end: 1 }, Range { start: 2, end: 8 }];
    let exprs1 = [e_x, e_1, Expr::Match { scrutinee: ExprId(0), arms: List { start: 0, len: 1 } }];
    let expr_r1 = [Range { start: 10, end: 11 }, Range { start: 12, end: 13 }, Range { start: 14, end: 30 }];
    let arms1 = [Arm { pat: PatId(1), guard: None, body: ExprId(1) }];
    let mut bv = empty_body(owner, body_tys.view(), ExprId(2));
    bv.params = &params;
    bv.pats = &pats1;
    bv.pat_ranges = &pat_r1;
    bv.exprs = &exprs1;
    bv.expr_ranges = &expr_r1;
    bv.arms = &arms1;
    let c1 = run_sig_check(&bv, &world, alloc);
    assert_eq!(c1.diags.len(), 1, "diags: {:?}", c1.diags);
    assert_eq!(c1.diags[0].code, DiagCode::NonExhaustiveMatch);
    assert_eq!(c1.diags[0].expected.and_then(|t| c1.types.view().node(t).ok()), Some(PNode::Uint(UintTy::U8)));

    // 2. Guarded arm: `match x { a: i32 if true => 1, b: u8 => 2 }` -> still missing `i32`!
    let pats2 = [p_x, p_a, p_b];
    let pat_r2 = [Range { start: 0, end: 1 }, Range { start: 2, end: 8 }, Range { start: 9, end: 14 }];
    let exprs2 = [e_x, e_1, e_2, e_true, Expr::Match { scrutinee: ExprId(0), arms: List { start: 0, len: 2 } }];
    let expr_r2 = [
        Range { start: 20, end: 21 },
        Range { start: 22, end: 23 },
        Range { start: 24, end: 25 },
        Range { start: 26, end: 30 },
        Range { start: 31, end: 50 },
    ];
    let arms2 = [
        Arm { pat: PatId(1), guard: Some(ExprId(3)), body: ExprId(1) },
        Arm { pat: PatId(2), guard: None, body: ExprId(2) },
    ];
    let mut bv2 = empty_body(owner, body_tys.view(), ExprId(4));
    bv2.params = &params;
    bv2.pats = &pats2;
    bv2.pat_ranges = &pat_r2;
    bv2.exprs = &exprs2;
    bv2.expr_ranges = &expr_r2;
    bv2.arms = &arms2;
    let c2 = run_sig_check(&bv2, &world, alloc);
    assert_eq!(c2.diags.len(), 1, "diags: {:?}", c2.diags);
    assert_eq!(c2.diags[0].code, DiagCode::NonExhaustiveMatch);
    assert_eq!(c2.diags[0].expected.and_then(|t| c2.types.view().node(t).ok()), Some(PNode::Int(IntTy::I32)));

    // 3. Exhaustive typed arms: `match x { a: i32 => 1, b: u8 => 2 }` -> 0 diags.
    let exprs3 = [e_x, e_1, e_2, Expr::Match { scrutinee: ExprId(0), arms: List { start: 0, len: 2 } }];
    let expr_r3 = [Range { start: 20, end: 21 }, Range { start: 22, end: 23 }, Range { start: 24, end: 25 }, Range { start: 26, end: 40 }];
    let arms3 = [
        Arm { pat: PatId(1), guard: None, body: ExprId(1) },
        Arm { pat: PatId(2), guard: None, body: ExprId(2) },
    ];
    let mut bv3 = empty_body(owner, body_tys.view(), ExprId(3));
    bv3.params = &params;
    bv3.pats = &pats2;
    bv3.pat_ranges = &pat_r2;
    bv3.exprs = &exprs3;
    bv3.expr_ranges = &expr_r3;
    bv3.arms = &arms3;
    let c3 = run_sig_check(&bv3, &world, alloc);
    assert!(c3.diags.is_empty(), "diags: {:?}", c3.diags);

    // 4. Exhaustive with catch-all: `match x { a: i32 => 1, rest => 2 }` -> 0 diags.
    let pats4 = [p_x, p_a, p_rest];
    bv3.pats = &pats4;
    let c4 = run_sig_check(&bv3, &world, alloc);
    assert!(c4.diags.is_empty(), "diags: {:?}", c4.diags);

    // 5. Redundant arm: `match x { a: i32 => 1, a2: i32 => 2, b: u8 => 3 }` -> RedundantPattern.
    let pats5 = [p_x, p_a, p_a2, p_b];
    let pat_r5 = [
        Range { start: 0, end: 1 },
        Range { start: 2, end: 8 },
        Range { start: 9, end: 16 },
        Range { start: 17, end: 22 },
    ];
    let exprs5 = [e_x, e_1, e_2, e_3, Expr::Match { scrutinee: ExprId(0), arms: List { start: 0, len: 3 } }];
    let arms5 = [
        Arm { pat: PatId(1), guard: None, body: ExprId(1) },
        Arm { pat: PatId(2), guard: None, body: ExprId(2) },
        Arm { pat: PatId(3), guard: None, body: ExprId(3) },
    ];
    let mut bv5 = empty_body(owner, body_tys.view(), ExprId(4));
    bv5.params = &params;
    bv5.pats = &pats5;
    bv5.pat_ranges = &pat_r5;
    bv5.exprs = &exprs5;
    bv5.expr_ranges = &expr_r2;
    bv5.arms = &arms5;
    let c5 = run_sig_check(&bv5, &world, alloc);
    assert_eq!(c5.diags.len(), 1, "diags: {:?}", c5.diags);
    assert_eq!(c5.diags[0].code, DiagCode::RedundantPattern);
    assert_eq!(c5.diags[0].found.and_then(|t| c5.types.view().node(t).ok()), Some(PNode::Int(IntTy::I32)));
}

#[test]
fn inferred_return_type_holes_and_result_error_union() {
    use crate::body::{IntSuffix, Lit};
    use crate::columns::AdjustKind;
    use crate::fixture::{Bb, Fx};
    use crate::ids::UintTy;
    use crate::portable::{PNode, PortableTys};

    let alloc = blaze_own::testing::heap().alloc();

    // 1. General `fn f() -> _` returning `42u32`: inferred_output == u32, 0 diags.
    {
        let owner = ItemKey(1010);
        let mut tys = PortableTys::new_in(alloc);
        let hole = tys.intern(PNode::Infer).expect("infer");
        let world = SigWorld::new(owner, tys, &[], hole, alloc);
        let body_tys = PortableTys::new_in(alloc);
        let exprs = [Expr::Lit(Lit::Int(Some(IntSuffix::Uint(UintTy::U32))))];
        let expr_r = [Range { start: 0, end: 5 }];
        let mut bv = empty_body(owner, body_tys.view(), ExprId(0));
        bv.exprs = &exprs;
        bv.expr_ranges = &expr_r;
        let checked = run_sig_check(&bv, &world, alloc);
        assert!(checked.diags.is_empty(), "diags: {:?}", checked.diags);
        let out = checked.inferred_output.expect("inferred_output");
        assert_eq!(checked.types.view().node(out), Ok(PNode::Uint(UintTy::U32)));
    }

    // 2. `fn f(flag: bool) -> Result<u32, _>` with `Err(ErrA)` and `Err(ErrB)` -> `Result<u32, ErrA | ErrB>`.
    {
        let mut fx = Fx::new();
        let u32_t = fx.u32();
        let err_a = fx.tuple_strukt("ErrA", 0, &[u32_t]);
        let err_b = fx.tuple_strukt("ErrB", 0, &[u32_t]);
        fx.use_path("ErrA", crate::world::Def::Ctor { adt: err_a, variant: 0, kind: crate::world::CtorKind::Tuple });
        fx.use_path("ErrB", crate::world::Def::Ctor { adt: err_b, variant: 0, kind: crate::world::CtorKind::Tuple });
        let err_a_t = fx.adt(err_a, &[]);
        let err_b_t = fx.adt(err_b, &[]);
        let p0 = fx.param(0);
        let p1 = fx.param(1);
        let (result, _vars) = fx.enum_("Result", 2, &[("Ok", &[p0]), ("Err", &[p1])]);
        let bool_t = fx.ty(PNode::Bool);
        let hole_t = fx.ty(PNode::Infer);
        let res_infer = fx.adt(result, &[u32_t, hole_t]);
        let f_key = fx.func("f", 0, &[bool_t], res_infer, Default::default());
        let ab_list = fx.list(&[err_a_t, err_b_t]);
        let ab_union = fx.ty(PNode::Union(ab_list));
        let f_flat = fx.func("f_flat", 0, &[bool_t, ab_union], res_infer, Default::default());
        fx.finish();

        let mut b = Bb::new();
        let p_flag = b.param("flag");
        let e_flag = b.local(p_flag);
        let e_err1 = b.path(&["Result", "Err"]);
        let e_a_fn = b.path(&["ErrA"]);
        let e_n1 = b.int(Some(IntSuffix::Uint(UintTy::U32)));
        let e_a = b.call(e_a_fn, &[e_n1]);
        let e_call_a = b.call(e_err1, &[e_a]);
        let e_err2 = b.path(&["Result", "Err"]);
        let e_b_fn = b.path(&["ErrB"]);
        let e_n2 = b.int(Some(IntSuffix::Uint(UintTy::U32)));
        let e_b = b.call(e_b_fn, &[e_n2]);
        let e_call_b = b.call(e_err2, &[e_b]);
        let w1 = b.wild();
        let w2 = b.wild();
        let root = b.match_(e_flag, &[(w1, e_call_a), (w2, e_call_b)]);

        let view = b.view(f_key, root);
        let mut ctx = crate::InferCtx::new_in(alloc);
        let checked = match crate::check(&view, &fx, &mut ctx, &crate::Needs::new_in(alloc), alloc).expect("check") {
            crate::Outcome::Done(c) => c,
            crate::Outcome::Need(n) => panic!("unexpected need: {n:?}"),
        };
        assert!(checked.diags.is_empty(), "diags: {:?}", checked.diags);
        let out = checked.inferred_output.expect("inferred_output");
        let tv = checked.types.view();
        let PNode::Adt(res_k, args_l) = tv.node(out).expect("out node") else {
            panic!("expected Result ADT, got {:?}", tv.node(out));
        };
        assert_eq!(res_k, result);
        let args = tv.list(args_l).expect("args");
        assert_eq!(args.len(), 2);
        assert_eq!(tv.node(args[0]), Ok(PNode::Uint(UintTy::U32)));
        let PNode::Union(u_l) = tv.node(args[1]).expect("err union") else {
            panic!("expected Union error set, got {:?}", tv.node(args[1]));
        };
        let members = tv.list(u_l).expect("union members");
        assert_eq!(members.len(), 2);
        assert!(matches!(tv.node(members[0]), Ok(PNode::Adt(k, _)) if k == err_a));
        assert!(matches!(tv.node(members[1]), Ok(PNode::Adt(k, _)) if k == err_b));
        assert!(
            checked
                .adjusts
                .column::<0>()
                .iter()
                .zip(checked.adjusts.column::<1>().iter())
                .any(|(&ex, k)| ex == e_a && matches!(k, AdjustKind::UnionInject { variant: 0 })),
            "expected UnionInject(0) on ErrA arg"
        );
        assert!(
            checked
                .adjusts
                .column::<0>()
                .iter()
                .zip(checked.adjusts.column::<1>().iter())
                .any(|(&ex, k)| ex == e_b && matches!(k, AdjustKind::UnionInject { variant: 1 })),
            "expected UnionInject(1) on ErrB arg"
        );

        // 3. Single error `Err(ErrA)` -> `Result<u32, ErrA>` (not a 1-element union).
        let mut b2 = Bb::new();
        let _p_flag2 = b2.param("flag");
        let e_err_s = b2.path(&["Result", "Err"]);
        let e_as_fn = b2.path(&["ErrA"]);
        let e_ns = b2.int(Some(IntSuffix::Uint(UintTy::U32)));
        let e_as = b2.call(e_as_fn, &[e_ns]);
        let root2 = b2.call(e_err_s, &[e_as]);
        let view2 = b2.view(f_key, root2);
        let mut ctx2 = crate::InferCtx::new_in(alloc);
        let checked2 = match crate::check(&view2, &fx, &mut ctx2, &crate::Needs::new_in(alloc), alloc).expect("check2") {
            crate::Outcome::Done(c) => c,
            crate::Outcome::Need(n) => panic!("unexpected need: {n:?}"),
        };
        assert!(checked2.diags.is_empty(), "diags2: {:?}", checked2.diags);
        let out2 = checked2.inferred_output.expect("inferred_output2");
        let tv2 = checked2.types.view();
        let PNode::Adt(_, args_l2) = tv2.node(out2).expect("out2 node") else { panic!("expected Result") };
        let args2 = tv2.list(args_l2).expect("args2");
        assert!(matches!(tv2.node(args2[1]), Ok(PNode::Adt(k, _)) if k == err_a));

        // 4. Flattening & deduplication: `Err(ErrA)` + `Err(ab)` where `ab: ErrA | ErrB` -> `ErrA | ErrB`.
        let mut b3 = Bb::new();
        let p_fl = b3.param("flag");
        let p_ab = b3.param("ab");
        let e_fl = b3.local(p_fl);
        let e_ab = b3.local(p_ab);
        let e_err3a = b3.path(&["Result", "Err"]);
        let e_a3_fn = b3.path(&["ErrA"]);
        let e_n3 = b3.int(Some(IntSuffix::Uint(UintTy::U32)));
        let e_a3 = b3.call(e_a3_fn, &[e_n3]);
        let c3a = b3.call(e_err3a, &[e_a3]);
        let e_err3b = b3.path(&["Result", "Err"]);
        let c3b = b3.call(e_err3b, &[e_ab]);
        let w3a = b3.wild();
        let w3b = b3.wild();
        let root3 = b3.match_(e_fl, &[(w3a, c3a), (w3b, c3b)]);
        let view3 = b3.view(f_flat, root3);
        let mut ctx3 = crate::InferCtx::new_in(alloc);
        let checked3 = match crate::check(&view3, &fx, &mut ctx3, &crate::Needs::new_in(alloc), alloc).expect("check3") {
            crate::Outcome::Done(c) => c,
            crate::Outcome::Need(n) => panic!("unexpected need: {n:?}"),
        };
        assert!(checked3.diags.is_empty(), "diags3: {:?}", checked3.diags);
        let out3 = checked3.inferred_output.expect("inferred_output3");
        let tv3 = checked3.types.view();
        let PNode::Adt(_, args_l3) = tv3.node(out3).expect("out3 node") else { panic!("expected Result") };
        let args3 = tv3.list(args_l3).expect("args3");
        let PNode::Union(u_l3) = tv3.node(args3[1]).expect("err union3") else {
            panic!("expected Union error set, got {:?}", tv3.node(args3[1]));
        };
        assert_eq!(tv3.list(u_l3).expect("union3").len(), 2);
    }
}
