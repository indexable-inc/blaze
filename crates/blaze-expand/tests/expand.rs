//! blaze-expand's interface: `macro_rules!` matching and transcription, hygiene, the keys, the
//! builtins `println!` needs, the derives, the attribute and proc-macro seam, errors as values,
//! nested calls listed, and a control whose planted bug the key oracle must catch.

#![feature(allocator_api)]

use blaze_expand::{
    AlsoCopy, BuiltinAttr, BuiltinDerive, BuiltinFn, Call, CallKind, CallShape, CallSite, Constness, Ctx, DefAt,
    DefSite, Delim, DeriveFacts, Diagnostic, Eager, Edition, ExpandCx, ExpandError, Expanded,
    Expansion, Export, Hole, HoleOf, HostAsk, HostError, HostWhy, IncludeAs, Included, Inside,
    LitKind, LocalInner, MacroDefinition, MarkOf, NestedKind, TokenOrigin, Place, PlaceOf, ProcCall, ProcDiagnostic,
    ProcKind, ProcLevel, Produced, Raw, Text, ReadAnswer, ReadKind, Reads, Spacing, TokenKind, TokenStream, call_key, expand,
    expansion_key, find_calls, lex, CfgPredicateError, cfg_predicate,
};
use blaze_expand::{DefinitionError, DefinitionRecords, stream_record_id};
use blaze_format::Aligned;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{Alloc, Vec as OwnVec};
use blaze_surface::{BuildError, MacroDefView};
use blaze_sim::{Broke, Config, Tape};

/// The outside of the tests' expansions: fixed env vars, files, enabled cfg names and a
/// position, and a proc macro that echoes its input.
struct TestCx {
    env: &'static [(&'static str, &'static str)],
    files: &'static [(&'static str, &'static [u8])],
    cfgs: &'static [&'static str],
    line: u32,
    /// What the echo proc macro also gives back: diagnostics, or output a host must not give.
    emits: &'static [Emit],
}

/// A proc macro's extra output, as an untrusted host could give it.
#[derive(Clone, Copy, Debug)]
enum Emit {
    /// A diagnostic of this level on its first input token.
    Level(ProcLevel),
    /// A panic with a message.
    Panic,
    /// A diagnostic whose text is past the end of the text given.
    TextPastEnd,
    /// A diagnostic whose text cuts a character.
    TextMidChar,
    /// A diagnostic naming an input token past the input.
    OriginPastInput,
    /// A token whose origin is the definition's, which a proc macro has none of.
    TokenFromDef,
}

impl TestCx {
    fn new() -> TestCx {
        TestCx {
            emits: &[],
            env: &[("V", "v")],
            files: &[
                ("a.txt", b"hi"),
                ("m.rs", b"\xef\xbb\xbffn f() -> u8 { 1 }"),
                ("bad.rs", b"\""),
            ],
            cfgs: &["unix"],
            line: 12,
        }
    }
}

const FAILED: HostError = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Failed };

impl ExpandCx for TestCx {
    fn env(&self, name: &str) -> Result<Option<&str>, HostError> {
        Ok(self.env.iter().find(|(it, _)| *it == name).map(|(_, value)| *value))
    }

    /// A predicate holds when its first token is an enabled name.
    fn cfg(&self, predicate: &TokenStream<'_>) -> Result<bool, HostError> {
        let first = predicate.tokens().first();
        Ok(first.is_some_and(|token| self.cfgs.contains(&predicate.text_of(token))))
    }

    fn include(&self, path: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Ok(match self.files.iter().find(|(it, _)| *it == path) {
            Some((_, bytes)) => Included::Bytes(bytes),
            None => Included::Missing,
        })
    }

    fn place(&self, what: PlaceOf) -> Result<Place<'_>, HostError> {
        Ok(match what {
            PlaceOf::Line => Place::Number(self.line),
            PlaceOf::Column => Place::Number(5),
            PlaceOf::File => Place::Text("src/lib.rs"),
            PlaceOf::ModulePath => Place::Text("demo"),
        })
    }

    fn run_proc_macro<'a>(
        &self,
        def: &MacroDefinition<'_>,
        call: ProcCall<'_>,
        alloc: Alloc<'a>,
    ) -> Result<Produced<'a>, HostError> {
        // The call carries the file `file!()` is answered with here ([`TestCx::place`]): what
        // `Span::file` answers (blaze-proc-macro server.rs). Every proc-macro test below
        // passing through here checks it.
        if call.file != Ok("src/lib.rs") {
            return Err(FAILED);
        }
        let mut tokens = TokenStream::new_in(alloc);
        let mut origins = OwnVec::new_in(alloc);
        for at in 0..call.input.len() {
            tokens.push_from(call.input, at).map_err(|_| FAILED)?;
            origins.push(TokenOrigin::Input(u32::try_from(at).map_err(|_| FAILED)?));
        }
        let mut reads = Reads::new_in(alloc);
        let name = def.proc_name().ok_or(FAILED)?;
        reads.push(ReadKind::ProcMacro, name, ReadAnswer::Bool(true)).map_err(|_| FAILED)?;
        // "é!" : bytes 0..2 are `é`, so 1..2 cuts it.
        let mut text = OwnVec::new_in(alloc);
        text.extend_from_slice("é!".as_bytes());
        let whole = Text { at: 0, len: 3 };
        let mut diagnostics = OwnVec::new_in(alloc);
        for emit in self.emits {
            let d = match *emit {
                Emit::Level(level) => ProcDiagnostic::Emitted { level, message: whole, at: Some(TokenOrigin::Input(0)) },
                Emit::Panic => ProcDiagnostic::Panicked { message: whole },
                Emit::TextPastEnd => ProcDiagnostic::Panicked { message: Text { at: 2, len: 9 } },
                Emit::TextMidChar => ProcDiagnostic::Panicked { message: Text { at: 1, len: 1 } },
                Emit::OriginPastInput => ProcDiagnostic::Emitted {
                    level: ProcLevel::Error,
                    message: whole,
                    at: Some(TokenOrigin::Input(u32::try_from(call.input.len()).map_err(|_| FAILED)?)),
                },
                Emit::TokenFromDef => {
                    *origins.first_mut().ok_or(FAILED)? = TokenOrigin::Def(0);
                    continue;
                }
            };
            diagnostics.push(d);
        }
        Ok(Produced { tokens, origins, reads, diagnostics, text })
    }
}

fn at(krate: u8) -> DefAt {
    DefAt {
        site: DefSite { krate: CrateItemKey(ContentId([krate; 32])), module: None },
        edition: Edition::E2021,
        export: Export::Local,
        local_inner: LocalInner::No,
        transparency: None,
    }
}

fn site() -> CallSite {
    CallSite {
        edition: Edition::E2021,
        panic_edition: Edition::E2021,
        ctx: Ctx::root(Edition::E2021),
    }
}

fn tokens(text: &str) -> TokenStream<'static> {
    lex(text, Edition::E2021, blaze_own::testing::heap().alloc()).expect("the test's text lexes")
}

fn rules(body: &str, krate: u8) -> MacroDefinition<'static> {
    MacroDefinition::rules(tokens(body), at(krate))
}

fn builtin(which: BuiltinFn) -> MacroDefinition<'static> {
    MacroDefinition::builtin_fn(which, at(1), blaze_own::testing::heap().alloc())
}

