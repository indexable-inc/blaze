//! blaze-expand's interface: `macro_rules!` matching and transcription, hygiene, the keys, the
//! builtins `println!` needs, the derives, the attribute and proc-macro seam, errors as values,
//! nested calls listed, and a control whose planted bug the key oracle must catch.

#![feature(allocator_api)]

use blaze_expand::{
    AlsoCopy, BuiltinDerive, BuiltinFn, Call, CallKind, CallShape, CallSite, Constness, Ctx, DefAt,
    DefSite, Delim, DeriveFacts, Diagnostic, Eager, Edition, ExpandCx, ExpandError, Expanded,
    Expansion, Export, Hole, HoleOf, HostAsk, HostError, HostWhy, IncludeAs, Included, Inside,
    LitKind, LocalInner, MacroDefinition, MarkOf, NestedKind, TokenOrigin, Place, PlaceOf, ProcCall, ProcKind,
    Produced, Raw, ReadAnswer, ReadKind, Reads, Spacing, TokenKind, TokenStream, call_key, expand,
    expansion_key, find_calls, lex,
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
}

impl TestCx {
    fn new() -> TestCx {
        TestCx {
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
        let mut tokens = TokenStream::new_in(alloc);
        let mut origins = OwnVec::new_in(alloc);
        for at in 0..call.input.len() {
            tokens.push_from(call.input, at).map_err(|_| FAILED)?;
            origins.push(TokenOrigin::Input(u32::try_from(at).map_err(|_| FAILED)?));
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

fn tokens(text: &str) -> TokenStream<'static> {
    lex(text, Edition::E2021, Alloc::global()).expect("the test's text lexes")
}

fn rules(body: &str, krate: u8) -> MacroDefinition<'static> {
    MacroDefinition::rules(tokens(body), at(krate))
}

fn builtin(which: BuiltinFn) -> MacroDefinition<'static> {
    MacroDefinition::builtin_fn(which, at(1), Alloc::global())
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
    done(expand(def, &Call::fn_like(&input, site()), cx, Alloc::global()))
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
    let calls = find_calls(&file, Alloc::global()).expect("the file's calls are found");
    let call = calls.iter().find(|it| it.kind == NestedKind::FnLike).expect("one call");
    let input = file.copy_in(call.inner().to_usize(), Alloc::global()).expect("the args copy");
    let call = Call::fn_like(&input, site());
    let exp = done(expand(def, &call, cx, Alloc::global()));
    let mut bytes = OwnVec::new_in(Alloc::global());
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
    let first = expand(&def, &Call::fn_like(&input, site()), &cx, Alloc::global()).expect("expands");
    let Expanded::NeedsExpanded(nested) = first else { panic!("concat! lists its nested call") };
    assert_eq!(nested.len(), 1);
    let inner = input.copy_in(nested[0].inner().to_usize(), Alloc::global()).expect("copies");
    let stringify = builtin(BuiltinFn::Stringify);
    let inner_call = Call {
        kind: CallKind::FnLike { args: &inner },
        site: site(),
        eager: Eager { inside: Inside::EagerInput, answers: &[] },
    };
    let answer = done(expand(&stringify, &inner_call, &cx, Alloc::global()));
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
    assert!(matches!(expand(&def, &refused, &cx, Alloc::global()), Err(ExpandError::Unsealed { .. })));
    let answers = [&sealed];
    let again = Call {
        kind: CallKind::FnLike { args: &input },
        site: site(),
        eager: Eager { inside: Inside::Plain, answers: &answers },
    };
    let exp = done(expand(&def, &again, &cx, Alloc::global()));
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
    let exp = call_fn(&def, "unix => a", &cx);
    assert!(matches!(exp.diagnostics[..], [Diagnostic::Builtin { .. }]));
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
    let exp = done(expand(&builtin(BuiltinFn::Line), &call, &cx, Alloc::global()));
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

fn derive_with(which: BuiltinDerive, item: &str, facts: DeriveFacts) -> Expansion<'static> {
    let cx = TestCx::new();
    let def = MacroDefinition::builtin_derive(which, at(1), Alloc::global());
    let item = tokens(item);
    let call =
        Call { kind: CallKind::Derive { item: &item, facts }, site: site(), eager: Eager::plain() };
    done(expand(&def, &call, &cx, Alloc::global()))
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
    let def = MacroDefinition::builtin_derive(BuiltinDerive::Clone, at(1), Alloc::global());
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
    assert!(lex("\"\\x80\"", Edition::E2021, Alloc::global()).is_err());
    let bad = call_fn(&builtin(BuiltinFn::Concat), "b\"x\"", &cx);
    assert!(matches!(bad.diagnostics[..], [Diagnostic::Builtin { .. }]), "{:?}", bad.diagnostics);
}

#[test]
fn an_attribute_call_takes_its_args_and_item() {
    let cx = TestCx::new();
    let krate = CrateItemKey(ContentId([9; 32]));
    let def =
        MacroDefinition::proc_macro(krate, ProcKind::Attr, "echo", &[], at(9), Alloc::global()).expect("names fit");
    let args = tokens("x");
    let item = tokens("fn f() {}");
    let call = Call {
        kind: CallKind::Attr { args: Some(&args), item: &item },
        site: site(),
        eager: Eager::plain(),
    };
    let exp = done(expand(&def, &call, &cx, Alloc::global()));
    assert_eq!(show(&exp.tokens), "fn f ( ) { }");
    assert!(
        exp.origins
            .iter()
            .enumerate()
            .all(|(at, it)| u32::try_from(at).map(TokenOrigin::Input) == Ok(*it))
    );
    assert_eq!(exp.reads.list()[0].kind, ReadKind::ProcMacro);

    let wrong = expand(&def, &Call::fn_like(&args, site()), &cx, Alloc::global());
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
    ]
}

