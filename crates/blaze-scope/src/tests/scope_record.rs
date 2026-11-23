//! A `CrateScope` exports record read in place (crate::scope_record) against the decode it
//! replaced (crate_scope.rs `read_record`, kept as the control form, rule 78).
//!
//! (a) Equality. Three crates (k2 reaches k0 through k1: globs, a re-exported enum with record
//! and tuple variants, `#[macro_export]` and 2.0 macros, nested modules) are solved twice, once
//! with every handed record decoded ([`Form::Heap`]) and once read in place ([`Form::InPlace`]):
//! the two solves write equal exports records, inner records and read lists. Then each record
//! (the three, an error crate's, and generated ones: many names, a long chain, many globs, many
//! macros of every body, many enums, many reaches, all empty) is read both ways, and every read a
//! dependent makes (the head, the reach list, each module's scope by every name in every
//! namespace, its globs, chain, textual scope and marks, each enum's variants and each name
//! found in them, each macro's source, and keys the record does not hold) answers alike.
//! (b) Determinism: the same parts given in other orders, with repeated reaches, write equal
//! bytes; a key twice is a typed refusal.
//! (c) Hostile bytes: every truncation, each header count and offset set past the end, a byte
//! long or u32::MAX, a row's count one too many, and every single byte flipped: a typed refusal
//! naming the table and offset, or (a flip the check cannot see, inside a name) a record whose
//! every read still answers. Never a panic.
//! (d) Allocation: handing a record in place is one small box (the `DepRecord`), and its check
//! and 10,000 lookups take no allocator at all; the control decode allocates per table. The
//! floor (rule 67) is printed, one line: `SCOPE RECORD IN PLACE: ...`.

use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::testing::Counting;
use blaze_own::{AVec, Alloc, Box, Erased};
use blaze_surface::{
    Binding, BindingState, BuildError, CallShape, Def, DefKind, Edition, GlobEdge, Namespace, Origin, ProcKind, RecordBytes,
    ScopeBuilder, ScopeEntry, Visibility,
};

use super::entry::{Texts, input};
use crate::crate_scope::{Dep, DepScope, Form, Ready, RecordRead, error_record, hand, read_record, resolve_by_records_in};
use crate::graph::Sysroot;
use crate::resolve::aligned;
use crate::scope_record::{DepRecord, Module, Parts, Verified, write};
use crate::task::{MacroBody, MacroSrc, Serial, Variant};

const WAIT: std::time::Duration = std::time::Duration::from_secs(60);

const K0: &str = "#![no_core]
#![feature(no_core, decl_macro)]
pub mod a {
    pub struct S;
    pub mod b {
        pub fn f() {}
        pub use super::S as T;
        pub(crate) fn hidden() {}
    }
    fn private() {}
}
pub mod c {
    pub use crate::a::b::*;
    pub use crate::a::*;
    pub enum E { V, W {}, X() }
}
pub use c::E;
#[macro_export]
macro_rules! mac { () => {} }
pub macro m2($x:expr) { $x }
pub mod deep {
    pub mod deeper {
        pub mod deepest {
            pub struct Z;
        }
    }
}
";

const K1: &str = "#![no_core]
pub mod m {
    pub use k0::c::E;
    pub use k0::c::*;
    pub fn g() {}
}
pub use m::g;
pub use k0::deep::deeper::deepest::Z;
k0::mac!();
";

const K2: &str = "#![no_core]
pub use k1::m::*;
pub use k1::Z;
pub mod own {
    pub enum Mine { One, Two {} }
}
";

static TEXTS: Texts = Texts(&[&[("/k0/lib.rs", K0)], &[("/k1/lib.rs", K1)], &[("/k2/lib.rs", K2)]]);

