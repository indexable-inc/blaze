//! The flat layout's round trips and refusals, and the flight table's single flight and cycle
//! refusal.

use std::{borrow::Cow, sync::Arc};

use blaze_build_script::output::{Directive, LinkArgTarget, Severity};
use blaze_format::Aligned;
use blaze_keyed::{
    kind::{Expansion, Key, ProcKind, ScriptRun, StartState},
    outside,
    proof::{self, Holds},
    record::RecordError,
};

use crate::{
    Digest, Subject, What,
    flat::{Corrupt, Why},
    machine::{Begin, Flights, Refusal},
    macro_call::{Ended, Level, MacroValue, MacroValueRef, StateOut},
    modules::{Export, ModuleValue, ModuleValueRef},
    reads::{Ask, Reads, SpanQuestion, SpanRef},
    script_run::{Exit, Mode, OutTree, ScriptValue, ScriptValueRef, blob_digest},
    tokens::{AnchorId, CtxId, InSpan, OutputWriter, RelativeRange, Role, Token, TokenKind, Transparency, stream_digest},
};

fn span(anchor: u64, start: u32, ctx: u32) -> InSpan {
    InSpan { anchor: AnchorId(anchor), range: RelativeRange { start, end: start + 1 }, ctx: CtxId(ctx) }
}

/// A macro call's value reads back as written, marks, tokens, diagnostics and state included.
#[test]
fn a_macro_value_round_trips() {
    let input = [
        Token { kind: TokenKind::Ident, text: "a", span: span(9, 0, 4) },
        Token { kind: TokenKind::Punct { joint: true }, text: "+", span: span(9, 2, 4) },
    ];
    let (_, numbering) = stream_digest(Role::Input, Some([span(9, 0, 4); 3]), input);
    let mut writer = OutputWriter::new(&numbering);
    writer.mark(CtxId(70), CtxId(4), Transparency::SemiOpaque).expect("ctx 4 is the input's");
    writer.token(&Token { kind: TokenKind::Ident, text: "b", span: span(9, 0, 70) }).expect("numbered");
    writer.token(&Token { kind: TokenKind::Ident, text: "c", span: span(9, 2, 4) }).expect("numbered");
    let moved = StartState::Statics(Digest::of("test", b"statics"));
    let bytes = MacroValue {
        ended: Ended::Output(writer),
        diagnostics: &[(Level::Warning, "careful"), (Level::Note, "see here")],
        printed: b"printed",
        state_out: StateOut::Moved(moved),
        state_bytes: b"\x01\x02",
    }
    .encode();
    let value = MacroValueRef::read(&bytes).expect("reads");
    let output = value.output.clone().expect("an output");
    let marks: Vec<_> = output.marks().collect();
    assert_eq!(marks.len(), 1);
    assert_eq!((marks[0].over, marks[0].transparency), (0, Transparency::SemiOpaque));
    let tokens: Vec<_> = output.tokens().map(|it| it.expect("reads")).collect();
    assert_eq!(tokens.iter().map(|it| it.text).collect::<Vec<_>>(), ["b", "c"]);
    // The mark's context is past the input's one context.
    assert_eq!((tokens[0].span.anchor, tokens[0].span.ctx, tokens[1].span.ctx), (0, 1, 0));
    assert_eq!(value.diagnostics().collect::<Vec<_>>(), [(Level::Warning, "careful"), (Level::Note, "see here")]);
    assert_eq!((value.printed, value.state_out, value.state_bytes), (&b"printed"[..], StateOut::Moved(moved), &b"\x01\x02"[..]));
}

/// A panic is a value, and an output span anchored outside the call is refused.
#[test]
fn a_panic_is_a_value_and_an_unnumbered_span_is_refused() {
    let bytes = MacroValue {
        ended: Ended::Panicked("boom"),
        diagnostics: &[],
        printed: b"",
        state_out: StateOut::Unchanged,
        state_bytes: b"",
    }
    .encode();
    assert_eq!(MacroValueRef::read(&bytes).expect("reads").output.err(), Some("boom"));
    let (_, numbering) = stream_digest(Role::Input, None, [Token { kind: TokenKind::Ident, text: "a", span: span(1, 0, 0) }]);
    let mut writer = OutputWriter::new(&numbering);
    assert!(writer.token(&Token { kind: TokenKind::Ident, text: "x", span: span(2, 0, 0) }).is_err());
}

