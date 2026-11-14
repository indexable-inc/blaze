//! `transmute`: `std::mem::transmute` or `transmute_copy`. It compiles for any two types of one
//! size and checks nothing else: a reference made to outlive its value, a `bool` or `char` or
//! enum given a value it can't hold, a pointer's provenance lost, a layout that is not stable
//! (`Vec`, a `repr(Rust)` struct) read as another. For the pairs std has a safe function for,
//! that function says the same with no `unsafe`, and the fix writes it:
//!
//! | from → to | instead |
//! |---|---|
//! | `f32` → `u32`, `f64` → `u64` (and `f16`, `f128`) | `x.to_bits()` |
//! | `u32` → `f32`, `u64` → `f64` | `f32::from_bits(x)` |
//! | `iN` → `uN`, `uN` → `iN` | `x.cast_unsigned()`, `x.cast_signed()` |
//! | a number → `[u8; N]` | `x.to_ne_bytes()` |
//! | `[u8; N]` → a number | `T::from_ne_bytes(x)` |
//! | `bool` → `u8`, `char` → `u32` | `u8::from(x)`, `u32::from(x)` |
//! | `*const T` / `*mut T` → `*const U` / `*mut U` | `p.cast()`, with `cast_mut`/`cast_const` |
//!
//! The number fixes are machine-applicable: each function is defined as that transmute. The
//! pointer fix is not (`cast` needs a sized pointee, so a fat pointer's would not compile).
//! When the call is all of its `unsafe { .. }` block and its argument is a local or a literal,
//! the fix replaces the block. Every other pair gets no fix. Outside test code.

use hir::{BuiltinType, Type};
use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, HasArgList as _},
};

use super::{Fix, called, in_test, is_std_fn, parent_expr, receiver_text, report};
use crate::{blaze::Lint, check::Cx, diag::Applicability};

const LINT: Lint = Lint::Transmute;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some(call) = ast::CallExpr::cast(node) else { return };
    let Some(f) = called(cx, call) else { return };
    let copy = match f.name(cx.db()).as_str() {
        "transmute" if is_std_fn(cx, f, &["core", "mem", "transmute"]) => false,
        "transmute_copy" if is_std_fn(cx, f, &["core", "mem", "transmute_copy"]) => true,
        _ => return,
    };
    if in_test(cx, node) {
        return;
    }
    let Some(range) = cx.range(node) else { return };
    let arg = call.arg_list().and_then(|it| it.args().next());
    let fix = match (copy, arg) {
        (false, Some(arg)) => fix(cx, call, arg),
        _ => None,
    };
    let what = if copy { "transmute_copy" } else { "transmute" };
    let message = match &fix {
        Some(_) => format!("this `{what}` has a safe function in std that says the same"),
        None => format!("this `{what}` checks nothing but the two types' sizes"),
    };
    report(
        cx,
        LINT,
        node,
        range,
        message,
        "a reference made to outlive its value, a value a `bool`, `char` or enum can't hold, a pointer's provenance or an unstable layout read as another all compile; say the conversion with the function that checks it (`to_bits`, `from_ne_bytes`, `cast`, `bytemuck`'s `cast` for plain-data structs)",
        fix,
    );
}

