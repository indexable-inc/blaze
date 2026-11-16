//! The vtable's shape, bytes and key against a hand-written fixture of the facts the checker
//! gives (codegen.md, "Trait objects: vtables and dynamic calls").

use std::alloc::Global;

use blaze_ids::{ContentId, Hasher};
use blaze_own::Alloc;
use blaze_typeck::{ItemKey, PNode, PortableTys, UintTy};

use super::{HEADER_WORDS, ObjectMethod, ObjectTrait, Shape, Upcast, Vtable, VtableTarget, Word, vtable_key};
use crate::Instance;

const Z: ItemKey = ItemKey(0x10);
const A: ItemKey = ItemKey(0x20);
const B: ItemKey = ItemKey(0x30);
const C: ItemKey = ItemKey(0x40);

fn m(item: u128, dispatch: bool) -> ObjectMethod {
    ObjectMethod { item: ItemKey(item), dispatch }
}

fn id(text: &str) -> ContentId {
    Hasher::new("vtable test shape").update(text.as_bytes()).finish()
}

/// The fixture: `trait Z { fn z1(&self); }`, `trait A: Z { fn a1(&self); fn a2<U>(&self); }`,
/// `trait B: Z { fn b1(&self); fn b2(self) where Self: Sized; }`,
/// `trait C: A + B { fn c1(&self); [fn c2(&self);] }` (a diamond through `Z`).
struct Fixture {
    z: [ObjectMethod; 1],
    a: [ObjectMethod; 2],
    b: [ObjectMethod; 2],
    c: Vec<ObjectMethod>,
}

impl Fixture {
    fn new(c2: bool) -> Fixture {
        let mut c = vec![m(0x41, true)];
        if c2 {
            c.push(m(0x42, true));
        }
        Fixture { z: [m(0x11, true)], a: [m(0x21, true), m(0x22, false)], b: [m(0x31, true), m(0x32, false)], c }
    }
    fn facts(&self, t: ItemKey) -> Option<ObjectTrait<'_>> {
        Some(match t {
            Z => ObjectTrait { supers: &[], methods: &self.z, shape: id("z1") },
            A => ObjectTrait { supers: &[Z], methods: &self.a, shape: id("a1 a2<U>; z1") },
            B => ObjectTrait { supers: &[Z], methods: &self.b, shape: id("b1 b2:Sized; z1") },
            C => ObjectTrait { supers: &[A, B], methods: &self.c, shape: id(if self.c.len() == 2 { "c1 c2; a; b" } else { "c1; a; b" }) },
            _ => return None,
        })
    }
}

/// The order: supertraits depth first in declaration order, the principal last, a trait
/// reached twice once (`Z` through `B`), no slot for a generic method (`a2`) or one with
/// `where Self: Sized` (`b2`) (the Reference, "Traits", dyn compatibility); `C`'s first chain
/// `C, A, Z` is the prefix, and `B` (off it) gets one word after its methods.
#[test]
fn the_shape_follows_the_traits_text() {
    let f = Fixture::new(false);
    let s = Shape::of(C, |t| f.facts(t), Global).unwrap();
    let words: Vec<Word> = s.words.iter().copied().collect();
    assert_eq!(
        words,
        [
            Word::Method { trait_: Z, item: ItemKey(0x11) },
            Word::Method { trait_: A, item: ItemKey(0x21) },
            Word::Method { trait_: B, item: ItemKey(0x31) },
            Word::Super(B),
            Word::Method { trait_: C, item: ItemKey(0x41) },
        ]
    );
    assert_eq!((s.slot(ItemKey(0x11)), s.slot(ItemKey(0x21)), s.slot(ItemKey(0x31)), s.slot(ItemKey(0x41))), (Some(0), Some(1), Some(2), Some(4)));
    assert_eq!((s.slot(ItemKey(0x22)), s.slot(ItemKey(0x32))), (None, None), "no slot without dispatch");
    assert_eq!((s.upcast(A), s.upcast(Z), s.upcast(C)), (Some(Upcast::Prefix), Some(Upcast::Prefix), Some(Upcast::Prefix)));
    assert_eq!(s.upcast(B), Some(Upcast::Load(3)), "B is off the first chain: one load");
    assert_eq!(s.upcast(ItemKey(0x99)), None);
    // `A`'s own table is a prefix of `C`'s: an upcast keeps the pointer.
    let sa = Shape::of(A, |t| f.facts(t), Global).unwrap();
    assert_eq!(&s.words[..sa.words.len()], &sa.words[..]);
    assert!(Shape::of(ItemKey(0x99), |t| f.facts(t), Global).is_err(), "a trait with no facts is refused by name");
}

