//! A crate's kept expansions as one set (`blaze_expand::kept`), the one key of a call's
//! expansion (`call_key`), and a read list asked again (`Reads::hold`).

#![feature(allocator_api)]

use blaze_expand::kept::{Asked, KeptEntry, KeptError, KeptSet, SetPlant, SetSource, write_entry, write_set, write_set_planted};
use blaze_expand::{
    BuiltinFn, Call, CallKey, CallSite, Ctx, DefAt, DefSite, Edition, ExpandCx, Expanded, Expansion, Export, Held, HostAsk,
    HostError, HostWhy, IncludeAs, Included, LocalInner, MacroDefinition, OriginMaps, Place, PlaceOf, ProcCall, Produced,
    ReadKind, SourceIndexes, TokenStream, call_key, expand, lex,
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
    lex(text, Edition::E2021, blaze_own::testing::heap().alloc()).expect("the test's text lexes")
}

fn fn_like(def: &MacroDefinition<'static>, args: &str, cx: &Cx) -> Expansion<'static> {
    let input = tokens(args);
    let site = CallSite { edition: Edition::E2021, panic_edition: Edition::E2021, ctx: Ctx::root(Edition::E2021) };
    match expand(def, &Call::fn_like(&input, site), cx, blaze_own::testing::heap().alloc()).expect("the call expands") {
        Expanded::Expansion(expansion) => expansion,
        Expanded::NeedsExpanded(_) => panic!("the call is not eager"),
    }
}

const BODY: &str = "($name:ident) => { pub struct $name; impl $name { pub fn new() -> $name { $name } } };";

/// The call site of every call here: a 2021 call written in source (the root context).
fn site() -> CallSite {
    CallSite { edition: Edition::E2021, panic_edition: Edition::E2021, ctx: Ctx::root(Edition::E2021) }
}

/// The key of `name!(input)` where `name` is `macro_rules! name { body }` at [`at`].
fn key_of(body: &str, input: &str) -> CallKey {
    let def = MacroDefinition::rules(tokens(body), at());
    call_key(&def, &Call::fn_like(&tokens(input), site()))
}

/// The key of a call of [`BODY`].
fn key(input: &str) -> CallKey {
    key_of(BODY, input)
}

/// Three calls' entries, one with asked atoms.
fn entries() -> Vec<(CallKey, Vec<u8>)> {
    let def = MacroDefinition::rules(tokens(BODY), at());
    ["One", "Two", "Three"]
        .into_iter()
        .map(|input| {
            let mut asked = Asked::new_in(blaze_own::testing::heap().alloc());
            if input == "Two" {
                asked.push("unix", None, true).expect("fits");
                asked.push("feature", Some("std"), false).expect("fits");
            }
            let entry = write_entry(&asked, &fn_like(&def, input, &CX), &same_maps(), blaze_own::testing::heap().alloc()).expect("an entry");
            (key(input), entry.to_vec())
        })
        .collect()
}

/// `made`'s entries as [`write_set`] takes them.
fn sources(made: &[(CallKey, Vec<u8>)]) -> Vec<(CallKey, SetSource<'_>)> {
    made.iter().map(|(key, bytes)| (*key, SetSource::Entry(&bytes[..]))).collect()
}

/// Whether every one of `made` reads back from `set` as it was put: found under its key, its
/// asked atoms equal, its expansion equal (`==`, and the same record written again).
fn reads_back(set: &KeptSet<'_>, made: &[(CallKey, Vec<u8>)]) -> bool {
    made.iter().all(|(key, bytes)| {
        let put = KeptEntry::open(bytes).expect("a made entry opens");
        let want = put.expansion(blaze_own::testing::heap().alloc()).expect("a made entry reads");
        let Ok(Some(entry)) = set.find(key) else { return false };
        let Ok(got) = entry.expansion(blaze_own::testing::heap().alloc()) else { return false };
        let record = |e: &Expansion<'_>| e.write_record(&same_maps(), blaze_own::testing::heap().alloc()).expect("its tokens make a token record");
        got == want
            && record(&got)[..] == record(&want)[..]
            && Iterator::eq(entry.asked(), put.asked())
    })
}

/// A set is the same bytes in whatever order its entries were made (rule 25), and every entry
/// is read back: its asked atoms as pushed, its expansion equal to the one written (written
/// again, the same record). A key not in the set finds nothing. The made entry itself reads
/// back too (its columns without a record root).
#[test]
fn a_set_is_order_free_and_reads_every_entry_back() {
    let made = entries();
    let mut forward = sources(&made);
    let mut backward: Vec<(CallKey, SetSource<'_>)> = forward.iter().rev().copied().collect();
    // The same call made twice (two calls of one text in a crate) is one entry.
    backward.push(forward[0]);
    let set = write_set(&mut forward, blaze_own::testing::heap().alloc()).expect("a set");
    assert_eq!(set[..], write_set(&mut backward, blaze_own::testing::heap().alloc()).expect("a set")[..]);
    let aligned = Aligned::copy_from(&set);
    let opened = KeptSet::open(&aligned).expect("the set opens");
    assert_eq!(opened.len(), 3);
    assert!(reads_back(&opened, &made), "every entry reads back as it was put");
    for (key, _) in &made {
        let entry = opened.find(key).expect("the entry opens").expect("the key is in the set");
        let asked: Vec<(&str, Option<&str>, bool)> = entry.asked().map(|a| (a.name, a.value, a.answer)).collect();
        match *key == self::key("Two") {
            true => assert_eq!(asked, [("unix", None, true), ("feature", Some("std"), false)]),
            false => assert!(asked.is_empty()),
        }
    }
    assert!(opened.find(&key("Four")).expect("nothing to open").is_none());
}

/// Two expansions under one key are refused by name (rule 21), never one of them picked: two
/// made ones, and a kept one beside a made one of another value. A buffer that is not a set, a
/// cut one, and one of the earlier layout are refused too, each by its own name.
#[test]
fn two_values_under_one_key_and_a_cut_set_are_refused() {
    let made = entries();
    let mut clash = vec![(made[0].0, SetSource::Entry(&made[0].1[..])), (made[0].0, SetSource::Entry(&made[1].1[..]))];
    assert_eq!(write_set(&mut clash, blaze_own::testing::heap().alloc()).err(), Some(KeptError::TwoValues { key: made[0].0 }));
    let set = write_set(&mut sources(&made), blaze_own::testing::heap().alloc()).expect("a set");
    let opened = KeptSet::open(&set).expect("the set opens");
    let kept = opened.find(&made[0].0).expect("opens").expect("found");
    let mut mixed = vec![(made[0].0, SetSource::Kept(kept)), (made[0].0, SetSource::Entry(&made[1].1[..]))];
    assert_eq!(write_set(&mut mixed, blaze_own::testing::heap().alloc()).err(), Some(KeptError::TwoValues { key: made[0].0 }));
    let mut agreeing = vec![(made[0].0, SetSource::Kept(kept)), (made[0].0, SetSource::Entry(&made[0].1[..]))];
    assert!(write_set(&mut agreeing, blaze_own::testing::heap().alloc()).is_ok(), "a kept entry and a made one of one value are one");
    assert!(matches!(KeptSet::open(&set[..set.len() - 1]).err(), Some(KeptError::Short { .. })));
    let mut other = set.to_vec();
    other[0] ^= 1;
    assert_eq!(KeptSet::open(&other).err(), Some(KeptError::Magic));
    other[..8].copy_from_slice(b"BZXSET1\0");
    assert_eq!(KeptSet::open(&other).err(), Some(KeptError::OldForm));
}

/// Every input `call_key` names moves it (rule 21): the definition's tokens, crate (what
/// `$crate` names), def-site module, edition, export and `local_inner_macros`; the call's
/// shape, edition, panic edition and input tokens. What is not an input does not: a comment or
/// a space in the definition or the input.
#[test]
fn every_input_of_a_call_moves_its_key() {
    let def_at = |at: DefAt, body: &str| MacroDefinition::rules(tokens(body), at);
    let fn_like = |def: &MacroDefinition<'_>, input: &str, site: CallSite| call_key(def, &Call::fn_like(&tokens(input), site));
    let base = def_at(at(), BODY);
    let module = blaze_id::ItemKey(ContentId([9; 32]));
    let moved = [
        fn_like(&def_at(at(), "() => {};"), "One", site()),
        fn_like(&def_at(DefAt { site: DefSite { krate: krate(2), module: None }, ..at() }, BODY), "One", site()),
        fn_like(&def_at(DefAt { site: DefSite { krate: krate(1), module: Some(module) }, ..at() }, BODY), "One", site()),
        fn_like(&def_at(DefAt { edition: Edition::E2018, ..at() }, BODY), "One", site()),
        fn_like(&def_at(DefAt { export: Export::Exported, ..at() }, BODY), "One", site()),
        fn_like(&def_at(DefAt { local_inner: LocalInner::Yes, ..at() }, BODY), "One", site()),
        call_key(&base, &Call::attr(None, &tokens("One"), site())),
        fn_like(&base, "One", CallSite { edition: Edition::E2018, ..site() }),
        fn_like(&base, "One", CallSite { panic_edition: Edition::E2018, ..site() }),
        fn_like(&base, "Two", site()),
    ];
    let mut keys: Vec<CallKey> = moved.to_vec();
    keys.push(key("One"));
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), moved.len() + 1, "two of the calls share a key");
    let spaced = BODY.replace("=>", " /* arm */  =>");
    assert_eq!(fn_like(&def_at(at(), &spaced), "One /* the name */", site()), key("One"), "a comment or a space moved the key");
}

