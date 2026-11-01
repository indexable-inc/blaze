//! `large_stack_arrays`: an array expression (`[x; N]` or `[a, b, ..]`) of a function's body
//! larger than its limit, made on the stack where a fiber has little of it.
//!
//! Follows clippy's `clippy_lints/src/large_stack_arrays.rs` (MIT or Apache-2.0), read from
//! indexable-inc's fork at c2308e38 (nightly-2026-05-27), the nearest checkout to the pinned
//! nightly's (rust-lang/rust 12433c30). As in clippy: an array expression written in the file,
//! of a function's or closure's body, not of a `const` or `static` (or a `const` block), whose
//! type's layout is larger than the limit; an array a macro makes up (`vec![a, b]`'s) is not
//! written in the file and is left alone. Unlike clippy, whose `array-size-threshold` is a
//! number of its own (16 KiB), the limit is derived from the stack a pool fiber runs on
//! ([`FIBER_STACK`]): 1/1024 of it, so a body can make a few such
//! arrays in a deep chain of calls; `array-size-threshold` in `clippy.toml` still sets it. The
//! size is the layout the front end computes; an array whose layout it can't compute (a length
//! or an element of a generic parameter) is left alone, never taken as small. The message names
//! the function and the stack the limit comes from.

use syntax::{
    SyntaxKind as K,
    view::{self as ast, AstNode as _, HasName as _},
};

use super::{Implemented, unwrap_in_result::written_ancestors};
use crate::check::Cx;

const LINT: Implemented = Implemented::LargeStackArrays;

/// The stack a pool fiber runs on above its guard page: `blaze_graph::sched::FIBER_STACK`.
/// blaze-lints has blaze-graph only as a dev-dependency; a test holds the two equal.
pub(super) const FIBER_STACK: u64 = 16 << 20;
/// Where [`FIBER_STACK`] is defined, as the messages name it.
pub(super) const FIBER_STACK_NAME: &str = "blaze_graph::sched::FIBER_STACK";

/// `size` bytes as a message says it: in MiB or KiB when it is a whole number of them.
pub(super) fn size_text(size: u64) -> String {
    match size {
        0 => "0 bytes".to_owned(),
        _ if size % (1 << 20) == 0 => format!("{} MiB", size >> 20),
        _ if size % (1 << 10) == 0 => format!("{} KiB", size >> 10),
        _ => format!("{size} bytes"),
    }
}

/// The share of a fiber's stack one array may take.
const SHARE: u64 = 1024;

/// The limit when `clippy.toml` sets none.
pub(super) const DEFAULT_LIMIT: u64 = FIBER_STACK / SHARE;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, expr: ast::Expr<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let ast::Expr::ArrayExpr(_) = expr else { return };
    if !cx.is_written(expr.syntax()) {
        return;
    }
    let Some(owner) = owner(cx, expr.syntax()) else { return };
    let Some(ty) = cx.ty(expr) else { return };
    let db = cx.db();
    if ty.as_array_element(db).is_none() {
        return;
    }
    let Ok(layout) = ty.layout(db) else { return };
    let size = layout.size();
    let configured = cx.conf().int("array-size-threshold", -1);
    let limit = u64::try_from(configured).unwrap_or(DEFAULT_LIMIT);
    if size <= limit {
        return;
    }
    let Some(range) = cx.range(expr.syntax()) else { return };
    let span = cx.span(range);
    let snippet = cx.snippet(range).to_owned();
    let why = match configured {
        0.. => format!("`array-size-threshold` in `clippy.toml` ({})", size_text(limit)),
        _ => format!(
            "{}: 1/{SHARE} of a pool fiber's stack (`{FIBER_STACK_NAME}`, {})",
            size_text(limit),
            size_text(FIBER_STACK)
        ),
    };
    let message = format!("this array in {owner} takes {} of the stack, over {why}", size_text(size));
    cx.lint(LINT, expr.syntax(), span, message)
        .help(format!("make it on the heap: `vec!{snippet}.into_boxed_slice()`, or keep it in a `static`"))
        .note("a fiber's stack is fixed: what one frame takes, every frame it calls has less of");
}

