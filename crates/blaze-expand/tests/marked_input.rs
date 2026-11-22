//! A call written in the text of an item a macro wrote is the macro's input with its tokens'
//! marks (`lex_marked`): a `$crate` the expansion wrote stays one identifier of the defining
//! crate, as rustc's token stream keeps it.
//!
//! Rust's rule (the Reference, "Macros By Example", "Hygiene"): `$crate` "refers to the crate
//! defining the macro" the token came from, wherever the token is passed on. The real site:
//! tracing 0.1.44's `event!` writes, inside the fn body that calls `debug!`, `static
//! __CALLSITE: $crate::callsite::DefaultCallsite = $crate::callsite2! { name: .., kind:
//! $crate::metadata::Kind::EVENT, target: module_path!(), level: $crate::Level::DEBUG, fields:
//! .. };`. The static is a body-local item lowered from its text (blaze-lower
//! `lower_body_expanding_marked`), and its call's input was lexed again by `lex`, so each
//! `$crate` was `$` and `crate`, and `kind: $kind:expr` met a `$`: tempo t1500, 14 tests NOT RUN
//! under tempo-payload-types `ValidationLatencyEstimator::observe` (src/budget.rs:192): "Macro {
//! path: \"$crate :: callsite2\", why: \"its expansion's error Binding: `expected
//! expression`\" }"; the Nov 17 census held 102 such callsites across tempo's dependencies.
//!
//! The fix's cases: the input as the copied text writes it, lexed with its marks, matches
//! tracing's `callsite2!` (all three arms as published) and its `$crate` keeps tracing's crate.
//! NEGATIVE TWIN: the same text with no marks (a `$crate` written at the call, outside any
//! macro, which rustc refuses: "expected expression, found `$`") is still the binding error,
//! from both lexers. CONTROL: a text with no `$crate` expands the same from `lex` and
//! `lex_marked`, so the marks are the one difference. And the reduced call the report names,
//! `debug!(x = ?self.f(), n = self.v.len(), "m")`, run through tracing's `fieldset!` to its
//! base case, shows the matcher reads that shape. NOT RUN by the writer (a write-only lane,
//! uncompiled).

#![feature(allocator_api)]

use blaze_expand::origin_column::TextMark;
use blaze_expand::{
    Call, CallSite, Ctx, DefAt, DefSite, Diagnostic, Edition, ExpandCx, Expanded, Expansion, Export, HostAsk, HostError,
    HostWhy, IncludeAs, Included, LocalInner, MacroDefinition, MarkOf, Place, PlaceOf, ProcCall, Produced, Raw, TokenKind, TokenOrigin, TokenStream, Transparency, call_key, expand, lex, lex_marked,
};
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::Alloc;

/// The crate that defines the macros (tracing's place in the tests).
const TRACING: u8 = 7;

/// tracing 0.1.44's `callsite2!` (src/macros.rs:2757), as published.
const CALLSITE2: &str = "
    (name: $name:expr, kind: $kind:expr, fields: $($fields:tt)*) => {{
        $crate::callsite2! {
            name: $name,
            kind: $kind,
            target: module_path!(),
            level: $crate::Level::TRACE,
            fields: $($fields)*
        }
    }};
    (
        name: $name:expr,
        kind: $kind:expr,
        level: $lvl:expr,
        fields: $($fields:tt)*
    ) => {{
        $crate::callsite2! {
            name: $name,
            kind: $kind,
            target: module_path!(),
            level: $lvl,
            fields: $($fields)*
        }
    }};
    (
        name: $name:expr,
        kind: $kind:expr,
        target: $target:expr,
        level: $lvl:expr,
        fields: $($fields:tt)*
    ) => {{
        static META: $crate::Metadata<'static> = {
            $crate::metadata! {
                name: $name,
                target: $target,
                level: $lvl,
                fields: $crate::fieldset!( $($fields)* ),
                callsite: &__CALLSITE,
                kind: $kind,
            }
        };
        $crate::callsite::DefaultCallsite::new(&META)
    }};
";

