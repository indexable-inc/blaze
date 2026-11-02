//! blaze's formatter on snippets, each against what the pinned nightly's rustfmt writes for it
//! (`harness/fmtdiff.py` compares whole corpora the same way).

use blaze_fmt::{Config, EmitMode, Input, Session, Verbosity};

/// `src` formatted with `options` (`key=value`, as `--config` takes them) set over the defaults
/// for edition 2021, and whether it was refused.
fn format(src: &str, options: &[&str]) -> (String, bool) {
    let mut config = Config::default();
    config.override_value("edition", "2021");
    for option in options {
        let (key, value) = option.split_once('=').expect("key=value");
        config.override_value(key, value);
    }
    config.set().emit_mode(EmitMode::Stdout);
    config.set().verbose(Verbosity::Quiet);
    let mut out = Vec::new();
    let refused = {
        let mut session = Session::new(config, Some(&mut out));
        session
            .format(Input::Text(src.to_owned()))
            .expect("formatting runs");
        session.has_parsing_errors()
    };
    (String::from_utf8(out).expect("UTF-8"), refused)
}

fn formats_as(src: &str, options: &[&str], want: &str) {
    let (got, refused) = format(src, options);
    assert!(!refused, "refused:\n{src}");
    assert_eq!(got, want);
}

#[test]
fn layout() {
    formats_as(
        "fn main(){let x=1;   println!(\"{}\",x+2);}\nstruct  A{a:u32,b:Vec<String>}\n",
        &[],
        "fn main() {\n    let x = 1;\n    println!(\"{}\", x + 2);\n}\nstruct A {\n    a: u32,\n    b: Vec<String>,\n}\n",
    );
}

#[test]
fn qualified_paths_keep_their_trait() {
    formats_as(
        "fn f<T: Iterator>(t: T) -> <T as Iterator>::Item { <T>::next(&mut t).unwrap_or_else(|| <<T as Iterator>::Item as Default>::default()) }\n",
        &[],
        "fn f<T: Iterator>(t: T) -> <T as Iterator>::Item {\n    <T>::next(&mut t).unwrap_or_else(|| <<T as Iterator>::Item as Default>::default())\n}\n",
    );
}

#[test]
fn imports_are_sorted_and_self_renames_dropped() {
    formats_as(
        "use foo::bar::baz as baz;\nuse a::{c, b};\n",
        &[],
        "use a::{b, c};\nuse foo::bar::baz;\n",
    );
}

#[test]
fn type_alias_keeps_both_where_clauses() {
    formats_as(
        "type Foo<T> where T: Copy = Vec<T> where T: Clone;\n",
        &[],
        "type Foo<T>\nwhere\n    T: Copy,\n= Vec<T>\nwhere\n    T: Clone;\n",
    );
}

#[test]
fn chained_ranges_in_a_macro_are_a_pattern() {
    let src = "fn is_id(c: char) -> bool {\n    matches!(c as u32, 0xB7 | 0xc0..=0xd6 | 0xd8..=0xf6 | 0xf8..=0x37D | 0x37F..=0x1FFF | 0x200C | 0x200D)\n}\n";
    formats_as(src, &[], src);
}

#[test]
fn max_width() {
    formats_as(
        "fn main() { let result = some_function_name(first_argument, second_argument, third_argument); }\n",
        &["max_width=60"],
        "fn main() {\n    let result = some_function_name(\n        first_argument,\n        second_argument,\n        third_argument,\n    );\n}\n",
    );
}

#[test]
fn chains_in_both_indent_styles() {
    let src = "fn main() { let x = aaaaaaaaaaaaaaaaaaaa.bbbbbbbbbbbbbbbbbbbbbbbbb().ccccccccccccccccccccccccc().ddddddddddddddddddddddd(); }\n";
    formats_as(
        src,
        &[],
        "fn main() {\n    let x = aaaaaaaaaaaaaaaaaaaa\n        .bbbbbbbbbbbbbbbbbbbbbbbbb()\n        .ccccccccccccccccccccccccc()\n        .ddddddddddddddddddddddd();\n}\n",
    );
    formats_as(
        src,
        &["indent_style=Visual"],
        "fn main() {\n    let x = aaaaaaaaaaaaaaaaaaaa.bbbbbbbbbbbbbbbbbbbbbbbbb()\n                                .ccccccccccccccccccccccccc()\n                                .ddddddddddddddddddddddd();\n}\n",
    );
}

#[test]
fn a_syntax_error_is_refused() {
    let (_, refused) = format("fn main() { let = ; }\n", &[]);
    assert!(refused);
}

// rustc's parser takes a prefix range as the end of a range, and leaves a misplaced `self` to
// name resolution.
#[test]
fn what_rustc_parses_is_formatted() {
    let src = "fn dots() {\n    .. .. ..;\n    (..) .. ..;\n}\nuse self::self;\n";
    let want = "fn dots() {\n    .. .. ..;\n    (..).. ..;\n}\nuse self::self;\n";
    assert_eq!(format(src, &[]), (want.to_owned(), false));
}

#[test]
fn what_rustc_rejects_is_refused() {
    let (_, refused) = format("fn main() { let _ = a < b < c; }\n", &[]);
    assert!(refused);
}