fn done(expanded: Result<Expanded<'static>, ExpandError>) -> Expansion<'static> {
    match expanded.expect("the call expands") {
        Expanded::Expansion(expansion) => expansion,
        Expanded::NeedsExpanded(nested) => {
            panic!("the call needs {} nested calls first", nested.len())
        }
    }
}

/// Expands `m!(args)` of `def` where it is written.
fn call_fn(def: &MacroDefinition<'static>, args: &str, cx: &TestCx) -> Expansion<'static> {
    let input = tokens(args);
    done(expand(def, &Call::fn_like(&input, site()), cx, blaze_own::testing::heap().alloc()))
}

/// The tokens as text, one space between each but between a joint punct and the punct it is
/// joined to (`::` is one operator, `: :` two tokens); invisible groups not shown.
fn show(stream: &TokenStream<'_>) -> String {
    let mut out = String::new();
    let mut joined = false;
    for token in stream.tokens() {
        let glued = joined && matches!(token.kind, TokenKind::Punct { .. });
        joined = matches!(token.kind, TokenKind::Punct { spacing: Spacing::Joint, .. });
        let text = stream.text_of(token);
        let piece = match token.kind {
            TokenKind::Ident { raw: Raw::Yes, .. } => format!("r#{text}"),
            TokenKind::Ident { raw: Raw::No, .. } => text.to_owned(),
            TokenKind::Punct { char, .. } => char.to_string(),
            TokenKind::Literal { kind: LitKind::Str, .. } => format!("\"{text}\""),
            TokenKind::Literal { kind: LitKind::Char, .. } => format!("'{text}'"),
            TokenKind::Literal { kind: LitKind::ByteStr, .. } => format!("b\"{text}\""),
            TokenKind::Literal { .. } => text.to_owned(),
            TokenKind::Open(Delim::Paren) => "(".to_owned(),
            TokenKind::Open(Delim::Brace) => "{".to_owned(),
            TokenKind::Open(Delim::Bracket) => "[".to_owned(),
            TokenKind::Close(Delim::Paren) => ")".to_owned(),
            TokenKind::Close(Delim::Brace) => "}".to_owned(),
            TokenKind::Close(Delim::Bracket) => "]".to_owned(),
            TokenKind::Open(Delim::Invisible) | TokenKind::Close(Delim::Invisible) => continue,
        };
        if !out.is_empty() && !glued {
            out.push(' ');
        }
        out.push_str(&piece);
    }
    out
}

#[test]
fn an_expr_fragment_is_transcribed() {
    let cx = TestCx::new();
    let exp = call_fn(&rules("($e:expr) => { $e * 2 }", 1), "1 + 2", &cx);
    // A multi-token `expr` fragment is one operand where it is put, as rustc's: mbe writes it
    // parenthesised (inside its invisible group), so `$e * 2` of `1 + 2` is 6, not 5.
    assert_eq!(show(&exp.tokens), "( 1 + 2 ) * 2");
    assert_eq!(exp.arm, Some(0));
    assert!(exp.diagnostics.is_empty());
}

#[test]
fn a_repetition_is_transcribed() {
    let cx = TestCx::new();
    let exp = call_fn(&rules("($($x:ident),*) => { $(let $x = 1;)* }", 1), "a, b, c", &cx);
    assert_eq!(show(&exp.tokens), "let a = 1 ; let b = 1 ; let c = 1 ;");
    // `a` is the call's, `let` the definition's.
    assert_eq!(exp.origins[1], TokenOrigin::Input(0));
    assert!(matches!(exp.origins[0], TokenOrigin::Def(_)));
    assert_eq!(exp.hygiene(1).mark, None);
    assert_eq!(exp.hygiene(0).mark.map(|it| it.of), Some(MarkOf::This));
}

#[test]
fn a_nested_repetition_is_transcribed() {
    let cx = TestCx::new();
    let def = rules("($($name:ident : $($v:literal)*);*) => { $(fn $name() { $($v;)* })* }", 1);
    let exp = call_fn(&def, "f: 1 2; g: 3", &cx);
    assert_eq!(show(&exp.tokens), "fn f ( ) { 1 ; 2 ; } fn g ( ) { 3 ; }");
}

#[test]
fn dollar_crate_carries_the_defining_crate() {
    let cx = TestCx::new();
    let exp = call_fn(&rules("() => { $crate::x }", 7), "", &cx);
    assert_eq!(show(&exp.tokens), "$crate :: x");
    let mark = exp.hygiene(0).mark.expect("`$crate` is the definition's");
    assert_eq!(mark.site.krate, CrateItemKey(ContentId([7; 32])));
    assert_eq!(mark.of, MarkOf::This);
}

/// `m!(args)` in `source` at whatever offset, its args cut out as the reader would.
fn expand_in_source(
    def: &MacroDefinition<'static>,
    source: &str,
    cx: &TestCx,
) -> (Expansion<'static>, OwnVec<u8, Alloc<'static>>, blaze_expand::ExpansionKey) {
    let file = tokens(source);
    let calls = find_calls(&file, blaze_own::testing::heap().alloc()).expect("the file's calls are found");
    let call = calls.iter().find(|it| it.kind == NestedKind::FnLike).expect("one call");
    let input = file.copy_in(call.inner().to_usize(), blaze_own::testing::heap().alloc()).expect("the args copy");
    let call = Call::fn_like(&input, site());
    let exp = done(expand(def, &call, cx, blaze_own::testing::heap().alloc()));
    let mut bytes = OwnVec::new_in(blaze_own::testing::heap().alloc());
    exp.encode(&mut bytes);
    let key = expansion_key(def, &call, &exp.reads);
    (exp, bytes, key)
}

#[test]
fn the_same_call_at_two_offsets_expands_equal() {
    let cx = TestCx::new();
    let def = rules("($($x:ident),*) => { $(let $x = 1;)* }", 1);
    let prefixes = ["", "\n\n", "/* c */ ", "fn a() {}\n", "    struct S;\n// m!(x)\n"];
    let args = ["a, b", "x", "a, b, c", ""];
    blaze_sim::check(Config::cases(64), |tape: &Tape| {
        let first = tape.pick(&prefixes, "first prefix").copied().unwrap_or("");
        let second = tape.pick(&prefixes, "second prefix").copied().unwrap_or("");
        let args = tape.pick(&args, "args").copied().unwrap_or("");
        let one = expand_in_source(&def, &format!("{first}m!({args})"), &cx);
        let two = expand_in_source(&def, &format!("{second}m!({args})"), &cx);
        if one.0 != two.0 || one.1[..] != two.1[..] {
            return Err(Broke::Oracle(format!(
                "`{args}` expands unequal after `{first:?}` and `{second:?}`"
            )));
        }
        if one.2 != two.2 {
            return Err(Broke::Oracle(format!("`{args}` has two keys")));
        }
        Ok(())
    });
}

/// The tokens of `stream` as (kind letter, text, joint) triples: `i` ident, `p` punct, `l`
/// literal, `(`/`)` a group.
fn shape(stream: &TokenStream<'_>) -> Vec<(char, String, bool)> {
    stream
        .tokens()
        .iter()
        .filter_map(|token| {
            let text = stream.text_of(token).to_owned();
            Some(match token.kind {
                TokenKind::Ident { spacing, .. } => ('i', text, spacing != Spacing::Alone),
                TokenKind::Punct { char, spacing } => ('p', char.to_string(), spacing == Spacing::Joint),
                TokenKind::Literal { .. } => ('l', text, false),
                TokenKind::Open(Delim::Invisible) | TokenKind::Close(Delim::Invisible) => return None,
                TokenKind::Open(_) => ('(', text, false),
                TokenKind::Close(_) => (')', text, false),
            })
        })
        .collect()
}

/// `core`'s `impl_int_cast!` (core/src/convert/num.rs:766), called as `impl_all_int_casts!`
/// calls it: a `ty` fragment, then `as`, then a bracketed list; `no rule matched` on `core`.
#[test]
fn a_ty_fragment_followed_by_as_matches() {
    let cx = TestCx::new();
    let def = rules("($Src:ty as [$($Dst:ty),*]) => { $(const impl C<$Src> for $Dst {})* }", 1);
    let exp = call_fn(&def, "u8 as [u8, u16]", &cx);
    assert!(exp.diagnostics.is_empty(), "{:?}", exp.diagnostics);
    assert_eq!(show(&exp.tokens), "const impl C < u8 > for u8 { } const impl C < u8 > for u16 { }");
    // As the inner call of a macro whose own `$Src:ty` it passes on.
    let outer = rules("([$($Src:ty),*]) => { $(m!($Src as [u8, i8]);)* }", 1);
    let exp = call_fn(&outer, "[u8, usize]", &cx);
    // `u8` was joint with the `,` after it in the call; it is not joint with the `as` after it
    // here (a printer would write `u8as`, which `impl_int_cast!` matches no rule of).
    assert_no_glued_words(&exp.tokens);
    let calls = find_calls(&exp.tokens, blaze_own::testing::heap().alloc()).expect("the expansion's calls are found");
    assert_eq!(calls.len(), 2);
    for call in calls.iter() {
        let mut input = exp.tokens.copy_in(call.inner().to_usize(), blaze_own::testing::heap().alloc()).expect("the args copy");
        input.seal(ContentId([7; 32]));
        let inner = call_fn_tokens(&def, &input, &cx);
        assert!(inner.diagnostics.is_empty(), "{:?}: {:?}", show(&input), inner.diagnostics);
    }
}

/// No identifier or literal is joint with an identifier or literal after it: written glued,
/// two words would read as one (`Debugfor`, `u8as`).
fn assert_no_glued_words(stream: &TokenStream<'_>) {
    let tokens = shape(stream);
    for pair in tokens.windows(2) {
        let word = |t: &(char, String, bool)| t.0 == 'i' || t.0 == 'l';
        assert!(!(word(&pair[0]) && pair[0].2 && word(&pair[1])), "{pair:?} in {tokens:?}");
    }
}

fn call_fn_tokens(def: &MacroDefinition<'static>, input: &TokenStream<'_>, cx: &TestCx) -> Expansion<'static> {
    done(expand(def, &Call::fn_like(input, site()), cx, blaze_own::testing::heap().alloc()))
}

/// `core`'s `fmt_refs!` and `bitop_impls!` (an anonymous lifetime `'_` in the transcribed
/// text) and `impl_number!` (`<<$number as SimdElement>::Mask`): each expansion's lifetime is
/// a joint `'` and an identifier, and `<<` two `<`.
#[test]
fn lifetimes_and_double_angles_are_transcribed_as_written() {
    let cx = TestCx::new();
    let def = rules("($($tr:ident),*) => { $(impl<T: $tr> $tr for &T { fn fmt(&self, f: &mut F<'_>) {} })* }", 1);
    let exp = call_fn(&def, "Debug, Display", &cx);
    assert_no_glued_words(&exp.tokens);
    let tokens = shape(&exp.tokens);
    let quotes: Vec<usize> = tokens.iter().enumerate().filter(|(_, t)| t.1 == "'").map(|(at, _)| at).collect();
    assert_eq!(quotes.len(), 2, "{tokens:?}");
    for at in quotes {
        assert_eq!(tokens[at], ('p', "'".to_owned(), true), "{tokens:?}");
        assert_eq!(tokens[at + 1].0, 'i', "{tokens:?}");
        assert_eq!(tokens[at + 1].1, "_", "{tokens:?}");
    }
    let def = rules("{ $($number:ty),* } => { $(type M = Mask<<$number as E>::Mask, N>;)* }", 1);
    let exp = call_fn(&def, "f16, u8", &cx);
    assert_no_glued_words(&exp.tokens);
    let tokens = shape(&exp.tokens);
    let angles = tokens.windows(2).filter(|w| w[0].1 == "<" && w[1].1 == "<").count();
    assert_eq!(angles, 2, "{tokens:?}");
}

#[test]
fn an_edit_changes_the_key_and_a_comment_does_not() {
    let input = tokens("1");
    let call = Call::fn_like(&input, site());
    let first = rules("($e:expr) => { $e * 2 }", 1);
    let edited = rules("($e:expr) => { $e * 3 }", 1);
    let commented = rules("($e:expr) => { /* why */ $e * 2 // twice\n }", 1);
    assert_ne!(call_key(&first, &call), call_key(&edited, &call));
    assert_eq!(call_key(&first, &call), call_key(&commented, &call));
    assert_eq!(first.content(), commented.content());
}

#[test]
fn stringify_prints_its_input() {
    let cx = TestCx::new();
    let exp = call_fn(&builtin(BuiltinFn::Stringify), "a + b", &cx);
    assert_eq!(show(&exp.tokens), "\"a + b\"");
}

/// `assert!`'s condition is an expression, so a comma in its turbofish ends no argument.
#[test]
fn assert_reads_its_condition_as_an_expression() {
    let cx = TestCx::new();
    let def = builtin(BuiltinFn::Assert);
    let exp = call_fn(&def, "none::<List, Secret>()", &cx);
    let shown = show(&exp.tokens);
    assert!(shown.starts_with("if ! ( none ::< List , Secret > ( ) ) {"), "{shown}");
    assert!(shown.contains("panicking :: panic"), "{shown}");
    assert!(!shown.contains("panic_2021"), "{shown}");
    let exp = call_fn(&def, "f::<A, B>(), \"m {}\", 1", &cx);
    let shown = show(&exp.tokens);
    assert!(shown.starts_with("if ! ( f ::< A , B > ( ) ) {"), "{shown}");
    assert!(shown.contains("panic_2021 ! ( \"m {}\" , 1 )"), "{shown}");
}

#[test]
fn concat_joins_literals() {
    let cx = TestCx::new();
    let exp = call_fn(&builtin(BuiltinFn::Concat), "\"a\", 1, 'c', true, -2, 1.5", &cx);
    assert_eq!(show(&exp.tokens), "\"a1ctrue-21.5\"");
}

#[test]
fn concat_is_eager() {
    let cx = TestCx::new();
    let def = builtin(BuiltinFn::Concat);
    let input = tokens("\"x\", stringify!(y z)");
    let first = expand(&def, &Call::fn_like(&input, site()), &cx, blaze_own::testing::heap().alloc()).expect("expands");
    let Expanded::NeedsExpanded(nested) = first else { panic!("concat! lists its nested call") };
    assert_eq!(nested.len(), 1);
    let inner = input.copy_in(nested[0].inner().to_usize(), blaze_own::testing::heap().alloc()).expect("copies");
    let stringify = builtin(BuiltinFn::Stringify);
    let inner_call = Call {
        kind: CallKind::FnLike { args: &inner },
        site: site(),
        eager: Eager { inside: Inside::EagerInput, answers: &[] },
    };
    let answer = done(expand(&stringify, &inner_call, &cx, blaze_own::testing::heap().alloc()));
    let key = expansion_key(&stringify, &inner_call, &answer.reads);
    let mut sealed = answer.tokens.clone();
    sealed.seal(key.0);
    // An unsealed answer is refused.
    let unsealed = [&answer.tokens];
    let refused = Call {
        kind: CallKind::FnLike { args: &input },
        site: site(),
        eager: Eager { inside: Inside::Plain, answers: &unsealed },
    };
    assert!(matches!(expand(&def, &refused, &cx, blaze_own::testing::heap().alloc()), Err(ExpandError::Unsealed { .. })));
    let answers = [&sealed];
    let again = Call {
        kind: CallKind::FnLike { args: &input },
        site: site(),
        eager: Eager { inside: Inside::Plain, answers: &answers },
    };
    let exp = done(expand(&def, &again, &cx, blaze_own::testing::heap().alloc()));
    assert_eq!(show(&exp.tokens), "\"xy z\"");
}

#[test]
fn cfg_reads_its_outcome() {
    let cx = TestCx::new();
    let exp = call_fn(&builtin(BuiltinFn::Cfg), "unix", &cx);
    assert_eq!(show(&exp.tokens), "true");
    let read = exp.reads.list()[0];
    assert_eq!((read.kind, read.answer), (ReadKind::Cfg, ReadAnswer::Bool(true)));
    assert_eq!(exp.reads.subject(&read), "unix");
    let exp = call_fn(&builtin(BuiltinFn::Cfg), "windows", &cx);
    assert_eq!(show(&exp.tokens), "false");
}

/// `cfg!` evaluates its predicate itself and records one read per atom, in evaluation order
/// (every atom, no short cut), each with its own outcome; the predicate's text is no subject.
/// Control: a lone atom is one read whose subject is that atom.
#[test]
fn cfg_records_each_atom_it_evaluated() {
    let cx = TestCx::new();
    let exp = call_fn(&builtin(BuiltinFn::Cfg), "any(windows, all(unix, not(feature = \"a\\\"b\")))", &cx);
    assert_eq!(show(&exp.tokens), "true");
    let reads: Vec<_> = exp.reads.list().iter().map(|it| (it.kind, exp.reads.subject(it).to_string(), it.answer)).collect();
    assert_eq!(
        reads,
        [
            (ReadKind::Cfg, "windows".to_string(), ReadAnswer::Bool(false)),
            (ReadKind::Cfg, "unix".to_string(), ReadAnswer::Bool(true)),
            (ReadKind::Cfg, "feature = \"a\\\"b\"".to_string(), ReadAnswer::Bool(false)),
        ]
    );
    assert!(reads.iter().all(|(_, subject, _)| !subject.contains("any")), "the predicate's text is no subject");
    let lone = call_fn(&builtin(BuiltinFn::Cfg), "unix", &cx);
    assert_eq!(lone.reads.list().len(), 1);
    // A predicate that is none is the builtin's own error, never a host's.
    let bad = call_fn(&builtin(BuiltinFn::Cfg), "not(unix, windows)", &cx);
    assert!(matches!(bad.diagnostics[..], [Diagnostic::Builtin { .. }]));
}

#[test]
fn env_records_its_read() {
    let cx = TestCx::new();
    let exp = call_fn(&builtin(BuiltinFn::Env), "\"V\"", &cx);
    assert_eq!(show(&exp.tokens), "\"v\"");
    let read = exp.reads.list()[0];
    assert_eq!(read.kind, ReadKind::Env);
    assert_eq!(exp.reads.subject(&read), "V");
    assert!(matches!(read.answer, ReadAnswer::Digest(_)));

    let missing = call_fn(&builtin(BuiltinFn::Env), "\"NOPE\"", &cx);
    assert!(missing.tokens.is_empty());
    assert!(matches!(missing.diagnostics[..], [Diagnostic::EnvNotDefined { .. }]));
    assert_eq!(missing.reads.list()[0].answer, ReadAnswer::Absent);

    let option = call_fn(&builtin(BuiltinFn::OptionEnv), "\"V\"", &cx);
    assert_eq!(
        show(&option.tokens),
        "$crate :: option :: Option :: Some :: < & ' static str > ( \"v\" )"
    );
}

#[test]
fn include_str_records_its_late_read() {
    let cx = TestCx::new();
    let exp = call_fn(&builtin(BuiltinFn::IncludeStr), "\"a.txt\"", &cx);
    assert_eq!(show(&exp.tokens), "\"hi\"");
    let read = exp.reads.list()[0];
    assert_eq!(read.kind, ReadKind::IncludeStr);
    assert_eq!(exp.reads.subject(&read), "a.txt");
    assert_eq!(
        read.answer,
        ReadAnswer::Digest(ContentId::of("blaze v2 expand included file 1", b"hi"))
    );
}

#[test]
fn include_lexes_its_file_at_the_call_site() {
    let cx = TestCx::new();
    let exp = call_fn(&builtin(BuiltinFn::Include), "\"m.rs\"", &cx);
    // The byte order mark is not a token.
    assert_eq!(show(&exp.tokens), "fn f ( ) -> u8 { 1 }");
    assert!(
        exp.origins.iter().all(|it| *it == TokenOrigin::Made(blaze_expand::Transparency::Transparent))
    );
    let read = exp.reads.list()[0];
    assert_eq!((read.kind, exp.reads.subject(&read)), (ReadKind::IncludeStr, "m.rs"));
    let bad = call_fn(&builtin(BuiltinFn::Include), "\"bad.rs\"", &cx);
    assert!(matches!(bad.diagnostics[..], [Diagnostic::Include { .. }]), "{:?}", bad.diagnostics);
    assert!(bad.tokens.is_empty());
}

#[test]
fn assert_panics_with_its_condition_or_its_message() {
    let cx = TestCx::new();
    let exp = call_fn(&builtin(BuiltinFn::Assert), "a == b", &cx);
    assert_eq!(
        show(&exp.tokens),
        "if ! ( a == b ) { $crate :: panicking :: panic ( \"assertion failed: a == b\" ) }"
    );
    let exp = call_fn(&builtin(BuiltinFn::Assert), "x, \"m {}\", 1", &cx);
    assert_eq!(
        show(&exp.tokens),
        "if ! ( x ) { $crate :: panic :: panic_2021 ! ( \"m {}\" , 1 ) }"
    );
    let exp = call_fn(&builtin(BuiltinFn::Assert), "", &cx);
    assert!(matches!(exp.diagnostics[..], [Diagnostic::Builtin { .. }]));
}

#[test]
fn a_text_that_is_one_group_keeps_its_delimiters() {
    assert_eq!(show(&tokens("[[1]]")), "[ [ 1 ] ]");
    assert_eq!(show(&tokens("(a)")), "( a )");
    assert_eq!(tokens("{ x }").len(), 3);
}

#[test]
fn concat_bytes_joins_bytes_and_arrays() {
    let cx = TestCx::new();
    let exp = call_fn(&builtin(BuiltinFn::ConcatBytes), "b\"ab\", b'c', [1, 2u8], [b'z'; 2]", &cx);
    assert!(exp.diagnostics.is_empty(), "{:?}", exp.diagnostics);
    assert_eq!(show(&exp.tokens), "b\"abc\\x01\\x02zz\"");
    for refused in ["\"s\"", "1", "[256]", "[[1]]", "'c'"] {
        let exp = call_fn(&builtin(BuiltinFn::ConcatBytes), refused, &cx);
        assert!(matches!(exp.diagnostics[..], [Diagnostic::Builtin { .. }]), "{refused}");
    }
}

/// An eager builtin's arguments are the parser's expressions (rustc's `get_exprs_from_tts`),
/// not the pieces between commas outside groups.
#[test]
fn a_builtin_s_arguments_are_the_parser_s_expressions() {
    let cx = TestCx::new();
    let env = builtin(BuiltinFn::Env);
    // `f::<A, B>()` is one argument: the message, refused as no string literal where it starts
    // (token 2). CONTROL: the comma walk read three arguments, "`env!` takes 1 or 2 arguments",
    // at no token.
    let exp = call_fn(&env, "\"A\", f::<A, B>()", &cx);
    assert!(matches!(exp.diagnostics[..], [Diagnostic::Builtin { at: Some(TokenOrigin::Input(2)), .. }]), "{:?}", exp.diagnostics);
    // The twin: three arguments are three, refused by their count, at no token.
    let exp = call_fn(&env, "\"A\", \"m\", \"x\"", &cx);
    assert!(matches!(exp.diagnostics[..], [Diagnostic::Builtin { at: None, .. }]), "{:?}", exp.diagnostics);
    // Two expressions with no `,`: refused where the second starts (token 1). CONTROL: the
    // comma walk read one argument `"a" "b"`, "expected a literal" at token 0.
    let exp = call_fn(&builtin(BuiltinFn::Concat), "\"a\" \"b\"", &cx);
    assert!(matches!(exp.diagnostics[..], [Diagnostic::Builtin { at: Some(TokenOrigin::Input(1)), .. }]), "{:?}", exp.diagnostics);
    // A trailing comma ends the list, as before.
    let exp = call_fn(&builtin(BuiltinFn::Concat), "\"a\", 1,", &cx);
    assert!(exp.diagnostics.is_empty(), "{:?}", exp.diagnostics);
    assert_eq!(show(&exp.tokens), "\"a1\"");
}

#[test]
fn cfg_select_takes_the_first_arm_that_holds() {
    let cx = TestCx::new();
    let def = builtin(BuiltinFn::CfgSelect);
    let exp = call_fn(&def, "windows => { a } unix => { b } _ => { c }", &cx);
    assert_eq!(show(&exp.tokens), "b");
    // The predicates up to the one that holds are read, no further.
    let answers: Vec<_> = exp.reads.list().iter().map(|it| it.answer).collect();
    assert_eq!(answers, [ReadAnswer::Bool(false), ReadAnswer::Bool(true)]);
    let exp = call_fn(&def, "windows => { a } _ => { c }", &cx);
    assert_eq!(show(&exp.tokens), "c");
    let exp = call_fn(&def, "windows => { a }", &cx);
    assert!(matches!(exp.diagnostics[..], [Diagnostic::Builtin { .. }]));
    // An arm may be an expression up to its `,` (the pinned std writes `target_os = "x" => 512,`).
    let exp = call_fn(&def, "windows => f(a, b), unix => 8 * g(1, 2), _ => c", &cx);
    assert_eq!(show(&exp.tokens), "8 * g ( 1 , 2 )");
    let exp = call_fn(&def, "unix =>", &cx);
    assert!(matches!(exp.diagnostics[..], [Diagnostic::Builtin { .. }]));
}

/// A `cfg_select!` arm's expression is what the parser reads as one (rustc_parse
/// cfg_select.rs `parse_cfg_select_branch_rhs`), so a comma in a turbofish ends no arm. The
/// site: core's slice/sort/unstable/quicksort.rs `inst_partition`, `feature =
/// "optimize_for_size" => partition_lomuto_branchless_simple::<T, F>, _ =>
/// partition_lomuto_branchless_cyclic::<T, F>,` (t1800, 13 tests NOT RUN: "no cfg predicate:
/// it stops at token 1", the second arm's predicate read as `F > , _`).
#[test]
fn a_cfg_select_arm_s_turbofish_comma_ends_no_arm() {
    let cx = TestCx::new();
    let def = builtin(BuiltinFn::CfgSelect);
    // The site's shape: the first arm does not hold, the wildcard's turbofish is taken whole.
    let exp = call_fn(&def, "windows => simple::<T, F>, _ => cyclic::<T, F>,", &cx);
    assert!(exp.diagnostics.is_empty(), "{:?}", exp.diagnostics);
    // `::<` shows glued: the lexer makes a `:` before `<` joint, as the assert! tests above
    // render `f ::< A , B >`; the arm's tokens are the input's, copied as lexed.
    assert_eq!(show(&exp.tokens), "cyclic ::< T , F >");
    // The twin: the first arm holds, its turbofish taken whole and the rest skipped.
    let exp = call_fn(&def, "unix => simple::<T, F>, _ => cyclic::<T, F>,", &cx);
    assert!(exp.diagnostics.is_empty(), "{:?}", exp.diagnostics);
    assert_eq!(show(&exp.tokens), "simple ::< T , F >");
    // A block-like arm needs no `,` before the next arm (`classify::expr_is_complete`).
    let exp = call_fn(&def, "windows => if a { b } else { c } _ => d", &cx);
    assert_eq!(show(&exp.tokens), "d");
    // The control: the same arms with no comma inside a turbofish read as before.
    let exp = call_fn(&def, "windows => simple::<T>, _ => cyclic::<T>,", &cx);
    assert_eq!(show(&exp.tokens), "cyclic ::< T >");
}

#[test]
fn line_is_a_hole_or_a_read_in_eager_input() {
    let cx = TestCx::new();
    let exp = call_fn(&builtin(BuiltinFn::Line), "", &cx);
    assert_eq!(show(&exp.tokens), "0u32");
    assert_eq!(exp.holes[..], [Hole { token: 0, of: HoleOf::Line }]);
    assert!(exp.reads.list().is_empty());

    let input = tokens("");
    let call = Call {
        kind: CallKind::FnLike { args: &input },
        site: site(),
        eager: Eager { inside: Inside::EagerInput, answers: &[] },
    };
    let exp = done(expand(&builtin(BuiltinFn::Line), &call, &cx, blaze_own::testing::heap().alloc()));
    assert_eq!(show(&exp.tokens), "12u32");
    assert!(exp.holes.is_empty());
    assert_eq!(exp.reads.list()[0].answer, ReadAnswer::Number(12));
}

#[test]
fn panic_picks_its_edition() {
    let cx = TestCx::new();
    let exp = call_fn(&builtin(BuiltinFn::Panic), "\"no\"", &cx);
    assert_eq!(show(&exp.tokens), "$crate :: panic :: panic_2021 ! ( \"no\" )");
    assert_eq!(exp.nested.len(), 1);
}

#[test]
fn format_args_is_the_builtin_form() {
    let cx = TestCx::new();
    let exp = call_fn(&builtin(BuiltinFn::FormatArgsNl), "\"{}\", x", &cx);
    assert_eq!(show(&exp.tokens), "builtin # format_args ( \"{}\\n\" , x )");
}

fn derive(which: BuiltinDerive, item: &str) -> Expansion<'static> {
    derive_with(which, item, DeriveFacts::PLAIN)
}

