//! A test harness, as rustc's `--test` writes it: a description of each `#[test]` and
//! `#[bench]` of the crate for libtest, and a `main` that serves them to usercode's fork
//! server or runs them with `test_main_env_args`. The harness is source text, a module
//! [`MODULE`] the build adds to the end of the crate's root file ([`HirFrontend::harness`]),
//! so the front end resolves, checks and lowers it as any other code: a test that is not a
//! `fn() -> impl Termination` is the type error rustc's harness gives.
//!
//! The module's second line is a digest of the listing it was written for; [`roots`] lists
//! the tests again and refuses a module written for others.

use std::fmt::Write as _;

use base_db::{Crate, Db};
use blaze_codegen::{CodegenError, exec, plan::Mode, settings::Panic};
use blaze_ids::Hasher;
use cfg::{CfgAtom, CfgExpr};
use hir_def::{
    FunctionId, Lookup, ModuleDefId, ModuleId, attrs::AttrFlags, nameres::crate_def_map,
    signatures::FunctionSignature, src::HasSource, unstable_features::UnstableFeatures,
};
use hir_expand::name::Name;
use hir_ty_mir::mir::location::macro_location_of;
use intern::{Symbol, sym};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, AstToken as _, HasAttrs as _, HasName as _},
};

use super::{
    super::{Hashes as _, HirFrontend, HirInstance, Kind},
    Listed, RootTest, refused,
};
use crate::home::Homes;

/// The module's name in the crate's root.
pub const MODULE: &str = "__blaze_test_harness";

/// What the module starts with, after the text of the root file: v1's mark, so a root file
/// is cut the same by either.
const MARK: &str = "\n// blaze's test harness, which is not in the file on disk.\n";

/// The start of the module's second line, which the listing's digest ends.
const LISTED: &str = "// blaze's test listing ";

const LISTING_DOMAIN: &str = "blaze v2 test listing 1";

/// Whether `krate` is built with `cfg(test)`, which a test harness is.
pub(super) fn tested(db: &Db, krate: Crate) -> bool {
    krate.cfg_options(db).check_atom(&CfgAtom::Flag(sym::test))
}

/// Where what blaze appends to a root file starts in `text`: this module or a proc-macro
/// crate's list ([`super::proc_macros`]).
pub(super) fn appended_at(text: &str) -> Option<usize> {
    [MARK, super::proc_macros::MARK].iter().filter_map(|it| text.find(it)).min()
}

/// One test as libtest describes it.
#[derive(Debug)]
struct Test {
    /// The name libtest shows: its path from the crate's root, joined by `::`.
    name: String,
    function: FunctionId,
    bench: bool,
    ignore: bool,
    ignore_message: Option<String>,
    should_panic: Option<Option<String>>,
    /// Where its name is: the file as rustc is told it, and the line and column of the
    /// start and the end, from 1.
    file: String,
    start: (u32, u32),
    end: (u32, u32),
}

/// The tests of `krate`, sorted by name as rustc's harness lists them: each `#[test]` and
/// `#[bench]` function a path from the root names. One in a function's body is left out, as
/// rustc leaves it (`unnameable_test_items`).
fn listing(frontend: &HirFrontend<'_>, db: &Db, krate: Crate) -> Result<Vec<Test>, String> {
    if UnstableFeatures::query(db, krate).is_enabled(&Symbol::intern("custom_test_frameworks")) {
        return Err("it enables `custom_test_frameworks`, whose `#![test_runner]` and \
                    `#[test_case]` blaze's harness does not run"
            // alloc: the error's message
            .to_owned());
    }
    let edition = krate.data(db).edition;
    let def_map = crate_def_map(db, krate);
    // alloc: once a harness, the listing it returns
    let mut tests = Vec::new();
    // alloc: once a harness, a module's path a module
    let mut stack: Vec<(ModuleId, String)> = vec![(def_map.crate_root(db), String::new())];
    while let Some((module, prefix)) = stack.pop() {
        let data = def_map.module(db, module);
        for def in data.scope(db).declarations() {
            let ModuleDefId::FunctionId(func) = def else { continue };
            let flags = AttrFlags::query(db, func.into());
            let bench = flags.contains(AttrFlags::IS_BENCH);
            if !bench && !flags.contains(AttrFlags::IS_TEST) {
                continue;
            }
            let own = &FunctionSignature::of(db, func).name;
            // alloc: the name libtest is given
            let name = format!("{prefix}{}", own.display(db, edition));
            if super::generic(db, func) {
                // alloc: the error's message
                return Err(format!(
                    "the test `{name}` is generic over a type or a constant, which a test can't \
                     be"
                ));
            }
            tests.push(test(frontend, db, krate, func, name, bench)?);
        }
        for (child, &id) in data.children(db) {
            // alloc: once a module, the prefix of its tests' names
            stack.push((id, format!("{prefix}{}::", child.display(db, edition))));
        }
    }
    tests.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(tests)
}