/// A record of another kind or key, a payload of another layout, or one cut short, is refused
/// by name, never read.
#[test]
fn another_layout_or_kind_is_refused() {
    let bytes = MacroValue {
        ended: Ended::Panicked("boom"),
        diagnostics: &[],
        printed: b"",
        state_out: StateOut::Unchanged,
        state_bytes: b"",
    }
    .encode();
    let key = Key::<Expansion>::from_id(Digest::of("test", b"key"));
    let record = Aligned::copy_from(&outside::write_value(&key, &[], &bytes));
    proof::scope(|snap| {
        let holds: Holds<'_, Expansion> = proof::hit(snap, key, &record).expect("its own key").holds();
        assert_eq!(proof::payload(&holds, &record).expect("opens"), &bytes[..]);
        let other = Key::<Expansion>::from_id(Digest::of("test", b"other"));
        assert!(matches!(proof::hit(snap, other, &record), Err(RecordError::Key { .. })));
        let as_script = Key::<ScriptRun>::from_id(key.id());
        assert!(matches!(proof::hit(snap, as_script, &record), Err(RecordError::Kind { .. })));
    });
    let mut layout = bytes.clone();
    layout[0] ^= 0xff;
    assert!(matches!(MacroValueRef::read(&layout), Err(Corrupt { why: Why::Layout { .. }, .. })));
    assert!(matches!(MacroValueRef::read(&bytes[..bytes.len() - 1]), Err(Corrupt { why: Why::Short, .. })));
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(matches!(MacroValueRef::read(&trailing), Err(Corrupt { why: Why::Trailing, .. })));
}

/// A script's value reads back with every directive cargo's reader types, and its tree.
#[test]
fn a_script_value_round_trips() {
    let directives = [
        Directive::LinkSearch(Cow::Borrowed("native=/x")),
        Directive::LinkLib(Cow::Borrowed("z")),
        Directive::LinkArg(LinkArgTarget::All, Cow::Borrowed("-a")),
        Directive::LinkArg(LinkArgTarget::Cdylib, Cow::Borrowed("-b")),
        Directive::LinkArg(LinkArgTarget::Bin, Cow::Borrowed("-c")),
        Directive::LinkArg(LinkArgTarget::SingleBin("tool".to_string()), Cow::Borrowed("-d")),
        Directive::LinkArg(LinkArgTarget::Test, Cow::Borrowed("-e")),
        Directive::LinkArg(LinkArgTarget::Bench, Cow::Borrowed("-f")),
        Directive::LinkArg(LinkArgTarget::Example, Cow::Borrowed("-g")),
        Directive::Cfg(Cow::Borrowed("x")),
        Directive::CheckCfg(Cow::Borrowed("cfg(x)")),
        Directive::Env(Cow::Borrowed("A"), Cow::Borrowed("1")),
        Directive::Metadata(Cow::Borrowed("k"), Cow::Borrowed("v")),
        Directive::RerunIfChanged(Cow::Borrowed("build.rs")),
        Directive::RerunIfEnvChanged(Cow::Borrowed("CC")),
        Directive::Log(Severity::Error, Cow::Borrowed("bad")),
        Directive::Log(Severity::Warning, Cow::Borrowed("meh")),
    ];
    let mut tree = OutTree::default();
    tree.push(b"z.rs", Mode::File, blob_digest(b"z"), 1);
    tree.push(b"a/b", Mode::Link, blob_digest(b"../z.rs"), 7);
    tree.seal().expect("each path once");
    let bytes = ScriptValue { exit: Exit::Signal(9), directives: &directives, out_dir: &tree, stderr: b"err" }.encode();
    let value = ScriptValueRef::read(&bytes).expect("reads");
    assert_eq!(value.exit, Exit::Signal(9));
    assert_eq!(value.directives().collect::<Vec<_>>(), directives);
    let paths: Vec<_> = value.out_dir.entries().map(|it| it.path).collect();
    assert_eq!(paths, [&b"a/b"[..], &b"z.rs"[..]]);
    assert_eq!(value.stderr, b"err");
    tree.push(b"z.rs", Mode::File, blob_digest(b"z"), 1);
    assert!(tree.seal().is_err());
}

