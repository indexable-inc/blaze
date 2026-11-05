//! The parts of `compiler/rustc_ast_pretty/src/pprust/{mod,state}.rs` at rust-lang/rust 12433c307
//! (MIT or Apache-2.0) that rustfmt calls: paths and tokens printed exactly as rustc prints them.

use std::borrow::Cow;

use crate::ast;
use crate::ast::token::{self as tk, IdentKind, Token, TokenKind};
use crate::span::{Ident, Symbol, kw};

pub(crate) fn path_to_string(p: &ast::Path) -> String {
    let mut out = String::new();
    for (i, segment) in p.segments.iter().enumerate() {
        if i > 0 {
            out.push_str("::");
        }
        print_path_segment(&mut out, segment);
    }
    out
}

pub(crate) fn path_segment_to_string(p: &ast::PathSegment) -> String {
    let mut out = String::new();
    print_path_segment(&mut out, p);
    out
}

/// rustfmt only prints the paths of attributes, macro calls and `pub(in ..)`, which rustc parses
/// in module style, so a segment never carries generic arguments here.
fn print_path_segment(out: &mut String, segment: &ast::PathSegment) {
    if segment.ident.name != kw::PathRoot {
        out.push_str(&print_ident(segment.ident));
        assert!(
            segment.args.is_none(),
            "rustfmt prints only module-style paths"
        );
    }
}

fn print_ident(ident: Ident) -> String {
    if ident.is_raw_guess() {
        format!("r#{}", ident.name)
    } else {
        ident.name.to_string()
    }
}

fn ident_kind_to_string(name: Symbol, kind: IdentKind) -> String {
    match kind {
        IdentKind::Normal => name.to_string(),
        IdentKind::Raw => format!("r#{name}"),
        IdentKind::ForcedKeyword => format!("k#{name}"),
    }
}

fn lifetime_kind_to_string(name: Symbol, kind: IdentKind) -> String {
    match kind {
        IdentKind::Normal => name.to_string(),
        IdentKind::Raw => {
            let s = name
                .as_str()
                .strip_prefix('\'')
                .expect("only lifetime idents should be passed with RawLifetime mode");
            format!("'r#{s}")
        }
        IdentKind::ForcedKeyword => unreachable!(),
    }
}

/// Print the token precisely, without converting `$crate` into its respective crate name.
pub(crate) fn token_to_string(token: &Token) -> Cow<'static, str> {
    token_kind_to_string(&token.kind)
}

pub(crate) fn token_kind_to_string(tok: &TokenKind) -> Cow<'static, str> {
    match *tok {
        tk::Eq => "=".into(),
        tk::Lt => "<".into(),
        tk::Le => "<=".into(),
        tk::EqEq => "==".into(),
        tk::Ne => "!=".into(),
        tk::Ge => ">=".into(),
        tk::Gt => ">".into(),
        tk::Bang => "!".into(),
        tk::Tilde => "~".into(),
        tk::OrOr => "||".into(),
        tk::AndAnd => "&&".into(),
        tk::Plus => "+".into(),
        tk::Minus => "-".into(),
        tk::Star => "*".into(),
        tk::Slash => "/".into(),
        tk::Percent => "%".into(),
        tk::Caret => "^".into(),
        tk::And => "&".into(),
        tk::Or => "|".into(),
        tk::Shl => "<<".into(),
        tk::Shr => ">>".into(),
        tk::PlusEq => "+=".into(),
        tk::MinusEq => "-=".into(),
        tk::StarEq => "*=".into(),
        tk::SlashEq => "/=".into(),
        tk::PercentEq => "%=".into(),
        tk::CaretEq => "^=".into(),
        tk::AndEq => "&=".into(),
        tk::OrEq => "|=".into(),
        tk::ShlEq => "<<=".into(),
        tk::ShrEq => ">>=".into(),

        /* Structural symbols */
        tk::At => "@".into(),
        tk::Dot => ".".into(),
        tk::DotDot => "..".into(),
        tk::DotDotDot => "...".into(),
        tk::DotDotEq => "..=".into(),
        tk::Comma => ",".into(),
        tk::Semi => ";".into(),
        tk::Colon => ":".into(),
        tk::PathSep => "::".into(),
        tk::RArrow => "->".into(),
        tk::LArrow => "<-".into(),
        tk::FatArrow => "=>".into(),
        tk::OpenParen => "(".into(),
        tk::CloseParen => ")".into(),
        tk::OpenBracket => "[".into(),
        tk::CloseBracket => "]".into(),
        tk::OpenBrace => "{".into(),
        tk::CloseBrace => "}".into(),
        tk::OpenInvisible(_) | tk::CloseInvisible(_) => "".into(),
        tk::Pound => "#".into(),
        tk::Dollar => "$".into(),
        tk::Question => "?".into(),
        tk::SingleQuote => "'".into(),

        /* Literals */
        tk::Literal(lit) => literal_to_string(lit).into(),

        /* Name components */
        tk::Ident(name, kind) => ident_kind_to_string(name, kind).into(),
        tk::NtIdent(ident, kind) => ident_kind_to_string(ident.name, kind).into(),

        tk::Lifetime(name, kind) | tk::NtLifetime(Ident { name, .. }, kind) => {
            lifetime_kind_to_string(name, kind).into()
        }

        /* Other */
        tk::DocComment(comment_kind, attr_style, data) => {
            doc_comment_to_string(comment_kind, attr_style, data).into()
        }
        tk::Eof => "<eof>".into(),
    }
}

fn doc_comment_to_string(
    comment_kind: tk::CommentKind,
    attr_style: ast::AttrStyle,
    data: Symbol,
) -> String {
    match (comment_kind, attr_style) {
        (tk::CommentKind::Line, ast::AttrStyle::Outer) => format!("///{data}"),
        (tk::CommentKind::Line, ast::AttrStyle::Inner) => format!("//!{data}"),
        (tk::CommentKind::Block, ast::AttrStyle::Outer) => format!("/**{data}*/"),
        (tk::CommentKind::Block, ast::AttrStyle::Inner) => format!("/*!{data}*/"),
    }
}

fn literal_to_string(lit: tk::Lit) -> String {
    let tk::Lit {
        kind,
        symbol,
        suffix,
    } = lit;
    let mut out = match kind {
        tk::Byte => format!("b'{symbol}'"),
        tk::Char => format!("'{symbol}'"),
        tk::Str => format!("\"{symbol}\""),
        tk::StrRaw(n) => {
            format!(
                "r{delim}\"{string}\"{delim}",
                delim = "#".repeat(n as usize),
                string = symbol
            )
        }
        tk::ByteStr => format!("b\"{symbol}\""),
        tk::ByteStrRaw(n) => {
            format!(
                "br{delim}\"{string}\"{delim}",
                delim = "#".repeat(n as usize),
                string = symbol
            )
        }
        tk::CStr => format!("c\"{symbol}\""),
        tk::CStrRaw(n) => {
            format!(
                "cr{delim}\"{symbol}\"{delim}",
                delim = "#".repeat(n as usize)
            )
        }
        tk::Integer | tk::Float | tk::Bool | tk::Err(_) => symbol.to_string(),
    };

    if let Some(suffix) = suffix {
        out.push_str(suffix.as_str())
    }

    out
}
