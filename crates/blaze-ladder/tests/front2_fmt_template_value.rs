//! A `format_args!` template is parsed from the literal's VALUE, not its source text
//! (blaze-lower fmt.rs `template_value`, `format_args_of`).
//!
//! The real sites are turso core/functions/string.rs:206 `format!("{pad}q\u{e9}")`, :211
//! `format!("q\u{e9}{pad}")` and core/vdbe/value.rs:2226 `format!("{}\u{e9}a", ..)`. turso
//! c1851p gave "cannot find function `e9` in this scope (segment 0) (E0425)" at each: the `{`
//! of the escape `\u{e9}` was read as a format brace.
//!
//! The rule: rustc_builtin_macros format.rs parses the template's value (the unescaped
//! string) with `rustc_parse_format::Parser`, and keeps the snippet only for spans. The
//! Reference, "String literals": `\u{e9}` is the one char `é`; a raw literal's value is its
//! text.
//!
//! The cases:
//! - `a` and `c` are turso's shape, with a capture before and after the escape: clean.
//! - `x` is `"\x7bn}"`, whose value is `{n}`: a capture of `n`, clean. Before the fix, its `}`
//!   was unmatched.
//! - `d` is the twin rustc refuses: `{e9}` written as a real capture with no `e9`, E0425.
//! - `r` is the CONTROL: in the raw `r"\u{e9}"` the backslash is itself, so `{e9}` is a
//!   capture. With `let e9` it is clean.
//! - `rd` is the raw control's twin: the same template with no `e9`, E0425.
//!
//! CONTROL (rule 23): scanning the source text again (`format_args_of` on
//! `template.of(self.text())` instead of `template_value(..).value`) makes `a` and `c` E0425
//! `e9` and `x` a syntax error; `r` and `rd` do not change.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_items.rs.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::check::{BuildOrder, Records, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::lower_made::{LowerTarget, lower_made};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

/// turso is a Rust 2024 crate, with std's `format!` and `String`.
const EDITION: &str = "2024";

/// Each fn of `fns` in the crate `text`, lowered and checked in order over one store: its
/// name and its outcome ("clean", the diagnostics, or the failure as printed). The harness of
/// tests/front2_items.rs.
fn check_fns(text: &str, fns: &[&str]) -> Vec<(String, String)> {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut got = Vec::new();
    for &n in fns {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global())
            .unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
        let outcome = match lower_made(&mut records, prepared, LowerTarget::Free(k)) {
            Err(e) => format!("not lowered: {e}"),
            Ok(body) => {
                let view = body.lowered.view();
                let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `{n}`: {e:?}"));
                let scope = body.scope(&spell);
                match check_body(&mut records, &body, &scope) {
                    Ok(done) if done.diags.is_empty() => "clean".to_string(),
                    Ok(done) => {
                        let d: Vec<String> = done.diags.iter().map(|d| format!("{} at {:?}: `{}`", d.code.code(), d.at, text.get(d.at.start as usize..d.at.end as usize).unwrap_or("?"))).collect();
                        format!("diags {d:?}")
                    }
                    Err(e) => format!("not checked: {e}"),
                }
            }
        };
        got.push((n.to_string(), outcome));
    }
    got
}

/// Expected lines: "fmt value: a: clean", "fmt value: c: clean", "fmt value: x: clean",
/// "fmt value: d: diags [\"E0425 at ..\"]", "fmt value: r: clean", "fmt value: rd: diags
/// [\"E0425 at ..\"]".
#[test]
fn a_format_template_is_parsed_from_its_value() {
    const TEXT: &str = r#"fn a(pad: &str) -> String { format!("{pad}q\u{e9}") }
fn c(n: u8) -> String { format!("\u{e9}{n}") }
fn x(n: u8) -> String { format!("\x7bn}") }
fn d() -> String { format!("{e9}") }
fn r() -> String { let e9 = 1u8; format!(r"\u{e9}") }
fn rd() -> String { format!(r"\u{e9}") }
fn main() {}
"#;
    let got = check_fns(TEXT, &["a", "c", "x", "d", "r", "rd"]);
    let lines: Vec<String> = got.iter().map(|(n, o)| format!("fmt value: {n}: {o}")).collect();
    for l in &lines {
        println!("{l}");
    }
    let outcome = |n: &str| got.iter().find(|(m, _)| m == n).map(|(_, o)| o.as_str()).unwrap_or("");
    for n in ["a", "c", "x", "r"] {
        assert_eq!(outcome(n), "clean", "`{n}`:\n{}", lines.join("\n"));
    }
    for n in ["d", "rd"] {
        assert!(outcome(n).contains("E0425"), "`{n}`: want E0425 for `e9`:\n{}", lines.join("\n"));
    }
}
