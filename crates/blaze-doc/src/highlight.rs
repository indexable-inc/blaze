//! Rust code as HTML with its tokens classed for color: rust-analyzer's lexer, and the classes
//! rustdoc's stylesheet names (`kw`, `string`, `comment`, `macro`, ...).

use parser::{Edition, LexedStr, SyntaxKind, T};

use crate::html::escape;

/// `code` as HTML, each token that has a color in a `<span>`.
pub(crate) fn rust(code: &str) -> String {
    let lexed = LexedStr::new(Edition::CURRENT, code);
    let mut out = String::with_capacity(code.len() * 2);
    let mut attr_depth: Option<i32> = None;
    let count = lexed.len();
    let next_significant =
        |from: usize| (from..count).map(|at| lexed.kind(at)).find(|kind| !kind.is_trivia());
    for at in 0..count {
        let kind = lexed.kind(at);
        let text = lexed.text(at);
        if kind == SyntaxKind::EOF {
            break;
        }
        if attr_depth.is_none() && kind == T![#] {
            let mut next = at + 1;
            if lexed.kind(next) == T![!] {
                next += 1;
            }
            if next < count && lexed.kind(next) == T!['['] {
                attr_depth = Some(0);
                out.push_str("<span class=\"attr\">");
            }
        }
        if let Some(depth) = attr_depth.as_mut() {
            match kind {
                T!['['] => *depth += 1,
                T![']'] => *depth -= 1,
                _ => {}
            }
            out.push_str(&escape(text));
            if *depth == 0 && kind == T![']'] {
                out.push_str("</span>");
                attr_depth = None;
            }
            continue;
        }
        let class = match kind {
            SyntaxKind::COMMENT if text.starts_with("///") || text.starts_with("//!") => {
                Some("doccomment")
            }
            SyntaxKind::COMMENT if text.starts_with("/**") || text.starts_with("/*!") => {
                Some("doccomment")
            }
            SyntaxKind::COMMENT => Some("comment"),
            SyntaxKind::STRING
            | SyntaxKind::BYTE_STRING
            | SyntaxKind::C_STRING
            | SyntaxKind::CHAR
            | SyntaxKind::BYTE => Some("string"),
            SyntaxKind::INT_NUMBER | SyntaxKind::FLOAT_NUMBER => Some("number"),
            SyntaxKind::LIFETIME_IDENT => Some("lifetime"),
            T![true] | T![false] => Some("bool-val"),
            T![self] | T![Self] => Some("self"),
            T![mut] | T![&] => Some("kw-2"),
            T![?] => Some("question-mark"),
            SyntaxKind::IDENT if next_significant(at + 1) == Some(T![!]) => Some("macro"),
            SyntaxKind::IDENT => match text {
                "Option" | "Result" | "String" | "Vec" | "Box" => Some("prelude-ty"),
                "Some" | "None" | "Ok" | "Err" => Some("prelude-val"),
                _ => None,
            },
            kind if kind.is_keyword(Edition::CURRENT) => Some("kw"),
            _ => None,
        };
        match class {
            Some(class) => {
                out.push_str("<span class=\"");
                out.push_str(class);
                out.push_str("\">");
                out.push_str(&escape(text));
                out.push_str("</span>");
            }
            None => out.push_str(&escape(text)),
        }
    }
    if attr_depth.is_some() {
        out.push_str("</span>");
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn classes_keywords_strings_and_macros() {
        let html = super::rust("fn main() { println!(\"hi\"); }");
        assert!(html.contains("<span class=\"kw\">fn</span>"), "{html}");
        assert!(html.contains("<span class=\"macro\">println</span>"), "{html}");
        assert!(html.contains("<span class=\"string\">&quot;hi&quot;</span>"), "{html}");
    }
}
