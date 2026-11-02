//! `AsmLabels`: `named_asm_labels` (a label with a name, or one a format argument makes, in an
//! `asm!` or the `naked_asm!` of a generic function) and `binary_asm_labels` (a label of only
//! `0`s and `1`s, on x86 without `options(att_syntax)`), found in each template string on its
//! own, as rustc's `InlineAsm::template_strs` keeps it: its string, the source text of its span,
//! its span.
//!
//! Follows rustc's `compiler/rustc_lint/src/builtin.rs` (`AsmLabels`,
//! `is_hexagon_register_span`, `is_hexagon_register_span_context`,
//! `is_hexagon_register_span_impl`), `compiler/rustc_lint/src/lints.rs` (`InvalidAsmLabel`),
//! `compiler/rustc_builtin_macros/src/asm.rs` (`expand_preparsed_asm`: `template_strs`),
//! `compiler/rustc_builtin_macros/src/util.rs` (`expr_to_spanned_string`),
//! `compiler/rustc_middle/src/ty/generics.rs` (`requires_monomorphization`) and
//! `compiler/rustc_target/src/asm/mod.rs` (`InlineAsmArch::from_arch`) (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use hir::InFile;
use hir_def::{
    DefWithBodyId, ModuleDefId,
    expr_store::ExpressionStoreDiagnostics,
    hir::{AsmOptions, Expr, ExprId, InlineAsmKind, generics::GenericParamDataRef},
};
use smallvec::SmallVec;
use syntax::{
    SyntaxNodePtr, TextRange,
    view::{self as ast, AstNode},
};

use crate::{
    cx::Cx,
    lints::bodies::{BodyCx, ExprKinds, ExprKind, LatePass, LintSet, lint_set},
    lower::{Found, Phase},
    model::{Diag, Span},
};

#[derive(Default)]
pub(crate) struct AsmLabels {
    /// The crate's asm arch, once an `asm!` asked for it.
    arch: Option<Option<Arch>>,
}

/// rustc's `sess.asm_arch` (`InlineAsmArch::from_arch` of the target's `arch`), as far as these
/// lints tell arches apart; `None` for an arch rustc has no inline assembly for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arch {
    /// `x86` or `x86_64`.
    X86,
    Hexagon,
    Other,
}

/// The asm arch of the crate's target: its `target_arch`, which rustc sets from the target's
/// `arch` (and refuses on the command line).
fn asm_arch(cx: &Cx<'_, '_>) -> Option<Arch> {
    let arch = cx.krate.cfg(cx.db).get_cfg_values("target_arch").next()?;
    match arch.as_str() {
        "x86" | "x86_64" => Some(Arch::X86),
        "hexagon" => Some(Arch::Hexagon),
        "arm" | "arm64ec" | "aarch64" | "amdgpu" | "riscv32" | "riscv64" | "nvptx64"
        | "loongarch32" | "loongarch64" | "mips" | "mips32r6" | "mips64" | "mips64r6"
        | "powerpc" | "powerpc64" | "s390x" | "sparc" | "sparc64" | "spirv" | "wasm32"
        | "wasm64" | "bpf" | "avr" | "msp430" | "m68k" | "csky" | "xtensa" => Some(Arch::Other),
        _ => None,
    }
}

impl LatePass for AsmLabels {
    const LINTS: LintSet = lint_set(["named_asm_labels", "binary_asm_labels"]);
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::InlineAsm]);

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let Expr::InlineAsm(asm) = b.store().expr(expr) else { return };
        let naked = match asm.kind {
            InlineAsmKind::Asm => false,
            InlineAsmKind::NakedAsm => true,
            InlineAsmKind::GlobalAsm => return,
        };
        if naked && !requires_monomorphization(b) {
            return;
        }
        let Some(at) = b.expr_node(expr) else { return };
        let Some(templates) = templates(b, at) else { return };
        let arch = *self.arch.get_or_insert_with(|| asm_arch(b.cx));
        let raw = asm.options.contains(AsmOptions::RAW);
        let binary = !asm.options.contains(AsmOptions::ATT_SYNTAX) && matches!(arch, Some(Arch::X86) | None);
        for template in &templates {
            for (label, kind) in labels(&template.value, raw, arch == Some(Arch::Hexagon)) {
                if kind == Label::Binary && !binary {
                    continue;
                }
                let found = template.snippet.and_then(|it| find_label_span(it, label));
                let span = match found {
                    Some((lo, hi)) => template.span.with_lo(template.span.lo + lo).with_hi(template.span.lo + hi),
                    // clone: a span, for the lint's diagnostic
                    None => template.span.clone(),
                };
                let mut diag = invalid_asm_label(kind, span, found.is_none());
                if naked {
                    generics_fact(b, &mut diag);
                }
                out.push(b.lint(diag, Phase::Late, at));
            }
        }
    }
}

