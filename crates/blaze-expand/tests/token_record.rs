//! An expansion's token record (drive.md D122, `Expansion::token_record`): the same tokens the
//! old rendered text lexes back to, a `999.try_into()` kept as its tokens, `$crate` kept as a
//! mark with no crate spelled, and one digest whatever the spaces of the text it came from.

#![feature(allocator_api)]

use blaze_expand::{
    Call, CallSite, Ctx, DefAt, DefSite, Edition, ExpandCx, Expanded, Expansion, Export, HostAsk, HostError,
    HostWhy, IncludeAs, Included, LocalInner, MacroDefinition, OriginMaps, Place, PlaceOf, ProcCall, Produced,
    SourceIndexes, TokenKind, TokenOrigin, TokenStream, Transparency, expand, glued_in_text, lex, token_record,
    write_token,
};
use blaze_format::Aligned;
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::{AString, Alloc};
use blaze_tokens::{MarkOf, Origin, Tokens};
use parser::SyntaxKind;

/// The outside: nothing the macros here ask for.
struct Cx;

impl ExpandCx for Cx {
    fn env(&self, _: &str) -> Result<Option<&str>, HostError> {
        Ok(None)
    }

    fn cfg(&self, _: &TokenStream<'_>) -> Result<bool, HostError> {
        Ok(false)
    }

    fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Err(HostError { ask: HostAsk::Include, why: HostWhy::Refused })
    }

    fn place(&self, _: PlaceOf) -> Result<Place<'_>, HostError> {
        Ok(Place::Number(1))
    }

    fn run_proc_macro<'a>(&self, _: &MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        Err(HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused })
    }
}

fn krate(byte: u8) -> CrateItemKey {
    CrateItemKey(ContentId([byte; 32]))
}

fn rules(body: &str, by: u8) -> MacroDefinition<'static> {
    let at = DefAt {
        site: DefSite { krate: krate(by), module: None },
        edition: Edition::E2021,
        export: Export::Local,
        local_inner: LocalInner::No,
        transparency: None,
    };
    MacroDefinition::rules(tokens(body), at)
}

fn tokens(text: &str) -> TokenStream<'static> {
    lex(text, Edition::E2021, blaze_own::testing::heap().alloc()).expect("the test's text lexes")
}

fn site() -> CallSite {
    CallSite { edition: Edition::E2021, panic_edition: Edition::E2021, ctx: Ctx::root(Edition::E2021) }
}

/// `m!(input)` of `def`, expanded, and its token record, its origins counted in record tokens
/// of the call's input and the definition's body.
fn expanded(def: &MacroDefinition<'static>, input: &str) -> (Expansion<'static>, Aligned) {
    let alloc = blaze_own::testing::heap().alloc();
    let input = tokens(input);
    let call = Call::fn_like(&input, site());
    let expansion = match expand(def, &call, &Cx, alloc).expect("the call expands") {
        Expanded::Expansion(expansion) => expansion,
        Expanded::NeedsExpanded(_) => panic!("the call is not eager"),
    };
    let sources = SourceIndexes::of(def, &call, alloc);
    let record = expansion.token_record(&sources.maps(), alloc).expect("its tokens make a token record");
    (expansion, Aligned::copy_from(&record))
}

/// The old item text of `stream` (blaze-scope resolve.rs `render_with`, `$crate` aside): each
/// token as `write_token` writes it, invisible delimiters not at all, a space between two
/// unless `glued_in_text`.
fn rendered(stream: &TokenStream<'_>) -> String {
    let mut out = AString::new_in(blaze_own::testing::heap().alloc());
    let mut prev: Option<TokenKind> = None;
    for token in stream.tokens() {
        if matches!(token.kind, TokenKind::Open(blaze_expand::Delim::Invisible) | TokenKind::Close(blaze_expand::Delim::Invisible)) {
            continue;
        }
        if prev.is_some_and(|prev| !glued_in_text(prev, token.kind)) {
            out.push(' ');
        }
        write_token(stream, token, &mut out);
        prev = Some(token.kind);
    }
    out.as_str().to_owned()
}

/// The token record of `text` lexed: every token made, no source.
fn relexed(text: &str) -> Aligned {
    let stream = tokens(text);
    let origins = vec![TokenOrigin::Made(Transparency::Transparent); stream.len()];
    let record = token_record(&stream, &origins, &OriginMaps::NONE, blaze_own::testing::heap().alloc())
        .expect("a lexed text makes a token record");
    Aligned::copy_from(&record)
}

/// Asserts the token record of `def` called on `input` holds, token for token, what the old
/// rendered text lexes back to: the same word (but for the `FRAGMENT` bit, which the text has
/// no group to carry, and `JOINT`, compared where it means something), the same text, and the
/// same joined flag between two punctuations. Returns the token count.
fn same_as_rendered(def: &MacroDefinition<'static>, input: &str) -> usize {
    let (expansion, bytes) = expanded(def, input);
    let record = Tokens::open(&bytes).expect("the record opens");
    let text = rendered(&expansion.tokens);
    let back_bytes = relexed(&text);
    let back = Tokens::open(&back_bytes).expect("the record opens");
    assert_eq!(record.len(), back.len(), "the record and the text `{text}` have other token counts");
    let mask = !(parser::FRAGMENT | parser::JOINT);
    for i in 0..record.len() {
        assert_eq!(record.word(i) & mask, back.word(i) & mask, "token {i} of `{text}`: other words");
        assert_eq!(record.text(i), back.text(i), "token {i} of `{text}`: other texts");
        if i + 1 < record.len() && record.kind(i).is_punct() && record.kind(i + 1).is_punct() {
            assert_eq!(record.is_joined(i), back.is_joined(i), "token {i} of `{text}`: other joins");
        }
    }
    record.len()
}

