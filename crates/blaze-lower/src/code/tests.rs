//! [`code_text_of`] on files written here: which comment edits a crate's code can observe.

use blaze_ids::ContentId;
use hegel::TestCase;
use hegel::generators as gs;
use span::Edition;
use syntax::{AstNode, SourceFile};

use super::{CodeText, Plant, code_text_of, planting, root_alone_of};

fn code(text: &str) -> CodeText {
    let parse = SourceFile::parse(text, Edition::CURRENT);
    code_text_of(parse.tree().syntax(), text)
}

/// An edit of a doc comment's text no macro sees (one doctest example of a library) leaves its
/// code text as it was, with the prelude's derives taken as inert or not.
#[test]
fn an_unseen_doc_edit_is_masked() {
    let before =
        code("/// ```\n/// assert_eq!(f(), 1);\n/// ```\n#[inline]\npub fn f() -> u8 { 1 }\n");
    let after =
        code("/// ```\n/// assert_eq!(f(), 2);\n/// ```\n#[inline]\npub fn f() -> u8 { 1 }\n");
    assert_eq!(before, after);
    let plain = code("// one\nfn f() {}\n");
    assert_eq!(plain, code("// two\nfn f() {}\n"), "a `//` comment is never seen");
    let derived = code("#[derive(Debug, Clone)]\nstruct S {\n    /// one\n    f: u8,\n}\n");
    assert_eq!(
        derived.inert,
        code("#[derive(Debug, Clone)]\nstruct S {\n    /// two\n    f: u8,\n}\n").inert
    );
    assert_ne!(
        derived.strict,
        code("#[derive(Debug, Clone)]\nstruct S {\n    /// two\n    f: u8,\n}\n").strict
    );
    assert!(!derived.shadows && !derived.whole);
}

/// A comment's shape stays: an edit that moves the code after it (another line, a block
/// comment's last line longer) changes the code text, for `line!()`, panics and debug info.
#[test]
fn a_comments_shape_is_kept() {
    let one = code("/// a\nfn f() { line!(); }\n");
    assert_ne!(one, code("/// a\n/// b\nfn f() { line!(); }\n"), "a line more");
    let block = code("/* ab */ fn f() { column!(); }\n");
    assert_eq!(block, code("/* cd */ fn f() { column!(); }\n"));
    assert_ne!(block, code("/* abc */ fn f() { column!(); }\n"), "a column more");
    assert_ne!(
        code("/* é */ fn f() {}\n"),
        code("/* ee */ fn f() {}\n"),
        "as many bytes, fewer characters"
    );
    assert_ne!(code("/// a\nfn f() { 1 }\n"), code("/// a\nfn f() { 2 }\n"), "the code itself");
}

/// A doc comment a macro is given keeps its text: in a macro call, a `macro_rules!` body, or
/// on an item (or under one) given to a derive or attribute macro of another crate.
#[test]
fn a_doc_a_macro_sees_is_kept() {
    let pairs = [
        ("m! {\n    /// one\n    fn f() {}\n}\n", "m! {\n    /// two\n    fn f() {}\n}\n"),
        (
            "macro_rules! m {\n    () => {\n        /// one\n        fn f() {}\n    };\n}\n",
            "macro_rules! m {\n    () => {\n        /// two\n        fn f() {}\n    };\n}\n",
        ),
        (
            "#[derive(Serialize)]\nstruct S {\n    /// one\n    f: u8,\n}\n",
            "#[derive(Serialize)]\nstruct S {\n    /// two\n    f: u8,\n}\n",
        ),
        (
            "/// one\n#[tokio::main]\nasync fn main() {}\n",
            "/// two\n#[tokio::main]\nasync fn main() {}\n",
        ),
        (
            "#[my_attr]\nmod m {\n    /// one\n    fn f() {}\n}\n",
            "#[my_attr]\nmod m {\n    /// two\n    fn f() {}\n}\n",
        ),
        (
            "mod m {\n    #![my_attr]\n    /// one\n    fn f() {}\n}\n",
            "mod m {\n    #![my_attr]\n    /// two\n    fn f() {}\n}\n",
        ),
        (
            "/// one\n#[cfg_attr(feature = \"x\", my_attr)]\nfn f() {}\n",
            "/// two\n#[cfg_attr(feature = \"x\", my_attr)]\nfn f() {}\n",
        ),
    ];
    for (one, two) in pairs {
        let (one_code, two_code) = (code(one), code(two));
        assert_ne!(one_code.inert, two_code.inert, "{one}");
        assert_ne!(one_code.strict, two_code.strict, "{one}");
    }
    let inert = (
        "/// one\n#[cfg_attr(test, inline, rustfmt::skip)]\nfn f() {}\n",
        "/// two\n#[cfg_attr(test, inline, rustfmt::skip)]\nfn f() {}\n",
    );
    assert_eq!(code(inert.0), code(inert.1), "a `cfg_attr` of inert attributes");
}