/// A template string as rustc's `template_strs` keeps it.
struct Template<'db> {
    /// The string, its escapes read.
    value: String,
    span: Span,
    /// The source text of `span`, quotes included (`span_to_snippet`).
    snippet: Option<&'db str>,
}

/// The template strings of the `asm!` written as `at`, those `cfg` removes left out; `None`
/// when one is not a string, which rustc refuses before there is an `asm!` to lint.
fn templates<'db>(b: &BodyCx<'_, '_, 'db>, at: InFile<SyntaxNodePtr>) -> Option<SmallVec<[Template<'db>; 2]>> {
    let cx = b.cx;
    let node = cx.syntax_view(at);
    let asm = ast::AsmExpr::cast(node.value)?;
    let inactive = |ptr: SyntaxNodePtr| {
        b.source_map.diagnostics().iter().any(|it| {
            matches!(it, ExpressionStoreDiagnostics::InactiveCode { node: inactive, .. }
                if inactive.file_id == at.file_id && inactive.value == ptr)
        })
    };
    let mut templates = SmallVec::new();
    for expr in asm.template() {
        if inactive(ast::ptr(expr.syntax())) {
            continue;
        }
        let (value, range) = template_str(cx, expr)?;
        let span = cx.span(node.with_value(range));
        let snippet = cx.snippet(&span);
        templates.push(Template { value, span, snippet });
    }
    Some(templates)
}

/// rustc's `expr_to_spanned_string` of a template: its string and the range of the expression
/// it is, a macro call's when it is one; what a macro was given as `$e:expr` is the expression
/// inside the parentheses the expansion puts it in.
fn template_str<'db>(cx: &Cx<'_, 'db>, expr: ast::Expr<'db>) -> Option<(String, TextRange)> {
    match expr {
        ast::Expr::ParenExpr(it) => template_str(cx, it.expr()?),
        ast::Expr::Literal(_) | ast::Expr::MacroExpr(_) => {
            let range = expr.syntax().text_range();
            Some((string_of(cx, expr)?, range))
        }
        _ => None,
    }
}

/// The string `expr` is once the macros it is a call of are expanded.
fn string_of<'db>(cx: &Cx<'_, 'db>, expr: ast::Expr<'db>) -> Option<String> {
    match expr {
        ast::Expr::Literal(literal) => match literal.kind() {
            ast::LiteralKind::String(it) => Some(it.value().ok()?.into_owned()),
            _ => None,
        },
        ast::Expr::ParenExpr(it) => string_of(cx, it.expr()?),
        ast::Expr::MacroExpr(it) => {
            let expanded = cx.vsema.expand_macro_call(it.macro_call()?)?;
            string_of(cx, ast::Expr::cast(expanded.value)?)
        }
        _ => None,
    }
}

/// `generics_of(owner).requires_monomorphization()` for the item of the body: a type or const
/// parameter of it or of its parent (a trait's `Self` too); lifetimes don't count.
fn requires_monomorphization(b: &BodyCx<'_, '_, '_>) -> bool {
    let db = b.db();
    hir_ty::generics::generics(db, b.owner.generic_def(db))
        .iter(false)
        .any(|(_, it)| !matches!(it, GenericParamDataRef::LifetimeParamData(_)))
}

/// That a lint of a `naked_asm!` read the generics of its function.
fn generics_fact(b: &BodyCx<'_, '_, '_>, diag: &mut Diag) {
    let DefWithBodyId::FunctionId(it) = b.owner else { return };
    // alloc: the lint's diagnostic
    let what = format!("the signature of `{}`", b.cx.def_path(ModuleDefId::FunctionId(it).into()));
    diag.fact(Fact::Signature(b.cx.key(ItemId::Function(it))), what);
}

/// `find_label_span`: where `label` first is in a template's source text, to its next `:`,
/// else to the last character but one.
fn find_label_span(snippet: &str, label: &str) -> Option<(u32, u32)> {
    let pos = snippet.find(label)?;
    let rest = &snippet[pos..];
    let end = pos + rest.find(':').unwrap_or(rest.len() - 1);
    Some((pos as u32, end as u32))
}

