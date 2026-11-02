//! `async_fn_in_trait`: an `async fn` of a trait another crate can reach, unless
//! `return_type_notation` is on, with the suggestion to write it as a `fn` returning
//! `impl Future + Send`, its body in an `async` block.
//!
//! Follows `compiler/rustc_lint/src/async_fn_in_trait.rs` and
//! `compiler/rustc_trait_selection/src/error_reporting/traits/suggestions.rs`
//! (`suggest_desugaring_async_fn_to_impl_future_in_trait`) (MIT or Apache-2.0).

use syntax::{
    NodeOrToken, TextRange, TextSize,
    view::{self as ast, AstNode},
};

use super::{
    Krate, visibility,
    walk::{Container, Item},
};
use crate::lints::names::effective::Effective;
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(super) fn async_fn<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, effective: &Effective, krate: &Krate) -> Option<Found> {
    let (ast::Item::Fn(f), Container::Trait(_)) = (item.node.value, item.container) else { return None };
    let async_token = f.async_token()?;
    if krate.return_type_notation {
        return None;
    }
    if !visibility::def(cx, item).is_some_and(|it| effective.is_reachable(it)) {
        return None;
    }
    let span = |range: TextRange| cx.span(item.node.with_value(range));
    let async_range = async_token.text_range();
    let mut async_end = async_range.end();
    let mut next = async_token.next_sibling_or_token();
    while let Some(NodeOrToken::Token(token)) = next {
        if token.kind() != syntax::SyntaxKind::WHITESPACE {
            break;
        }
        async_end = token.text_range().end();
        next = token.next_sibling_or_token();
    }
    // alloc: the suggestion of a lint that fires, with its parts
    let mut sugg = Vec::with_capacity(5);
    let removed = (span(TextRange::new(async_range.start(), async_end)), String::new()); // alloc: empty, no allocation
    match f.ret_type().and_then(|it| it.ty()) {
        None => {
            let end = f.param_list().map_or(async_range.end(), |it| it.syntax().text_range().end());
            sugg.push(removed);
            // alloc: a suggestion of a lint that fires
            sugg.push((span(TextRange::empty(end)), " -> impl std::future::Future<Output = ()> + Send".to_owned()));
        }
        Some(ty) => {
            let range = ty.syntax().text_range();
            // alloc: a suggestion of a lint that fires
            sugg.push((span(TextRange::empty(range.start())), "impl std::future::Future<Output = ".to_owned()));
            sugg.push((span(TextRange::empty(range.end())), "> + Send".to_owned())); // alloc: a suggestion of a lint that fires
            sugg.push(removed);
        }
    }
    if let Some(body) = f.body() {
        let range = body.syntax().text_range();
        let inner = TextRange::new(range.start() + TextSize::from(1), range.end() - TextSize::from(1));
        if inner.is_empty() {
            sugg.push((span(inner), " async {} ".to_owned())); // alloc: a suggestion of a lint that fires
        } else {
            sugg.push((span(TextRange::empty(inner.start())), "async {".to_owned())); // alloc: a suggestion of a lint that fires
            sugg.push((span(TextRange::empty(inner.end())), "} ".to_owned())); // alloc: a suggestion of a lint that fires
        }
    }
    let message = "use of `async fn` in public traits is discouraged as auto trait bounds cannot be specified";
    let mut diag = Diag::lint("async_fn_in_trait", message, span(async_range));
    diag.note("you can suppress this lint if you plan to use the trait only in your own code, or do not care about auto traits like `Send` on the `Future`");
    diag.multipart_suggestion(
        "you can alternatively desugar to a normal `fn` that returns `impl Future` and add any desired bounds such as `Send`, but these cannot be relaxed without a breaking API change",
        sugg,
        Applicability::MaybeIncorrect,
    );
    if let Some((id, name)) = super::item_id(cx, item) {
        // alloc: a lint that fires
        diag.fact(blaze_items::Fact::Signature(cx.key(id)), format!("the signature of `{name}`"));
    }
    Some(Found::lint(diag, Phase::Late, item.ptr()))
}