/// tracing 0.1.44's `fieldset!` (src/macros.rs:3165), as published (its commented arm left out).
const FIELDSET: &str = "
    (@ { $(,)* $($out:expr),* $(,)* } $(,)*) => {
        &[ $($out),* ]
    };
    (@ { $(,)* $($out:expr),* } $($k:ident).+ = ?$val:expr, $($rest:tt)*) => {
        $crate::fieldset!(@ { $($out),*, $crate::__tracing_stringify!($($k).+) } $($rest)*)
    };
    (@ { $(,)* $($out:expr),* } $($k:ident).+ = %$val:expr, $($rest:tt)*) => {
        $crate::fieldset!(@ { $($out),*, $crate::__tracing_stringify!($($k).+) } $($rest)*)
    };
    (@ { $(,)* $($out:expr),* } $($k:ident).+ = $val:expr, $($rest:tt)*) => {
        $crate::fieldset!(@ { $($out),*, $crate::__tracing_stringify!($($k).+) } $($rest)*)
    };
    (@ { $(,)* $($out:expr),* } ?$($k:ident).+, $($rest:tt)*) => {
        $crate::fieldset!(@ { $($out),*, $crate::__tracing_stringify!($($k).+) } $($rest)*)
    };
    (@ { $(,)* $($out:expr),* } %$($k:ident).+, $($rest:tt)*) => {
        $crate::fieldset!(@ { $($out),*, $crate::__tracing_stringify!($($k).+) } $($rest)*)
    };
    (@ { $(,)* $($out:expr),* } $($k:ident).+, $($rest:tt)*) => {
        $crate::fieldset!(@ { $($out),*, $crate::__tracing_stringify!($($k).+) } $($rest)*)
    };
    (@ { $(,)* $($out:expr),* } $k:literal = ?$val:expr, $($rest:tt)*) => {
        $crate::fieldset!(@ { $($out),*, $k } $($rest)*)
    };
    (@ { $(,)* $($out:expr),* } $k:literal = %$val:expr, $($rest:tt)*) => {
        $crate::fieldset!(@ { $($out),*, $k } $($rest)*)
    };
    (@ { $(,)* $($out:expr),* } $k:literal = $val:expr, $($rest:tt)*) => {
        $crate::fieldset!(@ { $($out),*, $k } $($rest)*)
    };
    (@ { $(,)* $($out:expr),* } { $k:expr } = ?$val:expr, $($rest:tt)*) => {
        $crate::fieldset!(@ { $($out),*, $k } $($rest)*)
    };
    (@ { $(,)* $($out:expr),* } { $k:expr } = %$val:expr, $($rest:tt)*) => {
        $crate::fieldset!(@ { $($out),*, $k } $($rest)*)
    };
    (@ { $(,)* $($out:expr),* } { $k:expr } = $val:expr, $($rest:tt)*) => {
        $crate::fieldset!(@ { $($out),*, $k } $($rest)*)
    };
    (@ { $(,)* $($out:expr),* } $($rest:tt)+) => {
        $crate::fieldset!(@ { \"message\", $($out),*, })
    };
    ($($args:tt)*) => {
        $crate::fieldset!(@ { } $($args)*,)
    };
";

/// The input of the static's `callsite2!` call as the body-local copy writes it (words with a
/// space between, `$crate` spelled whole: blaze-lower `ExpansionWords::of`), for the reduced
/// call `debug!(x = ?self.f(), n = self.v.len(), "m")`.
const COPIED: &str = "{ name : concat ! ( \"event src/budget.rs:206\" ) , kind : $crate :: metadata :: Kind :: EVENT , \
    target : module_path ! ( ) , level : $crate :: Level :: DEBUG , fields : x = ? self . f ( ) , n = self . v . len ( ) , \"m\" }";

/// The control: the same call with no `$crate` in its input.
const PLAIN: &str = "{ name : concat ! ( \"event src/budget.rs:206\" ) , kind : Kind :: EVENT , \
    target : module_path ! ( ) , level : Level :: DEBUG , fields : x = ? self . f ( ) , n = self . v . len ( ) , \"m\" }";

/// The outside of the expansions: nothing is read (a `macro_rules!` call reads nothing).
struct NoReads;

const NOT_ASKED: HostError = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Failed };

impl ExpandCx for NoReads {
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

    fn run_proc_macro<'a>(&self, _: &MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        Err(NOT_ASKED)
    }
}

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn site() -> CallSite {
    CallSite { edition: Edition::E2021, panic_edition: Edition::E2021, ctx: Ctx::root(Edition::E2021) }
}

fn tracing_site() -> DefSite {
    DefSite { krate: CrateItemKey(ContentId([TRACING; 32])), module: None }
}

fn rules(body: &str) -> MacroDefinition<'static> {
    let at = DefAt { site: tracing_site(), edition: Edition::E2021, export: Export::Local, local_inner: LocalInner::No, transparency: None };
    MacroDefinition::rules(lex(body, Edition::E2021, alloc()).expect("the definition lexes"), at)
}

/// The mark the expansion that wrote the copied text gave its `$crate` words.
fn tracing_mark() -> TextMark {
    TextMark { site: tracing_site(), transparency: Transparency::SemiOpaque, edition: Edition::E2021 }
}