/// Item position in the Reference's order: a `cfg_attr` applied where it stands, a derive passed
/// over, each other attribute offered in turn as a possible attribute macro, its call the item
/// without it and without the derives written before it; then the derives in order on the item
/// as a derive sees it. The Reference, "Attributes": "active attributes remove themselves from
/// the form they are on while inert attributes stay on"; "Derive macro helper attributes": a
/// helper is in scope "for attributes that are applied to that item and are applied lexically
/// after the derive macro invocation", and "These attributes are inert". A derive written before
/// an attribute macro is expanded first (rustc_expand `take_first_attr`), so it is not in the
/// macro's input (else a macro that re-emits its item runs it twice); a helper before it stays.
/// Expected: `test an_items_attributes_are_walked_in_order ... ok`.
/// D104: the implicit call's item is the whole item, its `#[cfg_attr(unix, inline)]` applied
/// to `#[inline]` and its `#[must_use]` kept; a planted walk that cut the first attribute (as
/// `attr_call` does for a written one) loses `inline` and fails.
#[test]
fn implicit_call_takes_the_whole_item_with_cfg_attr_applied() {
    let mut unix = |name: &str, _: Option<&str>| Ok::<bool, ()>(name == "unix");
    let made = blaze_expand::ItemAttrs::new(tokens("#[cfg_attr(unix, inline)] #[must_use] fn main(cx: &Cx, out: Out) {}"))
        .implicit_call(&mut unix)
        .expect("the attributes read");
    assert!(made.args.is_none(), "no arguments");
    assert_eq!(made.derives_taken, 0);
    assert_eq!(show(&made.item), "# [ inline ] # [ must_use ] fn main ( cx : & Cx , out : Out ) { }");
}

