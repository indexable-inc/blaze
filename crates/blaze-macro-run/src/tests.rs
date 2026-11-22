//! The session and the host wired through real frames, with simulated clients in place of the
//! pinned `proc_macro` client: each test writes the bridge requests a macro's client would, or,
//! for a hostile process, the frames and outcome bytes no session writes.

use blaze_comptime::wire::split_frame;
use blaze_comptime::{
    Answer, Call as SysCall, CallKey, CallKind, Crash, Deadline, Effect, Ended, EnvGrant, EnvValue, Event, FactKind, Found, Fuel, Grant,
    Grants, Guest, GuestEffect, GuestEvent, GuestOpts, Nanos, PROTOCOL, ProcMacroOpts, WireLimits,
};
use blaze_expand::{Delim, Edition, LitKind, ProcKind, ProcLevel, Raw, ReadKind, Spacing, TokenKind, TokenOrigin, TokenStream, Transparency, lex};
use blaze_id::ContentId;
use blaze_own::{AVec, Alloc, Vec};
use blaze_roots::{Glob, Mount, Mounts, Root, RootId, RootKind, Roots};

use crate::bridge::{Field, Malformed, Writer};
use crate::call::{Call, MacroDecl, MacroId, MacroProgram};
use crate::host::{MacroHost, MacroHostOpts, Outcome};
use crate::method::Method;
use crate::outcome::{OutcomeAt, OutcomeError, OutcomeWriter};
use crate::refused::{FileWhy, Refused, Why};
use crate::server::ServerError;
use crate::session::{Input, Output, Session, SessionOpts};

const DEADLINE: u64 = 1_000_000_000;
const FILE: &str = "src/lib.rs";

const DECLS: [MacroDecl<'static>; 2] = [
    MacroDecl { kind: ProcKind::Derive, name: "Demo", helpers: &["demo"] },
    MacroDecl { kind: ProcKind::FnLike, name: "make", helpers: &[] },
];

const PROGRAM: MacroProgram<'static> = MacroProgram { krate: "demo_macros", code: ContentId([7; 32]), decls: "_DECLS", macros: &DECLS };

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn bytes(b: &[u8]) -> AVec<'static, u8> {
    Vec::from_slice_in(b, alloc())
}

/// The input of a derive's call: `struct S ;`, lexed in 2021.
fn item(text: &str) -> TokenStream<'static> {
    lex(text, Edition::E2021, alloc()).unwrap()
}

fn derive<'c>(input: &'c TokenStream<'static>) -> Call<'c, 'static> {
    Call { which: MacroId(0), kind: ProcKind::Derive, edition: Edition::E2021, input, attr: None }
}

fn encoded(call: &Call<'_, '_>) -> AVec<'static, u8> {
    let mut out = Vec::new_in(alloc());
    call.encode(&mut out).unwrap();
    out
}

/// The run's grants: one package root `pkg` (`src/**`) at `/pkg`, `CARGO_PKG_NAME` granted.
fn with_grants<R>(input: &[u8], f: impl FnOnce(Grants<'_>) -> R) -> R {
    let globs = [Glob::new("src/**").unwrap()];
    let root_list = [Root { name: "pkg", kind: RootKind::Package { globs: &globs } }];
    let roots = Roots::new(&root_list).unwrap();
    let mount_list = [Mount { spelling: b"/pkg", root: RootId(0) }];
    let mounts = Mounts::new(&mount_list, Some(b"/pkg"), &roots).unwrap();
    let env = [EnvGrant { name: b"CARGO_PKG_NAME", value: EnvValue::Set(b"demo") }];
    let grants = Grants::proc_macro(ProcMacroOpts {
        key: CallKey([1; 32]),
        name: b"Demo",
        input,
        attr: None,
        roots,
        mounts,
        env: &env,
        fuel: Fuel { instructions: u64::MAX, cpu_ns: u64::MAX, calls: 10_000 },
        deadline: Deadline { within_ns: DEADLINE },
        ..
    })
    .unwrap();
    f(grants)
}

/// The package's one file, as the driver's lookups find it.
fn look(path: &[u8]) -> Found<'static> {
    match path {
        b"src/lib.rs" => Found::File { bytes: b"pub fn f() {}" },
        _ => Found::Absent,
    }
}

