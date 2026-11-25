//! Sans-IO resumable shell execution machine (`bstd::sh`).
//!
//! [`Machine`] evaluates a parsed [`Block`] using **deterministic effect-log replay** on top of
//! [`EngineState`]'s O(1) copy-on-write fork:
//! - Every external IO / clock / process / Rust-cell request yields [`Step::Need`] and resumes
//!   after [`Machine::answer`].
//! - Incremental output (`print`) yields [`Step::Stream`].
//! - Suspension and resumption work seamlessly at arbitrary depth inside `if`, `for`, `while`,
//!   `loop`, `match`, `try/catch`, `def`, closures (`each`/`where`/`filter`/`reduce`), and
//!   multi-stage pipelines.
//! - Errors automatically roll back `EngineState` transactionally to the pre-line snapshot.

use core::cmp::Ordering;
use core::fmt::Write as _;

use blaze_own::{ARc, AString, AVec, Alloc, Rc, Share};

use crate::ast::{
    AssignOp, AssignTarget, BinaryOp, Block, CallArg, CallExpr, Expr, ExprKind, InterpPart,
    ParamKind, PathKey, PathStep, Pattern, Stmt, TypeAnn, UnaryOp,
};
use crate::builtins::{
    dispatch_builtin, f64_floor, f64_powf, f64_powi, from_json_bytes, path_steps_to_members,
    to_json_string, to_text_string,
};
use crate::error::{ErrorKind, ShellError};
use crate::persist::{PMap, PVec};
use crate::protocol::{Answer, EvalOutcome, ExternalMode, Request, Step, StreamChunk};
use crate::span::Span;
use crate::state::{DefData, EngineState, VarSlot};
use crate::value::{Bytes, ClosureData, DateTime, Nanos, PathMember, RangeVal, Record, Table, Value};

/// Default execution fuel per `Machine::step` evaluation pass (rule 18: bounded progress).
pub const DEFAULT_FUEL: u32 = 200_000;

/// Maximum number of host request/answer round-trips per submitted line (rule 18).
const MAX_ANSWERS: usize = 4096;

/// Maximum nested user `def` / closure call depth (rule 18).
const MAX_CALL_DEPTH: u32 = 128;

/// Maximum nested alias expansion depth (rule 18).
const MAX_ALIAS_DEPTH: u32 = 32;

/// Maximum string repetition length for `"a" * n` (rule 18).
const MAX_STRING_REPEAT: usize = 1 << 20;

