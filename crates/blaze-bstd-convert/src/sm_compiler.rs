//! Compiler pass that lowers imperative / blocking / async Rust functions into explicit
//! `#![blaze::sans_io]` state machines with exact forward-availability + backward-liveness
//! dataflow analysis, mutability tracking, intra-file return-type inference, and cycle-safe
//! control-flow lowering (`if`/`else`, `if let`, `match`, `loop`/`while`/`for`, `?`, nested
//! effect hoisting), plus an implicit-state struct splitter and a custom proc-macro ->
//! `comptime` / `const` reflection scaffold generator.

use alloc::{
    borrow::ToOwned,
    collections::{BTreeMap, BTreeSet},
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::fmt::Write as _;

use syntax::{
    AstNode, Edition, SourceFile, SyntaxKind,
    ast::{self, HasArgList, HasLoopBody, HasName, HasVisibility},
};

use crate::{
    analyze::{
        CInteropInfo, NewtypeCandidate, NewtypeKind, ProcMacroEntry, ProcMacroPattern,
        StructSmCandidate, line_of_offset,
    },
    catalog::{EffectKind, classify_call_effect, classify_macro_effect},
};

/// Upper bound on dataflow fixpoint iterations (`bounded-progress`).
const MAX_DATAFLOW_ITERS: usize = 64;

/// A basic block identifier in [`SmCfg`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub usize);

/// One pure statement executed synchronously before a block's terminator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PureStmt {
    pub code: String,
    pub defined_var: Option<String>,
    pub is_mut: bool,
    pub var_type: Option<String>,
}

/// How the original code handled the `Result` of an effectful expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnwrapMode {
    Raw,
    TryPropagate,
    UnwrapOrPanic,
    UnwrapOrDefault,
}

/// One external effect requested at a suspension point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectCall {
    pub kind: EffectKind,
    pub args: Vec<String>,
    pub raw_expr: String,
    pub line: usize,
}

/// What a basic block ends with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminator {
    /// Suspend execution, emit an [`EffectCall`], and resume in `resume` once the matching
    /// completion event arrives.
    Suspend {
        suspend_index: usize,
        effect: EffectCall,
        bind: Option<String>,
        bind_mut: bool,
        bind_type: Option<String>,
        unwrap_mode: UnwrapMode,
        resume: BlockId,
    },
    /// Branch synchronously on `cond` (including `let pat = expr`).
    Branch { cond: String, then_bb: BlockId, else_bb: BlockId },
    /// Multi-way match on `scrutinee`.
    Match { scrutinee: String, arms: Vec<(String, BlockId)> },
    /// Unconditional jump (loop back-edge or `if`/`match` join).
    Goto(BlockId),
    /// Return from the state machine (`State::Done`).
    Return(String),
    /// Open / unterminated block while building the CFG (never a sentinel `Return("()")`!).
    Open,
}

/// One basic block in the lowered effect CFG.
#[derive(Debug, Clone)]
pub struct BasicBlock {
    pub id: BlockId,
    pub stmts: Vec<PureStmt>,
    pub term: Terminator,
}

/// Lowered effect CFG for a single Rust function.
#[derive(Debug, Clone)]
pub struct SmCfg {
    pub fn_name: String,
    pub pascal_name: String,
    pub is_pub: bool,
    pub is_async: bool,
    pub params: Vec<(String, String)>,
    pub return_type: String,
    pub returns_result: bool,
    pub entry: BlockId,
    pub blocks: Vec<BasicBlock>,
    pub local_types: BTreeMap<String, String>,
    pub mut_vars: BTreeSet<String>,
    pub suspend_count: usize,
}

/// Exact dataflow result: backward liveness intersected with forward definite availability.
#[derive(Debug, Clone)]
pub struct LivenessInfo {
    pub live_in: Vec<BTreeSet<String>>,
    pub live_out: Vec<BTreeSet<String>>,
    pub avail_in: Vec<BTreeSet<String>>,
    pub avail_out: Vec<BTreeSet<String>>,
    /// `suspend_index -> exact minimal sorted list of in-scope live locals saved across that suspension`.
    pub saved_at_suspend: BTreeMap<usize, Vec<String>>,
    pub total_locals: usize,
    pub max_live_at_suspend: usize,
}

/// Output of compiling one Rust function into an explicit sans-IO state machine.
#[derive(Debug, Clone)]
pub struct CompiledStateMachine {
    pub fn_name: String,
    pub machine_name: String,
    pub state_count: usize,
    pub effect_kinds: BTreeSet<EffectKind>,
    pub liveness: LivenessInfo,
    pub sans_io_code: String,
    pub bstd_driver_code: String,
    pub sim_test_code: String,
}

/// Which allocator representation the emitted sans-IO state machine uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmAllocMode {
    /// True `bstd`-native explicit allocator: `Machine<'a>` carries `alloc: bstd::mem::Alloc<'a>`,
    /// strings are `bstd::mem::String<bstd::mem::Alloc<'a>>`, vectors are
    /// `bstd::vec::Vec<T, bstd::mem::Alloc<'a>>`, and formatting uses `bstd::mem::format_in!`.
    BstdExplicitAlloc,
    /// Plain `String` / `Vec<T>` (zero `bstd` dependency; useful for standalone verification or
    /// incremental pass-1 migration before allocator threading).
    PlainAlloc,
}

/// Compile every effectful / async / IO function in `source` (or one named `only_fn`).
pub fn compile_file_functions(source: &str, only_fn: Option<&str>) -> Vec<CompiledStateMachine> {
    compile_file_functions_opts(source, only_fn, None, SmAllocMode::PlainAlloc)
}

/// Compile functions with explicit prefix disambiguation and allocator mode (`bstd::mem::Alloc<'a>`
/// or plain `alloc`).
pub fn compile_file_functions_opts(
    source: &str,
    only_fn: Option<&str>,
    name_prefix: Option<&str>,
    alloc_mode: SmAllocMode,
) -> Vec<CompiledStateMachine> {
    let parse = SourceFile::parse(source, Edition::CURRENT);
    let root = parse.tree();

    // Pre-scan every function signature in the file so local return types (`let deps = deps_of(...)`)
    // are inferred accurately without needing a separate type checker pass!
    let fn_sigs = collect_file_fn_signatures(&root);

    let mut out = Vec::new();
    for node in root.syntax().descendants() {
        if let Some(func) = ast::Fn::cast(node) {
            let Some(name) = func.name().map(|n| n.text().to_string()) else {
                continue;
            };
            if let Some(target) = only_fn {
                if name != target {
                    continue;
                }
            }
            if let Some(compiled) = compile_fn_with_context(source, &func, &fn_sigs, name_prefix, alloc_mode) {
                out.push(compiled);
            }
        }
    }
    out
}

/// Compile a single [`ast::Fn`] into an explicit sans-IO state machine if it performs any effects.
pub fn compile_fn(source: &str, func: &ast::Fn) -> Option<CompiledStateMachine> {
    let parse = SourceFile::parse(source, Edition::CURRENT);
    let fn_sigs = collect_file_fn_signatures(&parse.tree());
    compile_fn_with_context(source, func, &fn_sigs, None, SmAllocMode::PlainAlloc)
}

fn compile_fn_with_context(
    source: &str,
    func: &ast::Fn,
    fn_sigs: &BTreeMap<String, String>,
    name_prefix: Option<&str>,
    alloc_mode: SmAllocMode,
) -> Option<CompiledStateMachine> {
    let body = func.body()?;
    if !contains_effect(body.syntax()) {
        return None;
    }

    let cfg = lower_fn_to_cfg(source, func, &body, fn_sigs, name_prefix)?;
    if cfg.suspend_count == 0 {
        return None;
    }

    let liveness = compute_liveness(&cfg);
    let mut effect_kinds = BTreeSet::new();
    for bb in &cfg.blocks {
        if let Terminator::Suspend { effect, .. } = &bb.term {
            effect_kinds.insert(effect.kind);
        }
    }

    let sans_io_code = emit_sans_io_machine(&cfg, &liveness, &effect_kinds, alloc_mode);
    let bstd_driver_code = emit_bstd_driver(&cfg, &effect_kinds, alloc_mode);
    let sim_test_code = emit_sim_test(&cfg, &effect_kinds, alloc_mode);

    Some(CompiledStateMachine {
        fn_name: cfg.fn_name.clone(),
        machine_name: format!("{}Machine", cfg.pascal_name),
        state_count: cfg.suspend_count + 2,
        effect_kinds,
        liveness,
        sans_io_code,
        bstd_driver_code,
        sim_test_code,
    })
}

fn collect_file_fn_signatures(root: &SourceFile) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for node in root.syntax().descendants() {
        if let Some(f) = ast::Fn::cast(node) {
            if let Some(name) = f.name().map(|n| n.text().to_string()) {
                if let Some(ret) = f.ret_type().and_then(|r| r.ty()) {
                    let ty = ret.syntax().text().to_string().trim().to_owned();
                    map.insert(name, ty);
                }
            }
        }
    }
    map
}

// -----------------------------------------------------------------------------
// 1. AST -> Effect CFG lowering
// -----------------------------------------------------------------------------

struct CfgBuilder<'a> {
    source: &'a str,
    fn_sigs: &'a BTreeMap<String, String>,
    blocks: Vec<BasicBlock>,
    cur: BlockId,
    local_types: BTreeMap<String, String>,
    mut_vars: BTreeSet<String>,
    suspend_count: usize,
    iter_counter: usize,
    loop_stack: Vec<(BlockId, BlockId)>,
}

impl<'a> CfgBuilder<'a> {
    fn new(source: &'a str, fn_sigs: &'a BTreeMap<String, String>) -> Self {
        let entry = BasicBlock { id: BlockId(0), stmts: Vec::new(), term: Terminator::Open };
        Self {
            source,
            fn_sigs,
            blocks: vec![entry],
            cur: BlockId(0),
            local_types: BTreeMap::new(),
            mut_vars: BTreeSet::new(),
            suspend_count: 0,
            iter_counter: 0,
            loop_stack: Vec::new(),
        }
    }

    fn new_block(&mut self) -> BlockId {
        let id = BlockId(self.blocks.len());
        self.blocks.push(BasicBlock { id, stmts: Vec::new(), term: Terminator::Open });
        id
    }

    fn push_stmt(&mut self, stmt: PureStmt) {
        if let Some(v) = &stmt.defined_var {
            if stmt.is_mut {
                self.mut_vars.insert(v.clone());
            }
            if let Some(ty) = &stmt.var_type {
                self.local_types.entry(v.clone()).or_insert_with(|| ty.clone());
            }
        }
        detect_mutations_in_code(&stmt.code, &mut self.mut_vars);
        self.blocks[self.cur.0].stmts.push(stmt);
    }

    fn finish_with(&mut self, term: Terminator) {
        if let Terminator::Suspend { bind: Some(b), bind_mut, bind_type, .. } = &term {
            if *bind_mut {
                self.mut_vars.insert(b.clone());
            }
            if let Some(ty) = bind_type {
                self.local_types.insert(b.clone(), ty.clone());
            }
        }
        self.blocks[self.cur.0].term = term;
    }

    fn lower_block(&mut self, block: &ast::BlockExpr) -> Option<String> {
        let stmt_list = block.stmt_list()?;
        for stmt in stmt_list.statements() {
            self.lower_stmt(&stmt);
        }
        if let Some(tail) = stmt_list.tail_expr() {
            self.lower_tail_expr(&tail)
        } else {
            None
        }
    }

    /// Lower a block whose result is discarded (`if`/`else` stmt branch, `while`/`loop`/`for` body):
    /// any pure tail expression (such as `if cond { total += 1; }` without a trailing semicolon)
    /// MUST be emitted as a statement rather than dropped!
    fn lower_unit_block(&mut self, block: &ast::BlockExpr) {
        if let Some(tail) = self.lower_block(block) {
            if !tail.is_empty() && tail != "()" {
                let code = if tail.ends_with('}') || tail.ends_with(';') {
                    tail
                } else {
                    format!("{tail};")
                };
                self.push_stmt(PureStmt { code, defined_var: None, is_mut: false, var_type: None });
            }
        }
    }

