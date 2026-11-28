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
fn include_tokens_walk_past_transparent_mark_for_hygiene_and_edition() {
    let cx = TestCx {
        files: &[("triple.rs", b"pub struct Triple; macro_rules! triple { () => { $crate::Triple }; }")],
        ..TestCx::new()
    };
    // `core::include!` is defined in a foreign crate on edition 2024; the caller is edition 2018.
    let def = MacroDefinition::builtin_fn(
        BuiltinFn::Include,
        DefAt { edition: Edition::E2024, ..at(0xC0) },
        blaze_own::testing::heap().alloc(),
    );
    let input = lex("\"triple.rs\"", Edition::E2018, blaze_own::testing::heap().alloc()).expect("lex");
    let call = Call::fn_like(&input, CallSite { edition: Edition::E2018, panic_edition: Edition::E2018, ctx: Ctx::root(Edition::E2018) });
    let exp = done(expand(&def, &call, &cx, blaze_own::testing::heap().alloc()));
    assert!(exp.diagnostics.is_empty(), "{:?}", exp.diagnostics);
    for i in 0..exp.tokens.len() {
        let h = exp.hygiene(i);
        assert_eq!(h.mark, None, "token {i} (`{}`) must not carry foreign Transparent mark", exp.tokens.text_at(i));
        assert_eq!(h.edition, Edition::E2018, "token {i} must inherit call-site edition E2018");
    }

    // Layered case: a Transparent mark on top of a SemiOpaque outer mark resolves to the outer
    // SemiOpaque mark (both mark and edition), not the Transparent mark and not None.
    let outer_krate = CrateItemKey(ContentId([0xAA; 32]));
    let mut nested_input = TokenStream::new_in(blaze_own::testing::heap().alloc());
    let outer_ctx = nested_input
        .context(blaze_expand::CtxNode {
            parent: Ctx::root(Edition::E2018),
            mark: blaze_expand::Mark {
                of: MarkOf::Outer(ContentId([0xBB; 32])),
                transparency: blaze_expand::Transparency::SemiOpaque,
                edition: Edition::E2021,
                site: DefSite { krate: outer_krate, module: None },
                local_inner: false,
            },
        })
        .expect("outer ctx");
    nested_input
        .push(
            TokenKind::Literal { kind: LitKind::Str, suffix_len: 0, spacing: Spacing::Alone },
            "triple.rs",
            outer_ctx,
        )
        .expect("push arg");
    let nested_call = Call::fn_like(
        &nested_input,
        CallSite { edition: Edition::E2018, panic_edition: Edition::E2018, ctx: outer_ctx },
    );
    let nested_exp = done(expand(&def, &nested_call, &cx, blaze_own::testing::heap().alloc()));
    let h = nested_exp.hygiene(0);
    assert_eq!(h.mark.map(|m| m.site.krate), Some(outer_krate));
    assert_eq!(h.edition, Edition::E2021);
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
    let eval_ver = |text: &str| {
        let tokens = lex(text, Edition::E2021, blaze_own::testing::heap().alloc()).expect("the predicate lexes");
        blaze_expand::cfg_predicate_with_version(&tokens, true, blaze_own::testing::heap().alloc(), &mut |n, v| set(n, v))
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
    // `version(..)` requires `#![feature(cfg_version)]` (enforced whether true or false).
    for gated in ["version(\"1.76\")", "version(\"99.0\")", "not(version(\"99.0\"))", "all(unix, version(\"99.0\"))"] {
        assert!(matches!(eval(gated).0, Err(CfgPredicateError::UnstableCfgVersion { .. })), "{gated} must be gated");
    }
    // With `cfg_version` enabled, `version(..)` evaluates cooked escapes, line continuations, and raw strings.
    let ver_cases = [
        ("version(\"1.76\")", true),
        ("version(\"1.76.0\")", true),
        ("version(\"1.76\",)", true),
        ("version(\"\\x31.76\")", true),
        ("version(\"1.\\\n  76\")", true),
        ("version(r\"1.76\")", true),
        ("version(r#\"1.76\"#)", true),
        ("version(\"99.0\")", false),
        ("not(version(\"99.0\"))", true),
        ("version(\"..\")", false),
        ("all(unix, version(\"1.76\"), not(version(\"99.0\")))", true),
    ];
    for (text, want) in ver_cases {
        assert_eq!(eval_ver(text), Ok(want), "{text}");
    }
    assert_eq!(eval("all(unix, test)").1, ["unix", "test"]);
    assert!(matches!(eval("not(unix, test)").0, Err(CfgPredicateError::NotTakesOne { .. })));
    assert!(matches!(eval("not()").0, Err(CfgPredicateError::NotTakesOne { .. })));
    // (`lex` closes a group left open, so an unclosed `all(unix` is no case here.)
    assert!(matches!(eval("all(unix test)").0, Err(CfgPredicateError::Malformed { .. })));
    assert!(matches!(eval("all(unix,,)").0, Err(CfgPredicateError::Malformed { .. })));
    assert!(matches!(eval("unix test").0, Err(CfgPredicateError::Malformed { .. })));
    assert!(matches!(eval("feature = 3").0, Err(CfgPredicateError::Malformed { .. })));
    for bad in ["version()", "version(123)", "version(\"1.76\", \"1.77\")", "version(unix)", "unknown_call(\"foo\")"] {
        assert!(matches!(eval(bad).0, Err(CfgPredicateError::Malformed { .. })), "{bad}");
        assert!(matches!(eval_ver(bad), Err(CfgPredicateError::Malformed { .. })), "{bad} (with cfg_version)");
    }
    assert_eq!(eval("any(unix, unknown)").0, Err(CfgPredicateError::Atom(())));
}

#[test]
fn a_cfg_predicate_unwraps_none_delim_groups_from_macro_captures() {
    // `macro_rules!` `:expr` and `:meta` captures emit invisible `Delim::Invisible` groups around their
    // substituted tokens (e.g. `async-compression`'s `#[cfg(all(feature = $algo_s))]`).
    let cx = TestCx::new();
    let def = rules("($s:expr, $m:meta) => { all(feature = $s, $m) };", 1);
    let exp = call_fn(&def, "\"gzip\", unix", &cx);
    assert!(
        exp.tokens.tokens().iter().any(|t| matches!(t.kind, TokenKind::Open(Delim::Invisible))),
        "expected invisible Delim::Invisible groups from :expr/:meta captures"
    );
    let out = cfg_predicate(&exp.tokens, blaze_own::testing::heap().alloc(), &mut |name, value| -> Result<bool, ()> {
        Ok(matches!((name, value), ("feature", Some("gzip")) | ("unix", None)))
    });
    assert_eq!(out, Ok(true));

    let attr_def = rules("($s:expr) => { #[cfg_attr(all(feature = $s), derive(Debug))] };", 1);
    let attr_exp = call_fn(&attr_def, "\"gzip\"", &cx);
    let metas = blaze_expand::cfg_attr_metas(&attr_exp.tokens, blaze_own::testing::heap().alloc(), &mut |name, value| -> Result<bool, ()> {
        Ok(matches!((name, value), ("feature", Some("gzip"))))
    })
    .expect("cfg_attr with :expr capture parses");
    assert_eq!(metas.len(), 1);
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
    let foreign = CrateItemKey(ContentId([42; 32]));
    let defs = [
        rules("($x:expr) => { $x + 1 }", 1),
        rules("() => { $crate::Item }", 1).with_dollar_crate(Some(foreign)),
        MacroDefinition::macro2(Some(tokens("$x:expr")), tokens("$x * 2"), at(2)),
        MacroDefinition::macro2(None, tokens("() => { $crate::Item }"), at(2)).with_dollar_crate(Some(foreign)),
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
        assert_eq!(
            (read.kind(), read.at(), read.dollar_crate(), read.transparency()),
            (def.kind(), def.at(), def.dollar_crate(), def.transparency())
        );
        assert!(read.helpers().eq(def.helpers()) && read.proc_name() == def.proc_name());
        // Written again, the records are the same bytes (rule 25).
        let again = read.write(item, name, None, blaze_own::testing::heap().alloc()).expect("a small definition");
        assert_eq!(again.def[..], records.def[..]);
    }
    // Control: a `dollar_crate` override produces a distinct `MacroContent` hash from `None`.
    let plain_dollar = rules("() => { $crate::Item }", 1);
    assert_ne!(plain_dollar.content(), defs[1].content());
    assert_eq!(plain_dollar.dollar_crate(), None);
    assert_eq!(defs[1].dollar_crate(), Some(foreign));
    // Control: a body record of another stream is refused by its digest, never read as the body.
    let mut records = defs[0].write(item, "m", None, blaze_own::testing::heap().alloc()).expect("a small definition");
    let other = tokens("() => {}").write_record(blaze_own::testing::heap().alloc());
    records.body = Some((stream_record_id(&other), other));
    assert!(matches!(read_back(&defs[0], &records), Err(DefinitionError::Mismatch { .. })));
    // A proc macro written under a name that is not its export name is refused.
    let renamed = defs[6].write(item, "Deserialize", None, blaze_own::testing::heap().alloc());
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

/// An expression fragment with leading outer attributes (`#[cfg(windows)] "a".parse().unwrap()`,
/// as in `vec![#[cfg(windows)] ...]`) keeps the outer attributes OUTSIDE any synthetic
/// parentheses so `#[cfg(..)]` strips the whole element instead of leaving an empty `PAREN_EXPR`.
#[test]
fn expr_fragment_keeps_outer_attrs_outside_synthetic_parens() {
    let cx = TestCx::new();
    let def = rules("($($x:expr),+ $(,)?) => { [$($x),+] }", 1);
    let exp = call_fn(
        &def,
        "#[cfg(unix)] \"a\".parse().unwrap(), #[cfg(windows)] \"b\".parse().unwrap(), #[allow(unused)] 1",
        &cx,
    );
    let shown = show(&exp.tokens);
    assert!(
        shown.contains("# [ cfg ( windows ) ] ( \"b\" . parse ( ) . unwrap ( ) )"),
        "outer #[cfg(windows)] must stay outside synthetic parens: {shown}"
    );
    assert!(
        !shown.contains("( # [ cfg ( windows ) ]"),
        "outer #[cfg(windows)] must not be wrapped inside synthetic parens: {shown}"
    );
    assert!(
        shown.contains("# [ allow ( unused ) ] 1"),
        "leaf body after outer attr needs no synthetic parens: {shown}"
    );
}

/// Helper: expands `stringify!(input_stream)` and returns the unquoted string literal text.
fn stringify_stream(input: &TokenStream<'_>, cx: &TestCx) -> String {
    let mut sealed = input.clone();
    sealed.seal(ContentId([9; 32]));
    let exp = call_fn_tokens(&builtin(BuiltinFn::Stringify), &sealed, cx);
    assert!(exp.diagnostics.is_empty(), "{:?}", exp.diagnostics);
    assert_eq!(exp.tokens.len(), 1, "stringify! emits one string literal");
    assert!(
        matches!(exp.tokens.tokens()[0].kind, TokenKind::Literal { kind: LitKind::Str, .. }),
        "expected Str literal, got {:?}",
        exp.tokens.tokens()[0].kind
    );
    exp.tokens.text_at(0).to_owned()
}

/// Reproduces `cranelift-codegen-meta/src/pulley.rs:60` (`pulley_interpreter::for_each_op!(define)`):
/// tokens coming from a `macro_rules!` RHS template never carry `Spacing::JointHidden`, so
/// `stringify!($field_ty)` produces `"BinaryOperands < XReg >"` and `"UpperRegSet < XReg >"`.
#[test]
fn for_each_op_define_stringify_matches_rustc() {
    let cx = TestCx::new();
    let alloc = blaze_own::testing::heap().alloc();
    let for_each_op = rules(
        "($m:ident) => { $m! { \
            xadd32 = Xadd32 { operands: BinaryOperands<XReg> }; \
            xpush = Xpush { regs: UpperRegSet<XReg> }; \
        } }",
        1,
    );
    let define = rules(
        "($( $snake:ident = $camel:ident { $( $field:ident : $field_ty:ty ),* }; )*) => { \
            [ $( $( stringify!($field_ty) ),* ),* ] \
        }",
        2,
    );

    let step1 = call_fn(&for_each_op, "define", &cx);
    assert!(step1.diagnostics.is_empty(), "{:?}", step1.diagnostics);
    let calls1 = find_calls(&step1.tokens, alloc).expect("finds `define!` call");
    assert_eq!(calls1.len(), 1);
    let mut define_input = step1.tokens.copy_in(calls1[0].inner().to_usize(), alloc).expect("copies");
    define_input.seal(ContentId([7; 32]));

    let step2 = call_fn_tokens(&define, &define_input, &cx);
    assert!(step2.diagnostics.is_empty(), "{:?}", step2.diagnostics);
    let calls2 = find_calls(&step2.tokens, alloc).expect("finds `stringify!` calls");
    assert_eq!(calls2.len(), 2);

    let results: Vec<String> = calls2
        .iter()
        .map(|call| {
            let arg = step2.tokens.copy_in(call.inner().to_usize(), alloc).expect("copies");
            stringify_stream(&arg, &cx)
        })
        .collect();
    assert_eq!(results, vec!["BinaryOperands < XReg >", "UpperRegSet < XReg >"]);
    assert_eq!(results[0].strip_prefix("BinaryOperands <"), Some(" XReg >"));
}

/// Tokens from a `macro_rules!` RHS template (`TokenOrigin::Def`) drop `Spacing::JointHidden`
/// (keeping only `Spacing::Joint` on glued punct) and `stringify!` spaces them as rustc does.
#[test]
fn macro_rules_rhs_stringify_spacing_matches_rustc() {
    let cx = TestCx::new();
    let cases = [
        ("BinaryOperands<XReg>", "BinaryOperands < XReg >"),
        ("UpperRegSet<XReg>", "UpperRegSet < XReg >"),
        ("BinaryOperands<XReg, YReg>", "BinaryOperands < XReg, YReg >"),
        ("foo::Bar<XReg>", "foo :: Bar < XReg >"),
        ("$crate::f(1)", "$crate :: f(1)"),
        ("let x = 1;", "let x = 1;"),
        ("a.b.c()", "a.b.c()"),
        ("#[allow(dead_code)] struct S { x: i32 }", "#[allow(dead_code)] struct S { x : i32 }"),
        ("foo!(a, b)", "foo! (a, b)"),
        ("1 ! 2", "1 ! 2"),
        ("if ! ()", "if ! ()"),
        ("(a)!", "(a)!"),
        ("foo!=bar", "foo != bar"),
        ("a[0]", "a [0]"),
        ("1+2", "1 + 2"),
        ("&'a mut XReg", "& 'a mut XReg"),
        ("0..=10", "0 ..= 10"),
        ("0..10", "0 .. 10"),
        ("0...10", "0 ... 10"),
    ];
    for (rhs, expected) in cases {
        let def = rules(&format!("() => {{ {rhs} }}"), 1);
        let exp = call_fn(&def, "", &cx);
        assert!(exp.diagnostics.is_empty(), "{rhs}: {:?}", exp.diagnostics);
        let got = stringify_stream(&exp.tokens, &cx);
        assert_eq!(got, expected, "macro_rules! RHS `{rhs}`");
    }
}

/// Direct `stringify!(...)` and tokens forwarded from macro call input (`TokenOrigin::Input`,
/// e.g. `id_tt!(...)`) preserve `Spacing::JointHidden` so adjacent source tokens stay unspaced.
#[test]
fn direct_and_input_forwarded_stringify_spacing_matches_rustc() {
    let cx = TestCx::new();
    let id_tt = rules("($($t:tt)*) => { $($t)* }", 1);
    let cases = [
        ("BinaryOperands<XReg>", "BinaryOperands<XReg>"),
        ("foo::Bar<XReg>", "foo::Bar<XReg>"),
        ("foo::<XReg>", "foo::<XReg>"),
        ("a+b-c*d", "a+b-c*d"),
        ("-1", "-1"),
        ("&'a mut XReg", "&'a mut XReg"),
        ("BinaryOperands   <   XReg   >", "BinaryOperands < XReg >"),
        ("foo != bar", "foo != bar"),
        ("foo!=bar", "foo!=bar"),
        ("0..=10", "0..=10"),
    ];
    for (src, expected) in cases {
        let direct = stringify_stream(&tokens(src), &cx);
        assert_eq!(direct, expected, "direct stringify!({src})");

        let forwarded = call_fn(&id_tt, src, &cx);
        assert!(forwarded.diagnostics.is_empty(), "id_tt!({src}): {:?}", forwarded.diagnostics);
        let got = stringify_stream(&forwarded.tokens, &cx);
        assert_eq!(got, expected, "id_tt!({src}) -> stringify!");
    }
}

#[test]
fn include_tokens_carry_call_site_ctx_without_core_mark() {
    use blaze_expand::{CtxNode, Mark, Transparency};

    let core_krate = CrateItemKey(ContentId([1; 32]));
    let caller_krate = CrateItemKey(ContentId([2; 32]));
    let include_def = MacroDefinition::builtin_fn(
        BuiltinFn::Include,
        DefAt {
            site: DefSite { krate: core_krate, module: None },
            edition: Edition::E2021,
            export: Export::Exported,
            local_inner: LocalInner::No,
            transparency: None,
        },
        blaze_own::testing::heap().alloc(),
    );
    let cx = TestCx {
        files: &[(
            "src/triple.rs",
            b"pub struct Triple;\n#[macro_export]\nmacro_rules! triple { ($s:tt) => { $crate::Triple }; }\n",
        )],
        ..TestCx::new()
    };

    // Direct call from crate B at root context: included tokens carry no mark at all (never `core`).
    let input = tokens("\"src/triple.rs\"");
    let exp = done(expand(&include_def, &Call::fn_like(&input, site()), &cx, blaze_own::testing::heap().alloc()));
    assert!(exp.diagnostics.is_empty(), "{:?}", exp.diagnostics);
    assert!(exp.tokens.contexts().is_empty(), "include! must not push core::include's mark");
    assert!(exp.origins.iter().all(|it| *it == TokenOrigin::Made(Transparency::Transparent)));
    for i in 0..exp.tokens.len() {
        assert_eq!(exp.hygiene(i).mark, None, "token {i} (`{}`) carries no mark", exp.tokens.text_at(i));
        assert_eq!(exp.non_transparent_mark(i), None);
    }

    // Nested call where the `include!` call site itself carries crate B's `SemiOpaque` mark:
    // included tokens inherit the call site's context (crate B), never `core`.
    let alloc = blaze_own::testing::heap().alloc();
    let mut marked_input = lex("\"src/triple.rs\"", Edition::E2021, alloc).expect("lexes");
    let caller_mark = Mark {
        of: MarkOf::Outer(ContentId([9; 32])),
        transparency: Transparency::SemiOpaque,
        edition: Edition::E2021,
        site: DefSite { krate: caller_krate, module: None },
        local_inner: false,
    };
    let call_ctx = marked_input
        .context(CtxNode { parent: Ctx::root(Edition::E2021), mark: caller_mark })
        .expect("context");
    let nested_site = CallSite { edition: Edition::E2021, panic_edition: Edition::E2021, ctx: call_ctx };
    let nested_exp = done(expand(&include_def, &Call::fn_like(&marked_input, nested_site), &cx, alloc));
    assert!(nested_exp.diagnostics.is_empty(), "{:?}", nested_exp.diagnostics);
    for i in 0..nested_exp.tokens.len() {
        assert_eq!(nested_exp.hygiene(i).mark, Some(caller_mark));
        assert_eq!(nested_exp.non_transparent_mark(i), Some(caller_mark));
        assert_ne!(nested_exp.non_transparent_mark(i).map(|m| m.site.krate), Some(core_krate));
    }
}

#[test]
fn non_transparent_mark_skips_transparent_and_preserves_semi_opaque_and_opaque() {
    use blaze_expand::{CtxNode, Mark, Transparency, body_words};

    let alloc = blaze_own::testing::heap().alloc();
    let krate_a = CrateItemKey(ContentId([10; 32]));
    let krate_b = CrateItemKey(ContentId([20; 32]));
    let krate_c = CrateItemKey(ContentId([30; 32]));

    let transparent_a = Mark {
        of: MarkOf::This,
        transparency: Transparency::Transparent,
        edition: Edition::E2021,
        site: DefSite { krate: krate_a, module: None },
        local_inner: false,
    };
    let transparent_2024 = Mark {
        of: MarkOf::Outer(ContentId([11; 32])),
        transparency: Transparency::Transparent,
        edition: Edition::E2024,
        site: DefSite { krate: krate_a, module: None },
        local_inner: false,
    };
    let semi_b = Mark {
        of: MarkOf::Outer(ContentId([21; 32])),
        transparency: Transparency::SemiOpaque,
        edition: Edition::E2024,
        site: DefSite { krate: krate_b, module: None },
        local_inner: false,
    };
    let semi_b_2021 = Mark {
        of: MarkOf::Outer(ContentId([22; 32])),
        transparency: Transparency::SemiOpaque,
        edition: Edition::E2021,
        site: DefSite { krate: krate_b, module: None },
        local_inner: false,
    };
    let opaque_c = Mark {
        of: MarkOf::Outer(ContentId([31; 32])),
        transparency: Transparency::Opaque,
        edition: Edition::E2024,
        site: DefSite { krate: krate_c, module: None },
        local_inner: false,
    };

    let mut stream = TokenStream::new_in(alloc);
    let root = Ctx::root(Edition::E2021);
    let only_transparent = stream.context(CtxNode { parent: root, mark: transparent_a }).expect("ctx");
    let semi_ctx = stream.context(CtxNode { parent: root, mark: semi_b }).expect("ctx");
    let semi_2021_ctx = stream.context(CtxNode { parent: root, mark: semi_b_2021 }).expect("ctx");
    let opaque_ctx = stream.context(CtxNode { parent: root, mark: opaque_c }).expect("ctx");
    let transparent_over_semi = stream.context(CtxNode { parent: semi_ctx, mark: transparent_a }).expect("ctx");
    let transparent_2024_over_semi_2021 = stream.context(CtxNode { parent: semi_2021_ctx, mark: transparent_2024 }).expect("ctx");
    let transparent_over_opaque = stream.context(CtxNode { parent: opaque_ctx, mark: transparent_a }).expect("ctx");

    use blaze_expand::{local_hygiene, origin_column::TextMark};
    let this = ContentId([99; 32]);
    let semi_h = local_hygiene(&stream, semi_ctx, this);
    let semi_2021_h = local_hygiene(&stream, semi_2021_ctx, this);
    let opaque_h = local_hygiene(&stream, opaque_ctx, this);
    assert_ne!(semi_h, 0);
    assert_ne!(semi_2021_h, 0);
    assert_ne!(opaque_h, 0);
    assert_eq!(stream.non_transparent_mark(root), None);
    assert_eq!(stream.text_mark_of(root, this), None);
    assert_eq!(stream.node(only_transparent).map(|n| n.mark), Some(transparent_a));
    assert_eq!(stream.hygiene(only_transparent).mark, None);
    assert_eq!(stream.non_transparent_mark(only_transparent), None);
    assert_eq!(stream.text_mark_of(only_transparent, this), Some(TextMark::of(transparent_a)));
    assert_eq!(stream.non_transparent_mark(semi_ctx), Some(semi_b));
    assert_eq!(stream.text_mark_of(semi_ctx, this), Some(TextMark { hygiene: semi_h, ..TextMark::of(semi_b) }));
    assert_eq!(stream.non_transparent_mark(opaque_ctx), Some(opaque_c));
    assert_eq!(stream.text_mark_of(opaque_ctx, this), Some(TextMark { hygiene: opaque_h, ..TextMark::of(opaque_c) }));
    assert_eq!(stream.node(transparent_over_semi).map(|n| n.mark), Some(transparent_a));
    assert_eq!(stream.hygiene(transparent_over_semi).mark, Some(semi_b));
    assert_eq!(stream.non_transparent_mark(transparent_over_semi), Some(semi_b));
    // Nested E2021 Transparent over E2024 SemiOpaque keeps SemiOpaque hygiene + E2021 edition.
    assert_eq!(
        stream.text_mark_of(transparent_over_semi, this),
        Some(TextMark { site: semi_b.site, transparency: Transparency::SemiOpaque, edition: Edition::E2021, local_inner: false, hygiene: semi_h })
    );
    // Reverse nesting: E2024 Transparent over E2021 SemiOpaque keeps SemiOpaque hygiene + E2024 edition.
    assert_eq!(
        stream.text_mark_of(transparent_2024_over_semi_2021, this),
        Some(TextMark { site: semi_b_2021.site, transparency: Transparency::SemiOpaque, edition: Edition::E2024, local_inner: false, hygiene: semi_2021_h })
    );
    // `TextMark::local_hygiene()` depends only on `(site, transparency, hygiene)`, not `edition`.
    assert_eq!(
        stream.text_mark_of(semi_ctx, this).expect("mark").local_hygiene(),
        stream.text_mark_of(transparent_over_semi, this).expect("mark").local_hygiene(),
    );
    assert_eq!(stream.node(transparent_over_opaque).map(|n| n.mark), Some(transparent_a));
    assert_eq!(stream.hygiene(transparent_over_opaque).mark, Some(opaque_c));
    assert_eq!(stream.non_transparent_mark(transparent_over_opaque), Some(opaque_c));
    assert_eq!(
        stream.text_mark_of(transparent_over_opaque, this),
        Some(TextMark { site: opaque_c.site, transparency: Transparency::Opaque, edition: Edition::E2021, local_inner: false, hygiene: opaque_h })
    );

    // `body_words` uses `text_mark`: preserves innermost `Transparent` edition while keeping
    // enclosing non-transparent hygiene (`site`, `transparency`) and local hygiene.
    stream.ident("$crate", only_transparent).expect("push");
    stream.ident("$crate", transparent_over_semi).expect("push");
    stream.ident("$crate", transparent_2024_over_semi_2021).expect("push");
    stream.ident("$crate", transparent_over_opaque).expect("push");
    let body = body_words(&stream, ContentId([99; 32]), alloc);
    assert_eq!(body.words.len(), 4);
    assert_eq!(body.words[0].hygiene, 0);
    assert_eq!(body.words[0].mark, Some(TextMark::of(transparent_a)));
    assert_eq!(
        body.words[1].mark.map(|m| (m.site.krate, m.transparency, m.edition)),
        Some((krate_b, Transparency::SemiOpaque, Edition::E2021))
    );
    assert_eq!(
        body.words[2].mark.map(|m| (m.site.krate, m.transparency, m.edition)),
        Some((krate_b, Transparency::SemiOpaque, Edition::E2024))
    );
    assert_eq!(
        body.words[3].mark.map(|m| (m.site.krate, m.transparency, m.edition)),
        Some((krate_c, Transparency::Opaque, Edition::E2021))
    );
}

#[test]
fn include_text_mark_and_body_words_preserve_caller_edition() {
    use blaze_expand::{CtxNode, Mark, Transparency, body_words, local_hygiene, origin_column::TextMark};
    let alloc = blaze_own::testing::heap().alloc();
    let cx = TestCx {
        files: &[("inc.rs", b"match self { Self::A(ref mut x) => *x }")],
        ..TestCx::new()
    };

    // 1. E2024 `include!` invoked from E2021 caller -> `text_mark_of` and `body_words` carry `None`
    //    (falling back to caller E2021, never the built-in macro's E2024).
    let def_2024 = MacroDefinition::builtin_fn(
        BuiltinFn::Include,
        DefAt { edition: Edition::E2024, ..at(0xC0) },
        alloc,
    );
    let input_2021 = lex("\"inc.rs\"", Edition::E2021, alloc).expect("lex");
    let call_2021 = Call::fn_like(
        &input_2021,
        CallSite { edition: Edition::E2021, panic_edition: Edition::E2021, ctx: Ctx::root(Edition::E2021) },
    );
    let exp_2021 = done(expand(&def_2024, &call_2021, &cx, alloc));
    assert!(exp_2021.diagnostics.is_empty(), "{:?}", exp_2021.diagnostics);
    for i in 0..exp_2021.tokens.len() {
        assert_eq!(exp_2021.text_mark_of(i, ContentId([1; 32])), None, "E2024 include! in E2021 caller must leave text_mark_of None");
        assert_eq!(exp_2021.hygiene(i).edition, Edition::E2021);
    }
    let bw_2021 = body_words(&exp_2021.tokens, ContentId([1; 32]), alloc);
    assert!(bw_2021.words.iter().all(|w| w.mark.is_none()));

    // 2. E2021 `include!` invoked from E2024 caller -> `text_mark_of` and `body_words` carry `None`
    //    (falling back to caller E2024, never the built-in macro's E2021).
    let def_2021 = MacroDefinition::builtin_fn(
        BuiltinFn::Include,
        DefAt { edition: Edition::E2021, ..at(0xC1) },
        alloc,
    );
    let input_2024 = lex("\"inc.rs\"", Edition::E2024, alloc).expect("lex");
    let call_2024 = Call::fn_like(
        &input_2024,
        CallSite { edition: Edition::E2024, panic_edition: Edition::E2024, ctx: Ctx::root(Edition::E2024) },
    );
    let exp_2024 = done(expand(&def_2021, &call_2024, &cx, alloc));
    assert!(exp_2024.diagnostics.is_empty(), "{:?}", exp_2024.diagnostics);
    for i in 0..exp_2024.tokens.len() {
        assert_eq!(exp_2024.text_mark_of(i, ContentId([2; 32])), None, "E2021 include! in E2024 caller must leave text_mark_of None");
        assert_eq!(exp_2024.hygiene(i).edition, Edition::E2024);
    }
    let bw_2024 = body_words(&exp_2024.tokens, ContentId([2; 32]), alloc);
    assert!(bw_2024.words.iter().all(|w| w.mark.is_none()));

    // 3. `include!` invoked from a `Transparent(E2021)` proc-macro context over `Ctx::root(E2024)`
    //    walks past the transparent mark so included file tokens keep caller E2024 (`None`),
    //    not the proc-macro's E2021.
    let mut trans_input = TokenStream::new_in(alloc);
    let proc_mark = Mark {
        of: MarkOf::Outer(ContentId([0xD0; 32])),
        transparency: Transparency::Transparent,
        edition: Edition::E2021,
        site: DefSite { krate: CrateItemKey(ContentId([0xD1; 32])), module: None },
        local_inner: false,
    };
    let trans_ctx = trans_input
        .context(CtxNode { parent: Ctx::root(Edition::E2024), mark: proc_mark })
        .expect("trans ctx");
    trans_input
        .push(TokenKind::Literal { kind: LitKind::Str, suffix_len: 0, spacing: Spacing::Alone }, "inc.rs", trans_ctx)
        .expect("push");
    let trans_call = Call::fn_like(
        &trans_input,
        CallSite { edition: Edition::E2024, panic_edition: Edition::E2024, ctx: trans_ctx },
    );
    let exp_trans = done(expand(&def_2021, &trans_call, &cx, alloc));
    assert!(exp_trans.diagnostics.is_empty(), "{:?}", exp_trans.diagnostics);
    for i in 0..exp_trans.tokens.len() {
        assert_eq!(exp_trans.text_mark_of(i, ContentId([3; 32])), None, "include! inside Transparent proc-macro must inherit caller root E2024");
        assert_eq!(exp_trans.hygiene(i).edition, Edition::E2024);
    }

    // 4. `include!` invoked from `Transparent(E2024)` over `SemiOpaque(E2021)` preserves the
    //    enclosing `SemiOpaque(E2021)` mark and strips the transparent layer.
    let mut layered_input = TokenStream::new_in(alloc);
    let semi_mark = Mark {
        of: MarkOf::Outer(ContentId([0xE0; 32])),
        transparency: Transparency::SemiOpaque,
        edition: Edition::E2021,
        site: DefSite { krate: CrateItemKey(ContentId([0xE1; 32])), module: None },
        local_inner: false,
    };
    let trans_2024_mark = Mark {
        of: MarkOf::Outer(ContentId([0xE2; 32])),
        transparency: Transparency::Transparent,
        edition: Edition::E2024,
        site: DefSite { krate: CrateItemKey(ContentId([0xE3; 32])), module: None },
        local_inner: false,
    };
    let semi_ctx = layered_input
        .context(CtxNode { parent: Ctx::root(Edition::E2018), mark: semi_mark })
        .expect("semi ctx");
    let layered_ctx = layered_input
        .context(CtxNode { parent: semi_ctx, mark: trans_2024_mark })
        .expect("layered ctx");
    layered_input
        .push(TokenKind::Literal { kind: LitKind::Str, suffix_len: 0, spacing: Spacing::Alone }, "inc.rs", layered_ctx)
        .expect("push");
    let layered_call = Call::fn_like(
        &layered_input,
        CallSite { edition: Edition::E2018, panic_edition: Edition::E2018, ctx: layered_ctx },
    );
    let exp_layered = done(expand(&def_2024, &layered_call, &cx, alloc));
    assert!(exp_layered.diagnostics.is_empty(), "{:?}", exp_layered.diagnostics);
    let layered_key = ContentId([4; 32]);
    for i in 0..exp_layered.tokens.len() {
        let want_h = local_hygiene(&exp_layered.tokens, exp_layered.tokens.tokens()[i].ctx, layered_key);
        assert_eq!(exp_layered.text_mark_of(i, layered_key), Some(TextMark { hygiene: want_h, ..TextMark::of(semi_mark) }));
    }
}

#[test]
fn lex_marked_preserves_transparent_edition_without_folding_dollar_crate() {
    use blaze_expand::{Transparency, origin_column::TextMark};
    let alloc = blaze_own::testing::heap().alloc();
    let site = DefSite { krate: CrateItemKey(ContentId([41; 32])), module: None };
    let transparent_2021 = TextMark { site, transparency: Transparency::Transparent, edition: Edition::E2021, local_inner: false, hygiene: 0 };
    let semi_2021 = TextMark { site, transparency: Transparency::SemiOpaque, edition: Edition::E2021, local_inner: false, hygiene: 0 };

    let text = "$crate::Foo";
    // Transparent mark: `$` and `crate` remain two tokens (same as unmarked), while preserving
    // the transparent mark's edition via `text_mark_of`.
    let ts_trans = blaze_expand::lex_marked(text, Edition::E2024, &|_| Some(transparent_2021), alloc).expect("lex transparent");
    assert_eq!(ts_trans.len(), 5, "transparent `$crate::Foo` stays `$`, `crate`, `:`, `:`, `Foo`");
    assert!(matches!(ts_trans.tokens()[0].kind, TokenKind::Punct { char: '$', .. }));
    assert_eq!(ts_trans.text_at(1), "crate");
    assert_eq!(ts_trans.text_mark_of(ts_trans.tokens()[0].ctx, ContentId::default()), Some(transparent_2021));
    assert_eq!(ts_trans.non_transparent_mark(ts_trans.tokens()[0].ctx), None);

    // Control: SemiOpaque mark folds `$` + `crate` into one `$crate` identifier.
    let ts_semi = blaze_expand::lex_marked(text, Edition::E2024, &|_| Some(semi_2021), alloc).expect("lex semi");
    assert_eq!(ts_semi.len(), 4, "semi-opaque `$crate::Foo` folds to `$crate`, `:`, `:`, `Foo`");
    assert_eq!(ts_semi.text_at(0), "$crate");
    assert_eq!(
        ts_semi.text_mark_of(ts_semi.tokens()[0].ctx, ContentId::default()),
        Some(TextMark { hygiene: semi_2021.local_hygiene(), ..semi_2021 })
    );
}

#[test]
fn nested_transparent_proc_macro_over_semi_opaque_call_site_preserves_innermost_edition() {
    use blaze_expand::{
        CtxNode, HostAsk, HostError, HostWhy, Mark, ProcCall, ProcKind, Produced, Reads, Transparency,
        body_words, origin_column::TextMark,
    };
    use blaze_own::AVec;

    struct EmitRefMut(Edition);
    impl ExpandCx for EmitRefMut {
        fn cfg(&self, _: &TokenStream<'_>) -> Result<bool, HostError> {
            Err(HostError { ask: HostAsk::Cfg, why: HostWhy::Refused })
        }
        fn env(&self, _: &str) -> Result<Option<&str>, HostError> {
            Err(HostError { ask: HostAsk::Env, why: HostWhy::Refused })
        }
        fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
            Err(HostError { ask: HostAsk::Include, why: HostWhy::Refused })
        }
        fn place(&self, _: PlaceOf) -> Result<Place<'_>, HostError> {
            Err(HostError { ask: HostAsk::Place, why: HostWhy::Refused })
        }
        fn run_proc_macro<'a>(&self, _: &MacroDefinition<'_>, _: ProcCall<'_>, alloc: Alloc<'a>) -> Result<Produced<'a>, HostError> {
            let tokens = lex("ref mut arg", self.0, alloc).expect("lex");
            let mut origins = AVec::new_in(alloc);
            origins.extend(tokens.tokens().iter().map(|_| TokenOrigin::Made(Transparency::Transparent)));
            Ok(Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
        }
    }

    let alloc = blaze_own::testing::heap().alloc();
    let rules_2024_krate = CrateItemKey(ContentId([0x41; 32]));
    let rules_2021_krate = CrateItemKey(ContentId([0x42; 32]));
    let proc_2021_krate = CrateItemKey(ContentId([0x43; 32]));
    let proc_2024_krate = CrateItemKey(ContentId([0x44; 32]));

    // Direction 1: E2021 transparent proc-macro invoked from E2024 SemiOpaque `macro_rules!` ctx.
    let proc_2021 = MacroDefinition::proc_macro(
        proc_2021_krate,
        ProcKind::FnLike,
        "emit",
        &[],
        DefAt { edition: Edition::E2021, site: DefSite { krate: proc_2021_krate, module: None }, ..at(0x43) },
        alloc,
    )
    .expect("proc_2021");
    let mut input_2024 = TokenStream::new_in(alloc);
    let semi_2024 = Mark {
        of: MarkOf::Outer(ContentId([0x51; 32])),
        transparency: Transparency::SemiOpaque,
        edition: Edition::E2024,
        site: DefSite { krate: rules_2024_krate, module: None },
        local_inner: false,
    };
    let semi_2024_ctx = input_2024
        .context(CtxNode { parent: Ctx::root(Edition::E2024), mark: semi_2024 })
        .expect("ctx");
    let call_1 = Call::fn_like(&input_2024, CallSite { edition: Edition::E2024, panic_edition: Edition::E2024, ctx: semi_2024_ctx });
    let exp_1 = done(expand(&proc_2021, &call_1, &EmitRefMut(Edition::E2021), alloc));
    assert!(exp_1.diagnostics.is_empty(), "{:?}", exp_1.diagnostics);
    let key_1 = ContentId([0x61; 32]);
    let h_1 = blaze_expand::local_hygiene(&exp_1.tokens, exp_1.tokens.tokens()[0].ctx, key_1);
    let want_1 = TextMark { site: semi_2024.site, transparency: Transparency::SemiOpaque, edition: Edition::E2021, local_inner: false, hygiene: h_1 };
    for i in 0..exp_1.tokens.len() {
        assert_eq!(exp_1.text_mark_of(i, key_1), Some(want_1));
        assert_eq!(exp_1.non_transparent_mark(i), Some(semi_2024));
    }
    let bw_1 = body_words(&exp_1.tokens, key_1, alloc);
    assert!(bw_1.words.iter().all(|w| w.mark == Some(want_1) && w.hygiene == h_1 && w.hygiene != 0));

    // Direction 2: E2024 transparent proc-macro invoked from E2021 SemiOpaque `macro_rules!` ctx.
    let proc_2024 = MacroDefinition::proc_macro(
        proc_2024_krate,
        ProcKind::FnLike,
        "emit",
        &[],
        DefAt { edition: Edition::E2024, site: DefSite { krate: proc_2024_krate, module: None }, ..at(0x44) },
        alloc,
    )
    .expect("proc_2024");
    let mut input_2021 = TokenStream::new_in(alloc);
    let semi_2021 = Mark {
        of: MarkOf::Outer(ContentId([0x52; 32])),
        transparency: Transparency::SemiOpaque,
        edition: Edition::E2021,
        site: DefSite { krate: rules_2021_krate, module: None },
        local_inner: false,
    };
    let semi_2021_ctx = input_2021
        .context(CtxNode { parent: Ctx::root(Edition::E2021), mark: semi_2021 })
        .expect("ctx");
    let call_2 = Call::fn_like(&input_2021, CallSite { edition: Edition::E2021, panic_edition: Edition::E2021, ctx: semi_2021_ctx });
    let exp_2 = done(expand(&proc_2024, &call_2, &EmitRefMut(Edition::E2024), alloc));
    assert!(exp_2.diagnostics.is_empty(), "{:?}", exp_2.diagnostics);
    let key_2 = ContentId([0x62; 32]);
    let h_2 = blaze_expand::local_hygiene(&exp_2.tokens, exp_2.tokens.tokens()[0].ctx, key_2);
    let want_2 = TextMark { site: semi_2021.site, transparency: Transparency::SemiOpaque, edition: Edition::E2024, local_inner: false, hygiene: h_2 };
    for i in 0..exp_2.tokens.len() {
        assert_eq!(exp_2.text_mark_of(i, key_2), Some(want_2));
        assert_eq!(exp_2.non_transparent_mark(i), Some(semi_2021));
    }
    let bw_2 = body_words(&exp_2.tokens, key_2, alloc);
    assert!(bw_2.words.iter().all(|w| w.mark == Some(want_2) && w.hygiene == h_2 && w.hygiene != 0));
}