/// `frame` to the host, its lookups answered; the answer frame's body, if one came back.
fn to_host(host: &mut MacroHost<'_, '_, '_, 'static>, now: u64, frame: &[u8]) -> Option<AVec<'static, u8>> {
    let (body, used) = split_frame(frame, WireLimits::SHIPPED).unwrap().unwrap();
    assert_eq!(used, frame.len());
    host.step(Nanos(now), Event::Frame(body)).unwrap();
    drain(host, now)
}

fn drain(host: &mut MacroHost<'_, '_, '_, 'static>, now: u64) -> Option<AVec<'static, u8>> {
    loop {
        let found = match host.next_effect() {
            Some(Effect::Send { frame }) => {
                let (body, _) = split_frame(frame, WireLimits::SHIPPED).unwrap().unwrap();
                return Some(bytes(body));
            }
            Some(Effect::Look { path, .. }) => look(path),
            Some(Effect::Kill) | None => return None,
            Some(other) => panic!("a macro run made the effect {other:?}"),
        };
        host.step(Nanos(now), Event::Looked(found)).unwrap();
    }
}

/// A session's output, owned.
#[derive(Debug)]
enum Got {
    Send(AVec<'static, u8>),
    Run(MacroId, ProcKind, AVec<'static, u8>),
    Reply(AVec<'static, u8>),
    Answered,
    Last,
}

fn own(output: Output<'_>) -> (Got, Option<AVec<'static, u8>>) {
    match output {
        Output::Send { frame } => (Got::Send(bytes(frame)), None),
        Output::Run { which, kind, input } => (Got::Run(which, kind, bytes(input)), None),
        Output::Reply { bytes: reply } => (Got::Reply(bytes(reply)), None),
        Output::Answered => (Got::Answered, None),
        Output::Last { frame } => (Got::Last, Some(bytes(frame))),
    }
}

/// A session and a host joined by frames: the process and blaze of one call.
struct Link<'h, 's, 'g> {
    session: Session<'h, 'static>,
    host: MacroHost<'h, 's, 'g, 'static>,
    now: u64,
}

impl<'h, 's, 'g> Link<'h, 's, 'g> {
    /// Started: `Hello` answered and the client's run input returned.
    fn start(call: Call<'h, 's>, call_bytes: &'h [u8], grants: Grants<'g>, now: u64) -> (Link<'h, 's, 'g>, AVec<'static, u8>) {
        let host = MacroHost::new(MacroHostOpts { program: &PROGRAM, call, file: Some(FILE), grants, alloc: alloc(), scratch: alloc(), .. })
            .unwrap();
        let mut session = Session::new(SessionOpts { call: call_bytes, alloc: alloc(), .. });
        let (hello, _) = own(session.start().unwrap());
        let mut link = Link { session, host, now };
        let Got::Send(frame) = hello else { panic!("the session's start sent no hello") };
        let answer = to_host(&mut link.host, now, &frame).unwrap();
        let (run, _) = own(link.session.step(Input::Frame(&answer)).unwrap());
        let Got::Run(which, kind, input) = run else { panic!("the hello's answer started no run") };
        assert_eq!((which, kind), (MacroId(0), ProcKind::Derive));
        (link, input)
    }

    /// `input` to the session, every frame it sends answered, until it replies or ends.
    fn exchange(&mut self, input: Input<'_>) -> Got {
        let (mut got, mut last) = own(self.session.step(input).unwrap());
        loop {
            match got {
                Got::Send(frame) => {
                    let answer = to_host(&mut self.host, self.now, &frame).unwrap();
                    (got, last) = own(self.session.step(Input::Frame(&answer)).unwrap());
                }
                Got::Last => {
                    assert!(to_host(&mut self.host, self.now, &last.unwrap()).is_none());
                    return Got::Last;
                }
                other => return other,
            }
        }
    }

    /// A request's reply, or `None` when the session ended the call on it.
    fn request(&mut self, request: &[u8]) -> Option<AVec<'static, u8>> {
        match self.exchange(Input::Request(request)) {
            Got::Reply(reply) => Some(reply),
            Got::Last => None,
            other => panic!("a request gave {other:?}"),
        }
    }

    fn sys(&mut self, call: SysCall<'_>) {
        let got = self.exchange(Input::Sys(call));
        assert!(matches!(got, Got::Answered), "{got:?}");
    }

    fn returned(&mut self, output: &[u8]) {
        let got = self.exchange(Input::Returned(output));
        assert!(matches!(got, Got::Last), "{got:?}");
    }

    fn tick(&mut self, now: u64) {
        self.host.step(Nanos(now), Event::Tick).unwrap();
        assert!(drain(&mut self.host, now).is_none());
    }

    fn finish(self) -> Result<Outcome<'static>, Refused<'h, 'static>> {
        self.host.finish()
    }
}