/// The record of `(u32, C)`: size 4 and alignment 4 in words 1 and 2, every function and
/// supertrait word a relocation (no address in the bytes); the same pair built twice gives
/// equal bytes, relocations and key; a type that needs drop gets its glue at word 0. Appending
/// `c2` to `C` adds one word at `C`'s end and changes `C`'s key (its shape), not `B`'s table.
#[test]
fn the_record_is_bytes_and_relocations_and_its_key_covers_the_shape() {
    let f = Fixture::new(false);
    let s = Shape::of(C, |t| f.facts(t), Global).unwrap();
    let mut g = PortableTys::new_in(Alloc::global());
    let u32_ = g.intern(PNode::Uint(UintTy::U32)).unwrap();
    let alone = g.intern_list(&[u32_]).unwrap();
    let build = |g: &mut PortableTys<'_>, drop: Option<Instance>| Vtable::build(&s, u32_, &[], 4, 4, drop, g, Global).unwrap();
    let t1 = build(&mut g, None);
    let t2 = build(&mut g, None);
    assert_eq!(t1.bytes.len() as u32, 8 * (HEADER_WORDS + 5));
    let mut want = vec![0u8; 64];
    want[8] = 4;
    want[16] = 4;
    assert_eq!(&t1.bytes[..], &want[..], "only the size and alignment are bytes");
    assert_eq!(&t1.bytes[..], &t2.bytes[..]);
    let relocs: Vec<(u32, VtableTarget)> = t1.relocs.iter().copied().collect();
    let at = |item: u128| VtableTarget::Fn(Instance { item: ItemKey(item), args: alone });
    assert_eq!(relocs, [(24, at(0x11)), (32, at(0x21)), (40, at(0x31)), (48, VtableTarget::Super(B)), (56, at(0x41))]);
    assert_eq!(&t1.relocs[..], &t2.relocs[..]);
    let glue = Instance { item: ItemKey(0x5), args: alone };
    let t3 = build(&mut g, Some(glue));
    assert_eq!(t3.relocs[0], (0, VtableTarget::Fn(glue)), "drop glue at word 0");
    assert_eq!(&t3.bytes[..], &t1.bytes[..], "the glue is a relocation, not bytes");

    let key = |g: &mut PortableTys<'_>, f: &Fixture, t| vtable_key(u32_, t, &[], f.facts(t).unwrap().shape, g, Alloc::global()).unwrap();
    let (kc, kb) = (key(&mut g, &f, C), key(&mut g, &f, B));
    assert_eq!(kc, key(&mut g, &f, C), "one pair, one key");
    assert_ne!(kc, kb);
    let f2 = Fixture::new(true);
    let s2 = Shape::of(C, |t| f2.facts(t), Global).unwrap();
    assert_eq!(&s2.words[..5], &s.words[..], "an appended method only adds a word at the end");
    assert_eq!(s2.words.len(), 6);
    assert_ne!(key(&mut g, &f2, C), kc, "C's tables change key");
    assert_eq!(key(&mut g, &f2, B), kb, "B's tables do not");
    let sb = Shape::of(B, |t| f.facts(t), Global).unwrap();
    let sb2 = Shape::of(B, |t| f2.facts(t), Global).unwrap();
    assert_eq!(&sb.words[..], &sb2.words[..]);
}