/// The tokens inside the tree `text` (delimiters included), lexed as blaze-lower's `ByName::input`
/// cuts a call's input: by `lex`, or by `lex_marked` with a mark at each offset of `marked`.
fn input(text: &str, marked: Option<&[u32]>) -> TokenStream<'static> {
    let whole = match marked {
        None => lex(text, Edition::E2021, alloc()).expect("the input lexes"),
        Some(marked) => {
            let mark_at = |at: u32| marked.contains(&at).then(tracing_mark);
            lex_marked(text, Edition::E2021, &mark_at, alloc()).expect("the input lexes")
        }
    };
    whole.copy_in(1..whole.len() - 1, alloc()).expect("the inside copies")
}

/// The offsets of each `$crate` in `text`: where the copy's column starts its row.
fn dollar_crates(text: &str) -> Vec<u32> {
    text.match_indices("$crate").map(|(at, _)| u32::try_from(at).expect("a short text")).collect()
}

fn call(def: &MacroDefinition<'static>, input: &TokenStream<'static>) -> Expansion<'static> {
    match expand(def, &Call::fn_like(input, site()), &NoReads, alloc()).expect("the call expands") {
        Expanded::Expansion(expansion) => expansion,
        Expanded::NeedsExpanded(nested) => panic!("a `macro_rules!` call needs {} nested calls", nested.len()),
    }
}

/// Each diagnostic as its kind and, for a binding error, its message.
fn diags(exp: &Expansion<'_>) -> Vec<String> {
    exp.diagnostics
        .iter()
        .map(|d| match *d {
            Diagnostic::Binding { message, .. } => format!("Binding: {}", exp.note_text(message)),
            other => format!("{other:?}"),
        })
        .collect()
}

/// The tokens as text, one space between each; invisible groups not shown.
fn show(stream: &TokenStream<'_>) -> String {
    let mut out = Vec::new();
    for token in stream.tokens() {
        let text = stream.text_of(token);
        out.push(match token.kind {
            TokenKind::Ident { raw: Raw::Yes, .. } => format!("r#{text}"),
            TokenKind::Ident { raw: Raw::No, .. } | TokenKind::Literal { .. } => text.to_owned(),
            TokenKind::Punct { char, .. } => char.to_string(),
            TokenKind::Open(blaze_expand::Delim::Invisible) | TokenKind::Close(blaze_expand::Delim::Invisible) => continue,
            TokenKind::Open(d) => format!("{d:?}("),
            TokenKind::Close(d) => format!("){d:?}"),
        });
    }
    out.join(" ")
}

/// The copied text lexed with its marks: each `$crate` one identifier in a context of tracing's
/// mark; `callsite2!` matches its third arm, and the `$crate` it was handed (`kind: $kind`)
/// keeps tracing's crate in the expansion, an earlier expansion's mark beside the arm's own.
#[test]
fn a_copied_items_call_reads_its_dollar_crate_by_its_mark() {
    let def = rules(CALLSITE2);
    let marked = dollar_crates(COPIED);
    assert_eq!(marked.len(), 2, "the copy writes two `$crate`s");
    let args = input(COPIED, Some(&marked));
    // No `$` is left: both are folded into their `crate`.
    assert!(!args.tokens().iter().any(|t| matches!(t.kind, TokenKind::Punct { char: '$', .. })), "{}", show(&args));
    let folded: Vec<usize> = (0..args.len()).filter(|&i| args.text_at(i) == "$crate").collect();
    assert_eq!(folded.len(), 2, "{}", show(&args));
    for &i in &folded {
        let mark = args.hygiene(args.tokens()[i].ctx).mark.expect("the copy's `$crate` keeps its mark");
        assert_eq!(mark.site, tracing_site());
        assert!(matches!(mark.of, MarkOf::Outer(_)), "an input holds no mark of the call's own: {mark:?}");
        assert!(matches!(args.tokens()[i].kind, TokenKind::Ident { raw: Raw::No, .. }));
    }
    let exp = call(&def, &args);
    assert_eq!(diags(&exp), Vec::<String>::new(), "{}", show(&exp.tokens));
    assert_eq!(exp.arm, Some(2), "the arm with `target:` and `level:`");
    // The handed-in `$crate`s: tokens of the input, still tracing's.
    let handed: Vec<usize> =
        (0..exp.tokens.len()).filter(|&i| exp.tokens.text_at(i) == "$crate" && matches!(exp.origins[i], TokenOrigin::Input(_))).collect();
    assert_eq!(handed.len(), 2, "`$kind` and `$lvl` are transcribed: {}", show(&exp.tokens));
    for i in handed {
        let mark = exp.hygiene(i).mark.expect("a handed-in `$crate` keeps its mark");
        assert_eq!(mark.site, tracing_site());
        assert!(matches!(mark.of, MarkOf::Outer(_)));
    }
}