fn request(method: Method, f: impl FnOnce(&mut Writer<'_, '_>)) -> AVec<'static, u8> {
    let mut out = Vec::new_in(alloc());
    out.push(method.tag());
    f(&mut Writer { out: &mut out });
    out
}

/// The client's run output `Ok(TokenStream(handle))`.
fn output(handle: Option<u32>) -> AVec<'static, u8> {
    let mut out = Vec::new_in(alloc());
    let mut w = Writer { out: &mut out };
    w.ok(true);
    w.some(handle.is_some());
    if let Some(handle) = handle {
        w.handle(handle);
    }
    out
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

/// The item's handle: the last of the run input's (`ExpnGlobals`, then the item).
fn item_handle(run_input: &[u8]) -> u32 {
    u32_at(run_input, run_input.len() - 4)
}

/// The stream as text, its tokens apart: an identifier's or literal's text, a punctuation's
/// character, a delimiter's (a literal's text is the inside of its quotes).
fn show(stream: &TokenStream<'_>) -> AVec<'static, u8> {
    let mut out = Vec::new_in(alloc());
    for token in stream.tokens() {
        if !out.is_empty() {
            out.push(b' ');
        }
        let delim = |d: Delim, open: bool| match (d, open) {
            (Delim::Paren, true) => b'(',
            (Delim::Paren, false) => b')',
            (Delim::Brace, true) => b'{',
            (Delim::Brace, false) => b'}',
            (Delim::Bracket, true) => b'[',
            (Delim::Bracket, false) => b']',
            (Delim::Invisible, _) => b'~',
        };
        match token.kind {
            TokenKind::Punct { char, .. } => {
                let mut buf = [0; 4];
                out.extend_from_slice(char.encode_utf8(&mut buf).as_bytes());
            }
            TokenKind::Open(d) => out.push(delim(d, true)),
            TokenKind::Close(d) => out.push(delim(d, false)),
            TokenKind::Ident { .. } | TokenKind::Literal { .. } => out.extend_from_slice(stream.text_of(token).as_bytes()),
        }
    }
    out
}

fn refused_why<'a>(result: Result<Outcome<'a>, Refused<'_, 'a>>) -> Why<'a> {
    match result {
        Ok(_) => panic!("the call was taken"),
        Err(refused) => {
            // Every refusal names the macro (rule 19).
            assert_eq!((refused.krate, refused.name, refused.kind), ("demo_macros", "Demo", ProcKind::Derive));
            refused.why
        }
    }
}

#[test]
fn echo_gives_back_the_input_with_its_origins() {
    let input = item("struct S { a : u8 }");
    let call = derive(&input);
    let call_bytes = encoded(&call);
    with_grants(&call_bytes, |grants| {
        let (mut link, run) = Link::start(call, &call_bytes, grants, 0);
        link.returned(&output(Some(item_handle(&run))));
        let outcome = link.finish().unwrap();
        let produced = &outcome.produced;
        assert_eq!(&show(&produced.tokens)[..], &show(&input)[..]);
        let n = u32::try_from(input.tokens().len()).unwrap();
        assert!(produced.origins.iter().copied().eq((0..n).map(TokenOrigin::Input)));
        assert_eq!(outcome.done.findings.iter().count(), 0);
    });
}