    fn lower_stmt(&mut self, stmt: &ast::Stmt) {
        match stmt {
            ast::Stmt::LetStmt(let_stmt) => {
                let pat_txt = let_stmt
                    .pat()
                    .map(|p| p.syntax().text().to_string())
                    .unwrap_or_else(|| "_".to_owned());
                let is_mut = pat_txt.trim_start().starts_with("mut ");
                let simple_var = extract_simple_ident(&pat_txt);
                let explicit_ty = let_stmt.ty().map(|t| t.syntax().text().to_string().trim().to_owned());

                if let Some(init) = let_stmt.initializer() {
                    // Handle `let pat = <effect_expr> else { diverge };` cleanly!
                    if let Some(let_else) = let_stmt.let_else() {
                        if let Some((effect, unwrap_mode)) = extract_effect_expr(self.source, &init) {
                            let tmp = format!("__letelse_{}", self.suspend_count);
                            let tmp_ty = match unwrap_mode {
                                UnwrapMode::Raw => effect.kind.event_payload_type().to_owned(),
                                _ => effect.kind.ok_payload_type().to_owned(),
                            };
                            let else_blk = let_else
                                .block_expr()
                                .map(|b| b.syntax().text().to_string())
                                .unwrap_or_else(|| "{ return; }".to_owned());
                            let code = format!("let {pat_txt} = {tmp} else {else_blk};");
                            let resume = self.new_block();
                            let sidx = self.suspend_count;
                            self.suspend_count += 1;
                            self.finish_with(Terminator::Suspend {
                                suspend_index: sidx,
                                effect,
                                bind: Some(tmp),
                                bind_mut: false,
                                bind_type: Some(tmp_ty),
                                unwrap_mode,
                                resume,
                            });
                            self.cur = resume;
                            let ty = explicit_ty.or_else(|| Some(effect_ok_inner_type(&pat_txt)));
                            self.push_stmt(PureStmt { code, defined_var: simple_var, is_mut, var_type: ty });
                            return;
                        }
                    }

                    if let Some((effect, unwrap_mode)) = extract_effect_expr(self.source, &init) {
                        let inferred_ty = explicit_ty.unwrap_or_else(|| match unwrap_mode {
                            UnwrapMode::Raw => effect.kind.event_payload_type().to_owned(),
                            UnwrapMode::TryPropagate | UnwrapMode::UnwrapOrPanic | UnwrapMode::UnwrapOrDefault => {
                                effect.kind.ok_payload_type().to_owned()
                            }
                        });
                        let resume = self.new_block();
                        let sidx = self.suspend_count;
                        self.suspend_count += 1;
                        self.finish_with(Terminator::Suspend {
                            suspend_index: sidx,
                            effect,
                            bind: simple_var,
                            bind_mut: is_mut,
                            bind_type: Some(inferred_ty),
                            unwrap_mode,
                            resume,
                        });
                        self.cur = resume;
                        return;
                    }

                    // Nested effect sub-expression hoisting (`PathBuf::from(env::var(...)?)`, etc.)!
                    if let Some((inner_range, effect, unwrap_mode)) = find_nested_effect(self.source, &init) {
                        let tmp = format!("__eff_{}", self.suspend_count);
                        let tmp_ty = match unwrap_mode {
                            UnwrapMode::Raw => effect.kind.event_payload_type().to_owned(),
                            UnwrapMode::TryPropagate | UnwrapMode::UnwrapOrPanic | UnwrapMode::UnwrapOrDefault => {
                                effect.kind.ok_payload_type().to_owned()
                            }
                        };
                        let stmt_range = stmt.syntax().text_range();
                        let stmt_start: usize = stmt_range.start().into();
                        let inner_start: usize = inner_range.start().into();
                        let inner_end: usize = inner_range.end().into();
                        let stmt_txt = stmt.syntax().text().to_string();
                        let rel_s = inner_start.saturating_sub(stmt_start);
                        let rel_e = inner_end.saturating_sub(stmt_start).min(stmt_txt.len());
                        let mut spliced = String::with_capacity(stmt_txt.len());
                        spliced.push_str(&stmt_txt[..rel_s]);
                        spliced.push_str(&tmp);
                        spliced.push_str(&stmt_txt[rel_e..]);

                        let resume = self.new_block();
                        let sidx = self.suspend_count;
                        self.suspend_count += 1;
                        self.finish_with(Terminator::Suspend {
                            suspend_index: sidx,
                            effect,
                            bind: Some(tmp),
                            bind_mut: false,
                            bind_type: Some(tmp_ty),
                            unwrap_mode,
                            resume,
                        });
                        self.cur = resume;
                        let ty = explicit_ty.or_else(|| infer_expr_type(&init, &self.local_types, self.fn_sigs));
                        self.push_stmt(PureStmt {
                            code: spliced.trim().to_owned(),
                            defined_var: simple_var,
                            is_mut,
                            var_type: ty,
                        });
                        return;
                    }

                    let ty = explicit_ty.or_else(|| infer_expr_type(&init, &self.local_types, self.fn_sigs));
                    let code = stmt.syntax().text().to_string().trim().to_owned();
                    self.push_stmt(PureStmt { code, defined_var: simple_var, is_mut, var_type: ty });
                } else {
                    let code = stmt.syntax().text().to_string().trim().to_owned();
                    self.push_stmt(PureStmt { code, defined_var: simple_var, is_mut, var_type: explicit_ty });
                }
            }
            ast::Stmt::ExprStmt(expr_stmt) => {
                let Some(expr) = expr_stmt.expr() else {
                    return;
                };
                if !contains_effect(expr.syntax()) {
                    if let ast::Expr::ReturnExpr(ret) = &expr {
                        let val = ret
                            .expr()
                            .map(|e| e.syntax().text().to_string().trim().to_owned())
                            .unwrap_or_else(|| "()".to_owned());
                        self.finish_with(Terminator::Return(val));
                        self.cur = self.new_block();
                        return;
                    }
                    if matches!(expr, ast::Expr::BreakExpr(_)) {
                        if let Some(&(_, break_bb)) = self.loop_stack.last() {
                            self.finish_with(Terminator::Goto(break_bb));
                            self.cur = self.new_block();
                            return;
                        }
                    }
                    if matches!(expr, ast::Expr::ContinueExpr(_)) {
                        if let Some(&(cont_bb, _)) = self.loop_stack.last() {
                            self.finish_with(Terminator::Goto(cont_bb));
                            self.cur = self.new_block();
                            return;
                        }
                    }
                    let code = stmt.syntax().text().to_string().trim().to_owned();
                    self.push_stmt(PureStmt { code, defined_var: None, is_mut: false, var_type: None });
                    return;
                }
                self.lower_effectful_expr_stmt(&expr);
            }
            ast::Stmt::Item(_) => {
                let code = stmt.syntax().text().to_string().trim().to_owned();
                self.push_stmt(PureStmt { code, defined_var: None, is_mut: false, var_type: None });
            }
        }
    }

    fn lower_effectful_expr_stmt(&mut self, expr: &ast::Expr) {
        if let Some((effect, unwrap_mode)) = extract_effect_expr(self.source, expr) {
            let resume = self.new_block();
            let sidx = self.suspend_count;
            self.suspend_count += 1;
            self.finish_with(Terminator::Suspend {
                suspend_index: sidx,
                effect,
                bind: None,
                bind_mut: false,
                bind_type: None,
                unwrap_mode,
                resume,
            });
            self.cur = resume;
            return;
        }

        match expr {
            ast::Expr::IfExpr(if_expr) => {
                // Support `if let Ok(x) = fs::read_to_string(p) { ... }` by hoisting the scrutinee!
                let cond = if let Some(cond_expr) = if_expr.condition() {
                    if let ast::Expr::LetExpr(let_expr) = &cond_expr {
                        if let Some(scrut) = let_expr.expr() {
                            if let Some((effect, unwrap_mode)) = extract_effect_expr(self.source, &scrut) {
                                let tmp = format!("__iflet_{}", self.suspend_count);
                                let tmp_ty = match unwrap_mode {
                                    UnwrapMode::Raw => effect.kind.event_payload_type().to_owned(),
                                    _ => effect.kind.ok_payload_type().to_owned(),
                                };
                                let pat = let_expr
                                    .pat()
                                    .map(|p| p.syntax().text().to_string().trim().to_owned())
                                    .unwrap_or_else(|| "_".to_owned());
                                let cond_txt = format!("let {pat} = {tmp}");
                                let resume = self.new_block();
                                let sidx = self.suspend_count;
                                self.suspend_count += 1;
                                self.finish_with(Terminator::Suspend {
                                    suspend_index: sidx,
                                    effect,
                                    bind: Some(tmp),
                                    bind_mut: false,
                                    bind_type: Some(tmp_ty),
                                    unwrap_mode,
                                    resume,
                                });
                                self.cur = resume;
                                cond_txt
                            } else {
                                cond_expr.syntax().text().to_string().trim().to_owned()
                            }
                        } else {
                            cond_expr.syntax().text().to_string().trim().to_owned()
                        }
                    } else {
                        cond_expr.syntax().text().to_string().trim().to_owned()
                    }
                } else {
                    "true".to_owned()
                };

                let then_bb = self.new_block();
                let else_bb = self.new_block();
                let join_bb = self.new_block();

                self.finish_with(Terminator::Branch { cond, then_bb, else_bb });

                self.cur = then_bb;
                if let Some(then_blk) = if_expr.then_branch() {
                    self.lower_unit_block(&then_blk);
                }
                if !block_has_explicit_exit(&self.blocks[self.cur.0]) {
                    self.finish_with(Terminator::Goto(join_bb));
                }

                self.cur = else_bb;
                if let Some(else_br) = if_expr.else_branch() {
                    match else_br {
                        ast::ElseBranch::Block(b) => {
                            self.lower_unit_block(&b);
                        }
                        ast::ElseBranch::IfExpr(nested) => {
                            self.lower_effectful_expr_stmt(&ast::Expr::IfExpr(nested));
                        }
                    }
                }
                if !block_has_explicit_exit(&self.blocks[self.cur.0]) {
                    self.finish_with(Terminator::Goto(join_bb));
                }

                self.cur = join_bb;
            }
            ast::Expr::MatchExpr(match_expr) => {
                // Support `match TcpStream::connect(addr) { Ok(s) => ..., Err(e) => ... }` AND
                // `match x { A => { io(); }, B => { io2(); } }`!
                let scrutinee = if let Some(scrut) = match_expr.expr() {
                    if let Some((effect, unwrap_mode)) = extract_effect_expr(self.source, &scrut) {
                        let tmp = format!("__match_{}", self.suspend_count);
                        let tmp_ty = match unwrap_mode {
                            UnwrapMode::Raw => effect.kind.event_payload_type().to_owned(),
                            _ => effect.kind.ok_payload_type().to_owned(),
                        };
                        let resume = self.new_block();
                        let sidx = self.suspend_count;
                        self.suspend_count += 1;
                        self.finish_with(Terminator::Suspend {
                            suspend_index: sidx,
                            effect,
                            bind: Some(tmp.clone()),
                            bind_mut: false,
                            bind_type: Some(tmp_ty),
                            unwrap_mode,
                            resume,
                        });
                        self.cur = resume;
                        tmp
                    } else {
                        scrut.syntax().text().to_string().trim().to_owned()
                    }
                } else {
                    "()".to_owned()
                };

                let match_bb = self.cur;
                let join_bb = self.new_block();
                let mut arms = Vec::new();
                if let Some(arm_list) = match_expr.match_arm_list() {
                    for arm in arm_list.arms() {
                        let pat = arm
                            .pat()
                            .map(|p| p.syntax().text().to_string().trim().to_owned())
                            .unwrap_or_else(|| "_".to_owned());
                        let full_pat = if let Some(guard) = arm.guard() {
                            format!("{pat} {}", guard.syntax().text().to_string().trim())
                        } else {
                            pat
                        };
                        let arm_bb = self.new_block();
                        self.cur = arm_bb;
                        if let Some(body) = arm.expr() {
                            if contains_effect(body.syntax()) {
                                self.lower_effectful_expr_stmt(&body);
                            } else if let ast::Expr::ReturnExpr(ret) = &body {
                                let val = ret
                                    .expr()
                                    .map(|e| e.syntax().text().to_string().trim().to_owned())
                                    .unwrap_or_else(|| "()".to_owned());
                                self.finish_with(Terminator::Return(val));
                            } else if matches!(body, ast::Expr::BreakExpr(_)) {
                                if let Some(&(_, break_bb)) = self.loop_stack.last() {
                                    self.finish_with(Terminator::Goto(break_bb));
                                }
                            } else if matches!(body, ast::Expr::ContinueExpr(_)) {
                                if let Some(&(cont_bb, _)) = self.loop_stack.last() {
                                    self.finish_with(Terminator::Goto(cont_bb));
                                }
                            } else {
                                let code = format!("{};", body.syntax().text().to_string().trim());
                                self.push_stmt(PureStmt { code, defined_var: None, is_mut: false, var_type: None });
                            }
                        }
                        if !block_has_explicit_exit(&self.blocks[self.cur.0]) {
                            self.finish_with(Terminator::Goto(join_bb));
                        }
                        arms.push((full_pat, arm_bb));
                    }
                }
                self.blocks[match_bb.0].term = Terminator::Match { scrutinee, arms };
                self.cur = join_bb;
            }
            ast::Expr::WhileExpr(while_expr) => {
                let cond = while_expr
                    .condition()
                    .map(|c| c.syntax().text().to_string().trim().to_owned())
                    .unwrap_or_else(|| "true".to_owned());
                let header_bb = self.new_block();
                let body_bb = self.new_block();
                let exit_bb = self.new_block();

                self.finish_with(Terminator::Goto(header_bb));
                self.cur = header_bb;
                self.finish_with(Terminator::Branch { cond, then_bb: body_bb, else_bb: exit_bb });

                self.loop_stack.push((header_bb, exit_bb));
                self.cur = body_bb;
                if let Some(body) = while_expr.loop_body() {
                    self.lower_unit_block(&body);
                }
                if !block_has_explicit_exit(&self.blocks[self.cur.0]) {
                    self.finish_with(Terminator::Goto(header_bb));
                }
                self.loop_stack.pop();

                self.cur = exit_bb;
            }
            ast::Expr::LoopExpr(loop_expr) => {
                let header_bb = self.new_block();
                let exit_bb = self.new_block();

                self.finish_with(Terminator::Goto(header_bb));
                self.loop_stack.push((header_bb, exit_bb));
                self.cur = header_bb;
                if let Some(body) = loop_expr.loop_body() {
                    self.lower_unit_block(&body);
                }
                if !block_has_explicit_exit(&self.blocks[self.cur.0]) {
                    self.finish_with(Terminator::Goto(header_bb));
                }
                self.loop_stack.pop();

                self.cur = exit_bb;
            }
            ast::Expr::ForExpr(for_expr) => {
                let pat = for_expr
                    .pat()
                    .map(|p| p.syntax().text().to_string().trim().to_owned())
                    .unwrap_or_else(|| "_item".to_owned());
                let iter_expr = for_expr
                    .iterable()
                    .map(|e| e.syntax().text().to_string().trim().to_owned())
                    .unwrap_or_else(|| "Vec::new()".to_owned());
                let idx = self.iter_counter;
                self.iter_counter += 1;
                let items_var = format!("__for_items_{idx}");
                let pos_var = format!("__for_idx_{idx}");
                let items_init = format!("let {items_var} = ({iter_expr}).into_iter().collect::<Vec<_>>();");
                let pos_init = format!("let mut {pos_var}: usize = 0;");
                let loop_cond = format!("{pos_var} < {items_var}.len()");
                let elem_bind = format!("let {pat} = {items_var}[{pos_var}].clone();");
                let pos_inc = format!("{pos_var} += 1;");

                self.push_stmt(PureStmt {
                    code: items_init,
                    defined_var: Some(items_var),
                    is_mut: false,
                    var_type: Some("Vec<String>".to_owned()),
                });
                self.push_stmt(PureStmt {
                    code: pos_init,
                    defined_var: Some(pos_var),
                    is_mut: true,
                    var_type: Some("usize".to_owned()),
                });

                let header_bb = self.new_block();
                let body_bb = self.new_block();
                let exit_bb = self.new_block();

                self.finish_with(Terminator::Goto(header_bb));
                self.cur = header_bb;
                self.finish_with(Terminator::Branch {
                    cond: loop_cond,
                    then_bb: body_bb,
                    else_bb: exit_bb,
                });

                self.loop_stack.push((header_bb, exit_bb));
                self.cur = body_bb;
                let simple_pat = extract_simple_ident(&pat);
                self.push_stmt(PureStmt {
                    code: elem_bind,
                    defined_var: simple_pat,
                    is_mut: false,
                    var_type: Some("String".to_owned()),
                });
                self.push_stmt(PureStmt {
                    code: pos_inc,
                    defined_var: None,
                    is_mut: false,
                    var_type: None,
                });
                if let Some(body) = for_expr.loop_body() {
                    self.lower_unit_block(&body);
                }
                if !block_has_explicit_exit(&self.blocks[self.cur.0]) {
                    self.finish_with(Terminator::Goto(header_bb));
                }
                self.loop_stack.pop();

                self.cur = exit_bb;
            }
            ast::Expr::BlockExpr(blk) => {
                self.lower_unit_block(blk);
            }
            _ => {
                // Sub-expression hoist any nested effect inside an expression statement (e.g. `foo(fs::read_to_string(p)?)`).
                if let Some((inner_range, effect, unwrap_mode)) = find_nested_effect(self.source, expr) {
                    let tmp = format!("__eff_{}", self.suspend_count);
                    let tmp_ty = match unwrap_mode {
                        UnwrapMode::Raw => effect.kind.event_payload_type().to_owned(),
                        _ => effect.kind.ok_payload_type().to_owned(),
                    };
                    let erange = expr.syntax().text_range();
                    let es: usize = erange.start().into();
                    let is: usize = inner_range.start().into();
                    let ie: usize = inner_range.end().into();
                    let txt = expr.syntax().text().to_string();
                    let rs = is.saturating_sub(es);
                    let re = ie.saturating_sub(es).min(txt.len());
                    let mut spliced = String::with_capacity(txt.len() + 2);
                    spliced.push_str(&txt[..rs]);
                    spliced.push_str(&tmp);
                    spliced.push_str(&txt[re..]);
                    spliced.push(';');

                    let resume = self.new_block();
                    let sidx = self.suspend_count;
                    self.suspend_count += 1;
                    self.finish_with(Terminator::Suspend {
                        suspend_index: sidx,
                        effect,
                        bind: Some(tmp),
                        bind_mut: false,
                        bind_type: Some(tmp_ty),
                        unwrap_mode,
                        resume,
                    });
                    self.cur = resume;
                    self.push_stmt(PureStmt { code: spliced, defined_var: None, is_mut: false, var_type: None });
                } else {
                    let code = format!("{};", expr.syntax().text().to_string().trim());
                    self.push_stmt(PureStmt { code, defined_var: None, is_mut: false, var_type: None });
                }
            }
        }
    }

