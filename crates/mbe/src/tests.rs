//! Tests specific to declarative macros, aka macros by example. This covers
//! both stable `macro_rules!` macros as well as unstable `macro` macros.
// FIXME: Move more of the nameres independent tests from
// crates\hir-def\src\macro_expansion_tests\mod.rs to this
use expect_test::expect;
use span::{
    Edition, EditionedFileId, FileId, ROOT_ERASED_FILE_AST_ID, Span, SpanAnchor, SyntaxContext,
};
use stdx::format_to;
use tt::{TextRange, TextSize};

use crate::DeclarativeMacro;

fn check_(
    def_edition: Edition,
    call_edition: Edition,
    macro2: bool,
    decl: &str,
    arg: &str,
    render_debug: bool,
    expect: expect_test::Expect,
    parse: parser::TopEntryPoint,
) {
    let db = base_db::Db::for_tests();
    let decl_tt = &syntax_bridge::parse_to_token_tree(
        def_edition,
        SpanAnchor {
            file_id: EditionedFileId::new(FileId::from_raw(0), def_edition),
            ast_id: ROOT_ERASED_FILE_AST_ID,
        },
        SyntaxContext::root(Edition::CURRENT),
        decl,
    )
    .unwrap();
    let mac = if macro2 {
        DeclarativeMacro::parse_macro2(None::<&tt::TopSubtree>, decl_tt, |_| def_edition)
    } else {
        DeclarativeMacro::parse_macro_rules(decl_tt, |_| def_edition)
    };
    let call_anchor = SpanAnchor {
        file_id: EditionedFileId::new(FileId::from_raw(1), call_edition),
        ast_id: ROOT_ERASED_FILE_AST_ID,
    };
    let arg_tt = syntax_bridge::parse_to_token_tree(
        call_edition,
        call_anchor,
        SyntaxContext::root(Edition::CURRENT),
        arg,
    )
    .unwrap();
    let res = mac.expand(
        &db,
        &arg_tt,
        |_| (),
        crate::MacroCallStyle::FnLike,
        Span {
            range: TextRange::up_to(TextSize::of(arg)),
            anchor: call_anchor,
            ctx: SyntaxContext::root(Edition::CURRENT),
        },
    );
    let mut expect_res = String::new();
    if let Some(err) = res.err {
        format_to!(expect_res, "{err:#?}\n\n",);
    }
    if render_debug {
        format_to!(expect_res, "{:#?}\n\n", res.value.0);
    }
    let (node, map) =
        syntax_bridge::token_tree_to_syntax_node(res.value.0.view(), parse, &mut |_| def_edition);
    let (old_node, old_map) = syntax_bridge::reference::token_tree_to_syntax_node(
        &res.value.0,
        parse,
        &mut |_| def_edition,
    );
    assert_eq!(node.debug_dump(), old_node.debug_dump(), "the tree the old conversion parses");
    assert!(map == old_map, "the span map of the tree the old conversion parses");
    format_to!(
        expect_res,
        "{}",
        syntax_bridge::prettify_macro_expansion::prettify_macro_expansion(
            node.syntax_node(),
            &mut |_, _| None,
            |_| ()
        )
    );
    expect.assert_eq(&expect_res);
}

fn check(
    def_edition: Edition,
    call_edition: Edition,
    decl: &str,
    arg: &str,
    expect: expect_test::Expect,
) {
    check_(
        def_edition,
        call_edition,
        false,
        decl,
        arg,
        true,
        expect,
        parser::TopEntryPoint::SourceFile,
    );
}