#[test]
fn an_items_attributes_are_walked_in_order() {
    let mut unix = |name: &str, _: Option<&str>| Ok::<bool, ()>(name == "unix");
    let mut walk = blaze_expand::ItemAttrs::new(tokens(
        "#[inline] #[derive(Debug)] #[cfg_attr(unix, m::at(x), repr(C))] #[cfg(unix)] fn f() {}",
    ));
    let next = |walk: &mut blaze_expand::ItemAttrs<'static>, atom: &mut dyn FnMut(&str, Option<&str>) -> Result<bool, ()>| {
        match walk.next(atom).expect("the attributes read") {
            blaze_expand::NextAttr::Call { path, attr } => Some((String::from(path.as_str()), attr)),
            blaze_expand::NextAttr::End => None,
            blaze_expand::NextAttr::Removed => panic!("the item's cfg holds"),
        }
    };
    assert_eq!(next(&mut walk, &mut unix).map(|(p, _)| p), Some("inline".into()));
    let (path, attr) = next(&mut walk, &mut unix).expect("the cfg_attr leaves `m::at(x)`");
    assert_eq!(path, "m::at");
    let call = walk.attr_call(attr).expect("the call is made");
    assert_eq!(show(call.args.as_ref().expect("it has arguments")), "x");
    // The derive written before it ran first and is cut; the attributes after it as written:
    // the walk has not reached them.
    assert_eq!(show(&call.item), "# [ inline ] # [ repr ( C ) ] # [ cfg ( unix ) ] fn f ( ) { }");
    assert_eq!(call.derives_taken, 1);
    assert_eq!(next(&mut walk, &mut unix).map(|(p, _)| p), Some("repr".into()));
    assert_eq!(next(&mut walk, &mut unix), None);

    // A helper written after a derive and before the macro stays in the macro's input; the
    // derive does not.
    let mut helper = blaze_expand::ItemAttrs::new(tokens("#[derive(Clone)] #[serde(x)] #[m] struct S;"));
    assert_eq!(next(&mut helper, &mut unix).map(|(p, _)| p), Some("serde".into()));
    let (path, attr) = next(&mut helper, &mut unix).expect("`m` is offered");
    assert_eq!(path, "m");
    let call = helper.attr_call(attr).expect("the call is made");
    assert!(call.args.is_none());
    assert_eq!(show(&call.item), "# [ serde ( x ) ] struct S ;");
    assert_eq!(call.derives_taken, 1);

    let mut windows = |name: &str, _: Option<&str>| Ok::<bool, ()>(name == "windows");
    let gone = blaze_expand::ItemAttrs::new(tokens("#[cfg(unix)] struct S;")).derives(&mut windows);
    assert!(matches!(gone, Ok(None)));

    let derives = blaze_expand::ItemAttrs::new(tokens(
        "#[cfg_attr(unix, derive(Clone))] #[derive(core::marker::Copy)] #[repr(C)] struct S { #[cfg(windows)] a: u8, #[cfg_attr(unix, serde(skip))] b: u8 }",
    ))
    .derives(&mut unix)
    .expect("the attributes read")
    .expect("the item stays");
    let paths: std::vec::Vec<&str> = derives.uses.iter().map(|u| u.path.as_str()).collect();
    assert_eq!(paths, ["Clone", "core::marker::Copy"]);
    assert_eq!(derives.facts(&derives.uses[0]).copy, AlsoCopy::Yes);
    assert_eq!(show(&derives.input), "# [ repr ( C ) ] struct S { # [ serde ( skip ) ] b : u8 }");
    // An atom the cfg set cannot answer is an error, never a guess.
    let mut unknown = |_: &str, _: Option<&str>| Err::<bool, ()>(());
    assert!(blaze_expand::ItemAttrs::new(tokens("#[cfg_attr(unix, derive(Clone))] struct S;")).derives(&mut unknown).is_err());
}

/// konst 0.2.20's `__declare_string_cmp_fns!`, reduced: its matcher holds a `///` doc comment
/// between two fragments, and the call (`string.rs:15`) writes none. The Reference, "Comments":
/// "Doc comments are interpreted as a special syntax for attributes"; "Macros by example" says
/// nothing of a doc comment in a matcher. BLAZE'S RULE (rules/mod.rs `doc_attr_end`): a
/// `#[doc = ".."]` in a matcher matches nothing; a doc comment in the call is still tokens.
#[test]
fn a_doc_comment_in_a_matcher_matches_nothing() {
    let cx = TestCx::new();
    let def = rules(
        "(eq = $eq:ident, ord = $ord:ident, /// Equivalent to ord, but returns a U8Ordering
          inner = $inner:ident,) => { fn $eq() {} fn $inner() {} }",
        1,
    );
    let exp = call_fn(&def, "eq = eq_str, ord = cmp_str, inner = cmp_str_inner,", &cx);
    assert_eq!(show(&exp.tokens), "fn eq_str ( ) { } fn cmp_str_inner ( ) { }", "{:?}", exp.diagnostics);
}

/// icu_collections 2.4's `cpinvlist.rs:27`, reduced: an attribute macro first, a `cfg_attr`
/// leaving its helper later. The Reference, "Conditional compilation": "When the configuration
/// predicate is true, this attribute expands out to the attributes listed after the predicate";
/// "Attributes": `cfg` and `cfg_attr` are active, "During attribute processing, active
/// attributes remove themselves from the thing they are on"; "Procedural Macros": an attribute
/// macro's second stream is "the rest of the item including other attributes on the item". So
/// the macro sees `#[zerovec::skip_derive(..)]`, never the `cfg_attr` that left it, and can
/// consume it as its own helper.
#[test]
fn an_attribute_macro_sees_the_attributes_a_later_cfg_attr_leaves() {
    let mut no_alloc = |name: &str, value: Option<&str>| Ok::<bool, ()>(!(name == "feature" && value == Some("alloc")));
    let mut walk = blaze_expand::ItemAttrs::new(tokens(
        "#[zerovec::make_varule(ListULE)] #[zerovec::skip_derive(Ord)] #[derive(Debug)] \
         #[cfg_attr(not(feature = \"alloc\"), zerovec::skip_derive(ZeroMapKV, ToOwned))] pub struct List { size: u32 }",
    ));
    let attr = match walk.next(&mut no_alloc).expect("the attributes read") {
        blaze_expand::NextAttr::Call { path, attr } if path.as_str() == "zerovec::make_varule" => attr,
        _ => panic!("the first attribute is the macro's call"),
    };
    let call = walk.attr_call(attr).expect("the call is made");
    assert_eq!(
        show(&call.item),
        "# [ zerovec :: skip_derive ( Ord ) ] # [ derive ( Debug ) ] # [ zerovec :: skip_derive ( ZeroMapKV , ToOwned ) ] pub struct List { size : u32 }"
    );
}

#[test]
fn a_derive_attribute_names_its_paths() {
    let names = |attr: &str| {
        let t = tokens(attr);
        blaze_expand::derive_attr(&t, blaze_own::testing::heap().alloc()).map(|(constness, paths)| {
            let named: std::vec::Vec<String> = paths
                .iter()
                .map(|r| (r.start..r.end).map(|at| t.text_at(at as usize)).collect::<std::vec::Vec<_>>().concat())
                .collect();
            (constness, named)
        })
    };
    assert_eq!(names("#[derive(Clone, Copy,)]"), Some((Constness::NotConst, vec!["Clone".into(), "Copy".into()])));
    // A punctuation token's text is empty: the path's words, `::` dropped.
    assert_eq!(names("#[derive_const(::core::clone::Clone)]"), Some((Constness::Const, vec!["corecloneClone".into()])));
    assert_eq!(names("#[derive()]"), Some((Constness::NotConst, vec![])));
    assert_eq!(names("#[repr(C)]"), None);
    assert_eq!(names("#[derive(Clone = 1)]"), None);
}

/// `#[cfg_attr(pred, a, b)]` leaves `a` and `b` when `pred` holds and nothing otherwise; a
/// nested `cfg_attr` is applied in its place, in order; a `derive(..)` it leaves reads as one.
#[test]
fn a_cfg_attr_leaves_its_attributes_when_its_predicate_holds() {
    let metas = |attr: &str| {
        let t = tokens(attr);
        let got = blaze_expand::cfg_attr_metas(&t, blaze_own::testing::heap().alloc(), &mut |name, value| -> Result<bool, ()> {
            Ok(matches!((name, value), ("unix", None) | ("feature", Some("serde"))))
        });
        got.map(|ranges| {
            ranges
                .iter()
                .map(|r| (r.start..r.end).map(|at| t.text_at(at as usize)).collect::<std::vec::Vec<_>>().concat())
                .collect::<std::vec::Vec<String>>()
        })
    };
    // (A punctuation or delimiter token's text is empty: the meta's words.)
    assert_eq!(metas("#[cfg_attr(unix, derive(Clone, Copy), inline)]"), Ok(vec!["deriveCloneCopy".into(), "inline".into()]));
    assert_eq!(metas("#[cfg_attr(windows, derive(Clone))]"), Ok(vec![]));
    assert_eq!(
        metas("#[cfg_attr(all(), a, cfg_attr(feature = \"serde\", b, cfg_attr(windows, c)), d,)]"),
        Ok(vec!["a".into(), "b".into(), "d".into()])
    );
    assert_eq!(metas("#[cfg_attr(, a)]"), Err(blaze_expand::CfgAttrError::NoPredicate { token: 2 }));
    assert_eq!(metas("#[derive(Clone)]"), Err(blaze_expand::CfgAttrError::NotCfgAttr));
    // The derive it leaves, read in place.
    let t = tokens("#[cfg_attr(unix, derive(Debug, ::core::hash::Hash))]");
    let ranges = blaze_expand::cfg_attr_metas(&t, blaze_own::testing::heap().alloc(), &mut |_, _| -> Result<bool, ()> { Ok(true) })
        .expect("the predicate holds");
    let (constness, paths) = blaze_expand::derive_meta(&t, ranges[0].clone(), blaze_own::testing::heap().alloc()).expect("a derive");
    assert_eq!(constness, Constness::NotConst);
    assert_eq!(paths.len(), 2);
}

/// A built-in attribute may be named by its path in a standard prelude (the Reference,
/// Attributes, "Built-in attributes" live in the prelude): libc 0.2.191's `s!` writes
/// `#[::core::prelude::v1::derive(..)]`, which is `derive`; a path into no prelude is not.
#[test]
fn derive_named_by_its_prelude_path_is_a_derive() {
    let derives = |text: &str| {
        let t = tokens(text);
        blaze_expand::derive_meta(&t, 2..(t.tokens().len() - 1) as u32, blaze_own::testing::heap().alloc()).map(|(c, paths)| (c, paths.len()))
    };
    assert_eq!(derives("#[::core::prelude::v1::derive(Clone)]"), Some((Constness::NotConst, 1)));
    assert_eq!(derives("#[std::prelude::rust_2021::derive(Clone, ::core::cmp::PartialEq)]"), Some((Constness::NotConst, 2)));
    assert_eq!(derives("#[derive(Clone)]"), Some((Constness::NotConst, 1)));
    assert_eq!(derives("#[serde::prelude::v1::derive(Clone)]"), None);
    assert_eq!(derives("#[core::derive(Clone)]"), None);
}

