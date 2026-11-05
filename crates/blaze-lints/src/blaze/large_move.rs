//! `large_move`: in hot code (it is in `blaze::hot`), a value whose layout is over [`LIMIT`]
//! bytes moved by value, a `memcpy` of all of it at each move:
//!
//! - a parameter of a function taken by value (`self` included), and a function's return type:
//!   each call copies the argument in, and the result out to the caller's slot (a return of a
//!   local built in place is that one move, found once, at the signature);
//! - a place (a local, a field, an index, a deref) moved by value into a call's argument or a
//!   struct literal's field. A value made right there (a literal, a call's result, a
//!   constructor) is built in the slot it goes to and is not found;
//! - an enum whose largest variant is over `LIMIT` and over 4× its second largest: every value
//!   pays for the one, as with clippy's `large_enum_variant`, but only in hot code and only past
//!   the size limit.
//!
//! Sizes are the front end's layouts, so `Box`, `Vec`, `Arc` and the other pointer-sized
//! handles are small and never found; a type whose layout depends on a parameter it was not
//! given is not checked. Not checked: a method of a trait impl (its signature is the trait's),
//! and what a macro wrote. Keep one with `#[expect(blaze::large_move, reason = "..")]`.

use hir::{AsAssocItem as _, HirDisplay as _};
use hir_def::hir::{Expr, ExprId, UnaryOp};
use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::{Finding, Lint, exempt, facts};
use crate::check::Cx;

const LINT: Lint = Lint::LargeMove;

/// 512 bytes: eight 64-byte lines, past the copies LLVM writes out as a few vector moves on
/// x86-64 and arm64 (it calls `memcpy` for larger ones), and large enough that a move is not
/// lost in the call around it. Smaller structs pass by value well.
const LIMIT: u64 = 512;

/// How many times the second largest variant the largest must be to be found.
const VARIANT_RATIO: u64 = 4;

/// The size of `ty`'s layout when it is over [`LIMIT`].
fn large(db: &base_db::Db, ty: &hir::Type<'_>) -> Option<u64> {
    if ty.is_reference() || ty.is_raw_ptr() {
        return None;
    }
    let size = ty.layout(db).ok()?.size();
    (size > LIMIT).then_some(size)
}

const FIX: &str = "pass by reference or box the large variant";

