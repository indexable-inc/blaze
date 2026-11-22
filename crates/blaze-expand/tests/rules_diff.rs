//! The `macro_rules!` engine on the cases Rust's macro rules define, each call's expansion
//! compared token for token (kind, spacing, text, origin, the context's marks), with its
//! diagnostics' words and the arm, against the one mbe's engine gave while it was beside this
//! one (`rules_diff.expected`; the two agreed on every case). The standard library's every call
//! is the `rules_core` example's run against its baseline; here are the cases it may not reach
//! (local ambiguity, no rule matched, a binding at the wrong depth, `$crate`, nested
//! repetitions, the metavariable expressions, the 2021 `expr` rules), the engine's own bound,
//! and planted differences the oracle must catch.

#![feature(allocator_api)]

#[path = "../examples/rules_compare/compare.rs"]
mod compare;

use blaze_expand::{
    Call, CallSite, Ctx, DefAt, DefSite, Edition, ExpandCx, ExpandError, Expanded, Export,
    HostAsk, HostError, HostWhy, IncludeAs, Included, LocalInner, MacroDefinition, Place, PlaceOf,
    ProcCall, Produced, RulesLimit, Spacing, TokenKind, TokenOrigin, TokenStream,
    expand_prepared, lex, prepare,
};
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::Alloc;
use compare::{Difference, compare};

struct NoCx;

impl ExpandCx for NoCx {
    fn env(&self, _: &str) -> Result<Option<&str>, HostError> {
        Ok(None)
    }
    fn cfg(&self, _: &TokenStream<'_>) -> Result<bool, HostError> {
        Ok(false)
    }
    fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Ok(Included::Missing)
    }
    fn place(&self, _: PlaceOf) -> Result<Place<'_>, HostError> {
        Ok(Place::Number(1))
    }
    fn run_proc_macro<'a>(
        &self,
        _: &MacroDefinition<'_>,
        _: ProcCall<'_>,
        _: Alloc<'a>,
    ) -> Result<Produced<'a>, HostError> {
        Err(HostError { ask: HostAsk::ProcMacro, why: HostWhy::Failed })
    }
}

fn tokens(text: &str, edition: Edition) -> TokenStream<'static> {
    lex(text, edition, blaze_own::testing::heap().alloc()).expect("the test's text lexes")
}

fn def(body: &str, edition: Edition) -> MacroDefinition<'static> {
    let at = DefAt {
        site: DefSite { krate: CrateItemKey(ContentId([7; 32])), module: None },
        edition,
        export: Export::Local,
        local_inner: LocalInner::No,
        transparency: None,
    };
    MacroDefinition::rules(tokens(body, edition), at)
}

fn site(edition: Edition) -> CallSite {
    CallSite { edition, panic_edition: edition, ctx: Ctx::root(edition) }
}

/// `m!(input)` of `body`.
fn expanded(body: &str, input: &str, edition: Edition) -> Result<Expanded<'static>, ExpandError> {
    let def = Box::leak(Box::new(def(body, edition)));
    let prepared = prepare(def)?;
    let input = Box::leak(Box::new(tokens(input, edition)));
    let call = Call::fn_like(input, site(edition));
    expand_prepared(&prepared, &call, &NoCx, blaze_own::testing::heap().alloc())
}

/// The recorded expansions: one block a case, `== <case>` and then its rendering
/// ([`compare::render`]). They were recorded from mbe's engine while it was beside this one, the
/// two agreeing on every case; a case added since has no recording and is an error.
const EXPECTED: &str = include_str!("rules_diff.expected");

/// The recorded rendering of `case`, if there is one.
fn recorded(case: &str) -> Option<&'static str> {
    let head = format!("== {case}\n");
    let start = EXPECTED.find(&head)? + head.len();
    let end = EXPECTED[start..].find("\n== ").map_or(EXPECTED.len(), |at| start + at + 1);
    Some(&EXPECTED[start..end])
}

/// `m!(input)` expands as recorded; its result.
fn agree(body: &str, input: &str, edition: Edition) -> Result<Expanded<'static>, ExpandError> {
    let case = format!("m!({input}) of {body} in {edition:?}");
    let now = expanded(body, input, edition);
    let Some(then) = recorded(&case) else { panic!("{case}: no recorded expansion") };
    assert_eq!(then, compare::render(&now), "{case}: the recorded expansion");
    now
}