#[test]
fn transparent_edition_drives_parser_words_keyword_classification() {
    use blaze_expand::{
        HostAsk, HostError, HostWhy, ProcCall, ProcKind, Produced, Reads, Transparency, body_words,
    };
    use blaze_own::AVec;
    use parser::SyntaxKind;

    struct EmitText(Edition, &'static str);
    impl ExpandCx for EmitText {
        fn cfg(&self, _: &TokenStream<'_>) -> Result<bool, HostError> {
            Err(HostError { ask: HostAsk::Cfg, why: HostWhy::Refused })
        }
        fn env(&self, _: &str) -> Result<Option<&str>, HostError> {
            Err(HostError { ask: HostAsk::Env, why: HostWhy::Refused })
        }
        fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
            Err(HostError { ask: HostAsk::Include, why: HostWhy::Refused })
        }
        fn place(&self, _: PlaceOf) -> Result<Place<'_>, HostError> {
            Err(HostError { ask: HostAsk::Place, why: HostWhy::Refused })
        }
        fn run_proc_macro<'a>(&self, _: &MacroDefinition<'_>, _: ProcCall<'_>, alloc: Alloc<'a>) -> Result<Produced<'a>, HostError> {
            let tokens = lex(self.1, self.0, alloc).expect("lex");
            let mut origins = AVec::new_in(alloc);
            origins.extend(tokens.tokens().iter().map(|_| TokenOrigin::Made(Transparency::Transparent)));
            Ok(Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
        }
    }

    let alloc = blaze_own::testing::heap().alloc();
    let empty_2024 = TokenStream::new_in(alloc);
    let call_2024 = Call::fn_like(&empty_2024, CallSite { edition: Edition::E2024, panic_edition: Edition::E2024, ctx: Ctx::root(Edition::E2024) });
    let empty_2015 = TokenStream::new_in(alloc);
    let call_2015 = Call::fn_like(&empty_2015, CallSite { edition: Edition::E2015, panic_edition: Edition::E2015, ctx: Ctx::root(Edition::E2015) });

    // 1. E2015 transparent proc-macro emitting `async try gen` into an E2024 caller:
    //    `ParserWords` classifies all three as `IDENT` (not edition-2018/2024 keywords).
    let proc_2015 = MacroDefinition::proc_macro(
        CrateItemKey(ContentId([0x71; 32])),
        ProcKind::FnLike,
        "emit",
        &[],
        DefAt { edition: Edition::E2015, ..at(0x71) },
        alloc,
    )
    .expect("proc_2015");
    let exp_2015 = done(expand(&proc_2015, &call_2024, &EmitText(Edition::E2015, "async try gen"), alloc));
    let bw_2015 = body_words(&exp_2015.tokens, ContentId([0x81; 32]), alloc);
    let toks_2015 = parser::Tokens::new(&bw_2015.parser.words, true);
    assert_eq!(exp_2015.tokens.edition_of(exp_2015.tokens.tokens()[0].ctx), Edition::E2015);
    assert_eq!((toks_2015.kind(0), toks_2015.kind(1), toks_2015.kind(2)), (SyntaxKind::IDENT, SyntaxKind::IDENT, SyntaxKind::IDENT));

    // 2. E2021 transparent proc-macro emitting `async gen` into an E2024 caller:
    //    `async` is `ASYNC_KW` (E2021) and `gen` is `IDENT` (not E2024 `GEN_KW`).
    let proc_2021 = MacroDefinition::proc_macro(
        CrateItemKey(ContentId([0x72; 32])),
        ProcKind::FnLike,
        "emit",
        &[],
        DefAt { edition: Edition::E2021, ..at(0x72) },
        alloc,
    )
    .expect("proc_2021");
    let exp_2021 = done(expand(&proc_2021, &call_2024, &EmitText(Edition::E2021, "async gen"), alloc));
    let bw_2021 = body_words(&exp_2021.tokens, ContentId([0x82; 32]), alloc);
    let toks_2021 = parser::Tokens::new(&bw_2021.parser.words, true);
    assert_eq!((toks_2021.kind(0), toks_2021.kind(1)), (SyntaxKind::ASYNC_KW, SyntaxKind::IDENT));

    // 3. Negative control: E2024 transparent proc-macro emitting `async try gen` into E2015 caller:
    //    `ParserWords` classifies all three as keywords (`ASYNC_KW`, `TRY_KW`, `GEN_KW`).
    let proc_2024 = MacroDefinition::proc_macro(
        CrateItemKey(ContentId([0x73; 32])),
        ProcKind::FnLike,
        "emit",
        &[],
        DefAt { edition: Edition::E2024, ..at(0x73) },
        alloc,
    )
    .expect("proc_2024");
    let exp_2024 = done(expand(&proc_2024, &call_2015, &EmitText(Edition::E2024, "async try gen"), alloc));
    let bw_2024 = body_words(&exp_2024.tokens, ContentId([0x83; 32]), alloc);
    let toks_2024 = parser::Tokens::new(&bw_2024.parser.words, true);
    assert_eq!((toks_2024.kind(0), toks_2024.kind(1), toks_2024.kind(2)), (SyntaxKind::ASYNC_KW, SyntaxKind::TRY_KW, SyntaxKind::GEN_KW));

    // 4. Negative control (`include!`): `include!` defined in E2015 invoked from an E2021 caller
    //    parses `async try` under the caller's E2021 (`ASYNC_KW`, `TRY_KW`), NOT `include!`'s E2015.
    let cx = TestCx { files: &[("kw.rs", b"async try")], ..TestCx::new() };
    let inc_2015 = MacroDefinition::builtin_fn(BuiltinFn::Include, DefAt { edition: Edition::E2015, ..at(0x74) }, alloc);
    let inc_in = lex("\"kw.rs\"", Edition::E2021, alloc).expect("lex");
    let inc_call = Call::fn_like(&inc_in, CallSite { edition: Edition::E2021, panic_edition: Edition::E2021, ctx: Ctx::root(Edition::E2021) });
    let inc_exp = done(expand(&inc_2015, &inc_call, &cx, alloc));
    assert!(inc_exp.diagnostics.is_empty(), "{:?}", inc_exp.diagnostics);
    let bw_inc = body_words(&inc_exp.tokens, ContentId([0x84; 32]), alloc);
    let toks_inc = parser::Tokens::new(&bw_inc.parser.words, true);
    assert_eq!((toks_inc.kind(0), toks_inc.kind(1)), (SyntaxKind::ASYNC_KW, SyntaxKind::TRY_KW));
}

