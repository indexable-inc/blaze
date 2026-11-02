//! `overflowing_literals`: an integer or float literal out of the range of its type, with the
//! cast to `char`, the range endpoint and the hexadecimal and binary forms rustc says more of.
//!
//! Follows rustc's `compiler/rustc_lint/src/types/literal.rs` and the messages of
//! `compiler/rustc_lint/src/lints.rs` (`OverflowingInt`, `OverflowingBinHex`, ..) (MIT or
//! Apache-2.0).

use hir_ty::db::HirDatabase;
use hir::InFile;
use hir_def::{
    HasModule,
    hir::{Expr, ExprId, Literal},
};
use hir_ty::next_solver::{Ty, TyKind};
use rustc_apfloat::{
    Float as _,
    ieee::{Double, Half, Quad, Single},
};
use rustc_type_ir::{FloatTy, IntTy, UintTy, inherent::IntoKind};
use syntax::{
    SyntaxNodePtr,
    view::{self as ast, AstNode},
};

use super::super::util;
use crate::{
    lints::bodies::BodyCx,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

pub(super) const LINT: &str = "overflowing_literals";

/// A literal as rustc's `check_lit` is given it.
struct Lit<'db> {
    expr: ExprId,
    at: InFile<SyntaxNodePtr>,
    kind: ast::LiteralKind<'db>,
    /// `lit.span`: the literal's, or of a negated literal of a pattern, with its `-`.
    span: Span,
}

/// Whether the literal `expr` of an expression, `lit` lowered, may be out of the range of its
/// type: its lowered value against the type's range, with no syntax, no span and no
/// allocation. A literal that wrapped when lowered (`as i128` of one above `i128::MAX`) is
/// negative and goes to [`lint_literal`], which reads it as written. `negated`: it is the
/// operand of a `-`; `pointer_bits`: the crate's pointer width.
pub(super) fn may_overflow(
    b: &BodyCx<'_, '_, '_>,
    expr: ExprId,
    lit: &Literal,
    negated: bool,
    pointer_bits: Option<u16>,
) -> bool {
    let value = match *lit {
        Literal::Int(value, _) => match u128::try_from(value) {
            Ok(value) => value,
            Err(_) => return true,
        },
        Literal::Uint(value, _) => value,
        Literal::Float(..) => return float_overflows(b, expr, lit),
        _ => return false,
    };
    int_overflows(b, expr, value, negated, pointer_bits)
}

/// [`may_overflow`] of the literal `expr` of a pattern, whose `-` the lowering folded into its
/// value. The folded value is exact unless the literal wrapped, which needs at least 34
/// characters (`0x` and 32 hex digits for `2^127`; 39 decimal digits): the length of the
/// literal's syntax pointer, which costs no parse, tells.
pub(super) fn pat_may_overflow(b: &BodyCx<'_, '_, '_>, expr: ExprId, lit: &Literal, pointer_bits: Option<u16>) -> bool {
    let (value, negated) = match *lit {
        Literal::Int(value, _) => (value.unsigned_abs(), value < 0),
        Literal::Uint(value, _) => (value, false),
        Literal::Float(..) => return float_overflows(b, expr, lit),
        _ => return false,
    };
    let Some(at) = b.expr_node(expr) else { return false };
    if u32::from(at.value.text_range().len()) >= 34 {
        return true;
    }
    int_overflows(b, expr, value, negated, pointer_bits)
}

/// Whether an integer literal of magnitude `value` is out of the range of the type of `expr`
/// (rustc's checks of `lint_int_literal` and `lint_uint_literal`, before they read the source).
fn int_overflows(b: &BodyCx<'_, '_, '_>, expr: ExprId, value: u128, negated: bool, pointer_bits: Option<u16>) -> bool {
    match util::ty(b, expr).kind() {
        TyKind::Int(t) => {
            let t = match (t, pointer_bits) {
                (IntTy::Isize, Some(bits)) => t.normalize(bits),
                (IntTy::Isize, None) => return false,
                _ => t,
            };
            let max = int_ty_range(t).1 as u128;
            if negated { value > max + 1 } else { value > max }
        }
        TyKind::Uint(t) => {
            let t = match (t, pointer_bits) {
                (UintTy::Usize, Some(bits)) => t.normalize(bits),
                (UintTy::Usize, None) => return false,
                _ => t,
            };
            !negated && value > uint_ty_range(t).1
        }
        _ => false,
    }
}

