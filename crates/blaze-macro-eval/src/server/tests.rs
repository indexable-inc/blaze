use base_db::Db;
use intern::Symbol;
use span::{Edition, EditionedFileId, Span, SpanAnchor, SyntaxContext, TextRange};

use super::Server;
use crate::{
    protocol::Method,
    stream::{IdentKind, Spacing, Stream, Tree},
    wire::{Reader, Writer},
};

fn at(start: u32, end: u32) -> Span {
    let file_id = EditionedFileId::new(span::FileId::from_raw(3), Edition::CURRENT);
    Span {
        range: TextRange::new(start.into(), end.into()),
        anchor: SpanAnchor { file_id, ast_id: span::ROOT_ERASED_FILE_AST_ID },
        ctx: SyntaxContext::root(Edition::CURRENT),
    }
}

fn server(_db: &Db) -> Server {
    Server::new([at(0, 0), at(1, 1), at(2, 2)], Edition::CURRENT, false)
}

/// A request as the client writes it: its tag, then its arguments.
fn request(method: Method, args: impl FnOnce(&mut Writer<'_>)) -> Vec<u8> {
    let mut out = vec![method as u8];
    args(&mut Writer { out: &mut out });
    out
}

fn ident_tree(w: &mut Writer<'_>, name: &str, span: u32) {
    w.u8(2);
    w.str(name);
    w.u8(0);
    w.handle(span);
}

fn punct_tree(w: &mut Writer<'_>, ch: u8, joint: bool, span: u32) {
    w.u8(1);
    w.u8(ch);
    w.bool(joint);
    w.handle(span);
}

/// The handle an `Ok` reply holds.
fn handle_of(reply: &[u8]) -> u32 {
    let mut r = Reader::new(reply);
    assert!(r.some().unwrap(), "an Ok reply");
    r.handle().unwrap()
}

#[test]
fn the_sites_are_the_first_spans_and_the_input_follows() {
    let db = Db::new();
    let mut server = server(&db);
    let mut input = Vec::new();
    server.input(&[Stream::default(), Stream::default()], &mut input);
    let mut r = Reader::new(&input);
    assert_eq!([r.handle().unwrap(), r.handle().unwrap(), r.handle().unwrap()], [1, 2, 3]);
    assert_eq!([r.handle().unwrap(), r.handle().unwrap()], [1, 2]);
    assert!(r.is_empty());
}

#[test]
fn trees_given_become_a_stream_with_rustcs_spacing_and_come_back_as_given() {
    let db = Db::new();
    let mut server = server(&db);
    let mut reply = Vec::new();
    // `a+=` as three trees, the `+` joint.
    let concat = request(Method::TsConcatTrees, |w| {
        w.some(false);
        w.usize(3);
        ident_tree(w, "a", 2);
        punct_tree(w, b'+', true, 2);
        punct_tree(w, b'=', false, 2);
    });
    server.dispatch(&db, &concat, &mut reply).unwrap();
    let handle = handle_of(&reply);
    let trees = server.stream(handle).unwrap().trees();
    assert_eq!(trees.len(), 3);
    assert!(matches!(&trees[0], Tree::Ident { kind: IdentKind::Normal, spacing: Spacing::Alone, .. }));
    assert!(matches!(&trees[1], Tree::Punct { ch: b'+', spacing: Spacing::Joint, .. }));
    assert!(matches!(&trees[2], Tree::Punct { ch: b'=', spacing: Spacing::Alone, .. }));

    let into = request(Method::TsIntoTrees, |w| w.handle(handle));
    server.dispatch(&db, &into, &mut reply).unwrap();
    let mut r = Reader::new(&reply);
    assert!(r.some().unwrap());
    assert_eq!(r.count().unwrap(), 3);
    assert_eq!(r.u8().unwrap(), 2);
    assert_eq!(r.str().unwrap(), "a");
    assert_eq!(r.u8().unwrap(), 0);
    assert_eq!(r.handle().unwrap(), 2);
    assert_eq!(r.u8().unwrap(), 1);
    assert_eq!(r.u8().unwrap(), b'+');
    assert!(r.bool().unwrap());
    // `into_trees` took the stream: it is gone.
    assert!(server.dispatch(&db, &into, &mut reply).is_err());
}

/// The spans and jointness of the punctuation and identifiers of `reply` to `into_trees`.
fn read_tokens(server: &Server, reply: &[u8]) -> Vec<(Span, bool)> {
    let mut r = Reader::new(reply);
    assert!(r.some().unwrap());
    let count = r.count().unwrap();
    let mut tokens = Vec::new();
    for _ in 0..count {
        let joint = match r.u8().unwrap() {
            1 => {
                r.u8().unwrap();
                r.bool().unwrap()
            }
            2 => {
                r.str().unwrap();
                r.u8().unwrap();
                false
            }
            other => panic!("tree tag {other} in a test of tokens"),
        };
        tokens.push((server.span(r.handle().unwrap()).unwrap(), joint));
    }
    tokens
}

#[test]
fn glued_trees_come_back_at_the_span_rustcs_glued_token_splits_into() {
    let db = Db::new();
    let mut server = server(&db);
    let mut reply = Vec::new();
    let site = server.span_handle(at(10, 12));
    let [first, second] = [at(20, 21), at(21, 22)].map(|span| server.span_handle(span));
    let [quote, name] = [at(30, 31), at(31, 32)].map(|span| server.span_handle(span));
    // `::` with both marks at a two-byte span, `::` at each its byte, `'a` at its two, and a
    // `+` whose jointness glues to nothing.
    let concat = request(Method::TsConcatTrees, |w| {
        w.some(false);
        w.usize(8);
        punct_tree(w, b':', true, site);
        punct_tree(w, b':', false, site);
        punct_tree(w, b':', true, first);
        punct_tree(w, b':', false, second);
        punct_tree(w, b'\'', true, quote);
        ident_tree(w, "a", name);
        punct_tree(w, b'+', true, site);
        ident_tree(w, "b", site);
    });
    server.dispatch(&db, &concat, &mut reply).unwrap();
    let handle = handle_of(&reply);
    let into = request(Method::TsIntoTrees, |w| w.handle(handle));
    server.dispatch(&db, &into, &mut reply).unwrap();
    assert_eq!(
        read_tokens(&server, &reply),
        [
            (at(10, 11), true),
            (at(11, 12), false),
            (at(20, 21), true),
            (at(21, 22), false),
            (at(30, 32), true),
            (at(30, 32), false),
            (at(10, 12), true),
            (at(10, 12), false),
        ]
    );
}

#[test]
fn a_negative_literal_given_is_a_minus_before_it() {
    let db = Db::new();
    let mut server = server(&db);
    let mut reply = Vec::new();
    let from = request(Method::TsFromTokenTree, |w| {
        w.u8(3);
        w.u8(2);
        w.str("-1");
        w.some(true);
        w.str("i32");
        w.handle(2);
    });
    server.dispatch(&db, &from, &mut reply).unwrap();
    let trees = server.stream(handle_of(&reply)).unwrap().trees();
    assert!(matches!(&trees[0], Tree::Punct { ch: b'-', spacing: Spacing::JointHidden, .. }));
    match &trees[1] {
        Tree::Literal { symbol, suffix, .. } => {
            assert_eq!(symbol.as_str(), "1");
            assert_eq!(suffix.as_ref().map(Symbol::as_str), Some("i32"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn streams_are_cloned_by_handle_dropped_once_and_concatenated_in_order() {
    let db = Db::new();
    let mut server = server(&db);
    let mut reply = Vec::new();
    let one = |name: &str| {
        request(Method::TsFromTokenTree, |w| ident_tree(w, name, 2))
    };
    server.dispatch(&db, &one("a"), &mut reply).unwrap();
    let a = handle_of(&reply);
    server.dispatch(&db, &one("b"), &mut reply).unwrap();
    let b = handle_of(&reply);
    server.dispatch(&db, &request(Method::TsClone, |w| w.handle(a)), &mut reply).unwrap();
    let c = handle_of(&reply);
    assert_ne!(c, a);
    let concat = request(Method::TsConcatStreams, |w| {
        w.some(true);
        w.handle(a);
        w.usize(2);
        w.handle(b);
        w.handle(c);
    });
    server.dispatch(&db, &concat, &mut reply).unwrap();
    let all = handle_of(&reply);
    let names: Vec<&str> = server
        .stream(all)
        .unwrap()
        .trees()
        .iter()
        .map(|tree| match tree {
            Tree::Ident { sym, .. } => sym.as_str(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(names, ["a", "b", "a"]);
    server.dispatch(&db, &request(Method::TsDrop, |w| w.handle(all)), &mut reply).unwrap();
    assert!(server.dispatch(&db, &request(Method::TsDrop, |w| w.handle(all)), &mut reply).is_err());
    // `a` went into the concatenation.
    assert!(server.dispatch(&db, &request(Method::TsIsEmpty, |w| w.handle(a)), &mut reply).is_err());
}

#[test]
fn spans_are_interned_and_answered_without_the_database_where_they_can_be() {
    let db = Db::new();
    let mut server = server(&db);
    let mut reply = Vec::new();
    let handle = server.span_handle(at(10, 20));
    assert_eq!(server.span_handle(at(10, 20)), handle);
    server.dispatch(&db, &request(Method::SpanEnd, |w| w.handle(handle)), &mut reply).unwrap();
    assert_eq!(server.span(handle_of(&reply)).unwrap(), at(20, 20));
    let sub = request(Method::SpanSubspan, |w| {
        w.handle(handle);
        w.u8(0);
        w.usize(2);
        w.u8(1);
        w.usize(5);
    });
    server.dispatch(&db, &sub, &mut reply).unwrap();
    let mut r = Reader::new(&reply);
    assert!(r.some().unwrap());
    assert!(r.some().unwrap());
    assert_eq!(server.span(r.handle().unwrap()).unwrap(), at(12, 15));
    server.dispatch(&db, &request(Method::SpanSaveSpan, |w| w.handle(handle)), &mut reply).unwrap();
    let mut r = Reader::new(&reply);
    assert!(r.some().unwrap());
    let id = r.usize().unwrap();
    server.dispatch(&db, &request(Method::SpanRecoverProcMacroSpan, |w| w.usize(id)), &mut reply).unwrap();
    assert_eq!(handle_of(&reply), handle);
    assert!(!server.asked);
}

#[test]
fn a_diagnostic_is_kept_with_its_children_and_its_level() {
    let db = Db::new();
    let mut server = server(&db);
    let mut reply = Vec::new();
    let emit = request(Method::EmitDiagnostic, |w| {
        w.u8(0);
        w.str("bad input");
        w.usize(1);
        w.handle(2);
        w.usize(1);
        w.u8(3);
        w.str("try this");
        w.usize(0);
        w.usize(0);
    });
    server.dispatch(&db, &emit, &mut reply).unwrap();
    assert_eq!(server.emitted.len(), 1);
    assert!(server.emitted[0].error);
    assert_eq!(server.emitted[0].text, "error: bad input\n  help: try this");
}

#[test]
fn a_request_the_bridge_never_sends_stops_the_expansion() {
    let db = Db::new();
    let mut server = server(&db);
    let mut reply = Vec::new();
    assert!(server.dispatch(&db, &[200], &mut reply).is_err());
    assert!(server.dispatch(&db, &[Method::TsIsEmpty as u8, 1], &mut reply).is_err());
    let extra = request(Method::SpanStart, |w| {
        w.handle(1);
        w.u8(0);
    });
    assert!(server.dispatch(&db, &extra, &mut reply).is_err());
    assert!(server.dispatch(&db, &request(Method::SpanStart, |w| w.handle(99)), &mut reply).is_err());
}

#[test]
fn the_output_is_the_stream_or_the_panic_message() {
    let db = Db::new();
    let mut server = server(&db);
    let handle = server.stream_handle(Stream::default());
    let mut ok = Vec::new();
    let mut w = Writer { out: &mut ok };
    w.ok(true);
    w.some(true);
    w.handle(handle);
    assert!(matches!(server.output(&ok), Ok(Ok(_))));
    let mut panicked = Vec::new();
    let mut w = Writer { out: &mut panicked };
    w.ok(false);
    w.some(true);
    w.str("no fields");
    assert_eq!(server.output(&panicked).unwrap().unwrap_err(), "no fields");
}
