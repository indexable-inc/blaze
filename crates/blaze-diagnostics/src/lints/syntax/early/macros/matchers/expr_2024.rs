//! `edition_2024_expr_fragment_specifier`: an `expr` specifier, written before 2024, that will
//! accept more expressions in 2024.
//!
//! Follows rustc's `compiler/rustc_lint/src/macro_expr_fragment_specifier_2024_migration.rs`
//! (`check_tokens`, `check_ident_token`) (MIT or Apache-2.0).

use span::Edition;
use syntax::TextRange;

use super::tokens::{Ids, Kind, Punct, Tokens, Tt};

/// The `expr`s of `$x:expr` in the body of a definition (matchers and transcribers alike), each
/// in its token's edition.
pub(super) fn exprs(tokens: &Tokens<'_>) -> Vec<TextRange> {
    // alloc: empty until a lint fires; holds only what fires
    let mut out = Vec::new();
    walk(tokens, tokens.top, &mut out);
    out
}

/// `check_tokens`: `$`, an identifier, `:` and the specifier, as rustc's token cursor sees them
/// (a group is one tree, after which nothing is pending).
fn walk(tokens: &Tokens<'_>, tts: Ids, out: &mut Vec<TextRange>) {
    let (mut dollar, mut ident, mut colon) = (false, false, false);
    for tt in tokens.get(tts) {
        match tt {
            Tt::Token(token) => {
                if token.is(Punct::Dollar) {
                    dollar = true;
                    continue;
                }
                if let Kind::Ident { .. } = token.kind {
                    if dollar && ident && colon {
                        if token.edition < Edition::Edition2024 && tokens.name(token.range) == "expr" {
                            out.push(token.range);
                        }
                    } else if dollar {
                        ident = true;
                        continue;
                    }
                }
                if token.is(Punct::Colon) && dollar && ident {
                    colon = true;
                    continue;
                }
            }
            Tt::Delimited(group) => walk(tokens, group.tts, out),
        }
        (dollar, ident, colon) = (false, false, false);
    }
}

#[cfg(test)]
mod tests {
    use syntax::{
        SourceFile,
        view::{self as ast, AstNode as _},
    };

    use super::*;

    fn linted(text: &str, edition: Edition) -> Vec<String> {
        let file = SourceFile::parse(text, edition);
        let def = file.root().descendants().find_map(ast::Macro::cast).unwrap();
        let tokens = Tokens::of(def, &|_| (edition, true)).unwrap();
        exprs(&tokens).into_iter().map(|it| format!("{:?}", it)).collect()
    }

    #[test]
    fn each_expr_before_2024() {
        let text = "macro_rules! m { ($e:expr) => { $e }; ($($x:expr),*) => { 0 }; }";
        let at = |needle: &str| {
            let start = text.find(needle).unwrap() + needle.len() - 4;
            format!("{start}..{}", start + 4)
        };
        assert_eq!(linted(text, Edition::Edition2021), [at("($e:expr"), at("($($x:expr")]);
        assert!(linted(text, Edition::Edition2024).is_empty());
    }

    #[test]
    fn only_dollar_ident_colon_expr() {
        assert!(linted("macro_rules! m { ($e: $f:ty) => { e:expr }; }", Edition::Edition2021).is_empty());
        assert!(linted("macro_rules! m { ($e:expr_2021) => {}; }", Edition::Edition2021).is_empty());
    }
}