#[test]
fn echo_through_trees_keeps_groups_and_spans() {
    let input = item("struct S { a : u8 , b : ( u8 , [ u8 ; 2 ] ) }");
    let call = derive(&input);
    let call_bytes = encoded(&call);
    with_grants(&call_bytes, |grants| {
        let (mut link, run) = Link::start(call, &call_bytes, grants, 0);
        let handle = item_handle(&run);
        let trees = link.request(&request(Method::TsIntoTrees, |w| w.handle(handle))).unwrap();
        assert_eq!(trees[0], 0);
        // `ts_concat_trees(None, trees)`: the reply's `Vec<TokenTree>` is the request's.
        let mut concat = request(Method::TsConcatTrees, |w| w.some(false));
        concat.extend_from_slice(&trees[1..]);
        let joined = link.request(&concat).unwrap();
        link.returned(&output(Some(u32_at(&joined, 1))));
        let produced = link.finish().unwrap().produced;
        assert_eq!(&show(&produced.tokens)[..], &show(&input)[..]);
        let n = u32::try_from(input.tokens().len()).unwrap();
        assert!(produced.origins.iter().copied().eq((0..n).map(TokenOrigin::Input)));
    });
}

#[test]
fn parsed_tokens_are_made_at_the_call_site() {
    let input = item("struct S ;");
    let call = derive(&input);
    let call_bytes = encoded(&call);
    with_grants(&call_bytes, |grants| {
        let (mut link, _) = Link::start(call, &call_bytes, grants, 0);
        let reply = link.request(&request(Method::TsFromStr, |w| w.str("x + 1u8"))).unwrap();
        assert_eq!(&reply[..2], &[0, 0]);
        link.returned(&output(Some(u32_at(&reply, 2))));
        let produced = link.finish().unwrap().produced;
        assert_eq!(&show(&produced.tokens)[..], &b"x + 1u8"[..]);
        assert!(produced.origins.iter().all(|o| *o == TokenOrigin::Made(Transparency::Transparent)));
        assert!(matches!(produced.tokens.tokens()[2].kind, TokenKind::Literal { kind: LitKind::Integer, suffix_len: 2, .. }));
    });
}

#[test]
fn env_is_a_fact_and_an_ungranted_name_a_finding() {
    let input = item("struct S ;");
    let call = derive(&input);
    let call_bytes = encoded(&call);
    with_grants(&call_bytes, |grants| {
        let (mut link, _) = Link::start(call, &call_bytes, grants, 0);
        let reply = link.request(&request(Method::TrackEnvVar, |w| {
            w.str("CARGO_PKG_NAME");
            w.some(true);
            w.str("demo");
        }));
        assert_eq!(reply.as_deref(), Some(&[0u8][..]));
        link.sys(SysCall::Env { name: b"CARGO_PKG_NAME" });
        assert_eq!(link.session.answer().unwrap(), Answer::Value { value: Some(&b"demo"[..]) });
        link.sys(SysCall::Env { name: b"SECRET" });
        assert_eq!(link.session.answer().unwrap(), Answer::Value { value: None });
        link.returned(&output(None));
        let done = link.finish().unwrap().done;
        let facts: AVec<'_, (FactKind, &[u8], bool)> = {
            let mut facts = Vec::new_in(alloc());
            facts.extend(done.facts.iter().map(|f| (f.kind, f.name, f.value.is_some())));
            facts
        };
        assert!(facts.contains(&(FactKind::Env, &b"CARGO_PKG_NAME"[..], true)));
        assert!(facts.contains(&(FactKind::Env, &b"SECRET"[..], false)));
        let finding = done.findings.iter().next().unwrap();
        assert_eq!((finding.call, finding.name, finding.grant), (CallKind::Env, &b"SECRET"[..], Grant::Env));
    });
}

#[test]
fn a_path_outside_the_package_is_refused_by_name() {
    let input = item("struct S ;");
    let call = derive(&input);
    let call_bytes = encoded(&call);
    let cases: [(&str, Grant); 2] = [("/etc/passwd", Grant::Mount), ("/pkg/secret.key", Grant::Glob { root: RootId(0) })];
    for (path, want) in cases {
        with_grants(&call_bytes, |grants| {
            let (mut link, _) = Link::start(call, &call_bytes, grants, 0);
            // The client is told `Ok(())`; blaze refuses the call when it ends.
            assert_eq!(link.request(&request(Method::TrackPath, |w| w.str(path))).as_deref(), Some(&[0u8][..]));
            link.returned(&output(None));
            match refused_why(link.finish()) {
                why @ Why::Outside { .. } => {
                    assert_eq!(why.rule(), 9);
                    let Why::Outside { path: got, grant } = why else { unreachable!() };
                    assert_eq!((&got[..], grant), (path.as_bytes(), want));
                }
                other => panic!("{path}: {other:?}"),
            }
        });
    }
    // A path the package holds is read, and the call taken.
    with_grants(&call_bytes, |grants| {
        let (mut link, _) = Link::start(call, &call_bytes, grants, 0);
        assert!(link.request(&request(Method::TrackPath, |w| w.str("/pkg/src/lib.rs"))).is_some());
        link.returned(&output(None));
        assert_eq!(link.finish().unwrap().done.reads.len(), 1);
    });
}

