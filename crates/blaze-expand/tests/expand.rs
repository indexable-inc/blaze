//! blaze-expand's interface: `macro_rules!` matching and transcription, hygiene, the keys, the
//! builtins `println!` needs, the derives, the attribute and proc-macro seam, errors as values,
//! nested calls listed, and a control whose planted bug the key oracle must catch.

#![feature(allocator_api)]

use std::alloc::{Allocator, Global};

use blaze_expand::{
    BuiltinDerive, BuiltinFn, Call, CallKind, CallShape, CallSite, Ctx, DefAt, DefSite, Delim,
    Diagnostic, Eager, Edition, ExpandCx, ExpandError, Expanded, Expansion, Export, Hole, HoleOf,
    HostAsk, HostError, HostWhy, IncludeAs, Included, Inside, LitKind, LocalInner, MacroDef,
    MarkOf, NestedKind, Origin, Place, PlaceOf, ProcCall, ProcKind, Produced, Raw, ReadAnswer,
    ReadKind, Reads, Spacing, TokenKind, TokenStream, call_key, expand, expansion_key, find_calls,
    lex,
};
use blaze_id::{ContentId, CrateItemKey};
use blaze_sim::{Broke, Config, Tape};

/// The outside of the tests' expansions: fixed env vars, files, enabled cfg names and a
/// position, and a proc macro that echoes its input.
struct TestCx {
    env: &'static [(&'static str, &'static str)],
    files: &'static [(&'static str, &'static [u8])],
    cfgs: &'static [&'static str],
    line: u32,
}

impl TestCx {
    fn new() -> TestCx {
        TestCx { env: &[("V", "v")], files: &[("a.txt", b"hi")], cfgs: &["unix"], line: 12 }
    }
}

const FAILED: HostError = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Failed };

impl ExpandCx for TestCx {
    fn env(&self, name: &str) -> Result<Option<&str>, HostError> {
        Ok(self.env.iter().find(|(it, _)| *it == name).map(|(_, value)| *value))
    }