fn derive_with(which: BuiltinDerive, item: &str, facts: DeriveFacts) -> Expansion<'static> {
    let cx = TestCx::new();
    let def = MacroDefinition::builtin_derive(which, at(1), blaze_own::testing::heap().alloc());
    let item = tokens(item);
    let call =
        Call { kind: CallKind::Derive { item: &item, facts }, site: site(), eager: Eager::plain() };
    done(expand(&def, &call, &cx, blaze_own::testing::heap().alloc()))
}

/// `#[global_allocator]` keeps the static as written and writes beside it the four symbols of
/// the pinned alloc's `unsafe extern "Rust"` block, with its signatures exactly (`Alignment`,
/// `NonNull<u8>`), each calling the static's `GlobalAlloc` method; the static's name is the
/// item's own token. An item that is no static is refused with rustc's words and nothing else.
///
/// Prints: `test global_allocator_keeps_the_static_and_writes_the_four_symbols ... ok`
#[test]
fn global_allocator_keeps_the_static_and_writes_the_four_symbols() {
    let cx = TestCx::new();
    let def = MacroDefinition::builtin_attr(BuiltinAttr::GlobalAllocator, at(1), blaze_own::testing::heap().alloc());
    let item = tokens("static A: T = T;");
    let exp = done(expand(&def, &Call::attr(None, &item, site()), &cx, blaze_own::testing::heap().alloc()));
    assert!(exp.diagnostics.is_empty());
    let text = show(&exp.tokens);
    let layout = "$crate :: alloc :: Layout :: from_size_alignment_unchecked";
    let align = "align : $crate :: mem :: Alignment";
    let ptr = "ptr : $crate :: ptr :: NonNull < u8 >";
    let want = [
        "static A : T = T ;".to_owned(),
        format!(
            "# [ rustc_std_internal_symbol ] unsafe fn __rust_alloc ( size : usize , {align} ) -> * mut u8 \
             {{ unsafe {{ $crate :: alloc :: GlobalAlloc :: alloc ( & A , {layout} ( size , align ) ) }} }}"
        ),
        format!(
            "# [ rustc_std_internal_symbol ] unsafe fn __rust_dealloc ( {ptr} , size : usize , {align} ) \
             {{ unsafe {{ $crate :: alloc :: GlobalAlloc :: dealloc ( & A , $crate :: ptr :: NonNull :: as_ptr ( ptr ) , \
             {layout} ( size , align ) ) }} }}"
        ),
        format!(
            "# [ rustc_std_internal_symbol ] unsafe fn __rust_realloc ( {ptr} , old_size : usize , {align} , new_size : usize ) \
             -> * mut u8 {{ unsafe {{ $crate :: alloc :: GlobalAlloc :: realloc ( & A , $crate :: ptr :: NonNull :: as_ptr ( ptr ) , \
             {layout} ( old_size , align ) , new_size ) }} }}"
        ),
        format!(
            "# [ rustc_std_internal_symbol ] unsafe fn __rust_alloc_zeroed ( size : usize , {align} ) -> * mut u8 \
             {{ unsafe {{ $crate :: alloc :: GlobalAlloc :: alloc_zeroed ( & A , {layout} ( size , align ) ) }} }}"
        ),
    ]
    .join(" ");
    assert_eq!(text, want);
    assert_eq!(text.matches("unsafe fn").count(), 4, "{text}");
    // The static's name in each body is the item's own token, not one the expansion made.
    let a_inputs = exp
        .tokens
        .tokens()
        .iter()
        .zip(exp.origins.iter())
        .filter(|(token, origin)| exp.tokens.text_of(token) == "A" && matches!(origin, TokenOrigin::Input(_)))
        .count();
    assert_eq!(a_inputs, 5, "the static's name and the four `&A`: {text}");
    // A fn is no allocator.
    let item = tokens("fn f() {}");
    let exp = done(expand(&def, &Call::attr(None, &item, site()), &cx, blaze_own::testing::heap().alloc()));
    assert!(exp.tokens.tokens().is_empty());
    assert_eq!(exp.diagnostics.len(), 1);
    assert!(matches!(exp.diagnostics[0], Diagnostic::Builtin { .. }));
}

#[test]
fn clone_of_a_copy_type_without_type_parameters_is_star_self() {
    let copy = DeriveFacts { copy: AlsoCopy::Yes, constness: Constness::NotConst };
    let exp = derive_with(BuiltinDerive::Clone, "struct S { a: u8, b: (u8, u16) }", copy);
    let text = show(&exp.tokens);
    assert!(text.contains("$crate :: clone :: AssertParamIsClone < u8"), "{text}");
    assert!(text.contains("AssertParamIsClone < ( u8 , u16 )"), "{text}");
    assert!(text.contains("* self } }"), "{text}");
    assert!(!text.contains("Clone :: clone ("), "{text}");
    // A type parameter keeps the field-by-field clone, as rustc's.
    let exp = derive_with(BuiltinDerive::Clone, "struct G<T> { a: T }", copy);
    let text = show(&exp.tokens);
    assert!(text.contains("$crate :: clone :: Clone :: clone ( a_self )"), "{text}");
    // The facts are in the key.
    let item = tokens("struct S { a: u8 }");
    let def = MacroDefinition::builtin_derive(BuiltinDerive::Clone, at(1), blaze_own::testing::heap().alloc());
    let call = |facts| Call {
        kind: CallKind::Derive { item: &item, facts },
        site: site(),
        eager: Eager::plain(),
    };
    assert_ne!(call_key(&def, &call(copy)), call_key(&def, &call(DeriveFacts::PLAIN)));
}

#[test]
fn derive_const_writes_a_const_impl_with_const_bounds() {
    let konst = DeriveFacts { copy: AlsoCopy::No, constness: Constness::Const };
    let exp = derive_with(BuiltinDerive::PartialEq, "struct S<T> { a: T }", konst);
    let text = show(&exp.tokens);
    let header = "impl < T : [ const ] $crate :: cmp :: PartialEq , > \
                  const $crate :: cmp :: PartialEq for S < T , >";
    assert!(text.contains(header), "{text}");
    // `StructuralPartialEq` is no const trait.
    assert!(text.contains("impl < T , > $crate :: marker :: StructuralPartialEq for S"), "{text}");
}

#[test]
fn const_param_ty_derives_a_plain_marker_impl() {
    // As `core::marker` writes `#[derive(ConstParamTy)]`, and never const under `derive_const`.
    for facts in [DeriveFacts::PLAIN, DeriveFacts { copy: AlsoCopy::No, constness: Constness::Const }]
    {
        let exp = derive_with(BuiltinDerive::ConstParamTy, "enum E<T> { A(T), B }", facts);
        assert!(exp.diagnostics.is_empty(), "{:?}", exp.diagnostics);
        let text = show(&exp.tokens);
        let header = "impl < T : $crate :: marker :: ConstParamTy_ , > \
                      $crate :: marker :: ConstParamTy_ for E < T , > { }";
        assert!(text.contains(header), "{text}");
    }
    assert_eq!(BuiltinDerive::ConstParamTy.name(), "ConstParamTy");
}

#[test]
fn debug_and_clone_derive_their_impls() {
    let debug = derive(BuiltinDerive::Debug, "struct S { a: u32 }");
    let text = show(&debug.tokens);
    assert!(debug.diagnostics.is_empty(), "{:?}", debug.diagnostics);
    assert!(text.contains("# [ automatically_derived ] impl"), "{text}");
    assert!(text.contains("Debug") && text.contains("fmt"), "{text}");
    let clone = derive(BuiltinDerive::Clone, "struct S { a: u32 }");
    let text = show(&clone.tokens);
    assert!(text.contains("# [ automatically_derived ] impl"), "{text}");
    assert!(text.contains("Clone") && text.contains("clone"), "{text}");
    // The item's name is the item's token.
    let name = clone.tokens.tokens().iter().position(|it| clone.tokens.text_of(it) == "S");
    assert_eq!(name.map(|at| clone.origins[at]), Some(TokenOrigin::Input(1)));
}

#[test]
fn partial_eq_bounds_parameters_predicates_and_assoc_types() {
    let item = "enum E<'a, T: Copy, const N: usize> where T: Sized \
                { A(&'a T, [u8; N]), B { x: T::Out }, C }";
    let exp = derive(BuiltinDerive::PartialEq, item);
    assert!(exp.diagnostics.is_empty(), "{:?}", exp.diagnostics);
    let text = show(&exp.tokens);
    let header = "impl < ' a , T : $crate :: cmp :: PartialEq , const N : usize , > \
                  $crate :: cmp :: PartialEq for E < ' a , T , N , > \
                  where T : Sized , T : Copy , T :: Out : $crate :: cmp :: PartialEq , {";
    assert!(text.contains(header), "{text}");
    assert!(text.contains("$crate :: marker :: StructuralPartialEq for E"), "{text}");
    assert!(text.contains("__self_discr == __arg1_discr && match ( self , other ) {"), "{text}");
    assert!(text.contains("E :: B { x : x_self , }"), "{text}");
    assert!(text.contains("_unused => true }"), "{text}");
    // `T::Out` is the item's tokens, not made ones.
    let out = exp.tokens.tokens().iter().position(|it| exp.tokens.text_of(it) == "Out");
    assert!(matches!(out.map(|at| exp.origins[at]), Some(TokenOrigin::Input(_))));
}

#[test]
fn default_takes_the_marked_variant_and_refuses_without_one() {
    let exp = derive(BuiltinDerive::Default, "enum E { A, #[default] B }");
    assert!(
        show(&exp.tokens).contains("fn default ( ) -> Self { E :: B } }"),
        "{}",
        show(&exp.tokens)
    );
    let refused = derive(BuiltinDerive::Default, "enum E { A }");
    assert!(
        matches!(refused.diagnostics[..], [Diagnostic::Derive { .. }]),
        "{:?}",
        refused.diagnostics
    );
    assert!(refused.tokens.is_empty());
}

/// RFC 3681: `#[derive(Default)]` takes a field's declared default (as rustc's derive): the
/// other fields are `Default::default()` and `..` with no base fills the declared ones. A
/// struct with no declared default is unchanged. Expected: `test
/// default_takes_the_declared_field_defaults ... ok`.
#[test]
fn default_takes_the_declared_field_defaults() {
    let exp = derive(BuiltinDerive::Default, "struct O { a: u8, b: u8 = 40, c: Vec<u8> = Vec::new() }");
    let text = show(&exp.tokens);
    assert!(text.contains("fn default ( ) -> Self { O { a : $crate :: default :: Default :: default ( ) , .. } } }"), "{text}");
    let variant = derive(BuiltinDerive::Default, "enum E { A, #[default] R { w: u32 = 1, h: u32 } }");
    let text = show(&variant.tokens);
    assert!(text.contains("E :: R { h : $crate :: default :: Default :: default ( ) , .. }"), "{text}");
    let plain = derive(BuiltinDerive::Default, "struct P { a: u8 }");
    let text = show(&plain.tokens);
    assert!(text.contains("P { a : $crate :: default :: Default :: default ( ) , }"), "{text}");
}

#[test]
fn a_packed_struct_copies_its_fields_out() {
    let exp = derive(BuiltinDerive::Clone, "#[repr(packed)] struct P<T> { a: T, b: u8 }");
    let text = show(&exp.tokens);
    assert!(text.contains("where T : $crate :: marker :: Copy ,"), "{text}");
    assert!(text.contains("let a_self = { self . a } ;"), "{text}");
    assert!(text.contains("$crate :: clone :: Clone :: clone ( & a_self )"), "{text}");
}

#[test]
fn concat_reads_escapes() {
    let cx = TestCx::new();
    let exp = call_fn(&builtin(BuiltinFn::Concat), "\"a\\u{4_1}\\x42\\n\", '\\''", &cx);
    assert_eq!(show(&exp.tokens), "\"aAB\\n'\"");
    // `"\x80"` is no Rust text (rustc's lexer refuses an escape past `\x7f` in a `str`), so
    // `lex` refuses it too; what `concat!` itself refuses is a byte string.
    assert!(lex("\"\\x80\"", Edition::E2021, blaze_own::testing::heap().alloc()).is_err());
    let bad = call_fn(&builtin(BuiltinFn::Concat), "b\"x\"", &cx);
    assert!(matches!(bad.diagnostics[..], [Diagnostic::Builtin { .. }]), "{:?}", bad.diagnostics);
}