#[test]
fn a_macro_that_never_answers_is_stopped_at_its_deadline() {
    let input = item("struct S ;");
    let call = derive(&input);
    let call_bytes = encoded(&call);
    with_grants(&call_bytes, |grants| {
        let (mut link, _) = Link::start(call, &call_bytes, grants, 100);
        link.tick(100 + DEADLINE / 2);
        link.tick(100 + DEADLINE);
        let why = refused_why(link.finish());
        assert_eq!(why.rule(), 18);
        assert!(matches!(why, Why::Ended(Ended::Deadline { after_ns: DEADLINE })), "{why:?}");
    });
}

#[test]
fn malformed_bytes_are_typed_refusals_never_panics() {
    let input = item("struct S ;");
    let call = derive(&input);
    let call_bytes = encoded(&call);
    let short = [Method::TsDrop.tag(), 1, 0];
    let unknown = [Method::TsDrop.tag(), 99, 0, 0, 0];
    let trailing = [Method::TsIsEmpty.tag(), 1, 0, 0, 0, 9];
    let cases: [(&[u8], ServerError); 5] = [
        (&[], ServerError::Malformed { method: None, what: Malformed::Short(Field::Byte) }),
        (&[200], ServerError::NoSuchMethod(200)),
        (&short, ServerError::Malformed { method: Some(Method::TsDrop), what: Malformed::Short(Field::Handle) }),
        (&unknown, ServerError::NoSuchStream { method: Method::TsDrop, handle: 99 }),
        (&trailing, ServerError::Malformed { method: Some(Method::TsIsEmpty), what: Malformed::Trailing }),
    ];
    for (bad, want) in cases {
        with_grants(&call_bytes, |grants| {
            let (mut link, _) = Link::start(call, &call_bytes, grants, 0);
            assert!(link.request(bad).is_none());
            match refused_why(link.finish()) {
                Why::Server(got) => assert_eq!(got, want),
                other => panic!("{other:?}"),
            }
        });
    }
    // A client output that is not `Result<TokenStream, PanicMessage>`.
    with_grants(&call_bytes, |grants| {
        let (mut link, _) = Link::start(call, &call_bytes, grants, 0);
        link.returned(&[7]);
        match refused_why(link.finish()) {
            Why::Server(got) => assert_eq!(got, ServerError::Output(Malformed::Tag(Field::ResultTag, 7))),
            other => panic!("{other:?}"),
        }
    });
    // A position: refused by rule 71.
    with_grants(&call_bytes, |grants| {
        let (mut link, _) = Link::start(call, &call_bytes, grants, 0);
        assert!(link.request(&request(Method::SpanLine, |w| w.handle(4))).is_none());
        let why = refused_why(link.finish());
        assert_eq!(why.rule(), 71);
        assert!(matches!(why, Why::Server(ServerError::Position { method: Method::SpanLine })));
    });
}

#[test]
fn span_file_answers_a_written_token_and_refuses_a_site() {
    let input = item("struct S ;");
    let call = derive(&input);
    let call_bytes = encoded(&call);
    with_grants(&call_bytes, |grants| {
        let (mut link, _) = Link::start(call, &call_bytes, grants, 0);
        // Handles 1 to 3 are the sites; the item's tokens are 4 on.
        let reply = link.request(&request(Method::SpanFile, |w| w.handle(4))).unwrap();
        let mut want = Vec::new_in(alloc());
        want.push(0);
        Writer { out: &mut want }.str(FILE);
        assert_eq!(&reply[..], &want[..]);
        link.returned(&output(None));
        let produced = link.finish().unwrap().produced;
        let reads = produced.reads.list();
        assert_eq!(reads.len(), 1);
        assert_eq!(reads[0].kind, ReadKind::ProcMacro);
    });
    with_grants(&call_bytes, |grants| {
        let (mut link, _) = Link::start(call, &call_bytes, grants, 0);
        assert!(link.request(&request(Method::SpanFile, |w| w.handle(1))).is_none());
        assert!(matches!(refused_why(link.finish()), Why::File(FileWhy::Site)));
    });
}