/// A function's parameters and return type.
pub(super) fn check_fn<'t>(cx: &mut Cx<'_, 't>, f: ast::Fn<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    if cx.expansion().is_some() || !cx.is_written(f.syntax()) {
        return;
    }
    let db = cx.db();
    let Some(func) = cx.fn_def(f) else { return };
    if func.as_assoc_item(db).is_some_and(|it| it.implemented_trait(db).is_some()) {
        return;
    }
    let target = cx.krate().to_display_target(db);
    let name = func.name(db);
    let name = name.as_str();
    // alloc: the findings of one signature, reported at the edge
    let mut found: Vec<(ast::Node<'t>, String)> = Vec::new();
    if let Some(list) = f.param_list() {
        let params = func.assoc_fn_params(db);
        // The written parameters in the order `assoc_fn_params` has them: `self` first.
        let written = list
            .self_param()
            .map(|it| it.syntax())
            .into_iter()
            .chain(list.params().map(|it| it.ty().map_or(it.syntax(), |ty| ty.syntax())));
        for (node, param) in written.zip(params.iter()) {
            let ty = param.ty();
            let Some(size) = large(db, ty) else { continue };
            // alloc: a message, once per finding
            let message = format!(
                "`fn {name}` takes a `{}` of {size} bytes by value, copied in at every call: {FIX}",
                ty.display(db, target)
            );
            found.push((node, message));
        }
    }
    if let Some(ret) = f.ret_type().and_then(|it| it.ty()) {
        let ty = func.ret_type(db);
        if let Some(size) = large(db, &ty) {
            // alloc: a message, once per finding
            let message = format!(
                "`fn {name}` returns a `{}` of {size} bytes by value, copied out at every call: {FIX}",
                ty.display(db, target)
            );
            found.push((ret.syntax(), message));
        }
    }
    for (node, message) in found {
        let Some(range) = cx.range(node) else { continue };
        let span = cx.span(range);
        cx.blaze(LINT, node, span, message)
            .note(format!("a move of more than {LIMIT} bytes is a `memcpy` of all of it"))
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}

/// An enum whose largest variant is over [`LIMIT`] and [`VARIANT_RATIO`] times its second.
pub(super) fn check_enum<'t>(cx: &mut Cx<'_, 't>, item: ast::Enum<'t>) {
    if !cx.on_blaze(LINT) || cx.expansion().is_some() || !cx.is_written(item.syntax()) {
        return;
    }
    let Some(hir::Adt::Enum(def)) = cx.adt_def(ast::Adt::Enum(item)) else { return };
    let db = cx.db();
    let ty = hir::Adt::Enum(def).ty(db);
    let variants = def.variants(db);
    if variants.len() < 2 {
        return;
    }
    // alloc: the sizes of one enum's variants
    let mut sizes: Vec<(u64, hir::EnumVariant)> = Vec::with_capacity(variants.len());
    for variant in variants {
        let mut size = 0;
        for (_, field) in ty.variant_fields(db, variant) {
            let Ok(layout) = field.layout(db) else { return };
            size += layout.size();
        }
        sizes.push((size, variant));
    }
    sizes.sort_by(|a, b| b.0.cmp(&a.0));
    let ((largest, big), (second, small)) = (sizes[0], sizes[1]);
    if largest <= LIMIT || largest <= VARIANT_RATIO * second {
        return;
    }
    let enum_name = def.name(db);
    let big_name = big.name(db);
    let written = item.variant_list().and_then(|list| {
        list.variants().find(|it| it.name().is_some_and(|it| it.text() == big_name.as_str()))
    });
    let node = written.map_or(item.syntax(), |it| it.syntax());
    let Some(range) = cx.range(node) else { return };
    let message = format!(
        "`{}::{}` holds {largest} bytes, over {VARIANT_RATIO}× `{}`'s {second}: every `{}` is that large: {FIX}",
        enum_name.as_str(),
        big_name.as_str(),
        small.name(db).as_str(),
        enum_name.as_str()
    );
    let span = cx.span(range);
    cx.blaze(LINT, node, span, message)
        .note(format!(
            "box the large variant's fields (`{}(Box<..>)`), so a `{}` is a pointer there and every move of one is small",
            big_name.as_str(),
            enum_name.as_str()
        ))
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}

/// The body form: a place moved by value into a call or a struct literal.
pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let body = analysis.body();
    for (id, expr) in body.exprs() {
        match expr {
            Expr::Call { args, .. } | Expr::MethodCall { args, .. } => {
                for &arg in args.iter() {
                    moved_into(cx, analysis, arg, Dest::Call(id), &mut found);
                }
            }
            Expr::RecordLit { fields, .. } => {
                for field in fields.iter() {
                    moved_into(cx, analysis, field.expr, Dest::Literal, &mut found);
                }
            }
            _ => {}
        }
    }
    found
}