/// NEGATIVE TWIN: the same text with no marks is a `$crate` written at the call, outside any
/// macro: rustc lexes `$` and `crate` and refuses `kind: $kind:expr` at the `$` ("expected
/// expression, found `$`"). Both lexers give that error: `lex_marked` folds only a marked `$`.
#[test]
fn an_unmarked_dollar_crate_is_still_refused() {
    let def = rules(CALLSITE2);
    for (how, args) in [("lex", input(COPIED, None)), ("lex_marked, no marks", input(COPIED, Some(&[])))] {
        assert!(args.tokens().iter().any(|t| matches!(t.kind, TokenKind::Punct { char: '$', .. })), "{how}: {}", show(&args));
        let exp = call(&def, &args);
        let d = diags(&exp);
        assert!(d.iter().any(|d| d.starts_with("Binding: ") && d.contains("expected expression")), "{how}: {d:?}");
    }
}

/// CONTROL: with no `$crate` in the input, `lex` and `lex_marked` give the same tokens and the
/// same clean expansion, so the marks are the one difference the fix makes.
#[test]
fn a_text_with_no_dollar_crate_lexes_the_same_either_way() {
    let def = rules(CALLSITE2);
    let (plain, marked) = (input(PLAIN, None), input(PLAIN, Some(&[])));
    assert_eq!(show(&plain), show(&marked));
    assert_eq!(plain.contexts().len(), 0);
    assert_eq!(marked.contexts().len(), 0);
    let (a, b) = (call(&def, &plain), call(&def, &marked));
    assert_eq!(diags(&a), Vec::<String>::new());
    assert_eq!(diags(&b), Vec::<String>::new());
    assert_eq!(show(&a.tokens), show(&b.tokens));
    assert_eq!(a.arm, Some(2));
}

/// The reduced call the report names, `debug!(x = ?self.f(), n = self.v.len(), "m")`: its fields
/// through tracing's `fieldset!`, each step's output called again (sealed, as an expansion's
/// tokens are before they are another call's input), to the base case: `&[ "message", x's name,
/// n's name ]`, with no error on the way. The matcher reads this shape (`?self.f()` is an
/// `expr` that takes the windowed parse, `self` being a keyword).
#[test]
fn the_reduced_fields_reach_fieldsets_base_case() {
    let def = rules(FIELDSET);
    let mut args = input("( x = ? self . f ( ) , n = self . v . len ( ) , \"m\" )", None);
    let mut steps = Vec::new();
    for _ in 0..8 {
        let key = call_key(&def, &Call::fn_like(&args, site())).0;
        let exp = call(&def, &args);
        assert_eq!(diags(&exp), Vec::<String>::new(), "step {}: {}", steps.len(), show(&args));
        steps.push(exp.arm);
        let tokens = &exp.tokens;
        // A recursive step is `$crate :: fieldset ! ( .. )`; the base case is not.
        let bang = (0..tokens.len()).find(|&i| matches!(tokens.tokens()[i].kind, TokenKind::Punct { char: '!', .. }));
        let recursive = !tokens.is_empty() && tokens.text_at(0) == "$crate" && bang.is_some_and(|b| tokens.text_at(b - 1) == "fieldset");
        if !recursive {
            let shown = show(tokens);
            assert!(shown.starts_with("& Bracket( \"message\" ,") || shown.starts_with("& Bracket( message ,"), "{shown}");
            let (x, n) = (shown.find("__tracing_stringify ! Paren( x )Paren"), shown.find("__tracing_stringify ! Paren( n )Paren"));
            assert!(x.is_some() && n.is_some() && x < n, "x's name, then n's: {shown}");
            // The entry (arm 14), `x = ?..` (1), `n = ..` (3), the remainder (13), then the base
            // case (0).
            assert_eq!(steps, [Some(14), Some(1), Some(3), Some(13), Some(0)]);
            return;
        }
        let open = bang.expect("a recursive step has its `!`") + 1;
        let close = tokens.close_of(open).expect("the step's group closes");
        let mut next = tokens.copy_in(open + 1..close, alloc()).expect("the step's input copies");
        next.seal(key);
        args = next;
    }
    panic!("`fieldset!` did not reach its base case in 8 steps: arms {steps:?}");
}