#[test]
fn local_inner_mark_stamps_def_tokens_and_survives_metavar_forwarding_and_records() {
    use blaze_expand::{OriginMaps, SourceIndexes, body_words};

    let alloc = blaze_own::testing::heap().alloc();
    let dep_krate = CrateItemKey(ContentId([0x41; 32]));
    let outer_def = MacroDefinition::rules(
        lex("($arg:expr) => { wrapper!(inner_code!($arg)) };", Edition::E2021, alloc).expect("lexes"),
        DefAt {
            site: DefSite { krate: dep_krate, module: None },
            edition: Edition::E2021,
            export: Export::Exported,
            local_inner: LocalInner::Yes,
            transparency: None,
        },
    );
    let wrapper_def = MacroDefinition::rules(
        lex("($e:expr) => { $e };", Edition::E2021, alloc).expect("lexes"),
        DefAt {
            site: DefSite { krate: dep_krate, module: None },
            edition: Edition::E2021,
            export: Export::Exported,
            local_inner: LocalInner::No,
            transparency: None,
        },
    );

    let caller_input = lex("caller_mac!()", Edition::E2021, alloc).expect("lexes");
    let outer_call = Call::fn_like(&caller_input, site());
    let outer_exp = done(expand(&outer_def, &outer_call, &TestCx::new(), alloc));
    assert!(outer_exp.diagnostics.is_empty(), "{:?}", outer_exp.diagnostics);

    // Record round-trip preserves `local_inner` on `Mark` (both stream record and expansion record).
    let stream_rec = outer_exp.tokens.write_record(alloc);
    let stream_back = TokenStream::read_record(&stream_rec, alloc).expect("stream record reads");
    assert_eq!(stream_back, outer_exp.tokens);
    let sources = SourceIndexes::of(&outer_def, &outer_call, alloc);
    let exp_rec = outer_exp.write_record(&sources.maps(), alloc).expect("expansion record writes");
    let exp_back = Expansion::read_record(&exp_rec, alloc).expect("expansion record reads");
    assert_eq!(exp_back, outer_exp);
    let _ = OriginMaps::NONE;

    // `wrapper` and `inner_code` come from `outer_def` (`TokenOrigin::Def`) -> `local_inner: true`.
    // `caller_mac` comes from `caller_input` (`TokenOrigin::Input`) -> unmarked (`None`).
    let find_ident = |stream: &TokenStream<'_>, name: &str| {
        (0..stream.len())
            .find(|&i| stream.text_at(i) == name)
            .unwrap_or_else(|| panic!("token `{name}` not found"))
    };
    let w_idx = find_ident(&outer_exp.tokens, "wrapper");
    let ic_idx = find_ident(&outer_exp.tokens, "inner_code");
    let cm_idx = find_ident(&outer_exp.tokens, "caller_mac");
    assert_eq!(outer_exp.non_transparent_mark(w_idx).map(|m| (m.site.krate, m.local_inner)), Some((dep_krate, true)));
    assert_eq!(outer_exp.non_transparent_mark(ic_idx).map(|m| (m.site.krate, m.local_inner)), Some((dep_krate, true)));
    assert_eq!(outer_exp.non_transparent_mark(cm_idx), None, "caller argument has no local_inner mark");

    // Forward `inner_code!(caller_mac!())` through non-`local_inner` `wrapper!`:
    // `inner_code` MUST keep `local_inner: true, site.krate == dep_krate`, while `caller_mac` stays unmarked!
    let mut sealed = outer_exp.tokens.clone_in(alloc);
    sealed.seal(ContentId([0x99; 32]));
    let open = (0..sealed.len()).find(|&i| matches!(sealed.tokens()[i].kind, TokenKind::Open(Delim::Paren))).expect("open");
    let close = sealed.close_of(open).expect("close");
    let wrapper_args = sealed.copy_in(open + 1..close, alloc).expect("copy args");
    let wrapper_exp = done(expand(&wrapper_def, &Call::fn_like(&wrapper_args, site()), &TestCx::new(), alloc));
    assert!(wrapper_exp.diagnostics.is_empty(), "{:?}", wrapper_exp.diagnostics);
    let ic2 = find_ident(&wrapper_exp.tokens, "inner_code");
    let cm2 = find_ident(&wrapper_exp.tokens, "caller_mac");
    assert_eq!(wrapper_exp.non_transparent_mark(ic2).map(|m| (m.site.krate, m.local_inner)), Some((dep_krate, true)));
    assert_eq!(wrapper_exp.non_transparent_mark(cm2), None);

    // `body_words` carries `local_inner: true` onto `BodyWord::mark` for `inner_code`.
    let bw = body_words(&wrapper_exp.tokens, ContentId([0x77; 32]), alloc);
    let ic_word = bw.words.iter().find(|w| &bw.text[w.text.0 as usize..w.text.1 as usize] == "inner_code").expect("inner_code word");
    assert_eq!(ic_word.mark.map(|m| (m.site.krate, m.local_inner)), Some((dep_krate, true)));
}

