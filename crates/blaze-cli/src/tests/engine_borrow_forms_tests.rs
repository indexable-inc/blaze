//! THE BORROW FORMS OF THE DERIVES (piece RB7, NEVER COMPILED when written, 2026-11-17): the
//! bodies clap's, serde's and thiserror's derives write, in miniature, through the engine's
//! check, on the seam the real projects take. A subpattern that peels a reference under an ADT
//! field (clap's `Some(Cmd::Hello { who })` against `Option<&Cmd>`) is lowered now, not refused
//! `PeelUnderAdtField`; a call of a head that writes lifetimes and could not be related (a
//! projection of a trait with a lifetime, `V::Value` of `Visitor<'de>`) is still refused, but its
//! note says which callee and why (rule 19), never the bare "the flow lowering refused a form".
//! Each miniature has a negative with one injected borrow error, which must be found with its
//! code (rule 23: the check ran on that body). The checks run on a toolchain whose core
//! declares `copy`, `drop` and `owned_box` (blaze-world `fixture::CORE_WITH_LANGS`, piece RB8);
//! one control keeps the plain core and shows the missing `copy` is still a refusal by name.
//! A child of main.rs's tests, beside engine_check_tests.rs, whose helpers it uses.

use super::*;
use super::engine_check_tests::{engine_opts, error_lines, opts};

/// clap's `Cmd` match, the derive's `(a, Self::V { x })` against `(&u8, &Self)`, and a variant
/// inside a wrapper's field.
const CLAP: &str = "pub enum Cmd {\n    Hello { who: u8 },\n    Quit,\n}\n\npub enum Opt<T> {\n    No,\n    Yes(T),\n}\n\npub fn run(c: Opt<&Cmd>) -> u8 {\n    match c {\n        Opt::Yes(Cmd::Hello { who }) => *who,\n        _ => 0,\n    }\n}\n\npub enum Two {\n    V { x: u8 },\n    W,\n}\n\nimpl Two {\n    pub fn pick(a: &u8, b: &Two) -> u8 {\n        match (a, b) {\n            (a, Self::V { x }) => { let _ = a; *x }\n            _ => 0,\n        }\n    }\n}\n\npub struct Wrapper {\n    pub inner: Two,\n}\n\npub fn unwrap(w: &Wrapper) -> u8 {\n    match w {\n        Wrapper { inner: Two::V { x } } => *x,\n        _ => 0,\n    }\n}\n";

/// [`CLAP`]'s `run` over `Opt<&mut Cmd>` with one injected error: `who: &mut u8` (the peel of
/// `&mut` makes the mode `ref mut`), `r` reborrows `*who`, and `*who = 2` writes it while `r`
/// is used after (E0506).
const CLAP_WRONG: &str = "pub enum Cmd {\n    Hello { who: u8 },\n    Quit,\n}\n\npub enum Opt<T> {\n    No,\n    Yes(T),\n}\n\npub fn keep(_: &u8) {}\n\npub fn run(c: Opt<&mut Cmd>) -> u8 {\n    match c {\n        Opt::Yes(Cmd::Hello { who }) => { let r: &u8 = &*who; *who = 2; keep(r); 0 }\n        _ => 0,\n    }\n}\n";

/// serde's `Deserialize` in miniature: a `Visitor<'de>` with a `visit_seq`-like fn, a
/// `Deserializer<'de>` whose `deserialize_seq` returns `V::Value`, and the derive's
/// `deserialize` calling it through the caller's `D: Deserializer<'de>`.
const SERDE: &str = "pub trait Visitor<'de> {\n    type Value;\n    fn visit_seq(self, n: &'de [u8]) -> Self::Value;\n}\n\npub trait Deserializer<'de> {\n    fn deserialize_seq<V: Visitor<'de>>(self, v: V) -> V::Value;\n}\n\npub struct PointVisitor;\n\nimpl<'de> Visitor<'de> for PointVisitor {\n    type Value = u8;\n    fn visit_seq(self, n: &'de [u8]) -> u8 {\n        let _ = n;\n        0\n    }\n}\n\npub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> u8 {\n    d.deserialize_seq(PointVisitor)\n}\n";

/// [`SERDE`] with one injected error in the visitor's `visit_seq`: a shared borrow while a
/// mutable one is live (E0502).
const SERDE_WRONG: &str = "pub trait Visitor<'de> {\n    type Value;\n    fn visit_seq(self, n: &'de [u8]) -> Self::Value;\n}\n\npub fn keep(_: &u8) {}\n\npub struct PointVisitor;\n\nimpl<'de> Visitor<'de> for PointVisitor {\n    type Value = u8;\n    fn visit_seq(self, n: &'de [u8]) -> u8 {\n        let _ = n;\n        let mut v = 1u8;\n        let a = &mut v;\n        let b = &v;\n        keep(b);\n        *a = 2;\n        0\n    }\n}\n";