/// A file that could shadow a standard derive, `test` or `bench` says so, so its crate keys the
/// items those see by their docs too; an import from the standard library or the crate can't.
#[test]
fn a_shadowing_file_is_caught() {
    for text in [
        "use serde::Debug;\n",
        "use other::{Clone, Debug as Shown};\n",
        "use other::thing::Hash as Clone;\n",
        "use other::*;\n",
        "use other::{a::*, b};\n",
        "#[macro_use]\nextern crate other;\n",
        "macro_rules! test { () => {} }\n",
    ] {
        assert!(code(text).shadows, "{text}");
    }
    for text in [
        "use core::hash::{Hash, Hasher};\n",
        "use std::fmt::Debug;\n",
        "use crate::*;\n",
        "use super::*;\n",
        "use other::{Thing, More};\n",
        "use other::Thing as Other;\n",
        "#[macro_use]\nextern crate alloc;\n",
    ] {
        assert!(!code(text).shadows, "{text}");
    }
}

/// A file that reads bytes that may be Rust (`include_str!` of a `.rs` file or of a path that
/// is not one literal), or gives an out-of-line module to an attribute, makes its crate keep
/// its whole text; reading a README or an inert attribute on `mod x;` does not.
#[test]
fn a_file_its_code_may_read_whole_keeps_its_text() {
    for text in [
        "const S: &str = include_str!(\"lib.rs\");\n",
        "const S: &[u8] = include_bytes!(concat!(env!(\"OUT_DIR\"), \"/x.txt\"));\n",
        "const S: &str = include_str!(PATH);\n",
        "#[my_attr]\nmod other;\n",
    ] {
        assert!(code(text).whole, "{text}");
    }
    for text in [
        "#![doc = include_str!(\"../README.md\")]\n",
        "const S: &[u8] = include_bytes!(\"data.bin\");\n",
        "#[cfg(test)]\nmod tests;\n",
    ] {
        assert!(!code(text).whole, "{text}");
    }
}

fn alone(text: &str) -> bool {
    root_alone_of(SourceFile::parse(text, Edition::CURRENT).tree().syntax())
}

/// A root file that only prints, reads variables and starts programs (ripgrep's `build.rs`)
/// can read no other file of its source root.
#[test]
fn a_script_that_prints_and_runs_programs_reads_its_root_alone() {
    assert!(alone("fn main() {}\n"));
    assert!(alone(
        "use std::process::Command;\n\
         #[derive(Debug, Clone)]\nstruct Rev(String);\n\
         mod inline { pub fn f() -> u8 { 1 } }\n\
         fn main() {\n\
             let Ok(os) = std::env::var(\"CARGO_CFG_TARGET_OS\") else { return };\n\
             let out = Command::new(\"git\").args(&[\"rev-parse\", \"HEAD\"]).output();\n\
             if !os.is_empty() { println!(\"cargo:rustc-env=REV={}\", format!(\"{:?}\", out.is_ok())); }\n\
             std::println!(\"{}\", env!(\"CARGO_PKG_NAME\"));\n\
             assert!(!matches!(inline::f(), 2), \"{}\", line!());\n\
         }\n"
    ));
    // A dependency used for its functions, none of its names a macro's or a derive's.
    assert!(alone("use cc::Build;\nfn main() { Build::new().file(\"a.c\").compile(\"a\"); }\n"));
}

/// Anything that reads another file, or could expand to something that does, is refused: the
/// crate keeps its whole source root in its identity.
#[test]
fn what_could_read_another_file_is_not_alone() {
    let refused = [
        ("mod other;\nfn main() {}\n", "a module in another file"),
        ("#[path = \"../x.rs\"]\nmod other;\nfn main() {}\n", "a module at a path"),
        ("fn main() { let _ = include_str!(\"x.txt\"); }\n", "include_str!"),
        ("include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\n", "include!"),
        (
            "fn main() { println!(\"{}\", include_bytes!(\"x\").len()); }\n",
            "an include a macro is given",
        ),
        ("#[doc = include_str!(\"../README.md\")]\nfn main() {}\n", "an include in an attribute"),
        ("macro_rules! m { () => {}; }\nfn main() { m!(); }\n", "a macro defined here"),
        ("fn main() { other::mac!(); }\n", "another crate's macro"),
        ("fn main() { mac!(); }\n", "a macro of an unknown name"),
        ("#[macro_use]\nextern crate other;\nfn main() {}\n", "extern crate"),
        ("#[other::attr]\nfn main() {}\n", "an attribute macro"),
        ("#[derive(Other)]\nstruct S;\nfn main() {}\n", "a derive of another crate"),
        ("use other::*;\nfn main() { println!(); }\n", "a glob of another crate"),
        ("use crate::inner::*;\nmod inner {}\nfn main() {}\n", "a glob of its own"),
        ("use other::println;\nfn main() { println!(); }\n", "a plain macro's name imported"),
        ("use other::mac as vec;\nfn main() { let _: Vec<u8> = vec![]; }\n", "renamed to one"),
        ("use other::Debug;\n#[derive(Debug)]\nstruct S;\nfn main() {}\n", "a derive shadowed"),
        (
            "fn main() { println!(\"{}\", { #[path = \"x.rs\"] mod m; 1 }); }\n",
            "a module a macro is given",
        ),
        ("fn main() { println!(\"{}\", { use other::*; 1 }); }\n", "an import a macro is given"),
        ("fn main() { println!(\"{}\", other!()); }\n", "a macro a macro is given"),
        (
            "fn main() { println!(\"{}\", std::format!(\"\")); }\n",
            "a path's macro a macro is given",
        ),
    ];
    for (text, what) in refused {
        assert!(!alone(text), "{what}: {text}");
    }
}