/// A cfg predicate's outcome from its tokens, each atom asked once in order; malformed ones
/// are refused at their token.
#[test]
fn a_cfg_predicate_is_read_from_its_tokens() {
    let set = |name: &str, value: Option<&str>| -> Result<bool, ()> {
        Ok(match (name, value) {
            ("unix" | "test", None) => true,
            ("target_os", Some(os)) => os == "macos",
            ("feature", Some(f)) => f == "on",
            ("unknown", None) => return Err(()),
            _ => false,
        })
    };
    let eval = |text: &str| {
        let tokens = lex(text, Edition::E2021, blaze_own::testing::heap().alloc()).expect("the predicate lexes");
        let mut asked = Vec::new();
        let out = cfg_predicate(&tokens, blaze_own::testing::heap().alloc(), &mut |n, v| {
            asked.push(n.to_string());
            set(n, v)
        });
        (out, asked)
    };
    let cases = [
        ("unix", true),
        ("windows", false),
        ("target_os = \"macos\"", true),
        ("all(unix, not(windows), feature = \"on\",)", true),
        ("any(windows, all(test, target_os = \"linux\"))", false),
        ("any()", false),
        ("all()", true),
        ("not(any(windows, feature = \"off\"))", true),
    ];
    for (text, want) in cases {
        assert_eq!(eval(text).0, Ok(want), "{text}");
    }
    assert_eq!(eval("all(unix, test)").1, ["unix", "test"]);
    assert!(matches!(eval("not(unix, test)").0, Err(CfgPredicateError::NotTakesOne { .. })));
    assert!(matches!(eval("not()").0, Err(CfgPredicateError::NotTakesOne { .. })));
    // (`lex` closes a group left open, so an unclosed `all(unix` is no case here.)
    assert!(matches!(eval("all(unix test)").0, Err(CfgPredicateError::Malformed { .. })));
    assert!(matches!(eval("all(unix,,)").0, Err(CfgPredicateError::Malformed { .. })));
    assert!(matches!(eval("unix test").0, Err(CfgPredicateError::Malformed { .. })));
    assert!(matches!(eval("feature = 3").0, Err(CfgPredicateError::Malformed { .. })));
    assert_eq!(eval("any(unix, unknown)").0, Err(CfgPredicateError::Atom(())));
}

/// A predicate drawn from `tape` over the four atoms of [`TRI_ATOMS`], at most `depth` groups
/// deep (`any`, `all`, `not`, each with a trailing comma sometimes).
fn drawn_predicate(tape: &Tape, depth: u32) -> String {
    if depth == 0 || !tape.bool("a group") {
        return (*tape.pick(&TRI_ATOMS, "atom").unwrap_or(&TRI_ATOMS[0])).to_owned();
    }
    match tape.below(3, "group") {
        0 => format!("not({})", drawn_predicate(tape, depth - 1)),
        op => {
            let parts: Vec<String> = (0..tape.below(4, "members")).map(|_| drawn_predicate(tape, depth - 1)).collect();
            let comma = if !parts.is_empty() && tape.bool("trailing comma") { "," } else { "" };
            format!("{}({}{comma})", if op == 1 { "any" } else { "all" }, parts.join(", "))
        }
    }
}

/// The atoms of [`drawn_predicate`], as written.
const TRI_ATOMS: [&str; 4] = ["test", "unix", "feature = \"x\"", "target_os = \"macos\""];

/// The atom of [`TRI_ATOMS`] that `name` and `value` are.
fn tri_atom(name: &str, value: Option<&str>) -> usize {
    match (name, value) {
        ("test", None) => 0,
        ("unix", None) => 1,
        ("feature", Some("x")) => 2,
        ("target_os", Some("macos")) => 3,
        other => panic!("an atom the generator does not write: {other:?}"),
    }
}

/// THE ONE EVALUATOR'S TWO FORMS AGREE (rule 30): wherever every atom is known, the Kleene
/// outcome (`cfg_predicate_tri`) is the two-valued one; and where some are not, a known outcome
/// (`Yes` or `No`) is what EVERY completion of the unknown atoms gives: plan time never says a
/// wrong `No`. Controls: a predicate whose outcome turns on its unknown atom is `Unknown`, and
/// `any(test, unix)` with `test` known true is `Yes` though `unix` is not known.
#[test]
fn a_cfg_predicate_in_three_values_agrees_with_two() {
    use blaze_expand::{Tri, cfg_predicate_tri};
    let tri_of = |text: &str, known: [Option<bool>; 4]| {
        let tokens = lex(text, Edition::E2021, blaze_own::testing::heap().alloc()).expect("the predicate lexes");
        cfg_predicate_tri(&tokens, 0..tokens.tokens().len(), blaze_own::testing::heap().alloc(), &mut |n, v| match known[tri_atom(n, v)] {
            Some(value) => Tri::of(value),
            None => Tri::Unknown,
        })
    };
    let two_of = |text: &str, set: [bool; 4]| {
        let tokens = lex(text, Edition::E2021, blaze_own::testing::heap().alloc()).expect("the predicate lexes");
        cfg_predicate(&tokens, blaze_own::testing::heap().alloc(), &mut |n, v| Ok::<bool, ()>(set[tri_atom(n, v)]))
    };
    blaze_sim::check(Config::cases(400), |tape: &Tape| {
        let text = drawn_predicate(tape, 3);
        let known: [Option<bool>; 4] = core::array::from_fn(|_| match tape.below(3, "atom value") {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        });
        let tri = tri_of(&text, known);
        let mut outcomes = [false; 2];
        for completion in 0..16u32 {
            let set: [bool; 4] = core::array::from_fn(|at| known[at].unwrap_or(completion & (1 << at) != 0));
            if (0..4).any(|at| known[at].is_some_and(|value| value != (completion & (1 << at) != 0))) {
                continue;
            }
            let two = two_of(&text, set);
            match (&tri, &two) {
                (Ok(_), Ok(b)) => outcomes[usize::from(*b)] = true,
                (Err(_), Err(_)) => return Ok(()),
                _ => return Err(Broke::Oracle(format!("`{text}`: one form refused it and the other did not: {tri:?} against {two:?}"))),
            }
        }
        let tri = tri.map_err(|why| Broke::Oracle(format!("`{text}` refused: {why:?}")))?;
        let want = match outcomes {
            [true, false] => Tri::No,
            [false, true] => Tri::Yes,
            _ => Tri::Unknown,
        };
        // Kleene is sound, not complete: `all(unix, not(unix))` is `No` in every completion and
        // `Unknown` here. So a known outcome must be every completion's, and with every atom
        // known (one completion) the two forms are equal.
        let all_known = known.iter().all(Option::is_some);
        let fine = match tri {
            Tri::Unknown => !all_known,
            known_tri => known_tri == want,
        };
        if !fine {
            return Err(Broke::Oracle(format!("`{text}` under {known:?}: three values say {tri:?}, its completions say {want:?}")));
        }
        Ok(())
    });
    // Controls.
    assert_eq!(tri_of("unix", [None, None, None, None]), Ok(Tri::Unknown), "an unknown atom's own outcome is unknown");
    assert_eq!(tri_of("any(test, unix)", [Some(true), None, None, None]), Ok(Tri::Yes), "`any` with one known `Yes` is `Yes`");
    assert_eq!(tri_of("all(test, unix)", [Some(false), None, None, None]), Ok(Tri::No), "`all` with one known `No` is `No`");
    assert_eq!(tri_of("not(test)", [Some(true), None, None, None]), Ok(Tri::No));
}

/// A CRLF file lexes as rustc reads it, each `\r\n` a `\n` (the reference's CRLF
/// normalization): a `\` before the line end continues a string (vcpkg-0.2.15 src/lib.rs:369,
/// crossterm-0.29.0), and the tokens equal the LF file's.
#[test]
fn a_crlf_file_lexes_as_its_lf_form() {
    let crlf = lex("let s = \"a \\\r\n   b\";\r\n", Edition::E2021, blaze_own::testing::heap().alloc()).expect("CRLF is normalized");
    let lf = lex("let s = \"a \\\n   b\";\n", Edition::E2021, blaze_own::testing::heap().alloc()).expect("the LF form lexes");
    assert_eq!(show(&crlf), show(&lf));
    let doc_crlf = lex("/// d\r\nfn f() {}", Edition::E2021, blaze_own::testing::heap().alloc()).expect("a CRLF doc comment lexes");
    let doc_lf = lex("/// d\nfn f() {}", Edition::E2021, blaze_own::testing::heap().alloc()).expect("the LF form lexes");
    assert_eq!(show(&doc_crlf), show(&doc_lf));
}

#[test]
fn an_attribute_call_takes_its_args_and_item() {
    let cx = TestCx::new();
    let krate = CrateItemKey(ContentId([9; 32]));
    let def =
        MacroDefinition::proc_macro(krate, ProcKind::Attr, "echo", &[], at(9), blaze_own::testing::heap().alloc()).expect("names fit");
    let args = tokens("x");
    let item = tokens("fn f() {}");
    let call = Call {
        kind: CallKind::Attr { args: Some(&args), item: &item },
        site: site(),
        eager: Eager::plain(),
    };
    let exp = done(expand(&def, &call, &cx, blaze_own::testing::heap().alloc()));
    assert_eq!(show(&exp.tokens), "fn f ( ) { }");
    assert!(
        exp.origins
            .iter()
            .enumerate()
            .all(|(at, it)| u32::try_from(at).map(TokenOrigin::Input) == Ok(*it))
    );
    assert_eq!(exp.reads.list()[0].kind, ReadKind::ProcMacro);

    let wrong = expand(&def, &Call::fn_like(&args, site()), &cx, blaze_own::testing::heap().alloc());
    assert!(matches!(
        wrong,
        Err(ExpandError::WrongCallKind { def: CallShape::Attr, call: CallShape::FnLike })
    ));
}

/// `echo!(a b)` of a fn-like proc macro whose host also gives `emits`.
fn echo(emits: &'static [Emit]) -> Result<Expanded<'static>, ExpandError> {
    let cx = TestCx { emits, ..TestCx::new() };
    let krate = CrateItemKey(ContentId([9; 32]));
    let def =
        MacroDefinition::proc_macro(krate, ProcKind::FnLike, "echo", &[], at(9), blaze_own::testing::heap().alloc()).expect("names fit");
    let input = tokens("a b");
    expand(&def, &Call::fn_like(&input, site()), &cx, blaze_own::testing::heap().alloc())
}

#[test]
fn a_proc_macro_warning_keeps_its_tokens_and_an_error_drops_them() {
    for level in [ProcLevel::Warning, ProcLevel::Note, ProcLevel::Help] {
        let emits: &'static [Emit] = match level {
            ProcLevel::Warning => &[Emit::Level(ProcLevel::Warning)],
            ProcLevel::Note => &[Emit::Level(ProcLevel::Note)],
            _ => &[Emit::Level(ProcLevel::Help)],
        };
        let exp = done(echo(emits));
        assert_eq!(show(&exp.tokens), "a b", "{level:?}");
        match exp.diagnostics[..] {
            [Diagnostic::ProcMacro { level: l, message, at: Some(TokenOrigin::Input(0)) }] => {
                assert_eq!(l, level);
                // The text was copied out of the host's table into the expansion's notes.
                assert_eq!(exp.note_text(message), "é!");
            }
            ref other => panic!("{other:?}"),
        }
    }
    let error = done(echo(&[Emit::Level(ProcLevel::Warning), Emit::Level(ProcLevel::Error)]));
    assert!(error.tokens.is_empty() && error.origins.is_empty());
    assert_eq!(error.diagnostics.len(), 2);
    let panicked = done(echo(&[Emit::Panic]));
    assert!(panicked.tokens.is_empty());
    assert!(matches!(panicked.diagnostics[..], [Diagnostic::ProcMacroPanicked { .. }]));
    // Control: with nothing emitted the macro echoes its input.
    assert_eq!(show(&done(echo(&[])).tokens), "a b");
}

#[test]
fn a_proc_macro_hosts_out_of_range_output_is_refused_by_type() {
    assert!(matches!(echo(&[Emit::TextPastEnd]), Err(ExpandError::ProcMacroText { len: 3, .. })));
    assert!(matches!(echo(&[Emit::TextMidChar]), Err(ExpandError::ProcMacroText { len: 3, .. })));
    assert!(matches!(
        echo(&[Emit::OriginPastInput]),
        Err(ExpandError::ProcMacroOrigin { origin: TokenOrigin::Input(2) })
    ));
    assert!(matches!(
        echo(&[Emit::TokenFromDef]),
        Err(ExpandError::ProcMacroOrigin { origin: TokenOrigin::Def(0) })
    ));
}

