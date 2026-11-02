//! E0061 "this function takes 2 arguments but 1 argument was supplied" (E0057 for a call through
//! a `Fn` trait, E0060 for a C-variadic function): a call with the wrong number of arguments, on
//! the callee, with each argument that is missing, unexpected, swapped or moved labelled, the
//! function's definition and the parameters left without an argument, and the arguments to
//! provide, remove, swap or reorder; or the parentheses that make a run of arguments the tuple
//! a parameter takes.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/fn_ctxt/checks.rs` (`check_argument_types`,
//! `report_arg_errors`, `FnCallDiagCtxt`, `label_fn_like`, `get_hir_param_info`) and
//! `fn_ctxt/arg_matrix.rs` (`ArgMatrix::find_errors`) (MIT or Apache-2.0).

use std::cmp::{self, Ordering};

use blaze_ids::radix;
use either::Either;
use hir::{AsAssocItem as _, CallableKind, HirFileId, InFile, LangItem, PathResolution};
use hir_ty::next_solver::TyKind;
use intern::sym;
use rustc_type_ir::inherent::IntoKind as _;
use syntax::{
    TextRange,
    view::{
        self as ast, AstNode as _, HasArgList as _, HasGenericArgs as _, HasGenericParams as _, HasName as _,
        HasTypeBounds as _, Node,
    },
};

use super::binary_operator_cannot_be_applied::code_fact;
use stdx::list::Join;
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag, MultiSpan, Span},
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::MismatchedArgCount) -> Vec<Found> {
    let node = cx.node_view(d.call_expr);
    let inputs = Inputs::Count { expected: d.expected, is_fn_trait_call: d.is_fn_trait_call };
    let Some(call) = node.value.left().and_then(|it| CallCx::new(cx, node.file_id, it, inputs)) else {
        let span = cx.span_of_ptr(d.call_expr);
        let mut diag = Diag::error(
            Some(if d.is_fn_trait_call { "E0057" } else { "E0061" }),
            takes_message("this function", false, d.expected, d.found),
            span,
        );
        code_fact(cx, d.call_expr.map(|it| it.syntax_node_ptr()), &mut diag);
        // alloc: the error's parts, for an error hir found
        return vec![Found::new(diag, Phase::Bodies)];
    };
    let mut diag = call.report();
    code_fact(cx, d.call_expr.map(|it| it.syntax_node_ptr()), &mut diag);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// "this function takes 2 arguments but 1 argument was supplied".
fn takes_message(callee: &str, c_variadic: bool, expected: usize, found: usize) -> String {
    // alloc: the error's text, for an error hir found
    format!(
        "{callee} takes {}{} but {} {} supplied",
        if c_variadic { "at least " } else { "" },
        plural_count(expected, "argument"),
        plural_count(found, "argument"),
        if found == 1 { "was" } else { "were" },
    )
}

/// rustc's `potentially_plural_count`.
fn plural_count(count: usize, word: &str) -> String {
    // alloc: the error's text, for an error hir found
    format!("{count} {word}{}", if count == 1 { "" } else { "s" })
}

/// rustc's `Compatibility`: whether a provided argument can be passed for an expected input,
/// and when not, whether the types were compared (rustc's `Incompatible(Some(Sorts))`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Compat {
    Compatible,
    Incompatible { compared: bool },
}

/// rustc's `arg_matrix::Error`, of provided and expected indices.
#[derive(Debug, PartialEq, Eq)]
enum ArgError {
    Invalid(usize, usize, Compat),
    Missing(usize),
    Extra(usize),
    Swap(usize, usize, usize, usize),
    Permutation(Vec<(usize, usize)>),
}

impl Ord for ArgError {
    fn cmp(&self, other: &Self) -> Ordering {
        let key = |error: &ArgError| match error {
            ArgError::Invalid(..) => 0,
            ArgError::Extra(_) => 1,
            ArgError::Missing(_) => 2,
            ArgError::Swap(..) => 3,
            ArgError::Permutation(..) => 4,
        };
        match (self, other) {
            (ArgError::Invalid(a, ..), ArgError::Invalid(b, ..)) => a.cmp(b),
            (ArgError::Extra(a), ArgError::Extra(b)) => a.cmp(b),
            (ArgError::Missing(a), ArgError::Missing(b)) => a.cmp(b),
            (ArgError::Swap(a, b, ..), ArgError::Swap(c, d, ..)) => a.cmp(c).then(b.cmp(d)),
            (ArgError::Permutation(a), ArgError::Permutation(b)) => a.cmp(b),
            _ => key(self).cmp(&key(other)),
        }
    }
}

impl PartialOrd for ArgError {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// rustc's `arg_matrix::Issue`.
enum Issue {
    Invalid(usize),
    Missing(usize),
    Extra(usize),
    Swap(usize, usize),
    Permutation(Vec<Option<usize>>),
}

const _: () = assert!(size_of::<Compat>() == 1);

/// rustc's `ArgMatrix`: rows are the provided arguments left to match, columns the expected
/// inputs. One row-major buffer of 1-byte cells (rustc keeps a `Vec` per row); removing a row
/// or a column compacts it in place, O(rows × columns), with no allocation. Each cell is one
/// coercion query, made once per call and shared by every check that reads it.
#[derive(Clone)]
struct ArgMatrix {
    provided: Vec<usize>,
    expected: Vec<usize>,
    cells: Vec<Compat>,
}

impl ArgMatrix {
    fn new(provided: usize, expected: usize, mut compat: impl FnMut(usize, usize) -> Compat) -> ArgMatrix {
        let mut cells = Vec::with_capacity(provided * expected);
        for i in 0..provided {
            cells.extend((0..expected).map(|j| compat(i, j)));
        }
        // alloc: the error's parts, for an error hir found
        ArgMatrix { provided: (0..provided).collect(), expected: (0..expected).collect(), cells }
    }

    fn at(&self, provided: usize, expected: usize) -> Compat {
        self.cells[provided * self.expected.len() + expected]
    }

    fn compatible(&self, provided: usize, expected: usize) -> bool {
        self.at(provided, expected) == Compat::Compatible
    }

    fn eliminate_provided(&mut self, idx: usize) {
        let cols = self.expected.len();
        self.provided.remove(idx);
        self.cells.drain(idx * cols..(idx + 1) * cols);
    }

    fn eliminate_expected(&mut self, idx: usize) {
        let cols = self.expected.len();
        self.expected.remove(idx);
        let mut at = 0;
        self.cells.retain(|_| {
            let keep = at % cols != idx;
            at += 1;
            keep
        });
    }

    fn satisfy_input(&mut self, provided: usize, expected: usize) {
        self.eliminate_provided(provided);
        self.eliminate_expected(expected);
    }