/// A plain `//` comment is no part of the code text, there or not, with the spaces before it:
/// it ends its line, so no token moves. A line more moves every line after it.
#[test]
fn a_line_comment_at_a_lines_end_is_nothing() {
    let bare = code("fn f() {\n    g();\n}\n");
    assert_eq!(bare, code("fn f() { // opens\n    g();   // calls\n}\n"));
    assert_ne!(bare.inert, code("// before\nfn f() {\n    g();\n}\n").inert, "a line more");
    assert_eq!(
        code("\nfn f() {\n    g();\n}\n").inert,
        code("    // before\nfn f() {\n    g();\n}\n").inert,
        "a comment's line is an empty line"
    );
    // A comment the compiler reads stays text: a bidirectional control is denied.
    assert_ne!(bare.inert, code("fn f() { // \u{202E}\n    g();\n}\n").inert);
    assert_ne!(
        code("fn f() { // \u{202E} a\n    g();\n}\n").inert,
        code("fn f() { // \u{202E} b\n    g();\n}\n").inert
    );
}

/// What moves no token and no position is in no text, there or not: the spaces and tabs
/// before a line's end, and what is after the file's last token (a comment line appended,
/// blank lines, the last line's end). Spaces before a token are its column, and stay.
#[test]
fn what_moves_no_token_and_no_position_is_nothing() {
    let bare = code("fn f() {\n    g();\n}\n");
    for same in [
        "fn f() {  \n    g();\t\n}\n",
        "fn f() {\n    g();\n}\n// c\n",
        "fn f() {\n    g();\n}\n\n\n// one\n    // two\n\n",
        "fn f() {\n    g();\n}",
        "fn f() {\n    g();\n} // c",
        "fn f() {\n    g();\n}   ",
    ] {
        assert_eq!(bare, code(same), "{same:?}");
    }
    // In a file whose lines end `\r\n`, the spaces before a line's end are as little.
    assert_eq!(
        code("fn f() {\r\n    g();\r\n}\r\n").inert,
        code("fn f() {  \r\n    g(); \t\r\n}\r\n").inert
    );
    // Each with whether a crate that only checks against the file can observe it: what is
    // inside a plain body but its lines is in no interface.
    for (text, what, outside) in [
        ("fn f() {\n     g();\n}\n", "a column more, in a plain body", false),
        ("fn f()  {\n    g();\n}\n", "a column more, before a token on its line", true),
        ("fn f() {\n\n    g();\n}\n", "a line more", true),
        ("fn f() {\n    g();\n}\n/// doc\n", "a doc comment after the last item", true),
        ("fn f() {\n    g();\n}\n/* c */\n", "a block comment after the last item", true),
        ("fn f() {\n    g();\n}\n// \u{202E}\n", "a comment the compiler reads", true),
        ("fn f() {\n    h();\n}\n", "a token of a plain body", false),
    ] {
        let other = code(text);
        assert_ne!(bare.inert, other.inert, "{what}: {text:?}");
        assert_ne!(bare.strict, other.strict, "{what}: {text:?}");
        assert_eq!(bare.interface != other.interface, outside, "{what}: {text:?}");
    }
    // Whitespace inside a string is the string.
    assert_ne!(
        code("const A: &str = \"a \n b\";\n").inert,
        code("const A: &str = \"a\n b\";\n").inert
    );
    // A file of comments and whitespace alone has no code, whatever they say.
    assert_eq!(code("// one\n\n"), code(""));
}

/// A block comment that does not end is an error, so its text stays: no text of a file with
/// one is the text of a file that builds.
#[test]
fn a_block_comment_that_does_not_end_keeps_its_text() {
    let ended = code("fn f() {}\n/* a */ fn g() {}\n");
    assert_eq!(ended, code("fn f() {}\n/* b */ fn g() {}\n"));
    assert_ne!(ended.inert, code("fn f() {}\n/* a fn g() {}\n").inert);
    assert_ne!(
        code("fn f() {}\n/* abc").inert,
        code("fn f() {}\n/* a*/").inert,
        "as many bytes on as many lines"
    );
    assert_ne!(code("/* /* a */ b").inert, code("/* /* a */ */").inert, "a nested one");
    assert_ne!(code("/** a").inert, code("/** b").inert, "a doc comment's");
}

