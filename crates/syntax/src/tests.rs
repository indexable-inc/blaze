use std::{
    fs,
    path::{Path, PathBuf},
};

use ast::{HasModuleItem, HasName};
use blaze_graph::par;
use expect_test::expect_file;
use parser::Edition;
use stdx::format_to_acc;
use test_utils::{bench, bench_fixture, project_root};

use crate::{AstNode, SourceFile, SyntaxError, Text, TextRange, TextSize, ast, fuzz};

#[test]
fn parse_smoke_test() {
    let code = r#"
fn main() {
    println!("Hello, world!")
}
    "#;

    let parse = SourceFile::parse(code, Edition::CURRENT);
    // eprintln!("{:#?}", parse.syntax_node());
    assert!(parse.ok().is_ok());
}

#[test]
fn statements_that_are_no_expression_parse_as_macro_statements() {
    let text = "let x = 1; x + 1";
    assert!(!ast::Expr::parse(text, Edition::CURRENT).errors().is_empty());
    let parse = ast::MacroStmts::parse(text, Edition::CURRENT);
    assert_eq!(parse.errors(), Vec::<SyntaxError>::new());
    assert!(parse.tree().statements().next().is_some());
    assert!(!ast::MacroStmts::parse("let = ;", Edition::CURRENT).errors().is_empty());
}

#[test]
fn a_reparse_onto_a_known_edit_is_the_whole_parse() {
    let before = "fn f() { 1 }\nfn g() {}\n";
    let after = "fn f() { 12 }\nfn g() {}\n";
    let insert_at = TextSize::from(10);
    for outline in [false, true] {
        let parse = |text: &str| match outline {
            true => SourceFile::parse_outline(text, Edition::CURRENT),
            false => SourceFile::parse_shared(text, Edition::CURRENT),
        };
        let old = parse(before);
        let new = old.reparse_onto(TextRange::empty(insert_at), after, Edition::CURRENT);
        assert_eq!(new, parse(after));
        assert_eq!(new, old.reparse_to(after, Edition::CURRENT));
        assert_eq!(new.errors_with_bodies(), parse(after).errors_with_bodies());
    }
}

#[test]
fn a_weak_parse_upgrades_to_the_same_parse_while_the_tree_is_held() {
    let parse = SourceFile::parse("fn f() { 1 + }", Edition::CURRENT);
    let weak = parse.downgrade();
    let upgraded = weak.upgrade().expect("the parse holds its tree");
    assert_eq!(upgraded, parse);
    assert_eq!(upgraded.errors(), parse.errors());
}

#[test]
fn benchmark_parser() {
    if std::env::var("RUN_SLOW_BENCHES").is_err() {
        return;
    }

    let data = bench_fixture::glorious_old_parser();
    let tree = {
        let _b = bench("parsing");
        let p = SourceFile::parse(&data, Edition::CURRENT);
        assert!(p.errors().is_empty());
        assert_eq!(p.tree().syntax.text_range().len(), 352474.into());
        p.tree()
    };

    {
        let _b = bench("tree traversal");
        let fn_names =
            tree.syntax().descendants().filter_map(ast::Fn::cast).filter_map(|f| f.name()).count();
        assert_eq!(fn_names, 268);
    }
}

#[test]
fn validation_tests() {
    dir_tests(&test_data_dir(), &["parser/validation"], "rast", |text, path| {
        let parse = SourceFile::parse(text, Edition::CURRENT);
        let errors = parse.errors();
        assert_errors_are_present(&errors, path);
        parse.debug_dump()
    });
}

#[test]
fn parser_fuzz_tests() {
    for (_, text) in collect_rust_files(&test_data_dir(), &["parser/fuzz-failures"]) {
        fuzz::check_parser(&text)
    }
}

#[test]
fn reparse_fuzz_tests() {
    for (_, text) in collect_rust_files(&test_data_dir(), &["reparse/fuzz-failures"]) {
        let check = fuzz::CheckReparse::from_data(text.as_bytes()).unwrap();
        check.run();
    }
}