    fn eliminate_satisfied(&mut self) -> Vec<(usize, usize)> {
        let num_args = cmp::min(self.provided.len(), self.expected.len());
        // alloc: the error's parts, for an error hir found
        let mut eliminated = vec![];
        for i in (0..num_args).rev() {
            if self.compatible(i, i) {
                eliminated.push((self.provided[i], self.expected[i]));
                self.satisfy_input(i, i);
            }
        }
        eliminated
    }

    fn find_issue(&self) -> Option<Issue> {
        let (rows, cols) = (self.provided.len(), self.expected.len());

        let mut next_unmatched_idx = 0;
        for i in 0..cmp::max(rows, cols) {
            if i >= rows {
                return Some(Issue::Missing(next_unmatched_idx));
            }
            if cols == 0 {
                return Some(Issue::Extra(next_unmatched_idx));
            }
            let is_arg = i < cols;
            let is_input = i < rows;
            if is_arg && is_input && self.compatible(i, i) {
                next_unmatched_idx += 1;
                continue;
            }
            let unsatisfiable = !is_arg || !(0..rows).any(|row| self.compatible(row, i));
            let useless = !is_input || !(0..cols).any(|col| self.compatible(i, col));
            match (is_input, is_arg, useless, unsatisfiable) {
                (true, true, true, true) => return Some(Issue::Invalid(i)),
                (true, _, true, _) => return Some(Issue::Extra(i)),
                (_, true, _, true) => return Some(Issue::Missing(i)),
                (true, true, _, _) => {
                    for j in 0..cmp::min(rows, cols) {
                        if i == j || self.compatible(j, j) {
                            continue;
                        }
                        if self.compatible(i, j) && self.compatible(j, i) {
                            return Some(Issue::Swap(i, j));
                        }
                    }
                }
                _ => continue,
            }
        }

        // alloc: the error's parts, for an error hir found
        let mut permutation: Vec<Option<Option<usize>>> = vec![None; rows];
        let mut permutation_found = false;
        let mut stack = Vec::with_capacity(rows);
        for i in 0..rows {
            if permutation[i].is_some() {
                continue;
            }
            stack.clear();
            let mut j = i;
            let mut last = i;
            let mut is_cycle = true;
            loop {
                stack.push(j);
                let Some(first) = (0..cols).find(|&col| self.compatible(j, col)) else {
                    is_cycle = false;
                    break;
                };
                j = first;
                if stack.contains(&j) {
                    last = j;
                    break;
                }
            }
            if stack.len() <= 2 {
                is_cycle = false;
            }
            permutation_found = is_cycle;
            while let Some(x) = stack.pop() {
                if is_cycle {
                    permutation[x] = Some(Some(j));
                    j = x;
                    if j == last {
                        is_cycle = false;
                    }
                } else {
                    permutation[x] = Some(None);
                }
            }
        }
        if permutation_found {
            // alloc: the error's parts, for an error hir found
            return Some(Issue::Permutation(permutation.into_iter().map(Option::flatten).collect()));
        }
        None
    }

    /// The errors, sorted, and for each expected input the provided argument matched to it.
    fn find_errors(mut self) -> (Vec<ArgError>, Vec<Option<usize>>) {
        let provided_count = self.provided.len();
        // alloc: the error's parts, for an error hir found
        let mut errors = vec![];
        // alloc: the error's parts, for an error hir found
        let mut matched: Vec<Option<usize>> = vec![None; self.expected.len()];
        for (provided, expected) in self.eliminate_satisfied() {
            matched[expected] = Some(provided);
        }
        while !self.provided.is_empty() || !self.expected.is_empty() {
            match self.find_issue() {
                Some(Issue::Invalid(idx)) => {
                    let compat = self.at(idx, idx);
                    let (input, arg) = (self.provided[idx], self.expected[idx]);
                    self.satisfy_input(idx, idx);
                    errors.push(ArgError::Invalid(input, arg, compat));
                }
                Some(Issue::Extra(idx)) => {
                    let input = self.provided[idx];
                    self.eliminate_provided(idx);
                    errors.push(ArgError::Extra(input));
                }
                Some(Issue::Missing(idx)) => {
                    let arg = self.expected[idx];
                    self.eliminate_expected(idx);
                    errors.push(ArgError::Missing(arg));
                }
                Some(Issue::Swap(idx, other)) => {
                    let (input, other_input) = (self.provided[idx], self.provided[other]);
                    let (arg, other_arg) = (self.expected[idx], self.expected[other]);
                    let (min, max) = (cmp::min(idx, other), cmp::max(idx, other));
                    self.satisfy_input(min, max);
                    self.satisfy_input(max - 1, min);
                    errors.push(ArgError::Swap(input, other_input, arg, other_arg));
                    matched[other_arg] = Some(input);
                    matched[arg] = Some(other_input);
                }
                Some(Issue::Permutation(args)) => {
                    // alloc: the error's parts, for an error hir found
                    let mut idxs: Vec<usize> = args.iter().filter_map(|&a| a).collect();
                    // alloc: the error's parts, for an error hir found
                    let mut real: Vec<Option<(usize, usize)>> = vec![None; provided_count];
                    for (src, dst) in args.iter().enumerate().filter_map(|(src, dst)| dst.map(|dst| (src, dst))) {
                        let src_input = self.provided[src];
                        let dst_input = self.provided[dst];
                        let dst_arg = self.expected[dst];
                        real[src_input] = Some((dst_arg, dst_input));
                        matched[dst_arg] = Some(src_input);
                    }
                    radix::sort(&mut idxs);
                    idxs.reverse();
                    for i in idxs {
                        self.satisfy_input(i, i);
                    }
                    // alloc: the error's parts, for an error hir found
                    errors.push(ArgError::Permutation(real.into_iter().flatten().collect()));
                }
                None => {
                    let eliminated = self.eliminate_satisfied();
                    assert!(!eliminated.is_empty(), "didn't eliminated any indice in this round");
                    for (input, arg) in eliminated {
                        matched[arg] = Some(input);
                    }
                }
            }
        }
        errors.sort();
        (errors, matched)
    }
}

/// rustc's `SuggestionText`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SuggestionText {
    None,
    Provide(bool),
    Remove(bool),
    Swap,
    Reorder,
    DidYouMean,
}

/// One parameter of the callee's definition: rustc's `FnParam` with the generic parameter that
/// is its whole type, if any (`get_hir_param_info`).
struct ParamInfo {
    generic: Option<usize>,
    span: Span,
    /// "`a`", or "parameter #2" when it binds no unique name.
    display: String,
}

/// The inputs a call's arguments are checked against, as the front end's diagnostic says.
#[derive(Clone, Copy)]
pub(super) enum Inputs {
    /// `MismatchedArgCount`: the number of inputs the callee takes, and whether the call goes
    /// through a `Fn` trait, its arguments made a tuple.
    Count { expected: usize, is_fn_trait_call: bool },
    /// `IncorrectArguments`: as many as are given, a C-variadic callee's extra arguments left
    /// out as rustc's `report_arg_errors` leaves them.
    Fit,
}