    fn lower_tail_expr(&mut self, tail: &ast::Expr) -> Option<String> {
        if let Some((effect, unwrap_mode)) = extract_effect_expr(self.source, tail) {
            let tmp = format!("__tail_{}", self.suspend_count);
            let ty = match unwrap_mode {
                UnwrapMode::Raw => effect.kind.event_payload_type().to_owned(),
                _ => effect.kind.ok_payload_type().to_owned(),
            };
            let resume = self.new_block();
            let sidx = self.suspend_count;
            self.suspend_count += 1;
            self.finish_with(Terminator::Suspend {
                suspend_index: sidx,
                effect,
                bind: Some(tmp.clone()),
                bind_mut: false,
                bind_type: Some(ty),
                unwrap_mode,
                resume,
            });
            self.cur = resume;
            return Some(tmp);
        }
        if matches!(
            tail,
            ast::Expr::IfExpr(_)
                | ast::Expr::WhileExpr(_)
                | ast::Expr::LoopExpr(_)
                | ast::Expr::ForExpr(_)
                | ast::Expr::MatchExpr(_)
        ) && contains_effect(tail.syntax())
        {
            self.lower_effectful_expr_stmt(tail);
            return None;
        }
        Some(tail.syntax().text().to_string().trim().to_owned())
    }
}

fn block_has_explicit_exit(bb: &BasicBlock) -> bool {
    !matches!(bb.term, Terminator::Open)
}

fn effect_ok_inner_type(pat: &str) -> String {
    if pat.contains("Some(") || pat.contains("Ok(") {
        "String".to_owned()
    } else {
        "String".to_owned()
    }
}

fn detect_mutations_in_code(code: &str, mut_vars: &mut BTreeSet<String>) {
    let bytes = code.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &code[start..i];
            let rest = code[i..].trim_start();
            // Check for `ident = `, `ident +=`, `ident -=`, `ident.push(`, `ident.insert(`, `ident.clear(`, `&mut ident`.
            let is_assign = (rest.starts_with('=') && !rest.starts_with("=="))
                || rest.starts_with("+=")
                || rest.starts_with("-=")
                || rest.starts_with("*=")
                || rest.starts_with("/=")
                || rest.starts_with("|=")
                || rest.starts_with("&=")
                || rest.starts_with(".push(")
                || rest.starts_with(".push_str(")
                || rest.starts_with(".insert(")
                || rest.starts_with(".remove(")
                || rest.starts_with(".clear(")
                || rest.starts_with(".extend(")
                || rest.starts_with(".sort")
                || rest.starts_with(".get_mut(")
                || rest.starts_with(".poll(")
                || rest.starts_with(".next(")
                || rest.starts_with(".gen");
            let before = code[..start].trim_end();
            let is_mut_ref = before.ends_with("&mut") || before.ends_with("let mut");
            if (is_assign || is_mut_ref) && !before.ends_with("let") && !before.ends_with('.') {
                mut_vars.insert(ident.to_owned());
            }
        } else {
            i += 1;
        }
    }
}

fn lower_fn_to_cfg(
    source: &str,
    func: &ast::Fn,
    body: &ast::BlockExpr,
    fn_sigs: &BTreeMap<String, String>,
    name_prefix: Option<&str>,
) -> Option<SmCfg> {
    let fn_name = func.name()?.text().to_string();
    let pascal_name = match name_prefix {
        Some(pfx) if !pfx.is_empty() => format!("{}{}", to_pascal_case(pfx), to_pascal_case(&fn_name)),
        _ => to_pascal_case(&fn_name),
    };
    let is_pub = func.visibility().is_some();
    let is_async = func.async_token().is_some();

    let mut params = Vec::new();
    let mut builder = CfgBuilder::new(source, fn_sigs);
    if let Some(plist) = func.param_list() {
        for param in plist.params() {
            let pname = param
                .pat()
                .map(|p| p.syntax().text().to_string().trim().to_owned())
                .unwrap_or_else(|| "arg".to_owned());
            let is_mut = pname.starts_with("mut ");
            let pty_raw = param
                .ty()
                .map(|t| t.syntax().text().to_string().trim().to_owned())
                .unwrap_or_else(|| "String".to_owned());
            let owned_ty = own_param_type(&pty_raw);
            if let Some(ident) = extract_simple_ident(&pname) {
                if is_mut {
                    builder.mut_vars.insert(ident.clone());
                }
                builder.local_types.insert(ident.clone(), owned_ty.clone());
                params.push((ident, owned_ty));
            }
        }
    }

    let return_type = func
        .ret_type()
        .and_then(|r| r.ty())
        .map(|t| t.syntax().text().to_string().trim().to_owned())
        .unwrap_or_else(|| "()".to_owned());
    let returns_result = return_type.starts_with("Result<") || return_type.starts_with("core::result::Result<");

    let tail_val = builder.lower_block(body);
    let final_ret = tail_val.unwrap_or_else(|| {
        if returns_result {
            "Ok(())".to_owned()
        } else {
            "()".to_owned()
        }
    });
    builder.finish_with(Terminator::Return(final_ret));

    // Also scan terminator expressions for mutations (e.g. `poll.poll(&mut events, ...)`).
    let mut extra_muts = BTreeSet::new();
    for bb in &builder.blocks {
        match &bb.term {
            Terminator::Branch { cond, .. } => detect_mutations_in_code(cond, &mut extra_muts),
            Terminator::Match { scrutinee, .. } => detect_mutations_in_code(scrutinee, &mut extra_muts),
            Terminator::Suspend { effect, .. } => {
                for a in &effect.args {
                    detect_mutations_in_code(a, &mut extra_muts);
                }
            }
            Terminator::Return(r) => detect_mutations_in_code(r, &mut extra_muts),
            Terminator::Goto(_) | Terminator::Open => {}
        }
    }
    builder.mut_vars.extend(extra_muts);

    Some(SmCfg {
        fn_name,
        pascal_name,
        is_pub,
        is_async,
        params,
        return_type,
        returns_result,
        entry: BlockId(0),
        blocks: builder.blocks,
        local_types: builder.local_types,
        mut_vars: builder.mut_vars,
        suspend_count: builder.suspend_count,
    })
}

fn own_param_type(ty: &str) -> String {
    let t = ty.trim();
    if t == "&str" || t == "&Path" || t == "&std::path::Path" || t == "&PathBuf" {
        return "String".to_owned();
    }
    if t == "&[u8]" {
        return "Vec<u8>".to_owned();
    }
    // Strip optional leading `&`, lifetime `'a`, and `mut `.
    if let Some(after_amp) = t.strip_prefix('&') {
        let mut inner = after_amp.trim_start();
        if let Some(after_tick) = inner.strip_prefix('\'') {
            let lt_len = after_tick
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(after_tick.len());
            inner = after_tick[lt_len..].trim_start();
        }
        if let Some(after_mut) = inner.strip_prefix("mut ") {
            inner = after_mut.trim_start();
        }
        if inner == "str" || inner == "Path" || inner == "std::path::Path" {
            return "String".to_owned();
        }
        if let Some(arr_or_slice) = inner.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            if arr_or_slice.contains(';') {
                // Fixed-size array `&[u8; 16]` -> owned `[u8; 16]`.
                return format!("[{arr_or_slice}]");
            }
            return format!("Vec<{arr_or_slice}>");
        }
        if inner.starts_with("dyn ") {
            // Keep trait-object references as `'static` (or original) rather than emitting invalid bare `dyn T`.
            return t.to_owned();
        }
        return inner.to_owned();
    }
    t.to_owned()
}