/// A finding into `found` when `arg`, moved to `dest`, is a place of a large type.
fn moved_into(
    cx: &Cx<'_, '_>,
    analysis: &hir::BodyAnalysis<'_>,
    arg: ExprId,
    dest: Dest,
    found: &mut Vec<Finding>,
) {
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    if !is_place(body, arg) || infer.expr_adjustment(arg).is_some_and(|it| !it.is_empty()) {
        return;
    }
    let ty = analysis.ty(infer.expr_ty(arg));
    let Some(size) = large(db, &ty) else { return };
    let Some(range) = analysis.expr_range(arg) else { return };
    if exempt::exempt(cx.text(), range, LINT) {
        return;
    }
    let to = match dest {
        Dest::Call(call) => match super::alloc::written_callee(infer, body, call) {
            Some((f, _)) => format!("the call to `{}`", hir::Function::from(f).name(db).as_str()),
            None => "this call".to_owned(), // alloc: a message's part
        },
        Dest::Literal => "this struct literal".to_owned(), // alloc: a message's part
    };
    let target = cx.krate().to_display_target(db);
    let message =
        format!("this moves a `{}` of {size} bytes into {to}: {FIX}", ty.display(db, target));
    let note = format!("a move of more than {LIMIT} bytes is a `memcpy` of all of it");
    facts::push(found, LINT, range, message, Some(note));
}

/// Where a moved value goes.
#[derive(Clone, Copy)]
enum Dest {
    Call(ExprId),
    Literal,
}

/// Whether `expr` is a place whose value a move copies: a path, a field, an index or a deref.
fn is_place(body: &hir_def::expr_store::Body, expr: ExprId) -> bool {
    matches!(
        body.expr(expr),
        Expr::Path(_)
            | Expr::Field { .. }
            | Expr::Index { .. }
            | Expr::UnaryOp { op: UnaryOp::Deref, .. }
    )
}

#[cfg(test)]
mod tests {
    use super::super::perf_tests::{perf_only, perf_with};

    const LINT: &str = "large_move";

    const MAIN: &str = r#"
pub struct Big { bytes: [u8; 1024] }
pub struct Small { bytes: [u8; 64] }
pub fn take(b: Big) -> u8 { b.bytes[0] }
pub fn take_ref(b: &Big) -> u8 { b.bytes[0] }
pub fn shared(b: std::sync::Arc<Big>) -> u8 { b.bytes[0] }
pub fn small(s: Small) -> u8 { s.bytes[0] }
pub fn make() -> Big { Big { bytes: [0; 1024] } }
pub struct Holder { big: Big }
pub fn mover(b: &Big, c: Big) -> u8 {
    let built = take(make());
    let held = Holder { big: c };
    take_ref(b) + built
}
pub enum Message {
    Ping,
    Payload([u8; 2048]),
}
pub enum Even {
    A([u8; 1024]),
    B([u8; 1000]),
}
"#;

    #[test]
    fn large_params_returns_moves_and_variants_are_found() {
        let found = perf_only(LINT, MAIN);
        assert!(found.contains("`fn take` takes a `Big` of 1024 bytes by value"), "{found}");
        assert!(found.contains("`fn make` returns a `Big` of 1024 bytes by value"), "{found}");
        assert!(found.contains("`fn mover` takes a `Big` of 1024 bytes by value"), "{found}");
        assert!(
            found.contains("this moves a `Big` of 1024 bytes into this struct literal"),
            "{found}"
        );
        assert!(
            !found.contains("into the call to `take`"),
            "a call's result is built in the argument's slot, not moved:\n{found}"
        );
        assert!(
            found.contains("`Message::Payload` holds 2048 bytes, over 4× `Ping`'s 0"),
            "{found}"
        );
        assert!(!found.contains("`Even::"), "variants of like size are not found:\n{found}");
        assert!(!found.contains("`fn take_ref`"), "{found}");
        assert!(!found.contains("`fn shared`"), "an `Arc` is a pointer:\n{found}");
        assert!(!found.contains("`fn small`"), "{found}");
        assert!(found.contains("pass by reference or box the large variant"), "{found}");
    }

    #[test]
    fn large_move_is_in_blaze_hot_and_off_elsewhere() {
        let hot = perf_with("--deny=blaze::hot", MAIN);
        assert!(hot.contains("`fn take` takes a `Big` of 1024 bytes by value"), "{hot}");
        let cold = perf_with("", MAIN);
        assert!(!cold.contains("bytes by value"), "{cold}");
    }
}