/// A module's record reads back with its exports.
#[test]
fn a_module_value_round_trips() {
    let helpers = ["serde"];
    let exports = [
        Export { name: "Serialize", kind: ProcKind::Derive, index: 0, helpers: &helpers },
        Export { name: "route", kind: ProcKind::Attr, index: 1, helpers: &[] },
    ];
    let bytes = ModuleValue { module: Digest::of("test", b"m"), len: 3, exports: &exports }.encode();
    let value = ModuleValueRef::read(&bytes).expect("reads");
    let serialize = value.export("Serialize").expect("exported");
    assert_eq!((serialize.kind, serialize.index), (ProcKind::Derive, 0));
    assert_eq!(serialize.helpers().collect::<Vec<_>>(), ["serde"]);
    assert_eq!(value.export("route").map(|it| it.kind), Some(ProcKind::Attr));
    assert!(value.export("missing").is_none());
}

/// A read list sorts and dedups its asks; an ask that answered two ways is a race.
#[test]
fn a_read_list_is_sorted_once_and_a_moved_read_refused() {
    let one = Digest::of("test", b"1");
    let two = Digest::of("test", b"2");
    let mut reads = Reads::default();
    reads.push(&Ask::Env(b"B"), one);
    reads.push(&Ask::File(b"x"), one);
    reads.push(&Ask::Env(b"B"), one);
    reads.push(&Ask::Span(SpanQuestion::LineColumn, SpanRef { anchor: 0, range: RelativeRange { start: 1, end: 2 } }), two);
    let list = reads.finish().expect("no ask moved");
    let asks: Vec<_> = list.iter().map(|it| it.0).collect();
    assert_eq!(
        asks,
        [
            Ask::File(b"x"),
            Ask::Env(b"B"),
            Ask::Span(SpanQuestion::LineColumn, SpanRef { anchor: 0, range: RelativeRange { start: 1, end: 2 } }),
        ]
    );
    let mut moved = Reads::default();
    moved.push(&Ask::File(b"x"), one);
    moved.push(&Ask::File(b"x"), two);
    assert!(moved.finish().is_err());
}

/// One run per key: a second asker waits, a third after the end is told the address; a wait
/// that closes a cycle is refused.
#[test]
fn a_key_flies_once_and_a_cycle_is_refused() {
    let subject = Subject { what: What::ProcMacro, name: Arc::from("m") };
    let a = Key::<Expansion>::from_id(Digest::of("test", b"a"));
    let b = Key::<Expansion>::from_id(Digest::of("test", b"b"));
    let mut flights = Flights::<Expansion, u32>::default();
    let Begin::Run(ticket_a) = flights.begin(&subject, a, 1) else { panic!("the first asker runs") };
    assert_eq!(flights.begin(&subject, a, 2), Begin::Wait);
    let Begin::Run(ticket_b) = flights.begin(&subject, b, 2) else { panic!("another key runs") };
    // Task 1, running `a`, asks `b`, whose runner (task 2) waits on `a`: a cycle.
    assert!(matches!(flights.begin(&subject, b, 1), Begin::Failed(Refusal::Cycle { .. })));
    let address = Key::<Expansion>::from_id(Digest::of("test", b"value"));
    let woken = flights.end(ticket_a, Ok(address));
    assert_eq!(woken.waiters.as_slice(), [2]);
    assert_eq!(flights.begin(&subject, a, 3), Begin::Done(address));
    let refusal = Refusal::Fuel { subject: subject.clone(), used: 7 };
    flights.end(ticket_b, Err(refusal.clone()));
    assert_eq!(flights.begin(&subject, b, 3), Begin::Failed(refusal));
}
