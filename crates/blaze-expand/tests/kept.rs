//! A crate's kept expansions as one set (`blaze_expand::kept`), the key a call's text makes
//! (`text_key`), and a read list asked again (`Reads::hold`).

#![feature(allocator_api)]

use blaze_expand::kept::{Asked, KeptEntry, KeptError, KeptSet, write_entry, write_set};
use blaze_expand::{
    BuiltinFn, Call, CallShape, CallSite, Ctx, DefAt, DefSite, DefText, Edition, ExpandCx, Expanded, Expansion, Export, Held,
    HostAsk, HostError, HostWhy, IncludeAs, Included, LocalInner, MacroDefinition, Place, PlaceOf, ProcCall, Produced, ReadKind,
    TextCall, TextKey, TokenStream, expand, lex, text_key,
};
use blaze_format::Aligned;
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::Alloc;

/// The outside: one environment variable, one enabled cfg name, a line.
struct Cx {
    var: Option<&'static str>,
    cfg: &'static str,
    line: u32,
}

impl ExpandCx for Cx {
    fn env(&self, name: &str) -> Result<Option<&str>, HostError> {
        Ok(self.var.filter(|_| name == "VAR"))
    }

    fn cfg(&self, predicate: &TokenStream<'_>) -> Result<bool, HostError> {
        Ok(predicate.tokens().first().is_some_and(|token| predicate.text_of(token) == self.cfg))
    }

    fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Ok(Included::Bytes(b"1"))
    }

    fn place(&self, what: PlaceOf) -> Result<Place<'_>, HostError> {
        Ok(match what {
            PlaceOf::Line => Place::Number(self.line),
            PlaceOf::Column => Place::Number(1),
            PlaceOf::File => Place::Text("src/lib.rs"),
            PlaceOf::ModulePath => Place::Text("demo"),
        })
    }

    fn run_proc_macro<'a>(&self, _: &MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        Err(HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused })
    }
}

const CX: Cx = Cx { var: Some("one"), cfg: "unix", line: 7 };

fn krate(byte: u8) -> CrateItemKey {
    CrateItemKey(ContentId([byte; 32]))
}

fn at() -> DefAt {
    DefAt { site: DefSite { krate: krate(1), module: None }, edition: Edition::E2021, export: Export::Local, local_inner: LocalInner::No, transparency: None }
}

fn tokens(text: &str) -> TokenStream<'static> {
    lex(text, Edition::E2021, Alloc::global()).expect("the test's text lexes")
}