fn test(
    frontend: &HirFrontend<'_>,
    db: &Db,
    krate: Crate,
    function: FunctionId,
    name: String,
    bench: bool,
) -> Result<Test, String> {
    let ptr = function.lookup(db).ast_ptr(db);
    let root = ptr.file_id.root(db);
    let source = ptr.map(|it| it.to_view(root));
    let mut test = Test {
        name,
        function,
        bench,
        ignore: false,
        ignore_message: None,
        should_panic: None,
        // alloc: empty, which allocates nothing
        file: String::new(),
        start: (0, 0),
        end: (0, 0),
    };
    // What `cfg_attr` gives for the crate's cfgs is read as if written alone.
    // alloc: once a test, its attributes
    let mut metas: Vec<ast::Meta<'_>> = source.value.attrs().filter_map(|it| it.meta()).collect();
    metas.reverse();
    let named = |path: Option<ast::Path<'_>>| {
        let text = path?.syntax().text();
        ["ignore", "should_panic"].into_iter().find(|&it| text == it)
    };
    while let Some(meta) = metas.pop() {
        match meta {
            ast::Meta::CfgAttrMeta(cfg_attr) => {
                let Some(predicate) = cfg_attr.cfg_predicate() else { continue };
                let predicate = CfgExpr::parse_from_view(predicate);
                if krate.cfg_options(db).check(&predicate) != Some(false) {
                    let at = metas.len();
                    metas.extend(cfg_attr.metas());
                    metas[at..].reverse();
                }
            }
            ast::Meta::PathMeta(meta) => match named(meta.path()) {
                Some("ignore") => test.ignore = true,
                Some("should_panic") => test.should_panic = Some(None),
                _ => {}
            },
            ast::Meta::KeyValueMeta(meta) => {
                let value = meta.expr().map(string_of).transpose();
                // alloc: the error's message
                let value = value.map_err(|why| format!("the test `{}`: {why}", test.name))?;
                match named(meta.path()) {
                    Some("ignore") => (test.ignore, test.ignore_message) = (true, value.flatten()),
                    Some("should_panic") => test.should_panic = Some(value.flatten()),
                    _ => {}
                }
            }
            // rustc reads no message from `ignore` with a list after it, and ignores the test;
            // of `should_panic`'s list it reads `expected = ".."`, and the test panics with
            // any message for another.
            ast::Meta::TokenTreeMeta(meta) => match named(meta.path()) {
                Some("ignore") => test.ignore = true,
                Some("should_panic") => {
                    test.should_panic = Some(meta.token_tree().and_then(expected));
                }
                _ => {}
            },
            _ => {}
        }
    }
    let Some(name) = source.value.name() else {
        // alloc: the error's message
        return Err(format!("the test `{}` has no name in its source", test.name));
    };
    let range = name.syntax().text_range();
    let at = |range: TextRange| macro_location_of(db, source.file_id, range);
    let (Some((file, start_line, start_col)), Some((_, end_line, end_col))) =
        (at(range), at(TextRange::empty(range.end())))
    else {
        // alloc: the error's message
        return Err(format!("the test `{}`: its name is at no line of a file", test.name));
    };
    test.file = frontend.hashes.file_name(db, file)?;
    (test.start, test.end) = ((start_line, start_col), (end_line, end_col));
    Ok(test)
}

/// The value of a string literal; `None` for another expression, whose value rustc reads as
/// none (`#[ignore = 1]` is malformed and refused by the front end).
fn string_of(expr: ast::Expr<'_>) -> Result<Option<String>, String> {
    let ast::Expr::Literal(literal) = expr else { return Ok(None) };
    let ast::LiteralKind::String(string) = literal.kind() else { return Ok(None) };
    match string.value() {
        // alloc: once an attribute with a message, which libtest is given
        Ok(it) => Ok(Some(it.into_owned())),
        // alloc: the error's message
        Err(_) => Err(format!("the string {} has an escape that is not one", string.text())),
    }
}