/// What rustc's `CallCtxt`, `ArgsCtxt` and `ArgMatchingCtxt` know of a call.
pub(super) struct CallCx<'a, 'b, 'db> {
    cx: &'a Cx<'b, 'db>,
    file: HirFileId,
    callee_expr: Option<ast::Expr<'db>>,
    method_name: Option<ast::NameRef<'db>>,
    callable: Option<hir::Callable<'db>>,
    is_method: bool,
    is_fn_trait_call: bool,
    call_name: &'static str,
    full_call_span: Span,
    error_span: Span,
    args_span: Span,
    /// The type of each expected input, when the callee's signature is known.
    expected: Vec<Option<hir::Type<'db>>>,
    provided: Vec<(Option<hir::Type<'db>>, Span)>,
    c_variadic: bool,
    code: &'static str,
}

impl<'a, 'b, 'db> CallCx<'a, 'b, 'db> {
    pub(super) fn new(
        cx: &'a Cx<'b, 'db>,
        file: HirFileId,
        call: ast::Expr<'db>,
        inputs: Inputs,
    ) -> Option<CallCx<'a, 'b, 'db>> {
        let at = |range: TextRange| cx.span(InFile::new(file, range));
        let call_range = call.syntax().text_range();
        let (callee_expr, method_name, callable, is_method, call_name, full_call, error, args) = match call {
            ast::Expr::CallExpr(it) => {
                let callee = it.expr()?;
                let call_name = match callee {
                    ast::Expr::PathExpr(path) => match path.path().and_then(|it| cx.vsema.resolve_path(it)) {
                        Some(PathResolution::Def(hir::ModuleDef::Adt(hir::Adt::Struct(_)))) => "struct",
                        Some(PathResolution::Def(hir::ModuleDef::EnumVariant(_))) => "enum variant",
                        _ => "function",
                    },
                    _ => "function",
                };
                let callable = cx.vsema.resolve_expr_as_callable(callee);
                let full = callee.syntax().text_range();
                (Some(callee), None, callable, false, call_name, full, call_range, it.arg_list())
            }
            ast::Expr::MethodCallExpr(it) => {
                let name = it.name_ref()?;
                let start = name.syntax().text_range().start();
                let end = it.generic_arg_list().map_or(name.syntax().text_range().end(), |it| it.syntax().text_range().end());
                let callable = cx.vsema.resolve_method_call_as_callable(it);
                let full = TextRange::new(start, end);
                let error = TextRange::new(start, call_range.end());
                (it.receiver(), Some(name), callable, true, "method", full, error, it.arg_list())
            }
            _ => return None,
        };
        let mut provided = args
            .into_iter()
            .flat_map(|it| it.args())
            .map(|arg| {
                let ty = cx.vsema.type_of_expr(arg).map(|it| it.adjusted());
                (ty, at(arg.syntax().text_range()))
            })
            // alloc: the error's parts, for an error hir found
            .collect::<Vec<_>>();
        let params: Vec<hir::Type<'db>> =
            // alloc: clone: the error's parts, for an error hir found
            callable.as_ref().map(|it| it.params().into_iter().map(|it| it.ty().clone()).collect()).unwrap_or_default();
        let c_variadic = match callable.as_ref().map(|it| it.kind()) {
            Some(CallableKind::Function(f)) => f.is_varargs(cx.db),
            _ => false,
        };
        let (expected_len, is_fn_trait_call) = match inputs {
            Inputs::Count { expected, is_fn_trait_call } => (expected, is_fn_trait_call),
            Inputs::Fit => {
                if c_variadic {
                    provided.truncate(params.len());
                }
                // Only a call of a value through the `Fn` traits has its arguments tupled
                // (`TupleArgumentsFlag::TupleArguments`), never a method call.
                let through_fn_trait = !is_method && matches!(callable.as_ref().map(|it| it.kind()), Some(CallableKind::FnImpl(_)));
                (provided.len(), through_fn_trait)
            }
        };
        let expected = if params.len() == expected_len {
            // alloc: the error's parts, for an error hir found
            params.into_iter().map(Some).collect()
        } else {
            // alloc: the error's parts, for an error hir found
            vec![None; expected_len]
        };
        let code = if c_variadic && provided.len() < expected_len {
            "E0060"
        } else if is_fn_trait_call {
            "E0057"
        } else {
            "E0061"
        };
        Some(CallCx {
            cx,
            file,
            callee_expr,
            method_name,
            callable,
            is_method,
            is_fn_trait_call,
            call_name,
            full_call_span: at(full_call),
            error_span: at(error),
            args_span: at(TextRange::new(full_call.end(), error.end())),
            expected,
            provided,
            c_variadic,
            code,
        })
    }

    fn at(&self, range: TextRange) -> Span {
        self.cx.span(InFile::new(self.file, range))
    }

    fn ty(&self, ty: &hir::Type<'db>) -> String {
        self.cx.ty(ty)
    }

    /// rustc's `has_error_or_infer`, of a type that might not be known.
    fn known<'t>(&self, ty: &'t Option<hir::Type<'db>>) -> Option<&'t hir::Type<'db>> {
        ty.as_ref().filter(|it| !it.contains_unknown())
    }

    fn coerces(&self, provided: &hir::Type<'db>, expected: &hir::Type<'db>) -> bool {
        provided.could_coerce_to(self.cx.db, expected)
    }

    /// rustc's `check_compatible`; on the diagonal, its `compatibility_diagonal`, where a type
    /// that is not known checks as compatible.
    fn compat(&self, provided: usize, expected: usize) -> Compat {
        match (self.known(&self.provided[provided].0), self.known(&self.expected[expected])) {
            (Some(p), Some(e)) => {
                if self.coerces(p, e) {
                    Compat::Compatible
                } else {
                    Compat::Incompatible { compared: true }
                }
            }
            _ if provided == expected => Compat::Compatible,
            _ => Compat::Incompatible { compared: false },
        }
    }

    /// rustc's special case of `report_arg_errors`: the call's one error is an argument of the
    /// wrong type, the types compared, which rustc reports as that argument's "mismatched types"
    /// rather than as the call's error.
    pub(super) fn single_mismatch(&self) -> bool {
        let full = ArgMatrix::new(self.provided.len(), self.expected.len(), |p, e| self.compat(p, e));
        let (errors, _) = full.find_errors();
        matches!(errors[..], [ArgError::Invalid(_, _, Compat::Incompatible { compared: true })])
    }

    pub(super) fn report(&self) -> Diag {
        let full = ArgMatrix::new(self.provided.len(), self.expected.len(), |p, e| self.compat(p, e));
        // clone: on the error path, for an error hir found
        let (mut errors, matched) = full.clone().find_errors();

        if let Some(diag) = self.wrap_args_in_tuple(&full, &matched) {
            return diag;
        }
        if errors.is_empty() {
            return Diag::error(
                None,
                "argument type mismatch was detected, but rustc had trouble determining where",
                // clone: on the error path, for an error hir found
                self.error_span.clone(),
            );
        }

        if let [ArgError::Extra(idx)] = errors[..]
            && !self.remove_idx_is_perfect(&full, idx)
            && let Some(i) = (0..self.provided.len()).find(|&i| self.remove_idx_is_perfect(&full, i))
        {
            // alloc: the error's parts, for an error hir found
            errors = vec![ArgError::Extra(i)];
        }

        let mut diag = if self.expected.len() == self.provided.len() {
            Diag::error(
                Some("E0308"),
                // alloc: the error's text, for an error hir found
                format!("arguments to this {} are incorrect", self.call_name),
                // clone: on the error path, for an error hir found
                self.full_call_span.clone(),
            )
        } else {
            Diag::error(
                Some(self.code),
                takes_message(
                    // alloc: the error's text, for an error hir found
                    &format!("this {}", self.call_name),
                    self.c_variadic,
                    self.expected.len(),
                    self.provided.len(),
                ),
                // clone: on the error path, for an error hir found
                self.full_call_span.clone(),
            )
        };

        let (mut suggestions, labels, text) = self.labels_and_suggestion_text(&errors, &mut diag);
        self.label_generic_mismatches(&mut diag, &matched);
        self.append_arguments_changes(&matched, &mut suggestions);
        if labels.len() <= 5 {
            for (span, label) in labels {
                diag.span_label(span, label);
            }
        }
        self.label_fn_like(&mut diag, None, &matched);

        let message = match text {
            SuggestionText::None => None,
            // alloc: the error's text, for an error hir found
            SuggestionText::Provide(plural) => Some(format!("provide the argument{}", if plural { "s" } else { "" })),
            SuggestionText::Remove(plural) => {
                diag.multipart_suggestion(
                    // alloc: the error's text, for an error hir found
                    format!("remove the extra argument{}", if plural { "s" } else { "" }),
                    suggestions,
                    Applicability::HasPlaceholders,
                );
                None
            }
            SuggestionText::Swap => Some("swap these arguments".to_owned()), // alloc: the error's text, for an error hir found
            SuggestionText::Reorder => Some("reorder these arguments".to_owned()), // alloc: the error's text, for an error hir found
            SuggestionText::DidYouMean => Some("did you mean".to_owned()), // alloc: the error's text, for an error hir found
        };
        if let Some(message) = message {
            let code = self.suggestion_code(&matched);
            // clone: on the error path, for an error hir found
            diag.suggestion_verbose(self.args_span.clone(), message, code, Applicability::HasPlaceholders);
        }
        diag
    }

    /// rustc's `check_wrap_args_in_tuple`: `f(1, 2)` of `fn f(_: (u8, u8))`.
    fn wrap_args_in_tuple(&self, full: &ArgMatrix, matched: &[Option<usize>]) -> Option<Diag> {
        let mismatch = (0..self.provided.len().min(self.expected.len()))
            .find(|&i| full.at(i, i) == Compat::Incompatible { compared: true })?;
        let tuple = self.expected[mismatch].as_ref().filter(|it| it.is_tuple())?;
        let fields = tuple.tuple_fields(self.cx.db);
        if fields.is_empty() || self.provided.len() != self.expected.len() - 1 + fields.len() {
            return None;
        }
        let (wrapped, after) = self.provided[mismatch..].split_at(fields.len());
        let wrapped_fit = wrapped
            .iter()
            .zip(&fields)
            .all(|((provided, _), field)| provided.as_ref().is_some_and(|it| it.could_unify_with(self.cx.db, field)));
        let after_fit = self.expected[mismatch + 1..].iter().zip(after).all(|(expected, (provided, _))| {
            match (provided, expected) {
                (Some(p), Some(e)) => self.coerces(p, e),
                _ => false,
            }
        });
        if !wrapped_fit || !after_fit || fields.len() == 1 {
            return None;
        }
        let (_, lo) = wrapped.first()?;
        let (_, hi) = wrapped.last()?;
        let mut diag = Diag::error(
            Some(self.code),
            takes_message(self.call_name, self.c_variadic, self.expected.len(), self.provided.len()),
            // clone: on the error path, for an error hir found
            self.full_call_span.clone(),
        );
        diag.multipart_suggestion(
            "wrap these arguments in parentheses to construct a tuple",
            // alloc: the error's parts, for an error hir found
            vec![(lo.shrink_to_lo(), "(".to_owned()), (hi.shrink_to_hi(), ")".to_owned())],
            Applicability::MachineApplicable,
        );
        self.label_fn_like(&mut diag, Some(mismatch), matched);
        Some(diag)
    }

    /// Whether every other argument coerces to the input it would move to: cells of `full`,
    /// where a pair with a type not known counts as not coercing.
    fn remove_idx_is_perfect(&self, full: &ArgMatrix, idx: usize) -> bool {
        let rest = (0..self.provided.len()).filter(|&j| j != idx);
        (0..self.expected.len()).zip(rest).all(|(e, p)| {
            full.compatible(p, e)
                && self.known(&self.provided[p].0).is_some()
                && self.known(&self.expected[e]).is_some()
        })
    }

    fn labels_and_suggestion_text(
        &self,
        errors: &[ArgError],
        diag: &mut Diag,
    ) -> (Vec<(Span, String)>, Vec<(Span, String)>, SuggestionText) {
        // alloc: the error's parts, for an error hir found
        let mut labels = Vec::new();
        let mut text = SuggestionText::None;
        // alloc: the error's parts, for an error hir found
        let mut suggestions = vec![];
        let mut errors = errors.iter().peekable();
        let mut only_extras_so_far = errors.peek().is_some_and(|first| matches!(first, ArgError::Extra(0)));
        let mut prev_extra_idx: Option<usize> = None;
        while let Some(error) = errors.next() {
            only_extras_so_far &= matches!(error, ArgError::Extra(_));
            match error {
                ArgError::Invalid(provided, expected, Compat::Incompatible { compared: true }) => {
                    if let (Some(p), Some(e)) = (&self.provided[*provided].0, &self.expected[*expected]) {
                        let (p, e) = (self.ty(p), self.ty(e));
                        // alloc: clone: the error's parts, for an error hir found
                        diag.span_label(self.provided[*provided].1.clone(), format!("expected `{e}`, found `{p}`"));
                    }
                }
                ArgError::Invalid(..) => {}
                ArgError::Extra(idx) => {
                    let (provided_ty, provided_span) = &self.provided[*idx];
                    let ty_name =
                        // alloc: the error's text, for an error hir found
                        self.known(provided_ty).map_or_else(String::new, |it| format!(" of type `{}`", self.ty(it)));
                    // alloc: the error's text, for an error hir found
                    let number = if self.provided.len() == 1 { String::new() } else { format!(" #{}", idx + 1) };
                    // alloc: clone: the error's parts, for an error hir found
                    labels.push((provided_span.clone(), format!("unexpected argument{number}{ty_name}")));
                    if self.provided.len() == 1
                        && let Some(span) = self.expect_for_unwrap(provided_ty)
                    {
                        diag.suggestion_verbose(span, "did you mean to use `expect`?", "expect", Applicability::MaybeIncorrect);
                        continue;
                    }
                    // clone: on the error path, for an error hir found
                    let mut span = provided_span.clone();
                    if *idx > 0
                        && let Some((_, prev)) = self.provided.get(idx - 1)
                    {
                        span = prev.shrink_to_hi().to(&span);
                    }
                    let trim_next_comma = match errors.peek() {
                        Some(ArgError::Extra(next)) if only_extras_so_far && *next > idx + 1 => {
                            prev_extra_idx.is_none_or(|prev| prev + 1 == *idx)
                        }
                        None if only_extras_so_far => true,
                        _ => false,
                    };
                    if trim_next_comma {
                        let next_lo = match self.provided.get(idx + 1) {
                            Some((_, next)) => next.lo,
                            None => self.error_span.hi.saturating_sub(1),
                        };
                        span = span.with_hi(next_lo);
                    }
                    // alloc: empty, allocates nothing
                    suggestions.push((span, String::new()));
                    text = match text {
                        SuggestionText::None => SuggestionText::Remove(false),
                        SuggestionText::Remove(_) => SuggestionText::Remove(true),
                        _ => SuggestionText::DidYouMean,
                    };
                    prev_extra_idx = Some(*idx);
                }
                ArgError::Missing(first) => {
                    // alloc: the error's parts, for an error hir found
                    let mut missing = vec![*first];
                    while let Some(ArgError::Missing(next)) =
                        errors.next_if(|e| matches!(e, ArgError::Missing(next) if *next == missing.last().unwrap() + 1))
                    {
                        missing.push(*next);
                    }
                    let span_of = |first: usize, last: usize| match (self.provided.get(first), self.provided.get(last)) {
                        (Some((_, first)), Some((_, last))) => first.to(last),
                        _ => self.args_span.clone(), // clone: on the error path, for an error hir found
                    };
                    let tys = |idxs: &[usize]| -> Option<Vec<String>> {
                        // alloc: the error's parts, for an error hir found
                        idxs.iter().map(|&i| self.known(&self.expected[i]).map(|it| self.ty(it))).collect()
                    };
                    match missing[..] {
                        [idx] => {
                            // clone: on the error path, for an error hir found
                            let span = self.provided.get(idx).map_or_else(|| self.args_span.clone(), |(_, it)| it.clone());
                            // alloc: the error's text, for an error hir found
                            let rendered = tys(&[idx]).map_or_else(String::new, |it| format!(" of type `{}`", it[0]));
                            // alloc: the error's text, for an error hir found
                            labels.push((span, format!("argument #{}{rendered} is missing", idx + 1)));
                            text = match text {
                                SuggestionText::None => SuggestionText::Provide(false),
                                SuggestionText::Provide(_) => SuggestionText::Provide(true),
                                _ => SuggestionText::DidYouMean,
                            };
                        }
                        [a, b] => {
                            let rendered =
                                // alloc: the error's text, for an error hir found
                                tys(&[a, b]).map_or_else(String::new, |it| format!(" of type `{}` and `{}`", it[0], it[1]));
                            // alloc: the error's text, for an error hir found
                            labels.push((span_of(a, b), format!("two arguments{rendered} are missing")));
                            text = provide_plural(text);
                        }
                        [a, b, c] => {
                            // alloc: the error's text, for an error hir found
                            let rendered = tys(&[a, b, c]).map_or_else(String::new, |it| {
                                // alloc: the error's text, for an error hir found
                                format!(" of type `{}`, `{}`, and `{}`", it[0], it[1], it[2])
                            });
                            // alloc: the error's text, for an error hir found
                            labels.push((span_of(a, c), format!("three arguments{rendered} are missing")));
                            text = provide_plural(text);
                        }
                        _ => {
                            let span = span_of(missing[0], missing[missing.len() - 1]);
                            // alloc: the error's text, for an error hir found
                            labels.push((span, "multiple arguments are missing".to_owned()));
                            text = provide_plural(text);
                        }
                    }
                }
                ArgError::Swap(first_provided, second_provided, first_expected, second_expected) => {
                    for (provided, expected) in [(first_provided, first_expected), (second_provided, second_expected)] {
                        labels.push(self.expected_found(*provided, *expected));
                    }
                    text = match text {
                        SuggestionText::None => SuggestionText::Swap,
                        _ => SuggestionText::DidYouMean,
                    };
                }
                ArgError::Permutation(args) => {
                    for (expected, provided) in args {
                        labels.push(self.expected_found(*provided, *expected));
                    }
                    text = match text {
                        SuggestionText::None => SuggestionText::Reorder,
                        _ => SuggestionText::DidYouMean,
                    };
                }
            }
        }
        (suggestions, labels, text)
    }

    /// rustc's `label_generic_mismatches`: on each argument that fixes a generic parameter the
    /// parameters left without an argument also have.
    fn label_generic_mismatches(&self, diag: &mut Diag, matched: &[Option<usize>]) {
        let Some(CallableKind::Function(function)) = self.callable.as_ref().map(|it| it.kind()) else { return };
        let Some(source) = self.cx.vsema.source(function) else { return };
        let Some((params, _)) = self.param_info(source.file_id, source.value) else { return };
        for (idx, param) in params.iter().enumerate() {
            if matched.get(idx).copied().flatten().is_none() {
                continue;
            }
            let Some((_, arg_span)) = self.provided.get(idx) else { continue };
            let Some(generic) = param.generic else { continue };
            let others = params
                .iter()
                .enumerate()
                .filter(|&(other_idx, other)| {
                    other_idx != idx
                        && other.generic == Some(generic)
                        && matched.get(other_idx).copied().flatten().is_none()
                })
                .count();
            if others == 0 {
                continue;
            }
            let sort =
                // alloc: the error's text, for an error hir found
                self.expected[idx].as_ref().map_or_else(|| "_".to_owned(), |it| crate::print::sort_string(self.cx, it));
            let label = if others == params.len() - 1 {
                // alloc: the error's text, for an error hir found
                format!(
                    "expected all arguments to be this {sort} type because they need to match the type of this parameter"
                )
            } else {
                // alloc: the error's text, for an error hir found
                format!(
                    "expected some other arguments to be {} {sort} type to match the type of this parameter",
                    a_or_an(&sort)
                )
            };
            // clone: on the error path, for an error hir found
            diag.span_label(arg_span.clone(), label);
        }
    }

    /// "expected `u32`, found `&str`" on a provided argument that belongs elsewhere.
    fn expected_found(&self, provided: usize, expected: usize) -> (Span, String) {
        let (provided_ty, span) = &self.provided[provided];
        // alloc: the error's text, for an error hir found
        let expected = self.expected[expected].as_ref().map_or_else(|| "_".to_owned(), |it| self.ty(it));
        // alloc: the error's text, for an error hir found
        let found = self.known(provided_ty).map_or_else(String::new, |it| format!(", found `{}`", self.ty(it)));
        // alloc: clone: the error's parts, for an error hir found
        (span.clone(), format!("expected `{expected}`{found}"))
    }

    /// rustc's `maybe_suggest_expect_for_unwrap`: `x.unwrap("msg")` of an `Option` or a `Result`.
    fn expect_for_unwrap(&self, provided: &Option<hir::Type<'db>>) -> Option<Span> {
        let name = self.method_name?;
        if name.text() != "unwrap" {
            return None;
        }
        let receiver = self.cx.vsema.type_of_expr(self.callee_expr?)?.adjusted().strip_references();
        let hir::Adt::Enum(adt) = receiver.as_adt()? else { return None };
        let lang = hir_def::lang_item::lang_items(self.cx.db, self.cx.krate.base());
        let is_option = lang.Option.is_some_and(|it| hir::Enum::from(it) == adt);
        let is_result = || {
            let core = lang.Sized.map(|it| hir::Trait::from(it).module(self.cx.db).krate(self.cx.db));
            core == Some(adt.module(self.cx.db).krate(self.cx.db)) && *adt.name(self.cx.db).symbol() == sym::Result
        };
        let str_ref = hir::BuiltinType::str().ty(self.cx.db).add_reference(self.cx.db, hir::Mutability::Shared);
        ((is_option || is_result()) && self.known(provided).is_some_and(|it| self.coerces(it, &str_ref)))
            .then(|| self.at(name.syntax().text_range()))
    }

    /// rustc's `append_arguments_changes`.
    fn append_arguments_changes(&self, matched: &[Option<usize>], suggestions: &mut Vec<(Span, String)>) {
        let mut prev: i64 = -1;
        for (expected, provided) in matched.iter().enumerate() {
            if let Some(provided) = provided {
                prev = *provided as i64;
                continue;
            }
            let idx = (prev + 1) as usize;
            if let Some((_, arg_span)) = self.provided.get(idx) {
                prev += 1;
                let dominated = suggestions.iter().any(|(span, _)| {
                    (span.lo <= arg_span.lo && arg_span.hi <= span.hi) || (arg_span.lo < span.hi && span.lo < arg_span.hi)
                });
                if !dominated {
                    // clone: on the error path, for an error hir found
                    suggestions.push((arg_span.clone(), self.ty_to_snippet(expected)));
                }
            }
        }
    }

    /// rustc's `ty_to_snippet`: the placeholder for an argument to provide.
    fn ty_to_snippet(&self, expected: usize) -> String {
        let ty = &self.expected[expected];
        if let Some(ty) = ty
            && ty.is_unit()
        {
            // alloc: the error's text, for an error hir found
            return "()".to_owned();
        }
        if let Some(ty) = self.known(ty)
            && !matches!(ty.to_next_solver().kind(), TyKind::FnDef(..) | TyKind::Closure(..))
        {
            // alloc: the error's text, for an error hir found
            return format!("/* {} */", self.ty(ty));
        }
        match self.param_names().get(expected) {
            Some(Some(name)) if name != "self" => format!("/* {name} */"), // alloc: the error's text, for an error hir found
            _ => "/* value */".to_owned(), // alloc: the error's text, for an error hir found
        }
    }

    /// The names of the callee's parameters, rustc's `fn_arg_idents` past the receiver of a
    /// method call.
    fn param_names(&self) -> Vec<Option<String>> {
        // alloc: empty, allocates nothing
        let Some(callable) = &self.callable else { return Vec::new() };
        let names: Vec<Option<String>> = match callable.kind() {
            CallableKind::Function(_) | CallableKind::Closure(_) => callable
                .params()
                .into_iter()
                .map(|param| {
                    let source = self.cx.vsema.source(param)?;
                    match source.value {
                        Either::Left(_) => Some("self".to_owned()), // alloc: the error's text, for an error hir found
                        Either::Right(param) => match param.pat()? {
                            // alloc: the error's text, for an error hir found
                            ast::Pat::IdentPat(it) => Some(it.name()?.text().to_string()),
                            _ => None,
                        },
                    }
                })
                // alloc: the error's parts, for an error hir found
                .collect(),
            _ => Vec::new(), // alloc: empty, allocates nothing
        };
        names
    }

    /// rustc's `suggestion_code`: the whole argument list as it should be.
    fn suggestion_code(&self, matched: &[Option<usize>]) -> String {
        let multiline = self.arguments_formatting(matched);
        // alloc: the error's text, for an error hir found
        let mut code = "(".to_owned();
        let mut needs_comma = false;
        for (expected, provided) in matched.iter().enumerate() {
            if needs_comma {
                code += ",";
            }
            match &multiline {
                None if needs_comma => code += " ",
                None => {}
                Some(_) => code += "\n",
            }
            needs_comma = true;
            let (span, text) = match provided.and_then(|it| {
                let span = &self.provided[it].1;
                // alloc: clone: the error's parts, for an error hir found
                Some((span.clone(), self.cx.snippet(span)?.to_owned()))
            }) {
                Some((span, text)) => (Some(span), text),
                None => (None, self.ty_to_snippet(expected)),
            };
            if let Some((fallback, _)) = &multiline {
                // clone: on the error path, for an error hir found
                code += &span.and_then(|it| self.indentation_before(&it)).unwrap_or_else(|| fallback.clone());
            }
            code += &text;
        }
        if let Some((_, brace)) = multiline {
            code += ",\n";
            code += &brace;
        }
        code += ")";
        code
    }

    /// rustc's `arguments_formatting`: the indentation of each argument and of the closing
    /// parenthesis, when the arguments kept are on more than one line.
    fn arguments_formatting(&self, matched: &[Option<usize>]) -> Option<(String, String)> {
        let brace = self.indentation_before(&self.args_span)?;
        let mut kept = matched.iter().filter_map(|it| *it);
        let first = &self.provided[kept.next()?].1;
        let last = &self.provided[kept.next()?].1;
        let between = first.to(last);
        if !self.cx.snippet(&between).is_some_and(|it| it.contains('\n')) {
            return None;
        }
        Some((self.indentation_before(first)?, brace))
    }

    /// rustc's `SourceMap::indentation_before`.
    fn indentation_before(&self, span: &Span) -> Option<String> {
        let file = self.cx.file_of(span)?;
        let text = self.cx.file_text(file);
        let line = text.get(..span.lo as usize)?.lines().last()?;
        // alloc: the error's text, for an error hir found
        Some(line.chars().take_while(|it| it.is_whitespace()).collect())
    }

    /// rustc's `label_fn_like`: where the callee is defined, with its parameters left without an
    /// argument, and how their generic types tie them to the others.
    fn label_fn_like(&self, diag: &mut Diag, expected_idx: Option<usize>, matched: &[Option<usize>]) {
        let Some(callable) = &self.callable else { return };
        match callable.kind() {
            CallableKind::Function(function) => self.label_function(diag, function, expected_idx, matched),
            CallableKind::TupleStruct(it) => {
                if let Some(source) = self.cx.vsema.source(it)
                    && let Some(name) = source.value.name()
                {
                    diag.span_note(self.cx.span(source.with_value(name.syntax().text_range())), "tuple struct defined here");
                }
            }
            CallableKind::TupleEnumVariant(it) => {
                if let Some(source) = self.cx.vsema.source(it)
                    && let Some(name) = source.value.name()
                {
                    diag.span_note(self.cx.span(source.with_value(name.syntax().text_range())), "tuple variant defined here");
                }
            }
            CallableKind::Closure(_) => {
                let param = expected_idx.and_then(|it| callable.params().into_iter().nth(it)).and_then(|it| self.cx.vsema.source(it));
                if let Some(param) = param {
                    diag.span_note(self.cx.span(param.map(|it| it.syntax().text_range())), "closure parameter defined here");
                } else if let Some(span) = self.closure_span(callable) {
                    diag.span_note(span, "closure defined here");
                }
            }
            CallableKind::FnImpl(_) if self.is_fn_trait_call => {
                if let Some(span) = self.callable_bound() {
                    diag.span_note(span, "callable defined here");
                }
            }
            CallableKind::FnImpl(_) | CallableKind::FnPtr => {}
        }
    }

    fn label_function(&self, diag: &mut Diag, function: hir::Function, expected_idx: Option<usize>, matched: &[Option<usize>]) {
        let db = self.cx.db;
        let Some(source) = self.cx.vsema.source(function) else { return };
        let Some(name) = source.value.name() else { return };
        let mut spans = MultiSpan::of(self.cx.span(source.with_value(name.syntax().text_range())));
        if let Some((params, generics)) = self.param_info(source.file_id, source.value)
            && params.len() == matched.len()
        {
            self.label_params(&mut spans, &params, &generics, expected_idx, matched);
        }
        let descr = match function.as_assoc_item(db) {
            Some(_) if function.has_self_param(db) => "method",
            Some(_) => "associated function",
            None => "function",
        };
        // alloc: the error's text, for an error hir found
        diag.span_note(spans, format!("{descr} defined here"));
    }

    /// rustc's `get_hir_param_info`: the parameters past the receiver of a method call, each with
    /// the position among the function's generic parameters of the one that is its whole type.
    fn param_info(&self, file: HirFileId, function: ast::Fn<'_>) -> Option<(Vec<ParamInfo>, Vec<(Span, String)>)> {
        let at = |node: Node<'_>| self.cx.span(InFile::new(file, node.text_range()));
        let generics = || function.generic_param_list().into_iter().flat_map(|it| it.generic_params());
        let generic_names: Vec<(Span, String)> = generics()
            .map(|it| {
                let name = match it {
                    // alloc: the error's text, for an error hir found
                    ast::GenericParam::TypeParam(it) => it.name().map(|it| it.text().to_string()),
                    // alloc: the error's text, for an error hir found
                    ast::GenericParam::ConstParam(it) => it.name().map(|it| it.text().to_string()),
                    // alloc: the error's text, for an error hir found
                    ast::GenericParam::LifetimeParam(it) => it.lifetime().map(|it| it.text().to_string()),
                };
                (at(it.syntax()), name.unwrap_or_default())
            })
            // alloc: the error's parts, for an error hir found
            .collect();
        let type_param = |ty: Option<ast::Type<'_>>| -> Option<usize> {
            let ast::Type::PathType(ty) = ty? else { return None };
            let path = ty.path()?;
            if path.qualifier().is_some() {
                return None;
            }
            let segment = path.segment()?;
            if segment.generic_arg_list().is_some() {
                return None;
            }
            let name = segment.name_ref()?.text();
            generics().position(|it| matches!(it, ast::GenericParam::TypeParam(p) if p.name().is_some_and(|n| n.text() == name)))
        };
        let has_body = function.body().is_some();
        let list = function.param_list()?;
        // alloc: the error's parts, for an error hir found
        let mut params = Vec::new();
        if !self.is_method
            && let Some(self_param) = list.self_param()
        {
            // alloc: the error's text, for an error hir found
            params.push(ParamInfo { generic: None, span: at(self_param.syntax()), display: "`self`".to_owned() });
        }
        for param in list.params().filter(|it| it.dotdotdot_token().is_none()) {
            let idx = params.len();
            let binding = match param.pat() {
                Some(ast::Pat::IdentPat(it)) => it.name(),
                _ => None,
            };
            let span = match (binding, has_body) {
                (_, true) | (None, false) => at(param.syntax()),
                (Some(name), false) => at(name.syntax()),
            };
            let display = match binding {
                Some(name) if name.text() != "_" => format!("`{}`", name.text()), // alloc: the error's text, for an error hir found
                _ => format!("parameter #{}", idx + 1), // alloc: the error's text, for an error hir found
            };
            params.push(ParamInfo { generic: type_param(param.ty()), span, display });
        }
        Some((params, generic_names))
    }

    /// The labels of `label_fn_like` on the parameters.
    fn label_params(
        &self,
        spans: &mut MultiSpan,
        params: &[ParamInfo],
        generics: &[(Span, String)],
        expected_idx: Option<usize>,
        matched: &[Option<usize>],
    ) {
        struct Mismatched {
            idx: usize,
            generic: usize,
            deps: Vec<usize>,
        }
        let mut mismatched = Vec::<Mismatched>::new();
        match expected_idx {
            Some(idx) => match params.get(idx) {
                Some(ParamInfo { generic: Some(generic), .. }) => {
                    // alloc: empty, allocates nothing
                    mismatched.push(Mismatched { idx, generic: *generic, deps: Vec::new() })
                }
                // alloc: clone: the error's parts, for an error hir found
                Some(param) => spans.labels.push((param.span.clone(), String::new())),
                None => {}
            },
            None => {
                for (idx, (param, matched)) in params.iter().zip(matched).enumerate() {
                    if matched.is_some() {
                        continue;
                    }
                    match param.generic {
                        // alloc: empty, allocates nothing
                        Some(generic) => mismatched.push(Mismatched { idx, generic, deps: Vec::new() }),
                        // alloc: clone: the error's parts, for an error hir found
                        None => spans.labels.push((param.span.clone(), String::new())),
                    }
                }
            }
        }
        if mismatched.is_empty() {
            return;
        }
        // alloc: the error's parts, for an error hir found
        let mut dependants: Vec<Vec<usize>> = vec![Vec::new(); params.len()];
        // alloc: the error's parts, for an error hir found
        let mut generic_uses: Vec<Vec<usize>> = vec![Vec::new(); generics.len()];
        for (i, param) in mismatched.iter_mut().enumerate() {
            for (other_idx, (other, other_matched)) in params.iter().zip(matched).enumerate() {
                if other.generic == Some(param.generic) && other_matched.is_some() {
                    generic_uses[param.generic].extend([param.idx, other_idx]);
                    dependants[other_idx].push(i);
                    param.deps.push(other_idx);
                }
            }
        }
        let sort = |idx: usize| {
            // alloc: the error's text, for an error hir found
            self.expected[idx].as_ref().map_or_else(|| "_".to_owned(), |it| crate::print::sort_string(self.cx, it))
        };
        for param in &mismatched {
            // clone: on the error path, for an error hir found
            let span = params[param.idx].span.clone();
            let deps = param.deps.iter().map(|&dep| &params[dep].display);
            match (!param.deps.is_empty()).then(|| stdx::list::list(deps, Join::AndNoSerialComma, false)) {
                Some(deps) => spans
                    .labels
                    // alloc: the error's text, for an error hir found
                    .push((span, format!("this parameter needs to match the {} type of {deps}", sort(param.deps[0])))),
                None => spans.labels.push((span, String::new())), // alloc: empty, allocates nothing
            }
        }
        for (idx, (param, deps)) in params.iter().zip(&dependants).enumerate() {
            if !deps.is_empty() {
                let list = stdx::list::list(deps.iter().map(|&dep| &params[mismatched[dep].idx].display), Join::AndNoSerialComma, false);
                let s = if deps.len() != 1 { "s" } else { "" };
                // alloc: clone: the error's parts, for an error hir found
                spans.labels.push((param.span.clone(), format!("{list} need{s} to match the {} type of this parameter", sort(idx))));
            }
        }
        for ((span, name), uses) in generics.iter().zip(&mut generic_uses) {
            radix::sort(uses);
            uses.dedup();
            if !uses.is_empty() {
                let list = stdx::list::list(uses.iter().map(|&idx| &params[idx].display), Join::AndNoSerialComma, false);
                let quantifier = if uses.len() == 2 { "both" } else { "all" };
                // alloc: clone: the error's parts, for an error hir found
                spans.labels.push((span.clone(), format!("{list} {quantifier} reference this parameter `{name}`")));
            }
        }
    }

    /// rustc's `def_span` of a closure: from its start to the end of its parameters or return
    /// type.
    fn closure_span(&self, callable: &hir::Callable<'db>) -> Option<Span> {
        let closure = callable
            .params()
            .into_iter()
            .next()
            .and_then(|it| self.cx.vsema.source(it))
            .and_then(|it| {
                let closure = it.value.syntax().ancestors().find_map(ast::ClosureExpr::cast)?;
                Some(InFile::new(it.file_id, closure))
            })
            .or_else(|| self.callee_closure())?;
        let value = closure.value;
        let start = crate::cx::item_range_view(value.syntax()).start();
        let end = value
            .ret_type()
            .map(|it| it.syntax().text_range().end())
            .or_else(|| value.param_list().map(|it| it.syntax().text_range().end()))?;
        Some(self.cx.span(closure.with_value(TextRange::new(start, end))))
    }

    /// The closure a callee names: written in place, or the initializer of the local it is.
    fn callee_closure(&self) -> Option<InFile<ast::ClosureExpr<'db>>> {
        let mut callee = self.callee_expr?;
        while let ast::Expr::ParenExpr(it) = callee {
            callee = it.expr()?;
        }
        match callee {
            ast::Expr::ClosureExpr(it) => Some(InFile::new(self.file, it)),
            ast::Expr::PathExpr(path) => {
                let Some(PathResolution::Local(local)) = self.cx.vsema.resolve_path(path.path()?) else { return None };
                let source = local.primary_source_view(self.cx.db);
                let pat = source.source.value.left()?;
                let stmt = pat.syntax().parent().and_then(ast::LetStmt::cast)?;
                match stmt.initializer()? {
                    ast::Expr::ClosureExpr(it) => Some(InFile::new(source.source.file_id, it)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// The `Fn` bound that makes a callee of a type parameter callable: rustc's "callable
    /// defined here" on the clause of the enclosing item.
    fn callable_bound(&self) -> Option<Span> {
        let callee_ty = self.cx.vsema.type_of_expr(self.callee_expr?)?.adjusted().strip_references();
        let param = callee_ty.as_type_param(self.cx.db)?;
        let source = self.cx.vsema.source(param.merge())?;
        let Either::Left(ast::TypeOrConstParam::Type(type_param)) = source.value else { return None };
        let type_param = source.with_value(type_param);
        let name = type_param.value.name()?.text();
        let fn_traits: Vec<hir::Trait> = [LangItem::Fn, LangItem::FnMut, LangItem::FnOnce]
            .into_iter()
            .filter_map(|it| hir::Trait::lang(self.cx.db, self.cx.krate, it))
            // alloc: the error's parts, for an error hir found
            .collect();
        let is_fn_bound = |bound: &ast::TypeBound<'db>| {
            let Some(ast::Type::PathType(ty)) = bound.ty() else { return false };
            matches!(
                ty.path().and_then(|it| self.cx.vsema.resolve_path(it)),
                Some(PathResolution::Def(hir::ModuleDef::Trait(it))) if fn_traits.contains(&it)
            )
        };
        let inline = type_param.value.type_bound_list().into_iter().flat_map(|it| it.bounds());
        let owner = type_param.value.syntax().ancestors().find_map(ast::AnyHasGenericParams::cast)?;
        let where_bounds = owner
            .where_clause()
            .into_iter()
            .flat_map(|it| it.predicates())
            .filter(|pred| pred.ty().is_some_and(|ty| ty.syntax().text() == name))
            .flat_map(|pred| pred.type_bound_list().into_iter().flat_map(|it| it.bounds()));
        let bound = inline.chain(where_bounds).find(is_fn_bound)?;
        Some(self.cx.span(type_param.with_value(bound.syntax().text_range())))
    }
}

fn provide_plural(text: SuggestionText) -> SuggestionText {
    match text {
        SuggestionText::None | SuggestionText::Provide(_) => SuggestionText::Provide(true),
        _ => SuggestionText::DidYouMean,
    }
}

/// rustc's `a_or_an`, of a word that may start with a backtick.
fn a_or_an(s: &str) -> &'static str {
    let mut chars = s.chars();
    let Some(mut first) = chars.next() else { return "a" };
    if first == '`' {
        let Some(next) = chars.next() else { return "a" };
        first = next;
    }
    // alloc: the error's text, for an error hir found
    if ["a", "e", "i", "o", "u", "&"].contains(&first.to_lowercase().to_string().as_str()) { "an" } else { "a" }
}