/// Whether the float literal `expr`, `lit` lowered, is infinite in its type: rustc's
/// `float_is_infinite`, on the lowered literal's text.
fn float_overflows(b: &BodyCx<'_, '_, '_>, expr: ExprId, lit: &Literal) -> bool {
    let (TyKind::Float(t), Literal::Float(value, _)) = (util::ty(b, expr).kind(), lit) else { return false };
    match t {
        FloatTy::F16 => value.to_f16().is_infinite(),
        FloatTy::F32 => value.to_f32().is_infinite(),
        FloatTy::F64 => value.to_f64().is_infinite(),
        FloatTy::F128 => value.to_f128().is_infinite(),
    }
}

/// rustc's `lint_literal` of the literal `expr`, `negation` the `-` expression it is the
/// operand of; a pattern's literal says itself whether it has a `-`.
pub(super) fn lint_literal(b: &BodyCx<'_, '_, '_>, expr: ExprId, negation: Option<ExprId>, out: &mut Vec<Found>) {
    let negation = match negation {
        Some(it) => match util::span(b, it) {
            Some(span) => Some(span),
            None => return,
        },
        None => None,
    };
    let Some((lit, negated_pat)) = literal(b, expr) else { return };
    // clone: a span or node handle, for a literal `may_overflow` found
    let negation = if negated_pat { Some(lit.span.clone()) } else { negation };
    match util::ty(b, expr).kind() {
        TyKind::Int(t) => {
            let ast::LiteralKind::IntNumber(number) = &lit.kind else { return };
            if is_float_suffix(number.suffix()) {
                return;
            }
            let Ok(value) = number.value() else { return };
            lint_int_literal(b, &lit, t, value, negation, out);
        }
        TyKind::Uint(t) => {
            if negation.is_some() {
                return;
            }
            lint_uint_literal(b, &lit, t, out);
        }
        TyKind::Float(t) => {
            let text = match &lit.kind {
                ast::LiteralKind::FloatNumber(number) => number.value_string(),
                ast::LiteralKind::IntNumber(number) if is_float_suffix(number.suffix()) => number.value_string(),
                _ => return,
            };
            let infinite = match t {
                FloatTy::F16 => text.parse::<Half>().ok().map(|it| it.is_infinite()),
                FloatTy::F32 => text.parse::<Single>().ok().map(|it| it.is_infinite()),
                FloatTy::F64 => text.parse::<Double>().ok().map(|it| it.is_infinite()),
                FloatTy::F128 => text.parse::<Quad>().ok().map(|it| it.is_infinite()),
            };
            if infinite != Some(true) {
                return;
            }
            let ty = t.name_str();
            let written = snippet(b, &lit.span).unwrap_or(text);
            // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found; clone: a span or node handle, for a literal `may_overflow` found
            let mut diag = Diag::lint(LINT, format!("literal out of range for `{ty}`"), lit.span.clone());
            // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found
            diag.note(format!(
                "the literal `{written}` does not fit into the type `{ty}` and will be converted to `{ty}::INFINITY`"
            ));
            out.push(b.lint(diag, Phase::Late, lit.at));
        }
        _ => {}
    }
}

/// The literal `expr` is written as, and whether it is a pattern's with a `-` before it.
fn literal<'db>(b: &BodyCx<'_, '_, 'db>, expr: ExprId) -> Option<(Lit<'db>, bool)> {
    let at = b.expr_node(expr)?;
    let node = b.cx.syntax_view(at);
    let (lit, pat) = match ast::Literal::cast(node.value) {
        Some(lit) => {
            let pat = lit.syntax().parent().and_then(ast::LiteralPat::cast);
            (lit, pat)
        }
        None => {
            // clone: a span or node handle, for a literal `may_overflow` found
            let pat = ast::LiteralPat::cast(node.value)?;
            (pat.literal()?, Some(pat))
        }
    };
    let negated_pat = pat.as_ref().is_some_and(|it| it.minus_token().is_some());
    let range = match &pat {
        Some(pat) if negated_pat => pat.syntax().text_range(),
        _ => lit.syntax().text_range(),
    };
    let span = b.cx.span(node.with_value(range));
    Some((Lit { expr, at, kind: lit.kind(), span }, negated_pat))
}

