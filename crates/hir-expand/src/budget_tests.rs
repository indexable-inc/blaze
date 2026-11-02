//! Allocation budgets of hir-expand's hot reads: a `Name` from a symbol that exists, a
//! `ModPath` whose segment list exists, and span lookups, in order and scattered, each
//! allocating nothing; and a call's argument converted to tokens, a constant number. Each is
//! measured warm (its symbols and lists interned once before).

use intern::Symbol;
use span::{Edition, EditionedFileId, ErasedFileAstId, FileId, Span, SpanAnchor, SyntaxContext};
use syntax::{
    SourceFile, TextRange, TextSize,
    view::{self, AstNode as _},
};
use syntax_bridge::DocCommentDesugarMode;

use crate::{
    mod_path::{ModPath, PathKind},
    name::{AsName, Name},
    span_map::{ExpansionSpanMap, InOrder, RealSpanMap, SpanMap},
    tt,
};

fn span(range: TextRange) -> Span {
    Span {
        range,
        anchor: SpanAnchor {
            file_id: EditionedFileId::new(FileId::from_raw(7), Edition::CURRENT),
            ast_id: ErasedFileAstId::from_raw(0),
        },
        ctx: SyntaxContext::root(Edition::CURRENT),
    }
}

#[test]
fn a_name_of_an_existing_symbol_allocates_nothing() {
    let symbol = Symbol::intern("budget_name");
    let ident = tt::Ident {
        sym: symbol,
        span: span(TextRange::empty(TextSize::new(0))),
        is_raw: tt::IdentIsRaw::No,
    };
    let warm = (Name::new_root("budget_name"), Name::new_tuple_field(3), ident.as_name());
    let names = blaze_alloc_count::assert_at_most(0, "names of existing symbols", || {
        (
            Name::new_symbol_root(symbol),
            Name::new_root("budget_name"),
            Name::new_tuple_field(3),
            ident.as_name(),
        )
    });
    assert_eq!(names.0, warm.0);
    assert_eq!(names.1, warm.0);
    assert_eq!(names.2, warm.1);
    assert_eq!(names.3, warm.2);
}

#[test]
fn a_path_whose_segments_exist_allocates_nothing() {
    let [a, b, c] = ["budget_a", "budget_b", "budget_c"].map(Name::new_root);
    let warm = ModPath::from_segments(PathKind::Plain, [a, b, c]);
    let mut shorter = warm;
    shorter.pop_segment();
    let paths = blaze_alloc_count::assert_at_most(0, "paths whose segment lists exist", || {
        let mut path = ModPath::from_segments(PathKind::Plain, [a, b, c]);
        let popped = path.pop_segment();
        let mut again = path;
        again.push_segment(c);
        (path, popped, again, ModPath::from_kind(PathKind::Crate))
    });
    assert_eq!(paths.0, shorter);
    assert_eq!(paths.1, Some(c));
    assert_eq!(paths.2, warm);
    assert_eq!(paths.3.segments(), &[] as &[Name]);
}

#[test]
fn span_lookups_allocate_nothing() {
    let file_id = EditionedFileId::new(FileId::from_raw(7), Edition::CURRENT);
    let anchors: Vec<_> = (0..32u32)
        .map(|i| (TextSize::new(i * 40), ErasedFileAstId::from_raw(i)))
        .collect();
    let real = RealSpanMap::from_file(file_id, &anchors, TextSize::new(32 * 40));
    let mut expansion = ExpansionSpanMap::empty();
    let ranges: Vec<TextRange> =
        (0..320u32).map(|i| TextRange::new((i * 4).into(), (i * 4 + 3).into())).collect();
    for &range in &ranges {
        expansion.push(range.end(), span(range));
    }
    expansion.finish();
    for map in [SpanMap::RealSpanMap(&real), SpanMap::ExpansionSpanMap(&expansion)] {
        let expected: Vec<Span> = ranges.iter().map(|&range| map.span_for_range(range)).collect();
        let found = blaze_alloc_count::assert_at_most(0, "span lookups", || {
            let in_order = InOrder::new(map);
            let mut same = true;
            for (at, &range) in ranges.iter().enumerate() {
                same &= in_order.span_for_range(range) == expected[at];
                same &= map.span_for_range(range) == expected[at];
            }
            same
        });
        assert!(found, "the in-order lookup gives the plain lookup's spans");
    }
}