/// The record against the old rendered text, re-lexed, for a `macro_rules!` with fragments and
/// repetition, a nested call, a lifetime, suffixed literals and a doc comment. A writer that
/// rendered a lexeme (a string's quotes lost, a lifetime's `'`, a suffix), split a lifetime in
/// two, or kept an invisible group as a token fails it. Prints "token record oracle: 6 cases,
/// N tokens as the rendered text lexes".
#[test]
fn a_token_record_holds_what_the_rendered_text_lexes_back_to() {
    let cases = [
        (rules("($($x:ident: $t:ty),*) => { $(let $x: $t = Default::default();)* }", 1), "a: u8, b: Vec<u8>"),
        (rules("($e:expr) => { foo!($e); bar! { 1 } }", 1), "1 + 2"),
        (rules("($l:lifetime) => { struct S<$l>(&$l u8); fn f<'b>(x: &'b u8) {} }", 1), "'a"),
        (rules("() => { const X: u64 = 5u64 + 1.5e3f64 as u64 + 2. as u64; }", 1), ""),
        (rules("() => { /// docs here\n fn f() {} }", 1), ""),
        (rules("($($t:tt)*) => { m! { $($t)* } }", 1), "r#type b\"x\" 'c' r\"raw\" 7i8 ::"),
    ];
    let mut total = 0;
    for (def, input) in &cases {
        total += same_as_rendered(def, input);
    }
    println!("token record oracle: {} cases, {total} tokens as the rendered text lexes", cases.len());
}

/// The old text wrote `999.try_into()` as `999 .try_into()` only by its renderer's rule; the
/// record holds the integer and the `.` as two tokens whatever writes them. Prints "999.try_into:
/// 999 | . | try_into, shown `..`".
#[test]
fn an_integer_before_a_dot_stays_two_tokens() {
    let (_, bytes) = expanded(&rules("() => { let _ = 999.try_into(); }", 1), "");
    let record = Tokens::open(&bytes).expect("the record opens");
    let at = (0..record.len()).find(|&i| record.text(i) == "999").expect("the integer is a token");
    assert_eq!(record.kind(at), SyntaxKind::INT_NUMBER);
    assert_eq!((record.kind(at + 1), record.text(at + 1)), (SyntaxKind::DOT, "."));
    assert_eq!((record.kind(at + 2), record.text(at + 2)), (SyntaxKind::IDENT, "try_into"));
    let mut shown = String::new();
    record.shown(record.all(), &mut shown).expect("shown to a string");
    println!("999.try_into: {} | {} | {}, shown `{shown}`", record.text(at), record.text(at + 1), record.text(at + 2));
}

/// A `$crate` is one token spelled `$crate`, its crate the mark's definition site: no crate's
/// name is written into the record. A writer that rendered `$crate` as `crate` or a crate's
/// name, or dropped its context, fails. Prints "$crate: lexeme `$crate`, mark of crate 0x07".
#[test]
fn a_dollar_crate_carries_its_mark_and_spells_no_crate() {
    let (_, bytes) = expanded(&rules("() => { $crate::f() }", 7), "");
    let record = Tokens::open(&bytes).expect("the record opens");
    let at = (0..record.len()).find(|&i| record.text(i) == "$crate").expect("`$crate` is one token");
    assert_eq!(record.lexeme(at), Some("$crate"));
    assert!((0..record.len()).all(|i| record.text(i) != "crate"), "no `crate` is spelled in its place");
    let mark = record.mark(at).expect("a token the macro wrote has a mark");
    assert_eq!(mark.site.krate, krate(7));
    assert_eq!(mark.of, MarkOf::This);
    assert!(matches!(record.origin(at), Origin::Def(_)), "{:?}", record.origin(at));
    println!("$crate: lexeme `{}`, mark of crate {:#04x}", record.text(at), mark.site.krate.0.0[0]);
}

/// Two calls whose input differs only in spaces, and two definitions whose body does, give one
/// token digest; a control that changes a join (`+=` against `+ =`, two tokens the parser reads
/// apart) gives another. Prints "spacing: input digests equal, body digests equal, a join
/// changes the digest".
#[test]
fn spaces_do_not_change_the_token_digest() {
    let digest = |def: &MacroDefinition<'static>, input: &str| {
        let (_, bytes) = expanded(def, input);
        let record = Tokens::open(&bytes).expect("the record opens");
        record.digest(record.all())
    };
    let echo = rules("($($t:tt)*) => { $($t)* }", 1);
    assert_eq!(digest(&echo, "a+b(c)"), digest(&echo, "a + b ( c )"));
    let tight = rules("() => { f(a,b) }", 1);
    let loose = rules("() => {\n    f( a ,  b )\n}", 1);
    assert_eq!(digest(&tight, ""), digest(&loose, ""));
    assert_ne!(digest(&echo, "a+=b"), digest(&echo, "a+ =b"));
    println!("spacing: input digests equal, body digests equal, a join changes the digest");
}

/// A record's origins count record tokens of the source: in `'a x`, `x` is the input's second
/// record token (a lifetime is one), where the stream counts it third. Prints "origins: `x`
/// from input token 1".
#[test]
fn an_origin_counts_record_tokens_of_its_source() {
    let (_, bytes) = expanded(&rules("($l:lifetime $x:ident) => { $x }", 1), "'a x");
    let record = Tokens::open(&bytes).expect("the record opens");
    let at = (0..record.len()).find(|&i| record.text(i) == "x").expect("`x` is a token");
    assert_eq!(record.origin(at), Origin::Input(1));
    println!("origins: `x` from input token 1");
}