fn is_float_suffix(suffix: Option<&str>) -> bool {
    matches!(suffix, Some("f16" | "f32" | "f64" | "f128"))
}

fn snippet(b: &BodyCx<'_, '_, '_>, span: &Span) -> Option<String> {
    b.cx.snippet(span).map(str::to_owned)
}

/// The width of a pointer of the crate's target, in bits, that `isize` and `usize` have.
pub(super) fn pointer_width(b: &BodyCx<'_, '_, '_>) -> Option<u16> {
    let db = b.db();
    let layout = db.target_data_layout(b.owner.krate(db)).ok()?;
    u16::try_from(layout.pointer_size().bits()).ok()
}

/// rustc's `int_ty_range`: `isize` as `i64`, so that the lint is the same on 32 and 64 bits
/// where the type is not normalized first.
pub(super) fn int_ty_range(t: IntTy) -> (i128, i128) {
    match t {
        IntTy::Isize => (i64::MIN.into(), i64::MAX.into()),
        IntTy::I8 => (i8::MIN.into(), i8::MAX.into()),
        IntTy::I16 => (i16::MIN.into(), i16::MAX.into()),
        IntTy::I32 => (i32::MIN.into(), i32::MAX.into()),
        IntTy::I64 => (i64::MIN.into(), i64::MAX.into()),
        IntTy::I128 => (i128::MIN, i128::MAX),
    }
}

pub(super) fn uint_ty_range(t: UintTy) -> (u128, u128) {
    let max = match t {
        UintTy::Usize => u64::MAX.into(),
        UintTy::U8 => u8::MAX.into(),
        UintTy::U16 => u16::MAX.into(),
        UintTy::U32 => u32::MAX.into(),
        UintTy::U64 => u64::MAX.into(),
        UintTy::U128 => u128::MAX,
    };
    (0, max)
}

/// rustc's `Integer`: the integer sizes, by their bits.
fn fit_unsigned(value: u128) -> u32 {
    match value {
        0..=0xff => 8,
        0..=0xffff => 16,
        0..=0xffff_ffff => 32,
        0..=0xffff_ffff_ffff_ffff => 64,
        _ => 128,
    }
}

fn fit_signed(value: i128) -> u32 {
    match value {
        -0x80..=0x7f => 8,
        -0x8000..=0x7fff => 16,
        -0x8000_0000..=0x7fff_ffff => 32,
        -0x8000_0000_0000_0000..=0x7fff_ffff_ffff_ffff => 64,
        _ => 128,
    }
}

fn uint_name(bits: u32) -> &'static str {
    match bits {
        8 => "u8",
        16 => "u16",
        32 => "u32",
        64 => "u64",
        _ => "u128",
    }
}

fn int_name(bits: u32) -> &'static str {
    match bits {
        8 => "i8",
        16 => "i16",
        32 => "i32",
        64 => "i64",
        _ => "i128",
    }
}

/// rustc's `Size::sign_extend` of `value` to `bits`.
fn sign_extend(value: u128, bits: u32) -> i128 {
    let shift = 128 - bits;
    ((value << shift) as i128) >> shift
}

/// rustc's `Size::truncate` of `value` to `bits`.
fn truncate(value: u128, bits: u32) -> u128 {
    let shift = 128 - bits;
    (value << shift) >> shift
}

fn literal_to_i128(value: u128, negative: bool) -> Option<i128> {
    if negative {
        (value <= i128::MAX as u128 + 1).then(|| value.wrapping_neg() as i128)
    } else {
        value.try_into().ok()
    }
}