/// `macro_arg`'s conversion of a call's argument from syntax to tokens: a constant number of
/// allocations however long the argument is, since the builder is sized by the subtree's length.
#[test]
fn an_argument_costs_a_constant_number_of_allocations() {
    let used = |tokens: usize| {
        let text = format!("m!({});", "a + 1, ".repeat(tokens / 4));
        let parse = SourceFile::parse(&text, Edition::CURRENT);
        let arg = parse.root().descendants().find_map(view::TokenTree::cast).expect("the argument");
        let map = RealSpanMap::absolute(EditionedFileId::new(FileId::from_raw(7), Edition::CURRENT));
        let at = span(TextRange::empty(TextSize::new(0)));
        let run = || {
            syntax_bridge::call_input(
                arg.syntax(),
                InOrder::new(SpanMap::RealSpanMap(&map)),
                at,
                DocCommentDesugarMode::Mbe,
            )
        };
        let warm = run();
        let (again, used) = blaze_alloc_count::measure(run);
        assert!(again == warm, "{tokens} tokens");
        used.allocations
    };
    let (small, big) = (used(16), used(1024));
    assert!(small <= 12, "an argument of 16 tokens: {small} allocations, budget 12");
    assert_eq!(big, small, "1024 tokens against 16: no allocation grows with the argument");
}

/// Mapping up out of an expansion's span map and back down, warm: the first downmap builds the
/// map's index by anchor and grows the worker's `u32`s once; after that nothing allocates.
#[test]
fn span_mapping_is_free_once_warm() {
    let mut map = ExpansionSpanMap::empty();
    let ranges: Vec<TextRange> =
        (0..256u32).map(|i| TextRange::new((i * 4).into(), (i * 4 + 3).into())).collect();
    for &range in &ranges {
        map.push(range.end(), span(range));
    }
    map.finish();
    let probe = |map: &ExpansionSpanMap| {
        let mut found = 0usize;
        for &range in ranges.iter().step_by(17) {
            found += crate::map_node_range_up_rooted_span(map, range).is_some() as usize;
            found += map.ranges_with_span(span(range)).count();
            found += map.ranges_with_span_exact(span(range)).count();
        }
        found
    };
    let warm = probe(&map);
    let again = blaze_alloc_count::assert_at_most(0, "warm span mapping", || probe(&map));
    assert_eq!(again, warm);
}

/// An eager input's span map taken from the worker and given back: the second take reuses the
/// first's columns.
#[test]
fn an_eager_span_map_is_reused() {
    let fill = |map: &mut ExpansionSpanMap| {
        for i in 1..=64u32 {
            map.push((i * 4).into(), span(TextRange::new((i * 4 - 4).into(), (i * 4).into())));
        }
    };
    {
        let mut first = crate::eager::ArgMap::take();
        fill(&mut first);
    }
    blaze_alloc_count::assert_at_most(0, "a reused eager span map", || {
        let mut again = crate::eager::ArgMap::take();
        fill(&mut again);
        again.iter().count()
    });
}

/// An expression expansion's tokens before its last `;`, in the run's frame: no global
/// allocation, and the tokens are the input's.
#[test]
fn the_tokens_before_a_semicolon_are_framed() {
    let text = "a + b ;";
    let tt = syntax_bridge::parse_to_token_tree_static_span(
        Edition::CURRENT,
        span(TextRange::empty(TextSize::new(0))),
        text,
    )
    .expect("tokens");
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        let (before, semicolon) =
            blaze_alloc_count::assert_at_most(0, "the tokens before a `;`", || {
                crate::without_last_semicolon(frame, tt.view())
            })
            .expect("ends in `;`");
        assert_eq!(before.token_trees().len(), 3);
        assert_eq!(semicolon, span(TextRange::empty(TextSize::new(0))));
    });
}

/// `concat!` of literals, warm: the text is the worker's string and the expansion the frame's;
/// the literal's symbol exists after the first run.
#[test]
fn concat_is_framed() {
    let call = span(TextRange::empty(TextSize::new(0)));
    let tt = syntax_bridge::parse_to_token_tree_static_span(
        Edition::CURRENT,
        call,
        r#""a", 1, 'b', true"#,
    )
    .expect("tokens");
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        let warm = crate::builtin::concat_of_literals_in(frame, &tt, call);
        assert!(warm.err.is_none());
        let again = blaze_alloc_count::assert_at_most(0, "a warm `concat!`", || {
            crate::builtin::concat_of_literals_in(frame, &tt, call)
        });
        assert!(again.err.is_none());
        assert!(*again.value == *warm.value);
        assert_eq!(again.value.to_string(), r#""a1btrue""#);
    });
}