/// `InvalidAsmLabel` of a label of `kind` at `span`: its message, then the help and notes of
/// the variant, then those of its fields, as `#[derive(Diagnostic)]` adds them.
fn invalid_asm_label(kind: Label, span: Span, missing_precise_span: bool) -> Diag {
    const NAMED: &str = "avoid using named labels in inline assembly";
    const LOCAL: &str = "only local labels of the form `<number>:` should be used in inline asm";
    const SEE: &str = "see the asm section of Rust By Example \
        <https://doc.rust-lang.org/nightly/rust-by-example/unsafe/asm.html#labels> for more information";
    const MACRO: &str = "the label may be declared in the expansion of a macro";
    match kind {
        Label::Named => {
            let mut diag = Diag::lint("named_asm_labels", NAMED, span);
            diag.help(LOCAL).note(SEE);
            if missing_precise_span {
                diag.note(MACRO);
            }
            diag
        }
        Label::FormatArg => {
            let mut diag = Diag::lint("named_asm_labels", NAMED, span);
            diag.help(LOCAL).note("format arguments may expand to a non-numeric value").note(SEE);
            if missing_precise_span {
                diag.note(MACRO);
            }
            diag
        }
        Label::Binary => {
            let message = "avoid using labels containing only the digits `0` and `1` in inline assembly";
            // clone: a span, for the lint's diagnostic
            let mut diag = Diag::lint("binary_asm_labels", message, span.clone());
            diag.help("start numbering with `2` instead")
                .note("an LLVM bug makes these labels ambiguous with a binary literal number on x86")
                .note("see <https://github.com/llvm/llvm-project/issues/99547> for more information");
            if missing_precise_span {
                diag.note(MACRO);
            }
            diag.span_label(span, "use a different label that doesn't start with `0` or `1`");
            diag
        }
    }
}

/// rustc's `AsmLabelKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Label {
    Named,
    FormatArg,
    Binary,
}

/// The labels `AsmLabels` finds in a template's string: each statement (split at `\n` and `;`,
/// a `//` comment cut off) read from its start, a label ending at each `:` until something is
/// not one.
fn labels(template: &str, raw: bool, hexagon: bool) -> Vec<(&str, Label)> {
    // alloc: empty, pushed to only for a label in a template, which is then linted
    let mut labels = Vec::new();
    for statement in template.split(['\n', ';']) {
        let statement = statement.find("//").map_or(statement, |at| &statement[..at]);
        let mut start = 0;
        'label: for (at, _) in statement.match_indices(':') {
            let label = statement[start..at].trim();
            let mut chars = label.chars();
            let Some(first) = chars.next() else { break };
            let mut in_bracket = false;
            let mut kind = Label::Named;
            if !raw && first == '{' {
                in_bracket = true;
                kind = Label::FormatArg;
            } else if matches!(first, '0' | '1') {
                kind = Label::Binary;
            } else if !(first.is_ascii_alphabetic() || matches!(first, '.' | '_')) {
                break;
            }
            if hexagon && is_hexagon_register_span_context(label, statement, at) {
                break;
            }
            for c in chars {
                if !raw && in_bracket {
                    if c == '{' {
                        break 'label;
                    }
                    if c == '}' {
                        in_bracket = false;
                    }
                } else if !raw && c == '{' {
                    in_bracket = true;
                    kind = Label::FormatArg;
                } else {
                    let continues = match kind {
                        Label::Named | Label::FormatArg => c.is_ascii_alphanumeric() || matches!(c, '_' | '$'),
                        Label::Binary => matches!(c, '0' | '1'),
                    };
                    if !continues {
                        break 'label;
                    }
                }
            }
            labels.push((label, kind));
            start = at + 1;
        }
    }
    labels
}

/// Whether `possible_label` is a Hexagon register pair (`r1:0`, `V5:4.w`), not a label.
#[cfg(test)]
fn is_hexagon_register_span(possible_label: &str) -> bool {
    match possible_label.find(':') {
        Some(colon) => is_hexagon_register_span_impl(&possible_label[..colon], &possible_label[colon + 1..]),
        None => false,
    }
}

/// Whether `possible_label` and what follows the `:` at `colon` in `statement` (up to the first
/// character that is not ASCII alphanumeric or `.`) are a Hexagon register pair.
fn is_hexagon_register_span_context(possible_label: &str, statement: &str, colon: usize) -> bool {
    let after_start = colon + 1;
    if after_start >= statement.len() {
        return false;
    }
    let after = &statement[after_start..];
    let end = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '.')).unwrap_or(after.len());
    is_hexagon_register_span_impl(possible_label, &after[..end])
}