/// thiserror's `Display` in miniature: `fmt(&self, f: &mut Fmt<'_>)` writing through a helper
/// whose head writes lifetimes, and an `Arguments<'_>` builder.
const FAULT: &str = "pub struct Fmt<'a> {\n    pub buf: &'a mut u8,\n}\n\npub trait Show {\n    fn fmt(&self, f: &mut Fmt<'_>) -> u8;\n}\n\npub fn write_code(f: &mut Fmt<'_>, c: &u8) -> u8 {\n    *f.buf = *c;\n    0\n}\n\npub struct Fault {\n    code: u8,\n}\n\nimpl Show for Fault {\n    fn fmt(&self, f: &mut Fmt<'_>) -> u8 {\n        write_code(f, &self.code)\n    }\n}\n\npub struct Arguments<'a> {\n    pub parts: &'a [u8],\n    pub arg: &'a u8,\n}\n\nimpl<'a> Arguments<'a> {\n    pub fn new(parts: &'a [u8], arg: &'a u8) -> Arguments<'a> {\n        Arguments { parts, arg }\n    }\n}\n\npub fn build<'a>(p: &'a [u8], a: &'a u8) -> Arguments<'a> {\n    Arguments::new(p, a)\n}\n";

/// [`FAULT`]'s `Fmt` with one injected error: a shared borrow of `*c` while `f` holds it
/// mutably and is used after (E0502).
const FMT_WRONG: &str = "pub struct Fmt<'a> {\n    pub buf: &'a mut u8,\n}\n\npub fn write_code(f: &mut Fmt<'_>, c: &u8) -> u8 {\n    *f.buf = *c;\n    0\n}\n\npub fn bad(c: &mut u8) -> u8 {\n    let mut f = Fmt { buf: &mut *c };\n    let r: &u8 = &*c;\n    write_code(&mut f, r)\n}\n";

/// [`FAULT`]'s `Arguments` with one injected error: an assignment to `x` while the built
/// `Arguments` holds it and is used after (E0506).
const ARGS_WRONG: &str = "pub struct Arguments<'a> {\n    pub parts: &'a [u8],\n    pub arg: &'a u8,\n}\n\nimpl<'a> Arguments<'a> {\n    pub fn new(parts: &'a [u8], arg: &'a u8) -> Arguments<'a> {\n        Arguments { parts, arg }\n    }\n}\n\npub fn keep(_: &u8) {}\n\npub fn bad(p: &[u8]) -> u8 {\n    let mut x = 1u8;\n    let a = Arguments::new(p, &x);\n    x = 2;\n    keep(a.arg);\n    x\n}\n";

/// blaze-world's fixture shelf whose core declares the borrow check's lang items
/// (`fixture::CORE_WITH_LANGS`: `copy`, `drop`, `owned_box`), written on the home `/h` beside
/// the plain one (main.rs `shelf`), in `alloc`: its id.
fn shelf_with_langs(io: &Io, alloc: blaze_own::Alloc<'_>) -> blaze_toolchain::ToolchainId {
    let (id, files) = blaze_world::fixture::shelf_with_langs(graph::TRIPLE, blaze_world::fixture::STD, SHELF_CFG, alloc);
    for (name, bytes) in files {
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let path = Path::new(&format!("h/{}/{name}", toolchain::SHELF)).expect("a path");
        io.need_fs().expect("files").root().publish(&cx, &path, Content::bytes(&bytes), Durable::No).expect("write");
    }
    id
}

/// Which core the check's toolchain has.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Core {
    /// `fixture::CORE_WITH_LANGS`: bodies naming an ADT are borrow-checked.
    Langs,
    /// `fixture::CORE`: no `Copy` lang item, so such a body's borrows are not checked (the
    /// refusal [`engine_check_without_the_copy_lang_item_checks_no_borrows`] keeps).
    Plain,
}

/// One check of `text` on 2 workers: what it printed, the codes it listed, and its notes.
fn check_text(text: &str) -> (std::string::String, std::vec::Vec<std::string::String>, std::vec::Vec<std::string::String>) {
    check_text_on(text, Core::Langs)
}

/// [`check_text`] on the toolchain whose core is `core`.
fn check_text_on(text: &str, core: Core) -> (std::string::String, std::vec::Vec<std::string::String>, std::vec::Vec<std::string::String>) {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", text);
    let before = stdout(&io).len();
    let mut o = opts(&io, 2, true);
    if core == Core::Langs {
        o.toolchain = shelf_with_langs(&io, o.heap.alloc());
    }
    let (ids, checked) = engine_opts(&io, o);
    let said = stdout(&io).split_off(before);
    let codes = ids.into_iter().map(|(code, _)| code).collect();
    let (notes, _) = &checked.bodies;
    (said, codes, notes.iter().map(|n| n.to_string()).collect())
}

