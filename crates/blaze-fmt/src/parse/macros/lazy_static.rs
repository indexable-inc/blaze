// Follows `src/tools/rustfmt/src/parse/macros/lazy_static.rs` at rust-lang/rust 12433c307 (MIT or Apache-2.0).

use crate::ast;
use crate::ast::token;
use crate::ast::tokenstream::TokenStream;
use crate::parse::stream::{FollowedByType, exp};
use crate::span::symbol;

use crate::rewrite::RewriteContext;

pub(crate) fn parse_lazy_static(
    context: &RewriteContext<'_>,
    ts: &TokenStream,
) -> Option<Vec<(ast::Visibility, symbol::Ident, Box<ast::Ty>, Box<ast::Expr>)>> {
    let mut result = vec![];
    let mut parser = super::build_parser(context, ts);
    macro_rules! parse_or {
        ($method:ident $(,)* $($arg:expr),* $(,)*) => {
            match parser.$method($($arg,)*) {
                Ok(val) => {
                    if parser.dcx().has_errors().is_some() {
                        parser.dcx().reset_err_count();
                        return None;
                    } else {
                        val
                    }
                }
                Err(err) => {
                    err.cancel();
                    parser.dcx().reset_err_count();
                    return None;
                }
            }
        }
    }
    while parser.token.kind != token::Eof {
        // Parse a `lazy_static!` item.
        // FIXME: These `eat_*` calls should be converted to `parse_or` to avoid
        // silently formatting malformed lazy-statics.
        let vis = parse_or!(parse_visibility, FollowedByType::No);
        let _ = parser.eat_keyword(exp!(Static));
        let _ = parser.eat_keyword(exp!(Ref));
        let id = parse_or!(parse_ident);
        let _ = parser.eat(exp!(Colon));
        let ty = parse_or!(parse_ty);
        let _ = parser.eat(exp!(Eq));
        let expr = parse_or!(parse_expr);
        let _ = parser.eat(exp!(Semi));
        result.push((vis, id, ty, expr));
    }

    Some(result)
}
