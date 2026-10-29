//! The `main` of a test executable, as rustc's `--test` writes it: a description of each
//! `#[test]` and `#[bench]` of the crate for libtest, and `test_main_env_args` over them. blaze adds it to
//! the root file of a crate it builds with `cfg(test)`, as a module of its own, and the
//! program starts there instead of in rustc's: a test added, removed or marked otherwise is
//! a build like another.

use std::fmt::Write as _;

use ide_db::RootDatabase;
use syntax::{
    AstNode as _, AstToken as _,
    ast::{self, HasAttrs as _, HasName as _},
};

/// The module's name in the crate's root.
pub(crate) const MODULE: &str = "__blaze_test_harness";

/// What the module starts with, after the text of the root file.
const MARK: &str = "\n// blaze's test harness, which is not in the file on disk.\n";

/// Where the module, or a proc-macro crate's [`crate::macro_list`] written the same way,
/// starts in `text`.
fn written_at(text: &str) -> Option<usize> {
    [MARK, crate::macro_list::MARK].iter().filter_map(|it| text.find(it)).min()
}

/// The name of the module at the end of `text`, this one or a [`crate::macro_list`].
pub(crate) fn appended(text: &str) -> Option<&'static str> {
    let at = written_at(text)?;
    Some(if text[at..].starts_with(MARK) { MODULE } else { crate::macro_list::MODULE })
}

/// The text of the root file without the module.
pub(crate) fn without(text: &str) -> &str {
    written_at(text).map_or(text, |at| &text[..at])
}

/// `text` read anew from a file, with the module `was` had at its end. The module stays in
/// the crate from one revision to the next, where it is written again for the tests that
/// are there now: one that is gone a moment is another module when it is back.
pub(crate) fn kept(was: &str, text: String) -> String {
    match written_at(was) {
        Some(at) => text + &was[at..],
        None => text,
    }
}

/// blaze's harness for a crate.
pub(crate) struct Harness {
    /// The module, which starts with [`MARK`].
    pub(crate) text: String,
}

/// One test as libtest describes it.
struct Test {
    /// The function from the crate's root, and the name libtest shows: the same path
    /// without `crate::`.
    path: Vec<String>,
    /// A `#[bench]`, which libtest calls with a `Bencher`.
    bench: bool,
    ignore: bool,
    ignore_message: Option<String>,
    should_panic: ShouldPanic,
    /// Where its name is: the file as rustc is told it, and the line and column of the
    /// start and the end, from 1.
    file: String,
    start: (usize, usize),
    end: (usize, usize),
}

#[derive(Debug)]
enum ShouldPanic {
    No,
    Yes,
    WithMessage(String),
}

