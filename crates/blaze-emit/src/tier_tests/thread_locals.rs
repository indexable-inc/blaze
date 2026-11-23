//! `tls_addr` through the seam, by both tiers (the worklist's `tls_addr`, support/s5-thread-local).
//! tls_index_tests.rs's `bump() { N += 1; N }` of thread-local place 0, emitted by the Reference
//! tier (pass/tls_index.rs) and the Fast tier (compose/emit.rs `tls_addr`, table_gen/arm64.rs
//! `thread_locals`) with names that give the descriptor, the getter and place 0 a symbol each,
//! placed by the one new placer with their import words bound (the descriptor's address, the
//! getter's, index 0), and run three times on each of two threads: each thread counts 1, 2, 3
//! from its own copy, the getter is called once per thread (the later accesses take the in-line
//! path), and the two threads' tables differ.
//!
//! Prints nothing; passes as `test tier_tests::thread_locals::... ok`.

use blaze_code::image::ImageError;
use blaze_code::place::{At, PlaceError, Words};
use blaze_code::record::{Space, Symbol, Target};
use blaze_inproc::exec::Executable;
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::record::{Declare, DeclaredKind, Declares, declare, encode, place, read, seal};
use blaze_tier::{Emitted, Named, Names, Outside, RoutinePlaces, Table, TierName};

use super::{POLL_PAGE_ROOM, TierFn, carve_poll_page, fast, reference, table};
use crate::compose::emit::POLL_PAGE;
use crate::tls_index_tests::bump;
use crate::tls_index_tests::run::{Desc, GETTER_CALLS, getter, pthread_getspecific};

const DESC: &str = "blaze-emit tier_tests::thread_locals: the descriptor";
const GETTER: &str = "blaze-emit tier_tests::thread_locals: the getter";
const PLACE_0: &str = "blaze-emit tier_tests::thread_locals: thread-local place 0";

/// The names: the descriptor, the getter and thread-local place 0, each by its own symbol.
struct ThreadLocals;

impl Names for ThreadLocals {
    fn name(&self, of: Outside) -> Option<Named<'_>> {
        match of {
            Outside::TlsDesc => Some(Named::Symbol(Symbol::of_name(DESC))),
            Outside::TlsGetter => Some(Named::Symbol(Symbol::of_name(GETTER))),
            Outside::Tls(0) => Some(Named::Symbol(Symbol::of_name(PLACE_0))),
            Outside::Tls(_) | Outside::Func(_) | Outside::Static(_) | Outside::Vtable(_) => None,
        }
    }

    fn routines(&self) -> RoutinePlaces {
        RoutinePlaces::NONE
    }
}

/// The three targets' declared rows (blaze-tier record/declared.rs, D106), as a walk declares
/// them (blaze-ladder code_records.rs `instance_declares`: `tls_runtime`, `thread_local`): the
/// program's one descriptor and one getter (item 0, no name), and thread-local place 0 as the
/// thread-local of item [`PLACE_0_ITEM`].
impl Declares for ThreadLocals {
    fn declared(&self, target: Target) -> Option<Declare<'_>> {
        let row = |kind, item| Some(Declare { kind, item, name: &[] });
        match (target.space, target.symbol) {
            (Space::Static, s) if s == Symbol::of_name(DESC) => row(DeclaredKind::TlsDescriptor, 0),
            (Space::Func, s) if s == Symbol::of_name(GETTER) => row(DeclaredKind::TlsGetter, 0),
            (Space::Tls, s) if s == Symbol::of_name(PLACE_0) => row(DeclaredKind::ThreadLocal, PLACE_0_ITEM),
            _ => None,
        }
    }
}

/// The item key thread-local place 0 is declared as (any key: no walk follows it here).
const PLACE_0_ITEM: u128 = 1;

/// The three import words, in the placed data after the relro part.
struct Bound {
    page: u64,
    /// The descriptor's word, then the getter's, then place 0's.
    words: u64,
}

impl Words for Bound {
    fn word(&mut self, target: Target, _call: bool) -> Result<u64, ImageError> {
        match (target.space, target.symbol) {
            (Space::Static, s) if s == Symbol::of_name(DESC) => Ok(self.words),
            (Space::Func, s) if s == Symbol::of_name(GETTER) => Ok(self.words + 8),
            (Space::Tls, s) if s == Symbol::of_name(PLACE_0) => Ok(self.words + 16),
            _ => Err(ImageError::Unbound { target }),
        }
    }