/// Places `(concrete, t)`'s table for the native run: the bytes as words, each method word a
/// stand-in address (`0x1000 + item`: nothing is called), each supertrait word the address of
/// that supertrait's own placed table. The tables live for the test (leaked).
#[cfg(target_arch = "aarch64")]
fn place_table(f: &Fixture, t: ItemKey, concrete: blaze_typeck::PTy, size: u64, align: u64, g: &mut PortableTys<'_>) -> u64 {
    let shape = Shape::of(t, |x| f.facts(x), Global).unwrap();
    let table = Vtable::build(&shape, concrete, &[], size, align, None, g, Global).unwrap();
    let words: Vec<u64> = table.bytes.chunks(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();
    let words: &'static mut [u64] = Vec::leak(words);
    for &(offset, to) in table.relocs.iter() {
        words[offset as usize / 8] = match to {
            VtableTarget::Fn(i) => 0x1000 + i.item.0 as u64,
            VtableTarget::Super(s) => place_table(f, s, concrete, size, align, g),
        };
    }
    words.as_ptr() as u64
}

/// The address of a vtable in generated code (codegen.md, "Trait objects": "A wide pointer ...
/// the vtable address at 8"; the Reference, "Trait objects": a pointer to a trait object
/// carries the vtable): `f() = data_addr(vtable place 0)`, emitted, its `RelocTo::Vtable(0)`
/// word filled with the placed table of `(24-byte T, C)`, run natively: the returned address
/// is the table's, and words 1 and 2 there are T's size and alignment; word 6 (`B`'s, off the
/// first chain) is the address of `(T, B)`'s table, whose words 1 and 2 agree.
#[test]
#[cfg(target_arch = "aarch64")]
fn a_vtable_address_in_code_is_a_relocation_to_the_placed_table() {
    use blaze_emit::{Code, Isa, Os, RelocTo, emit};
    use blaze_inproc::exec::Executable;
    use blaze_lir::builder::{Param, Variadic};
    use blaze_lir::func::{BlockKind, Conv, RefKind};
    use blaze_lir::{Builder, Full, Ty};

    let f = Fixture::new(false);
    let mut g = PortableTys::new_in(Alloc::global());
    let concrete = g.intern(PNode::Uint(UintTy::U64)).unwrap();
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::PTR)]);
    b.set_signature(sig);
    let vt = b.reference(RefKind::Vtable, 0);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let at = b.data_addr(vt);
    b.ret(&[at]);
    let func = b.finish_checked().expect("builds");
    assert!(blaze_lir::print(func.view()).contains("= vtable 0"), "{}", blaze_lir::print(func.view()));
    let mut code = Code::new_in(Global);
    emit(func.view(), &[], Isa { lse: true, os: Os::MacOs }, &mut code).expect("emits");
    let named: Vec<RelocTo> = code.relocs.iter().map(|r| r.to).collect();
    assert_eq!(named, [RelocTo::Vtable(0)], "one literal word, the table's");
    let table = place_table(&f, C, concrete, 24, 8, &mut g);
    let exec = Executable::written(code.bytes.len(), 0, |out, _| {
        let base = out.as_ptr() as u64;
        out.copy_from_slice(&code.bytes);
        for r in code.relocs.iter() {
            let to = match r.to {
                RelocTo::Vtable(0) => table,
                RelocTo::Code(off) => base + u64::from(off),
                other => panic!("no {other:?} here"),
            };
            out[r.at as usize..r.at as usize + 8].copy_from_slice(&to.wrapping_add_signed(r.addend).to_le_bytes());
        }
    })
    .expect("the code maps");
    // SAFETY: the code is `fn() -> ptr` of the C convention, mapped and held by `exec`.
    let run = unsafe { std::mem::transmute::<usize, extern "C" fn() -> u64>(exec.code_address() as usize) };
    let got = run();
    assert_eq!(got, table);
    // SAFETY: `got` is the leaked 8-word table `place_table` made (header 3 + 5 words).
    let words = unsafe { core::slice::from_raw_parts(got as *const u64, 8) };
    assert_eq!((words[0], words[1], words[2]), (0, 24, 8), "no glue; size 24; alignment 8");
    assert_eq!(words[3], 0x1000 + 0x11, "z1 first: the first chain's root");
    // SAFETY: word 6 is the address of `(u64, B)`'s leaked table (header 3 + b1, z1 is shared: 2 words).
    let b_words = unsafe { core::slice::from_raw_parts(words[6] as *const u64, 5) };
    assert_eq!((b_words[1], b_words[2]), (24, 8));
}