/// Each note of a body whose borrows were not checked says why, by name: never the bare
/// lowering refusal of a call or a peel, and a call refused for its head names the callee's
/// head and what it lacked, with the shape's or the alignment's reason (rule 19).
fn notes_say_why(case: &str, notes: &[std::string::String], said: &str) {
    for note in notes {
        assert!(!note.contains("the flow lowering refused a form"), "{case}: a form the lowering refused without words: {note}\n{said}");
        if note.contains("CallUnrelated {") || note.contains("the call could not be related") {
            let reason = note.split_once("borrows not checked: [")
                .and_then(|(_, rest)| rest.split_once("] the call could not be related through the callee's signature"))
                .map(|(reason, _)| reason);
            assert!(reason.is_some_and(|r| r.starts_with("Shape/") || r.starts_with("Unaligned/") || r == "Bounds"), "{case}: the call's reason leads its note: {note}\n{said}");
            let worded = note.contains("shape_why: Some(") || (note.contains("Unaligned(") && !note.contains("Unaligned(NotTried)")) || note.contains("lacks: Bounds");
            assert!(worded, "{case}: a call refused without its reason: {note}\n{said}");
            assert!(note.contains("head: \"") || note.contains("fn "), "{case}: the callee is named by its head: {note}\n{said}");
        }
    }
}

/// THE RULE (A): clap's peels under a field lower: no note of a refused form, no error.
#[test]
fn engine_check_lowers_a_peel_under_an_adt_field() {
    let (said, codes, notes) = check_text(CLAP);
    println!("clap miniature: codes {codes:?}, notes {notes:?}\n{said}");
    notes_say_why("clap", &notes, &said);
    assert!(notes.iter().all(|n| !n.contains("PeelUnderAdtField")), "the peel is lowered: {notes:?}\n{said}");
    assert!(error_lines(&said).is_empty(), "clean text, no error: {said}");
}

/// RULE 23 for (A): one injected borrow error under the peel is found: E0506. Why that code,
/// from the program: `c: Opt<&mut Cmd>` matched by `Opt::Yes(Cmd::Hello { who })` meets `&mut
/// Cmd` with a non-reference pattern, so the default binding mode becomes `ref mut` (the
/// Reference, "Binding modes") and `who: &mut u8` borrows `(*c.0).who`; `r = &*who` is a
/// shared loan of `*who`, live until `keep(r)`; `*who = 2` between them writes a place that
/// loan covers: "cannot assign to `*who` because it is borrowed", E0506 (not E0502: the
/// conflicting access is an assignment, not a new borrow).
#[test]
fn engine_check_finds_a_borrow_error_under_a_peel() {
    let (said, codes, notes) = check_text(CLAP_WRONG);
    println!("clap miniature, wrong: codes {codes:?}, notes {notes:?}\n{said}");
    notes_say_why("clap wrong", &notes, &said);
    // At this seam a borrow finding's id is its rule, `borrows` (first run, 2026-11-18: `codes
    // ["borrows"]`); the E-code named above is which finding the flow check makes of it.
    assert!(codes.iter().any(|c| c == "borrows"), "the assignment to the borrowed field is found: {codes:?}\n{said}");
}

/// (B) serde's `Deserialize`: every body's note says why; the visitor's body is checked. A call
/// of `deserialize_seq` through `D: Deserializer<'de>` may stay refused (its `V::Value` is a
/// projection of a trait with a lifetime, whose `'de` is the caller's: see the lane's report),
/// but then by name and reason.
#[test]
fn engine_check_words_a_deserialize_call() {
    let (said, codes, notes) = check_text(SERDE);
    println!("serde miniature: codes {codes:?}, notes {notes:?}\n{said}");
    notes_say_why("serde", &notes, &said);
    assert!(error_lines(&said).is_empty(), "clean text, no error: {said}");
}

/// RULE 23 for (B): the visitor's injected borrow error is found: E0502. From the program: `a =
/// &mut v` is a mutable loan of `v` live until `*a = 2`; `b = &v` between them is a NEW shared
/// borrow of the same place: "cannot borrow `v` as immutable because it is also borrowed as
/// mutable", E0502. The body names `PointVisitor` (`self`), an ADT, so before the fixture core
/// declared `copy` it was not checked at all.
#[test]
fn engine_check_finds_a_borrow_error_in_a_visitor() {
    let (said, codes, notes) = check_text(SERDE_WRONG);
    println!("serde miniature, wrong: codes {codes:?}, notes {notes:?}\n{said}");
    notes_say_why("serde wrong", &notes, &said);
    assert!(codes.iter().any(|c| c == "borrows"), "the visitor's injected borrow error is found: {codes:?}\n{said}");
}