    /// A predicate holds when its first token is an enabled name.
    fn cfg<A: Allocator>(&self, predicate: &TokenStream<A>) -> Result<bool, HostError> {
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

    fn run_proc_macro<A: Allocator + Clone>(
        &self,
        def: &MacroDef<A>,
        call: ProcCall<'_, A>,
        alloc: A,
    ) -> Result<Produced<A>, HostError> {
        let mut tokens = TokenStream::new_in(alloc.clone());
        let mut origins = Vec::new_in(alloc.clone());
        for at in 0..call.input.len() {
            tokens.push_from(call.input, at).map_err(|_| FAILED)?;
            origins.push(Origin::Input(u32::try_from(at).map_err(|_| FAILED)?));
        }
        let mut reads = Reads::new_in(alloc);
        let name = def.proc_name().ok_or(FAILED)?;
        reads.push(ReadKind::ProcMacro, name, ReadAnswer::Bool(true)).map_err(|_| FAILED)?;
        Ok(Produced { tokens, origins, reads })
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

fn tokens(text: &str) -> TokenStream<Global> {
    lex(text, Edition::E2021, Global).expect("the test's text lexes")
}

fn rules(body: &str, krate: u8) -> MacroDef<Global> {
    MacroDef::rules(tokens(body), at(krate))
}

fn builtin(which: BuiltinFn) -> MacroDef<Global> {
    MacroDef::builtin_fn(which, at(1), Global)
}

fn done(expanded: Result<Expanded<Global>, ExpandError>) -> Expansion<Global> {
    match expanded.expect("the call expands") {
        Expanded::Expansion(expansion) => expansion,
        Expanded::NeedsExpanded(nested) => {
            panic!("the call needs {} nested calls first", nested.len())
        }
    }
}

/// Expands `m!(args)` of `def` where it is written.
fn call_fn(def: &MacroDef<Global>, args: &str, cx: &TestCx) -> Expansion<Global> {
    let input = tokens(args);
    done(expand(def, &Call::fn_like(&input, site()), cx, Global))
}

/// The tokens as text, one space between each but between a joint punct and the punct it is
/// joined to (`::` is one operator, `: :` two tokens); invisible groups not shown.
fn show<A: Allocator>(stream: &TokenStream<A>) -> String {
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
    assert_eq!(exp.origins[1], Origin::Input(0));
    assert!(matches!(exp.origins[0], Origin::Def(_)));
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
    def: &MacroDef<Global>,
    source: &str,
    cx: &TestCx,
) -> (Expansion<Global>, Vec<u8, Global>, blaze_expand::ExpansionKey) {
    let file = tokens(source);
    let calls = find_calls(&file, Global).expect("the file's calls are found");
    let call = calls.iter().find(|it| it.kind == NestedKind::FnLike).expect("one call");
    let input = file.copy_in(call.inner().to_usize(), Global).expect("the args copy");
    let call = Call::fn_like(&input, site());
    let exp = done(expand(def, &call, cx, Global));
    let mut bytes = Vec::new_in(Global);
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
    let first = expand(&def, &Call::fn_like(&input, site()), &cx, Global).expect("expands");
    let Expanded::NeedsExpanded(nested) = first else { panic!("concat! lists its nested call") };
    assert_eq!(nested.len(), 1);
    let inner = input.copy_in(nested[0].inner().to_usize(), Global).expect("copies");
    let stringify = builtin(BuiltinFn::Stringify);
    let inner_call = Call {
        kind: CallKind::FnLike { args: &inner },
        site: site(),
        eager: Eager { inside: Inside::EagerInput, answers: &[] },
    };
    let answer = done(expand(&stringify, &inner_call, &cx, Global));
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
    assert!(matches!(expand(&def, &refused, &cx, Global), Err(ExpandError::Unsealed { .. })));
    let answers = [&sealed];
    let again = Call {
        kind: CallKind::FnLike { args: &input },
        site: site(),
        eager: Eager { inside: Inside::Plain, answers: &answers },
    };
    let exp = done(expand(&def, &again, &cx, Global));
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
    let exp = done(expand(&builtin(BuiltinFn::Line), &call, &cx, Global));
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

fn derive(which: BuiltinDerive, item: &str) -> Expansion<Global> {
    let cx = TestCx::new();
    let def = MacroDef::builtin_derive(which, at(1), Global);
    let item = tokens(item);
    let call = Call { kind: CallKind::Derive { item: &item }, site: site(), eager: Eager::plain() };
    done(expand(&def, &call, &cx, Global))
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
    assert_eq!(name.map(|at| clone.origins[at]), Some(Origin::Input(1)));
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
    assert!(matches!(out.map(|at| exp.origins[at]), Some(Origin::Input(_))));
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
    let bad = call_fn(&builtin(BuiltinFn::Concat), "\"\\x80\"", &cx);
    assert!(!bad.diagnostics.is_empty());
}

#[test]
fn an_attribute_call_takes_its_args_and_item() {
    let cx = TestCx::new();
    let krate = CrateItemKey(ContentId([9; 32]));
    let def =
        MacroDef::proc_macro(krate, ProcKind::Attr, "echo", &[], at(9), Global).expect("names fit");
    let args = tokens("x");
    let item = tokens("fn f() {}");
    let call = Call {
        kind: CallKind::Attr { args: Some(&args), item: &item },
        site: site(),
        eager: Eager::plain(),
    };
    let exp = done(expand(&def, &call, &cx, Global));
    assert_eq!(show(&exp.tokens), "fn f ( ) { }");
    assert!(
        exp.origins
            .iter()
            .enumerate()
            .all(|(at, it)| u32::try_from(at).map(Origin::Input) == Ok(*it))
    );
    assert_eq!(exp.reads.list()[0].kind, ReadKind::ProcMacro);

    let wrong = expand(&def, &Call::fn_like(&args, site()), &cx, Global);
    assert!(matches!(
        wrong,
        Err(ExpandError::WrongCallKind { def: CallShape::Attr, call: CallShape::FnLike })
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

#[test]
fn nested_calls_are_listed_not_expanded() {
    let cx = TestCx::new();
    let exp = call_fn(&rules("() => { foo!(1); bar::baz![2]; }", 1), "", &cx);
    assert_eq!(show(&exp.tokens), "foo ! ( 1 ) ; bar :: baz ! [ 2 ] ;");
    assert_eq!(exp.nested.len(), 2);
    assert!(exp.nested.iter().all(|it| it.kind == NestedKind::FnLike));
    assert_eq!((exp.nested[1].path.start, exp.nested[1].path.end), (6, 10));
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
        let cx = TestCx { env, files: &[], cfgs: &[], line: 1 };
        let exp = done(expand(&def, &call, &cx, Global));
        let mut bytes = Vec::new_in(Global);
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