/// A hostile process: `Hello`, then `Return` of `outcome`, written by hand.
fn hostile(call: Call<'_, '_>, call_bytes: &[u8], outcome: &[u8]) -> Why<'static> {
    with_grants(call_bytes, |grants| {
        let mut host = MacroHost::new(MacroHostOpts { program: &PROGRAM, call, file: Some(FILE), grants, alloc: alloc(), scratch: alloc(), .. })
            .unwrap();
        let mut guest = Guest::new(GuestOpts { alloc: alloc(), .. });
        guest.call(&SysCall::Hello { protocol: PROTOCOL }).unwrap();
        let Some(GuestEffect::Send { frame }) = guest.next_effect() else { panic!("no hello") };
        let answer = to_host(&mut host, 0, frame).unwrap();
        guest.step(Nanos(0), GuestEvent::Frame(&answer)).unwrap();
        guest.call(&SysCall::Return { output: outcome }).unwrap();
        let Some(GuestEffect::Send { frame }) = guest.next_effect() else { panic!("no return") };
        assert!(to_host(&mut host, 0, frame).is_none());
        match host.finish() {
            Ok(_) => panic!("a hostile outcome was taken"),
            Err(refused) => {
                assert_eq!((refused.krate, refused.name), ("demo_macros", "Demo"));
                refused.why
            }
        }
    })
}

/// An outcome of `tokens` and one diagnostic at `diagnostic`.
fn outcome_of(tokens: &[(TokenKind, TokenOrigin, &str)], diagnostic: Option<TokenOrigin>) -> AVec<'static, u8> {
    let mut out = Vec::new_in(alloc());
    let mut w = OutcomeWriter::start(&mut out, false);
    for (kind, origin, text) in tokens {
        w.token(*kind, *origin, &[*text]);
    }
    w.diagnostic(ProcLevel::Error, diagnostic, "it went wrong");
    w.end(None);
    out
}

const IDENT: TokenKind = TokenKind::Ident { raw: Raw::No, spacing: Spacing::Alone };
const STR: TokenKind = TokenKind::Literal { kind: LitKind::Str, suffix_len: 0, spacing: Spacing::Alone };
const MADE: TokenOrigin = TokenOrigin::Made(Transparency::Transparent);

#[test]
fn a_forged_mark_is_refused_naming_the_macro() {
    let input = item("struct S ;");
    let call = derive(&input);
    let call_bytes = encoded(&call);
    let forged = [TokenOrigin::Def(0), TokenOrigin::Params(0), TokenOrigin::Input(3), TokenOrigin::AttrArgs(0)];
    for origin in forged {
        let why = hostile(call, &call_bytes, &outcome_of(&[(IDENT, MADE, "x"), (IDENT, origin, "y")], None));
        assert_eq!(why.rule(), 9);
        assert!(matches!(why, Why::Outcome(OutcomeError::ForgedMark { at: OutcomeAt::Token(1), origin: got }) if got == origin), "{why:?}");
        let why = hostile(call, &call_bytes, &outcome_of(&[], Some(origin)));
        assert!(matches!(why, Why::Outcome(OutcomeError::ForgedMark { at: OutcomeAt::Diagnostic(0), .. })), "{why:?}");
    }
    // The control: the same outcome with marks the call handed out is taken.
    with_grants(&call_bytes, |grants| {
        let (mut link, _) = Link::start(call, &call_bytes, grants, 0);
        let reply = link.request(&request(Method::TsFromStr, |w| w.str("y"))).unwrap();
        link.returned(&output(Some(u32_at(&reply, 2))));
        assert!(link.finish().is_ok());
    });
}

