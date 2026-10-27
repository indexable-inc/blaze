//! The program rustdoc makes of an example: the example's own crate attributes first, then
//! `extern crate` for the documented crate when the example names it, then the rest inside a
//! `main`, unless the example has one.

use syntax::{
    AstNode, Edition, SyntaxKind, TextSize,
    ast::{self, HasName},
};

/// What goes around an example that is the same for every example of a crate.
pub(crate) struct Frame<'a> {
    pub(crate) crate_name: &'a str,
    /// The attributes `#![doc(test(attr(..)))]` of the crate asks every example to have.
    pub(crate) attrs: &'a [String],
    /// Whether `#![doc(test(no_crate_inject))]` asks for no `extern crate`.
    pub(crate) no_crate_inject: bool,
}

/// The source of the program. Its lines are numbered from its own first, as rustdoc has rustc
/// number them for `line!()` and a panic's place.
pub(crate) struct Program {
    pub(crate) source: String,
}

/// An example taken apart into what goes where in its program.
#[derive(Default)]
struct Parts {
    crate_attrs: String,
    extern_crates: String,
    rest: String,
    has_main: bool,
    names_the_crate: bool,
}

const WRAP: &str = "fn f(){";

impl Parts {
    fn of(code: &str, crate_name: &str, edition: Edition) -> Parts {
        // Statements are only read inside a function.
        let wrapped = format!("{WRAP}{code}\n}}");
        let file = ast::SourceFile::parse(&wrapped, edition).tree();
        let whole = TextSize::of(&*wrapped);
        let body = file
            .syntax()
            .children()
            .find_map(ast::Fn::cast)
            .filter(|it| it.syntax().text_range().end() == whole)
            .and_then(|it| it.body())
            .and_then(|it| it.stmt_list());
        // Braces that do not pair up: all of it is the body of `main`, for rustc to refuse.
        let Some(body) = body else { return Parts { rest: code.to_owned(), ..Parts::default() } };
        let mut parts = Parts::default();
        let mut taken = 0;
        let mut in_attrs = true;
        for node in body.syntax().children() {
            let end = usize::from(node.text_range().end()) - WRAP.len();
            let text = &code[taken..end.min(code.len())];
            taken = end.min(code.len());
            let is_inner_attr =
                ast::Attr::cast(node.clone()).is_some_and(|it| it.excl_token().is_some());
            in_attrs &= is_inner_attr;
            if in_attrs {
                parts.crate_attrs.push_str(text);
            } else if let Some(it) = ast::ExternCrate::cast(node.clone()) {
                parts.names_the_crate |= it
                    .name_ref()
                    .is_some_and(|name| name.text().trim_start_matches("r#") == crate_name);
                parts.extern_crates.push_str(text);
            } else {
                parts.has_main |= is_main(&node);
                parts.rest.push_str(text);
            }
        }
        parts.rest.push_str(&code[taken..]);
        parts
    }
}

/// Whether the statement is `fn main`, or a macro call that may write one.
fn is_main(node: &syntax::SyntaxNode) -> bool {
    if let Some(function) = ast::Fn::cast(node.clone()) {
        return function.name().is_some_and(|it| it.text() == "main");
    }
    let call = node.descendants().find(|it| it.kind() == SyntaxKind::MACRO_CALL);
    let is_call = matches!(
        node.kind(),
        SyntaxKind::MACRO_CALL | SyntaxKind::EXPR_STMT | SyntaxKind::MACRO_EXPR
    );
    is_call
        && call.is_some_and(|it| {
            let words: Vec<String> = it
                .descendants_with_tokens()
                .filter_map(|it| it.into_token())
                .filter(|it| !it.kind().is_trivia())
                .map(|it| it.text().to_owned())
                .collect();
            words.windows(2).any(|it| it[0] == "fn" && it[1] == "main")
        })
}