/// (C) thiserror's `Display` and an `Arguments<'_>` builder: every note says why, the clean text
/// has no error, and each negative's one injected error is found with its code.
#[test]
fn engine_check_lowers_display_and_arguments() {
    let (said, codes, notes) = check_text(FAULT);
    println!("thiserror miniature: codes {codes:?}, notes {notes:?}\n{said}");
    notes_say_why("thiserror", &notes, &said);
    assert!(error_lines(&said).is_empty(), "clean text, no error: {said}");
}

/// RULE 23 for (C): each negative's one injected error is found with its code. From the
/// programs: in [`FMT_WRONG`], `f = Fmt { buf: &mut *c }` holds a mutable loan of `*c` in `f`,
/// live until `write_code(&mut f, r)`; `r = &*c` before that is a new shared borrow of `*c`:
/// E0502. In [`ARGS_WRONG`], `a = Arguments::new(p, &x)` puts the loan of `x` into `a` (the
/// head's `arg: &'a u8` and output `Arguments<'a>` share `'a`), live until `keep(a.arg)`; `x =
/// 2` between them assigns the borrowed place: E0506.
#[test]
fn engine_check_finds_the_borrow_errors_in_display_and_arguments() {
    for (case, text, code) in [("fmt wrong", FMT_WRONG, "E0502"), ("arguments wrong", ARGS_WRONG, "E0506")] {
        let (said, codes, notes) = check_text(text);
        println!("{case}: codes {codes:?}, notes {notes:?}\n{said}");
        notes_say_why(case, &notes, &said);
        assert!(codes.iter().any(|c| c == "borrows"), "{case}: the injected error ({code}) is found as a `borrows` finding: {codes:?}\n{said}");
    }
}

/// CONTROL (rule 23) for the lang items: the same negative under the peel on the plain core,
/// which declares no `copy`, is NOT borrow-checked: its note says `NoLangItem` (a refusal by
/// name, never a guess that an ADT is not `Copy`) and E0506 is not listed. So the E0506 of
/// [`engine_check_finds_a_borrow_error_under_a_peel`] comes from the lang item.
#[test]
fn engine_check_without_the_copy_lang_item_checks_no_borrows() {
    let (said, codes, notes) = check_text_on(CLAP_WRONG, Core::Plain);
    println!("clap miniature, wrong, plain core: codes {codes:?}, notes {notes:?}\n{said}");
    assert!(notes.iter().any(|n| n.contains("NoLangItem")), "the missing lang item is named: {notes:?}\n{said}");
    assert!(codes.iter().all(|c| c != "borrows"), "no borrow check ran on `run`: {codes:?}\n{said}");
}