fn words(result: &Result<Expanded<'static>, ExpandError>) -> Vec<String> {
    match result {
        Ok(Expanded::Expansion(exp)) => {
            exp.diagnostics.iter().map(|d| compare::diagnostic(exp, d)).collect()
        }
        other => panic!("not an expansion: {other:?}"),
    }
}

fn text(result: &Result<Expanded<'static>, ExpandError>) -> String {
    match result {
        Ok(Expanded::Expansion(exp)) => (0..exp.tokens.len())
            .map(|at| match exp.tokens.tokens()[at].kind {
                TokenKind::Punct { char, .. } => char.to_string(),
                _ => exp.tokens.text_at(at).to_owned(),
            })
            .collect::<Vec<_>>()
            .join(" "),
        other => panic!("not an expansion: {other:?}"),
    }
}

const E21: Edition = Edition::E2021;

/// The refinement: `macro_rules!` expansions, hygiene marks, and diagnostics agree token-for-token with the recorded reference expansions.
#[test]
fn rules_and_their_errors_agree() {
    // Local ambiguity: a repetition of `ident` before an `ident`.
    let out = agree("($($a:ident)* $b:ident) => {}", "x y", E21);
    assert_eq!(words(&out).len(), 1, "local ambiguity is an error");
    // No rule matched, with the furthest rule's words.
    let out = agree("(a $x:ident) => {}; (b $y:expr) => {}", "b struct", E21);
    assert_eq!(words(&out).len(), 1);
    let out = agree("(a) => {}", "c", E21);
    assert_eq!(words(&out).len(), 1);
    // Leftover tokens and an empty input.
    agree("(a) => {}", "a b", E21);
    agree("() => { x }", "", E21);
    // A binding used at the wrong depth, and a repetition with no variable.
    let out = agree("($($a:ident)*) => { $a }", "x y", E21);
    assert_eq!(words(&out).len(), 1, "`$a` outside its repetition");
    agree("($($a:ident)*) => { $(x)* }", "p q", E21);
    agree("($($a:ident)*) => { $($a)+ }", "", E21);
    // An undeclared name is written as it is.
    assert_eq!(text(&agree("() => { $nope }", "", E21)), "$ nope");
}

#[test]
fn crate_and_hygiene_agree() {
    let out = agree("($x:ident) => { $crate::f($x); let y = $x; }", "z", E21);
    assert!(text(&out).starts_with("$crate"));
    // A name the definition writes and one the call gives keep their contexts.
    agree("($e:expr) => { { let x = 1; $e + x } }", "x * 2", E21);
}

#[test]
fn repetitions_agree() {
    let body = "($($a:ident: $($b:expr),*);*) => { $( fn $a() { $( let _ = $b; )* } )* }";
    let out = agree(body, "f: 1, 2 + 3; g: ; h: x.y()", E21);
    assert!(text(&out).contains("fn h"));
    // Separators: punctuation, glued punctuation, a literal, a lifetime.
    agree("($($a:ident)=>*) => { $($a)=>* }", "a => b => c", E21);
    agree("($($a:ident)::*) => { $($a)::* }", "a::b::c", E21);
    agree("($($a:ident)'x*) => { $($a)'x* }", "a 'x b", E21);
    agree("($($a:literal);*) => { [$($a),*] }", "1; \"s\"; 'c'", E21);
    agree("($($a:tt)?) => { $($a)? }", "", E21);
    agree("($($a:tt)?) => { $($a)? }", "!", E21);
    // Nested repetitions of different lengths.
    agree("($( ($($x:tt)*) )*) => { $( [$($x)*] )* }", "(a b) () (c)", E21);
}

#[test]
fn fragments_agree() {
    let body = "($v:vis $i:ident $t:ty, $p:pat, $e:expr, $s:stmt, $b:block, $l:lifetime, \
        $m:meta, $pa:path, $lit:literal, $it:item, $tt:tt) => { $v $i $t $p $e $s $b $l $m $pa $lit $it $tt }";
    agree(body, "pub(crate) x Vec<u8>, Some(_), a + b * c, let y = 2, { 1 }, 'a, \
        derive(Debug), a::b::<C>, -1, fn f() {}, [z]", E21);
    // An expression put before a method call and a type before `<`.
    agree("($e:expr) => { $e.len() }", "a + b", E21);
    agree("($e:expr) => { $e.len() }", "-1", E21);
    agree("($t:ty) => { <$t>::f() }", "Vec<u8>", E21);
    agree("($p:path) => { $p::f() }", "Vec<u8>", E21);
    // 2021 `expr` refuses `_` and `const`; 2024 takes them.
    agree("($e:expr) => { $e }", "_", E21);
    agree("($e:expr) => { $e }", "const { 1 }", Edition::E2024);
    agree("($e:expr_2021) => { $e }", "_", Edition::E2024);
    // A fragment that does not parse.
    let out = agree("($t:ty) => {}", "+", E21);
    assert_eq!(words(&out).len(), 1);
}

