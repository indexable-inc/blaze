//! The front end on the ladder's programs (crates/blaze-cli/tests/ladder/*/src): each is read
//! whole, and every body checks with no diagnostic; what it does not take is a typed refusal
//! naming the construct and its place.

use std::alloc::Global;
use std::string::String;
use std::vec::Vec;

use blaze_own::Alloc;
use blaze_typeck::{InferCtx, Needs, Outcome};

use crate::front::Front;
use crate::run::refusals;
use crate::refusal::{NOT_YET, RefusalRule, SourceFile, syntax_name};

/// A rung's files, read from disk: `(path, text)` with paths relative to the package root.
fn rung(name: &str) -> Vec<(String, String), Global> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../blaze-cli/tests/ladder").join(name);
    let mut files = Vec::new_in(Global);
    let mut stack = std::vec![dir.join("src")];
    while let Some(d) = stack.pop() {
        let entries = std::fs::read_dir(&d).unwrap_or_else(|e| panic!("reading {}: {e}", d.display()));
        for entry in entries {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "rs") {
                let rel = path.strip_prefix(&dir).expect("under the rung").to_string_lossy().into_owned();
                let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {rel}: {e}"));
                files.push((rel, text));
            }
        }
    }
    files.sort();
    files
}

/// The type diagnostics of every body of the program of `files`, summed; a refusal or a
/// question the world holds back fails the test, naming the fn.
fn diagnostics(files: &[SourceFile<'_>]) -> usize {
    let program = Front::new(files, "src/main.rs", &mut crate::front::BlazeAttrs::default()).unwrap_or_else(|r| panic!("refused: {}", crate::run::plain(files, &[r])));
    let types = program.tys.view();
    let mut n = 0;
    for (i, f) in program.fns.iter().enumerate() {
        let Some(body) = f.body.as_ref() else { continue };
        let mut cx = InferCtx::new_in(Alloc::global());
        let hints = Needs::new_in(Alloc::global());
        match blaze_typeck::check(&body.body.view(types), &program.world(i), &mut cx, &hints, Alloc::global()) {
            Ok(Outcome::Done(c)) => n += c.diags.len(),
            Ok(Outcome::Need(q)) => panic!("`{}`: the world missed {:?}", f.path, q.questions),
            Err(e) => panic!("`{}`: {e:?}", f.path),
        }
    }
    n
}

/// Every rung's bodies check with the real context (blaze-typeck's `InferCtx`, blaze-ty with
/// blaze-solve) with no diagnostic.
#[test]
fn every_rung_checks_with_the_real_context() {
    for name in ["1-empty", "2-exit", "3-hello", "4-modules", "5-generics", "6-aggregates", "7-traits", "7a-drops", "7c-main-io"] {
        let owned = rung(name);
        let mut files = Vec::new_in(Global);
        files.extend(owned.iter().map(|(p, t)| SourceFile { path: p, text: t }));
        assert_eq!(diagnostics(&files), 0, "{name}");
    }
}

/// The control for the test above: a call whose bound no impl meets and a method no trait in
/// scope has are diagnostics, while the same file with the impl checks.
#[test]
fn the_real_context_refuses_an_unmet_bound() {
    let head = "#![feature(no_core, lang_items)]\n#![no_core]\n#[lang = \"sized\"] trait Sized {}\n\
                trait Size { fn size(&self) -> usize; }\nstruct A { n: usize }\nstruct B { n: usize }\n\
                impl Size for A { fn size(&self) -> usize { self.n } }\n\
                fn of<T: Size>(t: &T) -> usize { t.size() }\n";
    let diags = |main: &str| -> usize {
        let text = std::format!("{head}{main}");
        diagnostics(&[SourceFile { path: "src/main.rs", text: &text }])
    };
    assert_eq!(diags("fn main() { let a = A { n: 1 }; let _x = of(&a) + a.size(); }"), 0);
    assert!(diags("fn main() { let b = B { n: 1 }; let _x = of(&b); }") > 0, "`B: Size` has no impl");
    assert!(diags("fn main() { let b = B { n: 1 }; let _x = b.size(); }") > 0, "`B` has no method `size`");
}

/// A construct this build does not take is one refusal naming it, at its place, in the words
/// a Rust programmer uses, whichever stage refuses it: a closure passes the front end and the
/// checker and is refused by the build (`BuildError::Unsupported`), with the same finding the
/// front end gives. The plain rendering says it on its line and closes with what this build
/// runs today.
#[test]
fn a_closure_is_a_named_refusal() {
    let text = "struct S { a: u8 }\nfn main() { let f = |x: i32| x; }";
    let files = [SourceFile { path: "src/main.rs", text }];
    let all = refusals(&files, "src/main.rs");
    assert_eq!(all.len(), 1, "{all:?}");
    let r = &all[0];
    assert_eq!((r.rule, r.construct), (RefusalRule::Unsupported, Some("a closure")), "{r:?}");
    assert!(r.message.ends_with(NOT_YET), "{}", r.message);
    let at = (r.place + r.start) as usize;
    assert!(text[at..].starts_with("|x: i32| x"), "{r:?}: at `{}`", &text[at..]);
    let shown = crate::run::plain(&files, &all);
    let lines: Vec<&str> = shown.lines().collect();
    assert_eq!(lines, ["src/main.rs:2:21: a closure is not supported by this build of blaze yet", crate::run::RUNS_TODAY]);
}

/// `#[blaze::..]` is inert: a known name is no refusal and is counted as unchecked; an unknown
/// one is one advice finding at its attribute, naming the nearest known name; a tool's
/// attribute (`rustfmt::skip`) is inert too, and an unknown plain attribute is still refused.
#[test]
fn blaze_attributes_are_inert_and_unknown_ones_advice() {
    let head = "#![feature(no_core, lang_items)]\n#![no_core]\n#[lang = \"sized\"] trait Sized {}\n";
    let run = |items: &str| {
        let text = std::format!("{head}{items}\nfn main() {{}}");
        let files = [SourceFile { path: "src/main.rs", text: &text }];
        let (refused, blaze) = crate::run::findings(&files, "src/main.rs");
        (refused, blaze.advice.iter().map(|a| (a.rule, std::string::String::from(&*a.message))).collect::<Vec<_>>(), blaze.unchecked(), blaze.note())
    };
    let (refused, advice, unchecked, note) = run("#[blaze::noalloc]\n#[blaze::nopanic]\n#[rustfmt::skip]\nfn a() {}\n#[blaze::noalloc]\nfn b() {}");
    assert!(refused.is_empty() && advice.is_empty(), "{refused:?} {advice:?}");
    assert_eq!(unchecked, (3, 2), "three uses of two names, none checked");
    assert_eq!(note.as_deref(), Some("3 `blaze::` attributes of 2 kinds are recorded but not checked by this build"));

    let (refused, advice, unchecked, note) = run("#[blaze::noaloc]\nfn a() {}");
    assert!(refused.is_empty(), "advice never refuses: {refused:?}");
    assert_eq!(
        advice,
        [(
            RefusalRule::UnknownBlazeAttribute,
            "`blaze::noaloc` is not an attribute this build of blaze knows; the nearest known is `blaze::noalloc`".into()
        )]
    );
    assert_eq!((unchecked, note), ((0, 0), None));

    let (_, advice, _, _) = run("#[blaze::zzzzzzzz]\nfn a() {}");
    assert_eq!(advice[0].1, "`blaze::zzzzzzzz` is not an attribute this build of blaze knows");

    let (refused, _, _, _) = run("#[frobnicate]\nfn a() {}");
    assert_eq!(refused.len(), 1, "an unknown plain attribute is still refused: {refused:?}");
    assert_eq!(refused[0].rule, RefusalRule::Unsupported);
}

/// `where const { COND }` is evaluated at the impl's arguments, through a `const fn` over the
/// type: `true` lets `0u8.flat()` check; `false`, and a condition that calls a fn that is not
/// `const` (an evaluation error), leave the impl's method unfound: the probe drops the impl,
/// so the finding is `NoMethod` (it does not carry the condition's reason yet). Never true.
#[test]
fn a_where_const_condition_is_evaluated_at_the_impl_arguments() {
    for (value, body, holds) in [("true", "const fn", true), ("false", "const fn", false), ("true", "fn", false)] {
        let text = std::format!(
            "#![feature(no_core, lang_items, blaze_comptime)]\n#![no_core]\n#[lang = \"sized\"] trait Sized {{}}\n\
             {body} small<T>() -> bool {{ {value} }}\n\
             trait Flat {{ fn flat(&self) -> i32 {{ 42 }} }}\nimpl<T> Flat for T where const {{ small::<T>() }} {{}}\n\
             fn main() {{ let _ = 0u8.flat(); }}"
        );
        let files = [SourceFile { path: "src/main.rs", text: &text }];
        let all = refusals(&files, "src/main.rs");
        if holds {
            assert!(all.is_empty(), "{value} {body}: {all:?}");
        } else {
            assert_eq!(all.len(), 1, "{value} {body}: {all:?}");
            assert!(all[0].message.contains("the first NoMethod"), "{value} {body}: {all:?}");
        }
    }
}

#[test]
fn a_missing_module_file_is_named() {
    let files = [SourceFile { path: "src/main.rs", text: "mod gone;\nfn main() {}" }];
    let all = refusals(&files, "src/main.rs");
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0].rule, RefusalRule::Program, "{all:?}");
    assert!(all[0].message.contains("gone"), "{}", all[0].message);
}