fn contains_effect(node: &syntax::SyntaxNode) -> bool {
    for d in node.descendants() {
        // Ignore effects inside nested closures (executed lazily when invoked, not inline).
        if d.ancestors().any(|a| a.kind() == SyntaxKind::CLOSURE_EXPR && a != *node) {
            continue;
        }
        match d.kind() {
            SyntaxKind::AWAIT_EXPR => return true,
            SyntaxKind::CALL_EXPR => {
                if let Some(call) = ast::CallExpr::cast(d) {
                    if let Some(callee) = call.expr() {
                        let txt: String = callee
                            .syntax()
                            .text()
                            .to_string()
                            .chars()
                            .filter(|c| !c.is_whitespace())
                            .collect();
                        if classify_call_effect(&txt).is_some() {
                            return true;
                        }
                    }
                }
            }
            SyntaxKind::MACRO_CALL => {
                if let Some(mc) = ast::MacroCall::cast(d) {
                    let mname = mc.path().map(|p| p.syntax().text().to_string()).unwrap_or_default();
                    let leaf = mname.rsplit("::").next().unwrap_or(&mname).trim();
                    if classify_macro_effect(leaf).is_some() {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

fn extract_effect_expr(source: &str, expr: &ast::Expr) -> Option<(EffectCall, UnwrapMode)> {
    let mut mode = UnwrapMode::Raw;
    let mut owned: Option<ast::Expr> = None;

    for _ in 0..8 {
        let cur = owned.as_ref().unwrap_or(expr);
        match cur {
            ast::Expr::TryExpr(t) => {
                mode = UnwrapMode::TryPropagate;
                owned = Some(t.expr()?);
            }
            ast::Expr::AwaitExpr(a) => {
                let inner_txt = a
                    .expr()
                    .map(|e| e.syntax().text().to_string().trim().to_owned())
                    .unwrap_or_default();
                let off: usize = a.syntax().text_range().start().into();
                return Some((
                    EffectCall {
                        kind: EffectKind::AwaitFuture,
                        args: vec![inner_txt],
                        raw_expr: expr.syntax().text().to_string().trim().to_owned(),
                        line: line_of_offset(source, off),
                    },
                    mode,
                ));
            }
            ast::Expr::MethodCallExpr(mc) => {
                let mname = mc.name_ref().map(|n| n.text().to_string()).unwrap_or_default();
                match mname.as_str() {
                    "unwrap" | "expect" | "unwrap_or_else" => {
                        if mode == UnwrapMode::Raw {
                            mode = UnwrapMode::UnwrapOrPanic;
                        }
                        owned = Some(mc.receiver()?);
                    }
                    "unwrap_or_default" => {
                        if mode == UnwrapMode::Raw {
                            mode = UnwrapMode::UnwrapOrDefault;
                        }
                        owned = Some(mc.receiver()?);
                    }
                    "map_err" | "context" | "with_context" | "ok" | "send_json" | "call" | "output" => {
                        owned = Some(mc.receiver()?);
                    }
                    _ => break,
                }
            }
            ast::Expr::CallExpr(call) => {
                if let Some(callee) = call.expr() {
                    let callee_txt: String = callee
                        .syntax()
                        .text()
                        .to_string()
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .collect();
                    if let Some(kind) = classify_call_effect(&callee_txt) {
                        let args: Vec<String> = call
                            .arg_list()
                            .map(|al| {
                                al.args()
                                    .map(|a| a.syntax().text().to_string().trim().to_owned())
                                    .collect()
                            })
                            .unwrap_or_default();
                        let off: usize = call.syntax().text_range().start().into();
                        return Some((
                            EffectCall {
                                kind,
                                args,
                                raw_expr: expr.syntax().text().to_string().trim().to_owned(),
                                line: line_of_offset(source, off),
                            },
                            mode,
                        ));
                    }
                }
                break;
            }
            ast::Expr::MacroExpr(me) => {
                if let Some(mc) = me.macro_call() {
                    let mname = mc.path().map(|p| p.syntax().text().to_string()).unwrap_or_default();
                    let leaf = mname.rsplit("::").next().unwrap_or(&mname).trim();
                    if let Some(kind) = classify_macro_effect(leaf) {
                        let args_txt = mc
                            .token_tree()
                            .map(|tt| {
                                let t = tt.syntax().text().to_string();
                                let inner = if t.len() >= 2
                                    && matches!(
                                        (t.as_bytes()[0], t.as_bytes()[t.len() - 1]),
                                        (b'(', b')') | (b'[', b']') | (b'{', b'}')
                                    )
                                {
                                    &t[1..t.len() - 1]
                                } else {
                                    t.as_str()
                                };
                                inner.trim().to_owned()
                            })
                            .unwrap_or_default();
                        let off: usize = mc.syntax().text_range().start().into();
                        return Some((
                            EffectCall {
                                kind,
                                args: vec![format!("format!({args_txt})")],
                                raw_expr: expr.syntax().text().to_string().trim().to_owned(),
                                line: line_of_offset(source, off),
                            },
                            mode,
                        ));
                    }
                }
                break;
            }
            ast::Expr::ParenExpr(p) => {
                owned = Some(p.expr()?);
            }
            _ => break,
        }
    }
    None
}

fn find_nested_effect(
    source: &str,
    root: &ast::Expr,
) -> Option<(syntax::TextRange, EffectCall, UnwrapMode)> {
    if root.syntax().kind() == SyntaxKind::CLOSURE_EXPR {
        return None;
    }
    for node in root.syntax().descendants().skip(1) {
        if node.ancestors().any(|a| a.kind() == SyntaxKind::CLOSURE_EXPR) {
            continue;
        }
        if let Some(sub) = ast::Expr::cast(node) {
            if let Some((eff, mode)) = extract_effect_expr(source, &sub) {
                return Some((sub.syntax().text_range(), eff, mode));
            }
        }
    }
    None
}

fn extract_simple_ident(pat: &str) -> Option<String> {
    let trimmed = pat.trim().strip_prefix("mut ").unwrap_or(pat.trim()).trim();
    if !trimmed.is_empty()
        && trimmed != "_"
        && trimmed.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && trimmed.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    {
        Some(trimmed.to_owned())
    } else {
        None
    }
}

/// Infer local variable type from RHS syntax, intra-file function signatures, constructors,
/// typed `vec![elem; len]`, and `Result`/`Option` unwraps!
fn infer_expr_type(
    expr: &ast::Expr,
    env: &BTreeMap<String, String>,
    fn_sigs: &BTreeMap<String, String>,
) -> Option<String> {
    let txt = expr.syntax().text().to_string();
    let t = txt.trim();

    if t == "true"
        || t == "false"
        || t.ends_with(".is_empty()")
        || t.ends_with(".is_ok()")
        || t.ends_with(".is_some()")
    {
        return Some("bool".to_owned());
    }
    if t.starts_with("PathBuf::") || (t.contains(".join(") && !t.contains("collect")) {
        return Some("PathBuf".to_owned());
    }
    if t.starts_with('"') || t.starts_with("String::") || t.starts_with("format!") || t.ends_with(".to_string()") {
        return Some("String".to_owned());
    }
    if t.starts_with("vec![") {
        if t.contains("0u8") || t.contains("u8") {
            return Some("Vec<u8>".to_owned());
        }
        if t.contains("0u32") || t.contains("u32") {
            return Some("Vec<u32>".to_owned());
        }
        if t.contains("0u64") || t.contains("u64") {
            return Some("Vec<u64>".to_owned());
        }
        if t.contains("0usize") || t.contains("usize") {
            return Some("Vec<usize>".to_owned());
        }
        if t.contains("0i32") || t.contains("i32") {
            return Some("Vec<i32>".to_owned());
        }
        if t.contains("false") || t.contains("true") {
            return Some("Vec<bool>".to_owned());
        }
        return Some("Vec<String>".to_owned());
    }
    if t.starts_with("Vec::") {
        return Some("Vec<u8>".to_owned());
    }
    if t.ends_with(".len()") || t.ends_with(" as usize") || t.ends_with("usize") {
        return Some("usize".to_owned());
    }
    if t.ends_with(" as u64") || t.ends_with("u64") {
        return Some("u64".to_owned());
    }
    if t.ends_with(" as u32") || t.ends_with("u32") {
        return Some("u32".to_owned());
    }
    if t.ends_with(" as i32") || t.ends_with("i32") {
        return Some("i32".to_owned());
    }
    if t.chars().all(|c| c.is_ascii_digit() || c == '_') && !t.is_empty() {
        return Some("usize".to_owned());
    }

    // Check intra-file function calls: `foo(...)`, `foo(...)?`, `foo(...).unwrap_or_else(...)`!
    let unwraps = t.ends_with('?')
        || t.contains(".unwrap()")
        || t.contains(".expect(")
        || t.contains(".unwrap_or_else(")
        || t.contains(".unwrap_or_default()");
    if let Some(callee) = t.split('(').next() {
        let callee = callee.trim();
        let is_clean_path = !callee.is_empty()
            && callee
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':');
        // Type constructor `Poll::new(...)`, `Events::with_capacity(...)`, `Duration::from_millis(...)`!
        if is_clean_path {
            if let Some((ty_name, ctor)) = callee.rsplit_once("::") {
                if matches!(
                    ctor,
                    "new" | "with_capacity" | "default" | "from" | "from_millis" | "from_secs" | "from_nanos"
                ) && ty_name
                    .rsplit("::")
                    .next()
                    .is_some_and(|s| s.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
                {
                    return Some(ty_name.to_owned());
                }
            }
            // Local function call return type from `fn_sigs`!
            let fn_leaf = callee.rsplit("::").next().unwrap_or(callee);
            if let Some(sig_ret) = fn_sigs.get(fn_leaf) {
                if unwraps {
                    if let Some(inner) = unwrap_result_or_option(sig_ret) {
                        return Some(inner);
                    }
                }
                return Some(sig_ret.clone());
            }
        }
    }

    if let Some(ty) = env.get(t) {
        return Some(ty.clone());
    }
    None
}

fn unwrap_result_or_option(ty: &str) -> Option<String> {
    let t = ty.trim();
    if let Some(rest) = t.strip_prefix("Option<").and_then(|s| s.strip_suffix('>')) {
        return Some(rest.trim().to_owned());
    }
    if let Some(rest) = t
        .strip_prefix("Result<")
        .or_else(|| t.strip_prefix("core::result::Result<"))
        .and_then(|s| s.strip_suffix('>'))
    {
        // Split at the top-level comma (depth 0 in `<...>`).
        let mut depth = 0usize;
        for (idx, ch) in rest.char_indices() {
            match ch {
                '<' | '(' | '[' => depth += 1,
                '>' | ')' | ']' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => return Some(rest[..idx].trim().to_owned()),
                _ => {}
            }
        }
    }
    None
}

// -----------------------------------------------------------------------------
// 2. Forward Definite-Availability + Backward Liveness Dataflow Analysis
// -----------------------------------------------------------------------------

/// Compute exact backward liveness (`LIVE_IN`/`LIVE_OUT`) AND forward definite availability
/// (`AVAIL_IN`/`AVAIL_OUT`), so `saved(S) = LIVE_IN(resume(S)) ∩ AVAIL_OUT(S)`:
/// - dead variables are pruned immediately, and
/// - variables declared only in later/disjoint blocks can NEVER leak into earlier states!
pub fn compute_liveness(cfg: &SmCfg) -> LivenessInfo {
    type NameSet<'a> = BTreeSet<&'a str>;
    let n = cfg.blocks.len();
    let mut all_locals: NameSet<'_> = cfg
        .local_types
        .keys()
        .map(String::as_str)
        .chain(cfg.params.iter().map(|(p, _)| p.as_str()))
        .collect();
    for bb in &cfg.blocks {
        for stmt in &bb.stmts {
            if let Some(v) = &stmt.defined_var {
                all_locals.insert(v.as_str());
            }
        }
        if let Terminator::Suspend { bind: Some(b), .. } = &bb.term {
            all_locals.insert(b.as_str());
        }
    }

    let mut block_uses = vec![NameSet::new(); n];
    let mut block_defs = vec![NameSet::new(); n];
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut resume_bound: BTreeMap<usize, &str> = BTreeMap::new();

    for (idx, bb) in cfg.blocks.iter().enumerate() {
        let mut defined_so_far = NameSet::new();
        for stmt in &bb.stmts {
            let rhs = match stmt.code.split_once('=') {
                Some((_, r)) if stmt.defined_var.is_some() => r,
                _ => stmt.code.as_str(),
            };
            for ident in scan_identifiers(rhs) {
                if let Some(&loc) = all_locals.get(ident.as_str()) {
                    if !defined_so_far.contains(loc) {
                        block_uses[idx].insert(loc);
                    }
                }
            }
            if let Some(v) = &stmt.defined_var {
                defined_so_far.insert(v.as_str());
                block_defs[idx].insert(v.as_str());
            }
        }

        match &bb.term {
            Terminator::Suspend { effect, bind, resume, .. } => {
                preds[resume.0].push(idx);
                if let Some(b) = bind {
                    resume_bound.insert(resume.0, b.as_str());
                }
                for arg in &effect.args {
                    for ident in scan_identifiers(arg) {
                        if let Some(&loc) = all_locals.get(ident.as_str()) {
                            if !defined_so_far.contains(loc) {
                                block_uses[idx].insert(loc);
                            }
                        }
                    }
                }
            }
            Terminator::Branch { cond, then_bb, else_bb } => {
                preds[then_bb.0].push(idx);
                preds[else_bb.0].push(idx);
                for ident in scan_identifiers(cond) {
                    if let Some(&loc) = all_locals.get(ident.as_str()) {
                        if !defined_so_far.contains(loc) {
                            block_uses[idx].insert(loc);
                        }
                    }
                }
            }
            Terminator::Match { scrutinee, arms } => {
                for (_, target) in arms {
                    preds[target.0].push(idx);
                }
                for ident in scan_identifiers(scrutinee) {
                    if let Some(&loc) = all_locals.get(ident.as_str()) {
                        if !defined_so_far.contains(loc) {
                            block_uses[idx].insert(loc);
                        }
                    }
                }
            }
            Terminator::Goto(target) => {
                preds[target.0].push(idx);
            }
            Terminator::Return(expr) => {
                for ident in scan_identifiers(expr) {
                    if let Some(&loc) = all_locals.get(ident.as_str()) {
                        if !defined_so_far.contains(loc) {
                            block_uses[idx].insert(loc);
                        }
                    }
                }
            }
            Terminator::Open => {}
        }
    }

    // 2a. Forward definite-availability fixpoint (`avail_in`, `avail_out`).
    let entry_params: NameSet<'_> = cfg.params.iter().map(|(p, _)| p.as_str()).collect();
    let mut avail_in: Vec<NameSet<'_>> = (0..n).map(|_| all_locals.iter().copied().collect()).collect();
    let mut avail_out: Vec<NameSet<'_>> = (0..n).map(|_| all_locals.iter().copied().collect()).collect();
    avail_in[cfg.entry.0] = entry_params;

    let mut iter = 0;
    while iter < MAX_DATAFLOW_ITERS {
        iter += 1;
        let mut changed = false;
        for idx in 0..n {
            if idx != cfg.entry.0 {
                let mut new_in = if preds[idx].is_empty() {
                    NameSet::new()
                } else {
                    let mut acc: NameSet<'_> = avail_out[preds[idx][0]].iter().copied().collect();
                    for &p in &preds[idx][1..] {
                        acc.retain(|v| avail_out[p].contains(v));
                    }
                    acc
                };
                if let Some(&b) = resume_bound.get(&idx) {
                    new_in.insert(b);
                }
                if new_in != avail_in[idx] {
                    avail_in[idx] = new_in;
                    changed = true;
                }
            }
            let mut new_out: NameSet<'_> = avail_in[idx].iter().copied().collect();
            new_out.extend(block_defs[idx].iter().copied());
            if new_out != avail_out[idx] {
                avail_out[idx] = new_out;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // 2b. Backward liveness fixpoint (`live_in`, `live_out`).
    let mut live_in = vec![NameSet::new(); n];
    let mut live_out = vec![NameSet::new(); n];

    iter = 0;
    while iter < MAX_DATAFLOW_ITERS {
        iter += 1;
        let mut changed = false;
        for idx in (0..n).rev() {
            let mut out = NameSet::new();
            match &cfg.blocks[idx].term {
                Terminator::Suspend { resume, bind, .. } => {
                    for &v in &live_in[resume.0] {
                        if bind.as_deref() != Some(v) {
                            out.insert(v);
                        }
                    }
                }
                Terminator::Branch { then_bb, else_bb, .. } => {
                    out.extend(live_in[then_bb.0].iter().copied());
                    out.extend(live_in[else_bb.0].iter().copied());
                }
                Terminator::Match { arms, .. } => {
                    for (_, target) in arms {
                        out.extend(live_in[target.0].iter().copied());
                    }
                }
                Terminator::Goto(target) => {
                    out.extend(live_in[target.0].iter().copied());
                }
                Terminator::Return(_) | Terminator::Open => {}
            }

            let mut inp: BTreeSet<&str> = block_uses[idx].iter().copied().collect();
            for &v in &out {
                if !block_defs[idx].contains(v) {
                    inp.insert(v);
                }
            }
            if let Some(&bound) = resume_bound.get(&idx) {
                inp.remove(bound);
            }

            if out != live_out[idx] || inp != live_in[idx] {
                live_out[idx] = out;
                live_in[idx] = inp;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // 2c. Exact saved set = backward-live AND forward-definitely-available!
    let mut saved_at_suspend = BTreeMap::new();
    let mut max_live = 0usize;
    for (idx, bb) in cfg.blocks.iter().enumerate() {
        if let Terminator::Suspend { suspend_index, bind, resume, .. } = &bb.term {
            let mut vars: Vec<String> = live_in[resume.0]
                .iter()
                .filter(|&&v| bind.as_deref() != Some(v) && avail_out[idx].contains(v))
                .map(|&v| v.to_owned())
                .collect();
            vars.sort();
            max_live = max_live.max(vars.len());
            saved_at_suspend.insert(*suspend_index, vars);
        }
    }

    let own_sets = |sets: Vec<BTreeSet<&str>>| -> Vec<BTreeSet<String>> {
        sets.into_iter().map(|s| s.into_iter().map(str::to_owned).collect()).collect()
    };

    LivenessInfo {
        live_in: own_sets(live_in),
        live_out: own_sets(live_out),
        avail_in: own_sets(avail_in),
        avail_out: own_sets(avail_out),
        saved_at_suspend,
        total_locals: all_locals.len(),
        max_live_at_suspend: max_live,
    }
}

fn scan_identifiers(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = code.as_bytes();
    let mut i = 0;
    let mut in_str = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' {
            in_str = !in_str;
            i += 1;
            continue;
        }
        if in_str {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            // Rust 2021+ format-string captures: `"{ident}"` or `"{ident:?}"`.
            if b == b'{' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                    i += 2;
                    continue;
                }
                let start = i + 1;
                if start < bytes.len() && (bytes[start].is_ascii_alphabetic() || bytes[start] == b'_') {
                    let mut end = start + 1;
                    while end < bytes.len()
                        && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                    {
                        end += 1;
                    }
                    if end < bytes.len() && (bytes[end] == b'}' || bytes[end] == b':') {
                        out.push(code[start..end].to_owned());
                        i = end;
                        continue;
                    }
                }
            }
            i += 1;
            continue;
        }
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let preceded_by_dot = start > 0 && bytes[start - 1] == b'.';
            if !preceded_by_dot {
                out.push(code[start..i].to_owned());
            }
        } else {
            i += 1;
        }
    }
    out
}

// -----------------------------------------------------------------------------
// 3. Code Emission: Sans-IO State Machine + Thin bstd Driver + Sim Test
// -----------------------------------------------------------------------------

/// Map a plain Rust type (`String`, `Vec<T>`, `Result<String, String>`) into its `bstd` explicit
/// allocator equivalent when `mode == SmAllocMode::BstdExplicitAlloc`.
fn alloc_type(ty: &str, mode: SmAllocMode) -> String {
    if mode == SmAllocMode::PlainAlloc {
        return ty.to_owned();
    }
    let t = ty.trim();
    match t {
        "String" => "bstd::mem::String<bstd::mem::Alloc<'a>>".to_owned(),
        "Vec<u8>" => "bstd::vec::Vec<u8, bstd::mem::Alloc<'a>>".to_owned(),
        "Vec<String>" => "bstd::vec::Vec<bstd::mem::String<bstd::mem::Alloc<'a>>, bstd::mem::Alloc<'a>>".to_owned(),
        "Result<String, String>" => "Result<bstd::mem::String<bstd::mem::Alloc<'a>>, bstd::mem::String<bstd::mem::Alloc<'a>>>".to_owned(),
        "Result<Vec<u8>, String>" => "Result<bstd::vec::Vec<u8, bstd::mem::Alloc<'a>>, bstd::mem::String<bstd::mem::Alloc<'a>>>".to_owned(),
        "Result<(), String>" => "Result<(), bstd::mem::String<bstd::mem::Alloc<'a>>>".to_owned(),
        "Result<Vec<String>, String>" => "Result<bstd::vec::Vec<bstd::mem::String<bstd::mem::Alloc<'a>>, bstd::mem::Alloc<'a>>, bstd::mem::String<bstd::mem::Alloc<'a>>>".to_owned(),
        "Result<u32, String>" => "Result<u32, bstd::mem::String<bstd::mem::Alloc<'a>>>".to_owned(),
        "Result<u64, String>" => "Result<u64, bstd::mem::String<bstd::mem::Alloc<'a>>>".to_owned(),
        "Result<usize, String>" => "Result<usize, bstd::mem::String<bstd::mem::Alloc<'a>>>".to_owned(),
        "Result<(i32, Vec<u8>), String>" => "Result<(i32, bstd::vec::Vec<u8, bstd::mem::Alloc<'a>>), bstd::mem::String<bstd::mem::Alloc<'a>>>".to_owned(),
        _ => {
            if let Some(inner) = t.strip_prefix("Vec<").and_then(|s| s.strip_suffix('>')) {
                format!("bstd::vec::Vec<{}, bstd::mem::Alloc<'a>>", alloc_type(inner, mode))
            } else {
                t.to_owned()
            }
        }
    }
}

fn emit_sans_io_machine(
    cfg: &SmCfg,
    liveness: &LivenessInfo,
    effect_kinds: &BTreeSet<EffectKind>,
    alloc_mode: SmAllocMode,
) -> String {
    let vis = if cfg.is_pub { "pub " } else { "" };
    let p = &cfg.pascal_name;
    let lt = if alloc_mode == SmAllocMode::BstdExplicitAlloc { "<'a>" } else { "" };
    let str_ty = alloc_type("String", alloc_mode);
    let bytes_ty = alloc_type("Vec<u8>", alloc_mode);
    let ret_ty = alloc_type(&cfg.return_type, alloc_mode);
    let mut out = String::new();

    let _ = writeln!(
        out,
        "// Generated by blaze-bstd-convert: explicit sans-IO state machine for `{}`.",
        cfg.fn_name
    );
    let _ = writeln!(
        out,
        "// Dataflow analysis: {} total locals, at most {} saved at any suspension point (allocator: {}).",
        liveness.total_locals,
        liveness.max_live_at_suspend,
        if alloc_mode == SmAllocMode::BstdExplicitAlloc {
            "explicit `bstd::mem::Alloc<'a>`"
        } else {
            "plain `alloc`"
        }
    );

    // 1. Effect enum.
    let _ = writeln!(out, "#[derive(Debug, Clone, PartialEq, Eq)]");
    let _ = writeln!(out, "{vis}enum {p}Effect{lt} {{");
    for kind in effect_kinds {
        match kind {
            EffectKind::ReadFileString
            | EffectKind::ReadFileBytes
            | EffectKind::RemoveFile
            | EffectKind::ReadDir
            | EffectKind::StatFile
            | EffectKind::CreateDir => {
                let _ = writeln!(out, "    {} {{ path: {str_ty} }},", kind.variant_stem());
            }
            EffectKind::WriteFile => {
                let _ = writeln!(out, "    WriteFile {{ path: {str_ty}, data: {str_ty} }},");
            }
            EffectKind::RenameFile => {
                let _ = writeln!(out, "    RenameFile {{ from: {str_ty}, to: {str_ty} }},");
            }
            EffectKind::ReadEnv => {
                let _ = writeln!(out, "    ReadEnv {{ name: {str_ty} }},");
            }
            EffectKind::ReadArgs | EffectKind::ReadClock | EffectKind::ReadWallClock | EffectKind::Random => {
                let _ = writeln!(out, "    {},", kind.variant_stem());
            }
            EffectKind::Sleep => {
                let _ = writeln!(out, "    Sleep {{ duration_ms: u64 }},");
            }
            EffectKind::NetConnect | EffectKind::NetListen => {
                let _ = writeln!(out, "    {} {{ addr: {str_ty} }},", kind.variant_stem());
            }
            EffectKind::NetRead => {
                let _ = writeln!(out, "    NetRead {{ conn: u32 }},");
            }
            EffectKind::NetWrite => {
                let _ = writeln!(out, "    NetWrite {{ conn: u32, data: {bytes_ty} }},");
            }
            EffectKind::HttpGet => {
                let _ = writeln!(out, "    HttpGet {{ url: {str_ty} }},");
            }
            EffectKind::HttpPost => {
                let _ = writeln!(out, "    HttpPost {{ url: {str_ty}, body: {str_ty} }},");
            }
            EffectKind::RunProcess => {
                let _ = writeln!(out, "    RunProcess {{ program: {str_ty} }},");
            }
            EffectKind::SpawnThread => {
                let _ = writeln!(out, "    SpawnThread,");
            }
            EffectKind::WriteOut | EffectKind::WriteErr => {
                let _ = writeln!(out, "    {} {{ text: {str_ty} }},", kind.variant_stem());
            }
            EffectKind::AwaitFuture => {
                let _ = writeln!(out, "    Await {{ label: {str_ty} }},");
            }
        }
    }
    let _ = writeln!(out, "}}\n");

    // 2. Event enum.
    let _ = writeln!(out, "#[derive(Debug, Clone, PartialEq, Eq)]");
    let _ = writeln!(out, "{vis}enum {p}Event{lt} {{");
    let _ = writeln!(out, "    Start,");
    for kind in effect_kinds {
        let payload = kind.event_payload_type();
        if payload == "()" {
            let _ = writeln!(out, "    {}Done,", kind.variant_stem());
        } else {
            let mapped = alloc_type(payload, alloc_mode);
            let _ = writeln!(out, "    {}Done({mapped}),", kind.variant_stem());
        }
    }
    let _ = writeln!(out, "}}\n");

    // 3. Step enum.
    let _ = writeln!(out, "#[derive(Debug, Clone, PartialEq, Eq)]");
    let _ = writeln!(out, "{vis}enum {p}Step{lt} {{");
    let _ = writeln!(out, "    Need({p}Effect{lt}),");
    let _ = writeln!(out, "    Done({ret_ty}),");
    let _ = writeln!(out, "}}\n");

    // 4. Internal State enum with exact minimal in-scope live variables!
    let _ = writeln!(out, "#[derive(Debug)]");
    let _ = writeln!(out, "enum {p}State{lt} {{");
    if cfg.params.is_empty() {
        let _ = writeln!(out, "    Start,");
    } else {
        let _ = write!(out, "    Start {{ ");
        for (k, ty) in &cfg.params {
            let mapped = alloc_type(ty, alloc_mode);
            let _ = write!(out, "{k}: {mapped}, ");
        }
        let _ = writeln!(out, "}},");
    }

    for bb in &cfg.blocks {
        if let Terminator::Suspend { suspend_index, effect, .. } = &bb.term {
            let saved = liveness
                .saved_at_suspend
                .get(suspend_index)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let vname = format!("Wait{}{suspend_index}", effect.kind.variant_stem());
            if saved.is_empty() {
                let _ = writeln!(out, "    {vname},");
            } else {
                let _ = write!(out, "    {vname} {{ ");
                for var in saved {
                    let raw_ty = cfg
                        .local_types
                        .get(var)
                        .map(String::as_str)
                        .unwrap_or("String");
                    let mapped = alloc_type(raw_ty, alloc_mode);
                    let _ = write!(out, "{var}: {mapped}, ");
                }
                let _ = writeln!(out, "}},");
            }
        }
    }
    if alloc_mode == SmAllocMode::BstdExplicitAlloc {
        let _ = writeln!(out, "    Done(core::marker::PhantomData<&'a ()>),");
    } else {
        let _ = writeln!(out, "    Done,");
    }
    let _ = writeln!(out, "}}\n");

    // 5. Machine struct + `new`, `is_done`, `step`.
    let _ = writeln!(out, "#[derive(Debug)]");
    let _ = writeln!(out, "{vis}struct {p}Machine{lt} {{");
    if alloc_mode == SmAllocMode::BstdExplicitAlloc {
        let _ = writeln!(out, "    alloc: bstd::mem::Alloc<'a>,");
    }
    let _ = writeln!(out, "    state: {p}State{lt},");
    let _ = writeln!(out, "}}\n");

    let _ = writeln!(out, "impl{lt} {p}Machine{lt} {{");
    let _ = write!(out, "    {vis}fn new(");
    let mut first_arg = true;
    if alloc_mode == SmAllocMode::BstdExplicitAlloc {
        let _ = write!(out, "alloc: bstd::mem::Alloc<'a>");
        first_arg = false;
    }
    for (k, ty) in &cfg.params {
        if !first_arg {
            let _ = write!(out, ", ");
        }
        first_arg = false;
        let mapped = alloc_type(ty, alloc_mode);
        let _ = write!(out, "{k}: {mapped}");
    }
    let _ = writeln!(out, ") -> Self {{");
    let alloc_field = if alloc_mode == SmAllocMode::BstdExplicitAlloc { "alloc, " } else { "" };
    if cfg.params.is_empty() {
        let _ = writeln!(out, "        Self {{ {alloc_field}state: {p}State::Start }}");
    } else {
        let _ = write!(out, "        Self {{ {alloc_field}state: {p}State::Start {{ ");
        for (k, _) in &cfg.params {
            let _ = write!(out, "{k}, ");
        }
        let _ = writeln!(out, "}} }}");
    }
    let _ = writeln!(out, "    }}\n");

    let done_pat = if alloc_mode == SmAllocMode::BstdExplicitAlloc {
        format!("{p}State::Done(_)")
    } else {
        format!("{p}State::Done")
    };
    let done_val = if alloc_mode == SmAllocMode::BstdExplicitAlloc {
        format!("{p}State::Done(core::marker::PhantomData)")
    } else {
        format!("{p}State::Done")
    };

    let _ = writeln!(out, "    {vis}fn is_done(&self) -> bool {{");
    let _ = writeln!(out, "        matches!(self.state, {done_pat})");
    let _ = writeln!(out, "    }}\n");

    let _ = writeln!(out, "    {vis}fn step(&mut self, event: {p}Event{lt}) -> {p}Step{lt} {{");
    if alloc_mode == SmAllocMode::BstdExplicitAlloc {
        let _ = writeln!(out, "        let alloc = self.alloc;");
    }
    let _ = writeln!(out, "        let prev = core::mem::replace(&mut self.state, {done_val});");
    let _ = writeln!(out, "        match (prev, event) {{");

    // Entry arm: preserve `mut` on any mutated parameter!
    if cfg.params.is_empty() {
        let _ = writeln!(out, "            ({p}State::Start, {p}Event::Start) => {{");
    } else {
        let _ = write!(out, "            ({p}State::Start {{ ");
        for (k, _) in &cfg.params {
            if cfg.mut_vars.contains(k) {
                let _ = write!(out, "mut {k}, ");
            } else {
                let _ = write!(out, "{k}, ");
            }
        }
        let _ = writeln!(out, "}}, {p}Event::Start) => {{");
    }
    let mut visited = BTreeSet::new();
    emit_block_transition(&mut out, cfg, liveness, cfg.entry, 4, &mut visited, alloc_mode);
    let _ = writeln!(out, "            }}");

    // One arm per suspension state!
    for bb in &cfg.blocks {
        if let Terminator::Suspend {
            suspend_index,
            effect,
            bind,
            bind_mut,
            unwrap_mode,
            resume,
            ..
        } = &bb.term
        {
            let saved = liveness
                .saved_at_suspend
                .get(suspend_index)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let vname = format!("Wait{}{suspend_index}", effect.kind.variant_stem());
            let ev_name = format!("{}Done", effect.kind.variant_stem());
            let payload_is_unit = effect.kind.event_payload_type() == "()";

            if saved.is_empty() {
                let _ = write!(out, "            ({p}State::{vname}, ");
            } else {
                let _ = write!(out, "            ({p}State::{vname} {{ ");
                for var in saved {
                    if cfg.mut_vars.contains(var) {
                        let _ = write!(out, "mut {var}, ");
                    } else {
                        let _ = write!(out, "{var}, ");
                    }
                }
                let _ = write!(out, "}}, ");
            }

            if payload_is_unit {
                let _ = writeln!(out, "{p}Event::{ev_name}) => {{");
            } else {
                let _ = writeln!(out, "{p}Event::{ev_name}(__res)) => {{");
            }

            if !payload_is_unit {
                let mkw = if *bind_mut || bind.as_ref().is_some_and(|b| cfg.mut_vars.contains(b)) {
                    "mut "
                } else {
                    ""
                };
                match (bind, unwrap_mode) {
                    (Some(b), UnwrapMode::TryPropagate) if cfg.returns_result => {
                        let _ = writeln!(
                            out,
                            "                let {mkw}{b} = match __res {{ Ok(v) => v, Err(e) => return {p}Step::Done(Err(e.into())) }};"
                        );
                    }
                    (Some(b), UnwrapMode::UnwrapOrPanic) => {
                        let _ = writeln!(out, "                let {mkw}{b} = __res.expect(\"effect failed\");");
                    }
                    (Some(b), UnwrapMode::UnwrapOrDefault) => {
                        let _ = writeln!(out, "                let {mkw}{b} = __res.unwrap_or_default();");
                    }
                    (Some(b), _) => {
                        let _ = writeln!(out, "                let {mkw}{b} = __res;");
                    }
                    (None, UnwrapMode::TryPropagate) if cfg.returns_result => {
                        let _ = writeln!(
                            out,
                            "                if let Err(e) = __res {{ return {p}Step::Done(Err(e.into())); }}"
                        );
                    }
                    (None, _) => {
                        let _ = writeln!(out, "                let _ = __res;");
                    }
                }
            }

            let mut arm_visited = BTreeSet::new();
            emit_block_transition(&mut out, cfg, liveness, *resume, 4, &mut arm_visited, alloc_mode);
            let _ = writeln!(out, "            }}");
        }
    }

    let _ = writeln!(
        out,
        "            (state, event) => panic!(\"{p}Machine: unexpected event {{event:?}} in state {{state:?}}\"),"
    );
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}");

    out
}

/// Detect whether `header` is reachable from itself without crossing a `Suspend` (a synchronous
/// loop back-edge). If so, we wrap it in a real Rust `loop { ... }` with `continue;` instead of
/// unrolling!
fn is_sync_loop_header(cfg: &SmCfg, target: BlockId) -> bool {
    let mut stack = Vec::new();
    let mut seen = BTreeSet::new();
    push_sync_succs(&cfg.blocks[target.0].term, &mut stack);
    while let Some(cur) = stack.pop() {
        if cur == target {
            return true;
        }
        if seen.insert(cur) {
            push_sync_succs(&cfg.blocks[cur.0].term, &mut stack);
        }
    }
    false
}

fn push_sync_succs(term: &Terminator, stack: &mut Vec<BlockId>) {
    match term {
        Terminator::Branch { then_bb, else_bb, .. } => {
            stack.push(*then_bb);
            stack.push(*else_bb);
        }
        Terminator::Match { arms, .. } => {
            for (_, b) in arms {
                stack.push(*b);
            }
        }
        Terminator::Goto(b) => stack.push(*b),
        Terminator::Suspend { .. } | Terminator::Return(_) | Terminator::Open => {}
    }
}

/// Emit the synchronous body of basic block `bb_id` up to its next suspension or return.
/// Synchronous loop headers emit a genuine Rust `loop { ... }` with labelled `continue 'bb_<id>;`
/// so cyclic CFGs NEVER blow a recursion depth limit!
fn emit_block_transition(
    out: &mut String,
    cfg: &SmCfg,
    liveness: &LivenessInfo,
    bb_id: BlockId,
    indent: usize,
    active_loops: &mut BTreeSet<BlockId>,
    alloc_mode: SmAllocMode,
) {
    let pad = " ".repeat(indent * 4);
    let p = &cfg.pascal_name;

    // Back-edge to an enclosing synchronous loop header -> emit `continue 'bb_<id>;`!
    if active_loops.contains(&bb_id) {
        let _ = writeln!(out, "{pad}continue 'bb_{};", bb_id.0);
        return;
    }

    let is_loop = is_sync_loop_header(cfg, bb_id);
    let inner_indent = if is_loop {
        active_loops.insert(bb_id);
        let _ = writeln!(out, "{pad}'bb_{}: loop {{", bb_id.0);
        indent + 1
    } else {
        indent
    };
    let ipad = " ".repeat(inner_indent * 4);

    let bb = &cfg.blocks[bb_id.0];
    for stmt in &bb.stmts {
        let code = if alloc_mode == SmAllocMode::BstdExplicitAlloc {
            stmt.code
                .replace("format!(", "bstd::mem::format_in!(alloc, ")
                .replace("Vec::new()", "bstd::vec::Vec::new_in(alloc)")
                .replace("String::new()", "bstd::mem::String::new_in(alloc)")
        } else {
            stmt.code.clone()
        };
        let _ = writeln!(out, "{ipad}{code}");
    }

    let done_val = if alloc_mode == SmAllocMode::BstdExplicitAlloc {
        format!("{p}State::Done(core::marker::PhantomData)")
    } else {
        format!("{p}State::Done")
    };

    match &bb.term {
        Terminator::Suspend { suspend_index, effect, .. } => {
            let saved = liveness
                .saved_at_suspend
                .get(suspend_index)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let vname = format!("Wait{}{suspend_index}", effect.kind.variant_stem());
            let eff_expr = format_effect_ctor(p, effect, &cfg.local_types, alloc_mode);
            let _ = writeln!(out, "{ipad}let __effect = {eff_expr};");
            if saved.is_empty() {
                let _ = writeln!(out, "{ipad}self.state = {p}State::{vname};");
            } else {
                let _ = write!(out, "{ipad}self.state = {p}State::{vname} {{ ");
                for var in saved {
                    let _ = write!(out, "{var}, ");
                }
                let _ = writeln!(out, "}};");
            }
            let _ = writeln!(out, "{ipad}return {p}Step::Need(__effect);");
        }
        Terminator::Branch { cond, then_bb, else_bb } => {
            let _ = writeln!(out, "{ipad}if {cond} {{");
            emit_block_transition(out, cfg, liveness, *then_bb, inner_indent + 1, active_loops, alloc_mode);
            let _ = writeln!(out, "{ipad}}} else {{");
            emit_block_transition(out, cfg, liveness, *else_bb, inner_indent + 1, active_loops, alloc_mode);
            let _ = writeln!(out, "{ipad}}}");
        }
        Terminator::Match { scrutinee, arms } => {
            let _ = writeln!(out, "{ipad}match {scrutinee} {{");
            for (pat, arm_bb) in arms {
                let _ = writeln!(out, "{ipad}    {pat} => {{");
                emit_block_transition(out, cfg, liveness, *arm_bb, inner_indent + 2, active_loops, alloc_mode);
                let _ = writeln!(out, "{ipad}    }}");
            }
            let _ = writeln!(out, "{ipad}}}");
        }
        Terminator::Goto(target) => {
            emit_block_transition(out, cfg, liveness, *target, inner_indent, active_loops, alloc_mode);
        }
        Terminator::Return(expr) => {
            let _ = writeln!(out, "{ipad}self.state = {done_val};");
            let _ = writeln!(out, "{ipad}return {p}Step::Done({expr});");
        }
        Terminator::Open => {
            let _ = writeln!(out, "{ipad}self.state = {done_val};");
            let fallback = if cfg.returns_result { "Ok(())" } else { "()" };
            let _ = writeln!(out, "{ipad}return {p}Step::Done({fallback});");
        }
    }

    if is_loop {
        active_loops.remove(&bb_id);
        let _ = writeln!(out, "{pad}}}");
    }
}

fn format_path_arg(arg: &str, local_types: &BTreeMap<String, String>, alloc_mode: SmAllocMode) -> String {
    let bare = arg.trim().trim_start_matches('&').trim();
    let is_pathbuf = local_types
        .get(bare)
        .is_some_and(|t| t == "PathBuf" || t.ends_with("::PathBuf"))
        || arg.contains("PathBuf")
        || arg.contains(".join(");
    match (is_pathbuf, alloc_mode) {
        (true, SmAllocMode::BstdExplicitAlloc) => {
            format!("bstd::mem::format_in!(alloc, \"{{}}\", ({arg}).display())")
        }
        (false, SmAllocMode::BstdExplicitAlloc) => {
            format!("bstd::mem::format_in!(alloc, \"{{}}\", {arg})")
        }
        (true, SmAllocMode::PlainAlloc) => format!("({arg}).display().to_string()"),
        (false, SmAllocMode::PlainAlloc) => format!("({arg}).to_string()"),
    }
}

fn format_str_arg(arg: &str, alloc_mode: SmAllocMode) -> String {
    match alloc_mode {
        SmAllocMode::BstdExplicitAlloc => {
            if let Some(rest) = arg.strip_prefix("format!(") {
                format!("bstd::mem::format_in!(alloc, {rest}")
            } else {
                format!("bstd::mem::format_in!(alloc, \"{{}}\", {arg})")
            }
        }
        SmAllocMode::PlainAlloc => {
            if arg.starts_with("format!(") {
                arg.to_owned()
            } else {
                format!("({arg}).to_string()")
            }
        }
    }
}

fn format_effect_ctor(
    pascal: &str,
    effect: &EffectCall,
    local_types: &BTreeMap<String, String>,
    alloc_mode: SmAllocMode,
) -> String {
    let arg0 = effect.args.first().map(String::as_str).unwrap_or("\"\"");
    let arg1 = effect.args.get(1).map(String::as_str).unwrap_or("\"\"");
    let stem = effect.kind.variant_stem();
    match effect.kind {
        EffectKind::ReadFileString
        | EffectKind::ReadFileBytes
        | EffectKind::RemoveFile
        | EffectKind::ReadDir
        | EffectKind::StatFile
        | EffectKind::CreateDir => {
            let p = format_path_arg(arg0, local_types, alloc_mode);
            format!("{pascal}Effect::{stem} {{ path: {p} }}")
        }
        EffectKind::WriteFile => {
            let p = format_path_arg(arg0, local_types, alloc_mode);
            let d = format_str_arg(arg1, alloc_mode);
            format!("{pascal}Effect::WriteFile {{ path: {p}, data: {d} }}")
        }
        EffectKind::RenameFile => {
            let from = format_path_arg(arg0, local_types, alloc_mode);
            let to = format_path_arg(arg1, local_types, alloc_mode);
            format!("{pascal}Effect::RenameFile {{ from: {from}, to: {to} }}")
        }
        EffectKind::ReadEnv => {
            let n = format_str_arg(arg0, alloc_mode);
            format!("{pascal}Effect::ReadEnv {{ name: {n} }}")
        }
        EffectKind::ReadArgs
        | EffectKind::ReadClock
        | EffectKind::ReadWallClock
        | EffectKind::Random
        | EffectKind::SpawnThread => {
            format!("{pascal}Effect::{stem}")
        }
        EffectKind::Sleep => {
            format!("{pascal}Effect::Sleep {{ duration_ms: ({arg0}).as_millis() as u64 }}")
        }
        EffectKind::NetConnect | EffectKind::NetListen => {
            let a = format_str_arg(arg0, alloc_mode);
            format!("{pascal}Effect::{stem} {{ addr: {a} }}")
        }
        EffectKind::NetRead => format!("{pascal}Effect::NetRead {{ conn: 0 }}"),
        EffectKind::NetWrite => {
            let empty_vec = if alloc_mode == SmAllocMode::BstdExplicitAlloc {
                "bstd::vec::Vec::new_in(alloc)"
            } else {
                "Vec::new()"
            };
            format!("{pascal}Effect::NetWrite {{ conn: 0, data: {empty_vec} }}")
        }
        EffectKind::HttpGet => {
            let u = format_str_arg(arg0, alloc_mode);
            format!("{pascal}Effect::HttpGet {{ url: {u} }}")
        }
        EffectKind::HttpPost => {
            let u = format_str_arg(arg0, alloc_mode);
            let b = format_str_arg(arg1, alloc_mode);
            format!("{pascal}Effect::HttpPost {{ url: {u}, body: {b} }}")
        }
        EffectKind::RunProcess => {
            let prog = format_str_arg(arg0, alloc_mode);
            format!("{pascal}Effect::RunProcess {{ program: {prog} }}")
        }
        EffectKind::WriteOut | EffectKind::WriteErr => {
            let t = format_str_arg(arg0, alloc_mode);
            format!("{pascal}Effect::{stem} {{ text: {t} }}")
        }
        EffectKind::AwaitFuture => {
            let lbl = if alloc_mode == SmAllocMode::BstdExplicitAlloc {
                format!("bstd::mem::String::from_str_in({:?}, alloc)", effect.raw_expr)
            } else {
                format!("{:?}.to_string()", effect.raw_expr)
            };
            format!("{pascal}Effect::Await {{ label: {lbl} }}")
        }
    }
}

fn emit_bstd_driver(
    cfg: &SmCfg,
    effect_kinds: &BTreeSet<EffectKind>,
    alloc_mode: SmAllocMode,
) -> String {
    let vis = if cfg.is_pub { "pub " } else { "" };
    let p = &cfg.pascal_name;
    let driver_fn = to_snake_case(p);
    let lt = if alloc_mode == SmAllocMode::BstdExplicitAlloc { "<'a>" } else { "" };
    let ret_ty = alloc_type(&cfg.return_type, alloc_mode);
    let mut out = String::new();

    let mut caps: BTreeMap<&'static str, &'static str> = BTreeMap::new();
    for kind in effect_kinds {
        let key = kind.capability_key();
        if key == "fs_ro" && caps.contains_key("fs_rw") {
            continue;
        }
        if key == "fs_rw" {
            caps.remove("fs_ro");
        }
        if key != "async" {
            caps.insert(key, kind.bstd_capability());
        }
    }

    let _ = writeln!(
        out,
        "// Thin `bstd` driver for `{p}Machine`: explicit `Cx`, explicit `Alloc`, and narrowed capabilities only."
    );
    let _ = write!(out, "{vis}fn {driver_fn}_bstd{lt}(cx: &bstd::Cx");
    if alloc_mode == SmAllocMode::BstdExplicitAlloc {
        let _ = write!(out, ", alloc: bstd::mem::Alloc<'a>");
    }
    for cap in caps.values() {
        let _ = write!(out, ", {cap}");
    }
    for (k, ty) in &cfg.params {
        let mapped = alloc_type(ty, alloc_mode);
        let _ = write!(out, ", {k}: {mapped}");
    }
    let _ = writeln!(out, ") -> {ret_ty} {{");
    let _ = writeln!(out, "    const MAX_STEPS: usize = 10_000;");
    let _ = write!(out, "    let mut machine = {p}Machine::new(");
    let mut first = true;
    if alloc_mode == SmAllocMode::BstdExplicitAlloc {
        let _ = write!(out, "alloc");
        first = false;
    }
    for (k, _) in &cfg.params {
        if !first {
            let _ = write!(out, ", ");
        }
        first = false;
        let _ = write!(out, "{k}");
    }
    let _ = writeln!(out, ");");
    let _ = writeln!(out, "    let mut event = {p}Event::Start;");
    let _ = writeln!(out, "    for _ in 0..MAX_STEPS {{");
    let _ = writeln!(out, "        match machine.step(event) {{");
    let _ = writeln!(out, "            {p}Step::Done(val) => return val,");
    let _ = writeln!(out, "            {p}Step::Need(effect) => {{");
    let _ = writeln!(out, "                event = match effect {{");
    let err_fmt = |msg: &str| {
        if alloc_mode == SmAllocMode::BstdExplicitAlloc {
            format!("bstd::mem::format_in!(alloc, {msg})")
        } else {
            format!("format!({msg})")
        }
    };
    let str_in = |expr: &str| {
        if alloc_mode == SmAllocMode::BstdExplicitAlloc {
            format!("bstd::mem::String::from_str_in({expr}, alloc)")
        } else {
            format!("({expr}).to_owned()")
        }
    };
    for kind in effect_kinds {
        match kind {
            EffectKind::ReadFileString => {
                let ef = err_fmt("\"{e}\"");
                let ok_s = str_in("s");
                let _ = writeln!(
                    out,
                    "                    {p}Effect::ReadFileString {{ path }} => {{ let r = bstd::Path::new(path.as_str()).map_err(|e| {ef}).and_then(|p| fs.read_all(cx, &p, cx.frame()).map_err(|e| {ef})).and_then(|b| core::str::from_utf8(&b).map(|s| {ok_s}).map_err(|e| {ef})); {p}Event::ReadFileStringDone(r) }}"
                );
            }
            EffectKind::ReadFileBytes => {
                let ef = err_fmt("\"{e}\"");
                let ok_b = if alloc_mode == SmAllocMode::BstdExplicitAlloc {
                    "bstd::vec::Vec::from_slice_in(&b, alloc)"
                } else {
                    "b.to_vec()"
                };
                let _ = writeln!(
                    out,
                    "                    {p}Effect::ReadFileBytes {{ path }} => {{ let r = bstd::Path::new(path.as_str()).map_err(|e| {ef}).and_then(|p| fs.read_all(cx, &p, cx.frame()).map(|b| {ok_b}).map_err(|e| {ef})); {p}Event::ReadFileBytesDone(r) }}"
                );
            }
            EffectKind::WriteFile => {
                let ef = err_fmt("\"{e}\"");
                let _ = writeln!(
                    out,
                    "                    {p}Effect::WriteFile {{ path, data }} => {{ let r = bstd::Path::new(path.as_str()).map_err(|e| {ef}).and_then(|p| fs.publish_unsynced(cx, &p, bstd::fs::Content::Bytes(data.as_bytes()), bstd::fs::Unsynced::No).map(|_| ()).map_err(|e| {ef})); {p}Event::WriteFileDone(r) }}"
                );
            }
            EffectKind::ReadEnv => {
                let ef = err_fmt("\"{e}\"");
                let miss = err_fmt("\"missing env {name}\"");
                let ok_s = str_in("s");
                let _ = writeln!(
                    out,
                    "                    {p}Effect::ReadEnv {{ name }} => {{ let r = bstd::VarName::new(name.as_str()).map_err(|e| {ef}).and_then(|v| env.get(&v).map(|s| {ok_s}).ok_or_else(|| {miss})); {p}Event::ReadEnvDone(r) }}"
                );
            }
            EffectKind::ReadClock => {
                let _ = writeln!(
                    out,
                    "                    {p}Effect::ReadClock => {p}Event::ReadClockDone(clock.now().since_start().as_nanos() as u64),"
                );
            }
            EffectKind::ReadWallClock => {
                let _ = writeln!(
                    out,
                    "                    {p}Effect::ReadWallClock => {p}Event::ReadWallClockDone(clock.wall(cx).map(|w| w.as_ns()).unwrap_or(0)),"
                );
            }
            EffectKind::Sleep => {
                let _ = writeln!(
                    out,
                    "                    {p}Effect::Sleep {{ duration_ms }} => {{ let _ = clock.sleep(cx, core::time::Duration::from_millis(duration_ms)); {p}Event::SleepDone }}"
                );
            }
            EffectKind::WriteOut => {
                let _ = writeln!(
                    out,
                    "                    {p}Effect::WriteOut {{ text }} => {{ let _ = out.line(cx, text.as_str()); {p}Event::WriteOutDone }}"
                );
            }
            EffectKind::WriteErr => {
                let _ = writeln!(
                    out,
                    "                    {p}Effect::WriteErr {{ text }} => {{ let _ = out.err_line(cx, text.as_str()); {p}Event::WriteErrDone }}"
                );
            }
            EffectKind::HttpGet => {
                let ue = str_in("\"wire bstd::requests::Http::get\"");
                let _ = writeln!(
                    out,
                    "                    {p}Effect::HttpGet {{ url }} => {{ let _ = (cx, http, &url); {p}Event::HttpGetDone(Err({ue})) }}"
                );
            }
            EffectKind::HttpPost => {
                let ue = str_in("\"wire bstd::requests::Http::post\"");
                let _ = writeln!(
                    out,
                    "                    {p}Effect::HttpPost {{ url, body }} => {{ let _ = (cx, http, &url, &body); {p}Event::HttpPostDone(Err({ue})) }}"
                );
            }
            other => {
                let stem = other.variant_stem();
                let payload = other.event_payload_type();
                if payload == "()" {
                    let _ = writeln!(out, "                    {p}Effect::{stem} {{ .. }} => {p}Event::{stem}Done,");
                } else {
                    let ue = str_in(&format!("\"unwired effect {stem}\""));
                    let _ = writeln!(
                        out,
                        "                    {p}Effect::{stem} {{ .. }} => {p}Event::{stem}Done(Err({ue})),"
                    );
                }
            }
        }
    }
    let _ = writeln!(out, "                }};");
    let _ = writeln!(out, "            }}");
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(
        out,
        "    panic!(\"{driver_fn}_bstd: exceeded MAX_STEPS ({{MAX_STEPS}}) without reaching Done (bounded-progress)\");"
    );
    let _ = writeln!(out, "}}");

    out
}

fn emit_sim_test(
    cfg: &SmCfg,
    effect_kinds: &BTreeSet<EffectKind>,
    alloc_mode: SmAllocMode,
) -> String {
    let p = &cfg.pascal_name;
    let test_fn = to_snake_case(p);
    let mut out = String::new();
    let _ = writeln!(out, "#[cfg(test)]");
    let _ = writeln!(out, "fn {test_fn}_machine_sim_smoke() {{");
    if alloc_mode == SmAllocMode::BstdExplicitAlloc {
        let _ = writeln!(out, "    let alloc = bstd::mem::Heap::testing().alloc();");
    }
    let _ = write!(out, "    let mut m = {p}Machine::new(");
    let mut first = true;
    if alloc_mode == SmAllocMode::BstdExplicitAlloc {
        let _ = write!(out, "alloc");
        first = false;
    }
    for (_, ty) in &cfg.params {
        if !first {
            let _ = write!(out, ", ");
        }
        first = false;
        let _ = write!(out, "Default::default() /* {ty} */");
    }
    let _ = writeln!(out, ");");
    let _ = writeln!(out, "    let mut step = m.step({p}Event::Start);");
    let _ = writeln!(out, "    for _ in 0..64 {{");
    let _ = writeln!(out, "        match step {{");
    let _ = writeln!(out, "            {p}Step::Done(_) => {{ assert!(m.is_done()); return; }}");
    let _ = writeln!(out, "            {p}Step::Need(eff) => {{");
    let _ = writeln!(out, "                let ev = match eff {{");
    let empty_str = if alloc_mode == SmAllocMode::BstdExplicitAlloc {
        "bstd::mem::String::new_in(alloc)"
    } else {
        "String::new()"
    };
    let empty_vec = if alloc_mode == SmAllocMode::BstdExplicitAlloc {
        "bstd::vec::Vec::new_in(alloc)"
    } else {
        "Vec::new()"
    };
    for kind in effect_kinds {
        let stem = kind.variant_stem();
        let payload = kind.event_payload_type();
        let sample = match payload {
            "()" => format!("{p}Event::{stem}Done"),
            "u64" => format!("{p}Event::{stem}Done(0)"),
            "Vec<String>" => format!("{p}Event::{stem}Done({empty_vec})"),
            "Result<String, String>" => format!("{p}Event::{stem}Done(Ok({empty_str}))"),
            "Result<Vec<u8>, String>" => format!("{p}Event::{stem}Done(Ok({empty_vec}))"),
            "Result<(), String>" => format!("{p}Event::{stem}Done(Ok(()))"),
            "Result<u32, String>" | "Result<u64, String>" | "Result<usize, String>" => {
                format!("{p}Event::{stem}Done(Ok(0))")
            }
            "Result<Vec<String>, String>" => format!("{p}Event::{stem}Done(Ok({empty_vec}))"),
            "Result<(i32, Vec<u8>), String>" => format!("{p}Event::{stem}Done(Ok((0, {empty_vec})))"),
            _ => format!("{p}Event::{stem}Done(Default::default())"),
        };
        let pattern = match kind {
            EffectKind::ReadArgs
            | EffectKind::ReadClock
            | EffectKind::ReadWallClock
            | EffectKind::Random
            | EffectKind::SpawnThread => {
                format!("{p}Effect::{stem}")
            }
            _ => format!("{p}Effect::{stem} {{ .. }}"),
        };
        let _ = writeln!(out, "                    {pattern} => {sample},");
    }
    let _ = writeln!(out, "                }};");
    let _ = writeln!(out, "                step = m.step(ev);");
    let _ = writeln!(out, "            }}");
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}");
    out
}

// -----------------------------------------------------------------------------
// 4. Implicit-State Struct Splitter
// -----------------------------------------------------------------------------

pub fn compile_struct_sm(cand: &StructSmCandidate) -> String {
    let name = &cand.name;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "// Generated by blaze-bstd-convert from `{}` ({}:{}): {}",
        name,
        cand.file,
        cand.line,
        cand.reason()
    );
    let _ = writeln!(out, "// Core state machine is `#![blaze::sans_io]`: owns zero sockets, files, or clocks.\n");

    let _ = writeln!(out, "#[derive(Debug, Clone, PartialEq)]");
    let _ = writeln!(out, "pub enum {name}State {{");
    let _ = writeln!(out, "    Init,");
    if !cand.phase_fields.is_empty() || !cand.bool_flag_fields.is_empty() {
        let _ = writeln!(out, "    Active {{");
        for (fname, fty) in &cand.phase_fields {
            let _ = writeln!(out, "        {fname}: {fty},");
        }
        for bname in &cand.bool_flag_fields {
            let _ = writeln!(out, "        // Fold `{bname}: bool` into distinct enum variants where illegal mixes exist.");
            let _ = writeln!(out, "        {bname}: bool,");
        }
        for (oname, oty) in &cand.option_fields {
            let _ = writeln!(out, "        {oname}: {oty},");
        }
        let _ = writeln!(out, "    }},");
    }
    let _ = writeln!(out, "    Closed,");
    let _ = writeln!(out, "}}\n");

    let _ = writeln!(out, "#[derive(Debug)]");
    let _ = writeln!(out, "pub enum {name}Effect {{");
    for (fname, fty) in &cand.io_fields {
        let _ = writeln!(out, "    /// Driven by `{fname}: {fty}` in the thin bstd driver.");
        let _ = writeln!(out, "    Io_{}(Vec<u8>),", to_pascal_case(fname));
    }
    let _ = writeln!(out, "    ScheduleTimer {{ at_ns: u64 }},");
    let _ = writeln!(out, "}}\n");

    let _ = writeln!(out, "#[derive(Debug)]");
    let _ = writeln!(out, "pub enum {name}Event {{");
    let _ = writeln!(out, "    Tick {{ now_ns: u64 }},");
    for (fname, _) in &cand.io_fields {
        let _ = writeln!(out, "    On{}(Vec<u8>),", to_pascal_case(fname));
    }
    let _ = writeln!(out, "    Close,");
    let _ = writeln!(out, "}}\n");

    let _ = writeln!(out, "#[derive(Debug)]");
    let _ = writeln!(out, "pub struct {name}Core {{");
    let _ = writeln!(out, "    pub state: {name}State,");
    for (fname, fty) in &cand.other_fields {
        let _ = writeln!(out, "    pub {fname}: {fty},");
    }
    let _ = writeln!(out, "}}\n");

    let _ = writeln!(out, "impl {name}Core {{");
    let _ = writeln!(out, "    pub fn step(&mut self, event: {name}Event, out: &mut Vec<{name}Effect>) {{");
    let _ = writeln!(out, "        match event {{");
    let _ = writeln!(out, "            {name}Event::Close => self.state = {name}State::Closed,");
    let _ = writeln!(out, "            {name}Event::Tick {{ now_ns }} => {{ let _ = (now_ns, out); }}");
    for (fname, _) in &cand.io_fields {
        let _ = writeln!(
            out,
            "            {name}Event::On{}(bytes) => {{ let _ = (bytes, out); }}",
            to_pascal_case(fname)
        );
    }
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}\n");

    if !cand.io_fields.is_empty() {
        let _ = writeln!(out, "/// Thin `bstd` driver owning only the live IO handles separated from `{name}Core`.");
        let _ = writeln!(out, "pub struct {name}Driver {{");
        let _ = writeln!(out, "    pub core: {name}Core,");
        for (fname, fty) in &cand.io_fields {
            let _ = writeln!(out, "    // Migrate `{fty}` to its `bstd` handle (`bstd::net::Conn`, `bstd::clock::Instant`, `bstd::fs::File`).");
            let _ = writeln!(out, "    pub {fname}: {fty},");
        }
        let _ = writeln!(out, "}}");
    }

    out
}

// -----------------------------------------------------------------------------
// 5. Custom Proc-Macro -> `comptime` / `const` Reflection Migrator
// -----------------------------------------------------------------------------

pub fn generate_comptime_scaffold(crate_name: &str, entries: &[ProcMacroEntry]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "//! Comptime / `const` reflection replacement for custom proc-macro crate `{crate_name}`."
    );
    let _ = writeln!(
        out,
        "//! Eliminates `syn`, `quote`, `proc-macro2`, and Wasm proc-macro compilation by computing"
    );
    let _ = writeln!(
        out,
        "//! field layouts, offsets, enum tags, and checks at compile time via `const fn` + `core::mem::type_info`."
    );
    let _ = writeln!(out, "#![no_std]");
    let _ = writeln!(out, "#![feature(type_info)]");
    let _ = writeln!(out, "#![forbid(unsafe_code)]\n");
    let _ = writeln!(out, "use core::mem::type_info::{{Type, TypeKind}};\n");

    let _ = writeln!(out, "/// Field descriptor computed purely at compile time from `Type::of::<T>()`.");
    let _ = writeln!(out, "#[derive(Debug, Clone, Copy, PartialEq, Eq)]");
    let _ = writeln!(out, "pub struct FieldDesc {{");
    let _ = writeln!(out, "    pub name: &'static str,");
    let _ = writeln!(out, "    pub offset: usize,");
    let _ = writeln!(out, "}}\n");

    let _ = writeln!(out, "/// Compile-time field count of any struct `T` with zero proc macros.");
    let _ = writeln!(out, "pub const fn field_count<T: 'static>() -> usize {{");
    let _ = writeln!(out, "    match Type::of::<T>().kind {{");
    let _ = writeln!(out, "        TypeKind::Struct(s) => s.fields.len(),");
    let _ = writeln!(out, "        _ => panic!(\"expected a struct type for comptime reflection\"),");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}\n");

    for entry in entries {
        let label = entry.derive_name.as_deref().unwrap_or(&entry.fn_name);
        let pascal = to_pascal_case(label);
        let _ = writeln!(
            out,
            "// Replaces `#[{}({})]` from {}:{} ({})",
            entry.kind.as_str(),
            label,
            entry.file,
            entry.line,
            entry.pattern.comptime_recommendation()
        );
        match entry.pattern {
            ProcMacroPattern::FieldReflection | ProcMacroPattern::AttributeCodegen => {
                let _ = writeln!(out, "pub trait Comptime{pascal}: Sized + 'static {{");
                let _ = writeln!(out, "    const SIZE: usize = core::mem::size_of::<Self>();");
                let _ = writeln!(out, "    const ALIGN: usize = core::mem::align_of::<Self>();");
                let _ = writeln!(out, "    const FIELD_COUNT: usize = field_count::<Self>();");
                let _ = writeln!(out, "}}\n");
            }
            ProcMacroPattern::VariantReflection => {
                let _ = writeln!(
                    out,
                    "// Use `bstd::enum_tags!` or `bstd::comptime::TaggedEnum` in place of `{label}`."
                );
                let _ = writeln!(out, "pub use bstd::comptime::TaggedEnum as Comptime{pascal};\n");
            }
            ProcMacroPattern::TokenDsl => {
                let _ = writeln!(out, "pub const fn eval_{}(input: &str) -> usize {{", entry.fn_name);
                let _ = writeln!(out, "    // Replace token-stream parsing with a pure `const fn` parser over `input`.");
                let _ = writeln!(out, "    input.len()");
                let _ = writeln!(out, "}}\n");
            }
        }
    }

    out
}

/// Generate a declarative `[package.metadata.blaze.c]` TOML snippet for a crate migrating off
/// `build.rs` (`cc`, `cmake`, `cxx-build`, `bindgen`, `pkg-config`).
pub fn generate_blaze_c_toml_section(info: &CInteropInfo) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "[package.metadata.blaze.c]");
    if !info.source_files.is_empty() {
        let items: Vec<String> = info.source_files.iter().map(|s| format!("\"{s}\"")).collect();
        let _ = writeln!(out, "sources = [{}]", items.join(", "));
    } else if info.is_cpp {
        let _ = writeln!(out, "# sources = [\"vendor/**/*.cc\"]");
    } else {
        let _ = writeln!(out, "# sources = [\"vendor/**/*.c\"]");
    }
    if !info.include_dirs.is_empty() {
        let items: Vec<String> = info.include_dirs.iter().map(|s| format!("\"{s}\"")).collect();
        let _ = writeln!(out, "include = [{}]", items.join(", "));
    }
    if !info.defines.is_empty() {
        let pairs: Vec<String> = info
            .defines
            .iter()
            .map(|(k, v)| {
                let val = v.as_deref().unwrap_or("1");
                format!("{k} = \"{val}\"")
            })
            .collect();
        let _ = writeln!(out, "define = {{ {} }}", pairs.join(", "));
    }
    if let Some(std_flag) = &info.std_flag {
        let _ = writeln!(out, "std = \"{std_flag}\"");
    } else if info.is_cpp {
        let _ = writeln!(out, "std = \"c++20\"");
    }
    out
}