#[test]
fn metavariable_expressions_agree() {
    let out = agree(
        "($($a:ident),*) => { $( ${ignore($a)} ${index()} ${len()} )* ${count($a)} }",
        "x, y, z",
        E21,
    );
    assert!(text(&out).ends_with('3'), "`${{count}}` of three: {}", text(&out));
    agree("($($a:ident)*;)*) => {}", "", E21);
    agree("($( $($a:ident)* );*) => { ${count($a, 0)} ${count($a, 1)} }", "a b; c", E21);
    agree("($a:ident) => { ${concat($a, _x, 1)} }", "f", E21);
    agree("($a:ident) => { ${concat($a, -)} }", "f", E21);
}

#[test]
fn a_definition_deeper_than_the_bound_is_refused_by_name() {
    let depth = 1100;
    let body = format!("() => {{ {}{} }}", "(".repeat(depth), ")".repeat(depth));
    // On the test thread's own stack: nothing recurses on the definition's nesting (mbe's
    // parser did, and needed a 256 MiB stack here).
    match expanded(&body, "", E21) {
        Err(ExpandError::RulesBound { bound: RulesLimit::Depth, call: None, .. }) => {}
        other => panic!("a template nested {depth} deep: {other:?}"),
    }
}

/// The planted control: each difference planted in a copy of the compiled engine's expansion
/// is found, and of the right class.
#[test]
fn the_oracle_catches_planted_differences() {
    let body = "($($a:ident),*) => { $( let $a = 1; )* }";
    let fresh = || agree(body, "x, y", E21);
    let base = fresh();
    assert_eq!(compare(&base, &fresh()), None);
    type Plant = fn(&mut blaze_expand::Expansion<'static>);
    let plants: [(&str, Plant); 5] = [
        ("drop the last token", |exp| {
            let len = exp.tokens.len() - 1;
            exp.tokens = exp.tokens.copy_in(0..len, blaze_own::testing::heap().alloc()).expect("copies");
            exp.origins.truncate(len);
        }),
        ("make a token joint", |exp| {
            let mut copy = TokenStream::new_in(blaze_own::testing::heap().alloc());
            for at in 0..exp.tokens.len() {
                copy.push_from(&exp.tokens, at).expect("copies");
            }
            let mut out = TokenStream::new_in(blaze_own::testing::heap().alloc());
            for at in 0..copy.len() {
                let token = copy.tokens()[at];
                let kind = match (at, token.kind) {
                    (0, TokenKind::Ident { raw, .. }) => TokenKind::Ident { raw, spacing: Spacing::JointHidden },
                    (_, kind) => kind,
                };
                let ctx = out.import_ctx(&copy, token.ctx).expect("imports");
                out.push(kind, copy.text_at(at), ctx).expect("pushes");
            }
            exp.tokens = out;
        }),
        ("move an origin", |exp| exp.origins[1] = TokenOrigin::Def(0)),
        ("name another arm", |exp| exp.arm = Some(7)),
        ("lose a nested call", |exp| exp.nested.push(blaze_expand::NestedCall {
            kind: blaze_expand::NestedKind::FnLike,
            path: blaze_expand::Range { start: 0, end: 1 },
            name: None,
            args: blaze_expand::Range { start: 1, end: 2 },
            params: None,
            scope: blaze_expand::PathScope::AsWritten,
        })),
    ];
    for (what, plant) in plants {
        let mut planted = fresh();
        let Ok(Expanded::Expansion(exp)) = &mut planted else { panic!("expands") };
        plant(exp);
        let found = compare(&base, &planted);
        let class_ok = match (what, &found) {
            ("drop the last token", Some(Difference::Len { .. })) => true,
            ("make a token joint", Some(Difference::Token { .. })) => true,
            ("move an origin", Some(Difference::Origin { .. })) => true,
            ("name another arm", Some(Difference::Arm { .. })) => true,
            ("lose a nested call", Some(Difference::Nested)) => true,
            _ => false,
        };
        assert!(class_ok, "planted `{what}`, found {found:?}");
        // And the rendering the recordings hold sees it too.
        assert_ne!(compare::render(&base), compare::render(&planted), "planted `{what}`");
    }
}