#[test]
fn an_expansion_record_reads_back_equal_and_rewrites_equal_bytes() {
    for exp in varied_expansions() {
        let bytes = exp.write_record(Alloc::global());
        let read = Expansion::read_record(&bytes, Alloc::global()).expect("a written record reads back");
        assert!(read == exp, "{exp:?} read back as {read:?}");
        assert_eq!(read.write_record(Alloc::global())[..], bytes[..]);
        // A buffer cut short is refused, never read.
        let cut = Expansion::<'static>::read_record(&bytes[..bytes.len() - 1], Alloc::global());
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
    MacroDefinition::from(&view, body.as_deref(), params.as_deref(), Alloc::global())
}

#[test]
fn a_macro_definition_reads_back_from_its_records() {
    let krate = CrateItemKey(ContentId([9; 32]));
    let derive =
        MacroDefinition::proc_macro(krate, ProcKind::Derive, "Serialize", &["serde", "allow"], at(9), Alloc::global())
            .expect("names fit");
    let defs = [
        rules("($x:expr) => { $x + 1 }", 1),
        MacroDefinition::macro2(Some(tokens("$x:expr")), tokens("$x * 2"), at(2)),
        builtin(BuiltinFn::Concat),
        MacroDefinition::builtin_derive(BuiltinDerive::Clone, at(1), Alloc::global()),
        derive,
    ];
    let item = ItemKey(ContentId([5; 32]));
    for def in &defs {
        let name = def.proc_name().unwrap_or("m");
        let records = def.write(item, name, None, Alloc::global()).expect("a small definition");
        let read = read_back(def, &records).expect("the records are one definition");
        assert_eq!(read.content(), def.content());
        assert_eq!((read.kind(), read.at(), read.transparency()), (def.kind(), def.at(), def.transparency()));
        assert!(read.helpers().eq(def.helpers()) && read.proc_name() == def.proc_name());
        // Written again, the records are the same bytes (rule 25).
        let again = read.write(item, name, None, Alloc::global()).expect("a small definition");
        assert_eq!(again.def[..], records.def[..]);
    }
    // Control: a body record of another stream is refused by its digest, never read as the body.
    let mut records = defs[0].write(item, "m", None, Alloc::global()).expect("a small definition");
    let other = tokens("() => {}").write_record(Alloc::global());
    records.body = Some((stream_record_id(&other), other));
    assert!(matches!(read_back(&defs[0], &records), Err(DefinitionError::Mismatch { .. })));
    // A proc macro written under a name that is not its export name is refused.
    let renamed = defs[4].write(item, "Deserialize", None, Alloc::global());
    assert!(matches!(renamed.err(), Some(BuildError::Disagrees { .. })));
}

#[test]
fn an_expansion_record_refuses_a_token_entry_it_does_not_write() {
    let exp = call_fn(&builtin(BuiltinFn::Stringify), "a", &TestCx::new());
    let mut bytes = exp.write_record(Alloc::global());
    // The one token, an unsuffixed string literal standing alone: tag 2, kind 4 at bit 5.
    let word = (2u32 | 4 << 5).to_le_bytes();
    let at = bytes
        .windows(4)
        .position(|it| it == word)
        .expect("the string literal's entry is in the buffer");
    // Tag 7 names no token.
    bytes[at] = 7;
    let refused = Expansion::<'static>::read_record(&bytes, Alloc::global());
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
        let cx = TestCx { env, files: &[], cfgs: &[], line: 1 };
        let exp = done(expand(&def, &call, &cx, Alloc::global()));
        let mut bytes = OwnVec::new_in(Alloc::global());
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