/// A letter then digits before the colon, digits (then anything) after it.
fn is_hexagon_register_span_impl(before_colon: &str, after_colon: &str) -> bool {
    let mut chars = before_colon.chars();
    let Some(start) = chars.next() else { return false };
    if after_colon.is_empty() || !start.is_ascii_alphabetic() {
        return false;
    }
    let rest = chars.as_str();
    !rest.is_empty()
        && rest.chars().all(|c| c.is_ascii_digit())
        && after_colon.starts_with(|c: char| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(template: &str) -> Vec<(&str, Label)> {
        labels(template, false, false)
    }

    #[test]
    fn labels_end_at_the_first_thing_that_is_not_one() {
        assert_eq!(named("foo: bar1: nop"), [("foo", Label::Named), ("bar1", Label::Named)]);
        assert_eq!(named("blah1: 2bar: nop"), [("blah1", Label::Named)]);
        assert_eq!(named("def: def: nop"), [("def", Label::Named), ("def", Label::Named)]);
        for template in [
            "1bar: blah: nop",
            "ab cd: nop",
            "mov rax, qword ptr fs:[0]",
            ":lo12:FOO",
            ":bbb nop",
            ":",
            "\n:\n",
            "::::",
            "Ù: nop",
            "testÙ: nop",
            "_Ù_: nop",
        ] {
            assert!(named(template).is_empty(), "{template}");
        }
    }

    #[test]
    fn statements_split_at_newlines_and_semicolons_without_comments() {
        assert_eq!(named("\n ab: nop // ab: does foo\n // cd: nop\n"), [("ab", Label::Named)]);
        assert_eq!(named("nop; foo10: nop"), [("foo10", Label::Named)]);
        assert_eq!(named("nop\n bar6: bar7: nop"), [("bar6", Label::Named), ("bar7", Label::Named)]);
        let included = "lab1: nop\n// do more things\nlab2: nop // does bar\n// a: b\nlab3: nop; lab4: nop\n";
        let found: Vec<&str> = named(included).into_iter().map(|(it, _)| it).collect();
        assert_eq!(found, ["lab1", "lab2", "lab3", "lab4"]);
    }

    #[test]
    fn format_arguments_are_labels_unless_raw() {
        assert_eq!(named("test_{}: nop"), [("test_{}", Label::FormatArg)]);
        assert_eq!(named("{}_test: nop"), [("{}_test", Label::FormatArg)]);
        assert_eq!(named("{uwu}: nop"), [("{uwu}", Label::FormatArg)]);
        assert!(named("{{}: nop").is_empty());
        assert!(labels("test_{}: nop", true, false).is_empty());
        assert!(labels("{}: nop", true, false).is_empty());
    }

    #[test]
    fn binary_labels_are_only_zeros_and_ones() {
        assert_eq!(named("0: jmp 0b"), [("0", Label::Binary)]);
        assert_eq!(named("1001101: jmp 1001101b"), [("1001101", Label::Binary)]);
        assert!(named("12: jmp 12b").is_empty());
        assert!(named("2: jmp 2b").is_empty());
    }

    /// rustc's `compiler/rustc_lint/src/tests.rs`, `test_hexagon_register_span_patterns`.
    #[test]
    fn hexagon_register_span_patterns() {
        for it in ["r1:0", "r15:14", "V5:4", "V3:2", "V5:4.w", "V3:2.h", "r99:98", "V123:122.whatever"] {
            assert!(is_hexagon_register_span(it), "{it}");
        }
        for it in ["label1", "foo:", ":0", "r:0", "r1:", "r1:a", "1:0", "r1", "r", "", "ra:0"] {
            assert!(!is_hexagon_register_span(it), "{it}");
        }
    }

    #[test]
    fn hexagon_register_pairs_are_not_labels() {
        for template in [
            "r1:0 = memd(r29+#0)",
            "r3:2 = combine(#1, #0)",
            "r15:14 = memd(r30+#8)",
            "memd(r29+#0) = r5:4",
            "V5:4 = vaddw(v1:0, v1:0)",
            "v1:0.w = vsub(v1:0.w,v1:0.w):sat",
        ] {
            assert!(labels(template, false, true).is_empty(), "{template}");
        }
        assert_eq!(labels("label1: r7:6 = combine(#2, #3)", false, true), [("label1", Label::Named)]);
        assert_eq!(named("r1:0 = memd(r29+#0)"), [("r1", Label::Named)]);
    }

    #[test]
    fn a_label_span_runs_to_its_colon() {
        assert_eq!(find_label_span("\"bar: nop\"", "bar"), Some((1, 4)));
        assert_eq!(find_label_span("\"fooo\\u{003A} nop\"", "fooo"), Some((1, 17)));
        assert_eq!(find_label_span("\"\\x41\\x42\\x43\\x3A\"", "ABC"), None);
        assert_eq!(find_label_span("include_str!(\"named-asm-labels.s\")", "lab1"), None);
    }
}