#[test]
fn no_rule_matching_is_a_typed_error_naming_the_rules() {
    let cx = TestCx::new();
    let exp = call_fn(&rules("(a) => {}; (b) => {}", 1), "c", &cx);
    assert!(exp.tokens.is_empty());
    assert!(
        matches!(exp.diagnostics[..], [Diagnostic::NoRuleMatched { rules: 2, .. }]),
        "{:?}",
        exp.diagnostics
    );
}

/// The matcher names the rule that got furthest (rule 19: the failed arm). `a b d` against
/// `(a) => {}; (a b c) => {}`: rule 1 stops at token 1 (`b` where its input should end), rule 2
/// gets further (it takes `a b`, wants `c`, finds `d` at token 2), so the
/// failure says "rule 2 of 2" (the Reference, "Macros By Example": "tries each macro rule in
/// turn ... If no rule matches, an error is reported"; which rule to blame is blaze's choice,
/// the furthest). Expected line: "test the_closest_rule_is_named ... ok".
#[test]
fn the_closest_rule_is_named() {
    let cx = TestCx::new();
    let exp = call_fn(&rules("(a) => {}; (a b c) => {}", 1), "a b d", &cx);
    assert!(matches!(exp.diagnostics[..], [Diagnostic::NoRuleMatched { rules: 2, closest: Some(1), .. }]), "{:?}", exp.diagnostics);
    let inline = blaze_own::Inline::<4096>::new();
    let erased = blaze_own::Erased::new(&inline);
    let failed = blaze_expand::Failed::of(&exp, erased.bump()).expect("no rule matched is a failure");
    let shown = std::format!("{failed}");
    assert!(shown.contains("rule 2 of 2"), "{shown}");
}

#[test]
fn nested_calls_are_listed_not_expanded() {
    let cx = TestCx::new();
    let exp = call_fn(&rules("() => { foo!(1); bar::baz![2]; }", 1), "", &cx);
    assert_eq!(show(&exp.tokens), "foo ! ( 1 ) ; bar :: baz ! [ 2 ] ;");
    assert_eq!(exp.nested.len(), 2);
    assert!(exp.nested.iter().all(|it| it.kind == NestedKind::FnLike));
    assert_eq!((exp.nested[1].path.start, exp.nested[1].path.end), (6, 10));
}

/// Expansions that between them use every column of the record: definition and call tokens
/// under marks, nested calls, holes, reads, diagnostics with and without a place, and a rule.
fn varied_expansions() -> Vec<Expansion<'static>> {
    let cx = TestCx::new();
    vec![
        call_fn(&rules("($($x:ident),*) => { $(let $x = 1;)* foo!(2); }", 1), "a, b", &cx),
        call_fn(&rules("(a) => {}; (b) => {}", 1), "c", &cx),
        call_fn(&builtin(BuiltinFn::Line), "", &cx),
        call_fn(&builtin(BuiltinFn::Env), "\"NOPE\", \"why\"", &cx),
        call_fn(&builtin(BuiltinFn::OptionEnv), "\"V\"", &cx),
        call_fn(&builtin(BuiltinFn::IncludeStr), "\"a.txt\"", &cx),
        call_fn(&builtin(BuiltinFn::Cfg), "unix", &cx),
        call_fn(&builtin(BuiltinFn::Concat), "\"r#\", 1.5, 'c'", &cx),
        call_fn(&builtin(BuiltinFn::Include), "\"missing.rs\"", &cx),
        derive(
            BuiltinDerive::PartialEq,
            "enum E<'a, T> where T: Sized { A(&'a T), B { x: T::Out } }",
        ),
        derive(BuiltinDerive::Default, "enum E { A }"),
        // Tags 11 (a proc macro's warning, beside its tokens) and 12 (its panic).
        done(echo(&[Emit::Level(ProcLevel::Warning), Emit::Level(ProcLevel::Help)])),
        done(echo(&[Emit::Panic])),
    ]
}

/// Each source token `n` as record token `n`: the maps of sources with no lifetime and no
/// invisible group. For a test of a record's bytes, which reads no origin back as a place.
static SAME: [u32; 512] = {
    let mut map = [0; 512];
    let mut n = 0;
    while n < map.len() {
        map[n] = n as u32;
        n += 1;
    }
    map
};

fn same_maps() -> blaze_expand::OriginMaps<'static> {
    blaze_expand::OriginMaps { input: Some(&SAME), attr_args: Some(&SAME), body: Some(&SAME), params: Some(&SAME) }
}

#[test]
fn an_expansion_record_reads_back_equal_and_rewrites_equal_bytes() {
    for exp in varied_expansions() {
        let bytes = exp.write_record(&same_maps(), blaze_own::testing::heap().alloc()).expect("its tokens make a token record");
        let read = Expansion::read_record(&bytes, blaze_own::testing::heap().alloc()).expect("a written record reads back");
        assert!(read == exp, "{exp:?} read back as {read:?}");
        let again = read.write_record(&same_maps(), blaze_own::testing::heap().alloc()).expect("its tokens make a token record");
        assert_eq!(again[..], bytes[..]);
        // A buffer cut short is refused, never read.
        let cut = Expansion::<'static>::read_record(&bytes[..bytes.len() - 1], blaze_own::testing::heap().alloc());
        assert!(cut.is_err());
    }
}

/// A definition's records, read back: the `MacroDef` and its streams, each aligned.
fn read_back(
    def: &MacroDefinition<'static>,
    records: &DefinitionRecords<'static>,
) -> Result<MacroDefinition<'static>, DefinitionError> {
    let head = Aligned::copy_from(&records.def);
    let view = MacroDefView::read(&head).expect("a written record");
    let body = records.body.as_ref().map(|(_, bytes)| Aligned::copy_from(bytes));
    let params = records.params.as_ref().map(|(_, bytes)| Aligned::copy_from(bytes));
    assert_eq!(view.head().content, def.content().0);
    MacroDefinition::from(&view, body.as_deref(), params.as_deref(), blaze_own::testing::heap().alloc())
}

#[test]
fn a_macro_definition_reads_back_from_its_records() {
    let krate = CrateItemKey(ContentId([9; 32]));
    let derive =
        MacroDefinition::proc_macro(krate, ProcKind::Derive, "Serialize", &["serde", "allow"], at(9), blaze_own::testing::heap().alloc())
            .expect("names fit");
    let defs = [
        rules("($x:expr) => { $x + 1 }", 1),
        MacroDefinition::macro2(Some(tokens("$x:expr")), tokens("$x * 2"), at(2)),
        builtin(BuiltinFn::Concat),
        MacroDefinition::builtin_derive(BuiltinDerive::Clone, at(1), blaze_own::testing::heap().alloc()),
        derive,
    ];
    let item = ItemKey(ContentId([5; 32]));
    for def in &defs {
        let name = def.proc_name().unwrap_or("m");
        let records = def.write(item, name, None, blaze_own::testing::heap().alloc()).expect("a small definition");
        let read = read_back(def, &records).expect("the records are one definition");
        assert_eq!(read.content(), def.content());
        assert_eq!((read.kind(), read.at(), read.transparency()), (def.kind(), def.at(), def.transparency()));
        assert!(read.helpers().eq(def.helpers()) && read.proc_name() == def.proc_name());
        // Written again, the records are the same bytes (rule 25).
        let again = read.write(item, name, None, blaze_own::testing::heap().alloc()).expect("a small definition");
        assert_eq!(again.def[..], records.def[..]);
    }
    // Control: a body record of another stream is refused by its digest, never read as the body.
    let mut records = defs[0].write(item, "m", None, blaze_own::testing::heap().alloc()).expect("a small definition");
    let other = tokens("() => {}").write_record(blaze_own::testing::heap().alloc());
    records.body = Some((stream_record_id(&other), other));
    assert!(matches!(read_back(&defs[0], &records), Err(DefinitionError::Mismatch { .. })));
    // A proc macro written under a name that is not its export name is refused.
    let renamed = defs[4].write(item, "Deserialize", None, blaze_own::testing::heap().alloc());
    assert!(matches!(renamed.err(), Some(BuildError::Disagrees { .. })));
}

#[test]
fn an_expansion_record_refuses_a_token_entry_it_does_not_write() {
    let exp = call_fn(&builtin(BuiltinFn::Stringify), "a", &TestCx::new());
    let mut bytes = exp.write_record(&same_maps(), blaze_own::testing::heap().alloc()).expect("its tokens make a token record");
    // The one token, an unsuffixed string literal standing alone: tag 2, kind 4 at bit 5.
    let word = (2u32 | 4 << 5).to_le_bytes();
    let at = bytes
        .windows(4)
        .position(|it| it == word)
        .expect("the string literal's entry is in the buffer");
    // Tag 7 names no token.
    bytes[at] = 7;
    let refused = Expansion::<'static>::read_record(&bytes, blaze_own::testing::heap().alloc());
    assert!(
        matches!(
            refused,
            Err(blaze_expand::RecordError::Invalid {
                column: blaze_expand::RecordColumn::Tokens,
                ..
            })
        ),
        "{refused:?}"
    );
}

/// Which key the oracle is given.
#[derive(Clone, Copy, Debug)]
enum KeyOf {
    /// [`expansion_key`]: the call and its reads.
    Record,
    /// Planted: the call's key alone, blind to what `env!` read.
    CallOnly,
}

/// Two builds whose env differ: unequal expansions must have unequal keys, equal ones equal.
fn keys_follow_expansions(tape: &Tape, key_of: KeyOf) -> Result<(), Broke> {
    const ENVS: [&[(&str, &str)]; 3] = [&[("V", "a")], &[("V", "b")], &[]];
    let one = *tape.pick(&ENVS, "first env").ok_or_else(|| Broke::Harness("no env".into()))?;
    let two = *tape.pick(&ENVS, "second env").ok_or_else(|| Broke::Harness("no env".into()))?;
    let def = builtin(BuiltinFn::OptionEnv);
    let input = tokens("\"V\"");
    let call = Call::fn_like(&input, site());
    let run = |env: &'static [(&'static str, &'static str)]| {
        let cx = TestCx { env, files: &[], cfgs: &[], line: 1, emits: &[] };
        let exp = done(expand(&def, &call, &cx, blaze_own::testing::heap().alloc()));
        let mut bytes = OwnVec::new_in(blaze_own::testing::heap().alloc());
        exp.encode(&mut bytes);
        let key = match key_of {
            KeyOf::Record => expansion_key(&def, &call, &exp.reads).0,
            KeyOf::CallOnly => call_key(&def, &call).0,
        };
        (bytes, key)
    };
    let (one, two) = (run(one), run(two));
    match (one.0[..] == two.0[..], one.1 == two.1) {
        (false, true) => Err(Broke::Oracle("equal keys for unequal expansions".into())),
        (true, false) => Err(Broke::Oracle("unequal keys for equal expansions".into())),
        _ => Ok(()),
    }
}

#[test]
fn the_key_oracle_catches_a_key_blind_to_reads() {
    blaze_sim::control(
        Config::cases(64),
        KeyOf::Record,
        KeyOf::CallOnly,
        "equal keys for unequal expansions",
        keys_follow_expansions,
    );
}