impl Program {
    /// The program of the example `code`, whose `main` is named after `id` when rustdoc
    /// writes one.
    pub(crate) fn of(code: &str, frame: &Frame<'_>, id: &str, edition: Edition) -> Program {
        let parts = Parts::of(code, frame.crate_name, edition);
        let rest = parts.rest.trim();
        let mut source = String::new();
        if frame.attrs.is_empty() {
            source.push_str("#![allow(unused)]\n");
        }
        for attr in frame.attrs {
            source.push_str(&format!("#![{attr}]\n"));
        }
        let push_lines = |source: &mut String, text: &str| {
            if !text.is_empty() {
                source.push_str(text);
                if !text.ends_with('\n') {
                    source.push('\n');
                }
            }
        };
        push_lines(&mut source, &parts.crate_attrs);
        let injects = !parts.names_the_crate
            && !frame.no_crate_inject
            && frame.crate_name != "std"
            && code.contains(frame.crate_name);
        if injects {
            source.push_str("#[allow(unused_extern_crates)]\n");
            source.push_str(&format!("extern crate r#{};\n", frame.crate_name));
        }
        push_lines(&mut source, &parts.extern_crates);
        if parts.has_main || source.contains("![no_std]") {
            source.push_str(rest);
            return Program { source };
        }
        let name = format!("_doctest_main_{id}");
        let (returns, called) = match rest.ends_with("(())") {
            true => (" -> core::result::Result<(), impl core::fmt::Debug>", ".unwrap()"),
            false => ("", ""),
        };
        source.push_str(&format!(
            "fn main() {{ #[allow(non_snake_case)] fn {name}(){returns} {{\n{rest}\n}} {name}(){called} }}"
        ));
        Program { source }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(code: &str) -> String {
        let frame = Frame { crate_name: "app", attrs: &[], no_crate_inject: false };
        Program::of(code, &frame, "src_lib_rs_3_0", Edition::Edition2021).source
    }

    #[test]
    fn attributes_alone_have_a_main() {
        assert_eq!(
            program("#![allow(dead_code)]"),
            "#![allow(unused)]\n#![allow(dead_code)]\n\
             fn main() { #[allow(non_snake_case)] fn _doctest_main_src_lib_rs_3_0() {\n\n} _doctest_main_src_lib_rs_3_0() }"
        );
    }

    #[test]
    fn statements_go_into_a_main() {
        assert_eq!(
            program("assert_eq!(app::double(1), 2);"),
            "#![allow(unused)]\n#[allow(unused_extern_crates)]\nextern crate r#app;\n\
             fn main() { #[allow(non_snake_case)] fn _doctest_main_src_lib_rs_3_0() {\n\
             assert_eq!(app::double(1), 2);\n} _doctest_main_src_lib_rs_3_0() }"
        );
    }

    #[test]
    fn a_main_of_its_own_stays() {
        assert_eq!(
            program("use app::double;\nfn main() {\n    println!(\"printed {}\", double(5));\n}"),
            "#![allow(unused)]\n#[allow(unused_extern_crates)]\nextern crate r#app;\n\
             use app::double;\nfn main() {\n    println!(\"printed {}\", double(5));\n}"
        );
    }

    #[test]
    fn a_result_is_unwrapped() {
        assert_eq!(
            program("let n: u32 = \"7\".parse()?;\nOk::<(), std::num::ParseIntError>(())"),
            "#![allow(unused)]\n\
             fn main() { #[allow(non_snake_case)] fn _doctest_main_src_lib_rs_3_0() -> core::result::Result<(), impl core::fmt::Debug> {\n\
             let n: u32 = \"7\".parse()?;\nOk::<(), std::num::ParseIntError>(())\n} _doctest_main_src_lib_rs_3_0().unwrap() }"
        );
    }

    #[test]
    fn attributes_and_extern_crates_stay_outside() {
        assert_eq!(
            program(
                "#![allow(dead_code)]\nextern crate app as other;\nlet mut c = other::Counter { n: 1 };\nc.bump();"
            ),
            "#![allow(unused)]\n#![allow(dead_code)]\n\nextern crate app as other;\n\
             fn main() { #[allow(non_snake_case)] fn _doctest_main_src_lib_rs_3_0() {\n\
             let mut c = other::Counter { n: 1 };\nc.bump();\n} _doctest_main_src_lib_rs_3_0() }"
        );
    }
}