/// The message of `should_panic(expected = "...")`. `None` for any other list.
fn expected(tree: ast::TokenTree<'_>) -> Option<String> {
    let mut tokens = tree
        .syntax()
        .children_with_tokens()
        .filter_map(|it| it.into_token())
        .filter(|it| !it.kind().is_trivia());
    let [Some(open), Some(name), Some(eq), Some(value), Some(close), None] =
        [(); 6].map(|()| tokens.next())
    else {
        return None;
    };
    if [open, name, eq, close].map(|it| it.text()) != ["(", "expected", "=", ")"] {
        return None;
    }
    // alloc: the message libtest is given
    Some(ast::String::cast(value)?.value().ok()?.into_owned())
}

/// What libtest's `TestType` of every test of a crate is: rustc's, from the directory of the
/// crate's root file as rustc is told it.
fn test_type(frontend: &HirFrontend<'_>, db: &Db, krate: Crate) -> Result<&'static str, String> {
    let root = frontend.hashes.file_name(db, krate.data(db).root_file_id)?;
    let parent = std::path::Path::new(&root).parent();
    Ok(match parent {
        Some(it) if it.ends_with("src") => "UnitTest",
        Some(it) if it.ends_with("tests") => "IntegrationTest",
        _ => "Unknown",
    })
}

/// The digest of a listing, which the module's second line ends with.
fn digest(tests: &[Test], test_type: &str) -> String {
    let mut hasher = Hasher::new(LISTING_DOMAIN);
    hasher.part(test_type.as_bytes());
    for test in tests {
        hasher.part(test.name.as_bytes()).part(test.file.as_bytes());
        hasher.update(&[u8::from(test.bench), u8::from(test.ignore)]);
        let expected = test.should_panic.as_ref().and_then(Option::as_ref);
        for message in [test.ignore_message.as_ref(), expected] {
            match message {
                Some(it) => hasher.update(&[1]).part(it.as_bytes()),
                None => hasher.update(&[0]),
            };
        }
        hasher.update(&[u8::from(test.should_panic.is_some())]);
        for it in [test.start.0, test.start.1, test.end.0, test.end.1] {
            hasher.update(&it.to_le_bytes());
        }
    }
    // alloc: once a harness, the digest its text carries
    hasher.finish().to_string()
}

/// The roots of a test harness of `krate`: its C `main`, then each test's function, at the
/// place its [`RootTest::root`] says. The root file must end with the module [`HirFrontend::harness`]
/// writes for the tests it has now.
pub(super) fn roots(
    frontend: &HirFrontend<'_>,
    db: &Db,
    homes: &mut Homes,
    krate: Crate,
    listed: &mut Listed,
) -> Result<Vec<RootTest>, CodegenError> {
    let tests = listing(frontend, db, krate).map_err(|what| refused(db, krate, what))?;
    let test_type = test_type(frontend, db, krate).map_err(|what| refused(db, krate, what))?;
    let main = harness_main(db, krate).map_err(|what| refused(db, krate, what))?;
    let text = db.file_text(krate.data(db).root_file_id).text(db);
    let written = text.find(MARK).and_then(|at| text[at + MARK.len()..].lines().next());
    let want = digest(&tests, test_type);
    if written.and_then(|it| it.strip_prefix(LISTED)) != Some(want.as_str()) {
        return Err(refused(
            db,
            krate,
            "its test harness was written for other tests than the crate has: the build must \
             write it again (`HirFrontend::harness`)",
        ));
    }
    listed.instance(HirInstance(Kind::Start(main)));
    Ok(tests
        .into_iter()
        .map(|test| RootTest {
            root: listed.function(db, homes, test.function),
            name: test.name,
            function: test.function,
            ignore: test.ignore,
            should_panic: test.should_panic,
            bench: test.bench,
            outside: false,
        })
        // alloc: once a harness, the tests cargo is given
        .collect())
}

/// The harness's `main`, in [`MODULE`] at the crate's root.
fn harness_main(db: &Db, krate: Crate) -> Result<FunctionId, String> {
    let def_map = crate_def_map(db, krate);
    let root = def_map.module(db, def_map.crate_root(db));
    let module = root.children(db).get(&Name::new_symbol_root(Symbol::intern(MODULE)));
    let Some(&module) = module else {
        // alloc: the error's message
        return Err(format!(
            "its root file has no test harness (the module `{MODULE}` the build writes with \
             `HirFrontend::harness`)"
        ));
    };
    let main = def_map.module(db, module).scope(db).get(&Name::new_symbol_root(sym::main));
    match main.values.map(|it| it.def) {
        Some(ModuleDefId::FunctionId(func)) => Ok(func),
        // alloc: the error's message
        _ => Err(format!("its test harness `{MODULE}` has no `main`")),
    }
}

impl HirFrontend<'_> {
    /// The module of a test harness for `krate`, to add to the end of the text of its root
    /// file ([`Self::on_disk`] gives the text before); `None` when `krate` has no `cfg(test)`
    /// (a test with `harness = false`), whose `main` is its own. `fork_server` is
    /// `blaze_usercode::test::fork_server`, asked for the module that records what each test
    /// ran when a binary built in `mode` has jump-table slots ([`exec::records`]): the text
    /// differs by mode, so a harness written for one is written again for another.
    pub fn harness(
        &self,
        krate: Crate,
        panic: Panic,
        mode: Mode,
        fork_server: fn(bool) -> &'static str,
    ) -> Result<Option<String>, CodegenError> {
        let fork_server = fork_server(exec::records(mode));
        self.with(|db, _| {
            if !tested(db, krate) {
                return Ok(None);
            }
            if panic == Panic::Abort {
                return Err(refused(
                    db,
                    krate,
                    "a test harness with `panic = \"abort\"`, which rustc builds only with \
                     `-Zpanic-abort-tests`, and blaze's fork server does not run",
                ));
            }
            let tests = listing(self, db, krate).map_err(|what| refused(db, krate, what))?;
            let test_type = test_type(self, db, krate).map_err(|what| refused(db, krate, what))?;
            Ok(Some(text(&tests, test_type, &digest(&tests, test_type), fork_server)))
        })
    }

    /// The text of a root file as it is on disk: without the test harness or the list of
    /// proc macros blaze appends to it.
    pub fn on_disk(text: &str) -> &str {
        appended_at(text).map_or(text, |at| &text[..at])
    }
}