/// `include_str!`'s path is relative to the file the call is in (core `macros/mod.rs`, the
/// pinned library: "The file is located relative to the current file"), `..` taken
/// lexically. It may leave the package (D60, docs/v2/plan/decisions.json): a member reads under
/// the workspace's source root, a git source under its checkout, a registry package only under
/// itself; past that bound, or an absolute path, the call is refused (`Included::Outside`),
/// never read from the host. A build script's output folder is answered from its record.
#[test]
fn an_include_path_is_relative_to_the_calling_file_and_stays_in_its_root() {
    use blaze_expand::{OutsideRoot, ReadRoot, SourceOrigin};
    let target = |origin, package, folder, written| {
        blaze_expand::include_target(ReadRoot { origin, package }, folder, written, blaze_own::testing::heap().alloc())
            .map(|s| s.iter().copied().collect::<Vec<_>>().join("/"))
    };
    let member = |folder, written| target(SourceOrigin::Checkout, "crates/a", folder, written);
    assert_eq!(member("src", "data.txt"), Ok("crates/a/src/data.txt".to_string()));
    assert_eq!(member("src/x", "../../README.md"), Ok("crates/a/README.md".to_string()));
    assert_eq!(member("src", "./y/./z.rs"), Ok("crates/a/src/y/z.rs".to_string()));
    // Another member's file, and the checkout's own: inside the source root.
    assert_eq!(member("src", "../../b/genesis.json"), Ok("crates/b/genesis.json".to_string()));
    assert_eq!(member("", "../../Cargo.toml"), Ok("Cargo.toml".to_string()));
    let past = Err(OutsideRoot { origin: SourceOrigin::Checkout });
    assert_eq!(member("", "../../../etc/passwd"), past);
    assert_eq!(member("src", "/etc/passwd"), past);
    // A git source reads beside its package in its checkout; a registry package stops at itself.
    assert_eq!(target(SourceOrigin::GitCheckout, "pkg", "src", "../../shared/x.txt"), Ok("shared/x.txt".to_string()));
    assert_eq!(target(SourceOrigin::Package, "reg-1.0.0", "src", "../x.txt"), Ok("reg-1.0.0/x.txt".to_string()));
    let edge = Err(OutsideRoot { origin: SourceOrigin::Package });
    assert_eq!(target(SourceOrigin::Package, "reg-1.0.0", "src", "../../other/x.txt"), edge);
    assert_eq!(target(SourceOrigin::Package, "reg", "src", "../../reg-x/y"), edge);
}

/// `panic!` behaves by the edition of the code that WROTE the call, not core's. The pinned core
/// (library/core/src/macros/mod.rs) marks it `#[rustc_builtin_macro(core_panic)]`
/// `#[allow_internal_unstable(edition_panic)]` `macro_rules! panic`, and library/core/src/
/// panic.rs holds its two forms, `pub macro panic_2015 {` and `pub macro panic_2021 {`
/// (`#[unstable(feature = "edition_panic", ..)]`). The same text from a 2018 crate takes the
/// 2015 form (its `"{}"` is no format string there); from a 2021 crate the 2021 form.
/// Expected: the test passes, the two forms printed differ only in `panic_2015`/`panic_2021`.
#[test]
fn the_same_panic_takes_the_form_of_the_writing_crate_s_edition() {
    let cx = TestCx::new();
    let def = builtin(BuiltinFn::Panic);
    let input = tokens("\"{}\", x");
    let at = |edition| CallSite { edition, panic_edition: edition, ctx: Ctx::root(edition) };
    let old = done(expand(&def, &Call::fn_like(&input, at(Edition::E2018)), &cx, blaze_own::testing::heap().alloc()));
    let new = done(expand(&def, &Call::fn_like(&input, at(Edition::E2021)), &cx, blaze_own::testing::heap().alloc()));
    assert_eq!(show(&old.tokens), "$crate :: panic :: panic_2015 ! ( \"{}\" , x )");
    assert_eq!(show(&new.tokens), "$crate :: panic :: panic_2021 ! ( \"{}\" , x )");
    // `assert!` with a message picks the same way (core's `assert!` is under `edition_panic`
    // too): from 2018, `panic_2015!`.
    let assert = done(expand(
        &builtin(BuiltinFn::Assert),
        &Call::fn_like(&tokens("c, \"{}\", x"), at(Edition::E2018)),
        &cx,
        blaze_own::testing::heap().alloc(),
    ));
    assert_eq!(show(&assert.tokens), "if ! ( c ) { $crate :: panic :: panic_2015 ! ( \"{}\" , x ) }");
}

/// `cfg_select!` keeps the first arm whose predicate holds IN THE INVOKING CRATE'S cfg set:
/// the pinned core (library/core/src/macros/mod.rs) documents it as "Selects code at
/// compile-time based on `cfg` predicates. This macro evaluates, at compile-time, a series of
/// `cfg` predicates, selects the first that is true, and emits the code guarded by that
/// predicate." and marks it `#[rustc_builtin_macro] pub macro cfg_select($($tt:tt)*)`. The
/// same call under two crates' sets (one sets `unix`, the other `feature`) keeps two arms.
/// Expected: the test passes.
#[test]
fn cfg_select_keeps_the_arm_the_invoking_crate_s_cfg_holds() {
    let def = builtin(BuiltinFn::CfgSelect);
    let text = "feature => { small } unix => { cyclic } _ => { other }";
    let unix = TestCx::new();
    let sized = TestCx { cfgs: &["feature"], ..TestCx::new() };
    let none = TestCx { cfgs: &[], ..TestCx::new() };
    assert_eq!(show(&call_fn(&def, text, &unix).tokens), "cyclic");
    assert_eq!(show(&call_fn(&def, text, &sized).tokens), "small");
    assert_eq!(show(&call_fn(&def, text, &none).tokens), "other");
    // Each outcome asked is in the read list, so a changed cfg set reruns the call.
    let read = call_fn(&def, text, &unix);
    let answers: Vec<_> = read.reads.list().iter().map(|it| (it.kind, it.answer)).collect();
    assert_eq!(answers, [(ReadKind::Cfg, ReadAnswer::Bool(false)), (ReadKind::Cfg, ReadAnswer::Bool(true))]);
}

/// `asm!` expands to `builtin # asm ( .. )`, the parser's one typed form of inline assembly
/// (`ASM_EXPR`): the pinned core (library/core/src/arch.rs) has `#[rustc_builtin_macro] pub
/// macro asm("assembly template", $(operands,)* $(options($(option),*))?)`, and the Reference
/// (inline-assembly.html) names the operand kinds `in`, `out`, `lateout`, `inout`,
/// `inlateout`, `sym`, `const` and `label`, then `options(..)` and `clobber_abi(..)`. The
/// expansion parses with no error into one `ASM_EXPR` of six operands of the kinds written,
/// one `ASM_OPTIONS` and one `ASM_CLOBBER_ABI`; `global_asm!` and `naked_asm!` take their own
/// keyword; an empty call is the builtin's error. Expected: the test passes.
#[test]
fn asm_expands_to_the_parser_s_typed_form_with_each_operand_s_kind() {
    use parser::{Step, SyntaxKind};
    let cx = TestCx::new();
    let text = "\"mov {0}, {1}\", in(reg) a, out(\"x0\") b, inout(reg) c => d, lateout(reg) _, \
                const 4, sym f, options(nomem, nostack), clobber_abi(\"C\")";
    let exp = call_fn(&builtin(BuiltinFn::Asm), text, &cx);
    assert!(exp.diagnostics.is_empty(), "{:?}", exp.diagnostics);
    assert!(show(&exp.tokens).starts_with("builtin # asm ( \"mov {0}, {1}\" ,"), "{}", show(&exp.tokens));
    let words = blaze_expand::parser_words(&exp.tokens, blaze_own::testing::heap().alloc());
    let output = parser::PrefixEntryPoint::Expr.parse(parser::Tokens::new(&words.words, true));
    let (mut asm, mut options, mut abis) = (0, 0, 0);
    let mut operands: Vec<String> = Vec::new();
    let mut in_dir = false;
    for step in output.iter() {
        match step {
            Step::Error { msg } => panic!("the expansion does not parse: {msg}"),
            Step::Enter { kind: SyntaxKind::ASM_EXPR } => asm += 1,
            Step::Enter { kind: SyntaxKind::ASM_OPTIONS } => options += 1,
            Step::Enter { kind: SyntaxKind::ASM_CLOBBER_ABI } => abis += 1,
            Step::Enter { kind: SyntaxKind::ASM_DIR_SPEC } => in_dir = true,
            Step::Enter { kind: SyntaxKind::ASM_CONST } => operands.push("const".to_owned()),
            Step::Enter { kind: SyntaxKind::ASM_SYM } => operands.push("sym".to_owned()),
            Step::Enter { kind: SyntaxKind::ASM_LABEL } => operands.push("label".to_owned()),
            Step::Token { kind, .. } if in_dir => {
                operands.push(format!("{kind:?}").trim_end_matches("_KW").to_lowercase());
                in_dir = false;
            }
            _ => {}
        }
    }
    assert_eq!((asm, options, abis), (1, 1, 1));
    assert_eq!(operands, ["in", "out", "inout", "lateout", "const", "sym"]);
    for (which, keyword) in [(BuiltinFn::GlobalAsm, "global_asm"), (BuiltinFn::NakedAsm, "naked_asm")] {
        let exp = call_fn(&builtin(which), "\"nop\"", &cx);
        assert_eq!(show(&exp.tokens), format!("builtin # {keyword} ( \"nop\" )"));
    }
    let empty = call_fn(&builtin(BuiltinFn::Asm), "", &cx);
    assert!(matches!(empty.diagnostics[..], [Diagnostic::Builtin { .. }]), "{:?}", empty.diagnostics);
}

/// A host with a read root answers an include by the file's ROOT-RELATIVE path (decision D60,
/// docs/v2/plan/decisions.json: an include is bounded by the unit's source root and its digest
/// goes in the read list), and the read list names the file by that path: never the machine's
/// folder (a leading `/`) nor the call's spelling (`../`). A host's answer that is not
/// root-relative is refused, never recorded. Expected: the test passes.
#[test]
fn an_include_s_read_list_entry_is_root_relative() {
    struct Rooted {
        inner: TestCx,
        /// What the host answers for every path: the root-relative path and whether it exists.
        answer: (&'static str, bool),
    }
    impl ExpandCx for Rooted {
        fn env(&self, name: &str) -> Result<Option<&str>, HostError> {
            self.inner.env(name)
        }
        fn cfg(&self, predicate: &TokenStream<'_>) -> Result<bool, HostError> {
            self.inner.cfg(predicate)
        }
        fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
            Ok(match self.answer {
                (path, true) => Included::At { path, bytes: b"{}" },
                (path, false) => Included::MissingAt { path },
            })
        }
        fn place(&self, what: PlaceOf) -> Result<Place<'_>, HostError> {
            self.inner.place(what)
        }
        fn run_proc_macro<'a>(&self, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'a>) -> Result<Produced<'a>, HostError> {
            self.inner.run_proc_macro(def, call, alloc)
        }
    }
    let written = "\"../../chainspec/src/genesis/dev.json\"";
    let found = Rooted { inner: TestCx::new(), answer: ("crates/chainspec/src/genesis/dev.json", true) };
    for which in [BuiltinFn::IncludeStr, BuiltinFn::IncludeBytes] {
        let input = tokens(written);
        let exp = done(expand(&builtin(which), &Call::fn_like(&input, site()), &found, blaze_own::testing::heap().alloc()));
        assert!(exp.diagnostics.is_empty(), "{:?}", exp.diagnostics);
        let read = exp.reads.list()[0];
        let subject = exp.reads.subject(&read);
        assert_eq!(subject, "crates/chainspec/src/genesis/dev.json");
        assert!(!subject.starts_with('/') && !subject.contains(".."), "{subject}");
        assert_eq!(read.answer, ReadAnswer::Digest(ContentId::of("blaze v2 expand included file 1", b"{}")));
    }
    // A missing file is recorded absent under its root-relative path: creating it reruns.
    let missing = Rooted { inner: TestCx::new(), answer: ("crates/chainspec/x.json", false) };
    let input = tokens(written);
    let exp = done(expand(&builtin(BuiltinFn::IncludeStr), &Call::fn_like(&input, site()), &missing, blaze_own::testing::heap().alloc()));
    let read = exp.reads.list()[0];
    assert_eq!((exp.reads.subject(&read), read.answer), ("crates/chainspec/x.json", ReadAnswer::Absent));
    assert!(matches!(exp.diagnostics[..], [Diagnostic::Include { .. }]), "{:?}", exp.diagnostics);
    // A host that answers the machine's folder, or a spelling, is refused by name.
    for bad in ["/Users/me/tempo/crates/x.json", "crates/../x.json", "crates//x.json", ""] {
        let host = Rooted { inner: TestCx::new(), answer: (bad, true) };
        let input = tokens(written);
        let refused = expand(&builtin(BuiltinFn::IncludeStr), &Call::fn_like(&input, site()), &host, blaze_own::testing::heap().alloc());
        assert!(
            matches!(refused, Err(ExpandError::Host(HostError { ask: HostAsk::Include, why: HostWhy::Unreadable }))),
            "{bad:?}: {:?}",
            refused.map(|_| ())
        );
    }
}