#[test]
fn unbalanced_brace() {
    check(
        Edition::CURRENT,
        Edition::CURRENT,
        r#"
() => { { }
"#,
        r#""#,
        expect![[r#"
            SUBTREE $$ 1:Root[0000, 0]@0..0#ROOT2024 1:Root[0000, 0]@0..0#ROOT2024
              SUBTREE {} 0:Root[0000, 0]@9..10#ROOT2024 0:Root[0000, 0]@11..12#ROOT2024

            {}"#]],
    );
}

#[test]
fn token_mapping_smoke_test() {
    check(
        Edition::CURRENT,
        Edition::CURRENT,
        r#"
( struct $ident:ident ) => {
    struct $ident {
        map: ::std::collections::HashSet<()>,
    }
};
"#,
        r#"
struct MyTraitMap2
"#,
        expect![[r#"
            SUBTREE $$ 1:Root[0000, 0]@0..20#ROOT2024 1:Root[0000, 0]@0..20#ROOT2024
              IDENT   struct 0:Root[0000, 0]@34..40#ROOT2024
              IDENT   MyTraitMap2 1:Root[0000, 0]@8..19#ROOT2024
              SUBTREE {} 0:Root[0000, 0]@48..49#ROOT2024 0:Root[0000, 0]@100..101#ROOT2024
                IDENT   map 0:Root[0000, 0]@58..61#ROOT2024
                PUNCH   : [alone] 0:Root[0000, 0]@61..62#ROOT2024
                PUNCH   : [joint] 0:Root[0000, 0]@63..64#ROOT2024
                PUNCH   : [alone] 0:Root[0000, 0]@64..65#ROOT2024
                IDENT   std 0:Root[0000, 0]@65..68#ROOT2024
                PUNCH   : [joint] 0:Root[0000, 0]@68..69#ROOT2024
                PUNCH   : [alone] 0:Root[0000, 0]@69..70#ROOT2024
                IDENT   collections 0:Root[0000, 0]@70..81#ROOT2024
                PUNCH   : [joint] 0:Root[0000, 0]@81..82#ROOT2024
                PUNCH   : [alone] 0:Root[0000, 0]@82..83#ROOT2024
                IDENT   HashSet 0:Root[0000, 0]@83..90#ROOT2024
                PUNCH   < [alone] 0:Root[0000, 0]@90..91#ROOT2024
                SUBTREE () 0:Root[0000, 0]@91..92#ROOT2024 0:Root[0000, 0]@92..93#ROOT2024
                PUNCH   > [joint] 0:Root[0000, 0]@93..94#ROOT2024
                PUNCH   , [alone] 0:Root[0000, 0]@94..95#ROOT2024

            struct MyTraitMap2 {
                map: ::std::collections::HashSet<()>,
            }"#]],
    );
}

#[test]
fn token_mapping_floats() {
    // Regression test for https://github.com/rust-lang/rust-analyzer/issues/12216
    // (and related issues)
    check(
        Edition::CURRENT,
        Edition::CURRENT,
        r#"
($($tt:tt)*) => {
    $($tt)*
};
"#,
        r#"
fn main() {
    1;
    1.0;
    ((1,),).0.0;
    let x = 1;
}
"#,
        expect![[r#"
            SUBTREE $$ 1:Root[0000, 0]@0..63#ROOT2024 1:Root[0000, 0]@0..63#ROOT2024
              IDENT   fn 1:Root[0000, 0]@1..3#ROOT2024
              IDENT   main 1:Root[0000, 0]@4..8#ROOT2024
              SUBTREE () 1:Root[0000, 0]@8..9#ROOT2024 1:Root[0000, 0]@9..10#ROOT2024
              SUBTREE {} 1:Root[0000, 0]@11..12#ROOT2024 1:Root[0000, 0]@61..62#ROOT2024
                LITERAL Integer 1 1:Root[0000, 0]@17..18#ROOT2024
                PUNCH   ; [alone] 1:Root[0000, 0]@18..19#ROOT2024
                LITERAL Float 1.0 1:Root[0000, 0]@24..27#ROOT2024
                PUNCH   ; [alone] 1:Root[0000, 0]@27..28#ROOT2024
                SUBTREE () 1:Root[0000, 0]@33..34#ROOT2024 1:Root[0000, 0]@39..40#ROOT2024
                  SUBTREE () 1:Root[0000, 0]@34..35#ROOT2024 1:Root[0000, 0]@37..38#ROOT2024
                    LITERAL Integer 1 1:Root[0000, 0]@35..36#ROOT2024
                    PUNCH   , [alone] 1:Root[0000, 0]@36..37#ROOT2024
                  PUNCH   , [alone] 1:Root[0000, 0]@38..39#ROOT2024
                PUNCH   . [alone] 1:Root[0000, 0]@40..41#ROOT2024
                LITERAL Float 0.0 1:Root[0000, 0]@41..44#ROOT2024
                PUNCH   ; [alone] 1:Root[0000, 0]@44..45#ROOT2024
                IDENT   let 1:Root[0000, 0]@50..53#ROOT2024
                IDENT   x 1:Root[0000, 0]@54..55#ROOT2024
                PUNCH   = [alone] 1:Root[0000, 0]@56..57#ROOT2024
                LITERAL Integer 1 1:Root[0000, 0]@58..59#ROOT2024
                PUNCH   ; [alone] 1:Root[0000, 0]@59..60#ROOT2024

            fn main(){
                1;
                1.0;
                ((1,),).0.0;
                let x = 1;
            }"#]],
    );
}

#[test]
fn expr_2021() {
    check(
        Edition::Edition2024,
        Edition::Edition2024,
        r#"
($($e:expr),* $(,)?) => {
    $($e);* ;
};
"#,
        r#"
    _,
    const { 1 },
"#,
        expect![[r#"
            SUBTREE $$ 1:Root[0000, 0]@0..25#ROOT2024 1:Root[0000, 0]@0..25#ROOT2024
              SUBTREE $$ 0:Root[0000, 0]@34..35#ROOT2024 0:Root[0000, 0]@34..35#ROOT2024
                IDENT   _ 1:Root[0000, 0]@5..6#ROOT2024
              PUNCH   ; [joint] 0:Root[0000, 0]@36..37#ROOT2024
              SUBTREE $$ 0:Root[0000, 0]@34..35#ROOT2024 0:Root[0000, 0]@34..35#ROOT2024
                SUBTREE () 0:Root[0000, 0]@34..35#ROOT2024 0:Root[0000, 0]@34..35#ROOT2024
                  IDENT   const 1:Root[0000, 0]@12..17#ROOT2024
                  SUBTREE {} 1:Root[0000, 0]@18..19#ROOT2024 1:Root[0000, 0]@22..23#ROOT2024
                    LITERAL Integer 1 1:Root[0000, 0]@20..21#ROOT2024
              PUNCH   ; [alone] 0:Root[0000, 0]@39..40#ROOT2024

            _;
            (const {
                1
            });"#]],
    );
    check(
        Edition::Edition2021,
        Edition::Edition2024,
        r#"
($($e:expr),* $(,)?) => {
    $($e);* ;
};
"#,
        r#"
    _,
"#,
        expect![[r#"
            ExpandError {
                inner: (
                    1:Root[0000, 0]@5..6#ROOT2024,
                    MatchFailure(
                        MatchFailure {
                            token: Some(
                                "reserved identifier `_`",
                            ),
                            remaining: "sequence start",
                            remaining_span: None,
                            missing_comma: None,
                        },
                    ),
                ),
            }

            SUBTREE $$ 1:Root[0000, 0]@0..8#ROOT2024 1:Root[0000, 0]@0..8#ROOT2024
              PUNCH   ; [alone] 0:Root[0000, 0]@39..40#ROOT2024

            ;"#]],
    );
    check(
        Edition::Edition2021,
        Edition::Edition2024,
        r#"
($($e:expr),* $(,)?) => {
    $($e);* ;
};
"#,
        r#"
    const { 1 },
"#,
        expect![[r#"
            ExpandError {
                inner: (
                    1:Root[0000, 0]@5..10#ROOT2024,
                    MatchFailure(
                        MatchFailure {
                            token: Some(
                                "keyword `const`",
                            ),
                            remaining: "sequence start",
                            remaining_span: None,
                            missing_comma: None,
                        },
                    ),
                ),
            }

            SUBTREE $$ 1:Root[0000, 0]@0..18#ROOT2024 1:Root[0000, 0]@0..18#ROOT2024
              PUNCH   ; [alone] 0:Root[0000, 0]@39..40#ROOT2024

            ;"#]],
    );
    check(
        Edition::Edition2024,
        Edition::Edition2024,
        r#"
($($e:expr_2021),* $(,)?) => {
    $($e);* ;
};
"#,
        r#"
    4,
    "literal",
    funcall(),
    future.await,
    break 'foo bar,
"#,
        expect![[r#"
            SUBTREE $$ 1:Root[0000, 0]@0..76#ROOT2024 1:Root[0000, 0]@0..76#ROOT2024
              SUBTREE $$ 0:Root[0000, 0]@39..40#ROOT2024 0:Root[0000, 0]@39..40#ROOT2024
                LITERAL Integer 4 1:Root[0000, 0]@5..6#ROOT2024
              PUNCH   ; [joint] 0:Root[0000, 0]@41..42#ROOT2024
              SUBTREE $$ 0:Root[0000, 0]@39..40#ROOT2024 0:Root[0000, 0]@39..40#ROOT2024
                LITERAL Str literal 1:Root[0000, 0]@12..21#ROOT2024
              PUNCH   ; [joint] 0:Root[0000, 0]@41..42#ROOT2024
              SUBTREE $$ 0:Root[0000, 0]@39..40#ROOT2024 0:Root[0000, 0]@39..40#ROOT2024
                SUBTREE () 0:Root[0000, 0]@39..40#ROOT2024 0:Root[0000, 0]@39..40#ROOT2024
                  IDENT   funcall 1:Root[0000, 0]@27..34#ROOT2024
                  SUBTREE () 1:Root[0000, 0]@34..35#ROOT2024 1:Root[0000, 0]@35..36#ROOT2024
              PUNCH   ; [joint] 0:Root[0000, 0]@41..42#ROOT2024
              SUBTREE $$ 0:Root[0000, 0]@39..40#ROOT2024 0:Root[0000, 0]@39..40#ROOT2024
                SUBTREE () 0:Root[0000, 0]@39..40#ROOT2024 0:Root[0000, 0]@39..40#ROOT2024
                  IDENT   future 1:Root[0000, 0]@42..48#ROOT2024
                  PUNCH   . [alone] 1:Root[0000, 0]@48..49#ROOT2024
                  IDENT   await 1:Root[0000, 0]@49..54#ROOT2024
              PUNCH   ; [joint] 0:Root[0000, 0]@41..42#ROOT2024
              SUBTREE $$ 0:Root[0000, 0]@39..40#ROOT2024 0:Root[0000, 0]@39..40#ROOT2024
                SUBTREE () 0:Root[0000, 0]@39..40#ROOT2024 0:Root[0000, 0]@39..40#ROOT2024
                  IDENT   break 1:Root[0000, 0]@60..65#ROOT2024
                  PUNCH   ' [joint] 1:Root[0000, 0]@66..67#ROOT2024
                  IDENT   foo 1:Root[0000, 0]@67..70#ROOT2024
                  IDENT   bar 1:Root[0000, 0]@71..74#ROOT2024
              PUNCH   ; [alone] 0:Root[0000, 0]@44..45#ROOT2024

            4;
            "literal";
            (funcall());
            (future.await);
            (break 'foo bar);"#]],
    );
    check(
        Edition::Edition2024,
        Edition::Edition2024,
        r#"
($($e:expr_2021),* $(,)?) => {
    $($e);* ;
};
"#,
        r#"
    _,
"#,
        expect![[r#"
            ExpandError {
                inner: (
                    1:Root[0000, 0]@5..6#ROOT2024,
                    MatchFailure(
                        MatchFailure {
                            token: Some(
                                "reserved identifier `_`",
                            ),
                            remaining: "sequence start",
                            remaining_span: None,
                            missing_comma: None,
                        },
                    ),
                ),
            }

            SUBTREE $$ 1:Root[0000, 0]@0..8#ROOT2024 1:Root[0000, 0]@0..8#ROOT2024
              PUNCH   ; [alone] 0:Root[0000, 0]@44..45#ROOT2024

            ;"#]],
    );
}

#[test]
fn minus_belongs_to_literal() {
    let decl = r#"
(-1) => {-1};
(- 2) => {- 2};
(- 3.0) => {- 3.0};
(@$lit:literal) => {$lit}
"#;
    let check = |args, expect| check(Edition::CURRENT, Edition::CURRENT, decl, args, expect);
    check(
        "-1",
        expect![[r#"
            SUBTREE $$ 1:Root[0000, 0]@0..2#ROOT2024 1:Root[0000, 0]@0..2#ROOT2024
              PUNCH   - [alone] 0:Root[0000, 0]@10..11#ROOT2024
              LITERAL Integer 1 0:Root[0000, 0]@11..12#ROOT2024

            -1"#]],
    );
    check(
        "- 1",
        expect![[r#"
            SUBTREE $$ 1:Root[0000, 0]@0..3#ROOT2024 1:Root[0000, 0]@0..3#ROOT2024
              PUNCH   - [alone] 0:Root[0000, 0]@10..11#ROOT2024
              LITERAL Integer 1 0:Root[0000, 0]@11..12#ROOT2024

            -1"#]],
    );
    check(
        "-2",
        expect![[r#"
            SUBTREE $$ 1:Root[0000, 0]@0..2#ROOT2024 1:Root[0000, 0]@0..2#ROOT2024
              PUNCH   - [alone] 0:Root[0000, 0]@25..26#ROOT2024
              LITERAL Integer 2 0:Root[0000, 0]@27..28#ROOT2024

            -2"#]],
    );
    check(
        "- 2",
        expect![[r#"
            SUBTREE $$ 1:Root[0000, 0]@0..3#ROOT2024 1:Root[0000, 0]@0..3#ROOT2024
              PUNCH   - [alone] 0:Root[0000, 0]@25..26#ROOT2024
              LITERAL Integer 2 0:Root[0000, 0]@27..28#ROOT2024

            -2"#]],
    );
    check(
        "-3.0",
        expect![[r#"
            SUBTREE $$ 1:Root[0000, 0]@0..4#ROOT2024 1:Root[0000, 0]@0..4#ROOT2024
              PUNCH   - [alone] 0:Root[0000, 0]@43..44#ROOT2024
              LITERAL Float 3.0 0:Root[0000, 0]@45..48#ROOT2024

            -3.0"#]],
    );
    check(
        "- 3.0",
        expect![[r#"
            SUBTREE $$ 1:Root[0000, 0]@0..5#ROOT2024 1:Root[0000, 0]@0..5#ROOT2024
              PUNCH   - [alone] 0:Root[0000, 0]@43..44#ROOT2024
              LITERAL Float 3.0 0:Root[0000, 0]@45..48#ROOT2024

            -3.0"#]],
    );
    check(
        "@1",
        expect![[r#"
            SUBTREE $$ 1:Root[0000, 0]@0..2#ROOT2024 1:Root[0000, 0]@0..2#ROOT2024
              LITERAL Integer 1 1:Root[0000, 0]@1..2#ROOT2024

            1"#]],
    );
    check(
        "@-1",
        expect![[r#"
            SUBTREE $$ 1:Root[0000, 0]@0..3#ROOT2024 1:Root[0000, 0]@0..3#ROOT2024
              PUNCH   - [alone] 1:Root[0000, 0]@1..2#ROOT2024
              LITERAL Integer 1 1:Root[0000, 0]@2..3#ROOT2024

            -1"#]],
    );
    check(
        "@1.0",
        expect![[r#"
            SUBTREE $$ 1:Root[0000, 0]@0..4#ROOT2024 1:Root[0000, 0]@0..4#ROOT2024
              LITERAL Float 1.0 1:Root[0000, 0]@1..4#ROOT2024

            1.0"#]],
    );
    check(
        "@-1.0",
        expect![[r#"
            SUBTREE $$ 1:Root[0000, 0]@0..5#ROOT2024 1:Root[0000, 0]@0..5#ROOT2024
              PUNCH   - [alone] 1:Root[0000, 0]@1..2#ROOT2024
              LITERAL Float 1.0 1:Root[0000, 0]@2..5#ROOT2024

            -1.0"#]],
    );
    check(
        "@--1.0",
        expect![[r#"
            ExpandError {
                inner: (
                    1:Root[0000, 0]@1..2#ROOT2024,
                    MatchFailure(
                        MatchFailure {
                            token: Some(
                                "`-`",
                            ),
                            remaining: "meta-variable `$lit:literal`",
                            remaining_span: Some(
                                0:Root[0000, 0]@53..65#ROOT2024,
                            ),
                            missing_comma: None,
                        },
                    ),
                ),
            }

            SUBTREE $$ 1:Root[0000, 0]@0..6#ROOT2024 1:Root[0000, 0]@0..6#ROOT2024
              IDENT   missing 0:Root[0000, 0]@72..75#ROOT2024

            missing"#]],
    );
}

/// Taking a fragment allocates the same however much of its level follows it: the parser reads a
/// window of the level, not the rest of it, into the thread's input words.
#[test]
fn a_fragment_allocates_the_same_however_much_follows_it() {
    let db = base_db::Db::for_tests();
    let anchor = SpanAnchor {
        file_id: EditionedFileId::new(FileId::from_raw(0), Edition::CURRENT),
        ast_id: ROOT_ERASED_FILE_AST_ID,
    };
    let ctx = SyntaxContext::root(Edition::CURRENT);
    let used = |rest: usize| {
        let text = format!("a + b * 2, {}", "x, ".repeat(rest));
        let tt = syntax_bridge::parse_to_token_tree(Edition::CURRENT, anchor, ctx, &text).unwrap();
        let fragment = || {
            let mut iter = tt.iter();
            let delim_span = tt.top_subtree().delimiter.delim_span();
            let parsed =
                crate::expect_fragment(&db, &mut iter, parser::PrefixEntryPoint::Expr, delim_span);
            assert!(parsed.err.is_none());
            parsed.value.len()
        };
        // The first makes the tree's words and the thread's buffers; a warm one reuses them.
        fragment();
        let (len, used) = blaze_alloc_count::measure(fragment);
        assert_eq!(len, 5, "`a + b * 2` is five token trees");
        used
    };
    // Both past the first window, so that both read the same one.
    let (short, long) = (used(100), used(10_000));
    assert_eq!(short, long);
    assert_eq!(short.allocations, 0, "a warm fragment allocates nothing");
}

/// A warm expansion allocates as often for ten token trees as for a thousand: the matcher's
/// buffers, the bindings and the transcriber's nesting stack are the task's frame, so only the
/// output's columns, sized once from the bindings, reach the allocator. `$($e)*` gives an
/// output exactly as long as its input, which the builder's capacity covers. The output's bytes
/// grow with it, so the counts are compared, not the bytes.
#[test]
fn a_warm_expansion_allocates_the_same_for_any_input() {
    let db = base_db::Db::for_tests();
    let anchor = SpanAnchor {
        file_id: EditionedFileId::new(FileId::from_raw(0), Edition::CURRENT),
        ast_id: ROOT_ERASED_FILE_AST_ID,
    };
    let ctx = SyntaxContext::root(Edition::CURRENT);
    let decl = "($($e:tt)*) => { $($e)* }";
    let decl = syntax_bridge::parse_to_token_tree(Edition::CURRENT, anchor, ctx, decl).unwrap();
    let mac = DeclarativeMacro::parse_macro_rules(&decl, |_| Edition::CURRENT);
    let call_site = Span { range: TextRange::empty(TextSize::new(0)), anchor, ctx };
    let used = |trees: usize| {
        let text = "x ".repeat(trees);
        let arg = syntax_bridge::parse_to_token_tree(Edition::CURRENT, anchor, ctx, &text).unwrap();
        let expand = || {
            let expanded =
                mac.expand(&db, &arg, |_| (), crate::MacroCallStyle::FnLike, call_site);
            assert!(expanded.err.is_none());
            expanded.value.0.view().token_trees().len()
        };
        // The first commits the frame's pages and makes the thread's buffers; a warm one reuses them.
        expand();
        let (len, used) = blaze_alloc_count::measure(expand);
        assert_eq!(len, trees, "the expansion is its input");
        used
    };
    assert_eq!(used(10).allocations, used(1_000).allocations);
}

/// An expansion into the caller's frame allocates nothing global once warm, and gives the tree
/// the global expansion gives.
#[test]
fn an_expansion_into_a_frame_allocates_nothing() {
    let db = base_db::Db::for_tests();
    let anchor = SpanAnchor {
        file_id: EditionedFileId::new(FileId::from_raw(0), Edition::CURRENT),
        ast_id: ROOT_ERASED_FILE_AST_ID,
    };
    let ctx = SyntaxContext::root(Edition::CURRENT);
    let decl = "($($e:tt)*) => { $($e)* }";
    let decl = syntax_bridge::parse_to_token_tree(Edition::CURRENT, anchor, ctx, decl).unwrap();
    let mac = DeclarativeMacro::parse_macro_rules(&decl, |_| Edition::CURRENT);
    let call_site = Span { range: TextRange::empty(TextSize::new(0)), anchor, ctx };
    let arg = syntax_bridge::parse_to_token_tree(Edition::CURRENT, anchor, ctx, &"x ".repeat(100))
        .unwrap();
    let style = crate::MacroCallStyle::FnLike;
    let global = mac.expand(&db, &arg, |_| (), style, call_site).value.0;
    assert!(global.keeps_words());
    // The first task warms the thread's two frame levels, the builder's buffers and tt's walk
    // stack.
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        drop(mac.expand_in(frame, &db, &arg, |_| (), style, call_site))
    });
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        let (expanded, used) = blaze_alloc_count::measure(|| {
            mac.expand_in(frame, &db, &arg, |_| (), style, call_site)
        });
        assert!(expanded.err.is_none());
        assert_eq!(used.allocations, 0, "{used:?}");
        assert!(expanded.value.0 == global && !expanded.value.0.keeps_words());
    });
}


/// A `stmt` or a `block` is one thing to rustc, so it goes out in a group without delimiters
/// spanned by its metavariable, and the syntax tree's fragment keeps that span: rustc lints a
/// `$s:stmt`'s statement at the metavariable.
#[test]
fn a_stmt_and_a_block_go_out_in_groups_spanned_by_their_metavariables() {
    let db = base_db::Db::for_tests();
    let def = SpanAnchor {
        file_id: EditionedFileId::new(FileId::from_raw(0), Edition::CURRENT),
        ast_id: ROOT_ERASED_FILE_AST_ID,
    };
    let call =
        SpanAnchor { file_id: EditionedFileId::new(FileId::from_raw(1), Edition::CURRENT), ..def };
    let ctx = SyntaxContext::root(Edition::CURRENT);
    let text = "($s:stmt ; $b:block) => { $s ; $b }";
    let decl = syntax_bridge::parse_to_token_tree(Edition::CURRENT, def, ctx, text).unwrap();
    let mac = DeclarativeMacro::parse_macro_rules(&decl, |_| Edition::CURRENT);
    let input = "let x = 1 ; { x }";
    let arg = syntax_bridge::parse_to_token_tree(Edition::CURRENT, call, ctx, input).unwrap();
    let call_site = Span { range: TextRange::empty(TextSize::new(0)), anchor: call, ctx };
    let expanded = mac.expand(&db, &arg, |_| (), crate::MacroCallStyle::FnLike, call_site);
    assert!(expanded.err.is_none(), "{:?}", expanded.err);
    let tree = expanded.value.0;

    // The template's `$s` and `$b`: a metavariable's span is its name's.
    let name = |var: &str| {
        let at = text.rfind(var).unwrap() as u32 + 1;
        Span { range: TextRange::at(TextSize::new(at), TextSize::new(1)), anchor: def, ctx }
    };
    let groups: Vec<Span> = tree
        .iter()
        .filter_map(|it| match it {
            tt::iter::TtElement::Subtree(sub, _) => {
                (sub.delimiter.kind == tt::DelimiterKind::Invisible).then_some(sub.delimiter.open)
            }
            tt::iter::TtElement::Leaf(_) => None,
        })
        .collect();
    assert_eq!(groups, [name("$s"), name("$b")], "{tree:#?}");

    let (node, map) = syntax_bridge::token_tree_to_syntax_node(
        tree.view(),
        parser::TopEntryPoint::MacroStmts,
        &mut |_| Edition::CURRENT,
    );
    let block = node
        .root()
        .descendants()
        .find(|it| it.kind() == parser::SyntaxKind::BLOCK_EXPR)
        .expect("the block");
    assert_eq!(map.fragment_span(block.text_range()), Some(name("$b")), "{}", node.debug_dump());
}

/// `${concat(..)}` gives the spans of the identifiers it made, which no source has.
#[test]
fn concat_gives_the_spans_of_the_identifiers_it_made() {
    let db = base_db::Db::for_tests();
    let anchor = SpanAnchor {
        file_id: EditionedFileId::new(FileId::from_raw(0), Edition::CURRENT),
        ast_id: ROOT_ERASED_FILE_AST_ID,
    };
    let ctx = SyntaxContext::root(Edition::CURRENT);
    let text = "($a:ident) => { $a ${concat($a, _x)} b ${concat(c, $a)} }";
    let decl = syntax_bridge::parse_to_token_tree(Edition::CURRENT, anchor, ctx, text).unwrap();
    let mac = DeclarativeMacro::parse_macro_rules(&decl, |_| Edition::CURRENT);
    let arg = syntax_bridge::parse_to_token_tree(Edition::CURRENT, anchor, ctx, "a").unwrap();
    let call_site = Span { range: TextRange::empty(TextSize::new(0)), anchor, ctx };
    let expanded = mac.expand_making(&db, &arg, |_| (), crate::MacroCallStyle::FnLike, call_site);
    assert!(expanded.err.is_none(), "{:?}", expanded.err);
    let (tree, _, made) = expanded.value;
    let idents: Vec<(String, Span)> = tree
        .iter()
        .filter_map(|it| match it {
            tt::iter::TtElement::Leaf(tt::Leaf::Ident(ident)) => {
                Some((ident.sym.as_str().to_owned(), ident.span))
            }
            _ => None,
        })
        .collect();
    let names: Vec<&str> = idents.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["a", "a_x", "b", "ca"]);
    assert_eq!(made.as_slice(), [idents[1].1, idents[3].1]);
    assert!(!made.contains(&idents[0].1) && !made.contains(&idents[2].1), "{made:?}");
}