/// A comment on a line of its own before code moves every line after it, which `line!()`, a
/// panic's location and a diagnostic read: the text changes. What it says does not.
#[test]
fn a_comment_on_its_own_line_moves_the_lines_after_it() {
    let bare = code("fn f() -> u32 {\n    line!()\n}\n");
    let noted = code("fn f() -> u32 {\n    // which line\n    line!()\n}\n");
    assert_ne!(bare.inert, noted.inert);
    assert_ne!(bare.interface, noted.interface, "a dependent's diagnostic points here too");
    assert_eq!(noted, code("fn f() -> u32 {\n        // another thing said\n    line!()\n}\n"));
    assert_eq!(noted, code("fn f() -> u32 {\n\n    line!()\n}\n"), "its line is an empty line");
    // Before the first item, and between two.
    assert_ne!(code("fn f() {}\nfn g() {}\n").inert, code("fn f() {}\n// g\nfn g() {}\n").inert);
}

/// A doc comment is an attribute of its item: one that comes or goes changes the text (a
/// lint reads whether an item is documented), in a body too. What it says is read by rustdoc
/// and by a macro given its item, whose units and items keep it
/// ([`an_unseen_doc_edit_is_masked`], [`a_doc_a_macro_sees_is_kept`]).
#[test]
fn a_doc_comment_that_comes_or_goes_changes_the_text() {
    let bare = code("pub fn f() {}\n");
    let documented = code("/// What f does.\npub fn f() {}\n");
    assert_ne!(bare.inert, documented.inert);
    assert_ne!(bare.strict, documented.strict);
    assert_ne!(bare.interface, documented.interface);
    // On one line with its item, where it moves nothing: still an attribute more.
    assert_ne!(code("pub fn f() {}").inert, code("/** f */pub fn f() {}").inert);
    assert_ne!(
        code("//! one\npub fn f() {}\n").inert,
        code("// one\npub fn f() {}\n").inert,
        "an inner doc comment and a plain one"
    );
}

/// A macro is given tokens: a `//` comment among them, and the spaces before a line's end,
/// are none, so they are in no text. What the macro's tokens can say of where they are
/// (`line!()`, `column!()`, a span `stringify!`'s caller reads) moves with a line or a
/// column, which stay.
#[test]
fn a_comment_among_a_macros_tokens_is_nothing_and_a_moved_token_is_not() {
    let bare = code("fn f() {\n    m!(a,\n        b);\n}\n");
    assert_eq!(bare, code("fn f() {\n    m!(a, // the first\n        b);  \n}\n"));
    assert_eq!(
        code("macro_rules! m {\n    () => {\n        1\n    };\n}\n"),
        code("macro_rules! m { // none\n    () => { // gives\n        1\n    };\n}\n")
    );
    for (before, after, what) in [
        (
            "fn f() -> u32 {\n    m!(line!())\n}\n",
            "fn f() -> u32 {\n    // moved\n    m!(line!())\n}\n",
            "a line",
        ),
        (
            "fn f() -> u32 {\n    m!(\n        line!()\n    )\n}\n",
            "fn f() -> u32 {\n    m!(\n        // moved\n        line!()\n    )\n}\n",
            "a line, inside the macro's tokens",
        ),
        (
            "fn f() -> u32 {\n    m!(column!())\n}\n",
            "fn f() -> u32 {\n    /* c */ m!(column!())\n}\n",
            "a column",
        ),
        (
            "fn f() -> &'static str {\n    stringify!(a+b)\n}\n",
            "fn f() -> &'static str {\n    stringify!(a + b)\n}\n",
            "whether two tokens have whitespace between them",
        ),
        (
            "fn f() -> &'static str {\n    stringify!(a /* c */ b)\n}\n",
            "fn f() -> &'static str {\n    stringify!(a /* cd */ b)\n}\n",
            "the column of a token after a block comment",
        ),
    ] {
        assert_ne!(code(before).inert, code(after).inert, "{what}");
        assert_ne!(code(before).strict, code(after).strict, "{what}");
    }
}