fn fn_like(def: &MacroDefinition<'static>, args: &str, cx: &Cx) -> Expansion<'static> {
    let input = tokens(args);
    let site = CallSite { edition: Edition::E2021, panic_edition: Edition::E2021, ctx: Ctx::root(Edition::E2021) };
    match expand(def, &Call::fn_like(&input, site), cx, Alloc::global()).expect("the call expands") {
        Expanded::Expansion(expansion) => expansion,
        Expanded::NeedsExpanded(_) => panic!("the call is not eager"),
    }
}

const BODY: &str = "($name:ident) => { pub struct $name; impl $name { pub fn new() -> $name { $name } } };";

fn call(input: &'static str) -> TextCall<'static> {
    TextCall {
        def: DefText::Rules { body: BODY },
        def_name: "make",
        def_krate: krate(1),
        def_edition: Edition::E2021,
        export: Export::Local,
        local_inner: LocalInner::No,
        shape: CallShape::FnLike,
        attr: "",
        edition: Edition::E2021,
        input,
    }
}

/// Three calls' entries, one with asked atoms.
fn entries() -> Vec<(TextKey, Vec<u8>)> {
    let def = MacroDefinition::rules(tokens(BODY), at());
    ["One", "Two", "Three"]
        .into_iter()
        .map(|input| {
            let mut asked = Asked::new_in(Alloc::global());
            if input == "Two" {
                asked.push("unix", None, true).expect("fits");
                asked.push("feature", Some("std"), false).expect("fits");
            }
            let entry = write_entry(&asked, &fn_like(&def, input, &CX), Alloc::global()).expect("an entry");
            (text_key(&call(input)), entry.to_vec())
        })
        .collect()
}

/// A set is the same bytes in whatever order its entries were made (rule 25), and every entry
/// is read back in place: its asked atoms as pushed, its expansion equal to the one written
/// (written again, the same record). A key not in the set finds nothing.
#[test]
fn a_set_is_order_free_and_reads_every_entry_back() {
    let made = entries();
    let mut forward: Vec<(TextKey, &[u8])> = made.iter().map(|(key, bytes)| (*key, &bytes[..])).collect();
    let mut backward: Vec<(TextKey, &[u8])> = forward.iter().rev().copied().collect();
    // The same call made twice (two calls of one text in a crate) is one entry.
    backward.push(forward[0]);
    let set = write_set(&mut forward, Alloc::global()).expect("a set");
    assert_eq!(set[..], write_set(&mut backward, Alloc::global()).expect("a set")[..]);
    let aligned = Aligned::copy_from(&set);
    let opened = KeptSet::open(&aligned).expect("the set opens");
    assert_eq!(opened.len(), 3);
    for (key, bytes) in &made {
        let found = opened.find(key).expect("the key is in the set");
        assert_eq!(found, &bytes[..]);
        let entry = KeptEntry::open(found).expect("the entry opens");
        let expansion = entry.expansion(Alloc::global()).expect("the expansion reads back");
        assert_eq!(expansion.write_record(Alloc::global())[..], *entry.record(), "a kept expansion written again is the same record");
        let asked: Vec<(&str, Option<&str>, bool)> = entry.asked().map(|a| (a.name, a.value, a.answer)).collect();
        match *key == text_key(&call("Two")) {
            true => assert_eq!(asked, [("unix", None, true), ("feature", Some("std"), false)]),
            false => assert!(asked.is_empty()),
        }
    }
    assert!(opened.find(&text_key(&call("Four"))).is_none());
}

/// Two expansions under one key are refused by name (rule 21), never one of them picked; a
/// buffer that is not a set, or is cut, is refused too.
#[test]
fn two_values_under_one_key_and_a_cut_set_are_refused() {
    let made = entries();
    let mut clash: Vec<(TextKey, &[u8])> = vec![(made[0].0, &made[0].1[..]), (made[0].0, &made[1].1[..])];
    assert_eq!(write_set(&mut clash, Alloc::global()).err(), Some(KeptError::TwoValues { key: made[0].0 }));
    let mut all: Vec<(TextKey, &[u8])> = made.iter().map(|(key, bytes)| (*key, &bytes[..])).collect();
    let set = write_set(&mut all, Alloc::global()).expect("a set");
    let cut = Aligned::copy_from(&set[..set.len() - 1]);
    assert_eq!(KeptSet::open(&cut).err(), Some(KeptError::EntryEnd { index: 2 }));
    let mut other = set.to_vec();
    other[0] ^= 1;
    assert_eq!(KeptSet::open(&Aligned::copy_from(&other)).err(), Some(KeptError::Magic));
}

/// Every input a text key names moves it: the definition's text, name, crate and edition, its
/// export and `local_inner_macros`, the call's shape, attribute, edition and input.
#[test]
fn every_input_of_a_text_call_moves_its_key() {
    let base = call("One");
    let moved = [
        TextCall { def: DefText::Rules { body: "() => {};" }, ..base },
        TextCall { def: DefText::BuiltinDerive, ..base },
        TextCall { def_name: "other", ..base },
        TextCall { def_krate: krate(2), ..base },
        TextCall { def_edition: Edition::E2018, ..base },
        TextCall { export: Export::Exported, ..base },
        TextCall { local_inner: LocalInner::Yes, ..base },
        TextCall { shape: CallShape::Attr, ..base },
        TextCall { attr: "path::attr", ..base },
        TextCall { edition: Edition::E2018, ..base },
        TextCall { input: "Two", ..base },
    ];
    let mut keys: Vec<TextKey> = moved.iter().map(text_key).collect();
    keys.push(text_key(&base));
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), moved.len() + 1, "two of the calls share a key");
    assert_eq!(text_key(&base), text_key(&call("One")));
}

/// A read list holds while every read is answered as it was, names the first read answered
/// otherwise (the planted change: the variable's value, the cfg, the line), and says which
/// read it cannot ask again (an included file).
#[test]
fn a_read_list_is_asked_again_and_names_what_moved() {
    let env = MacroDefinition::builtin_fn(BuiltinFn::Env, at(), Alloc::global());
    let cfg = MacroDefinition::builtin_fn(BuiltinFn::Cfg, at(), Alloc::global());
    let include = MacroDefinition::builtin_fn(BuiltinFn::IncludeStr, at(), Alloc::global());
    let mut reads = fn_like(&env, "\"VAR\"", &CX).reads;
    reads.extend(&fn_like(&cfg, "unix", &CX).reads).expect("fits");
    assert_eq!(reads.list().iter().map(|r| r.kind).collect::<Vec<_>>(), [ReadKind::Env, ReadKind::Cfg]);
    let hold = |cx: &Cx| reads.hold(cx, Edition::E2021, Alloc::global()).expect("the host answers");
    assert_eq!(reads.askable(), Ok(()));
    assert_eq!(hold(&CX), Held::Holds);
    assert_eq!(hold(&Cx { var: Some("two"), ..CX }), Held::Moved { read: 0 });
    assert_eq!(hold(&Cx { var: None, ..CX }), Held::Moved { read: 0 });
    assert_eq!(hold(&Cx { cfg: "windows", ..CX }), Held::Moved { read: 1 });
    let included = fn_like(&include, "\"a.txt\"", &CX).reads;
    assert_eq!(included.askable(), Err((0, ReadKind::IncludeStr)));
    assert_eq!(included.hold(&CX, Edition::E2021, Alloc::global()), Ok(Held::NotAskable { read: 0, kind: ReadKind::IncludeStr }));
}