#[test]
fn call_site_ctx_from_derive_attr_fn_like_and_call_key_hygiene() {
    use blaze_expand::{
        ItemAttrs, NextAttr, ProcCall, ProcKind, Produced, Reads, Transparency, body_words,
        call_key, local_hygiene,
    };
    use blaze_own::AVec;

    struct EmitHost(Transparency);
    impl ExpandCx for EmitHost {
        fn cfg(&self, _: &TokenStream<'_>) -> Result<bool, HostError> {
            Err(HostError { ask: HostAsk::Cfg, why: HostWhy::Refused })
        }
        fn env(&self, _: &str) -> Result<Option<&str>, HostError> {
            Err(HostError { ask: HostAsk::Env, why: HostWhy::Refused })
        }
        fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
            Err(HostError { ask: HostAsk::Include, why: HostWhy::Refused })
        }
        fn place(&self, _: PlaceOf) -> Result<Place<'_>, HostError> {
            Err(HostError { ask: HostAsk::Place, why: HostWhy::Refused })
        }
        fn run_proc_macro<'a>(
            &self,
            _: &MacroDefinition<'_>,
            call: ProcCall<'_>,
            alloc: Alloc<'a>,
        ) -> Result<Produced<'a>, HostError> {
            // Find `host` in input to echo as `TokenOrigin::Input`, and synthesize a second
            // `host` with `self.0` (`Transparent` = call_site, `SemiOpaque` = mixed_site,
            // `Opaque` = def_site).
            let host_at = (0..call.input.len())
                .find(|&i| call.input.text_at(i) == "host")
                .map(|i| u32::try_from(i).expect("short"));
            let mut out = TokenStream::new_in(alloc);
            let mut origins = AVec::new_in(alloc);
            if let Some(at) = host_at {
                out.ident("host", Ctx::root(Edition::E2021)).expect("push");
                origins.push(TokenOrigin::Input(at));
            }
            out.ident("host", Ctx::root(Edition::E2021)).expect("push");
            origins.push(TokenOrigin::Made(self.0));
            Ok(Produced {
                tokens: out,
                origins,
                reads: Reads::new_in(alloc),
                diagnostics: AVec::new_in(alloc),
                text: AVec::new_in(alloc),
            })
        }
    }

    let alloc = blaze_own::testing::heap().alloc();
    let outer_krate = CrateItemKey(ContentId([0xA1; 32]));
    let inner_krate = CrateItemKey(ContentId([0xA2; 32]));
    let proc_krate = CrateItemKey(ContentId([0xA3; 32]));

    // 1. Nested `outer!()` -> `inner!()` -> `#[derive(Snafu)] enum E { A { host: u32 } }`:
    //    `Derives::call_site` imports the full parent chain into `derives.input`, so a
    //    `Transparent` (`Span::call_site()`) token emitted by the derive matches `local_hygiene`
    //    and `text_mark` of the input `host` token, while `Opaque` (`def_site`) and `SemiOpaque`
    //    (`mixed_site`) stay hygienically isolated!
    let inner_def = MacroDefinition::rules(
        lex("() => { #[derive(Snafu)] enum E { A { host: u32 } } };", Edition::E2024, alloc).expect("lex"),
        DefAt { site: DefSite { krate: inner_krate, module: None }, edition: Edition::E2024, ..at(0xA2) },
    );
    let outer_def = MacroDefinition::rules(
        lex("() => { inner!(); };", Edition::E2021, alloc).expect("lex"),
        DefAt { site: DefSite { krate: outer_krate, module: None }, edition: Edition::E2021, ..at(0xA1) },
    );
    let outer_exp = done(expand(&outer_def, &Call::fn_like(&TokenStream::new_in(alloc), CallSite::root(Edition::E2024)), &TestCx::new(), alloc));
    let mut outer_sealed = outer_exp.tokens.clone_in(alloc);
    let outer_key = ContentId([0xB1; 32]);
    outer_sealed.seal(outer_key);

    // Extract `inner!()` call site via `CallSite::before_open`.
    let open = (0..outer_sealed.len()).find(|&i| matches!(outer_sealed.tokens()[i].kind, TokenKind::Open(Delim::Paren))).expect("open");
    let close = outer_sealed.close_of(open).expect("close");
    let mut inner_args = outer_sealed.copy_in(open + 1..close, alloc).expect("copy args");
    let inner_site = CallSite::before_open(&outer_sealed, open, &mut inner_args, Edition::E2024).expect("before_open");
    assert_eq!(inner_site.edition, Edition::E2021, "fn-like call site takes path token's edition");
    assert_eq!(inner_args.non_transparent_mark(inner_site.ctx).map(|m| m.site.krate), Some(outer_krate));

    let inner_exp = done(expand(&inner_def, &Call::fn_like(&inner_args, inner_site), &TestCx::new(), alloc));
    let mut inner_sealed = inner_exp.tokens.clone_in(alloc);
    let inner_key = ContentId([0xB2; 32]);
    inner_sealed.seal(inner_key);

    let mut derives = ItemAttrs::new(inner_sealed)
        .derives(&mut |_, _| Ok::<bool, ()>(true))
        .expect("derives")
        .expect("item stays");
    let derive_site = derives.call_site(0, Edition::E2021).expect("derive call_site");
    assert_eq!(derive_site.edition, Edition::E2024, "derive call site takes derive path token's edition");
    assert_eq!(derives.input.non_transparent_mark(derive_site.ctx).map(|m| m.site.krate), Some(inner_krate));

    let derive_def = MacroDefinition::proc_macro(
        proc_krate,
        ProcKind::Derive,
        "Snafu",
        &[],
        DefAt { site: DefSite { krate: proc_krate, module: None }, edition: Edition::E2021, ..at(0xA3) },
        alloc,
    )
    .expect("proc derive");

    let call_call_site = Call::derive(&derives.input, DeriveFacts::PLAIN, derive_site);
    let this_key = ContentId([0xB3; 32]);

    // Transparent (`Span::call_site()`): matches `Input` `host` in both `local_hygiene` and `TextMark::local_hygiene`!
    let exp_trans = done(expand(&derive_def, &call_call_site, &EmitHost(Transparency::Transparent), alloc));
    let h_input = local_hygiene(&exp_trans.tokens, exp_trans.tokens.tokens()[0].ctx, this_key);
    let h_made = local_hygiene(&exp_trans.tokens, exp_trans.tokens.tokens()[1].ctx, this_key);
    assert_ne!(h_input, 0);
    assert_eq!(h_made, h_input, "Span::call_site() must share local_hygiene with enclosing macro_rules! input token");
    assert_eq!(
        exp_trans.text_mark_of(1, this_key).expect("made mark").local_hygiene(),
        exp_trans.text_mark_of(0, this_key).expect("input mark").local_hygiene(),
        "Span::call_site() TextMark::local_hygiene must match enclosing macro_rules! input token"
    );
    let bw = body_words(&exp_trans.tokens, this_key, alloc);
    assert_eq!(bw.words[0].hygiene, bw.words[1].hygiene);

    // Negative controls: `Opaque` (`def_site`) and `SemiOpaque` (`mixed_site`) MUST NOT match!
    for neg in [Transparency::Opaque, Transparency::SemiOpaque] {
        let exp_neg = done(expand(&derive_def, &call_call_site, &EmitHost(neg), alloc));
        let h_in = local_hygiene(&exp_neg.tokens, exp_neg.tokens.tokens()[0].ctx, this_key);
        let h_out = local_hygiene(&exp_neg.tokens, exp_neg.tokens.tokens()[1].ctx, this_key);
        assert_ne!(h_out, h_in, "{neg:?} proc-macro token must stay hygienically isolated from input `host`");
        assert_ne!(
            exp_neg.text_mark_of(1, this_key).expect("neg mark").local_hygiene(),
            exp_neg.text_mark_of(0, this_key).expect("input mark").local_hygiene(),
            "{neg:?} TextMark::local_hygiene must stay isolated from input `host`"
        );
    }

    // 2. Attribute macro (`ItemAttrs::attr_call`): imports attribute path ctx into `made.item`.
    let attr_emitter = MacroDefinition::rules(
        lex("() => { #[my_attr] fn f() { let host = 1; } };", Edition::E2021, alloc).expect("lex"),
        DefAt { site: DefSite { krate: inner_krate, module: None }, edition: Edition::E2021, ..at(0xA2) },
    );
    let attr_exp = done(expand(&attr_emitter, &Call::fn_like(&TokenStream::new_in(alloc), CallSite::root(Edition::E2024)), &TestCx::new(), alloc));
    let mut attr_sealed = attr_exp.tokens.clone_in(alloc);
    attr_sealed.seal(ContentId([0xB4; 32]));
    let mut walk = ItemAttrs::new(attr_sealed);
    let NextAttr::Call { attr, .. } = walk.next(&mut |_, _| Ok::<bool, ()>(true)).expect("next") else {
        panic!("expected attr call");
    };
    let made = walk.attr_call(attr).expect("attr_call");
    let attr_site = made.call_site(Edition::E2024);
    assert_eq!(attr_site.edition, Edition::E2021);
    assert_eq!(made.item.non_transparent_mark(attr_site.ctx).map(|m| m.site.krate), Some(inner_krate));

    // 3. Key hygiene (Rule 21): two calls with identical token text & input stream but different
    //    `CallSite.ctx` produce distinct `call_key`s!
    let mut shared_input = lex("host", Edition::E2021, alloc).expect("lex");
    let mark_a = blaze_expand::Mark {
        of: MarkOf::Outer(ContentId([0xC1; 32])),
        transparency: Transparency::SemiOpaque,
        edition: Edition::E2021,
        site: DefSite { krate: outer_krate, module: None },
        local_inner: false,
    };
    let mark_b = blaze_expand::Mark {
        of: MarkOf::Outer(ContentId([0xC2; 32])),
        transparency: Transparency::SemiOpaque,
        edition: Edition::E2021,
        site: DefSite { krate: inner_krate, module: None },
        local_inner: false,
    };
    let ctx_a = shared_input.context(blaze_expand::CtxNode { parent: Ctx::root(Edition::E2021), mark: mark_a }).expect("ctx_a");
    let ctx_b = shared_input.context(blaze_expand::CtxNode { parent: Ctx::root(Edition::E2021), mark: mark_b }).expect("ctx_b");
    let fn_proc = MacroDefinition::proc_macro(
        proc_krate,
        ProcKind::FnLike,
        "emit",
        &[],
        DefAt { site: DefSite { krate: proc_krate, module: None }, edition: Edition::E2021, ..at(0xA3) },
        alloc,
    )
    .expect("fn proc");
    let key_root = call_key(&fn_proc, &Call::fn_like(&shared_input, CallSite::root(Edition::E2021)));
    let key_a = call_key(&fn_proc, &Call::fn_like(&shared_input, CallSite { edition: Edition::E2021, panic_edition: Edition::E2021, ctx: ctx_a }));
    let key_b = call_key(&fn_proc, &Call::fn_like(&shared_input, CallSite { edition: Edition::E2021, panic_edition: Edition::E2021, ctx: ctx_b }));
    assert_ne!(key_root, key_a, "root CallSite.ctx vs outer mark A must yield distinct call_key");
    assert_ne!(key_a, key_b, "outer mark A vs outer mark B must yield distinct call_key");

    // 4. `Call::check` (Rule 74): refuses a `CallSite.ctx` that does not belong to `Call::input()`.
    let empty_input = TokenStream::new_in(alloc);
    let bad_call = Call::fn_like(&empty_input, CallSite { edition: Edition::E2021, panic_edition: Edition::E2021, ctx: ctx_a });
    assert!(
        matches!(
            expand(&fn_proc, &bad_call, &EmitHost(Transparency::Transparent), alloc),
            Err(blaze_expand::ExpandError::Stream { stream: Some(blaze_expand::Stream::Input), error: blaze_expand::StreamError::NoSuchContext { .. } })
        ),
        "Call::check must refuse a CallSite.ctx not in Call::input()"
    );
}