/// Generate a `blaze::c::import!` / `blaze::cpp::import!` migration module replacing `bindgen`,
/// `cc`, `cmake`, `cxx`, `autocxx`, and `pkg-config` `build.rs` scripts (decisions D174 & D176).
pub fn generate_c_interop_scaffold(crate_name: &str, info: &CInteropInfo) -> String {
    let mut out = String::new();
    let tools: Vec<&str> = info.tools.iter().map(String::as_str).collect();
    let macro_name = info.import_macro();
    let lang = if info.is_cpp { "C++" } else { "C" };
    let mod_name: String = crate_name
        .trim_end_matches("-sys")
        .trim_end_matches("_sys")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    let mod_ident = if mod_name.is_empty() { "ffi".to_owned() } else { mod_name };

    let _ = writeln!(
        out,
        "//! Hermetic {lang} header import migration for `{crate_name}` (replaces `{}`).",
        tools.join("`, `")
    );
    let _ = writeln!(
        out,
        "//! Built on blaze's pinned C/C++ toolchain (`docs/v2/plan/design/c.md`, decisions D174/D176):"
    );
    let _ = writeln!(
        out,
        "//! - No `build.rs`, no host `libclang`/`bindgen`, and no host `cc`/`cmake`/`pkg-config`."
    );
    let _ = writeln!(
        out,
        "//! - Declarations are cached as content-addressed records keyed by the headers read."
    );
    let _ = writeln!(
        out,
        "//! - {lang} source units in `[package.metadata.blaze.c]` compile per file, lazily on demand."
    );
    if !info.out_dir_includes.is_empty() {
        let _ = writeln!(out, "//!");
        let _ = writeln!(
            out,
            "//! Replace these `include!(concat!(env!(\"OUT_DIR\"), ...))` bindgen sites with `use crate::c_interop_migration::{mod_ident}::*;`:"
        );
        for site in &info.out_dir_includes {
            let _ = writeln!(out, "//!   - `{site}`");
        }
    }
    let _ = writeln!(out, "#![no_std]\n");

    let _ = writeln!(out, "{macro_name} {{");
    let _ = writeln!(out, "    mod {mod_ident};");
    if info.headers.is_empty() {
        let ext = if info.is_cpp { "hpp" } else { "h" };
        let _ = writeln!(out, "    header \"{mod_ident}.{ext}\";");
    } else {
        for h in &info.headers {
            let _ = writeln!(out, "    header \"{h}\";");
        }
    }
    for dir in &info.include_dirs {
        let _ = writeln!(out, "    include \"{dir}\";");
    }
    for (k, v) in &info.defines {
        if let Some(val) = v {
            let _ = writeln!(out, "    define {k} = \"{val}\";");
        } else {
            let _ = writeln!(out, "    define {k};");
        }
    }
    if let Some(std_flag) = &info.std_flag {
        let _ = writeln!(out, "    std \"{std_flag}\";");
    } else if info.is_cpp {
        let _ = writeln!(out, "    std \"c++20\";");
    }
    let _ = writeln!(out, "}}");

    out
}

