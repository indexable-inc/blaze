//! E3 (D122, "An expansion is its token record"): a call written inside an expansion takes its
//! input as a token sub-range of that expansion's sealed stream (crate::nested_input), never its
//! rendered text lexed again, so `$crate` and definition-site marks survive at any depth and the
//! call's key holds them.
//!
//! The shape: objc2's `extern_class!`-style chain (tempo census, 2026-11-17: objc2_foundation's
//! `NSString`/`NSMutableString` NotFound in 858 + 254 + 84 fns). `a!` passes `$crate::inner` to
//! `b!`, which calls it. Lexing `b!`'s input from `a!`'s rendered text gave each token the root
//! context: `$crate` was rendered `crate` with no mark, so `crate::inner!` was looked up in the
//! calling crate, where there is no `inner`, and `S` was never declared.

use std::vec::Vec;

use blaze_expand::{
    Call, CallSite, Ctx, DefAt, DefSite, Edition, ExpandCx, Expanded, Export, HostAsk, HostError, HostWhy, IncludeAs,
    Included, LocalInner, MacroDefinition, MarkOf, Place, PlaceOf, ProcCall, Produced, TokenStream, call_key, expand, lex,
    lex_inside,
};
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{Namespace, PathKind};

use super::entry::{Texts, input, resolved};
use crate::names::Written;
use crate::nested_input::{ExpansionTokens, InputCut};
use crate::resolve::render_with;

fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// The objc2 shape: `a!` hands `$crate::inner` to `b!` as a `path`, `b!` calls it.
const OBJC2_DEP: &str = "#![no_core]
#[macro_export]
macro_rules! a { () => { $crate::b!{ ($crate::inner) } } }
#[macro_export]
macro_rules! b { (($m:path)) => { $m!{} } }
#[macro_export]
macro_rules! inner { () => { pub struct S; } }
";

const OBJC2_USER: &str = "#![no_core]
dep::a!();
";

static OBJC2: Texts = Texts(&[&[("/user/lib.rs", OBJC2_USER)], &[("/dep/lib.rs", OBJC2_DEP)]]);

/// Three levels: `$crate` passed down as a `tt` (`a!` to `b!`), then as a `path` (`b!` to `c!`),
/// and called by `c!`.
const DEEP_DEP: &str = "#![no_core]
#[macro_export]
macro_rules! a { () => { $crate::b!{ $crate } } }
#[macro_export]
macro_rules! b { ($k:tt) => { $k::c!{ ($k::inner) } } }
#[macro_export]
macro_rules! c { (($m:path)) => { $m!{} } }
#[macro_export]
macro_rules! inner { () => { pub struct T; } }
";

const DEEP_USER: &str = "#![no_core]
dep::a!();
";

static DEEP: Texts = Texts(&[&[("/user/lib.rs", DEEP_USER)], &[("/dep/lib.rs", DEEP_DEP)]]);

/// Where `name` written in the user crate's root resolves: the defining crate's index.
fn declared_in(texts: &'static Texts, name: &'static str) -> Result<usize, std::string::String> {
    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[])];
    let names = resolved(&crates, texts);
    let segments: &'static [&'static str] = std::vec![name].leak();
    names
        .item_at(names.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments }, heap())
        .map(|at| at.krate)
        .map_err(|e| std::format!("{e:?}; notes {:?}", names.notes(0)))
}

/// Rule: `b!`, called in `a!`'s expansion, takes `($crate::inner)` as `a!`'s tokens, the
/// `$crate` keeping the dependency's mark, so `$m!{}` calls the dependency's `inner!` and `S`
/// is declared in the user crate (index 0), where the call stands.
///
/// Expected: passes. Control: before E3 the input was lexed from the rendered text and the
/// lookup failed: `crate::inner` resolved in the user crate, which has no `inner` (a NotFound
/// macro error), so `S` was not declared and `item_at` answered an error.
#[test]
fn objc2_shape_a_crate_path_passed_to_a_nested_call_keeps_its_crate() {
    let at = declared_in(&OBJC2, "S");
    assert_eq!(at, Ok(0), "`S` is declared in the user crate by the dependency's `inner!`");
}

/// Rule: three levels deep, `$crate` passed as a `tt` and then as a `path`, keeps the
/// dependency's mark; `T` is declared in the user crate.
#[test]
fn three_levels_pass_crate_as_tt_and_as_path() {
    let at = declared_in(&DEEP, "T");
    assert_eq!(at, Ok(0), "`T` is declared in the user crate by the dependency's `inner!`");
}

/// No host: the definitions here read nothing outside.
struct NoHost;

impl ExpandCx for NoHost {
    fn env(&self, _: &str) -> Result<Option<&str>, HostError> {
        Err(HostError { ask: HostAsk::Env, why: HostWhy::Refused })
    }

    fn cfg(&self, _: &TokenStream<'_>) -> Result<bool, HostError> {
        Err(HostError { ask: HostAsk::Cfg, why: HostWhy::Refused })
    }

    fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Ok(Included::Missing)
    }

    fn place(&self, _: PlaceOf) -> Result<Place<'_>, HostError> {
        Ok(Place::Number(1))
    }

    fn run_proc_macro<'a>(&self, _: &MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        Err(HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused })
    }
}

const DEP_KEY: CrateItemKey = CrateItemKey(ContentId([3; 32]));
const USER_KEY: CrateItemKey = CrateItemKey(ContentId([4; 32]));

fn site() -> CallSite {
    CallSite { edition: Edition::E2021, panic_edition: Edition::E2021, ctx: Ctx::root(Edition::E2021) }
}