/// rustc's `get_type_suggestion`: the next type that fits, of the same size where it can; none
/// for `isize` and `usize`.
fn type_suggestion(ty: Ty<'_>, value: u128, negative: bool) -> Option<&'static str> {
    match ty.kind() {
        TyKind::Uint(UintTy::Usize) | TyKind::Int(IntTy::Isize) => None,
        TyKind::Uint(_) => Some(uint_name(fit_unsigned(value))),
        TyKind::Int(_) => {
            let signed = literal_to_i128(value, negative).map(fit_signed);
            if negative {
                return signed.map(int_name);
            }
            let unsigned = fit_unsigned(value);
            Some(match signed {
                Some(signed) if unsigned >= signed => int_name(signed),
                _ => uint_name(unsigned),
            })
        }
        _ => None,
    }
}

/// rustc's `get_bin_hex_repr`: the literal as written when it is hexadecimal or binary.
fn bin_hex_repr(b: &BodyCx<'_, '_, '_>, lit: &Lit<'_>) -> Option<String> {
    let src = snippet(b, &lit.span)?;
    let mut chars = src.chars();
    if chars.next()? == '0' && matches!(chars.next(), Some('x' | 'b')) {
        return Some(src);
    }
    None
}

enum IntType {
    Signed(IntTy),
    Unsigned(UintTy),
}

/// rustc's `report_bin_hex_error`.
fn report_bin_hex_error(
    b: &BodyCx<'_, '_, '_>,
    lit: &Lit<'_>,
    ty: IntType,
    bits: u32,
    repr: String,
    value: u128,
    negative: bool,
    out: &mut Vec<Found>,
) {
    let (name, actually) = match ty {
        IntType::Signed(t) => {
            let extended = sign_extend(value, bits);
            let actually = if negative { extended.wrapping_neg() } else { extended };
            // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found
            (t.name_str(), actually.to_string())
        }
        // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found
        IntType::Unsigned(t) => (t.name_str(), truncate(value, bits).to_string()),
    };
    // clone: a span or node handle, for a literal `may_overflow` found
    let span = lit.span.clone();
    // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found; clone: a span or node handle, for a literal `may_overflow` found
    let mut diag = Diag::lint(LINT, format!("literal out of range for `{name}`"), span.clone());
    if negative {
        // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found
        diag.note(format!("the literal `{repr}` (decimal `{value}`) does not fit into the type `{name}`"));
        // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found
        diag.note(format!("and the value `-{repr}` will become `{actually}{name}`"));
    } else {
        // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found
        diag.note(format!(
            "the literal `{repr}` (decimal `{value}`) does not fit into the type `{name}` and will become `{actually}{name}`"
        ));
    }
    let lit_ty = util::ty(b, lit.expr);
    let suffix_at = repr.chars().position(|it| it == 'i' || it == 'u');
    if let Some(suggestion_ty) = type_suggestion(lit_ty, value, negative) {
        // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found
        let message = format!("consider using the type `{suggestion_ty}` instead");
        match suffix_at {
            Some(at) => {
                let sans_suffix = &repr[..at];
                diag.suggestion(
                    // clone: a span or node handle, for a literal `may_overflow` found
                    span.clone(),
                    message,
                    // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found
                    format!("{sans_suffix}{suggestion_ty}"),
                    Applicability::MachineApplicable,
                );
            }
            None => {
                diag.help(message);
            }
        }
    }
    if !negative
        && let TyKind::Int(int_ty) = lit_ty.kind()
        && let Some(bit_width) = int_ty.bit_width()
        && value & (1u128 << (bit_width - 1)) != 0
    {
        let lit_no_suffix = match suffix_at {
            Some(at) => &repr[..at],
            None => &repr[..],
        };
        let uint_ty = uint_name(fit_unsigned(value));
        let int_ty = int_ty.name_str();
        diag.suggestion(
            span,
            // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found
            format!(
                "to use as a negative number (decimal `{actually}`), consider using the type `{uint_ty}` for the literal and cast it to `{int_ty}`"
            ),
            // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found
            format!("{lit_no_suffix}{uint_ty} as {int_ty}"),
            Applicability::MaybeIncorrect,
        );
    }
    // clone: a span or node handle, for a literal `may_overflow` found
    out.push(b.lint(diag, Phase::Late, lit.at.clone()));
}

/// The cast `expr` is the operand of, if it is one's (rustc's `parent_hir_node` an
/// `ExprKind::Cast`).
fn parent_cast(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> Option<ExprId> {
    b.exprs().iter().copied().find(|&it| matches!(b.store().expr(it), Expr::Cast { expr: inner, .. } if inner == expr))
}

/// rustc's `lint_overflowing_range_endpoint`: the end of an exclusive range (`0..256` of `u8`,
/// through a cast as in `0..256 as u8`) one past the type's maximum, which `..=` makes fit.
/// Whether it linted.
fn lint_overflowing_range_endpoint(
    b: &BodyCx<'_, '_, '_>,
    lit: &Lit<'_>,
    suffix: Option<&str>,
    value: u128,
    max: u128,
    ty: &str,
    out: &mut Vec<Found>,
) -> bool {
    let (id, span) = match parent_cast(b, lit.expr) {
        Some(cast) => match util::span(b, cast) {
            Some(span) => (cast, span),
            None => return false,
        },
        // clone: a span or node handle, for a literal `may_overflow` found
        None => (lit.expr, lit.span.clone()),
    };
    let range = b.exprs().iter().copied().find(|&it| match b.store().expr(it) {
        Expr::RecordLit { fields, .. } => fields.iter().any(|field| field.expr == id),
        _ => false,
    });
    let Some(range) = range else { return false };
    let Expr::RecordLit { fields, .. } = b.store().expr(range) else { return false };
    let Some(node) = util::syntax(b, range) else { return false };
    if !ast::RangeExpr::can_cast(node.value.kind()) {
        return false;
    }
    let Some(range_span) = util::span(b, range) else { return false };
    let [start, end] = fields else { return false };
    if !(end.expr == id && value - 1 == max) {
        return false;
    }
    let suffix = suffix.unwrap_or("");
    let literal = value - 1;
    let message = "use an inclusive range instead";
    // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found; clone: a span or node handle, for a literal `may_overflow` found
    let mut diag = Diag::lint(LINT, format!("range endpoint is out of range for `{ty}`"), range_span.clone());
    if span.lo == lit.span.lo {
        let Some(start) = b.expr_span(start.expr).and_then(|it| snippet(b, &it)) else { return false };
        diag.suggestion(
            range_span.shrink_to_lo().to(&lit.span.shrink_to_hi()),
            message,
            // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found
            format!("{start}..={literal}{suffix}"),
            Applicability::MachineApplicable,
        );
    } else {
        diag.suggestion_with_style(
            // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found; clone: a span or node handle, for a literal `may_overflow` found
            vec![(span.shrink_to_lo(), "=".to_owned()), (lit.span.clone(), format!("{literal}{suffix}"))],
            message,
            Applicability::MachineApplicable,
            SuggestionStyle::ShowCode,
        );
    }
    // clone: a span or node handle, for a literal `may_overflow` found
    out.push(b.lint(diag, Phase::Late, lit.at.clone()));
    true
}

/// rustc's `lint_int_literal`.
fn lint_int_literal(
    b: &BodyCx<'_, '_, '_>,
    lit: &Lit<'_>,
    t: IntTy,
    value: u128,
    negation: Option<Span>,
    out: &mut Vec<Found>,
) {
    let int_type = match t {
        IntTy::Isize => match pointer_width(b) {
            Some(width) => t.normalize(width),
            None => return,
        },
        _ => t,
    };
    let (min, max) = int_ty_range(int_type);
    let max = max as u128;
    let negative = negation.is_some();
    if !((negative && value > max + 1) || (!negative && value > max)) {
        return;
    }
    if let Some(repr) = bin_hex_repr(b, lit) {
        let Some(bits) = int_type.bit_width() else { return };
        report_bin_hex_error(b, lit, IntType::Signed(t), bits as u32, repr, value, negative, out);
        return;
    }
    let ast::LiteralKind::IntNumber(number) = &lit.kind else { return };
    if lint_overflowing_range_endpoint(b, lit, number.suffix(), value, max, t.name_str(), out) {
        return;
    }
    // clone: a span or node handle, for a literal `may_overflow` found
    let span = negation.unwrap_or_else(|| lit.span.clone());
    // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found
    let written = snippet(b, &span).unwrap_or_else(|| if negative { format!("-{value}") } else { value.to_string() });
    let ty = t.name_str();
    // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found
    let mut diag = Diag::lint(LINT, format!("literal out of range for `{ty}`"), span);
    // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found
    diag.note(format!("the literal `{written}` does not fit into the type `{ty}` whose range is `{min}..={max}`"));
    if let Some(suggestion_ty) = type_suggestion(util::ty(b, lit.expr), value, negative) {
        // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found
        diag.help(format!("consider using the type `{suggestion_ty}` instead"));
    }
    // clone: a span or node handle, for a literal `may_overflow` found
    out.push(b.lint(diag, Phase::Late, lit.at.clone()));
}

/// rustc's `lint_uint_literal`.
fn lint_uint_literal(b: &BodyCx<'_, '_, '_>, lit: &Lit<'_>, t: UintTy, out: &mut Vec<Found>) {
    let uint_type = match t {
        UintTy::Usize => match pointer_width(b) {
            Some(width) => t.normalize(width),
            None => return,
        },
        _ => t,
    };
    let (_, max) = uint_ty_range(uint_type);
    let ast::LiteralKind::IntNumber(number) = &lit.kind else { return };
    if is_float_suffix(number.suffix()) {
        return;
    }
    let Ok(value) = number.value() else { return };
    if value <= max {
        return;
    }
    if let Some(cast) = parent_cast(b, lit.expr)
        && util::ty(b, cast).is_char()
        && let Some(cast_span) = util::span(b, cast)
    {
        let diag = if value > 0x10FFFF {
            let mut diag = Diag::lint(LINT, "value exceeds maximum `char` value", cast_span);
            diag.note("maximum valid `char` value is `0x10FFFF`");
            diag
        } else if (0xD800..=0xDFFF).contains(&value) {
            let mut diag = Diag::lint(LINT, "surrogate values are not valid for `char`", cast_span);
            diag.note("`0xD800..=0xDFFF` are reserved for Unicode surrogates and are not valid `char` values");
            diag
        } else {
            // clone: a span or node handle, for a literal `may_overflow` found
            let mut diag = Diag::lint(LINT, "only `u8` can be cast into `char`", cast_span.clone());
            diag.suggestion(
                cast_span,
                "use a `char` literal instead",
                // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found
                format!("'\\u{{{value:X}}}'"),
                Applicability::MachineApplicable,
            );
            diag
        };
        // clone: a span or node handle, for a literal `may_overflow` found
        out.push(b.lint(diag, Phase::Late, lit.at.clone()));
        return;
    }
    if lint_overflowing_range_endpoint(b, lit, number.suffix(), value, max, t.name_str(), out) {
        return;
    }
    if let Some(repr) = bin_hex_repr(b, lit) {
        let Some(bits) = uint_type.bit_width() else { return };
        report_bin_hex_error(b, lit, IntType::Unsigned(t), bits as u32, repr, value, false, out);
        return;
    }
    let ty = t.name_str();
    // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found
    let written = snippet(b, &lit.span).unwrap_or_else(|| value.to_string());
    // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found; clone: a span or node handle, for a literal `may_overflow` found
    let mut diag = Diag::lint(LINT, format!("literal out of range for `{ty}`"), lit.span.clone());
    // alloc: the overflowing_literals lint's text, for a literal `may_overflow` found
    diag.note(format!("the literal `{written}` does not fit into the type `{ty}` whose range is `0..={max}`"));
    // clone: a span or node handle, for a literal `may_overflow` found
    out.push(b.lint(diag, Phase::Late, lit.at.clone()));
}