#[test]
fn text_is_lexed_by_the_host_never_trusted() {
    let input = item("struct S ;");
    let call = derive(&input);
    let call_bytes = encoded(&call);
    let not_tokens: [(TokenKind, &str); 5] =
        [(IDENT, "$crate"), (IDENT, "a b"), (IDENT, "a//"), (IDENT, "1x"), (STR, "a\" + \"b")];
    for (kind, text) in not_tokens {
        let why = hostile(call, &call_bytes, &outcome_of(&[(IDENT, MADE, "ok"), (kind, MADE, text)], None));
        assert!(matches!(why, Why::Outcome(OutcomeError::NotAToken { at: 1 })), "{text}: {why:?}");
    }
    let punct = TokenKind::Punct { char: 'a', spacing: Spacing::Alone };
    let why = hostile(call, &call_bytes, &outcome_of(&[(punct, MADE, "")], None));
    assert!(matches!(why, Why::Outcome(OutcomeError::BadPunct { at: 0, char: 'a' })), "{why:?}");
    let close = TokenKind::Close(Delim::Paren);
    let why = hostile(call, &call_bytes, &outcome_of(&[(close, MADE, "")], None));
    assert!(matches!(why, Why::Outcome(OutcomeError::Unbalanced { at: 0 })), "{why:?}");
    for garbage in [&b""[..], b"\x01\x09", b"\x02\x01", b"garbage"] {
        let why = hostile(call, &call_bytes, garbage);
        assert_eq!(why.rule(), 3);
        assert!(matches!(why, Why::Outcome(OutcomeError::Malformed(_))), "{why:?}");
    }
}

#[test]
fn a_garbage_frame_crashes_the_run() {
    let input = item("struct S ;");
    let call = derive(&input);
    let call_bytes = encoded(&call);
    with_grants(&call_bytes, |grants| {
        let mut host = MacroHost::new(MacroHostOpts { program: &PROGRAM, call, file: Some(FILE), grants, alloc: alloc(), scratch: alloc(), .. })
            .unwrap();
        host.step(Nanos(0), Event::Frame(&[0xff, 0x00, 0x13])).unwrap();
        assert!(drain(&mut host, 0).is_none());
        match host.finish() {
            Err(Refused { why: Why::Ended(Ended::Crashed(Crash::Wire(_))), .. }) => {}
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("a garbage frame was taken"),
        }
    });
}

#[test]
fn the_same_call_twice_gives_equal_outcome_bytes() {
    let input = item("struct S { a : u8 }");
    let call = derive(&input);
    let call_bytes = encoded(&call);
    let run = |start: u64| {
        with_grants(&call_bytes, |grants| {
            let (mut link, run) = Link::start(call, &call_bytes, grants, start);
            link.request(&request(Method::TrackEnvVar, |w| {
                w.str("CARGO_PKG_NAME");
                w.some(false);
            }));
            let made = link.request(&request(Method::TsFromStr, |w| w.str("impl S { fn f () { } }"))).unwrap();
            let joined = link
                .request(&request(Method::TsConcatStreams, |w| {
                    w.some(true);
                    w.handle(item_handle(&run));
                    w.usize(1);
                    w.handle(u32_at(&made, 2));
                }))
                .unwrap();
            link.request(&request(Method::SpanFile, |w| w.handle(4))).unwrap();
            link.returned(&output(Some(u32_at(&joined, 1))));
            link.finish().unwrap()
        })
    };
    let (a, b) = (run(0), run(777_000));
    assert_eq!(&a.done.output[..], &b.done.output[..]);
    assert!(a.produced.tokens == b.produced.tokens);
    assert_eq!(&a.produced.origins[..], &b.produced.origins[..]);
    assert_eq!(a.produced.reads.list(), b.produced.reads.list());
    assert_eq!(a.done.reads.digest(), b.done.reads.digest());
    assert_eq!(a.done.facts.digest(), b.done.facts.digest());
    let mut want = show(&input);
    want.extend_from_slice(b" impl S { fn f ( ) { } }");
    assert_eq!(&show(&a.produced.tokens)[..], &want[..]);
}

#[test]
fn a_call_with_other_grants_is_refused_before_it_runs() {
    let input = item("struct S ;");
    let call = derive(&input);
    let other = item("struct T ;");
    let other_bytes = encoded(&derive(&other));
    with_grants(&other_bytes, |grants| {
        let made = MacroHost::new(MacroHostOpts { program: &PROGRAM, call, file: None, grants, alloc: alloc(), scratch: alloc(), .. });
        assert!(matches!(made, Err(crate::host::MacroHostError::NotGranted)));
    });
}