/// Test that Rust-analyzer can parse and validate the rust-analyzer
#[test]
fn self_hosting_parsing() {
    let crates_dir = project_root().join("crates");

    let mut files = Vec::new();
    let mut work = vec![crates_dir.into_std_path_buf()];
    while let Some(dir) = work.pop() {
        for entry in dir.read_dir().unwrap() {
            let entry = entry.unwrap();
            let file_type = entry.file_type().unwrap();
            let path = entry.path();
            let file_name = &path.file_name().unwrap_or_default().to_str().unwrap_or_default();
            let is_hidden = file_name.starts_with('.');
            if !is_hidden {
                if file_type.is_dir() {
                    work.push(path);
                } else if file_type.is_file() && file_name.ends_with(".rs") {
                    files.push(path);
                }
            }
        }
    }

    files.retain(|path| {
        // Get all files which are not in the crates/syntax/test_data folder
        !path.components().any(|component| component.as_os_str() == "test_data")
    });

    assert!(
        files.len() > 100,
        "self_hosting_parsing found too few files - is it running in the right directory?"
    );

    let errors = par::filter_map(files, |file| {
        let text = read_text(&file);
        match SourceFile::parse(&text, Edition::CURRENT).ok() {
            Ok(_) => None,
            Err(err) => Some((file, err)),
        }
    });

    if !errors.is_empty() {
        let errors = errors.into_iter().fold(String::new(), |mut acc, (path, err)| {
            format_to_acc!(acc, "{}: {:?}\n", path.display(), err[0])
        });
        panic!("Parsing errors:\n{errors}\n");
    }
}