/// Every node kind the parser makes has the words a Rust programmer uses for it, so no
/// refusal shows a parser's kind name.
#[test]
fn every_node_kind_has_a_plain_name() {
    use parser::SyntaxKind;
    let late = [
        SyntaxKind::UNSAFE_BINDER_CAST_EXPR,
        SyntaxKind::UNSAFE_BINDER_TYPE,
        SyntaxKind::SPEC_CLAUSE,
        SyntaxKind::UNION_TYPE,
        SyntaxKind::RECORD_TYPE,
    ];
    let nodes = (SyntaxKind::ABI as u16..=SyntaxKind::YIELD_EXPR as u16).map(SyntaxKind::from).chain(late);
    let missing: Vec<SyntaxKind> = nodes.filter(|&k| syntax_name(k).is_none()).collect();
    assert!(missing.is_empty(), "node kinds with no plain name: {missing:?}");
    for k in [SyntaxKind::TRY_EXPR, SyntaxKind::CLOSURE_EXPR, SyntaxKind::FOR_EXPR, SyntaxKind::MACRO_CALL, SyntaxKind::ENUM] {
        let words = syntax_name(k).expect("named");
        assert!(!words.contains('_') || words.contains('`'), "{k:?}: {words}");
    }
    assert_eq!(syntax_name(SyntaxKind::TRY_EXPR), Some("the `?` operator"));
}