/// Control-flow signal unwinding out of [`Evaluator`].
pub(crate) enum Control<'a> {
    Need(Request<'a>),
    Stream(StreamChunk<'a>),
    Break(Span),
    Continue(Span),
    Return(Value<'a>),
    Error(ShellError<'a>),
}

/// Sans-IO resumable execution machine for one submitted shell command line.
pub struct Machine<'a> {
    parsed: Result<Block<'a>, ShellError<'a>>,
    initial_state: EngineState<'a>,
    answers: AVec<'a, Answer<'a>>,
    emitted_streams: usize,
    fuel_limit: u32,
    done: bool,
    awaiting_answer: bool,
    alloc: Alloc<'a>,
}

impl<'a> Machine<'a> {
    /// Create a new machine for `src` starting from `state` with [`DEFAULT_FUEL`].
    pub fn new(src: &str, state: EngineState<'a>) -> Machine<'a> {
        Machine::new_with_fuel(src, state, DEFAULT_FUEL)
    }

    /// Create a new machine for `src` starting from `state` with an explicit `fuel` budget.
    pub fn new_with_fuel(src: &str, state: EngineState<'a>, fuel: u32) -> Machine<'a> {
        let alloc = state.alloc();
        let parsed = crate::parse::parse(src, alloc);
        Machine {
            parsed,
            initial_state: state,
            answers: AVec::new_in(alloc),
            emitted_streams: 0,
            fuel_limit: fuel,
            done: false,
            awaiting_answer: false,
            alloc,
        }
    }

    /// True once `step()` has returned [`Step::Done`].
    #[inline]
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Supply the host's [`Answer`] for the last [`Step::Need`] request.
    pub fn answer(&mut self, answer: Answer<'a>) {
        if self.awaiting_answer {
            self.answers.push(answer);
            self.awaiting_answer = false;
        }
    }

    /// Advance execution until the next [`Step::Need`], [`Step::Stream`], or [`Step::Done`].
    pub fn step(&mut self) -> Step<'a> {
        let alloc = self.alloc;
        if self.done {
            return Step::Done(EvalOutcome {
                value: Value::Nothing,
                state: self.initial_state.fork(),
                exit_code: self.initial_state.last_exit,
                terminal_passthrough: false,
            });
        }
        if self.awaiting_answer {
            self.done = true;
            let err = ShellError::new(
                ErrorKind::User,
                Span::ZERO,
                "`Machine::step` called while awaiting `Machine::answer`",
                alloc,
            );
            let mut st = self.initial_state.fork();
            st.last_exit = 1;
            return Step::Done(EvalOutcome {
                value: Value::error(err, alloc),
                state: st,
                exit_code: 1,
                terminal_passthrough: false,
            });
        }

        let block = match &self.parsed {
            Ok(b) => b,
            Err(err) => {
                self.done = true;
                let mut st = self.initial_state.fork();
                st.last_exit = 1;
                return Step::Done(EvalOutcome {
                    value: Value::error(err.clone(), alloc),
                    state: st,
                    exit_code: 1,
                    terminal_passthrough: false,
                });
            }
        };

        let mut eval = Evaluator {
            state: self.initial_state.fork(),
            answers: self.answers.as_slice(),
            answer_cursor: 0,
            emitted_streams: self.emitted_streams,
            stream_cursor: 0,
            fuel: self.fuel_limit,
            alloc,
            row_ctx: None,
            current_input: Value::Nothing,
            call_depth: 0,
            alias_depth: 0,
            terminal_passthrough: false,
            external_exit_set: false,
        };

        let result = eval.eval_block_top_level(block);
        let Evaluator {
            state: mut eval_state,
            terminal_passthrough,
            external_exit_set,
            ..
        } = eval;

        match result {
            Ok(value) | Err(Control::Return(value)) => {
                self.done = true;
                if !external_exit_set {
                    eval_state.last_exit = 0;
                }
                let exit_code = eval_state.last_exit;
                Step::Done(EvalOutcome {
                    value,
                    state: eval_state,
                    exit_code,
                    terminal_passthrough,
                })
            }
            Err(Control::Need(req)) => {
                if self.answers.len() >= MAX_ANSWERS {
                    self.done = true;
                    let err = ShellError::new(
                        ErrorKind::Budget,
                        Span::ZERO,
                        "request limit exceeded for single command",
                        alloc,
                    );
                    let mut st = self.initial_state.fork();
                    st.last_exit = 1;
                    return Step::Done(EvalOutcome {
                        value: Value::error(err, alloc),
                        state: st,
                        exit_code: 1,
                        terminal_passthrough: false,
                    });
                }
                self.awaiting_answer = true;
                Step::Need(req)
            }
            Err(Control::Stream(chunk)) => {
                self.emitted_streams += 1;
                Step::Stream(chunk)
            }
            Err(Control::Break(span)) => {
                self.done = true;
                let err = ShellError::new(ErrorKind::User, span, "`break` outside of loop", alloc);
                let mut st = self.initial_state.fork();
                st.last_exit = 1;
                Step::Done(EvalOutcome {
                    value: Value::error(err, alloc),
                    state: st,
                    exit_code: 1,
                    terminal_passthrough: false,
                })
            }
            Err(Control::Continue(span)) => {
                self.done = true;
                let err = ShellError::new(ErrorKind::User, span, "`continue` outside of loop", alloc);
                let mut st = self.initial_state.fork();
                st.last_exit = 1;
                Step::Done(EvalOutcome {
                    value: Value::error(err, alloc),
                    state: st,
                    exit_code: 1,
                    terminal_passthrough: false,
                })
            }
            Err(Control::Error(err)) => {
                self.done = true;
                let exit_code = if external_exit_set && eval_state.last_exit != 0 {
                    eval_state.last_exit
                } else {
                    1
                };
                let mut st = self.initial_state.fork();
                st.last_exit = exit_code;
                Step::Done(EvalOutcome {
                    value: Value::error(err, alloc),
                    state: st,
                    exit_code,
                    terminal_passthrough: false,
                })
            }
        }
    }
}

// =============================================================================
// Evaluator
// =============================================================================

pub(crate) struct Evaluator<'a, 'm> {
    state: EngineState<'a>,
    answers: &'m [Answer<'a>],
    answer_cursor: usize,
    emitted_streams: usize,
    stream_cursor: usize,
    fuel: u32,
    alloc: Alloc<'a>,
    row_ctx: Option<Value<'a>>,
    current_input: Value<'a>,
    call_depth: u32,
    alias_depth: u32,
    terminal_passthrough: bool,
    external_exit_set: bool,
}

impl<'a, 'm> Evaluator<'a, 'm> {
    #[inline]
    pub(crate) fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }

    #[inline]
    pub(crate) fn state(&self) -> &EngineState<'a> {
        &self.state
    }

    #[inline]
    pub(crate) fn state_mut(&mut self) -> &mut EngineState<'a> {
        &mut self.state
    }

    #[inline]
    pub(crate) fn charge(&mut self, cost: u32, span: Span) -> Result<(), Control<'a>> {
        if self.fuel < cost {
            self.fuel = 0;
            return Err(Control::Error(ShellError::new(
                ErrorKind::Budget,
                span,
                "execution fuel budget exhausted",
                self.alloc,
            )));
        }
        self.fuel -= cost;
        Ok(())
    }

    pub(crate) fn request(
        &mut self,
        req: Request<'a>,
        span: Span,
    ) -> Result<Answer<'a>, Control<'a>> {
        self.charge(1, span)?;
        if self.answer_cursor < self.answers.len() {
            let ans = self.answers[self.answer_cursor].clone();
            self.answer_cursor += 1;
            if let Answer::Err(mut err) = ans {
                if err.span == Span::ZERO {
                    err.span = span;
                }
                return Err(Control::Error(err));
            }
            Ok(ans)
        } else {
            Err(Control::Need(req))
        }
    }

    pub(crate) fn emit_stream(&mut self, chunk: StreamChunk<'a>) -> Result<(), Control<'a>> {
        if self.stream_cursor < self.emitted_streams {
            self.stream_cursor += 1;
            Ok(())
        } else {
            Err(Control::Stream(chunk))
        }
    }

    fn eval_block_top_level(&mut self, block: &Block<'a>) -> Result<Value<'a>, Control<'a>> {
        let mut last = Value::Nothing;
        for stmt in block.stmts.iter() {
            last = self.eval_stmt(stmt, true, None)?;
        }
        Ok(last)
    }

    fn eval_block_scoped(
        &mut self,
        block: &Block<'a>,
        input: Value<'a>,
        extra_bindings: &[(&str, Value<'a>)],
    ) -> Result<Value<'a>, Control<'a>> {
        let alloc = self.alloc;
        let saved_vars = self.state.vars.fork();
        let saved_input = core::mem::replace(&mut self.current_input, input.share());
        let mut locals: AVec<'a, ARc<'a, str>> = AVec::with_capacity_in(extra_bindings.len(), alloc);
        for &(name, ref val) in extra_bindings {
            locals.push(Rc::from_str_in(name, alloc));
            self.state.set_var(name, val.share(), false);
        }

        let mut last = Value::Nothing;
        let mut outcome: Result<Value<'a>, Control<'a>> = Ok(Value::Nothing);
        for stmt in block.stmts.iter() {
            match self.eval_stmt(stmt, false, Some(&mut locals)) {
                Ok(v) => last = v,
                Err(ctrl) => {
                    outcome = Err(ctrl);
                    break;
                }
            }
        }
        if outcome.is_ok() {
            outcome = Ok(last);
        }

        // Restore lexical scope while preserving mutations to outer `mut` variables.
        let mut restored = saved_vars;
        let mut updates: AVec<'a, (ARc<'a, str>, VarSlot<'a>)> = AVec::new_in(alloc);
        restored.for_each(&mut |k, old_slot| {
            if old_slot.mutable && !locals.iter().any(|l| l.as_ref() == k.as_ref()) {
                if let Some(new_slot) = self.state.vars.get(k.as_ref()) {
                    updates.push((k.share(), new_slot.share()));
                }
            }
        });
        for (k, slot) in updates.into_iter() {
            restored.insert(k, slot);
        }
        self.state.vars = restored;
        self.current_input = saved_input;
        outcome
    }

    fn eval_stmt(
        &mut self,
        stmt: &Stmt<'a>,
        interactive_tail: bool,
        mut locals: Option<&mut AVec<'a, ARc<'a, str>>>,
    ) -> Result<Value<'a>, Control<'a>> {
        match stmt {
            Stmt::Let { name, mutable, ty, value, span } => {
                self.charge(1, *span)?;
                let val = self.eval_expr_inner(value, Value::Nothing, false)?;
                if let Some(t) = ty {
                    check_type_ann(&val, *t, *span, self.alloc)?;
                }
                if let Some(loc) = locals.as_deref_mut() {
                    loc.push(name.share());
                }
                self.state.set_var(name.as_ref(), val, *mutable);
                Ok(Value::Nothing)
            }
            Stmt::Assign { target, op, value, span } => {
                self.charge(1, *span)?;
                let rhs = self.eval_expr_inner(value, Value::Nothing, false)?;
                self.apply_assign(target, *op, rhs, *span)?;
                Ok(Value::Nothing)
            }
            Stmt::Def { name, params, body, span } => {
                self.charge(1, *span)?;
                let def = Rc::new_in(
                    DefData {
                        name: name.share(),
                        params: params.clone(),
                        body: body.share(),
                        span: *span,
                    },
                    self.alloc,
                );
                self.state.defs.insert(name.share(), def);
                self.state.generation = self.state.generation.wrapping_add(1);
                Ok(Value::Nothing)
            }
            Stmt::Alias { name, expansion, span } => {
                self.charge(1, *span)?;
                self.state.aliases.insert(name.share(), expansion.share());
                self.state.generation = self.state.generation.wrapping_add(1);
                Ok(Value::Nothing)
            }
            Stmt::For { var, iter, body, span } => {
                self.charge(1, *span)?;
                let iterable = self.eval_expr_inner(iter, Value::Nothing, false)?;
                self.eval_for(var.as_ref(), iterable, body, *span)?;
                Ok(Value::Nothing)
            }
            Stmt::While { cond, body, span } => {
                loop {
                    self.charge(1, *span)?;
                    let c = self.eval_expr_inner(cond, Value::Nothing, false)?;
                    if !c.is_truthy() {
                        break;
                    }
                    match self.eval_block_scoped(body, Value::Nothing, &[]) {
                        Ok(_) => {}
                        Err(Control::Break(_)) => break,
                        Err(Control::Continue(_)) => continue,
                        Err(other) => return Err(other),
                    }
                }
                Ok(Value::Nothing)
            }
            Stmt::Loop { body, span } => {
                loop {
                    self.charge(1, *span)?;
                    match self.eval_block_scoped(body, Value::Nothing, &[]) {
                        Ok(_) => {}
                        Err(Control::Break(_)) => break,
                        Err(Control::Continue(_)) => continue,
                        Err(other) => return Err(other),
                    }
                }
                Ok(Value::Nothing)
            }
            Stmt::Break(span) => Err(Control::Break(*span)),
            Stmt::Continue(span) => Err(Control::Continue(*span)),
            Stmt::Return { value, span } => {
                self.charge(1, *span)?;
                let val = match value {
                    Some(e) => self.eval_expr_inner(e, Value::Nothing, false)?,
                    None => Value::Nothing,
                };
                Err(Control::Return(val))
            }
            Stmt::Expr(expr) => self.eval_expr_inner(expr, Value::Nothing, interactive_tail),
        }
    }

    fn eval_for(
        &mut self,
        var: &str,
        iterable: Value<'a>,
        body: &Block<'a>,
        span: Span,
    ) -> Result<(), Control<'a>> {
        match iterable {
            Value::Nothing => Ok(()),
            Value::List(items) => {
                for i in 0..items.len() {
                    self.charge(1, span)?;
                    let item = items.get(i).unwrap().share();
                    match self.eval_block_scoped(body, Value::Nothing, &[(var, item)]) {
                        Ok(_) => {}
                        Err(Control::Break(_)) => break,
                        Err(Control::Continue(_)) => continue,
                        Err(other) => return Err(other),
                    }
                }
                Ok(())
            }
            Value::Table(t) => {
                for r in 0..t.nrows() {
                    self.charge(1, span)?;
                    let item = Value::Record(t.row_record(r).unwrap());
                    match self.eval_block_scoped(body, Value::Nothing, &[(var, item)]) {
                        Ok(_) => {}
                        Err(Control::Break(_)) => break,
                        Err(Control::Continue(_)) => continue,
                        Err(other) => return Err(other),
                    }
                }
                Ok(())
            }
            Value::Range(r) => {
                if r.step == 0 {
                    return Err(Control::Error(ShellError::new(
                        ErrorKind::Type,
                        span,
                        "range step cannot be 0",
                        self.alloc,
                    )));
                }
                let mut cur = r.start;
                loop {
                    if let Some(end) = r.end {
                        let in_bounds = if r.step > 0 {
                            if r.inclusive { cur <= end } else { cur < end }
                        } else if r.inclusive {
                            cur >= end
                        } else {
                            cur > end
                        };
                        if !in_bounds {
                            break;
                        }
                    }
                    self.charge(1, span)?;
                    match self.eval_block_scoped(body, Value::Nothing, &[(var, Value::Int(cur))]) {
                        Ok(_) => {}
                        Err(Control::Break(_)) => break,
                        Err(Control::Continue(_)) => {}
                        Err(other) => return Err(other),
                    }
                    match cur.checked_add(r.step) {
                        Some(next) => cur = next,
                        None => break,
                    }
                }
                Ok(())
            }
            other => {
                let mut msg = AString::from_str_in("cannot iterate over ", self.alloc);
                msg.push_str(other.type_name());
                msg.push_str(" in `for` loop");
                Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), self.alloc)))
            }
        }
    }

    fn apply_assign(
        &mut self,
        target: &AssignTarget<'a>,
        op: AssignOp,
        rhs: Value<'a>,
        span: Span,
    ) -> Result<(), Control<'a>> {
        let alloc = self.alloc;
        // `$env.VAR = value`
        if target.var.as_ref() == "env" && target.path.len() == 1 {
            if let PathKey::Field(key) = &target.path[0].key {
                let new_text = match op {
                    AssignOp::Set => rhs.to_display_string(alloc),
                    AssignOp::AddSet | AssignOp::AppendSet => {
                        let mut s = AString::from_str_in(
                            self.state.get_env(key.as_ref()).unwrap_or(""),
                            alloc,
                        );
                        let _ = rhs.format_cell(&mut s);
                        s
                    }
                    _ => {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::Type,
                            span,
                            "unsupported assignment operator on `$env`",
                            alloc,
                        )));
                    }
                };
                self.state.set_env(key.as_ref(), new_text.as_str());
                return Ok(());
            }
        }

        let Some(slot) = self.state.get_var(target.var.as_ref()) else {
            let mut msg = AString::from_str_in("undefined variable `$", alloc);
            msg.push_str(target.var.as_ref());
            msg.push('`');
            return Err(Control::Error(ShellError::new(
                ErrorKind::Undefined,
                target.span,
                msg.as_str(),
                alloc,
            )));
        };
        if !slot.mutable {
            let mut msg = AString::from_str_in("cannot assign to immutable variable `$", alloc);
            msg.push_str(target.var.as_ref());
            msg.push('`');
            return Err(Control::Error(
                ShellError::new(ErrorKind::Immutable, target.span, msg.as_str(), alloc)
                    .with_help("declare with `mut` instead of `let`", alloc),
            ));
        }

        let root = slot.value.share();
        let updated = if target.path.is_empty() {
            self.apply_assign_op(op, root, rhs, span)?
        } else {
            self.assign_at_path(root, target.path.as_slice(), op, rhs, span)?
        };
        self.state.set_var(target.var.as_ref(), updated, true);
        Ok(())
    }

    fn apply_assign_op(
        &mut self,
        op: AssignOp,
        lhs: Value<'a>,
        rhs: Value<'a>,
        span: Span,
    ) -> Result<Value<'a>, Control<'a>> {
        match op {
            AssignOp::Set => Ok(rhs),
            AssignOp::AddSet => self.eval_binary_val(BinaryOp::Add, lhs, rhs, span),
            AssignOp::SubSet => self.eval_binary_val(BinaryOp::Sub, lhs, rhs, span),
            AssignOp::MulSet => self.eval_binary_val(BinaryOp::Mul, lhs, rhs, span),
            AssignOp::DivSet => self.eval_binary_val(BinaryOp::Div, lhs, rhs, span),
            AssignOp::AppendSet => self.eval_binary_val(BinaryOp::Append, lhs, rhs, span),
        }
    }

    fn assign_at_path(
        &mut self,
        container: Value<'a>,
        steps: &[PathStep<'a>],
        op: AssignOp,
        rhs: Value<'a>,
        span: Span,
    ) -> Result<Value<'a>, Control<'a>> {
        let alloc = self.alloc;
        let (first, rest) = (&steps[0], &steps[1..]);
        match (&first.key, container) {
            (PathKey::Field(name), Value::Record(mut rec)) => {
                if rest.is_empty() {
                    let val = if op == AssignOp::Set {
                        rhs
                    } else {
                        let old = rec.get(name.as_ref()).map(Share::share).ok_or_else(|| {
                            let mut msg = AString::from_str_in("record has no field `", alloc);
                            msg.push_str(name.as_ref());
                            msg.push('`');
                            Control::Error(ShellError::new(
                                ErrorKind::MissingColumn,
                                first.span,
                                msg.as_str(),
                                alloc,
                            ))
                        })?;
                        self.apply_assign_op(op, old, rhs, span)?
                    };
                    rec.insert(name.share(), val);
                } else {
                    let child = rec.get(name.as_ref()).map(Share::share).ok_or_else(|| {
                        let mut msg = AString::from_str_in("record has no field `", alloc);
                        msg.push_str(name.as_ref());
                        msg.push('`');
                        Control::Error(ShellError::new(
                            ErrorKind::MissingColumn,
                            first.span,
                            msg.as_str(),
                            alloc,
                        ))
                    })?;
                    let updated_child = self.assign_at_path(child, rest, op, rhs, span)?;
                    rec.insert(name.share(), updated_child);
                }
                Ok(Value::Record(rec))
            }
            (PathKey::Index(idx), Value::List(mut items)) => {
                let Some(old) = items.get(*idx).map(Share::share) else {
                    let mut msg = AString::new_in(alloc);
                    let _ = write!(msg, "index {} out of bounds for list of length {}", idx, items.len());
                    return Err(Control::Error(ShellError::new(
                        ErrorKind::IndexOutOfBounds,
                        first.span,
                        msg.as_str(),
                        alloc,
                    )));
                };
                let updated_elem = if rest.is_empty() {
                    self.apply_assign_op(op, old, rhs, span)?
                } else {
                    self.assign_at_path(old, rest, op, rhs, span)?
                };
                items.set(*idx, updated_elem);
                Ok(Value::List(items))
            }
            (PathKey::Index(row), Value::Table(mut table)) => {
                let Some(rec) = table.row_record(*row) else {
                    let mut msg = AString::new_in(alloc);
                    let _ = write!(msg, "row index {} out of bounds for table with {} rows", row, table.nrows());
                    return Err(Control::Error(ShellError::new(
                        ErrorKind::IndexOutOfBounds,
                        first.span,
                        msg.as_str(),
                        alloc,
                    )));
                };
                let updated_rec = if rest.is_empty() {
                    self.apply_assign_op(op, Value::Record(rec), rhs, span)?
                } else {
                    self.assign_at_path(Value::Record(rec), rest, op, rhs, span)?
                };
                let Value::Record(new_rec) = updated_rec else {
                    return Err(Control::Error(ShellError::new(
                        ErrorKind::Type,
                        span,
                        "table row assignment must produce a record",
                        alloc,
                    )));
                };
                for c in 0..table.ncols() {
                    let cname = table.columns.get(c).unwrap().as_ref();
                    let cell = new_rec.get(cname).map(Share::share).unwrap_or(Value::Nothing);
                    let mut col = table.cols.get(c).unwrap().share();
                    col.set(*row, cell);
                    table.cols.set(c, col);
                }
                Ok(Value::Table(table))
            }
            (_, other) => {
                let mut msg = AString::from_str_in("cannot assign through path on ", alloc);
                msg.push_str(other.type_name());
                Err(Control::Error(ShellError::new(ErrorKind::Type, first.span, msg.as_str(), alloc)))
            }
        }
    }

    pub(crate) fn eval_expr(
        &mut self,
        expr: &Expr<'a>,
        input: Value<'a>,
    ) -> Result<Value<'a>, Control<'a>> {
        self.eval_expr_inner(expr, input, false)
    }

    pub(crate) fn eval_expr_with_row(
        &mut self,
        expr: &Expr<'a>,
        row: Value<'a>,
    ) -> Result<Value<'a>, Control<'a>> {
        let prev_row = core::mem::replace(&mut self.row_ctx, Some(row.share()));
        let prev_in = core::mem::replace(&mut self.current_input, row.share());
        let res = self.eval_expr_inner(expr, row, false);
        self.row_ctx = prev_row;
        self.current_input = prev_in;
        res
    }

    fn eval_expr_inner(
        &mut self,
        expr: &Expr<'a>,
        input: Value<'a>,
        interactive_tail: bool,
    ) -> Result<Value<'a>, Control<'a>> {
        self.charge(1, expr.span)?;
        let alloc = self.alloc;
        match &expr.kind {
            ExprKind::Nothing => Ok(Value::Nothing),
            ExprKind::Bool(b) => Ok(Value::Bool(*b)),
            ExprKind::Int(n) => Ok(Value::Int(*n)),
            ExprKind::Float(f) => Ok(Value::Float(*f)),
            ExprKind::String(s) => Ok(Value::String(s.share())),
            ExprKind::Size(n) => Ok(Value::Size(Bytes(*n))),
            ExprKind::Duration(n) => Ok(Value::Duration(Nanos(*n))),
            ExprKind::Interpolated(parts) => {
                let mut out = AString::new_in(alloc);
                for part in parts.iter() {
                    match part {
                        InterpPart::Literal(lit) => out.push_str(lit.as_ref()),
                        InterpPart::Expr(e) => {
                            let v = self.eval_expr_inner(e, Value::Nothing, false)?;
                            let _ = v.format_cell(&mut out);
                        }
                    }
                }
                Ok(Value::string(out.as_str(), alloc))
            }
            ExprKind::List(items) => {
                let mut out = PVec::new(alloc);
                for item in items.iter() {
                    out.push(self.eval_expr_inner(item, Value::Nothing, false)?);
                }
                Ok(Value::List(out))
            }
            ExprKind::Record(entries) => {
                let mut rec = Record::new(alloc);
                for (k, vexpr) in entries.iter() {
                    let v = self.eval_expr_inner(vexpr, Value::Nothing, false)?;
                    rec.insert(k.share(), v);
                }
                Ok(Value::Record(rec))
            }
            ExprKind::Table { headers, rows } => {
                let mut cols = PVec::new(alloc);
                for h in headers.iter() {
                    cols.push(h.share());
                }
                let mut table = Table::new(cols, alloc);
                for row_exprs in rows.iter() {
                    let mut vals = AVec::with_capacity_in(row_exprs.len(), alloc);
                    for cell in row_exprs.iter() {
                        vals.push(self.eval_expr_inner(cell, Value::Nothing, false)?);
                    }
                    table.push_row_values(vals.as_slice());
                }
                Ok(Value::Table(table))
            }
            ExprKind::Closure { params, body } => {
                let captured = self.state.capture_vars();
                let data = ClosureData {
                    params: params.clone(),
                    body: body.share(),
                    captured,
                    span: expr.span,
                };
                Ok(Value::Closure(Rc::new_in(data, alloc)))
            }
            ExprKind::Var(name) => self.eval_var(name.as_ref(), expr.span),
            ExprKind::CellPath { base, steps } => {
                let mut cur = self.eval_expr_inner(base, input, false)?;
                for step in steps.iter() {
                    let member = match &step.key {
                        PathKey::Field(name) => {
                            PathMember::Field { name: name.share(), optional: step.optional }
                        }
                        PathKey::Index(index) => {
                            PathMember::Index { index: *index, optional: step.optional }
                        }
                    };
                    cur = cur.follow_member(&member, step.span, alloc).map_err(Control::Error)?;
                }
                Ok(cur)
            }
            ExprKind::BarePath(steps) => {
                if let Some(row) = self.row_ctx.as_ref().map(Share::share) {
                    let mut cur = row;
                    for step in steps.iter() {
                        let member = match &step.key {
                            PathKey::Field(name) => {
                                PathMember::Field { name: name.share(), optional: step.optional }
                            }
                            PathKey::Index(index) => {
                                PathMember::Index { index: *index, optional: step.optional }
                            }
                        };
                        cur = cur.follow_member(&member, step.span, alloc).map_err(Control::Error)?;
                    }
                    Ok(cur)
                } else {
                    let members = path_steps_to_members(steps.as_slice(), alloc);
                    Ok(Value::CellPath(Rc::from_slice_in(members.as_slice(), alloc)))
                }
            }
            ExprKind::Unary { op, expr: inner } => {
                let val = self.eval_expr_inner(inner, Value::Nothing, false)?;
                self.eval_unary(*op, val, expr.span)
            }
            ExprKind::Binary { op, left, right } => {
                if *op == BinaryOp::And {
                    let lhs = self.eval_expr_inner(left, Value::Nothing, false)?;
                    if !lhs.is_truthy() {
                        return Ok(Value::Bool(false));
                    }
                    let rhs = self.eval_expr_inner(right, Value::Nothing, false)?;
                    return Ok(Value::Bool(rhs.is_truthy()));
                }
                if *op == BinaryOp::Or {
                    let lhs = self.eval_expr_inner(left, Value::Nothing, false)?;
                    if lhs.is_truthy() {
                        return Ok(Value::Bool(true));
                    }
                    let rhs = self.eval_expr_inner(right, Value::Nothing, false)?;
                    return Ok(Value::Bool(rhs.is_truthy()));
                }
                let lhs = self.eval_expr_inner(left, Value::Nothing, false)?;
                let rhs = self.eval_expr_inner(right, Value::Nothing, false)?;
                self.eval_binary_val(*op, lhs, rhs, expr.span)
            }
            ExprKind::Range { start, step, end, inclusive } => {
                let s = match start {
                    Some(e) => match self.eval_expr_inner(e, Value::Nothing, false)? {
                        Value::Int(n) => n,
                        other => {
                            let mut msg = AString::from_str_in("range start must be int, got ", alloc);
                            msg.push_str(other.type_name());
                            return Err(Control::Error(ShellError::new(
                                ErrorKind::Type,
                                e.span,
                                msg.as_str(),
                                alloc,
                            )));
                        }
                    },
                    None => 0,
                };
                let delta = match step {
                    Some(e) => match self.eval_expr_inner(e, Value::Nothing, false)? {
                        Value::Int(second) => second.checked_sub(s).ok_or_else(|| {
                            Control::Error(ShellError::new(
                                ErrorKind::Overflow,
                                e.span,
                                "range step overflow",
                                alloc,
                            ))
                        })?,
                        other => {
                            let mut msg = AString::from_str_in("range step must be int, got ", alloc);
                            msg.push_str(other.type_name());
                            return Err(Control::Error(ShellError::new(
                                ErrorKind::Type,
                                e.span,
                                msg.as_str(),
                                alloc,
                            )));
                        }
                    },
                    None => 1,
                };
                let e_val = match end {
                    Some(e) => match self.eval_expr_inner(e, Value::Nothing, false)? {
                        Value::Int(n) => Some(n),
                        other => {
                            let mut msg = AString::from_str_in("range end must be int, got ", alloc);
                            msg.push_str(other.type_name());
                            return Err(Control::Error(ShellError::new(
                                ErrorKind::Type,
                                e.span,
                                msg.as_str(),
                                alloc,
                            )));
                        }
                    },
                    None => None,
                };
                Ok(Value::Range(RangeVal { start: s, step: delta, end: e_val, inclusive: *inclusive }))
            }
            ExprKind::Pipeline(stages) => {
                let mut acc = input;
                let last_idx = stages.len().saturating_sub(1);
                for (i, stage) in stages.iter().enumerate() {
                    let stage_interactive = interactive_tail && i == last_idx;
                    let prev_in = core::mem::replace(&mut self.current_input, acc.share());
                    let res = self.eval_expr_inner(stage, acc, stage_interactive);
                    self.current_input = prev_in;
                    acc = res?;
                }
                Ok(acc)
            }
            ExprKind::Call(call) => self.eval_call(call, input, interactive_tail),
            ExprKind::If { cond, then_block, else_branch } => {
                let c = self.eval_expr_inner(cond, Value::Nothing, false)?;
                if c.is_truthy() {
                    self.eval_block_scoped(then_block, input, &[])
                } else if let Some(else_expr) = else_branch {
                    self.eval_expr_inner(else_expr, input, false)
                } else {
                    Ok(Value::Nothing)
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                let val = self.eval_expr_inner(scrutinee, Value::Nothing, false)?;
                for arm in arms.iter() {
                    self.charge(1, arm.span)?;
                    if let Some(binding) = self.match_pattern(&arm.pattern, &val)? {
                        let extra: AVec<'a, (&str, Value<'a>)> = {
                            let mut v = AVec::new_in(alloc);
                            if let Some((bname, bval)) = &binding {
                                v.push((bname.as_ref(), bval.share()));
                            }
                            v
                        };
                        let saved_vars = self.state.vars.fork();
                        for &(k, ref bv) in extra.iter() {
                            self.state.set_var(k, bv.share(), false);
                        }
                        let guard_ok = match &arm.guard {
                            Some(g) => match self.eval_expr_inner(g, Value::Nothing, false) {
                                Ok(gv) => gv.is_truthy(),
                                Err(e) => {
                                    self.state.vars = saved_vars;
                                    return Err(e);
                                }
                            },
                            None => true,
                        };
                        if !guard_ok {
                            self.state.vars = saved_vars;
                            continue;
                        }
                        let res = self.eval_expr_inner(&arm.body, input.share(), false);
                        // Preserve mutations to outer `mut` variables.
                        let mut restored = saved_vars;
                        let mut updates: AVec<'a, (ARc<'a, str>, VarSlot<'a>)> = AVec::new_in(alloc);
                        restored.for_each(&mut |k, old_slot| {
                            let shadowed = extra.iter().any(|(ek, _)| *ek == k.as_ref());
                            if old_slot.mutable && !shadowed {
                                if let Some(ns) = self.state.vars.get(k.as_ref()) {
                                    updates.push((k.share(), ns.share()));
                                }
                            }
                        });
                        for (k, s) in updates.into_iter() {
                            restored.insert(k, s);
                        }
                        self.state.vars = restored;
                        return res;
                    }
                }
                Ok(Value::Nothing)
            }
            ExprKind::Try { try_block, catch_var, catch_block } => {
                let saved_state = self.state.fork();
                match self.eval_block_scoped(try_block, input, &[]) {
                    Ok(v) => Ok(v),
                    Err(Control::Error(err)) => {
                        self.state = saved_state;
                        if let Some(cb) = catch_block {
                            let err_rec = error_to_record(&err, alloc);
                            if let Some(cv) = catch_var {
                                self.eval_block_scoped(
                                    cb,
                                    err_rec.share(),
                                    &[(cv.as_ref(), err_rec)],
                                )
                            } else {
                                self.eval_block_scoped(cb, err_rec, &[])
                            }
                        } else {
                            Ok(Value::Nothing)
                        }
                    }
                    Err(other) => Err(other),
                }
            }
            ExprKind::Block(block) => self.eval_block_scoped(block, input, &[]),
            ExprKind::RustCell { code } => self.eval_rust_cell(code.as_str(), input, expr.span),
        }
    }

    fn eval_var(&self, name: &str, span: Span) -> Result<Value<'a>, Control<'a>> {
        if name == "env" {
            return Ok(Value::Record(self.state.env_record()));
        }
        if name == "in" {
            return Ok(self.current_input.share());
        }
        if name == "it" {
            if let Some(row) = &self.row_ctx {
                return Ok(row.share());
            }
        }
        if let Some(slot) = self.state.get_var(name) {
            return Ok(slot.value.share());
        }
        let mut msg = AString::from_str_in("undefined variable `$", self.alloc);
        msg.push_str(name);
        msg.push('`');
        Err(Control::Error(ShellError::new(ErrorKind::Undefined, span, msg.as_str(), self.alloc)))
    }

    fn eval_unary(&self, op: UnaryOp, val: Value<'a>, span: Span) -> Result<Value<'a>, Control<'a>> {
        let alloc = self.alloc;
        match op {
            UnaryOp::Not => Ok(Value::Bool(!val.is_truthy())),
            UnaryOp::Neg => match val {
                Value::Int(n) => n.checked_neg().map(Value::Int).ok_or_else(|| {
                    Control::Error(ShellError::new(ErrorKind::Overflow, span, "integer negation overflow", alloc))
                }),
                Value::Float(f) => Ok(Value::Float(-f)),
                Value::Size(Bytes(n)) => n.checked_neg().map(|x| Value::Size(Bytes(x))).ok_or_else(|| {
                    Control::Error(ShellError::new(ErrorKind::Overflow, span, "size negation overflow", alloc))
                }),
                Value::Duration(Nanos(n)) => n.checked_neg().map(|x| Value::Duration(Nanos(x))).ok_or_else(|| {
                    Control::Error(ShellError::new(ErrorKind::Overflow, span, "duration negation overflow", alloc))
                }),
                other => {
                    let mut msg = AString::from_str_in("cannot negate ", alloc);
                    msg.push_str(other.type_name());
                    Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
                }
            },
        }
    }

    pub(crate) fn eval_binary_val(
        &mut self,
        op: BinaryOp,
        lhs: Value<'a>,
        rhs: Value<'a>,
        span: Span,
    ) -> Result<Value<'a>, Control<'a>> {
        let alloc = self.alloc;
        match op {
            BinaryOp::Add => match (lhs, rhs) {
                (Value::Int(a), Value::Int(b)) => a.checked_add(b).map(Value::Int).ok_or_else(|| {
                    Control::Error(ShellError::new(ErrorKind::Overflow, span, "integer addition overflow", alloc))
                }),
                (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a + b)),
                (Value::Int(a), Value::Float(b)) => Ok(Value::Float((a as f64) + b)),
                (Value::Float(a), Value::Int(b)) => Ok(Value::Float(a + (b as f64))),
                (Value::Size(Bytes(a)), Value::Size(Bytes(b))) => {
                    a.checked_add(b).map(|x| Value::Size(Bytes(x))).ok_or_else(|| {
                        Control::Error(ShellError::new(ErrorKind::Overflow, span, "size addition overflow", alloc))
                    })
                }
                (Value::Duration(Nanos(a)), Value::Duration(Nanos(b))) => {
                    a.checked_add(b).map(|x| Value::Duration(Nanos(x))).ok_or_else(|| {
                        Control::Error(ShellError::new(ErrorKind::Overflow, span, "duration addition overflow", alloc))
                    })
                }
                (Value::Date(dt), Value::Duration(Nanos(d)))
                | (Value::Duration(Nanos(d)), Value::Date(dt)) => dt
                    .utc_ns
                    .checked_add(d)
                    .map(|ns| Value::Date(DateTime { utc_ns: ns, offset_secs: dt.offset_secs }))
                    .ok_or_else(|| {
                        Control::Error(ShellError::new(ErrorKind::Overflow, span, "datetime overflow", alloc))
                    }),
                (Value::String(a), Value::String(b)) => {
                    let mut s = AString::with_capacity_in(a.len() + b.len(), alloc);
                    s.push_str(a.as_ref());
                    s.push_str(b.as_ref());
                    Ok(Value::string(s.as_str(), alloc))
                }
                (Value::List(mut a), Value::List(b)) => {
                    for i in 0..b.len() {
                        self.charge(1, span)?;
                        a.push(b.get(i).unwrap().share());
                    }
                    Ok(Value::List(a))
                }
                (a, b) => binary_type_err("+", &a, &b, span, alloc),
            },
            BinaryOp::Sub => match (lhs, rhs) {
                (Value::Int(a), Value::Int(b)) => a.checked_sub(b).map(Value::Int).ok_or_else(|| {
                    Control::Error(ShellError::new(ErrorKind::Overflow, span, "integer subtraction overflow", alloc))
                }),
                (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a - b)),
                (Value::Int(a), Value::Float(b)) => Ok(Value::Float((a as f64) - b)),
                (Value::Float(a), Value::Int(b)) => Ok(Value::Float(a - (b as f64))),
                (Value::Size(Bytes(a)), Value::Size(Bytes(b))) => {
                    a.checked_sub(b).map(|x| Value::Size(Bytes(x))).ok_or_else(|| {
                        Control::Error(ShellError::new(ErrorKind::Overflow, span, "size subtraction overflow", alloc))
                    })
                }
                (Value::Duration(Nanos(a)), Value::Duration(Nanos(b))) => {
                    a.checked_sub(b).map(|x| Value::Duration(Nanos(x))).ok_or_else(|| {
                        Control::Error(ShellError::new(ErrorKind::Overflow, span, "duration subtraction overflow", alloc))
                    })
                }
                (Value::Date(dt), Value::Duration(Nanos(d))) => dt
                    .utc_ns
                    .checked_sub(d)
                    .map(|ns| Value::Date(DateTime { utc_ns: ns, offset_secs: dt.offset_secs }))
                    .ok_or_else(|| {
                        Control::Error(ShellError::new(ErrorKind::Overflow, span, "datetime overflow", alloc))
                    }),
                (Value::Date(a), Value::Date(b)) => a
                    .utc_ns
                    .checked_sub(b.utc_ns)
                    .map(|ns| Value::Duration(Nanos(ns)))
                    .ok_or_else(|| {
                        Control::Error(ShellError::new(ErrorKind::Overflow, span, "duration overflow", alloc))
                    }),
                (a, b) => binary_type_err("-", &a, &b, span, alloc),
            },
            BinaryOp::Mul => match (lhs, rhs) {
                (Value::Int(a), Value::Int(b)) => a.checked_mul(b).map(Value::Int).ok_or_else(|| {
                    Control::Error(ShellError::new(ErrorKind::Overflow, span, "integer multiplication overflow", alloc))
                }),
                (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a * b)),
                (Value::Int(a), Value::Float(b)) => Ok(Value::Float((a as f64) * b)),
                (Value::Float(a), Value::Int(b)) => Ok(Value::Float(a * (b as f64))),
                (Value::Size(Bytes(a)), Value::Int(b)) | (Value::Int(b), Value::Size(Bytes(a))) => {
                    a.checked_mul(b).map(|x| Value::Size(Bytes(x))).ok_or_else(|| {
                        Control::Error(ShellError::new(ErrorKind::Overflow, span, "size multiplication overflow", alloc))
                    })
                }
                (Value::Size(Bytes(a)), Value::Float(b))
                | (Value::Float(b), Value::Size(Bytes(a))) => {
                    Ok(Value::Size(Bytes(((a as f64) * b) as i64)))
                }
                (Value::Duration(Nanos(a)), Value::Int(b))
                | (Value::Int(b), Value::Duration(Nanos(a))) => {
                    a.checked_mul(b).map(|x| Value::Duration(Nanos(x))).ok_or_else(|| {
                        Control::Error(ShellError::new(ErrorKind::Overflow, span, "duration multiplication overflow", alloc))
                    })
                }
                (Value::Duration(Nanos(a)), Value::Float(b))
                | (Value::Float(b), Value::Duration(Nanos(a))) => {
                    Ok(Value::Duration(Nanos(((a as f64) * b) as i64)))
                }
                (Value::String(s), Value::Int(n)) => {
                    if n <= 0 {
                        return Ok(Value::string("", alloc));
                    }
                    let count = n as usize;
                    let total = s.len().saturating_mul(count);
                    if total > MAX_STRING_REPEAT {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::Budget,
                            span,
                            "repeated string exceeds memory budget",
                            alloc,
                        )));
                    }
                    let mut out = AString::with_capacity_in(total, alloc);
                    for _ in 0..count {
                        self.charge(1, span)?;
                        out.push_str(s.as_ref());
                    }
                    Ok(Value::string(out.as_str(), alloc))
                }
                (a, b) => binary_type_err("*", &a, &b, span, alloc),
            },
            BinaryOp::Div => match (lhs, rhs) {
                (Value::Int(a), Value::Int(b)) => {
                    if b == 0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "division by zero",
                            alloc,
                        )));
                    }
                    Ok(Value::Float((a as f64) / (b as f64)))
                }
                (Value::Float(a), Value::Float(b)) => {
                    if b == 0.0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "division by zero",
                            alloc,
                        )));
                    }
                    Ok(Value::Float(a / b))
                }
                (Value::Int(a), Value::Float(b)) => {
                    if b == 0.0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "division by zero",
                            alloc,
                        )));
                    }
                    Ok(Value::Float((a as f64) / b))
                }
                (Value::Float(a), Value::Int(b)) => {
                    if b == 0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "division by zero",
                            alloc,
                        )));
                    }
                    Ok(Value::Float(a / (b as f64)))
                }
                (Value::Size(Bytes(a)), Value::Int(b)) => {
                    if b == 0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "division by zero",
                            alloc,
                        )));
                    }
                    Ok(Value::Size(Bytes(a / b)))
                }
                (Value::Size(Bytes(a)), Value::Float(b)) => {
                    if b == 0.0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "division by zero",
                            alloc,
                        )));
                    }
                    Ok(Value::Size(Bytes(((a as f64) / b) as i64)))
                }
                (Value::Size(Bytes(a)), Value::Size(Bytes(b))) => {
                    if b == 0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "division by zero",
                            alloc,
                        )));
                    }
                    Ok(Value::Float((a as f64) / (b as f64)))
                }
                (Value::Duration(Nanos(a)), Value::Int(b)) => {
                    if b == 0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "division by zero",
                            alloc,
                        )));
                    }
                    Ok(Value::Duration(Nanos(a / b)))
                }
                (Value::Duration(Nanos(a)), Value::Float(b)) => {
                    if b == 0.0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "division by zero",
                            alloc,
                        )));
                    }
                    Ok(Value::Duration(Nanos(((a as f64) / b) as i64)))
                }
                (Value::Duration(Nanos(a)), Value::Duration(Nanos(b))) => {
                    if b == 0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "division by zero",
                            alloc,
                        )));
                    }
                    Ok(Value::Float((a as f64) / (b as f64)))
                }
                (a, b) => binary_type_err("/", &a, &b, span, alloc),
            },
            BinaryOp::FloorDiv => match (lhs, rhs) {
                (Value::Int(a), Value::Int(b)) => {
                    if b == 0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "division by zero",
                            alloc,
                        )));
                    }
                    let q = a.checked_div(b).ok_or_else(|| {
                        Control::Error(ShellError::new(ErrorKind::Overflow, span, "division overflow", alloc))
                    })?;
                    let r = a % b;
                    let floored = if r != 0 && ((r < 0) != (b < 0)) { q - 1 } else { q };
                    Ok(Value::Int(floored))
                }
                (Value::Float(a), Value::Float(b)) => {
                    if b == 0.0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "division by zero",
                            alloc,
                        )));
                    }
                    Ok(Value::Float(f64_floor(a / b)))
                }
                (Value::Int(a), Value::Float(b)) => {
                    if b == 0.0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "division by zero",
                            alloc,
                        )));
                    }
                    Ok(Value::Float(f64_floor((a as f64) / b)))
                }
                (Value::Float(a), Value::Int(b)) => {
                    if b == 0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "division by zero",
                            alloc,
                        )));
                    }
                    Ok(Value::Float(f64_floor(a / (b as f64))))
                }
                (Value::Size(Bytes(a)), Value::Int(b)) => {
                    if b == 0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "division by zero",
                            alloc,
                        )));
                    }
                    Ok(Value::Size(Bytes(a / b)))
                }
                (Value::Duration(Nanos(a)), Value::Int(b)) => {
                    if b == 0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "division by zero",
                            alloc,
                        )));
                    }
                    Ok(Value::Duration(Nanos(a / b)))
                }
                (a, b) => binary_type_err("//", &a, &b, span, alloc),
            },
            BinaryOp::Mod => match (lhs, rhs) {
                (Value::Int(a), Value::Int(b)) => {
                    if b == 0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "modulo by zero",
                            alloc,
                        )));
                    }
                    a.checked_rem(b).map(Value::Int).ok_or_else(|| {
                        Control::Error(ShellError::new(ErrorKind::Overflow, span, "modulo overflow", alloc))
                    })
                }
                (Value::Float(a), Value::Float(b)) => {
                    if b == 0.0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "modulo by zero",
                            alloc,
                        )));
                    }
                    Ok(Value::Float(a % b))
                }
                (Value::Int(a), Value::Float(b)) => {
                    if b == 0.0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "modulo by zero",
                            alloc,
                        )));
                    }
                    Ok(Value::Float((a as f64) % b))
                }
                (Value::Float(a), Value::Int(b)) => {
                    if b == 0 {
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::DivisionByZero,
                            span,
                            "modulo by zero",
                            alloc,
                        )));
                    }
                    Ok(Value::Float(a % (b as f64)))
                }
                (a, b) => binary_type_err("mod", &a, &b, span, alloc),
            },
            BinaryOp::Pow => match (lhs, rhs) {
                (Value::Int(a), Value::Int(b)) => {
                    if b >= 0 {
                        let exp = u32::try_from(b).map_err(|_| {
                            Control::Error(ShellError::new(ErrorKind::Overflow, span, "exponent overflow", alloc))
                        })?;
                        a.checked_pow(exp).map(Value::Int).ok_or_else(|| {
                            Control::Error(ShellError::new(ErrorKind::Overflow, span, "power overflow", alloc))
                        })
                    } else {
                        Ok(Value::Float(f64_powi(a as f64, b)))
                    }
                }
                (Value::Float(a), Value::Float(b)) => Ok(Value::Float(f64_powf(a, b))),
                (Value::Int(a), Value::Float(b)) => Ok(Value::Float(f64_powf(a as f64, b))),
                (Value::Float(a), Value::Int(b)) => Ok(Value::Float(f64_powi(a, b))),
                (a, b) => binary_type_err("**", &a, &b, span, alloc),
            },
            BinaryOp::Eq => Ok(Value::Bool(lhs == rhs)),
            BinaryOp::Ne => Ok(Value::Bool(lhs != rhs)),
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
                let Some(ord) = lhs.compare(&rhs) else {
                    return binary_type_err(
                        match op {
                            BinaryOp::Lt => "<",
                            BinaryOp::Le => "<=",
                            BinaryOp::Gt => ">",
                            _ => ">=",
                        },
                        &lhs,
                        &rhs,
                        span,
                        alloc,
                    );
                };
                let ok = match op {
                    BinaryOp::Lt => ord == Ordering::Less,
                    BinaryOp::Le => ord != Ordering::Greater,
                    BinaryOp::Gt => ord == Ordering::Greater,
                    BinaryOp::Ge => ord != Ordering::Less,
                    _ => false,
                };
                Ok(Value::Bool(ok))
            }
            BinaryOp::RegexMatch | BinaryOp::RegexNotMatch => {
                let (Some(text), Some(pat)) = (lhs.as_str(), rhs.as_str()) else {
                    return binary_type_err(
                        if op == BinaryOp::RegexMatch { "=~" } else { "!~" },
                        &lhs,
                        &rhs,
                        span,
                        alloc,
                    );
                };
                let cost = ((text.len().saturating_mul(pat.len().max(1))) / 64).max(1);
                self.charge(cost.min(u32::MAX as usize) as u32, span)?;
                let matched = match_pattern_str(text, pat, span, alloc)?;
                Ok(Value::Bool(if op == BinaryOp::RegexMatch { matched } else { !matched }))
            }
            BinaryOp::In | BinaryOp::NotIn => {
                let found = match &rhs {
                    Value::List(items) => {
                        let mut hit = false;
                        for i in 0..items.len() {
                            self.charge(1, span)?;
                            if items.get(i).unwrap() == &lhs {
                                hit = true;
                                break;
                            }
                        }
                        hit
                    }
                    Value::Record(rec) => match lhs.as_str() {
                        Some(k) => rec.contains_key(k),
                        None => false,
                    },
                    Value::Table(t) => match &lhs {
                        Value::String(k) => t.col_index(k.as_ref()).is_some(),
                        Value::Record(r) => {
                            let mut hit = false;
                            for row in 0..t.nrows() {
                                self.charge(1, span)?;
                                if t.row_record(row).as_ref() == Some(r) {
                                    hit = true;
                                    break;
                                }
                            }
                            hit
                        }
                        _ => false,
                    },
                    Value::String(haystack) => match lhs.as_str() {
                        Some(needle) => haystack.as_ref().contains(needle),
                        None => return binary_type_err("in", &lhs, &rhs, span, alloc),
                    },
                    Value::Range(r) => match lhs.as_int() {
                        Some(n) => {
                            if r.step == 0 {
                                false
                            } else if r.step > 0 {
                                let after_start = n >= r.start;
                                let before_end = match r.end {
                                    None => true,
                                    Some(e) => if r.inclusive { n <= e } else { n < e },
                                };
                                after_start && before_end && ((n - r.start) % r.step == 0)
                            } else {
                                let after_start = n <= r.start;
                                let before_end = match r.end {
                                    None => true,
                                    Some(e) => if r.inclusive { n >= e } else { n > e },
                                };
                                after_start && before_end && ((r.start - n) % (-r.step) == 0)
                            }
                        }
                        None => false,
                    },
                    _ => return binary_type_err("in", &lhs, &rhs, span, alloc),
                };
                Ok(Value::Bool(if op == BinaryOp::In { found } else { !found }))
            }
            BinaryOp::StartsWith => match (lhs.as_str(), rhs.as_str()) {
                (Some(a), Some(b)) => Ok(Value::Bool(a.starts_with(b))),
                _ => binary_type_err("starts-with", &lhs, &rhs, span, alloc),
            },
            BinaryOp::EndsWith => match (lhs.as_str(), rhs.as_str()) {
                (Some(a), Some(b)) => Ok(Value::Bool(a.ends_with(b))),
                _ => binary_type_err("ends-with", &lhs, &rhs, span, alloc),
            },
            BinaryOp::Append => match (lhs, rhs) {
                (Value::List(mut a), Value::List(b)) => {
                    for i in 0..b.len() {
                        self.charge(1, span)?;
                        a.push(b.get(i).unwrap().share());
                    }
                    Ok(Value::List(a))
                }
                (Value::List(mut a), val) => {
                    a.push(val);
                    Ok(Value::List(a))
                }
                (Value::Table(mut a), Value::Table(b)) => {
                    for r in 0..b.nrows() {
                        self.charge(1, span)?;
                        a.push_record(&b.row_record(r).unwrap());
                    }
                    Ok(Value::Table(a))
                }
                (Value::Table(mut a), Value::Record(r)) => {
                    a.push_record(&r);
                    Ok(Value::Table(a))
                }
                (Value::String(a), Value::String(b)) => {
                    let mut out = AString::with_capacity_in(a.len() + b.len(), alloc);
                    out.push_str(a.as_ref());
                    out.push_str(b.as_ref());
                    Ok(Value::string(out.as_str(), alloc))
                }
                (Value::Binary(a), Value::Binary(b)) => {
                    let mut out = AVec::with_capacity_in(a.len() + b.len(), alloc);
                    out.extend_from_slice(a.as_ref());
                    out.extend_from_slice(b.as_ref());
                    Ok(Value::binary(out.as_slice(), alloc))
                }
                (a, b) => binary_type_err("++", &a, &b, span, alloc),
            },
            BinaryOp::And => Ok(Value::Bool(lhs.is_truthy() && rhs.is_truthy())),
            BinaryOp::Or => Ok(Value::Bool(lhs.is_truthy() || rhs.is_truthy())),
            BinaryOp::Xor => Ok(Value::Bool(lhs.is_truthy() ^ rhs.is_truthy())),
        }
    }

    fn match_pattern(
        &mut self,
        pat: &Pattern<'a>,
        val: &Value<'a>,
    ) -> Result<Option<Option<(ARc<'a, str>, Value<'a>)>>, Control<'a>> {
        match pat {
            Pattern::Wildcard => Ok(Some(None)),
            Pattern::Binding(name) => Ok(Some(Some((name.share(), val.share())))),
            Pattern::Literal(expr) => {
                let lit = self.eval_expr_inner(expr, Value::Nothing, false)?;
                if &lit == val { Ok(Some(None)) } else { Ok(None) }
            }
            Pattern::Range { start, end, inclusive } => {
                let matched = match val.as_int() {
                    Some(n) => {
                        if *inclusive {
                            n >= *start && n <= *end
                        } else {
                            n >= *start && n < *end
                        }
                    }
                    None => false,
                };
                if matched { Ok(Some(None)) } else { Ok(None) }
            }
            Pattern::Or(pats) => {
                for p in pats.iter() {
                    if let Some(b) = self.match_pattern(p, val)? {
                        return Ok(Some(b));
                    }
                }
                Ok(None)
            }
        }
    }

    pub(crate) fn invoke_closure(
        &mut self,
        closure: &ClosureData<'a>,
        args: &[Value<'a>],
        input: Value<'a>,
        span: Span,
    ) -> Result<Value<'a>, Control<'a>> {
        let alloc = self.alloc;
        if self.call_depth >= MAX_CALL_DEPTH {
            return Err(Control::Error(ShellError::new(
                ErrorKind::Budget,
                span,
                "maximum call depth exceeded",
                alloc,
            )));
        }
        self.call_depth += 1;
        let saved_vars = self.state.vars.fork();
        let saved_input = core::mem::replace(&mut self.current_input, input.share());
        let saved_row = self.row_ctx.take();

        // Build lexical closure environment from captured snapshot + parameter bindings.
        let mut c_vars = PMap::new(alloc);
        closure.captured.for_each(&mut |k, v| {
            c_vars.insert(k.share(), VarSlot { value: v.share(), mutable: false });
        });
        for (i, pname) in closure.params.iter().enumerate() {
            let v = args.get(i).map(Share::share).unwrap_or(Value::Nothing);
            c_vars.insert(pname.share(), VarSlot { value: v, mutable: false });
        }
        self.state.vars = c_vars;

        let mut last = Value::Nothing;
        let mut res: Result<Value<'a>, Control<'a>> = Ok(Value::Nothing);
        for stmt in closure.body.stmts.iter() {
            match self.eval_stmt(stmt, false, None) {
                Ok(v) => last = v,
                Err(Control::Return(v)) => {
                    res = Ok(v);
                    break;
                }
                Err(ctrl) => {
                    res = Err(ctrl);
                    break;
                }
            }
        }
        if res.is_ok() {
            res = Ok(last);
        }

        self.state.vars = saved_vars;
        self.current_input = saved_input;
        self.row_ctx = saved_row;
        self.call_depth -= 1;
        res
    }

    fn eval_call(
        &mut self,
        call: &CallExpr<'a>,
        input: Value<'a>,
        interactive_tail: bool,
    ) -> Result<Value<'a>, Control<'a>> {
        let alloc = self.alloc;
        if !call.forced_external {
            // 1. Check aliases.
            if let Some(expansion) = self.state.aliases.get(call.head.as_ref()).map(Share::share) {
                return self.eval_alias(expansion.as_ref(), call, input, interactive_tail);
            }
            // 2. Check user `def`s.
            if let Some(def) = self.state.defs.get(call.head.as_ref()).map(Share::share) {
                return self.invoke_def(&def, call, input);
            }
            // 3. Check builtins.
            if let Some(val) = dispatch_builtin(self, call, input.share())? {
                return Ok(val);
            }
        }

        // 4. External process execution via `Request::SpawnExternal`.
        let mut ext_args: AVec<'a, AString<'a>> = AVec::with_capacity_in(call.args.len(), alloc);
        for arg in call.args.iter() {
            match arg {
                CallArg::Positional(e) => {
                    let v = self.eval_expr_inner(e, Value::Nothing, false)?;
                    ext_args.push(value_to_ext_arg(&v, alloc));
                }
                CallArg::Flag { name, short, value, .. } => {
                    let mut f = AString::new_in(alloc);
                    if *short {
                        f.push('-');
                    } else {
                        f.push_str("--");
                    }
                    f.push_str(name.as_ref());
                    ext_args.push(f);
                    if let Some(ve) = value {
                        let v = self.eval_expr_inner(ve, Value::Nothing, false)?;
                        ext_args.push(value_to_ext_arg(&v, alloc));
                    }
                }
            }
        }

        let stdin = match &input {
            Value::Nothing => None,
            Value::Binary(b) => {
                let mut v = AVec::with_capacity_in(b.len(), alloc);
                v.extend_from_slice(b.as_ref());
                Some(v)
            }
            Value::String(s) => {
                let mut v = AVec::with_capacity_in(s.len(), alloc);
                v.extend_from_slice(s.as_bytes());
                Some(v)
            }
            other => {
                let text = to_text_string(other, alloc);
                let mut v = AVec::with_capacity_in(text.len(), alloc);
                v.extend_from_slice(text.as_bytes());
                Some(v)
            }
        };

        let mut env_pairs = AVec::with_capacity_in(self.state.env.len(), alloc);
        self.state.env.collect_entries(&mut env_pairs);
        env_pairs.sort_unstable_by(|a, b| a.0.as_ref().cmp(b.0.as_ref()));

        let mode = if interactive_tail {
            ExternalMode::Interactive
        } else {
            ExternalMode::Capture
        };

        let req = Request::SpawnExternal {
            program: AString::from_str_in(call.head.as_ref(), alloc),
            args: ext_args,
            cwd: AString::from_str_in(self.state.cwd.as_ref(), alloc),
            env: env_pairs,
            stdin,
            mode,
        };
        let ans = self.request(req, call.head_span)?;
        let Answer::External(res) = ans else {
            return Err(Control::Error(ShellError::new(
                ErrorKind::External,
                call.head_span,
                "unexpected host answer for external command",
                alloc,
            )));
        };

        self.state.last_exit = res.exit_code;
        self.external_exit_set = true;

        if res.interactive {
            self.terminal_passthrough = true;
            return Ok(Value::Nothing);
        }

        if res.exit_code != 0 {
            let mut msg = AString::new_in(alloc);
            if !res.stderr.is_empty() {
                if let Ok(err_text) = core::str::from_utf8(res.stderr.as_slice()) {
                    msg.push_str(err_text.trim());
                }
            }
            if msg.is_empty() {
                let _ = write!(
                    msg,
                    "external command `{}` exited with status {}",
                    call.head.as_ref(),
                    res.exit_code
                );
            }
            return Err(Control::Error(ShellError::new(
                ErrorKind::External,
                call.head_span,
                msg.as_str(),
                alloc,
            )));
        }

        match core::str::from_utf8(res.stdout.as_slice()) {
            Ok(text) => Ok(Value::string(text, alloc)),
            Err(_) => Ok(Value::binary(res.stdout.as_slice(), alloc)),
        }
    }

    fn eval_alias(
        &mut self,
        expansion: &str,
        call: &CallExpr<'a>,
        input: Value<'a>,
        interactive_tail: bool,
    ) -> Result<Value<'a>, Control<'a>> {
        let alloc = self.alloc;
        if self.alias_depth >= MAX_ALIAS_DEPTH {
            return Err(Control::Error(ShellError::new(
                ErrorKind::Budget,
                call.head_span,
                "alias expansion recursion limit exceeded",
                alloc,
            )));
        }
        let mut parsed = crate::parse::parse(expansion, alloc).map_err(Control::Error)?;
        // Append invocation args onto the expanded head call if applicable.
        if parsed.stmts.len() == 1 && !call.args.is_empty() {
            if let Stmt::Expr(expr) = &mut parsed.stmts[0] {
                match &mut expr.kind {
                    ExprKind::Call(target_call) => {
                        for a in call.args.iter() {
                            target_call.args.push(a.clone());
                        }
                    }
                    ExprKind::Pipeline(stages) if !stages.is_empty() => {
                        if let ExprKind::Call(first_call) = &mut stages[0].kind {
                            for a in call.args.iter() {
                                first_call.args.push(a.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        self.alias_depth += 1;
        let mut last = Value::Nothing;
        let mut res = Ok(Value::Nothing);
        let n = parsed.stmts.len();
        for (i, stmt) in parsed.stmts.iter().enumerate() {
            let st_val = match stmt {
                Stmt::Expr(e) => self.eval_expr_inner(
                    e,
                    if i == 0 { input.share() } else { Value::Nothing },
                    interactive_tail && i + 1 == n,
                ),
                other => self.eval_stmt(other, false, None),
            };
            match st_val {
                Ok(v) => last = v,
                Err(e) => {
                    res = Err(e);
                    break;
                }
            }
        }
        self.alias_depth -= 1;
        if res.is_ok() { Ok(last) } else { res }
    }

    fn invoke_def(
        &mut self,
        def: &DefData<'a>,
        call: &CallExpr<'a>,
        input: Value<'a>,
    ) -> Result<Value<'a>, Control<'a>> {
        let alloc = self.alloc;
        if self.call_depth >= MAX_CALL_DEPTH {
            return Err(Control::Error(ShellError::new(
                ErrorKind::Budget,
                call.head_span,
                "maximum call depth exceeded",
                alloc,
            )));
        }

        // Evaluate all call arguments in the caller's scope before entering the callee.
        let mut pos_vals: AVec<'a, (Value<'a>, Span)> = AVec::new_in(alloc);
        let mut flag_vals: AVec<'a, (ARc<'a, str>, bool, Option<Value<'a>>, Span)> =
            AVec::new_in(alloc);
        let mut arg_i = 0usize;
        while arg_i < call.args.len() {
            match &call.args[arg_i] {
                CallArg::Positional(e) => {
                    let v = self.eval_expr_inner(e, Value::Nothing, false)?;
                    pos_vals.push((v, e.span));
                    arg_i += 1;
                }
                CallArg::Flag { name, short, value, span } => {
                    let takes_val = def.params.iter().any(|p| {
                        p.kind == ParamKind::Named
                            && if *short {
                                p.short.is_some_and(|ch| {
                                    let mut buf = [0u8; 4];
                                    name.as_ref() == ch.encode_utf8(&mut buf)
                                })
                            } else {
                                name.as_ref() == p.name.as_ref()
                            }
                    });
                    let v = match value {
                        Some(ve) => Some(self.eval_expr_inner(ve, Value::Nothing, false)?),
                        None if takes_val => {
                            if let Some(CallArg::Positional(next_e)) = call.args.get(arg_i + 1) {
                                arg_i += 1;
                                Some(self.eval_expr_inner(next_e, Value::Nothing, false)?)
                            } else {
                                None
                            }
                        }
                        None => None,
                    };
                    flag_vals.push((name.share(), *short, v, *span));
                    arg_i += 1;
                }
            }
        }

        let mut callee_vars = PMap::new(alloc);
        let mut pos_idx = 0usize;
        for param in def.params.iter() {
            let bound = match param.kind {
                ParamKind::Positional => {
                    if let Some((v, _)) = pos_vals.get(pos_idx) {
                        pos_idx += 1;
                        v.share()
                    } else if let Some(default_expr) = &param.default {
                        self.eval_expr_inner(default_expr, Value::Nothing, false)?
                    } else {
                        let mut msg = AString::from_str_in("missing required argument `", alloc);
                        msg.push_str(param.name.as_ref());
                        msg.push_str("` for `");
                        msg.push_str(def.name.as_ref());
                        msg.push('`');
                        return Err(Control::Error(ShellError::new(
                            ErrorKind::Arity,
                            call.head_span,
                            msg.as_str(),
                            alloc,
                        )));
                    }
                }
                ParamKind::OptionalPositional => {
                    if let Some((v, _)) = pos_vals.get(pos_idx) {
                        pos_idx += 1;
                        v.share()
                    } else if let Some(default_expr) = &param.default {
                        self.eval_expr_inner(default_expr, Value::Nothing, false)?
                    } else {
                        Value::Nothing
                    }
                }
                ParamKind::Rest => {
                    let mut rest = PVec::new(alloc);
                    while pos_idx < pos_vals.len() {
                        rest.push(pos_vals[pos_idx].0.share());
                        pos_idx += 1;
                    }
                    Value::List(rest)
                }
                ParamKind::Flag => {
                    let present = flag_vals.iter().any(|(fname, is_short, _, _)| {
                        if *is_short {
                            param.short.is_some_and(|ch| {
                                let mut buf = [0u8; 4];
                                fname.as_ref() == ch.encode_utf8(&mut buf)
                            })
                        } else {
                            fname.as_ref() == param.name.as_ref()
                        }
                    });
                    Value::Bool(present)
                }
                ParamKind::Named => {
                    let found = flag_vals.iter().find(|(fname, is_short, _, _)| {
                        if *is_short {
                            param.short.is_some_and(|ch| {
                                let mut buf = [0u8; 4];
                                fname.as_ref() == ch.encode_utf8(&mut buf)
                            })
                        } else {
                            fname.as_ref() == param.name.as_ref()
                        }
                    });
                    if let Some((_, _, Some(v), _)) = found {
                        v.share()
                    } else if let Some(default_expr) = &param.default {
                        self.eval_expr_inner(default_expr, Value::Nothing, false)?
                    } else {
                        Value::Nothing
                    }
                }
            };

            if let Some(ty) = param.ty {
                check_type_ann(&bound, ty, call.head_span, alloc)?;
            }
            callee_vars.insert(param.name.share(), VarSlot { value: bound, mutable: false });
        }

        self.call_depth += 1;
        let saved_vars = core::mem::replace(&mut self.state.vars, callee_vars);
        let saved_input = core::mem::replace(&mut self.current_input, input);
        let saved_row = self.row_ctx.take();

        let mut last = Value::Nothing;
        let mut res: Result<Value<'a>, Control<'a>> = Ok(Value::Nothing);
        for stmt in def.body.stmts.iter() {
            match self.eval_stmt(stmt, false, None) {
                Ok(v) => last = v,
                Err(Control::Return(v)) => {
                    res = Ok(v);
                    break;
                }
                Err(ctrl) => {
                    res = Err(ctrl);
                    break;
                }
            }
        }
        if res.is_ok() {
            res = Ok(last);
        }

        self.state.vars = saved_vars;
        self.current_input = saved_input;
        self.row_ctx = saved_row;
        self.call_depth -= 1;
        res
    }

    fn eval_rust_cell(
        &mut self,
        code: &str,
        input: Value<'a>,
        span: Span,
    ) -> Result<Value<'a>, Control<'a>> {
        let alloc = self.alloc;
        let input_json = to_json_string(&input, true, span, alloc).map_err(Control::Error)?;
        let mut prior_cells = AVec::with_capacity_in(self.state.rust_cells.len(), alloc);
        for i in 0..self.state.rust_cells.len() {
            prior_cells.push(self.state.rust_cells.get(i).unwrap().code.share());
        }
        let req = Request::EvalRust {
            prior_cells,
            code: AString::from_str_in(code, alloc),
            input_json,
            cwd: AString::from_str_in(self.state.cwd.as_ref(), alloc),
        };
        let ans = self.request(req, span)?;
        let Answer::Rust(res) = ans else {
            return Err(Control::Error(ShellError::new(
                ErrorKind::RustCell,
                span,
                "unexpected host answer for `rust { ... }` cell",
                alloc,
            )));
        };
        if !res.ok {
            let msg = if !res.stderr.is_empty() {
                res.stderr.as_str()
            } else {
                "rust cell failed"
            };
            return Err(Control::Error(ShellError::new(ErrorKind::RustCell, span, msg, alloc)));
        }
        self.state.push_rust_cell(code);
        if let Some(json_out) = &res.output_json {
            from_json_bytes(json_out.as_bytes(), span, alloc).map_err(Control::Error)
        } else {
            Ok(Value::string(res.stdout.as_str(), alloc))
        }
    }
}

// =============================================================================
// Helpers
// =============================================================================

fn value_to_ext_arg<'a>(v: &Value<'a>, alloc: Alloc<'a>) -> AString<'a> {
    match v {
        Value::CellPath(path) => {
            let mut out = AString::new_in(alloc);
            for (i, m) in path.iter().enumerate() {
                if i > 0 {
                    out.push('.');
                }
                match m {
                    PathMember::Field { name, .. } => out.push_str(name.as_ref()),
                    PathMember::Index { index, .. } => {
                        let _ = write!(out, "{index}");
                    }
                }
            }
            out
        }
        other => other.to_display_string(alloc),
    }
}

fn error_to_record<'a>(err: &ShellError<'a>, alloc: Alloc<'a>) -> Value<'a> {
    let mut rec = Record::new(alloc);
    rec.insert_str("kind", Value::string(err.kind.as_str(), alloc));
    rec.insert_str("msg", Value::string(err.message.as_str(), alloc));
    rec.insert_str("message", Value::string(err.message.as_str(), alloc));
    rec.insert_str(
        "help",
        match &err.help {
            Some(h) => Value::string(h.as_str(), alloc),
            None => Value::Nothing,
        },
    );
    Value::Record(rec)
}

fn check_type_ann<'a>(
    val: &Value<'a>,
    ty: TypeAnn,
    span: Span,
    alloc: Alloc<'a>,
) -> Result<(), Control<'a>> {
    let ok = match ty {
        TypeAnn::Any => true,
        TypeAnn::Nothing => matches!(val, Value::Nothing),
        TypeAnn::Bool => matches!(val, Value::Bool(_)),
        TypeAnn::Int => matches!(val, Value::Int(_)),
        TypeAnn::Float => matches!(val, Value::Float(_)),
        TypeAnn::Number => matches!(val, Value::Int(_) | Value::Float(_)),
        TypeAnn::String => matches!(val, Value::String(_)),
        TypeAnn::Size => matches!(val, Value::Size(_)),
        TypeAnn::Duration => matches!(val, Value::Duration(_)),
        TypeAnn::Date => matches!(val, Value::Date(_)),
        TypeAnn::Range => matches!(val, Value::Range(_)),
        TypeAnn::Binary => matches!(val, Value::Binary(_)),
        TypeAnn::List => matches!(val, Value::List(_) | Value::Table(_)),
        TypeAnn::Record => matches!(val, Value::Record(_)),
        TypeAnn::Table => matches!(val, Value::Table(_)),
        TypeAnn::Closure => matches!(val, Value::Closure(_)),
    };
    if ok {
        Ok(())
    } else {
        let mut msg = AString::new_in(alloc);
        let _ = write!(msg, "type mismatch: expected {:?}, got {}", ty, val.type_name());
        Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
    }
}

fn binary_type_err<'a>(
    op: &str,
    lhs: &Value<'a>,
    rhs: &Value<'a>,
    span: Span,
    alloc: Alloc<'a>,
) -> Result<Value<'a>, Control<'a>> {
    let mut msg = AString::new_in(alloc);
    let _ = write!(msg, "cannot apply `{op}` to {} and {}", lhs.type_name(), rhs.type_name());
    Err(Control::Error(ShellError::new(ErrorKind::Type, span, msg.as_str(), alloc)))
}

// =============================================================================
// Polynomial-time `no_std` Regex / Glob Matcher for `=~` and `!~`
// =============================================================================

#[derive(Clone)]
enum Atom<'a> {
    Any,
    Lit(char),
    Digit,
    NotDigit,
    Word,
    NotWord,
    Space,
    NotSpace,
    Class { negated: bool, ranges: AVec<'a, (char, char)> },
}

impl Atom<'_> {
    fn matches(&self, c: char) -> bool {
        match self {
            Atom::Any => c != '\n',
            Atom::Lit(ch) => c == *ch,
            Atom::Digit => c.is_ascii_digit(),
            Atom::NotDigit => !c.is_ascii_digit(),
            Atom::Word => c.is_ascii_alphanumeric() || c == '_',
            Atom::NotWord => !(c.is_ascii_alphanumeric() || c == '_'),
            Atom::Space => c.is_whitespace(),
            Atom::NotSpace => !c.is_whitespace(),
            Atom::Class { negated, ranges } => {
                let hit = ranges.iter().any(|&(lo, hi)| c >= lo && c <= hi);
                if *negated { !hit } else { hit }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quant {
    One,
    ZeroOrMore,
    OneOrMore,
    ZeroOrOne,
}

struct RegexTerm<'a> {
    atom: Atom<'a>,
    quant: Quant,
}

fn match_pattern_str<'a>(
    text: &str,
    pat: &str,
    span: Span,
    alloc: Alloc<'a>,
) -> Result<bool, Control<'a>> {
    // Support glob syntax when the pattern starts with `*` (e.g. `*.rs`).
    if pat.starts_with('*') {
        let glob = blaze_glob::Pattern::new_in(pat, alloc).map_err(|e| {
            let mut msg = AString::new_in(alloc);
            let _ = write!(msg, "invalid pattern `{pat}`: {e}");
            Control::Error(ShellError::new(ErrorKind::Format, span, msg.as_str(), alloc))
        })?;
        return Ok(glob.matches(text));
    }

    let mut chars: AVec<'a, char> = AVec::new_in(alloc);
    for c in text.chars() {
        chars.push(c);
    }

    for branch in split_unescaped_pipes(pat, alloc).iter() {
        if match_regex_branch(chars.as_slice(), branch.as_str(), span, alloc)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn split_unescaped_pipes<'a>(pat: &str, alloc: Alloc<'a>) -> AVec<'a, AString<'a>> {
    let mut branches = AVec::new_in(alloc);
    let mut cur = AString::new_in(alloc);
    let mut in_class = false;
    let mut escaped = false;
    for ch in pat.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
        } else if ch == '\\' {
            cur.push(ch);
            escaped = true;
        } else if ch == '[' && !in_class {
            in_class = true;
            cur.push(ch);
        } else if ch == ']' && in_class {
            in_class = false;
            cur.push(ch);
        } else if ch == '|' && !in_class {
            branches.push(cur);
            cur = AString::new_in(alloc);
        } else {
            cur.push(ch);
        }
    }
    branches.push(cur);
    branches
}

fn match_regex_branch<'a>(
    text: &[char],
    branch: &str,
    span: Span,
    alloc: Alloc<'a>,
) -> Result<bool, Control<'a>> {
    let mut bchars: AVec<'a, char> = AVec::new_in(alloc);
    for c in branch.chars() {
        bchars.push(c);
    }
    let mut at = 0usize;
    let anchored_start = if bchars.first() == Some(&'^') {
        at = 1;
        true
    } else {
        false
    };
    let mut end_len = bchars.len();
    let anchored_end = if end_len > at && bchars[end_len - 1] == '$' {
        // Count preceding backslashes to ensure `$` is unescaped.
        let mut bs = 0usize;
        let mut k = end_len - 1;
        while k > at && bchars[k - 1] == '\\' {
            bs += 1;
            k -= 1;
        }
        if bs % 2 == 0 {
            end_len -= 1;
            true
        } else {
            false
        }
    } else {
        false
    };

    let mut terms: AVec<'a, RegexTerm<'a>> = AVec::new_in(alloc);
    while at < end_len {
        let atom = match bchars[at] {
            '.' => {
                at += 1;
                Atom::Any
            }
            '\\' => {
                at += 1;
                if at >= end_len {
                    return Err(Control::Error(ShellError::new(
                        ErrorKind::Format,
                        span,
                        "trailing backslash in regex pattern",
                        alloc,
                    )));
                }
                let esc = bchars[at];
                at += 1;
                match esc {
                    'd' => Atom::Digit,
                    'D' => Atom::NotDigit,
                    'w' => Atom::Word,
                    'W' => Atom::NotWord,
                    's' => Atom::Space,
                    'S' => Atom::NotSpace,
                    'n' => Atom::Lit('\n'),
                    't' => Atom::Lit('\t'),
                    'r' => Atom::Lit('\r'),
                    other => Atom::Lit(other),
                }
            }
            '[' => {
                at += 1;
                let negated = if at < end_len && (bchars[at] == '^' || bchars[at] == '!') {
                    at += 1;
                    true
                } else {
                    false
                };
                let mut ranges = AVec::new_in(alloc);
                let mut closed = false;
                while at < end_len {
                    if bchars[at] == ']' && !ranges.is_empty() {
                        at += 1;
                        closed = true;
                        break;
                    }
                    let lo = bchars[at];
                    at += 1;
                    if at + 1 < end_len && bchars[at] == '-' && bchars[at + 1] != ']' {
                        let hi = bchars[at + 1];
                        at += 2;
                        ranges.push((lo, hi));
                    } else {
                        ranges.push((lo, lo));
                    }
                }
                if !closed {
                    return Err(Control::Error(ShellError::new(
                        ErrorKind::Format,
                        span,
                        "unclosed character class `[` in regex pattern",
                        alloc,
                    )));
                }
                Atom::Class { negated, ranges }
            }
            c => {
                at += 1;
                Atom::Lit(c)
            }
        };

        let quant = if at < end_len {
            match bchars[at] {
                '*' => {
                    at += 1;
                    Quant::ZeroOrMore
                }
                '+' => {
                    at += 1;
                    Quant::OneOrMore
                }
                '?' => {
                    at += 1;
                    Quant::ZeroOrOne
                }
                _ => Quant::One,
            }
        } else {
            Quant::One
        };
        terms.push(RegexTerm { atom, quant });
    }

    let start_max = if anchored_start { 0 } else { text.len() };
    for start in 0..=start_max {
        if dp_match_from(text, start, terms.as_slice(), anchored_end, alloc) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// O(text.len() * terms.len()) forward NFA-style DP matcher.
fn dp_match_from<'a>(
    text: &[char],
    start: usize,
    terms: &[RegexTerm<'a>],
    anchored_end: bool,
    alloc: Alloc<'a>,
) -> bool {
    let m = terms.len();
    let mut cur: AVec<'a, bool> = AVec::with_capacity_in(m + 1, alloc);
    let mut next: AVec<'a, bool> = AVec::with_capacity_in(m + 1, alloc);
    for _ in 0..=m {
        cur.push(false);
        next.push(false);
    }
    cur[0] = true;
    // Epsilon closure over nullable quantifiers (`*`, `?`).
    for j in 0..m {
        if cur[j] && matches!(terms[j].quant, Quant::ZeroOrMore | Quant::ZeroOrOne) {
            cur[j + 1] = true;
        }
    }

    if cur[m] && (!anchored_end || start == text.len()) {
        return true;
    }

    for i in start..text.len() {
        let ch = text[i];
        for b in next.iter_mut() {
            *b = false;
        }
        for j in 0..m {
            if !cur[j] {
                continue;
            }
            if terms[j].atom.matches(ch) {
                match terms[j].quant {
                    Quant::One | Quant::ZeroOrOne => {
                        next[j + 1] = true;
                    }
                    Quant::ZeroOrMore => {
                        next[j] = true;
                        next[j + 1] = true;
                    }
                    Quant::OneOrMore => {
                        // Staying in state `j` requires matching at least once, which we just did;
                        // also allow advancing to `j + 1` (and via epsilon closure from `j + 1`).
                        next[j] = true;
                        next[j + 1] = true;
                    }
                }
            }
        }
        // Epsilon closure on `next`.
        for j in 0..m {
            if next[j] && matches!(terms[j].quant, Quant::ZeroOrMore | Quant::ZeroOrOne) {
                next[j + 1] = true;
            }
        }
        core::mem::swap(&mut cur, &mut next);
        if cur[m] && (!anchored_end || i + 1 == text.len()) {
            return true;
        }
        if !cur.iter().any(|&b| b) {
            return false;
        }
    }
    cur[m]
}