/// What a file says of its crate beside its texts: whether a standard macro could mean
/// another in it, whether it names blaze's extensions, whether it may read a file by path.
#[test]
fn what_keeps_a_crate_from_an_interface_is_caught() {
    let plain = code("pub fn f(path: &str) -> usize {\n    println!(\"{path}\");\n    path.len()\n}\n");
    assert!(!plain.unplain && !plain.extends && !plain.reaches);
    for unplain in [
        "macro_rules! vec { () => { impl A for B {} }; }\n",
        "macro_rules! make { () => { macro_rules! vec { () => {}; } }; }\n",
        "use other::format;\n",
        "use other::*;\n",
        "#[macro_use]\nextern crate other;\n",
        "other::make!();\n",
        "thread_local! { static A: u8 = 0; }\n",
    ] {
        assert!(code(unplain).unplain, "{unplain}");
    }
    for kept in ["use std::fmt::*;\n", "use crate::a::*;\n", "macro_rules! mine { () => { 1 }; }\n"] {
        assert!(!code(kept).unplain, "{kept}");
    }
    assert!(code("#[blaze::pure]\nfn f() {}\n").extends);
    assert!(code("const ID: u64 = fn_info(&f).code_id;\n").extends);
    for reaches in [
        "#[path = \"../other/src/a.rs\"]\nmod a;\n",
        "#[cfg_attr(unix, path = \"unix.rs\")]\nmod sys;\n",
        "include!(\"gen.rs\");\n",
        "const A: &str = include_str!(\"a.txt\");\n",
        "make! { #[path = \"a.rs\"] mod a; }\n",
    ] {
        assert!(code(reaches).reaches, "{reaches}");
    }
}

/// What kind of item a generated file holds: what of it another crate's check reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// A function with no parameter but lifetimes: its body is its own crate's alone.
    Plain,
    /// The same, as a method of an `impl` with no parameter.
    Method,
    /// A body instantiated where it is used.
    Generic,
    /// A body evaluated where it is used.
    Const,
    /// A body inlined where it is used.
    Inline,
    /// A body whose type its signature hides.
    Hidden,
    /// A default method, instantiated for each `Self`.
    Default,
    /// A body that defines an `impl`, the crate's wherever it is written.
    Holds,
    /// A body that calls a macro whose expansion is not known.
    Calls,
    /// A constant's value.
    Value,
    /// An exported macro's tokens.
    Macro,
}

const KINDS: [Kind; 11] = [
    Kind::Plain,
    Kind::Method,
    Kind::Generic,
    Kind::Const,
    Kind::Inline,
    Kind::Hidden,
    Kind::Default,
    Kind::Holds,
    Kind::Calls,
    Kind::Value,
    Kind::Macro,
];

/// One item of a generated file: its kind, which parameter type its signature has, the digit
/// its body or value holds, whether its body has a line more, whether a `cfg` gates it and
/// whether a comment ends its first line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Item {
    kind: Kind,
    sig: u8,
    value: u8,
    long: bool,
    gated: bool,
    noted: bool,
}

/// One edit of a generated file that only its own crate's unit is asked of: what moves no
/// token and no position, and what does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Own {
    /// An edit every text is asked of ([`Edit`]).
    Of(Edit),
    /// Spaces at the end of the item's first line come.
    Spaces,
    /// A comment line and a blank line come after the file's last line.
    Ending,
    /// A comment on a line of its own comes before the item: every line from it on moves.
    Heading,
    /// A doc comment comes on the item, on the item's own first line: an attribute more, and
    /// a column moved.
    Doc,
}

const OWN: [Own; 11] = [
    Own::Of(Edit::Body),
    Own::Of(Edit::Lines),
    Own::Of(Edit::Signature),
    Own::Of(Edit::Removed),
    Own::Of(Edit::Added),
    Own::Of(Edit::Cfg),
    Own::Of(Edit::Comment),
    Own::Spaces,
    Own::Ending,
    Own::Heading,
    Own::Doc,
];

/// What the unit of a crate of the one file `text` is keyed by of it
/// ([`super::own_narrow`]'s choice for it: its code text, with the prelude's macros taken as
/// macros where the file could shadow one).
fn own(text: &str) -> ContentId {
    let code = code(text);
    match code.shadows {
        true => code.strict,
        false => code.inert,
    }
}

/// `edit` of item `at` of `items`: the file before and after, and whether anything the
/// crate's own unit makes depends on what moved: its code and its check on every token, and
/// its locations, debug info and diagnostics on every line and column.
fn own_edited(items: &[Item], at: usize, edit: Own) -> (String, String, bool) {
    let before = file(items);
    match edit {
        Own::Of(edit) => {
            let (after, _) = edited(items, at, edit);
            (before, file(&after), edit != Edit::Comment)
        }
        Own::Spaces => {
            let after = with_item(items, at, |text| text.replacen('\n', "  \t\n", 1));
            (before, after, false)
        }
        Own::Ending => {
            let after = format!("{before}// the end\n\n");
            (before, after, false)
        }
        Own::Heading => (before, with_item(items, at, |text| format!("// item\n{text}")), true),
        Own::Doc => (before, with_item(items, at, |text| format!("/** item */{text}")), true),
    }
}

/// The file of `items` with the text of item `at` as `change` makes it.
fn with_item(items: &[Item], at: usize, change: impl Fn(String) -> String) -> String {
    items
        .iter()
        .enumerate()
        .map(|(index, it)| if index == at { change(it.text(index)) } else { it.text(index) })
        .collect()
}