/// The module: `T0`, `T1`, ... in the listing's order, which is libtest's.
fn text(tests: &[Test], test_type: &str, digest: &str, fork_server: &str) -> String {
    // alloc: once a harness, the text cargo writes
    let mut text = String::from(MARK);
    _ = writeln!(text, "{LISTED}{digest}");
    text.push_str("#[cfg(test)]\n#[doc(hidden)]\n#[allow(warnings, clippy::all)]\npub mod ");
    text.push_str(MODULE);
    text.push_str(" {\n    extern crate test;\n");
    text.push_str(fork_server);
    text.push_str(
        "\n    pub fn main() -> test::ExitCode {\n        \
         const TESTS: &[&test::TestDescAndFn] = &[",
    );
    for at in 0..tests.len() {
        _ = write!(text, "&T{at}, ");
    }
    text.push_str(
        "];\n        \
         if let ::core::option::Option::Some(socket) = ::std::env::var_os(fork_server::SOCKET) {\n            \
         fork_server::serve(TESTS, socket)\n        \
         }\n        \
         test::test_main_env_args(TESTS)\n    }\n",
    );
    for (at, test) in tests.iter().enumerate() {
        let name = &test.name;
        let ((start_line, start_col), (end_line, end_col)) = (test.start, test.end);
        _ = write!(
            text,
            "\n    const T{at}: test::TestDescAndFn = test::TestDescAndFn {{\n        \
             desc: test::TestDesc {{\n            \
             name: test::StaticTestName({name:?}),\n            \
             ignore: {ignore},\n            \
             ignore_message: ",
            ignore = test.ignore,
        );
        _ = match &test.ignore_message {
            Some(it) => write!(text, "::core::option::Option::Some({it:?})"),
            None => write!(text, "::core::option::Option::None"),
        };
        _ = write!(
            text,
            ",\n            \
             source_file: {file:?},\n            \
             start_line: {start_line}usize,\n            \
             start_col: {start_col}usize,\n            \
             end_line: {end_line}usize,\n            \
             end_col: {end_col}usize,\n            \
             should_panic: test::ShouldPanic::",
            file = test.file,
        );
        _ = match &test.should_panic {
            None => write!(text, "No"),
            Some(None) => write!(text, "Yes"),
            Some(Some(it)) => write!(text, "YesWithMessage({it:?})"),
        };
        _ = write!(
            text,
            ",\n            \
             compile_fail: false,\n            \
             no_run: false,\n            \
             test_type: test::TestType::{test_type},\n        \
             }},\n        \
             testfn: "
        );
        _ = match test.bench {
            true => write!(text, "test::StaticBenchFn(|b| test::assert_test_result(crate::{name}(b)))"),
            false => write!(text, "test::StaticTestFn(|| test::assert_test_result(crate::{name}()))"),
        };
        text.push_str(",\n    };\n");
    }
    text.push_str("}\n");
    text
}