#[test]
fn recursive_macro_rules_text_mark_preserves_distinct_local_hygiene() {
    use blaze_expand::{call_key, lex_marked, local_hygiene};

    let alloc = blaze_own::testing::heap().alloc();
    let varint_def = MacroDefinition::rules(
        lex(
            "($ty:ty, $proto_ty:ident) => ( \
                 varint!($ty, $proto_ty, from_uint64(value) { value as $ty }); \
             ); \
             ($ty:ty, $proto_ty:ident, from_uint64($from_val:ident) $from_expr:expr) => ( \
                 pub fn merge(value: &mut $ty) { let $from_val = 42u64; *value = $from_expr; } \
             );",
            Edition::E2021,
            alloc,
        )
        .expect("varint def lexes"),
        DefAt {
            site: DefSite { krate: CrateItemKey(ContentId([0x55; 32])), module: Some(ItemKey(ContentId([0x56; 32]))) },
            edition: Edition::E2021,
            export: Export::Local,
            local_inner: LocalInner::No,
            transparency: None,
        },
    );

    // Expansion 1: `varint!(i32, int32)` matches Arm 1 and emits Arm 2 invocation.
    let call1_input = lex("i32, int32", Edition::E2021, alloc).expect("call1 input");
    let call1 = Call::fn_like(&call1_input, CallSite::root(Edition::E2021));
    let key1 = call_key(&varint_def, &call1).0;
    let exp1 = done(expand(&varint_def, &call1, &TestCx::new(), alloc));
    assert!(exp1.diagnostics.is_empty(), "{:?}", exp1.diagnostics);
    let mut sealed1 = exp1.tokens.clone_in(alloc);
    sealed1.seal(key1);

    // Expansion 2: extract `varint!(...)` args from Expansion 1 and expand Arm 2.
    let open = (0..sealed1.len()).find(|&i| matches!(sealed1.tokens()[i].kind, TokenKind::Open(Delim::Paren))).expect("open");
    let close = sealed1.close_of(open).expect("close");
    let mut call2_input = sealed1.copy_in(open + 1..close, alloc).expect("copy args");
    let call2_site = CallSite::before_open(&sealed1, open, &mut call2_input, Edition::E2021).expect("before_open");
    let call2 = Call::fn_like(&call2_input, call2_site);
    let key2 = call_key(&varint_def, &call2).0;
    assert_ne!(key1, key2);
    let exp2 = done(expand(&varint_def, &call2, &TestCx::new(), alloc));
    assert!(exp2.diagnostics.is_empty(), "{:?}", exp2.diagnostics);

    // Locate the 4 occurrences of `value` in Arm 2's output:
    //   [0] `value: &mut i32` (Arm 2 template)
    //   [1] `let value = 42u64;` (Arm 1 template via `$from_val`)
    //   [2] `*value` (Arm 2 template)
    //   [3] `value as i32` (Arm 1 template via `$from_expr`)
    let mut value_positions = blaze_own::Vec::new_in(alloc);
    value_positions.extend((0..exp2.tokens.len()).filter(|&i| exp2.tokens.text_at(i) == "value"));
    assert_eq!(value_positions.len(), 4, "expected 4 `value` tokens, got {value_positions:?}");

    let m_param = exp2.text_mark_of(value_positions[0], key2).expect("param mark");
    let m_let = exp2.text_mark_of(value_positions[1], key2).expect("let mark");
    let m_deref = exp2.text_mark_of(value_positions[2], key2).expect("deref mark");
    let m_cast = exp2.text_mark_of(value_positions[3], key2).expect("cast mark");

    // Same macro definition => identical `(site, transparency, edition, local_inner)`.
    assert_eq!((m_param.site, m_param.transparency, m_param.edition, m_param.local_inner), (m_let.site, m_let.transparency, m_let.edition, m_let.local_inner));
    // Arm 2 tokens match each other; Arm 1 tokens match each other; Arm 1 != Arm 2!
    assert_eq!(m_param, m_deref);
    assert_eq!(m_let, m_cast);
    assert_ne!(m_param.hygiene, m_let.hygiene, "Arm 1 and Arm 2 must carry distinct hygiene");
    assert_ne!(m_param.as_outer_mark().of, m_let.as_outer_mark().of);
    assert_ne!(m_param.local_hygiene(), m_let.local_hygiene());
    assert_eq!(m_param.local_hygiene(), m_deref.local_hygiene());
    assert_eq!(m_let.local_hygiene(), m_cast.local_hygiene());

    // `lex_marked` round-trip preserves exact bit-for-bit `body::local_hygiene == TextMark::local_hygiene()`.
    let relexed_param = lex_marked("value", Edition::E2021, &|_| Some(m_param), alloc).expect("relex param");
    let relexed_let = lex_marked("value", Edition::E2021, &|_| Some(m_let), alloc).expect("relex let");
    let h_relex_param = local_hygiene(&relexed_param, relexed_param.tokens()[0].ctx, ContentId([0x77; 32]));
    let h_relex_let = local_hygiene(&relexed_let, relexed_let.tokens()[0].ctx, ContentId([0x77; 32]));
    assert_eq!(h_relex_param, m_param.local_hygiene());
    assert_eq!(h_relex_let, m_let.local_hygiene());
    assert_ne!(h_relex_param, h_relex_let);
}