/// Write a `#![no_std]` `src/newtypes.rs` module promoting bare domain primitives (`offset`/`len`,
/// `*_id`, `port`, `*_ms`, `align`) into explicit `bstd::newtype!` / `bstd::id::id!` types
/// (affine `Offset`/`Len` algebra, zero blanket `Deref`, composable with `bstd::TypeSet!`).
pub fn write_newtypes_scaffold(
    crate_name: &str,
    candidates: &[NewtypeCandidate],
    out: &mut impl core::fmt::Write,
) {
    let _ = writeln!(
        out,
        "//! Explicit zero-cost domain newtypes for `{crate_name}` (`bstd::newtype!`, rules 76 & 85)."
    );
    let _ = writeln!(
        out,
        "//! - Affine `Offset` + `Len` → `Offset`, `Offset - Offset` → `Len` (`Offset + Offset` is a compile error)."
    );
    let _ = writeln!(
        out,
        "//! - No blanket `Deref` on numeric types; compose bundles order-independently with `bstd::TypeSet!`."
    );
    let _ = writeln!(out, "#![no_std]");
    let _ = writeln!(out, "#![forbid(unsafe_code)]\n");

    let mut reexports: BTreeSet<&'static str> = BTreeSet::new();
    let mut emitted_ids: BTreeSet<String> = BTreeSet::new();
    let mut id_u32_decls: Vec<String> = Vec::new();
    let mut id_opaque_decls: Vec<(String, String)> = Vec::new();

    for c in candidates {
        match c.kind {
            NewtypeKind::AffineOffsetLen => {
                if c.raw_type == "u32" {
                    reexports.insert("TextOffset");
                    reexports.insert("TextLen");
                    reexports.insert("TextSpan");
                } else {
                    reexports.insert("ByteOffset");
                    reexports.insert("ByteLen");
                    reexports.insert("ByteSpan");
                }
            }
            NewtypeKind::OpaquePort => {
                reexports.insert("Port");
            }
            NewtypeKind::AlignmentPow2 => {
                reexports.insert("Alignment");
            }
            NewtypeKind::DurationUnit => {}
            NewtypeKind::TypedId => {
                let pascal = to_pascal_case(&c.name);
                if emitted_ids.insert(pascal.clone()) {
                    if c.raw_type == "u32" {
                        id_u32_decls.push(pascal);
                    } else {
                        id_opaque_decls.push((pascal, c.raw_type.clone()));
                    }
                }
            }
        }
    }

    if !reexports.is_empty() {
        let list: Vec<&str> = reexports.into_iter().collect();
        let _ = writeln!(out, "pub use bstd::newtype::{{{}}};\n", list.join(", "));
    }

    for id_name in &id_u32_decls {
        let _ = writeln!(out, "bstd::id::id! {{");
        let _ = writeln!(out, "    pub struct {id_name};");
        let _ = writeln!(out, "}}\n");
    }

    for (id_name, raw_ty) in &id_opaque_decls {
        let _ = writeln!(out, "bstd::newtype! {{");
        let _ = writeln!(out, "    opaque pub struct {id_name}({raw_ty});");
        let _ = writeln!(out, "}}\n");
    }

    let _ = writeln!(out, "// Detected bare-primitive sites to migrate (sample up to 20):");
    for c in candidates.iter().take(20) {
        let _ = writeln!(
            out,
            "// - {}:{} ({}) `{}: {}` -> `{}`",
            c.file, c.line, c.owner, c.name, c.raw_type, c.suggestion
        );
    }
}

pub fn to_pascal_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut cap_next = true;
    for c in s.chars() {
        if c == '_' || c == '-' || !c.is_ascii_alphanumeric() {
            cap_next = true;
        } else if cap_next {
            out.push(c.to_ascii_uppercase());
            cap_next = false;
        } else {
            out.push(c);
        }
    }
    if out.is_empty() {
        "Machine".to_owned()
    } else {
        out
    }
}

fn to_snake_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (idx, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if idx > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}