    fn value(&mut self, target: Target) -> Result<u64, ImageError> {
        match target.space == Space::Outside && target.symbol == Symbol::of_name(POLL_PAGE) {
            true => Ok(self.page),
            false => Err(ImageError::Unbound { target }),
        }
    }
}

/// `e` declared, sealed, read back and placed, its import words `desc`'s address, the getter's
/// and 0.
fn placed(mut e: Emitted<'static>, tier: TierName, desc: &'static Desc) -> Executable {
    let name = tier.as_str();
    declare(&mut e, &ThreadLocals).unwrap_or_else(|error| panic!("the {name} tier's record does not declare its targets: {error}"));
    let sealed = seal(e).unwrap_or_else(|error| panic!("the {name} tier's record does not seal: {error}"));
    let mut bytes: AVec<'static, u8> = Vec::new_in(Alloc::global());
    encode(&sealed, &mut bytes);
    let view = read(&bytes).unwrap_or_else(|error| panic!("the {name} tier's record does not read back: {error}"));
    let relro_len = view.relro().len();
    let align = 1usize << view.relro_align();
    let symbol = Symbol::of_name("blaze-emit tier_tests::thread_locals: bump");
    let mut done: Result<(), PlaceError> = Ok(());
    let exec = Executable::written(view.code().len(), POLL_PAGE_ROOM + relro_len + align + 8 + 24, |code, data| {
        let (page, rest) = carve_poll_page(data);
        let skip = (align - rest.as_ptr() as usize % align) % align;
        let words_at = skip + relro_len.next_multiple_of(8);
        let values = [desc as *const Desc as u64, getter as usize as u64, 0];
        for (k, value) in values.into_iter().enumerate() {
            rest[words_at + 8 * k..words_at + 8 * k + 8].copy_from_slice(&value.to_le_bytes());
        }
        let words = rest[words_at..].as_ptr() as u64;
        let relro = &mut rest[skip..skip + relro_len];
        let at = At { code: code.as_ptr() as u64, relro: relro.as_ptr() as u64 };
        let mut bound = Bound { page, words };
        done = place(&view, symbol, at, &mut bound, code, relro);
    })
    .expect("the code maps");
    done.unwrap_or_else(|error| panic!("the {name} tier's record does not place: {error}"));
    exec
}

/// Each tier's `bump`, on its own key, run three times on each of two threads.
#[test]
fn both_tiers_give_each_thread_its_own_copy() {
    let func = bump();
    let machine = table().head().machine;
    let tiers: [(TierName, TierFn); 2] = [(TierName::Reference, reference), (TierName::Fast, fast)];
    for (name, emit) in tiers {
        let tier = name.as_str();
        let emitted = emit(func.view(), &ThreadLocals, machine, Alloc::global()).unwrap_or_else(|error| panic!("the {tier} tier refused `bump`: {error}"));
        let desc = Desc::leaked();
        let key = desc.key as usize;
        let exec = placed(emitted, name, desc);
        let entry = exec.code_address() as usize;
        let gate = std::sync::Barrier::new(2);
        let run = || {
            // SAFETY: a placed C function of no parameters returning an integer; its import
            // words hold the descriptor (leaked), the getter and index 0; mapped while `exec`
            // lives, which outlives the scope.
            let f = unsafe { core::mem::transmute::<usize, extern "C" fn() -> u64>(entry) };
            gate.wait();
            let counts: std::vec::Vec<u64> = (0..3).map(|_| f()).collect();
            // SAFETY: the key was created by `Desc::leaked`.
            let table = unsafe { pthread_getspecific(key) } as usize;
            // Both threads alive here, so neither table is freed and reused for the other.
            gate.wait();
            (counts, table, GETTER_CALLS.with(core::cell::Cell::get))
        };
        let (a, b) = std::thread::scope(|s| {
            let one = s.spawn(run);
            let two = s.spawn(run);
            (one.join().unwrap(), two.join().unwrap())
        });
        assert_eq!(a.0, [1, 2, 3], "the {tier} tier, thread one");
        assert_eq!(b.0, [1, 2, 3], "the {tier} tier, thread two, from its own copy");
        assert_ne!(a.1, b.1, "the {tier} tier: two tables");
        assert_eq!((a.2, b.2), (1, 1), "the {tier} tier: the getter once per thread");
        drop(exec);
    }
}
