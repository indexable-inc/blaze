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

/// blaze's own rustfmt.toml (`.rustfmt.toml` at the repository's root).
const BLAZE_STYLE: &[&str] = &[
    "edition=2024",
    "style_edition=2024",
    "use_small_heuristics=Max",
];

/// `src` formats as `want`, and `want` formats as itself: a second run changes nothing.
fn formats_as_fixpoint(src: &str, options: &[&str], want: &str) {
    formats_as(src, options, want);
    formats_as(want, options, want);
}

// An attribute that is no meta item (`weigh = with_source_map_bytes` names no literal) is kept as
// written. Inside a macro_rules body its lines kept their source columns while the body around
// them was formatted from column 0 and indented after, so each run moved its arguments right by
// the body's indent (the fmt workflow's loop on crates/hir-def/src/signatures.rs). Upstream
// rustfmt still drifts here; blaze fmt keeps the layout as written.
#[test]
fn a_verbatim_attribute_in_a_macro_body_stays_put() {
    let src = r#"macro_rules! signature_queries {
    ($Sig:ident, $Id:ident) => {
        impl $Sig {
            /// The signature with where each part of it is written in its file.
            #[blaze_query::tracked(
                returns(ref),
                lru,
                weigh = with_source_map_bytes,
                mem_tag = Nameres
            )]
            pub fn with_source_map(db: &Db, id: $Id) -> (Arc<$Sig>, ExpressionStoreSourceMap) {
                Self::relative(db, id)
            }
        }
    };
}
"#;
    formats_as_fixpoint(src, BLAZE_STYLE, src);
}

// The same attribute on the body's first line, which the body's trim leaves at column 0, and in
// a `{{ ... }}` body, indented from the branch rather than one level in.
#[test]
fn a_verbatim_attribute_first_in_a_macro_body_stays_put() {
    let src = r#"macro_rules! m {
    ($t:ty) => {
        #[tracked(
            returns(ref),
            weigh = bytes
        )]
        fn f(x: $t) {}
    };
}
"#;
    formats_as_fixpoint(src, BLAZE_STYLE, src);
    let src = r#"macro_rules! m {
    ($t:ty) => {{
        #[tracked(
            returns(ref),
            weigh = bytes
        )]
        fn f(x: $t) {}
        f
    }};
}
"#;
    formats_as_fixpoint(src, BLAZE_STYLE, src);
}

// A meta-item attribute too long for its line is laid out block-indented one level past it, as
// rustfmt writes it, inside a macro body as outside one.
#[test]
fn a_long_meta_attribute_in_a_macro_body_is_block_indented() {
    formats_as_fixpoint(
        r#"macro_rules! m {
    ($t:ident) => {
        #[cfg_attr(feature = "serde_derive_feature", derive(serde::Serialize, serde::Deserialize, Debug, Clone))]
        struct $t;
    };
}
"#,
        BLAZE_STYLE,
        r#"macro_rules! m {
    ($t:ident) => {
        #[cfg_attr(
            feature = "serde_derive_feature",
            derive(serde::Serialize, serde::Deserialize, Debug, Clone)
        )]
        struct $t;
    };
}
"#,
    );
}

// A match arm's `return` body too long for the arm's line goes in a block with a `;`, its line
// exactly max_width wide. Formatted again, that block's statement kept the `;`'s column twice
// and broke the line (upstream rustfmt does); blaze fmt counts it once both times.
#[test]
fn a_return_arm_moved_into_a_block_stays_one_line() {
    formats_as_fixpoint(
        "fn f() -> R {\n    match a {\n        _ => return Err(usage(\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\")),\n    }\n}\n",
        BLAZE_STYLE,
        "fn f() -> R {\n    match a {\n        _ => {\n            return Err(usage(\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"));\n        }\n    }\n}\n",
    );
}