/// A read list holds while every read is answered as it was, names the first read answered
/// otherwise (the planted change: the variable's value, the cfg, the line), and says which
/// read it cannot ask again (an included file).
#[test]
fn a_read_list_is_asked_again_and_names_what_moved() {
    let env = MacroDefinition::builtin_fn(BuiltinFn::Env, at(), blaze_own::testing::heap().alloc());
    let cfg = MacroDefinition::builtin_fn(BuiltinFn::Cfg, at(), blaze_own::testing::heap().alloc());
    let include = MacroDefinition::builtin_fn(BuiltinFn::IncludeStr, at(), blaze_own::testing::heap().alloc());
    let mut reads = fn_like(&env, "\"VAR\"", &CX).reads;
    reads.extend(&fn_like(&cfg, "unix", &CX).reads).expect("fits");
    assert_eq!(reads.list().iter().map(|r| r.kind).collect::<Vec<_>>(), [ReadKind::Env, ReadKind::Cfg]);
    let hold = |cx: &Cx| reads.hold(cx, Edition::E2021, blaze_own::testing::heap().alloc()).expect("the host answers");
    assert_eq!(reads.askable(), Ok(()));
    assert_eq!(hold(&CX), Held::Holds);
    assert_eq!(hold(&Cx { var: Some("two"), ..CX }), Held::Moved { read: 0 });
    assert_eq!(hold(&Cx { var: None, ..CX }), Held::Moved { read: 0 });
    assert_eq!(hold(&Cx { cfg: "windows", ..CX }), Held::Moved { read: 1 });
    let included = fn_like(&include, "\"a.txt\"", &CX).reads;
    assert_eq!(included.askable(), Err((0, ReadKind::IncludeStr)));
    assert_eq!(included.hold(&CX, Edition::E2021, blaze_own::testing::heap().alloc()), Ok(Held::NotAskable { read: 0, kind: ReadKind::IncludeStr }));
}