/// The oracle of a unit's own key: the code text of `items` and of `items` after `edit` of
/// item `at` differ exactly when something the unit makes depends on what the edit moved.
fn check_own(items: &[Item], at: usize, edit: Own) -> Result<(), String> {
    let (before, after, observed) = own_edited(items, at, edit);
    let moved = own(&before) != own(&after);
    match (observed, moved) {
        (true, false) => Err(format!(
            "{:?} {edit:?}: what the unit makes depends on it, and its key is as it was",
            items[at].kind
        )),
        (false, true) => Err(format!(
            "{:?} {edit:?}: it moves no token and no position, and the unit's key moved",
            items[at].kind
        )),
        _ => Ok(()),
    }
}

impl Item {
    fn text(&self, at: usize) -> String {
        let Item { kind, sig, value, long, gated, noted } = *self;
        let ty = ["u8", "u16", "u32"][usize::from(sig) % 3];
        let value = value % 10;
        let note = if noted { " // noted" } else { "" };
        let more = if long { "    let _more = 0;\n" } else { "" };
        let gate = if gated { "#[cfg(feature = \"gate\")]\n" } else { "" };
        let item = match kind {
            Kind::Plain => {
                format!("pub fn f{at}(x: {ty}) -> u32 {{{note}\n{more}    {value}\n}}\n")
            }
            Kind::Method => format!(
                "impl S{at} {{{note}\n    pub fn get(&self, x: {ty}) -> u32 {{\n    {more}        {value}\n    }}\n}}\n"
            ),
            Kind::Generic => {
                format!("pub fn f{at}<T>(t: T, x: {ty}) -> u32 {{{note}\n{more}    {value}\n}}\n")
            }
            Kind::Const => {
                format!("pub const fn f{at}(x: {ty}) -> u32 {{{note}\n{more}    {value}\n}}\n")
            }
            Kind::Inline => format!(
                "#[inline]\npub fn f{at}(x: {ty}) -> u32 {{{note}\n{more}    {value}\n}}\n"
            ),
            Kind::Hidden => format!(
                "pub fn f{at}(x: {ty}) -> impl Copy {{{note}\n{more}    {value}u32\n}}\n"
            ),
            Kind::Default => format!(
                "pub trait T{at} {{{note}\n    fn get(&self, x: {ty}) -> u32 {{\n    {more}        {value}\n    }}\n}}\n"
            ),
            Kind::Holds => format!(
                "pub fn f{at}(x: {ty}) -> u32 {{{note}\n{more}    impl Marker{value} for S{at} {{}}\n    0\n}}\n"
            ),
            Kind::Calls => format!(
                "pub fn f{at}(x: {ty}) -> u32 {{{note}\n{more}    make!({value});\n    0\n}}\n"
            ),
            Kind::Value => format!("pub const C{at}: {ty} = {value};{note}\n"),
            Kind::Macro => format!(
                "#[macro_export]\nmacro_rules! m{at} {{{note}\n    () => {{\n        {value}{ty}\n    }};\n}}\n"
            ),
        };
        format!("{gate}{item}")
    }

    /// Whether another crate's check can read the digit this item's body or value holds.
    fn read_from_outside(&self) -> bool {
        !matches!(self.kind, Kind::Plain | Kind::Method)
    }

    fn has_body(&self) -> bool {
        !matches!(self.kind, Kind::Value | Kind::Macro)
    }
}

/// One edit of a generated file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Edit {
    /// The digit its body or value holds becomes another.
    Body,
    /// Its body gets or loses a line.
    Lines,
    /// Its signature's parameter type becomes another.
    Signature,
    /// The item goes.
    Removed,
    /// An item comes, after it.
    Added,
    /// A `cfg` on it comes or goes.
    Cfg,
    /// A comment at the end of its first line comes or goes.
    Comment,
}

const EDITS: [Edit; 7] = [
    Edit::Body,
    Edit::Lines,
    Edit::Signature,
    Edit::Removed,
    Edit::Added,
    Edit::Cfg,
    Edit::Comment,
];

fn file(items: &[Item]) -> String {
    items.iter().enumerate().map(|(at, it)| it.text(at)).collect()
}

/// What a crate of the one file `text` shows a crate that only checks against it
/// ([`super::own_narrow`]'s choice for it: its interface text, or its code text where no body
/// may be left out).
fn shown(text: &str) -> ContentId {
    let code = code(text);
    match code.unplain {
        true => code.inert,
        false => code.interface,
    }
}