/// The module for the tests of `krate`, `None` when a test is not one the module can say as
/// rustc's harness does (its source or its name is not there, or its `should_panic` names no
/// string), which is a bug to fix (no-rustc H, #88).
/// `file_name` is what rustc is told a file is called.
pub(crate) fn module(
    db: &RootDatabase,
    krate: hir::Crate,
    functions: impl IntoIterator<Item = hir::Function>,
    file_name: &dyn Fn(vfs::FileId) -> Option<String>,
) -> Option<Harness> {
    use hir::HasSource as _;
    let edition = krate.edition(db);
    let mut tests = Vec::new();
    for function in functions {
        let bench = function.is_bench(db);
        if !bench && !function.is_test(db) {
            continue;
        }
        let Some(path) = path_of(db, function, edition) else { continue };
        let source = function.source(db)?;
        let mut test = Test {
            path,
            bench,
            ignore: false,
            ignore_message: None,
            should_panic: ShouldPanic::No,
            file: String::new(),
            start: (0, 0),
            end: (0, 0),
        };
        // What `cfg_attr` gives for the crate's cfgs is written as if alone.
        let mut metas: Vec<ast::Meta> = source.value.attrs().filter_map(|it| it.meta()).collect();
        metas.reverse();
        let mut attrs = Vec::new();
        while let Some(meta) = metas.pop() {
            let ast::Meta::CfgAttrMeta(cfg_attr) = meta else {
                attrs.push(meta);
                continue;
            };
            let predicate = cfg::CfgExpr::parse_from_ast(cfg_attr.cfg_predicate()?);
            if krate.cfg(db).check(&predicate) != Some(false) {
                let inner: Vec<ast::Meta> = cfg_attr.metas().collect();
                metas.extend(inner.into_iter().rev());
            }
        }
        for meta in attrs {
            let named = |path: Option<ast::Path>| path.map(|it| it.syntax().text().to_string());
            match meta {
                ast::Meta::PathMeta(meta) => match named(meta.path()).as_deref() {
                    Some("ignore") => test.ignore = true,
                    Some("should_panic") => test.should_panic = ShouldPanic::Yes,
                    _ => {}
                },
                ast::Meta::KeyValueMeta(meta) => {
                    let value = meta.expr().and_then(|it| string_of(&it));
                    match named(meta.path()).as_deref() {
                        Some("ignore") => (test.ignore, test.ignore_message) = (true, value),
                        Some("should_panic") => {
                            test.should_panic = ShouldPanic::WithMessage(value?);
                        }
                        _ => {}
                    }
                }
                // rustc reads no message from `ignore` with a list after it, and ignores the
                // test.
                ast::Meta::TokenTreeMeta(meta) => match named(meta.path()).as_deref() {
                    Some("ignore") => test.ignore = true,
                    Some("should_panic") => {
                        test.should_panic =
                            ShouldPanic::WithMessage(expected(&meta.token_tree()?)?);
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        let name = source.value.name()?;
        let range = source.with_value(name.syntax()).original_file_range_rooted(db);
        let file = range.file_id.file_id(db);
        test.file = file_name(file)?;
        let lines = ide_db::line_index(db, file);
        let at = |offset| {
            let it = lines.line_col(offset);
            (it.line as usize + 1, it.col as usize + 1)
        };
        (test.start, test.end) = (at(range.range.start()), at(range.range.end()));
        tests.push(test);
    }
    // rustc lists them by name.
    tests.sort_by(|a, b| a.path.cmp(&b.path));
    let root = krate.root_file(db);
    let test_type = match file_name(root).as_deref().map(std::path::Path::new) {
        Some(it) if it.parent().is_some_and(|it| it.ends_with("src")) => "UnitTest",
        Some(it) if it.parent().is_some_and(|it| it.ends_with("tests")) => "IntegrationTest",
        _ => "Unknown",
    };
    let tested = krate.cfg(db).check_atom(&cfg::CfgAtom::Flag(intern::sym::test));
    Some(Harness { text: text(&tests, test_type, tested) })
}

/// The path of a test from its crate's root, which joined by `::` is the name libtest shows.
/// `None` for a test inside a function, which rustc's harness leaves out too: it can't be
/// named.
fn path_of(
    db: &RootDatabase,
    function: hir::Function,
    edition: syntax::Edition,
) -> Option<Vec<String>> {
    let modules = function.module(db).path_to_root(db);
    let mut path = Vec::new();
    for module in modules.iter().rev().skip(1) {
        path.push(module.name(db)?.display(db, edition).to_string());
    }
    path.push(function.name(db).display(db, edition).to_string());
    Some(path)
}

/// The name libtest shows of a `#[test]` of `krate`, the one `--exact` picks it by.
pub(crate) fn name(
    db: &RootDatabase,
    krate: hir::Crate,
    function: hir::Function,
) -> Option<String> {
    Some(path_of(db, function, krate.edition(db))?.join("::"))
}

/// `tested` is whether the crate has `cfg(test)`, which the module then asks for: a library
/// built with it for its unit tests has its root file in common with the library others link.
fn text(tests: &[Test], test_type: &str, tested: bool) -> String {
    let mut text = String::from(MARK);
    if tested {
        text.push_str("#[cfg(test)]\n");
    }
    text.push_str("#[doc(hidden)]\n#[allow(warnings, clippy::all)]\npub mod ");
    text.push_str(MODULE);
    text.push_str(
        " {\n    extern crate test;\n\n    pub fn main() -> test::ExitCode {\n        \
         const TESTS: &[&test::TestDescAndFn] = &[",
    );
    for at in 0..tests.len() {
        _ = write!(text, "&T{at}, ");
    }
    text.push_str("];\n        test::test_main_env_args(TESTS)\n    }\n");
    text.push_str(MARKED);
    for (at, test) in tests.iter().enumerate() {
        let name = test.path.join("::");
        let ignore_message = match &test.ignore_message {
            Some(it) => format!("::core::option::Option::Some({it:?})"),
            None => "::core::option::Option::None".to_owned(),
        };
        let should_panic = match &test.should_panic {
            ShouldPanic::No => "No".to_owned(),
            ShouldPanic::Yes => "Yes".to_owned(),
            ShouldPanic::WithMessage(it) => format!("YesWithMessage({it:?})"),
        };
        let ((start_line, start_col), (end_line, end_col)) = (test.start, test.end);
        let mark = nul_ended(&format!("{}{name}", crate::sandbox::MARK));
        _ = write!(
            text,
            "\n    const T{at}: test::TestDescAndFn = test::TestDescAndFn {{\n        \
             desc: test::TestDesc {{\n            \
             name: test::StaticTestName({name:?}),\n            \
             ignore: {ignore},\n            \
             ignore_message: {ignore_message},\n            \
             source_file: {file:?},\n            \
             start_line: {start_line}usize,\n            \
             start_col: {start_col}usize,\n            \
             end_line: {end_line}usize,\n            \
             end_col: {end_col}usize,\n            \
             should_panic: test::ShouldPanic::{should_panic},\n            \
             compile_fail: false,\n            \
             no_run: false,\n            \
             test_type: test::TestType::{test_type},\n        \
             }},\n        \
             testfn: {testfn},\n    \
             }};\n",
            ignore = test.ignore,
            file = test.file,
            testfn = match test.bench {
                true => format!(
                    "test::StaticBenchFn(|b| {{ let _marked = marked::Marked::on({mark}); \
                     test::assert_test_result(crate::{name}(b)) }})"
                ),
                false => format!(
                    "test::StaticTestFn(|| {{ let _marked = marked::Marked::on({mark}); \
                     test::assert_test_result(crate::{name}()) }})"
                ),
            },
        );
    }
    text.push_str("}\n");
    text
}

/// What gives the sandbox the test a thread runs ([`crate::sandbox::MARK`]): each test is
/// called between `Marked::on` with its marker and the end of what that returns, on its
/// thread, which look up the marker and the marker with no name (an unwinding test's too).
/// Outside a sandbox each is one `access` that finds nothing.
const MARKED: &str = r#"
    mod marked {
        pub struct Marked;

        impl Marked {
            pub fn on(mark: &'static [u8]) -> Marked {
                look(mark);
                Marked
            }
        }

        impl ::core::ops::Drop for Marked {
            fn drop(&mut self) {
                look(b"/.blaze-test/\0")
            }
        }

        #[cfg(unix)]
        fn look(path: &[u8]) {
            unsafe extern "C" {
                fn access(path: *const ::core::ffi::c_char, mode: ::core::ffi::c_int) -> ::core::ffi::c_int;
            }
            // SAFETY: `path` ends with a nul.
            unsafe { access(path.as_ptr().cast(), 0) };
        }

        #[cfg(not(unix))]
        fn look(_: &[u8]) {}
    }
"#;

/// `text` as a byte string literal with a nul at its end, which every edition reads.
fn nul_ended(text: &str) -> String {
    let mut out = String::from("b\"");
    for &byte in text.as_bytes() {
        match byte {
            b'"' | b'\\' => out.extend(['\\', byte as char]),
            0x20..=0x7e => out.push(byte as char),
            _ => out.push_str(&format!("\\x{byte:02x}")),
        }
    }
    out.push_str("\\0\"");
    out
}

/// The value of a string literal.
fn string_of(expr: &ast::Expr) -> Option<String> {
    let ast::Expr::Literal(literal) = expr else { return None };
    let ast::LiteralKind::String(string) = literal.kind() else { return None };
    Some(string.value().ok()?.into_owned())
}

/// The message of `should_panic(expected = "...")`. `None` for any other argument: rustc
/// takes no other.
fn expected(tree: &ast::TokenTree) -> Option<String> {
    let tokens: Vec<_> = tree
        .syntax()
        .children_with_tokens()
        .filter_map(|it| it.into_token())
        .filter(|it| !it.kind().is_trivia())
        .collect();
    match tokens.as_slice() {
        [open, name, eq, value, close]
            if open.text() == "("
                && name.text() == "expected"
                && eq.text() == "="
                && close.text() == ")" =>
        {
            Some(ast::String::cast(value.clone())?.value().ok()?.into_owned())
        }
        _ => None,
    }
}