/// Clap's `value_parser!` autoref specialization (`&Infer<T>: for<'s> From<&'s str>` vs
/// `Infer<T>: FromStr`) with a type like `alloy_primitives::Bytes` that implements only
/// `From<&'static str>` + `FromStr`: the higher-ranked bound must reject the `'static`-only
/// impl so method resolution falls through to `FromStr` (while `impl<'a> From<&'a str>` still
/// picks the higher-ranked impl), with zero type or borrow errors.
#[test]
fn clap_autoref_hrtb_rejects_static_from_impl() {
    const SRC: &str = r#"
pub trait MyFrom<T> {
    fn from_val(v: T) -> Self;
}
pub trait MyFromStr: Sized {
    fn parse(s: &str) -> Self;
}
pub struct Bytes;
impl MyFrom<&'static str> for Bytes {
    fn from_val(_: &'static str) -> Self { Bytes }
}
impl MyFromStr for Bytes {
    fn parse(_: &str) -> Self { Bytes }
}
pub struct Owned;
impl<'a> MyFrom<&'a str> for Owned {
    fn from_val(_: &'a str) -> Self { Owned }
}
impl MyFromStr for Owned {
    fn parse(_: &str) -> Self { Owned }
}
pub struct Infer<T>(*const T);
impl<T> Infer<T> {
    pub fn new() -> Self { Infer(0 as *const T) }
}
pub trait ViaRefStr {
    fn pick(&self) -> u32;
}
impl<T: for<'s> MyFrom<&'s str>> ViaRefStr for &Infer<T> {
    fn pick(&self) -> u32 { 1 }
}
pub trait ViaFromStr {
    fn pick(&self) -> bool;
}
impl<T: MyFromStr> ViaFromStr for Infer<T> {
    fn pick(&self) -> bool { true }
}
pub fn check_bytes() -> bool {
    (&&Infer::<Bytes>::new()).pick()
}
pub fn check_owned() -> u32 {
    (&&Infer::<Owned>::new()).pick()
}
pub enum MyOpt<T> {
    None,
    Some(T),
}
pub trait MyIter {
    type Item;
    fn next_item(&mut self) -> MyOpt<Self::Item>;
}
pub struct FilterWrap<I, P> {
    iter: I,
    pred: P,
}
impl<I: MyIter, P> MyIter for FilterWrap<I, P>
where
    P: for<'a> MyFrom<&'a I::Item>,
{
    type Item = I::Item;
    fn next_item(&mut self) -> MyOpt<Self::Item> { MyOpt::None }
}
pub struct U8Iter;
impl MyIter for U8Iter {
    type Item = u8;
    fn next_item(&mut self) -> MyOpt<u8> { MyOpt::None }
}
impl<'a> MyFrom<&'a u8> for Owned {
    fn from_val(_: &'a u8) -> Self { Owned }
}
pub fn check_filter_assoc(mut f: FilterWrap<U8Iter, Owned>) -> MyOpt<u8> {
    f.next_item()
}
"#;
    let (said, codes, notes) = check_text(SRC);
    assert!(error_lines(&said).is_empty(), "expected 0 errors (codes={codes:?}, notes={notes:?}):\n{said}");
}

/// Blocker C6 (`sqlx-core/src/migrate/source.rs` `ResolveError`) + all `thiserror_impl`
/// `Option<E>` source/from/transparent/ref/qualified-path/macro/const-generic shapes: runs the
/// real `blaze_proc_macro::comptime_patch::try_expand` output through Blaze's own front2
/// typechecker and borrow checker (no `rustc`), asserting 0 errors and complete checking, plus a
/// negative control confirming the old `&Option<E> as &(dyn Error + 'static)` triggers E0605.
#[test]
fn thiserror_option_source_unwraps_instead_of_casting_option() {
    use blaze_expand::{Delim, Edition, LitKind, ProcCall, ProcKind, Spacing, TokenKind, TokenStream, lex};
    use blaze_proc_macro::comptime_patch::{PatchResult, try_expand};

    fn render_stream(stream: &TokenStream<'_>) -> std::string::String {
        let tokens = stream.tokens();
        let mut out = std::string::String::new();
        let mut prev_joint = false;
        for (i, tok) in tokens.iter().enumerate() {
            match tok.kind {
                TokenKind::Open(d) => {
                    if !out.is_empty() && !prev_joint && !out.ends_with(['(', '[', '{', '<', ':']) {
                        out.push(' ');
                    }
                    out.push(match d {
                        Delim::Paren => '(',
                        Delim::Bracket => '[',
                        Delim::Brace => '{',
                        Delim::Invisible => ' ',
                    });
                    prev_joint = true;
                }
                TokenKind::Close(d) => {
                    out.push(match d {
                        Delim::Paren => ')',
                        Delim::Bracket => ']',
                        Delim::Brace => '}',
                        Delim::Invisible => ' ',
                    });
                    prev_joint = false;
                }
                TokenKind::Punct { char: c, spacing } => {
                    if !out.is_empty()
                        && !prev_joint
                        && ((c == ':' && out.ends_with(':'))
                            || (!matches!(c, ',' | ';' | ':' | '.' | '>' | '!' | '?')
                                && !out.ends_with(['(', '[', '{', ':', '\'', '<', '&', '#'])))
                    {
                        out.push(' ');
                    }
                    out.push(c);
                    prev_joint = spacing == Spacing::Joint || matches!(c, '\'' | '#');
                }
                TokenKind::Ident { .. } => {
                    if !out.is_empty()
                        && !prev_joint
                        && !out.ends_with(['(', '[', '{', '<', '\'', '.', '&', '#'])
                        && !out.ends_with("::")
                    {
                        out.push(' ');
                    }
                    out.push_str(stream.text_at(i));
                    prev_joint = false;
                }
                TokenKind::Literal { kind, suffix_len, .. } => {
                    if !out.is_empty()
                        && !prev_joint
                        && !out.ends_with(['(', '[', '{', '<', ':', '\'', '.', '&', '#'])
                    {
                        out.push(' ');
                    }
                    let text = stream.text_at(i);
                    let split = text.len().saturating_sub(suffix_len as usize);
                    let (inside, suffix) = text.split_at(split);
                    match kind {
                        LitKind::Str => {
                            out.push('"');
                            out.push_str(inside);
                            out.push('"');
                            out.push_str(suffix);
                        }
                        LitKind::Char => {
                            out.push('\'');
                            out.push_str(inside);
                            out.push('\'');
                            out.push_str(suffix);
                        }
                        _ => out.push_str(text),
                    }
                    prev_joint = false;
                }
            }
        }
        out
    }

    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let expand = |src: &str| -> std::string::String {
        let stream = lex(src, Edition::E2021, alloc).expect("lex derive input");
        let call = ProcCall {
            kind: ProcKind::Derive,
            input: &stream,
            attr_args: None,
            file: Ok("/workspace/src/lib.rs"),
        };
        let PatchResult::Expanded(prod) =
            try_expand("thiserror_impl", ProcKind::Derive, "Error", &call, Edition::E2021, alloc)
        else {
            panic!("expected comptime patch expansion for:\n{src}");
        };
        render_stream(&prod.tokens)
            .replace("::core::fmt::", " crate::mini_core::fmt::")
            .replace("::core::error::", " crate::mini_core::error::")
            .replace("::core::option::", " crate::mini_core::option::")
            .replace("::core::convert::", " crate::mini_core::convert::")
            .replace("::core::write!", " crate::mini_write!")
    };

    let resolve_impl = expand(r#"#[error("{code}")] pub struct ResolveError { pub code: u8, #[source] pub source: Option<IoErr> }"#);
    let implicit_impl = expand(r#"#[error("implicit")] pub struct ImplicitOptErr { pub source: Option<IoErr> }"#);
    let tuple_impl = expand(r#"#[error("tuple")] pub struct TupleOptErr(#[source] pub Option<IoErr>);"#);
    let enum_impl = expand(r#"
        pub enum OptEnumErr {
            #[error("named {code}")]
            Named { code: u8, #[source] cause: Option<IoErr> },
            #[error("implicit")]
            Implicit { source: Option<IoErr> },
            #[error("tuple")]
            Tuple(#[source] Option<IoErr>),
            #[error("plain")]
            Plain,
        }
    "#);
    let from_named_impl = expand(r#"#[error("from named")] pub struct FromOptNamed { #[from] pub source: Option<IoErr> }"#);
    let from_tuple_impl = expand(r#"#[error("from tuple")] pub struct FromOptTuple(#[from] pub Option<IoErr>);"#);
    let from_enum_impl = expand(r#"
        pub enum FromOptEnum {
            #[error("io")]
            Io { #[from] source: Option<IoErr> },
            #[error("fmt")]
            Fmt(#[from] Option<FmtErr>),
        }
    "#);
    let ref_impl = expand(r#"#[error("ref")] pub struct RefSourceErr { #[source] pub source: &'static IoErr }"#);
    let opt_ref_impl = expand(r#"#[error("opt ref")] pub struct OptRefSourceErr { #[source] pub source: Option<&'static IoErr> }"#);
    let from_opt_ref_impl = expand(r#"#[error("from opt ref")] pub struct FromOptRefErr { #[from] pub source: Option<&'static IoErr> }"#);
    let custom_opt_impl = expand(r#"#[error("custom")] pub struct CustomOptErr { #[source] pub source: Option<super::IoErr> }"#);
    let custom_opt_enum_impl = expand(r#"pub enum CustomOptEnum { #[error("custom")] Wrapped(#[source] Option<super::IoErr>) }"#);
    let trans_custom_impl = expand(r#"#[error(transparent)] pub struct TransCustomOpt(pub Option<super::IoErr>);"#);
    let nested_custom_impl = expand(r#"#[error("nested")] pub struct NestedCustomOpt { #[source] pub source: super::Option<Option<super::IoErr>> }"#);
    let qpath_impl = expand(r#"#[error("qpath")] pub struct QPathOptErr { #[source] pub source: <Provider as Factory>::Option<super::IoErr> }"#);
    let lit_qpath_impl = expand(r#"#[error("lit qpath")] pub struct LitQPathOptErr { #[source] pub source: <LitProvider<'>'> as Factory>::Option<super::IoErr> }"#);
    let unicode_impl = expand(r#"#[error("unicode")] pub struct UnicodeOptErr { #[source] pub source: módulo::Option<IoErr> }"#);
    let braced_macro_impl = expand(r#"#[error("macro")] pub struct BracedMacroOptErr { #[source] pub source: Option<error_ty! { super::IoErr, () }> }"#);
    let const_opt_impl = expand(r#"#[error("const opt")] pub struct ConstOptErr { #[from] pub source: Option<3> }"#);

    let clean = format!(
        r#"
#[lang = "sized"]
pub trait Sized {{}}
#[lang = "unsize"]
pub trait Unsize<T: ?Sized> {{}}
#[lang = "coerce_unsized"]
pub trait CoerceUnsized<T: ?Sized> {{}}
impl<'a, 'b: 'a, T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<&'a U> for &'b T {{}}

#[macro_export]
macro_rules! mini_write {{
    ($f:expr, $lit:literal $(, $arg:expr)*) => {{{{
        let _ = ($f, $lit $(, &$arg)*);
        crate::mini_core::fmt::Result::Ok(())
    }}}};
}}

pub mod mini_core {{
    pub mod option {{
        pub enum Option<T> {{
            None,
            Some(T),
        }}
        impl<T> Option<T> {{
            pub fn as_ref(&self) -> Option<&T> {{
                match self {{
                    Option::Some(x) => Option::Some(x),
                    Option::None => Option::None,
                }}
            }}
        }}
    }}
    pub mod fmt {{
        pub struct Formatter<'a> {{
            pub dummy: &'a u8,
        }}
        pub enum Result {{
            Ok(()),
            Err,
        }}
        impl<'a> Formatter<'a> {{
            pub fn write_str(&mut self, _s: &str) -> Result {{ Result::Ok(()) }}
        }}
        pub trait Display {{
            fn fmt(&self, f: &mut Formatter<'_>) -> Result;
        }}
    }}
    pub mod convert {{
        use crate::Sized;
        pub trait From<T>: Sized {{
            fn from(value: T) -> Self;
        }}
    }}
    pub mod error {{
        use crate::Sized;
        use super::option::Option;
        pub trait Error {{
            fn source(&self) -> Option<&(dyn Error + 'static)> {{ Option::None }}
        }}
        impl<T: Error + ?Sized> Error for &T {{
            fn source(&self) -> Option<&(dyn Error + 'static)> {{ (**self).source() }}
        }}
    }}
}}

pub use crate::mini_core::option::Option;

pub struct IoErr;
impl crate::mini_core::fmt::Display for IoErr {{
    fn fmt(&self, _f: &mut crate::mini_core::fmt::Formatter<'_>) -> crate::mini_core::fmt::Result {{
        crate::mini_core::fmt::Result::Ok(())
    }}
}}
impl crate::mini_core::error::Error for IoErr {{}}

pub struct FmtErr;
impl crate::mini_core::fmt::Display for FmtErr {{
    fn fmt(&self, _f: &mut crate::mini_core::fmt::Formatter<'_>) -> crate::mini_core::fmt::Result {{
        crate::mini_core::fmt::Result::Ok(())
    }}
}}
impl crate::mini_core::error::Error for FmtErr {{}}

pub struct ResolveError {{ pub code: u8, pub source: Option<IoErr> }}
{resolve_impl}

pub struct ImplicitOptErr {{ pub source: Option<IoErr> }}
{implicit_impl}

pub struct TupleOptErr(pub Option<IoErr>);
{tuple_impl}

pub enum OptEnumErr {{
    Named {{ code: u8, cause: Option<IoErr> }},
    Implicit {{ source: Option<IoErr> }},
    Tuple(Option<IoErr>),
    Plain,
}}
{enum_impl}

pub struct FromOptNamed {{ pub source: Option<IoErr> }}
{from_named_impl}

pub struct FromOptTuple(pub Option<IoErr>);
{from_tuple_impl}

pub enum FromOptEnum {{
    Io {{ source: Option<IoErr> }},
    Fmt(Option<FmtErr>),
}}
{from_enum_impl}

pub struct RefSourceErr {{ pub source: &'static IoErr }}
{ref_impl}

pub struct OptRefSourceErr {{ pub source: Option<&'static IoErr> }}
{opt_ref_impl}

pub struct FromOptRefErr {{ pub source: Option<&'static IoErr> }}
{from_opt_ref_impl}

pub mod módulo {{
    pub use crate::mini_core::option::Option;
}}
pub struct UnicodeOptErr {{ pub source: módulo::Option<IoErr> }}
{unicode_impl}

pub mod custom_opt {{
    pub struct Option<T>(pub crate::mini_core::option::Option<T>);
    impl<T> Option<T> {{
        pub fn as_ref(&self) -> crate::mini_core::option::Option<&T> {{ self.0.as_ref() }}
    }}
    impl<T: crate::mini_core::fmt::Display> crate::mini_core::fmt::Display for Option<T> {{
        fn fmt(&self, f: &mut crate::mini_core::fmt::Formatter<'_>) -> crate::mini_core::fmt::Result {{
            match &self.0 {{
                crate::mini_core::option::Option::Some(v) => crate::mini_core::fmt::Display::fmt(v, f),
                crate::mini_core::option::Option::None => f.write_str("none"),
            }}
        }}
    }}
    impl<T: crate::mini_core::error::Error + 'static> crate::mini_core::error::Error for Option<T> {{
        fn source(&self) -> crate::mini_core::option::Option<&(dyn crate::mini_core::error::Error + 'static)> {{
            match self.0.as_ref() {{
                crate::mini_core::option::Option::Some(e) => crate::mini_core::option::Option::Some(e as &(dyn crate::mini_core::error::Error + 'static)),
                crate::mini_core::option::Option::None => crate::mini_core::option::Option::None,
            }}
        }}
    }}
    pub struct CustomOptErr {{ pub source: Option<super::IoErr> }}
    {custom_opt_impl}
    pub enum CustomOptEnum {{ Wrapped(Option<super::IoErr>) }}
    {custom_opt_enum_impl}
    pub struct TransCustomOpt(pub Option<super::IoErr>);
    {trans_custom_impl}
    pub struct NestedCustomOpt {{ pub source: super::Option<Option<super::IoErr>> }}
    {nested_custom_impl}
}}

pub mod qpath_and_macro {{
    use crate::mini_core::option::Option;
    pub trait Factory {{
        type Option<T>;
    }}
    pub struct Provider;
    impl Factory for Provider {{
        type Option<T> = Option<T>;
    }}
    pub struct LitProvider<const C: char>;
    impl Factory for LitProvider<'>'> {{
        type Option<T> = Option<T>;
    }}
    pub struct QPathOptErr {{ pub source: <Provider as Factory>::Option<super::IoErr> }}
    {qpath_impl}
    pub struct LitQPathOptErr {{ pub source: <LitProvider<'>'> as Factory>::Option<super::IoErr> }}
    {lit_qpath_impl}
    macro_rules! error_ty {{
        ($t:ty, $_ignored:ty) => {{ $t }};
    }}
    pub struct BracedMacroOptErr {{ pub source: Option<error_ty! {{ super::IoErr, () }}> }}
    {braced_macro_impl}
}}

pub mod const_opt {{
    pub struct Option<const N: usize>;
    impl<const N: usize> crate::mini_core::fmt::Display for Option<N> {{
        fn fmt(&self, f: &mut crate::mini_core::fmt::Formatter<'_>) -> crate::mini_core::fmt::Result {{ f.write_str("const-opt") }}
    }}
    impl<const N: usize> crate::mini_core::error::Error for Option<N> {{}}
    pub struct ConstOptErr {{ pub source: Option<3> }}
    {const_opt_impl}
}}

pub fn exercise(leaf: &'static IoErr) {{
    use crate::mini_core::convert::From as _;
    use crate::mini_core::error::Error as _;
    let _ = FromOptNamed::from(IoErr).source();
    let _ = FromOptTuple::from(IoErr).source();
    let _ = FromOptEnum::from(IoErr).source();
    let _ = FromOptEnum::from(FmtErr).source();
    let _ = RefSourceErr {{ source: leaf }}.source();
    let _ = OptRefSourceErr {{ source: Option::Some(leaf) }}.source();
    let _ = FromOptRefErr::from(leaf).source();
    let _ = const_opt::ConstOptErr::from(const_opt::Option::<3>).source();
}}
"#
    );
    let (said, codes, notes) = check_text(&clean);
    assert!(
        error_lines(&said).is_empty() && !said.contains("NOT checked"),
        "expected 0 errors and complete front2 check for all comptime-expanded thiserror Option shapes (codes={codes:?}, notes={notes:?}):\n{said}\n--- SOURCE ---\n{clean}"
    );

    // Negative control: casting `&Opt<IoErr>` directly to `&(dyn Error + 'static)` fails E0605.
    const BAD_CAST: &str = r#"
#[lang = "sized"]
pub trait Sized {}
#[lang = "unsize"]
pub trait Unsize<T: ?Sized> {}
#[lang = "coerce_unsized"]
pub trait CoerceUnsized<T: ?Sized> {}
impl<'a, 'b: 'a, T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<&'a U> for &'b T {}

pub trait Error {
    fn source(&self) -> Opt<&(dyn Error + 'static)> { Opt::None }
}
pub enum Opt<T> {
    None,
    Some(T),
}
pub struct IoErr;
impl Error for IoErr {}

pub struct ResolveError {
    pub source: Opt<IoErr>,
}
impl Error for ResolveError {
    fn source(&self) -> Opt<&(dyn Error + 'static)> {
        Opt::Some(&self.source as &(dyn Error + 'static))
    }
}
"#;
    let (bad_said, bad_codes, _) = check_text(BAD_CAST);
    assert!(
        bad_said.contains("E0605"),
        "negative control must catch E0605 NonPrimitiveCast on `&Opt<IoErr> as &(dyn Error + 'static)` (codes={bad_codes:?}):\n{bad_said}"
    );
}