/// `edit` of item `at` of `items`: the file after, and whether another crate's check can
/// observe the edit (what the oracle holds the interface text to). Positions count: an edit
/// that moves a line moves what a diagnostic or a location in the dependent says.
fn edited(items: &[Item], at: usize, edit: Edit) -> (Vec<Item>, bool) {
    let mut after = items.to_vec();
    let item = &mut after[at];
    let observed = match edit {
        Edit::Body => {
            item.value = (item.value % 10 + 1) % 10;
            item.read_from_outside()
        }
        Edit::Lines if item.has_body() => {
            item.long = !item.long;
            true
        }
        // A constant and a macro have no body to lengthen: their value moves instead.
        Edit::Lines => {
            item.value = (item.value % 10 + 1) % 10;
            true
        }
        Edit::Signature => {
            item.sig = (item.sig % 3 + 1) % 3;
            true
        }
        Edit::Removed => {
            after.remove(at);
            true
        }
        Edit::Added => {
            let added = Item { value: (item.value % 10 + 1) % 10, ..*item };
            after.insert(at + 1, added);
            true
        }
        Edit::Cfg => {
            item.gated = !item.gated;
            true
        }
        Edit::Comment => {
            item.noted = !item.noted;
            false
        }
    };
    (after, observed)
}

/// The oracle: the interface text of `items` and of `items` after `edit` of item `at` differ
/// exactly when another crate's check can observe the edit.
fn check(items: &[Item], at: usize, edit: Edit) -> Result<(), String> {
    let (after, observed) = edited(items, at, edit);
    let (before, after) = (file(items), file(&after));
    let moved = shown(&before) != shown(&after);
    match (observed, moved) {
        (true, false) => Err(format!(
            "{:?} {edit:?}: a dependent can observe it, and the interface text is as it was",
            items[at].kind
        )),
        (false, true) => Err(format!(
            "{:?} {edit:?}: no dependent can observe it, and the interface text moved",
            items[at].kind
        )),
        _ => Ok(()),
    }
}

/// The invariant: any edit of a file that changes what a crate checking against it can
/// observe (a signature, an item added or removed, a constant's value, a macro's tokens, a
/// generic, `const`, `#[inline]`, `impl Trait` or default-method body, a body that defines an
/// item or calls an unknown macro, a `cfg`, a line moved) changes its interface text, and an
/// edit of a comment or of a plain function's body that moves no line leaves it as it was.
/// Drawn: up to five items of any kind, and one edit of one of them.
#[hegel::test]
fn the_interface_text_moves_exactly_with_what_a_dependent_observes(tc: TestCase) {
    let drawn: Vec<(u8, u8, u8, bool)> = tc.draw(
        gs::vecs(gs::tuples!(
            gs::integers().max_value(10),
            gs::integers().max_value(2),
            gs::integers().max_value(9),
            gs::booleans()
        ))
        .min_size(1)
        .max_size(5),
    );
    let items: Vec<Item> = drawn
        .iter()
        .map(|&(kind, sig, value, long)| Item {
            kind: KINDS[usize::from(kind)],
            sig,
            value,
            long,
            gated: false,
            noted: false,
        })
        .collect();
    let at: usize = tc.draw(gs::integers().max_value(items.len() - 1));
    let edit: usize = tc.draw(gs::integers().max_value(EDITS.len() - 1));
    if let Err(found) = check(&items, at, EDITS[edit]) {
        panic!("{found}\n{}", file(&items));
    }
}

/// The invariant: a unit's key moves exactly when something its results depend on moves. Of
/// its own crate's file it holds the code text, so any edit that changes a token (a body, a
/// signature, an item or a `cfg` that comes or goes, a doc comment that comes) or the line or
/// column of one (a line more in a body, a comment on a line of its own) changes it, and an
/// edit that moves neither (a `//` comment at a line's end, spaces before a line's end, a
/// comment or blank lines after the last line) leaves it as it was.
/// Drawn: up to five items of any kind, some with a comment at a line's end already, and one
/// edit of one of them.
#[hegel::test]
fn a_units_own_key_moves_exactly_with_what_its_results_depend_on(tc: TestCase) {
    // The last of each: whether its body has a line more (1) and a comment ends its first
    // line (2).
    let drawn: Vec<(u8, u8, u8, u8)> = tc.draw(
        gs::vecs(gs::tuples!(
            gs::integers().max_value(10),
            gs::integers().max_value(2),
            gs::integers().max_value(9),
            gs::integers().max_value(3)
        ))
        .min_size(1)
        .max_size(5),
    );
    let items: Vec<Item> = drawn
        .iter()
        .map(|&(kind, sig, value, laid)| Item {
            kind: KINDS[usize::from(kind)],
            sig,
            value,
            long: laid & 1 != 0,
            gated: false,
            noted: laid & 2 != 0,
        })
        .collect();
    let at: usize = tc.draw(gs::integers().max_value(items.len() - 1));
    let edit: usize = tc.draw(gs::integers().max_value(OWN.len() - 1));
    if let Err(found) = check_own(&items, at, OWN[edit]) {
        panic!("{found}\n{}", file(&items));
    }
}

/// One item of `kind`, edited by `edit`, asked of its own unit's key: the directed case of a
/// control.
fn directed_own(kind: Kind, edit: Own) -> Result<(), String> {
    let item = Item { kind, sig: 0, value: 1, long: false, gated: false, noted: false };
    check_own(&[item], 0, edit)
}