/// The function or closure the array is made in, as the message names it; `None` when it is in
/// a `const`, a `static` or a `const` block, whose value is not made on a stack.
fn owner<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> Option<String> {
    for it in written_ancestors(cx, node).skip(1) {
        match it.kind() {
            K::CONST | K::STATIC | K::CONST_ARG => return None,
            K::BLOCK_EXPR if ast::BlockExpr::cast(it).is_some_and(|it| it.const_token().is_some()) => return None,
            K::CLOSURE_EXPR => {
                let f = written_ancestors(cx, it).find_map(ast::Fn::cast).and_then(|f| f.name());
                return Some(match f {
                    Some(name) => format!("a closure in `{}`", name.text()),
                    None => "a closure".to_owned(),
                });
            }
            K::FN => {
                let name = ast::Fn::cast(it).and_then(|f| f.name());
                return Some(name.map_or_else(|| "this function".to_owned(), |it| format!("`{}`", it.text())));
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::super::more_rules_tests::lint_only;

    const LINT: &str = "large_stack_arrays";
    const FOUND: &str = "of the stack, over";

    #[test]
    fn the_limit_and_sizes_are_as_messages_say() {
        assert_eq!(super::FIBER_STACK, 16 << 20);
        assert_eq!(super::DEFAULT_LIMIT, 16 << 10);
        assert_eq!(super::size_text(1 << 20), "1 MiB");
        assert_eq!(super::size_text(20000), "20000 bytes");
    }

    #[test]
    fn an_array_over_the_fibers_share_is_found_and_one_under_it_or_in_a_static_is_not() {
        let found = lint_only(
            LINT,
            r#"
pub static TABLE: [u8; 1 << 20] = [0; 1 << 20];
pub const SMALL: [u8; 4] = [0; 4];
pub fn big() -> u8 {
    let buf = [0u8; 1 << 20];
    buf[0]
}
pub fn listed() -> u32 {
    let words = [[0u32; 4096], [1u32; 4096]];
    words[0][0]
}
pub fn small() -> u8 {
    let buf = [0u8; 16384];
    buf[0]
}
pub fn in_closure() -> u8 {
    let f = || [7u8; 20000][0];
    f()
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 3, "{found}");
        assert!(found.contains("this array in `big` takes 1 MiB of the stack, over 16 KiB: 1/1024 of a pool fiber's stack (`blaze_graph::sched::FIBER_STACK`, 16 MiB)"), "{found}");
        assert!(found.contains("this array in `listed` takes 32 KiB"), "{found}");
        assert!(found.contains("this array in a closure in `in_closure` takes 20000 bytes"), "{found}");
        assert!(found.contains("`vec![0u8; 1 << 20].into_boxed_slice()`"), "{found}");
    }

    #[test]
    fn an_array_of_a_generic_length_is_left_alone_not_taken_as_small() {
        let found = lint_only(
            LINT,
            r#"
pub fn generic<const N: usize>() -> u8 {
    let buf = [0u8; N];
    buf[0]
}
pub fn of_t<T: Copy>(x: T) -> T {
    let buf = [x; 100000];
    buf[0]
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 0, "a layout the front end can't compute is no finding:\n{found}");
    }

    #[test]
    fn an_array_handed_to_a_macro_and_one_through_a_renamed_type_are_found() {
        let found = lint_only(
            LINT,
            r#"
mod words { pub type Page = [u64; 8192]; }
use words::Page as Block;
macro_rules! stmt { ($($t:tt)*) => { $($t)* }; }
pub fn f() -> u64 {
    stmt! { let a = [0u64; 8192]; }
    let b: Block = [1; 8192];
    a[0] + b[0]
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 2, "{found}");
        assert!(found.contains("this array in `f` takes 64 KiB"), "{found}");
    }

    #[test]
    fn array_size_threshold_in_clippy_toml_sets_the_limit() {
        let conf = crate::conf::Conf { table: "array-size-threshold = 64".parse().unwrap(), ..Default::default() };
        let found = crate::tests::lint_with(
            &super::super::more_rules_tests::fixture("pub fn f() -> u8 { let buf = [0u8; 100]; buf[0] }\n"),
            "-A clippy::all -W clippy::large_stack_arrays",
            conf,
        );
        assert!(found.contains("this array in `f` takes 100 bytes of the stack, over `array-size-threshold` in `clippy.toml` (64 bytes)"), "{found}");
    }

    #[test]
    fn fiber_stack_is_the_schedulers() {
        assert_eq!(super::FIBER_STACK, blaze_graph::sched::FIBER_STACK as u64);
        assert_eq!(super::FIBER_STACK_NAME, "blaze_graph::sched::FIBER_STACK");
    }
}