/// A derive-like definition: three impls over the same forty-odd identifiers and punctuation,
/// the type's name the only part that differs between calls.
const DERIVE: &str = "($name:ident) => { \
    impl ::core::clone::Clone for $name { #[inline] fn clone(&self) -> $name { let this = self; ::core::clone::Clone::clone(this) } } \
    impl ::core::fmt::Debug for $name { fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result { f.write_str(\"x\") } } \
    impl ::core::cmp::PartialEq for $name { #[inline] fn eq(&self, other: &$name) -> bool { let _ = other; true } } \
};";

fn derive_key(name: &str) -> CallKey {
    key_of(DERIVE, name)
}

/// `n` calls of [`DERIVE`] on `Name0`, `Name1`, ..., each with its key and entry.
fn derive_like(n: usize) -> Vec<(CallKey, Vec<u8>)> {
    let def = MacroDefinition::rules(tokens(DERIVE), at());
    (0..n)
        .map(|i| {
            let name = format!("Name{i}");
            let entry = write_entry(&Asked::new_in(blaze_own::testing::heap().alloc()), &fn_like(&def, &name, &CX), &same_maps(), blaze_own::testing::heap().alloc()).expect("an entry");
            (derive_key(&name), entry.to_vec())
        })
        .collect()
}

/// (i) A set in the one-root form answers every entry with the expansion that was put: a
/// thousand derive-like calls and the three of [`entries`] (one with asked atoms), each found
/// under its key with equal asked atoms and an equal expansion (`==`, and the same record
/// written again). Prints "every entry of a 1003-entry set reads back as it was put".
#[test]
fn every_entry_of_a_large_set_reads_back_as_it_was_put() {
    let mut made = derive_like(1000);
    made.extend(entries());
    let set = write_set(&mut sources(&made), blaze_own::testing::heap().alloc()).expect("a set");
    let opened = KeptSet::open(&set).expect("the set opens");
    assert_eq!(opened.len(), made.len());
    assert!(reads_back(&opened, &made), "an entry read back otherwise than it was put");
    println!("every entry of a {}-entry set reads back as it was put", opened.len());
}