#[test]
fn doc_comment_desugars_to_str_raw_with_minimal_hashes() {
    // Doc-comment provenance on `LitKind` must add 0 bytes to `LitKind`, `TokenKind`, and `Token`.
    assert_eq!(core::mem::size_of::<LitKind>(), 2);
    assert_eq!(core::mem::size_of::<TokenKind>(), 8);
    assert_eq!(core::mem::size_of::<blaze_expand::Token>(), 20);

    let alloc = blaze_own::testing::heap().alloc();
    let assert_doc_lit = |src: &str, inner: bool, want_kind: LitKind, want_body: Option<&str>| {
        let stream = lex(src, Edition::E2021, alloc).expect("doc comment lexes");
        let idx = if inner { 5 } else { 4 };
        let tok = stream.tokens()[idx];
        let TokenKind::Literal { kind, suffix_len, .. } = tok.kind else {
            panic!("expected literal at token {idx} for `{src:?}`, got {:?}", tok.kind);
        };
        assert_eq!((kind, suffix_len), (want_kind, 0), "literal kind for `{src:?}`");
        if let Some(body) = want_body {
            assert_eq!(stream.text_at(idx), body, "literal body for `{src:?}`");
        }
        // Every desugared doc comment must round-trip through source rendering back to the exact
        // same raw/cooked literal kind (without doc provenance) and body text.
        let mut rendered = blaze_own::AString::new_in(alloc);
        blaze_expand::write_tokens(&stream, &mut rendered);
        let relexed = lex(rendered.as_str(), Edition::E2021, alloc).expect("rendered doc comment re-lexes");
        assert_eq!(relexed.tokens().len(), stream.tokens().len(), "round-trip token count for `{src:?}`");
        let TokenKind::Literal { kind: rk, suffix_len: rsl, .. } = relexed.tokens()[idx].kind else {
            panic!("expected literal after round-trip for `{src:?}`");
        };
        assert_eq!((rk, rsl), (kind.without_doc_provenance(), 0), "round-trip literal kind for `{src:?}`");
        assert_eq!(relexed.text_at(idx), stream.text_at(idx), "round-trip literal body for `{src:?}`");
    };

    assert_doc_lit("/// attr\n", false, LitKind::DocStrRaw(0), Some(" attr"));
    assert_doc_lit("///\n", false, LitKind::DocStrRaw(0), Some(""));
    assert_doc_lit("//! inner\n", true, LitKind::DocStrRaw(0), Some(" inner"));
    assert_doc_lit("/** block */", false, LitKind::DocStrRaw(0), Some(" block "));
    assert_doc_lit("/*! inner block */", true, LitKind::DocStrRaw(0), Some(" inner block "));
    assert_doc_lit("/// has \"quote\"\n", false, LitKind::DocStrRaw(1), Some(" has \"quote\""));
    assert_doc_lit("/// trailing \"\n", false, LitKind::DocStrRaw(1), Some(" trailing \""));
    assert_doc_lit("/// has \"#\n", false, LitKind::DocStrRaw(2), Some(" has \"#"));
    assert_doc_lit("/// a\"###b\n", false, LitKind::DocStrRaw(4), Some(" a\"###b"));
    assert_doc_lit("/// \"\"#\n", false, LitKind::DocStrRaw(2), Some(" \"\"#"));
    assert_doc_lit("/// has \\n backslash\n", false, LitKind::DocStrRaw(0), Some(" has \\n backslash"));
    assert_doc_lit("/** a\r\nb */", false, LitKind::DocStrRaw(0), Some(" a\nb "));

    // 254 hashes after `"` needs 255 hashes (the u8 maximum for raw strings); 255+ hashes after `"`
    // saturates at `DocStrRaw(255)` (preserving the raw literal kind, matching `rustc_ast/src/tokenstream.rs`).
    let mut hashes_254 = blaze_own::AString::new_in(alloc);
    hashes_254.push_str("/// \"");
    for _ in 0..254 {
        hashes_254.push('#');
    }
    hashes_254.push('\n');
    let mut want_254 = blaze_own::AString::new_in(alloc);
    want_254.push_str(" \"");
    for _ in 0..254 {
        want_254.push('#');
    }
    assert_doc_lit(hashes_254.as_str(), false, LitKind::DocStrRaw(255), Some(want_254.as_str()));

    for n in [255usize, 256] {
        let mut src = blaze_own::AString::new_in(alloc);
        src.push_str("/// \"");
        for _ in 0..n {
            src.push('#');
        }
        src.push('\n');
        let mut want = blaze_own::AString::new_in(alloc);
        want.push_str(" \"");
        for _ in 0..n {
            want.push('#');
        }
        let stream = lex(src.as_str(), Edition::E2021, alloc).expect("255+ hashes doc lexes");
        let tok = stream.tokens()[4];
        assert_eq!(
            tok.kind,
            TokenKind::Literal { kind: LitKind::DocStrRaw(255), suffix_len: 0, spacing: Spacing::Alone },
            "{n} hashes after `\"` must saturate at DocStrRaw(255)"
        );
        assert_eq!(stream.text_at(4), want.as_str());
        // Forwarding through `macro_rules!` must preserve raw `StrRaw(255)`, never cooked `Str`.
        let forward = rules("($($t:tt)*) => { $($t)* };", 1);
        let forwarded = call_fn(&forward, src.as_str(), &TestCx::new());
        assert_eq!(
            forwarded.tokens.tokens()[4].kind,
            TokenKind::Literal { kind: LitKind::StrRaw(255), suffix_len: 0, spacing: Spacing::Alone },
            "forwarded {n}-hash doc comment must remain StrRaw(255)"
        );
        assert_eq!(forwarded.tokens.text_at(4), want.as_str());
    }

    // Bare `\r` (not followed by `\n`) in a doc comment is refused by the lexer, matching rustc.
    assert!(lex("/** bare\rcr */", Edition::E2021, alloc).is_err());
}