/// An outline is the full parse once walked: through the cursor, which parses each lazy body
/// the first time it walks into it, the same tree, and with its bodies the same errors.
#[test]
fn outline_is_the_full_parse() {
    let root = project_root().into_std_path_buf();
    let mut files = collect_rust_files(
        &root.join("crates/parser/test_data"),
        &["parser/ok", "parser/inline/ok"],
    );
    let mut sources = Vec::new();
    for dir in ["crates/syntax/src", "crates/parser/src"] {
        rust_files_under(&root.join(dir), &mut sources);
    }
    files.extend(sources.into_iter().map(|path| {
        let text = read_text(&path);
        (path, text)
    }));
    assert!(files.len() > 100, "the parser's test data and the sources of syntax and parser");

    let lazies: usize = par::map(files, |(path, text)| {
        let full = SourceFile::parse(&text, Edition::CURRENT);
        let outline = SourceFile::parse_outline(Text::from(text.as_str()), Edition::CURRENT);
        let tree = outline.tree.as_ref().unwrap().flat().clone();
        let _ = outline.errors();
        assert!(
            tree.lazies().all(|at| !tree.is_parsed(at)),
            "{}: `errors` parsed a body",
            path.display()
        );

        let elements = |root: crate::SyntaxNode| {
            root.preorder_with_tokens()
                .filter_map(|event| match event {
                    crate::WalkEvent::Enter(it) => Some((it.kind(), it.text_range())),
                    crate::WalkEvent::Leave(_) => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            elements(outline.syntax_node()),
            elements(full.syntax_node()),
            "{}: the elements",
            path.display()
        );
        assert_eq!(
            format!("{:#?}", outline.tree().syntax()),
            format!("{:#?}", full.tree().syntax()),
            "{}: the dump",
            path.display()
        );
        assert_eq!(
            sorted_errors(outline.errors_with_bodies()),
            sorted_errors(full.errors()),
            "{}: the errors",
            path.display()
        );
        tree.lazies().count()
    })
    .into_iter()
    .sum();
    assert!(lazies > 1000, "the outlines left {lazies} bodies lazy");
}

#[test]
fn outline_leaves_bodies_lazy() {
    let text = "
fn f() { 1 }
impl S {
    fn g(&self) -> u8 { 2 }
}
const C: () = { fn h() {} };
fn m() -> u8 { if true { 1 } else { 2 } }
fn k() { #[a] }
";
    let outline = SourceFile::parse_outline(Text::from(text), Edition::CURRENT);
    let tree = outline.tree.as_ref().unwrap().flat().clone();
    let lazy: Vec<&str> = tree.lazies().map(|at| &text[tree.range(at)]).collect();
    assert_eq!(lazy, ["{ 1 }", "{ 2 }", "{}", "{ if true { 1 } else { 2 } }"]);
    let errors = sorted_errors(outline.errors());
    assert!(!errors.is_empty(), "the attribute without an item in `k`, parsed in full");
    assert!(tree.lazies().all(|at| !tree.is_parsed(at)));

    let fns: Vec<ast::Fn> = outline
        .tree()
        .items()
        .filter_map(|it| match it {
            ast::Item::Fn(it) => Some(vec![it]),
            ast::Item::Impl(it) => Some(
                it.assoc_item_list()?
                    .assoc_items()
                    .filter_map(|it| match it {
                        ast::AssocItem::Fn(it) => Some(it),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        })
        .flatten()
        .collect();
    let names: Vec<_> = fns
        .iter()
        .map(|it| (it.name().unwrap().to_string(), it.body_is_lazy()))
        .collect();
    assert_eq!(
        names,
        [
            ("f".to_owned(), true),
            ("g".to_owned(), true),
            ("m".to_owned(), true),
            ("k".to_owned(), false)
        ]
    );
    assert!(tree.lazies().all(|at| !tree.is_parsed(at)), "finding the bodies parsed one");

    assert_eq!(sorted_errors(outline.errors_with_bodies()), errors, "the bodies have none");
    assert!(tree.lazies().all(|at| tree.is_parsed(at)));
    let m = fns[2].body().unwrap();
    assert!(fns[2].body_is_lazy() && m.tail_expr().is_some());
}

fn sorted_errors(errors: Vec<SyntaxError>) -> Vec<(crate::TextRange, String)> {
    let mut errors: Vec<_> = errors.into_iter().map(|it| (it.range(), it.to_string())).collect();
    errors.sort_by(|(a, x), (b, y)| (a.start(), a.end(), x).cmp(&(b.start(), b.end(), y)));
    errors
}

/// The `.rs` files under `dir`, at any depth.
fn rust_files_under(dir: &Path, acc: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files_under(&path, acc);
        } else if path.extension().unwrap_or_default() == "rs" {
            acc.push(path);
        }
    }
}

#[test]
fn doc_comment_on_literal_expr() {
    let parse = SourceFile::parse("fn f() { ///\n0..0; }", parser::Edition::CURRENT);
    assert!(parse.errors().is_empty());
}

fn test_data_dir() -> PathBuf {
    project_root().into_std_path_buf().join("crates/syntax/test_data")
}

fn assert_errors_are_present(errors: &[SyntaxError], path: &Path) {
    assert!(!errors.is_empty(), "There should be errors in the file {:?}", path.display());
}

/// Calls callback `f` with input code and file paths for each `.rs` file in `test_data_dir`
/// subdirectories defined by `paths`.
///
/// If the content of the matching output file differs from the output of `f()`
/// the test will fail.
///
/// If there is no matching output file it will be created and filled with the
/// output of `f()`, but the test will fail.
fn dir_tests<F>(test_data_dir: &Path, paths: &[&str], outfile_extension: &str, f: F)
where
    F: Fn(&str, &Path) -> String,
{
    for (path, input_code) in collect_rust_files(test_data_dir, paths) {
        let actual = f(&input_code, &path);
        let path = path.with_extension(outfile_extension);
        expect_file![path].assert_eq(&actual)
    }
}

/// Collects all `.rs` files from `dir` subdirectories defined by `paths`.
fn collect_rust_files(root_dir: &Path, paths: &[&str]) -> Vec<(PathBuf, String)> {
    paths
        .iter()
        .flat_map(|path| {
            let path = root_dir.to_owned().join(path);
            rust_files_in_dir(&path).into_iter()
        })
        .map(|path| {
            let text = read_text(&path);
            (path, text)
        })
        .collect()
}

/// Collects paths to all `.rs` files from `dir` in a sorted `Vec<PathBuf>`.
fn rust_files_in_dir(dir: &Path) -> Vec<PathBuf> {
    let mut acc = Vec::new();
    for file in fs::read_dir(dir).unwrap() {
        let file = file.unwrap();
        let path = file.path();
        if path.extension().unwrap_or_default() == "rs" {
            acc.push(path);
        }
    }
    acc.sort();
    acc
}

/// Read file and normalize newlines.
///
/// `rustc` seems to always normalize `\r\n` newlines to `\n`:
///
/// ```
/// let s = "
/// ";
/// assert_eq!(s.as_bytes(), &[10]);
/// ```
///
/// so this should always be correct.
fn read_text(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|_| panic!("File at {path:?} should be valid"))
        .replace("\r\n", "\n")
}

/// A warm exact list of a parse's errors is one allocation (the box) plus a clone of each
/// stored parser error's message, as `errors` makes; none for a parse without errors.
#[test]
fn exact_errors_allocate_only_their_box() {
    let text = "fn f() { let = ; } fn g( { } struct S { a: }";
    let parse = SourceFile::parse(text, Edition::CURRENT);
    let want = parse.errors();
    assert!(!want.is_empty());
    assert_eq!(parse.errors_exact().as_deref(), Some(&want[..]));
    blaze_alloc_count::assert_at_most(1 + want.len() as u64, "exact errors", || {
        std::hint::black_box(parse.errors_exact());
    });
    let clean = SourceFile::parse("fn f() {}", Edition::CURRENT);
    assert_eq!(clean.errors_exact(), None);
    blaze_alloc_count::assert_at_most(0, "no errors", || {
        std::hint::black_box(clean.errors_exact());
    });

    let outline = SourceFile::parse_outline(text, Edition::CURRENT);
    let want = outline.errors_with_bodies();
    assert_eq!(outline.errors_with_bodies_exact().as_deref(), Some(&want[..]));
    assert_eq!(outline.errors_exact().as_deref().unwrap_or_default(), &outline.errors()[..]);
}