/// (ii) The bytes are a function of the entries (rule 25): two making orders give equal sets,
/// and so do a set rewritten from its own entries (every call found again, the next solve's
/// common case) and one half found, half made. The atom and key tables are in first-use order
/// over the entries sorted by key, never over the order of making. Prints "two making orders, a
/// rewrite from the set and a half-found set: N bytes each, equal".
#[test]
fn two_making_orders_and_a_rewrite_from_the_set_give_equal_bytes() {
    let made = derive_like(200);
    let mut forward = sources(&made);
    let n = forward.len();
    // 7919 is prime and does not divide 200: a permutation.
    let mut shuffled: Vec<(CallKey, SetSource<'_>)> = (0..n).map(|i| forward[(i * 7919) % n]).collect();
    let set = write_set(&mut forward, blaze_own::testing::heap().alloc()).expect("a set");
    assert_eq!(set[..], write_set(&mut shuffled, blaze_own::testing::heap().alloc()).expect("a set")[..]);
    let opened = KeptSet::open(&set).expect("the set opens");
    let mut found: Vec<(CallKey, SetSource<'_>)> =
        (0..opened.len()).rev().map(|i| (opened.key(i), SetSource::Kept(opened.entry(i).expect("the entry opens")))).collect();
    assert_eq!(set[..], write_set(&mut found, blaze_own::testing::heap().alloc()).expect("a set")[..], "a set rewritten from itself is the same bytes");
    let mut half: Vec<(CallKey, SetSource<'_>)> = made
        .iter()
        .enumerate()
        .map(|(i, (key, bytes))| match i % 2 {
            0 => (*key, SetSource::Entry(&bytes[..])),
            _ => (*key, SetSource::Kept(opened.find(key).expect("the entry opens").expect("the key is in the set"))),
        })
        .collect();
    assert_eq!(set[..], write_set(&mut half, blaze_own::testing::heap().alloc()).expect("a set")[..], "half found, half made is the same bytes");
    println!("two making orders, a rewrite from the set and a half-found set: {} bytes each, equal", set.len());
}

/// (iii) A set cut anywhere (every prefix: each section boundary and every offset between) is
/// [`KeptError::Short`], a byte past its end [`KeptError::Trailing`], and a set with any one
/// byte changed either is refused or reads every entry without a panic (a typed error or some
/// expansion: the store's digest, not this reader, says the bytes are the ones written).
/// Prints "a changed byte: N of M sets still opened, every entry read without a panic".
#[test]
fn a_cut_or_changed_set_is_a_typed_refusal_never_a_panic() {
    let made = entries();
    let set = write_set(&mut sources(&made), blaze_own::testing::heap().alloc()).expect("a set");
    for len in 0..set.len() {
        let refused = KeptSet::open(&set[..len]).err();
        assert!(matches!(refused, Some(KeptError::Short { .. })), "the set cut at byte {len} of {}: {refused:?}", set.len());
    }
    let mut longer = set.to_vec();
    longer.push(0);
    assert_eq!(KeptSet::open(&longer).err(), Some(KeptError::Trailing));
    let mut opened_after_change = 0usize;
    for at in 0..set.len() {
        let mut changed = set.to_vec();
        changed[at] ^= 0xff;
        let Ok(opened) = KeptSet::open(&changed) else { continue };
        opened_after_change += 1;
        for index in 0..opened.len() {
            if let Ok(entry) = opened.entry(index) {
                let _ = entry.expansion(blaze_own::testing::heap().alloc());
                let _ = entry.asked().count();
            }
        }
    }
    println!("a changed byte: {opened_after_change} of {} sets still opened, every entry read without a panic", set.len());
}

/// (iv) The one-root form against the per-entry form it replaced, on 1,000 derive-like calls.
/// The old set (BZXSET1) is computed from its layout: a 16-byte head, 40 bytes a key and its
/// end, then each entry at a multiple of 16: its 8-byte head padded to 16 and the whole
/// expansion record (a blaze-format header, a 14-slice root, padded columns, every token's text
/// and each context's two 32-byte keys). The new one (BZXSET3, its tokens rows of the shared
/// `atoms` table) pays per entry 32 bytes of key, 48 of row and 4 of asked count beside its
/// columns; each token is a 4-byte atom id instead of its text, length and 4-byte kind (each
/// distinct token is one 12-byte atom and its text once a set); the context keys are LEB128
/// ids. Expected R about 0.6 here
/// (short texts, one context, the fixed 4-byte id and the origins kept as they are bound it);
/// asserted below 0.75. Prints "kept set: N bytes new, M bytes old (ratio R)".
#[test]
fn a_set_of_derive_like_expansions_is_a_fraction_of_the_old_form() {
    let made = derive_like(1000);
    let set = write_set(&mut sources(&made), blaze_own::testing::heap().alloc()).expect("a set");
    let mut end = 0usize;
    for (_, bytes) in &made {
        let expansion = KeptEntry::open(bytes).expect("opens").expansion(blaze_own::testing::heap().alloc()).expect("reads");
        // The old form held the record's columns: the record less its token record.
        let alloc = blaze_own::testing::heap().alloc();
        let record = expansion.write_record(&same_maps(), alloc).expect("its tokens make a token record");
        let tokens = expansion.token_record(&same_maps(), alloc).expect("its tokens make a token record");
        end = end.next_multiple_of(16) + 16 + record.len() - tokens.len();
    }
    let old = (16 + made.len() * 40).next_multiple_of(16) + end;
    let ratio = set.len() as f64 / old as f64;
    println!("kept set: {} bytes new, {old} bytes old (ratio {ratio:.2})", set.len());
    assert!(ratio < 0.75, "the one-root set is {ratio:.2} of the per-entry form");
    assert!(reads_back(&KeptSet::open(&set).expect("the set opens"), &made));
}

/// (v) The planted bug: the first entry's `Tokens` row ends one token late. The set still opens
/// (its rows are in order and inside their columns), and the round trip of (i) catches it: the
/// first entry takes the second's first token. Prints "planted row off by one: caught by the
/// round trip".
#[test]
fn a_row_off_by_one_is_caught_by_the_round_trip() {
    let made = derive_like(3);
    let set = write_set(&mut sources(&made), blaze_own::testing::heap().alloc()).expect("a set");
    assert!(reads_back(&KeptSet::open(&set).expect("the set opens"), &made), "the unplanted set reads back");
    let planted = write_set_planted(&mut sources(&made), SetPlant::RowOffByOne, blaze_own::testing::heap().alloc()).expect("a planted set");
    let opened = KeptSet::open(&planted).expect("the planted set opens: its rows are in order");
    assert!(!reads_back(&opened, &made), "the planted row was not caught");
    println!("planted row off by one: caught by the round trip");
}

/// How an asker of a call keys it: by [`call_key`] (both layers today), or the planted form of
/// the two-key past, a key that also holds where the call is written.
#[derive(Clone, Copy, PartialEq)]
enum Keying {
    One,
    PlantedWithPlace,
}

/// One asker of a call: keys it, takes the kept expansion under its key when there is one, else
/// expands it (counted in `runs`) and keeps it. Returns the expansion's record bytes.
fn ask(
    def: &MacroDefinition<'static>,
    input: &TokenStream<'static>,
    place: u8,
    keying: Keying,
    kept: &mut Vec<(CallKey, Vec<u8>)>,
    runs: &mut u32,
) -> Vec<u8> {
    let call = Call::fn_like(input, site());
    let mut key = call_key(def, &call);
    if keying == Keying::PlantedWithPlace {
        key.0.0[31] ^= place;
    }
    if !kept.is_empty() {
        let set = write_set(&mut sources(kept), blaze_own::testing::heap().alloc()).expect("a set");
        let aligned = Aligned::copy_from(&set);
        let opened = KeptSet::open(&aligned).expect("the set opens");
        if let Some(entry) = opened.find(&key).expect("the entry opens") {
            let sources = SourceIndexes::of(def, &call, blaze_own::testing::heap().alloc());
            let expansion = entry.expansion(blaze_own::testing::heap().alloc()).expect("the kept expansion reads");
            return expansion.write_record(&sources.maps(), blaze_own::testing::heap().alloc()).expect("its tokens make a token record").to_vec();
        }
    }
    *runs += 1;
    let expansion = match expand(def, &call, &CX, blaze_own::testing::heap().alloc()).expect("the call expands") {
        Expanded::Expansion(expansion) => expansion,
        Expanded::NeedsExpanded(_) => panic!("the call is not eager"),
    };
    let sources = SourceIndexes::of(def, &call, blaze_own::testing::heap().alloc());
    let entry = write_entry(&Asked::new_in(blaze_own::testing::heap().alloc()), &expansion, &sources.maps(), blaze_own::testing::heap().alloc()).expect("an entry");
    kept.push((key, entry.to_vec()));
    expansion.write_record(&sources.maps(), blaze_own::testing::heap().alloc()).expect("its tokens make a token record").to_vec()
}

/// Each source token `n` as record token `n`: the maps of sources with no lifetime and no
/// invisible group. For a comparison of two records' bytes, which reads no origin as a place.
static SAME: [u32; 512] = {
    let mut map = [0; 512];
    let mut n = 0;
    while n < map.len() {
        map[n] = n as u32;
        n += 1;
    }
    map
};

fn same_maps() -> OriginMaps<'static> {
    OriginMaps { input: Some(&SAME), attr_args: Some(&SAME), body: Some(&SAME), params: Some(&SAME) }
}

/// One call, one key, one expansion. The names library keys a statement call from its source
/// text, lexed alone at the root context (blaze-scope resolve.rs `expand_decls`); the lowering
/// keys the same call from the tokens of the body it lowers, copied out of the body's stream
/// (blaze-lower by_name.rs `ByName::call`). Both key by `call_key`, so the call asked by the
/// names library and then by the lowering is expanded ONCE (the counter of expander runs) and
/// both see byte-equal output. The planted form (a key that also holds where the call is, as
/// two layers' keys of one call differed before) runs it twice. Prints
/// "one key: a call asked by the names library and the lowering is expanded once".
#[test]
fn a_call_asked_by_the_names_library_and_the_lowering_is_expanded_once() {
    let def = MacroDefinition::rules(tokens(BODY), at());
    let names_input = blaze_expand::lex_inside("Kept", Edition::E2021, blaze_own::testing::heap().alloc()).expect("the input lexes");
    // The cause: the input lexed as a stream of its own ends `Alone`, not joint with the
    // close delimiter the body's stream holds after it: another key.
    assert_ne!(call_key(&def, &Call::fn_like(&tokens("Kept"), site())), call_key(&def, &Call::fn_like(&names_input, site())));
    let body = tokens("fn f() { make!(Kept); }");
    let at_name = (0..body.tokens().len()).find(|&i| body.text_at(i) == "Kept").expect("the body holds the call's input");
    let lowering_input = body.copy_in(at_name..at_name + 1, blaze_own::testing::heap().alloc()).expect("the input copies");
    for (keying, want_runs) in [(Keying::One, 1), (Keying::PlantedWithPlace, 2)] {
        let mut kept = Vec::new();
        let mut runs = 0;
        let by_names = ask(&def, &names_input, 1, keying, &mut kept, &mut runs);
        let by_lowering = ask(&def, &lowering_input, 2, keying, &mut kept, &mut runs);
        assert_eq!(by_names, by_lowering, "the two layers saw different expansions of one call");
        assert_eq!(runs, want_runs, "expander runs for one call asked twice");
    }
    println!("one key: a call asked by the names library and the lowering is expanded once");
}

/// A kept entry carries its expansion's token record (D122): opened in place from a 16-aligned
/// copy of the entry, it is byte for byte the expansion's own `token_record` with the call's
/// maps, and its digest equals that record's. A byte of its padding changed is refused by name
/// (`KeptError::TokenPad`). Prints "kept entry: token record of N tokens opens in place, equal".
#[test]
fn a_kept_entry_carries_its_token_record() {
    let def = MacroDefinition::rules(tokens(BODY), at());
    let input = tokens("Carried");
    let call = Call::fn_like(&input, site());
    let expansion = match expand(&def, &call, &CX, blaze_own::testing::heap().alloc()).expect("the call expands") {
        Expanded::Expansion(expansion) => expansion,
        Expanded::NeedsExpanded(_) => panic!("the call is not eager"),
    };
    let sources = SourceIndexes::of(&def, &call, blaze_own::testing::heap().alloc());
    let entry = write_entry(&Asked::new_in(blaze_own::testing::heap().alloc()), &expansion, &sources.maps(), blaze_own::testing::heap().alloc()).expect("an entry");
    let aligned = Aligned::copy_from(&entry);
    let opened = KeptEntry::open(&aligned).expect("the entry opens");
    let tokens = opened.tokens().expect("the token record opens in place");
    let want = expansion.token_record(&sources.maps(), blaze_own::testing::heap().alloc()).expect("its tokens make a token record");
    let want_view = blaze_tokens::Tokens::open(&Aligned::copy_from(&want)).map(|v| (v.len(), v.digest(v.all()))).expect("the record opens");
    assert_eq!((tokens.len(), tokens.digest(tokens.all())), want_view, "the entry's token record is not the expansion's");
    assert_eq!(opened.expansion(blaze_own::testing::heap().alloc()).expect("the expansion reads"), expansion);
    assert_eq!((entry.len() - want.len()) % 16, 0, "the token record is not at a multiple of 16");
    println!("kept entry: token record of {} tokens opens in place, equal", tokens.len());
}