#[test]
fn previous_expansion_schema_4_is_refused() {
    use blaze_format::{Part, Record, Slice, Writer, WriterOpts, schema_of};
    let alloc = blaze_own::testing::heap().alloc();
    let v4 = schema_of(&[
        Part::Str("blaze-expand Expansion"),
        Part::U64(4),
        Part::Schema(<[Slice<u8>; 15] as Record>::SCHEMA),
    ]);
    assert_ne!(v4, blaze_expand::EXPANSION_SCHEMA);
    let mut w = Writer::new(WriterOpts { schema: v4, alloc, capacity: None });
    let root: [Slice<u8>; 15] = [Slice::EMPTY; 15];
    let at = w.push(&root);
    let bytes = w.finish(at);
    let aligned = Aligned::copy_from(&bytes);
    assert!(matches!(Expansion::<'static>::read_record(&aligned, alloc), Err(blaze_expand::RecordError::Format(_))));
}

#[test]
fn previous_stream_schema_2_is_refused() {
    use blaze_format::{Part, Record, Slice, Writer, WriterOpts, schema_of};
    let alloc = blaze_own::testing::heap().alloc();
    let v2 = schema_of(&[
        Part::Str("blaze-expand TokenStream"),
        Part::U64(2),
        Part::Schema(<[Slice<u8>; 14] as Record>::SCHEMA),
    ]);
    assert_ne!(v2, blaze_expand::STREAM_SCHEMA);
    let mut w = Writer::new(WriterOpts { schema: v2, alloc, capacity: None });
    let root: [Slice<u8>; 14] = [Slice::EMPTY; 14];
    let at = w.push(&root);
    let bytes = w.finish(at);
    let aligned = Aligned::copy_from(&bytes);
    assert!(matches!(TokenStream::read_record(&aligned, alloc), Err(blaze_expand::RecordError::Format(_))));
}

/// Exact `ctor-1.0.13` pattern (`__parse_feature_input!` in `ctor-1.0.13/src/macros/features.rs`):
/// doc comments desugar to `#[doc = r"..."]` (`LitKind::StrRaw(n)`), so `#[doc = r" attr"]`,
/// `#[doc = r" crate"]`, `#[doc = r#" has "quote""#]`, and `#[doc = $s:literal]` all match as in
/// rustc, while mismatched text, leading spaces, hash counts, and cooked `LitKind::Str` do not.
#[test]
fn doc_comment_matches_raw_doc_attr_in_macro_rules() {
    let cx = TestCx::new();

    // ctor-1.0.13 optional `/// crate` and `/// attr` sections followed by `$doc:literal`.
    let ctor_def = rules(
        "(
            $( #[doc = r\" crate\"] $( #[doc = $doc_crate:literal] )* )?
            feature: $feat:literal;
            $( #[doc = r\" attr\"] $( #[doc = $doc_attr:literal] )* )?
            attr: $attr:ident;
        ) => {
            const FEAT: &str = $feat;
            const ATTR_DOCS: &[&str] = &[ $( $( $doc_attr ),* )? ];
            const CRATE_DOCS: &[&str] = &[ $( $( $doc_crate ),* )? ];
            struct $attr;
        }",
        1,
    );
    let exp = call_fn(
        &ctor_def,
        "/// crate\n/// crate doc\nfeature: \"my_feat\";\n/// attr\n/// attr doc 1\n/// attr doc 2\nattr: MyAttr;",
        &cx,
    );
    assert!(exp.diagnostics.is_empty(), "{:?}", exp.diagnostics);
    assert_eq!(exp.arm, Some(0));
    assert!(show(&exp.tokens).contains("struct MyAttr ;"), "got: {}", show(&exp.tokens));

    // Quotes, hashes, inner doc comments, `:literal`, and `:expr` capture.
    let raw_match = rules(
        "(#[doc = r#\" has \"quote\"\"#]) => { const Q: i32 = 1; };
         (#[doc = r##\" has \"#\"##]) => { const H: i32 = 2; };
         (#![doc = r\" inner\"]) => { const I: i32 = 3; };
         (#[doc = $s:literal]) => { const L: &str = $s; };",
        1,
    );
    let q = call_fn(&raw_match, "/// has \"quote\"\n", &cx);
    assert_eq!((q.arm, q.diagnostics.is_empty()), (Some(0), true));
    let h = call_fn(&raw_match, "/// has \"#\n", &cx);
    assert_eq!((h.arm, h.diagnostics.is_empty()), (Some(1), true));
    let inner = call_fn(&raw_match, "//! inner\n", &cx);
    assert_eq!((inner.arm, inner.diagnostics.is_empty()), (Some(2), true));
    let fallback = call_fn(&raw_match, "/// other\n", &cx);
    assert_eq!((fallback.arm, fallback.diagnostics.is_empty()), (Some(3), true));

    // Explicit `#[doc = " attr"]` matches explicit cooked string, NOT `/// attr` (rustc semantics).
    let cooked_def = rules("(#[doc = \" attr\"]) => { const COOKED: i32 = 1; };", 1);
    let cooked_ok = call_fn(&cooked_def, "#[doc = \" attr\"]", &cx);
    assert_eq!((cooked_ok.arm, cooked_ok.diagnostics.is_empty()), (Some(0), true));
    let cooked_doc = call_fn(&cooked_def, "/// attr\n", &cx);
    assert_eq!(cooked_doc.arm, None, "cooked #[doc = \" attr\"] must not match /// attr");

    // Negative controls for `#[doc = r" attr"]` and hash-count strictness.
    let attr_def = rules("(#[doc = r\" attr\"]) => { const OK: i32 = 1; };", 1);
    assert_eq!(call_fn(&attr_def, "/// attr\n", &cx).arm, Some(0));
    assert_eq!(call_fn(&attr_def, "/// other\n", &cx).arm, None, "different doc text must not match");
    assert_eq!(call_fn(&attr_def, "///attr\n", &cx).arm, None, "missing leading space must not match");
    assert_eq!(call_fn(&attr_def, "", &cx).arm, None, "explicit #[doc = r\" attr\"] must not match empty");

    let hash_strict = rules("(#[doc = r#\" attr\"#]) => { const OK: i32 = 1; };", 1);
    assert_eq!(call_fn(&hash_strict, "/// attr\n", &cx).arm, None, "StrRaw(1) must not match StrRaw(0)");
}

#[test]
fn doc_comment_in_matcher_still_ignored_for_all_doc_forms() {
    let cx = TestCx::new();
    let def = rules(
        "(
            /// line doc in matcher
            //! inner doc in matcher
            /** block doc in matcher */
            /*! inner block doc in matcher */
            $x:ident
        ) => { fn $x() {} }",
        1,
    );
    let ok = call_fn(&def, "hello", &cx);
    assert_eq!(show(&ok.tokens), "fn hello ( ) { }", "{:?}", ok.diagnostics);
    let with_doc = call_fn(&def, "/// line doc in matcher\nhello", &cx);
    assert_eq!(with_doc.arm, None, "a doc comment written in a matcher must not match a doc comment in the call");

    // Explicitly spaced `# [ doc = r" attr" ]`, `# [ doc = " attr" ]`, and `# ! [ doc = r" attr" ]`
    // in a matcher are NOT doc comments: they stay real matcher tokens (Codex P2 regression test).
    let spaced_raw = rules("(# [ doc = r\" attr\" ] $x:ident) => { fn $x() {} };", 1);
    assert_eq!(call_fn(&spaced_raw, "/// attr\nfoo", &cx).arm, Some(0));
    assert_eq!(call_fn(&spaced_raw, "#[doc = r\" attr\"] foo", &cx).arm, Some(0));
    assert_eq!(call_fn(&spaced_raw, "foo", &cx).arm, None, "explicit spaced #[doc = r\" attr\"] must not be skipped");

    let spaced_cooked = rules("(# [ doc = \" attr\" ] $x:ident) => { fn $x() {} };", 1);
    assert_eq!(call_fn(&spaced_cooked, "#[doc = \" attr\"] foo", &cx).arm, Some(0));
    assert_eq!(call_fn(&spaced_cooked, "/// attr\nfoo", &cx).arm, None);
    assert_eq!(call_fn(&spaced_cooked, "foo", &cx).arm, None, "explicit spaced #[doc = \" attr\"] must not be skipped");

    let spaced_inner = rules("(# ! [ doc = r\" inner\" ] $x:ident) => { fn $x() {} };", 1);
    assert_eq!(call_fn(&spaced_inner, "//! inner\nfoo", &cx).arm, Some(0));
    assert_eq!(call_fn(&spaced_inner, "foo", &cx).arm, None, "explicit spaced #![doc = r\" inner\"] must not be skipped");

    // Forwarding a doc comment through `macro_rules!` strips doc provenance at the declarative
    // input boundary (`LitKind::StrRaw`), whereas a `///` written directly in a template keeps
    // `LitKind::DocStrRaw` (matching rustc).
    let forward = rules("($($t:tt)*) => { $($t)* };", 1);
    let forwarded = call_fn(&forward, "/// forwarded\n", &cx);
    assert!(
        matches!(forwarded.tokens.tokens()[4].kind, TokenKind::Literal { kind: LitKind::StrRaw(0), .. }),
        "forwarded doc comment should become plain StrRaw(0), got {:?}",
        forwarded.tokens.tokens()[4].kind
    );
    let from_template = rules("() => { /// templated\n };", 1);
    let templated = call_fn(&from_template, "", &cx);
    assert!(
        matches!(templated.tokens.tokens()[4].kind, TokenKind::Literal { kind: LitKind::DocStrRaw(0), .. }),
        "template doc comment should keep DocStrRaw(0), got {:?}",
        templated.tokens.tokens()[4].kind
    );
}