/// Every edit of every kind of item, one item a file: the oracle's cases one by one.
#[test]
fn a_units_own_key_holds_every_edit_of_every_kind_of_item() {
    for kind in KINDS {
        for edit in OWN {
            assert_eq!(directed_own(kind, edit), Ok(()), "{kind:?} {edit:?}");
        }
    }
}

/// With a key that holds its file's bytes, an edit that moves no token and no position makes
/// the unit again, the cutoff gone: the oracle says so for each such edit.
#[test]
fn the_oracle_catches_a_key_that_holds_its_files_bytes() {
    for edit in [Own::Of(Edit::Comment), Own::Spaces, Own::Ending] {
        assert_eq!(directed_own(Kind::Plain, edit), Ok(()));
        assert_eq!(
            planting(Plant::Bytes, || directed_own(Kind::Plain, edit)),
            Err(format!(
                "Plain {edit:?}: it moves no token and no position, and the unit's key moved"
            ))
        );
    }
}

/// With a key that leaves a token out (a number's digits), an edit of it leaves the unit as
/// it was, stale: the oracle says so.
#[test]
fn the_oracle_catches_a_key_that_drops_a_token() {
    let edit = Own::Of(Edit::Body);
    assert_eq!(directed_own(Kind::Plain, edit), Ok(()));
    assert_eq!(
        planting(Plant::Stale, || directed_own(Kind::Plain, edit)),
        Err("Plain Of(Body): what the unit makes depends on it, and its key is as it was"
            .to_owned())
    );
}

/// One item of `kind`, edited by `edit`: the directed case of a control.
fn directed(kind: Kind, edit: Edit) -> Result<(), String> {
    let item = Item { kind, sig: 0, value: 1, long: false, gated: false, noted: false };
    check(&[item], 0, edit)
}

/// The directed case passes as the code is, and with `plant` the oracle finds exactly it.
fn caught(plant: Plant, kind: Kind, edit: Edit) {
    assert_eq!(directed(kind, edit), Ok(()));
    assert_eq!(
        planting(plant, || directed(kind, edit)),
        Err(format!(
            "{kind:?} {edit:?}: a dependent can observe it, and the interface text is as it was"
        ))
    );
}

#[test]
fn the_oracle_catches_a_generic_body_left_out() {
    caught(Plant::Generic, Kind::Generic, Edit::Body);
}

#[test]
fn the_oracle_catches_a_const_body_left_out() {
    caught(Plant::Const, Kind::Const, Edit::Body);
}

#[test]
fn the_oracle_catches_an_inline_body_left_out() {
    caught(Plant::Inline, Kind::Inline, Edit::Body);
}

#[test]
fn the_oracle_catches_a_body_a_signature_hides_a_type_of_left_out() {
    caught(Plant::Hidden, Kind::Hidden, Edit::Body);
}

#[test]
fn the_oracle_catches_a_default_methods_body_left_out() {
    caught(Plant::Trait, Kind::Default, Edit::Body);
}

#[test]
fn the_oracle_catches_a_body_that_defines_an_item_left_out() {
    caught(Plant::Items, Kind::Holds, Edit::Body);
}

#[test]
fn the_oracle_catches_a_body_that_calls_an_unknown_macro_left_out() {
    caught(Plant::Macros, Kind::Calls, Edit::Body);
}

#[test]
fn the_oracle_catches_a_shape_with_no_line_count() {
    caught(Plant::Positions, Kind::Plain, Edit::Lines);
}

/// A body that gives an associated type its hidden type is kept, though its own signature
/// names no `impl Trait`.
#[test]
fn a_body_that_defines_an_associated_types_hidden_type_is_kept() {
    let text = |value: u8| {
        format!(
            "impl Iterator for S {{\n    type Item = impl Sized;\n    fn next(&mut self) -> Option<Self::Item> {{\n        Some({value})\n    }}\n}}\n"
        )
    };
    assert_ne!(code(&text(1)).interface, code(&text(2)).interface);
}

/// The cutoff, directed: a plain function's body edit that moves no line, and a comment, are
/// in no interface text; the same body edit is in the code text, which the crate's own code
/// and every unit that builds code read.
#[test]
fn a_plain_bodys_edit_is_in_the_code_text_and_not_the_interface() {
    for kind in [Kind::Plain, Kind::Method] {
        let item = Item { kind, sig: 0, value: 1, long: false, gated: false, noted: false };
        let (after, observed) = edited(&[item], 0, Edit::Body);
        assert!(!observed);
        let (before, after) = (code(&file(&[item])), code(&file(&after)));
        assert_eq!(before.interface, after.interface, "{kind:?}");
        assert_eq!(before.interface_strict, after.interface_strict, "{kind:?}");
        assert_ne!(before.inert, after.inert, "{kind:?}");
        assert_ne!(before.strict, after.strict, "{kind:?}");
    }
}