/// The section names a refusal names, in the record's order (scope_record.rs `SECTIONS`).
const SECTIONS: [&str; 8] = ["reaches", "module rows", "chain keys", "enum rows", "variant rows", "macro rows", "strings", "blob"];
/// Where section `i`'s count is in the header; its offset is the next four bytes.
const fn field(i: usize) -> usize {
    116 + 8 * i
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn set32(bytes: &mut [u8], at: usize, v: u32) {
    bytes[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn key(tag: &str, i: usize) -> ItemKey {
    let mut bytes = [0u8; 40];
    bytes[..8].copy_from_slice(&(i as u64).to_le_bytes());
    let tag = tag.as_bytes();
    let n = tag.len().min(32);
    bytes[8..8 + n].copy_from_slice(&tag[..n]);
    ItemKey(ContentId::of("blaze-scope scope_record test", &bytes))
}

fn crate_key(i: usize) -> CrateItemKey {
    CrateItemKey(key("crate", i).0)
}

/// `n` distinct names, `prefix` then `i` in base 26, in one buffer of `g`.
fn names(prefix: &str, n: usize, g: Alloc<'static>) -> &'static [&'static str] {
    let mut buf: AVec<'static, u8> = AVec::new_in(g);
    let mut ends: AVec<'static, usize> = AVec::with_capacity_in(n, g);
    for i in 0..n {
        buf.extend_from_slice(prefix.as_bytes());
        let mut x = i;
        loop {
            buf.push(b'a' + (x % 26) as u8);
            x /= 26;
            if x == 0 {
                break;
            }
        }
        ends.push(buf.len());
    }
    let buf: &'static [u8] = buf.leak_static();
    let mut out = AVec::with_capacity_in(n, g);
    let mut start = 0;
    for &end in ends.iter() {
        out.push(core::str::from_utf8(&buf[start..end]).unwrap_or_else(|e| panic!("a generated name is not UTF-8: {e}")));
        start = end;
    }
    out.leak_static()
}

fn binding(def: ItemKey, i: usize) -> Binding {
    let vis = match i % 3 {
        0 => Visibility::Public,
        1 => Visibility::Crate(crate_key(0)),
        _ => Visibility::Restricted(key("restricted", i)),
    };
    let origin = if i % 2 == 0 { Origin::Declared } else { Origin::Import };
    Binding { def: Def { key: def, kind: DefKind::Other }, vis, origin, expansion: (i % 3) as u16, state: BindingState::Determined }
}

/// A module's exports scope: `entries` (sorted here) and `globs` (sorted here), written in `g`.
fn scope_bytes(module: ItemKey, entries: &[(Namespace, &'static str, Binding)], globs: &[GlobEdge], g: Alloc<'static>) -> &'static [u8] {
    let mut s: ScopeBuilder<'static, &'static str> = ScopeBuilder::new_in(module, g);
    s.bindings.extend(entries.iter().map(|&(ns, name, binding)| ScopeEntry { ns, name, binding }));
    s.bindings.sort_unstable_by(|a, b| (a.ns, a.name).cmp(&(b.ns, b.name)));
    s.globs.extend_from_slice(globs);
    s.globs.sort_unstable();
    let bytes = s.write(g).unwrap_or_else(|e| panic!("a generated scope did not write: {e}"));
    bytes.leak()
}

fn record_of(parts: &Parts<'_>, g: blaze_own::Bump<'static>) -> &'static [u8] {
    let bytes = write(parts, g.alloc()).unwrap_or_else(|e| panic!("a generated record did not write: {e}"));
    aligned(&bytes, g)
}

fn base<'p>(name: &'p str, root: ItemKey) -> Parts<'p> {
    Parts { key: crate_key(7), root, edition: Edition::E2021, proc_key: None, errored: false, name, reaches: &[], modules: &[], enums: &[], macros: &[] }
}

/// The generated records: (what each is, its bytes, 16-aligned).
fn generated(bump: blaze_own::Bump<'static>) -> Vec<(&'static str, &'static [u8])> {
    let g = bump.alloc();
    let mut out = Vec::new();
    let root = key("root", 0);

    // Many names: one module, 1,000 in each namespace, one name of two-byte UTF-8.
    let many = names("n", 1000, g);
    let mut entries = Vec::new();
    for (i, n) in many.iter().enumerate() {
        for (j, ns) in [Namespace::Types, Namespace::Values, Namespace::Macros].into_iter().enumerate() {
            entries.push((ns, *n, binding(key("def", 3 * i + j), i + j)));
        }
    }
    entries.push((Namespace::Types, "é_ü", binding(key("def", 99_999), 0)));
    let modules = [Module { key: root, scope: scope_bytes(root, &entries, &[], g), chain: Some(&[]) }];
    out.push(("many names", record_of(&Parts { modules: &modules, ..base("many", root) }, bump)));

    // A long chain: 200 modules nested, each with one binding and a glob of its parent.
    let keys: Vec<ItemKey> = (0..200).map(|i| key("chain", i)).collect();
    let mut chains: Vec<Vec<ItemKey>> = Vec::new();
    for i in 0..keys.len() {
        chains.push((0..=i).rev().map(|j| keys[j]).collect());
    }
    let mut modules = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        let globs: Vec<GlobEdge> = (i > 0).then(|| GlobEdge { source: Def::module(keys[i - 1]), vis: Visibility::Public, expansion: 0 }).into_iter().collect();
        let scope = scope_bytes(*k, &[(Namespace::Types, "x", binding(key("x", i), i))], &globs, g);
        modules.push(Module { key: *k, scope, chain: (i % 7 != 3).then_some(chains[i].as_slice()) });
    }
    out.push(("long chain", record_of(&Parts { modules: &modules, ..base("chain", keys[0]) }, bump)));

    // Many globs: one module with 400, and three names.
    let globs: Vec<GlobEdge> = (0..400)
        .map(|i| GlobEdge { source: Def::module(key("source", i)), vis: if i % 2 == 0 { Visibility::Public } else { Visibility::Crate(crate_key(1)) }, expansion: (i % 4) as u16 })
        .collect();
    let entries = [("a", 0), ("b", 1), ("c", 2)].map(|(n, i)| (Namespace::Values, n, binding(key("g", i), i)));
    let modules = [Module { key: root, scope: scope_bytes(root, &entries, &globs, g), chain: None }];
    out.push(("many globs", record_of(&Parts { modules: &modules, ..base("globs", root) }, bump)));

    // Many macros: 600, every body, every flag, empty and long texts.
    let texts = names("text_", 600, g);
    let mut macros: Vec<(ItemKey, MacroSrc<'static>)> = Vec::new();
    for i in 0..600 {
        let t = texts[i];
        let body = match i % 9 {
            0 => MacroBody::Rules(t),
            1 => MacroBody::Macro2 { params: None, body: t },
            2 => MacroBody::Macro2 { params: Some(t), body: "" },
            3 => MacroBody::Macro2 { params: Some(""), body: t },
            4 => MacroBody::BuiltinFn(t),
            5 => MacroBody::BuiltinOther(if i % 2 == 0 { CallShape::Attr } else { CallShape::Derive }),
            6 => MacroBody::Proc { kind: ProcKind::FnLike, helpers: "" },
            7 => MacroBody::Proc { kind: ProcKind::Attr, helpers: t },
            _ => MacroBody::BuiltinDerive { helpers: t },
        };
        macros.push((key("macro", i), MacroSrc { name: texts[(i * 7) % 600], body, module: key("module", i % 5), export: i % 2 == 0, local_inner: i % 3 == 0 }));
    }
    let modules = [Module { key: root, scope: scope_bytes(root, &[], &[], g), chain: Some(&[]) }];
    out.push(("many macros", record_of(&Parts { modules: &modules, macros: &macros, ..base("macros", root) }, bump)));

    // Many enums: 300, of 0 to 8 variants, record ones among them.
    let vnames = names("V", 9, g);
    let variants: Vec<Vec<Variant<&'static str>>> =
        (0..300).map(|e| (0..e % 9).map(|v| Variant { name: vnames[v], key: key("variant", 9 * e + v), record: (e + v) % 2 == 1 }).collect()).collect();
    let enums: Vec<(ItemKey, &[Variant<&str>])> = variants.iter().enumerate().map(|(e, v)| (key("enum", e), v.as_slice())).collect();
    out.push(("many enums", record_of(&Parts { enums: &enums, ..base("enums", root) }, bump)));

    // Many reaches, a proc key, another edition.
    let reaches: Vec<CrateItemKey> = (0..500).map(|i| crate_key(100 + i)).collect();
    let modules = [Module { key: root, scope: scope_bytes(root, &[], &[], g), chain: Some(&[]) }];
    out.push((
        "many reaches",
        record_of(&Parts { reaches: &reaches, modules: &modules, proc_key: Some(crate_key(9)), edition: Edition::E2024, ..base("reaches", root) }, bump),
    ));

    // All empty: no module, enum, macro or reach, and an empty name.
    out.push(("empty", record_of(&base("", root), bump)));
    out
}

/// Every read a dependent makes of one record through `dep`, as text, the keys asked being
/// `modules`, `enums` and `macros` (each list sorted) and `absent`, and the names `names`.
fn reads<'d>(dep: Dep<'d>, modules: &[ItemKey], enums: &[ItemKey], macros: &[ItemKey], names: &[&'d str], absent: &[ItemKey]) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!("crate {:?} root {:?} name {:?} edition {:?} proc {:?}", dep.key(), dep.root(), dep.name(), dep.edition(), dep.proc_key()));
    let reaches = dep.reaches();
    out.push(format!("reaches {} {:?} {:?}", reaches.len(), reaches.iter().collect::<Vec<_>>(), reaches.get(reaches.len())));
    for m in modules.iter().chain(absent) {
        out.push(format!("chain {m:?} {:?}", dep.chain(*m)));
        let Some(s) = dep.scope(*m) else {
            out.push(format!("scope {m:?} none"));
            continue;
        };
        let globs = s.globs();
        out.push(format!("globs {m:?} {} {:?} {:?}", globs.len(), globs.iter().collect::<Vec<_>>(), globs.get(globs.len())));
        out.push(format!("textual {m:?} {:?} mark {:?}", s.textual(), modules.iter().map(|c| s.mark(*c)).collect::<Vec<_>>()));
        for ns in [Namespace::Types, Namespace::Values, Namespace::Macros] {
            for &n in names {
                out.push(format!("get {m:?} {ns:?} {n:?} {:?}", s.get(ns, n)));
            }
        }
    }
    for e in enums.iter().chain(absent) {
        let Some(v) = dep.enum_of(*e) else {
            out.push(format!("enum {e:?} none"));
            continue;
        };
        out.push(format!("enum {e:?} {} {:?} {:?}", v.len(), v.iter().collect::<Vec<_>>(), v.get(v.len())));
        for &n in names {
            out.push(format!("variant {e:?} {n:?} {:?}", v.find(n)));
        }
    }
    for d in macros.iter().chain(absent) {
        out.push(format!("macro {d:?} {:?}", dep.macro_of(*d)));
    }
    out
}

/// `bytes` read both ways, every read alike, and what it holds listed alike; `what` names it.
fn both_ways(what: &str, bytes: &'static [u8], bump: blaze_own::Bump<'static>) {
    let g = bump.alloc();
    let heap = read_record(bytes, g).unwrap_or_else(|e| panic!("{what}: the control decode refused: {e:?}"));
    let heap: &'static DepScope<'static> = bump.alloc_val(heap);
    let place = DepRecord::open(Verified::check(bytes).unwrap_or_else(|e| panic!("{what}: the check refused: {e:?}")));
    let sorted = |keys: &mut Vec<ItemKey>| keys.sort_unstable();
    let mut modules: Vec<ItemKey> = heap.scopes.keys().copied().collect();
    let mut enums: Vec<ItemKey> = heap.enums.keys().copied().collect();
    let mut macros: Vec<ItemKey> = heap.macros.keys().copied().collect();
    sorted(&mut modules);
    sorted(&mut enums);
    sorted(&mut macros);
    assert_eq!(modules, place.modules().collect::<Vec<_>>(), "{what}: the modules held differ");
    assert_eq!(enums, place.enums().collect::<Vec<_>>(), "{what}: the enums held differ");
    assert_eq!(macros, place.macros().collect::<Vec<_>>(), "{what}: the macros held differ");
    let mut probe: Vec<&str> = heap.scopes.values().flat_map(|s| s.bindings.iter().map(|e| e.name)).collect();
    probe.extend(heap.enums.values().flat_map(|v| v.iter().map(|v| v.name)));
    probe.extend(["missing", "", "é_ü"]);
    probe.sort_unstable();
    probe.dedup();
    let absent = [key("absent", 0), key("absent", 1)];
    let a = reads(Dep::Record(heap), &modules, &enums, &macros, &probe, &absent);
    let b = reads(Dep::InPlace(&place), &modules, &enums, &macros, &probe, &absent);
    assert_eq!(a.len(), b.len(), "{what}: the reads differ in number");
    let wrong: Vec<(&String, &String)> = a.iter().zip(b.iter()).filter(|(a, b)| a != b).take(5).collect();
    assert!(wrong.is_empty(), "{what}: the in-place record answers otherwise (decode, in place): {wrong:?}");
    assert_eq!(heap.errored, place.errored(), "{what}: the errored flag differs");
    assert_eq!(place.byte_len(), bytes.len());
}

#[test]
fn the_in_place_record_answers_as_the_decode_on_real_solves_and_generated_records() {
    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let inputs = [input("k0", "/k0/lib.rs", &[]), input("k1", "/k1/lib.rs", &[("k0", 0)]), input("k2", "/k2/lib.rs", &[("k1", 1)])];
    let solve = |form: Form| {
        resolve_by_records_in(&inputs, Sysroot::default(), &TEXTS, &Serial, &Ready::serial(WAIT), false, form, bump)
            .unwrap_or_else(|e| panic!("the solve with {form:?} records: {e:?}"))
            .0
    };
    let heap = solve(Form::Heap);
    let place = solve(Form::InPlace);
    let mut records = Vec::new();
    for (i, (h, p)) in heap.iter().zip(place.iter()).enumerate() {
        assert_eq!(&h.record[..], &p.record[..], "crate {i}: the exports records of the two forms differ");
        assert_eq!(&h.inner[..], &p.inner[..], "crate {i}: the inner records of the two forms differ");
        let read = |s: &crate::crate_scope::CrateScope<'_>| crate::asked::encode(inputs[i].key, &[], &s.asked, g);
        assert_eq!(&read(h)[..], &read(p)[..], "crate {i}: the read lists of the two forms differ");
        records.push((inputs[i].name, aligned(&p.record, bump)));
    }
    // k2 reaches k0 (its `Z` is k0's) through k1: the reach list is read in place.
    let k2 = DepRecord::open(Verified::check(records[2].1).unwrap_or_else(|e| panic!("k2: {e:?}")));
    assert!(k2.reaches().iter().any(|k| k == inputs[0].key), "k2's record does not reach k0");
    let broken = error_record(crate_key(3), "broken", Edition::E2021, g).unwrap_or_else(|e| panic!("the error record: {e}"));
    records.push(("an error crate", aligned(&broken, bump)));
    assert!(DepRecord::open(Verified::check(records[3].1).unwrap_or_else(|e| panic!("the error record: {e:?}"))).errored());
    records.extend(generated(bump));
    for (what, bytes) in records {
        both_ways(what, bytes, bump);
    }
}

#[test]
fn equal_parts_write_equal_bytes_in_any_order() {
    let g = blaze_own::testing::heap().alloc();
    let root = key("root", 0);
    let ks: Vec<ItemKey> = (0..20).map(|i| key("m", i)).collect();
    let chain: Vec<ItemKey> = ks.iter().rev().copied().collect();
    let modules: Vec<Module<'_>> = ks
        .iter()
        .enumerate()
        .map(|(i, k)| Module { key: *k, scope: scope_bytes(*k, &[(Namespace::Values, "v", binding(key("v", i), i))], &[], g), chain: (i % 2 == 0).then_some(&chain[..]) })
        .collect();
    let vs = [Variant { name: "A", key: key("a", 0), record: false }, Variant { name: "B", key: key("b", 0), record: true }];
    let enums: Vec<(ItemKey, &[Variant<&str>])> = (0..10).map(|i| (key("e", i), &vs[..i % 3])).collect();
    let macros: Vec<(ItemKey, MacroSrc<'_>)> =
        (0..10).map(|i| (key("mac", i), MacroSrc { name: "m", body: MacroBody::Rules("() => {}"), module: root, export: i % 2 == 0, local_inner: false })).collect();
    let reaches: Vec<CrateItemKey> = (0..10).map(crate_key).collect();
    let parts = Parts { modules: &modules, enums: &enums, macros: &macros, reaches: &reaches, ..base("order", root) };
    let first = write(&parts, g).unwrap_or_else(|e| panic!("{e}"));

    let (mut m2, mut e2, mut x2, mut r2) = (modules.clone(), enums.clone(), macros.clone(), reaches.clone());
    m2.reverse();
    e2.reverse();
    x2.reverse();
    r2.reverse();
    r2.extend_from_slice(&reaches[..4]);
    let second = write(&Parts { modules: &m2, enums: &e2, macros: &x2, reaches: &r2, ..parts }, g).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(&first[..], &second[..], "the same parts in other orders wrote other bytes");

    // A key twice is a typed refusal naming the table.
    let mut twice = modules.clone();
    twice.push(modules[3]);
    match write(&Parts { modules: &twice, ..parts }, g) {
        Err(BuildError::Twice { table, key }) => {
            assert_eq!(table, "CrateScope modules");
            assert_eq!(key, modules[3].key.0);
        }
        other => panic!("a module twice was not refused: {other:?}"),
    }
    let mut twice = macros.clone();
    twice.push(macros[0]);
    assert!(matches!(write(&Parts { macros: &twice, ..parts }, g), Err(BuildError::Twice { table: "CrateScope macros", .. })));
}

/// The table a refusal names, if it names one.
fn table_of(e: &RecordRead) -> Option<&'static str> {
    match e {
        RecordRead::Past { table, .. }
        | RecordRead::Layout { table, .. }
        | RecordRead::Misplaced { table, .. }
        | RecordRead::OutOfOrder { table, .. }
        | RecordRead::Utf8 { table, .. } => Some(table),
        _ => None,
    }
}

/// `bytes` as a record: refused, or every read answers (no panic either way).
fn check_or_read(bytes: &[u8]) -> Result<(), RecordRead> {
    let aligned_buf = crate::resolve::AlignedBuf::copy_from(bytes, Alloc::global());
    let bytes = aligned_buf.as_slice();
    let record = DepRecord::open(Verified::check(bytes)?);
    let modules: Vec<ItemKey> = record.modules().collect();
    let enums: Vec<ItemKey> = record.enums().collect();
    let macros: Vec<ItemKey> = record.macros().collect();
    let _ = reads(Dep::InPlace(&record), &modules, &enums, &macros, &["a", "S", "E", "V", "missing"], &[key("absent", 0)]);
    Ok(())
}

#[test]
fn hostile_bytes_are_refused_by_table_and_offset_never_a_panic() {
    let g = super::SyncFrame::leaked();
    let inputs = [input("k0", "/k0/lib.rs", &[])];
    let (solved, _) = resolve_by_records_in(&inputs, Sysroot::default(), &TEXTS, &Serial, &Ready::serial(WAIT), false, Form::InPlace, g)
        .unwrap_or_else(|e| panic!("k0: {e:?}"));
    let k0: Vec<u8> = solved[0].record.to_vec();
    let all = generated(g);
    let mut cases: Vec<(&str, Vec<u8>, usize)> = std::vec![("k0", k0, 1)];
    cases.extend(all.iter().map(|(what, b)| (*what, b.to_vec(), if b.len() > 8192 { 61 } else { 1 })));

    for (what, good, stride) in cases.iter() {
        assert!(check_or_read(good).is_ok(), "{what}: the good record is refused");
        // Every truncation (by `stride` in a large record, the last 256 always).
        for n in (0..good.len()).filter(|n| n % stride == 0 || good.len() - n <= 256) {
            assert!(check_or_read(&good[..n]).is_err(), "{what}: cut to {n} of {} bytes, accepted", good.len());
        }
        for i in 0..8 {
            let (count, at) = (le32(good, field(i)), le32(good, field(i) + 4));
            // A count one too many, past the end, or u32::MAX: refused, naming this section.
            for c in [count.wrapping_add(1), good.len() as u32, u32::MAX] {
                let mut bad = good.clone();
                set32(&mut bad, field(i), c);
                let e = check_or_read(&bad).err().unwrap_or_else(|| panic!("{what}: {} count {c} accepted", SECTIONS[i]));
                assert_eq!(table_of(&e), Some(SECTIONS[i]), "{what}: {} count {c}: {e:?}", SECTIONS[i]);
            }
            // An offset moved: refused, naming the section whose rows end there.
            for o in [at.wrapping_add(16), good.len() as u32 + 16, u32::MAX] {
                let mut bad = good.clone();
                set32(&mut bad, field(i) + 4, o);
                let e = check_or_read(&bad).err().unwrap_or_else(|| panic!("{what}: {} offset {o} accepted", SECTIONS[i]));
                let named = if i == 0 { "header" } else { SECTIONS[i - 1] };
                assert_eq!(table_of(&e), Some(named), "{what}: {} offset {o}: {e:?}", SECTIONS[i]);
            }
        }
        // The name's range past the strings.
        let mut bad = good.clone();
        set32(&mut bad, 112, u32::MAX);
        assert!(matches!(check_or_read(&bad), Err(RecordRead::Past { table: "crate name", at: 108 })), "{what}: a name past the strings");
        // A row lying about its run: the first enum's count, the first module's scope length.
        if le32(good, field(3)) > 0 {
            let row = le32(good, field(3) + 4) as usize;
            let mut bad = good.clone();
            set32(&mut bad, row + 36, le32(good, row + 36) + 1);
            let e = check_or_read(&bad).err().unwrap_or_else(|| panic!("{what}: an enum's count one too many, accepted"));
            assert!(matches!(table_of(&e), Some("variant rows" | "enum rows")), "{what}: {e:?}");
        }
        if le32(good, field(1)) > 0 {
            let row = le32(good, field(1) + 4) as usize;
            let mut bad = good.clone();
            set32(&mut bad, row + 36, le32(good, row + 36) + 16);
            assert!(check_or_read(&bad).is_err(), "{what}: a module scope 16 bytes long too many, accepted");
        }
        // Every byte flipped (by `stride` in a large record): refused or read, never a panic.
        for at in (0..good.len()).step_by(*stride) {
            let mut bad = good.clone();
            bad[at] ^= 0xff;
            let _ = check_or_read(&bad);
        }
    }
    // Not this record: another version, or a v4 record's head.
    let mut old = cases[0].1.clone();
    set32(&mut old, 4, 4);
    assert!(matches!(check_or_read(&old), Err(RecordRead::NotThisRecord)));
    // Unaligned bytes are refused before any is read: `aligned`'s copy starts on 16, so one
    // byte on does not.
    let a = aligned(&cases[0].1, g);
    assert!(matches!(Verified::check(&a[1..]), Err(RecordRead::Unaligned)));
}

#[test]
fn an_in_place_record_opens_and_answers_with_no_allocation() {
    struct CountingBump<'a>(Counting<&'a blaze_mem::Frame>);
    // SAFETY: delegates every call to the inner `Counting<&Frame>`.
    unsafe impl core::alloc::Allocator for CountingBump<'_> {
        fn allocate(&self, layout: core::alloc::Layout) -> Result<core::ptr::NonNull<[u8]>, core::alloc::AllocError> {
            self.0.allocate(layout)
        }
        unsafe fn deallocate(&self, ptr: core::ptr::NonNull<u8>, layout: core::alloc::Layout) {
            // SAFETY: caller's contract passed through.
            unsafe { self.0.deallocate(ptr, layout) }
        }
        unsafe fn grow(&self, ptr: core::ptr::NonNull<u8>, old: core::alloc::Layout, new: core::alloc::Layout) -> Result<core::ptr::NonNull<[u8]>, core::alloc::AllocError> {
            // SAFETY: caller's contract passed through.
            unsafe { self.0.grow(ptr, old, new) }
        }
        unsafe fn shrink(&self, ptr: core::ptr::NonNull<u8>, old: core::alloc::Layout, new: core::alloc::Layout) -> Result<core::ptr::NonNull<[u8]>, core::alloc::AllocError> {
            // SAFETY: caller's contract passed through.
            unsafe { self.0.shrink(ptr, old, new) }
        }
    }
    // SAFETY: backed by `blaze_mem::Frame`, which implements `BumpAlloc`.
    unsafe impl blaze_own::BumpAlloc for CountingBump<'_> {}

    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("test frame");
    let erased = Erased::new(&frame);
    let g = erased.bump();
    let inputs = [input("k0", "/k0/lib.rs", &[])];
    let (solved, _) = resolve_by_records_in(&inputs, Sysroot::default(), &TEXTS, &Serial, &Ready::serial(WAIT), false, Form::InPlace, g)
        .unwrap_or_else(|e| panic!("k0: {e:?}"));
    let bytes = aligned(&solved[0].record, g);
    let root = DepRecord::open(Verified::check(bytes).unwrap_or_else(|e| panic!("k0: {e:?}"))).root();
    let enum_key = DepRecord::open(Verified::check(bytes).unwrap_or_else(|e| panic!("k0: {e:?}"))).enums().next().unwrap_or_else(|| panic!("k0's record holds no enum"));

    let count_frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("counting frame");
    let counting = Erased::new(CountingBump(Counting::new(&count_frame)));
    let c = counting.bump();
    // Handing it in place: one box, the `DepRecord`.
    let before = counting.inner().0.allocations();
    let dep = hand(Form::InPlace, bytes, c).unwrap_or_else(|e| panic!("in place: {e:?}"));
    let handed = counting.inner().0.allocations() - before;
    assert_eq!(handed, 1, "handing a record in place allocated {handed} blocks, not its one box");

    // The check, the open and 10,000 lookups: none takes an allocator, none allocates here.
    const ROUNDS: u32 = 10_000;
    let before = counting.inner().0.allocations();
    let start = std::time::Instant::now();
    let proof = Verified::check(bytes).unwrap_or_else(|e| panic!("k0: {e:?}"));
    let checked = start.elapsed();
    let start = std::time::Instant::now();
    let mut hits = 0u32;
    for _ in 0..ROUNDS {
        let record = DepRecord::open(core::hint::black_box(proof));
        let scope = record.scope(core::hint::black_box(root));
        core::hint::black_box(record.chain(root));
        if scope.is_some_and(|s| s.view().get(Namespace::Types, core::hint::black_box("a")).is_some())
            && record.variants(enum_key).and_then(|v| v.find(core::hint::black_box("V"))).is_some()
            && dep.scope(root).is_some_and(|s| s.get(Namespace::Types, "c").is_some())
            && dep.reaches().is_empty()
        {
            hits += 1;
        }
    }
    let per = start.elapsed() / ROUNDS;
    let looked = counting.inner().0.allocations() - before;
    assert_eq!(hits, ROUNDS, "a lookup in k0's record missed");
    assert_eq!(looked, 0, "the check, open and lookups allocated {looked} blocks");

    // Control: the decode it replaced allocates per table, module and name.
    let before = counting.inner().0.allocations();
    let start = std::time::Instant::now();
    let decoded = read_record(bytes, c.alloc()).unwrap_or_else(|e| panic!("the control decode: {e:?}"));
    let decode = start.elapsed();
    let control = counting.inner().0.allocations() - before;
    assert!(control > 1, "control: the decode allocated {control} blocks, so the counter does not see it");
    assert_eq!(decoded.root, root);
    std::println!(
        "SCOPE RECORD IN PLACE: check {checked:?} once for {} bytes; open+4 lookups {per:?} per round over {ROUNDS}, {looked} allocations; handed in place {handed} box; control decode {decode:?}, {control} allocations",
        bytes.len()
    );
}