/// The fix of `call`, `transmute(arg)`, when std has a safe function for its two types.
fn fix<'t>(cx: &Cx<'_, 't>, call: ast::CallExpr<'t>, arg: ast::Expr<'t>) -> Option<Fix> {
    let db = cx.db();
    let node = call.syntax();
    let from = cx.ty(arg)?;
    let to = cx.ty(ast::Expr::CallExpr(call))?;
    let x = receiver_text(cx, arg, cx.range(arg.syntax())?);
    let plain = cx.snippet(cx.range(arg.syntax())?).to_owned();
    let (text, applicability) = if let (Some(a), Some(b)) = (from.as_builtin(), to.as_builtin()) {
        (numbers(a, b, &x, &plain)?, Applicability::MachineApplicable)
    } else if let (Some(a), Some((elem, n))) = (from.as_builtin(), to.as_array(db)) {
        if width(a)? != n || !is_u8(&elem) || a.is_bool() || a.is_char() {
            return None;
        }
        (format!("{x}.to_ne_bytes()"), Applicability::MachineApplicable)
    } else if let (Some((elem, n)), Some(b)) = (from.as_array(db), to.as_builtin()) {
        if width(b)? != n || !is_u8(&elem) || b.is_bool() || b.is_char() {
            return None;
        }
        (format!("{}::from_ne_bytes({plain})", b.name().as_str()), Applicability::MachineApplicable)
    } else if let (Some((_, a)), Some((_, b))) = (from.as_raw_ptr(), to.as_raw_ptr()) {
        let mutability = match (a, b) {
            (hir::Mutability::Shared, hir::Mutability::Mut) => ".cast_mut()",
            (hir::Mutability::Mut, hir::Mutability::Shared) => ".cast_const()",
            _ => "",
        };
        (format!("{x}{mutability}.cast()"), Applicability::MaybeIncorrect)
    } else {
        return None;
    };
    let range = whole_block(cx, node, arg).or_else(|| cx.range(node))?;
    Some(Fix {
        message: "say the conversion with std's safe function",
        // alloc: the fix's one part
        parts: vec![(range, text)],
        applicability,
    })
}

/// The safe function for a transmute of the number `a` to the number `b`, applied to `x` (the
/// argument as a receiver) or `plain` (as written).
fn numbers(a: BuiltinType, b: BuiltinType, x: &str, plain: &str) -> Option<String> {
    let to = b.name();
    let to = to.as_str();
    // `width` leaves `usize` and `isize` out, so no pair here is the target's to decide.
    if width(a)? != width(b)? {
        return None;
    }
    Some(match () {
        () if a.is_float() && b.is_uint() => format!("{x}.to_bits()"),
        () if a.is_uint() && b.is_float() => format!("{to}::from_bits({plain})"),
        () if a.is_int() && b.is_uint() => format!("{x}.cast_unsigned()"),
        () if a.is_uint() && b.is_int() => format!("{x}.cast_signed()"),
        () if a.is_bool() && to == "u8" => format!("u8::from({plain})"),
        () if a.is_char() && to == "u32" => format!("u32::from({plain})"),
        () => return None,
    })
}

/// How many bytes the number `ty` is; `usize` and `isize` are left out, their width being the
/// target's.
fn width(ty: BuiltinType) -> Option<usize> {
    Some(match ty.name().as_str() {
        "u8" | "i8" | "bool" => 1,
        "u16" | "i16" | "f16" => 2,
        "u32" | "i32" | "f32" | "char" => 4,
        "u64" | "i64" | "f64" => 8,
        "u128" | "i128" | "f128" => 16,
        _ => return None,
    })
}

fn is_u8(ty: &Type<'_>) -> bool {
    ty.as_builtin().is_some_and(|it| it.name().as_str() == "u8")
}

/// The `unsafe { .. }` block `node` is all of, when its argument `arg` needs no `unsafe` of its
/// own (a local, a literal), so the fix can replace the block.
fn whole_block<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>, arg: ast::Expr<'t>) -> Option<TextRange> {
    let own = match arg {
        ast::Expr::Literal(_) => true,
        ast::Expr::PathExpr(path) => {
            matches!(path.path().and_then(|it| cx.path(it)), Some(hir::PathResolution::Local(_)))
        }
        _ => false,
    };
    if !own {
        return None;
    }
    let list = parent_expr(node)?;
    if list.kind() != SyntaxKind::STMT_LIST {
        return None;
    }
    let block = ast::BlockExpr::cast(list.parent()?)?;
    block.unsafe_token()?;
    if block.statements().next().is_some() || block.label().is_some() {
        return None;
    }
    cx.range(block.syntax())
}