/// `m!()` of `macro_rules! m { () => { b!{ (Hidden) } } }` defined in [`DEP_KEY`]: its
/// expansion rendered as `expand_decls` renders it, its tokens sealed with the call's key, and
/// the nested call's input as [`ExpansionTokens::input`] cuts it (with that key and the
/// text between the nested call's braces).
fn nested_input_of_def_site_name() -> (TokenStream<'static>, ContentId, &'static str, Vec<Option<u32>>) {
    let alloc = heap();
    let body = lex("() => { b!{ (Hidden) } };", Edition::E2021, alloc).expect("the body lexes");
    let at = DefAt { site: DefSite { krate: DEP_KEY, module: None }, edition: Edition::E2021, export: Export::Exported, local_inner: LocalInner::No, transparency: None };
    let def = MacroDefinition::rules(body, at);
    let input = lex_inside("", Edition::E2021, alloc).expect("the input lexes");
    let call = Call::fn_like(&input, site());
    let expansion = match expand(&def, &call, &NoHost, alloc).expect("the call expands") {
        Expanded::Expansion(expansion) => expansion,
        Expanded::NeedsExpanded(_) => panic!("the call is not eager"),
    };
    let mut places: AVec<'static, (u32, u32, u32)> = AVec::new_in(alloc);
    let mut words: AVec<'static, Option<u32>> = AVec::new_in(alloc);
    let text = render_with(
        &expansion.tokens,
        Some(USER_KEY),
        |at| expansion.hygiene(at).mark.map(|mark| mark.site.krate),
        |_| false,
        &mut |token, start, end, _| places.push((start as u32, end as u32, token as u32)),
        Some(&mut words),
        alloc,
    )
    .expect("the expansion renders");
    let text: &'static str = std::boxed::Box::leak(text.as_str().into());
    let key = call_key(&def, &call).0;
    let mut sealed = expansion.tokens.clone();
    sealed.seal(key);
    let sealed: &'static TokenStream<'static> = std::boxed::Box::leak(std::boxed::Box::new(sealed));
    let tokens = ExpansionTokens { stream: sealed, text, places: places.leak_static(), words: words.leak_static() };
    let (open, close) = (text.find('{').expect("the nested call's brace"), text.rfind('}').expect("its close"));
    let inside = &text[open + 1..close];
    let nested = tokens.input(inside, InputCut::Inside).expect("the nested input is a range of the expansion's tokens");
    let written: Vec<Option<u32>> = nested.written(alloc).iter().copied().collect();
    (nested.copy_in(alloc).expect("the range copies"), key, inside, written)
}

/// Rule: a definition-site name (`Hidden`, written in `m!`'s body) passed through the nested
/// call `b!{ (Hidden) }` keeps its hygiene: the copied token's mark is the definition's
/// (`DEP_KEY`), sealed as `Outer(m!'s key)`. Its indexes as written are the words the renderer
/// placed (`(`, `Hidden`, `)`: 0, 1, 2). Control: the same text lexed (what `expand_decls` did
/// before E3) has no mark at all.
#[test]
fn a_def_site_name_through_a_nested_call_keeps_its_hygiene() {
    let (copy, key, inside, written) = nested_input_of_def_site_name();
    let at = (0..copy.len()).find(|&i| copy.text_at(i) == "Hidden").expect("the input holds `Hidden`");
    let mark = copy.hygiene(copy.tokens()[at].ctx).mark.expect("`Hidden` keeps the definition's mark");
    assert_eq!(mark.site.krate, DEP_KEY, "the mark is the definition's");
    assert_eq!(mark.of, MarkOf::Outer(key), "the outer expansion's own mark is sealed with its call's key");
    assert_eq!(written, [Some(0), Some(1), Some(2)], "the input's tokens indexed as written");
    let lexed = lex_inside(inside, Edition::E2021, heap()).expect("the text lexes");
    let again = (0..lexed.len()).find(|&i| lexed.text_at(i) == "Hidden").expect("the text holds `Hidden`");
    assert!(lexed.hygiene(lexed.tokens()[again].ctx).mark.is_none(), "control: the lexed text has lost the mark");
}

/// Rule 21: two calls of one definition whose inputs have equal text and different contexts
/// (the nested input's marked tokens, and the same text lexed at the root) get different keys.
/// Control: the lexed input called twice is one key.
#[test]
fn equal_input_text_with_different_contexts_is_two_keys() {
    let (copy, _, inside, _) = nested_input_of_def_site_name();
    let lexed = lex_inside(inside, Edition::E2021, heap()).expect("the text lexes");
    let texts = |s: &TokenStream<'_>| (0..s.len()).map(|i| std::string::String::from(s.text_at(i))).collect::<Vec<_>>();
    assert_eq!(texts(&copy), texts(&lexed), "the two inputs have equal text");
    let body = lex("($($t:tt)*) => {};", Edition::E2021, heap()).expect("the body lexes");
    let at = DefAt { site: DefSite { krate: USER_KEY, module: None }, edition: Edition::E2021, export: Export::Local, local_inner: LocalInner::No, transparency: None };
    let def = MacroDefinition::rules(body, at);
    let marked = call_key(&def, &Call::fn_like(&copy, site()));
    let plain = call_key(&def, &Call::fn_like(&lexed, site()));
    assert_ne!(marked, plain, "the key covers the input's contexts");
    assert_eq!(plain, call_key(&def, &Call::fn_like(&lexed, site())), "control: one input, one key");
}
