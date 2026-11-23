//! D189 S3: the supplier of each body's `BodyScope` record under `BlockScopes::Bodies`
//! (front2 body_scopes.rs `Supplied`). Over one crate's held bodies:
//! - a body that is no surface body is made on its first ask, equal to `body_scope`'s own
//!   record, and the second ask is the same record (memoised per request);
//! - a surface body is read from the store: its stored record is what is answered;
//! - a surface body the store holds no record for is refused (`NotStored`), never made;
//! - a key no crate solve held is refused (`NotHeld`).
//!
//! Control (rule 23): the store hands body A the record of body B; the supplier answers B's
//! blocks for A, which proves the stored record is what is read (and not made again).

use core::cell::{Cell, RefCell};

use blaze_id::ItemKey;
use blaze_mem::{Frame, Tag};
use blaze_own::Erased;
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::body_scopes::{NoSurface, Stored, StoredBodyScopes, Supplied};
use blaze_ladder::front2::locals::{BodyScopes, NoBodyScope};
use blaze_own::AVec;
use blaze_scope::body_scope::body_scope;
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::prepares::Prepares;
use blaze_scope::resolve::{BodyDeps, CrateInput, HeldBody, Names, aligned, resolve_crates};
use blaze_scope::surface::BlockScopes;
use blaze_scope::task::{Par, Scratch, Serial};

const USER: &str = "pub trait Show { fn show(&self) -> u8; }
pub fn one() -> u8 { struct A; impl Show for A { fn show(&self) -> u8 { 1 } } A.show() }
pub fn two() -> u8 { struct B; impl Show for B { fn show(&self) -> u8 { 2 } } let inner = { struct C; impl Show for C { fn show(&self) -> u8 { 3 } } C.show() }; inner + B.show() }
pub fn three() -> u8 { struct D; impl Show for D { fn show(&self) -> u8 { 4 } } D.show() }
pub fn main() {}
";

struct Bodies;

impl Par for Bodies {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        Serial.map(n, f, out);
    }

    fn block_scopes(&self) -> BlockScopes {
        BlockScopes::Bodies
    }
}

/// The store of the test: `surface` bodies, each with the record `records` gives it (`None`:
/// held by no record).
struct Store {
    surface: Vec<(ItemKey, Option<&'static [u8]>)>,
}

impl StoredBodyScopes<'static> for Store {
    fn stored(&self, body: ItemKey) -> Stored<'static> {
        match self.surface.iter().find(|(k, _)| *k == body) {
            None => Stored::NotSurface,
            Some((_, Some(bytes))) => Stored::Record(bytes),
            Some((_, None)) => Stored::Missing,
        }
    }
}

#[test]
fn records_are_read_from_the_store_or_made_once_and_a_missing_one_is_refused() {
    let alloc = blaze_own::testing::heap().alloc();
    let bump = blaze_own::testing::bump();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc: &'static Toolchain = Box::leak(Box::new(Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"))));
    let edition = edition::PackageEdition::of_manifest("2021").expect("a known edition");
    let files: &'static [(String, String)] = Box::leak(vec![("/user/src/main.rs".to_string(), USER.to_string())].into_boxed_slice());
    let user: &'static UserCrate<'static> = Box::leak(Box::new(UserCrate { name: "user", dir: "/user/", root: "/user/src/main.rs", files, edition, panic: blaze_build::Panic::Unwind }));
    let inputs: &'static Inputs<'static> = Box::leak(Box::new(Inputs::new(user, tc).unwrap_or_else(|e| panic!("the crates: {e}"))));
    let crates: &'static [CrateInput<'static>] = Vec::leak(inputs.crates());
    let at = |name: &str| crates.iter().position(|c| c.name == name).map(|i| CrateId(u32::try_from(i).expect("few crates")));
    let sysroot = Sysroot { core: at("core"), alloc: at("alloc"), std: at("std"), bstd: at("bstd") };
    let texts = &inputs.texts;
    let names: &'static Names<'static> = Box::leak(Box::new(resolve_crates(crates, sysroot, texts, &Bodies, bump).unwrap_or_else(|e| panic!("resolve_crates under Bodies: {e}"))));
    let c = names.crates()[0];
    let prepares = Prepares::new_in(bump);
    let deps = BodyDeps::of_names(names, 0, alloc);

    // Each held body's own record, by `body_scope`, as the truth.
    let by_text = |word: &str| c.bodies.iter().find(|h| h.src.text.contains(word)).copied().unwrap_or_else(|| panic!("a held body writing `{word}`"));
    let (a, b, d) = (by_text("struct A;"), by_text("struct B;"), by_text("struct D;"));
    let own = |h: HeldBody<'static>| body_scope(c, &deps, &crates[0], 0, h, texts, prepares, bump).unwrap_or_else(|e| panic!("body_scope: {e}"));
    let modules_of = |v: &blaze_scope::body_scope::BodyScopeView<'_>| v.modules().collect::<Vec<_>>();
    let truth = |h: HeldBody<'static>| {
        let scope = own(h);
        let mut m: Vec<ItemKey> = scope.blocks.iter().map(|r| r.module).collect();
        m.sort_unstable();
        m
    };
    let record_of = |h: HeldBody<'static>| -> &'static [u8] { aligned(&own(h).write(alloc).unwrap_or_else(|e| panic!("write: {e:?}")), bump) };

    // No surface body: every record made here, once.
    let none: &'static NoSurface = &NoSurface;
    let made = Supplied::new(names, crates, texts, prepares, none, &Scratch::kept, bump);
    let first = made.of(a.src.key).unwrap_or_else(|e| panic!("body `one` made: {e:?}"));
    let again = made.of(a.src.key).unwrap_or_else(|e| panic!("body `one` again: {e:?}"));
    println!("supplier: made: `one` blocks {:?}; asked {}; same record twice {}", modules_of(first).len(), made.asked(), core::ptr::eq(first, again));
    assert_eq!(modules_of(first), truth(a), "a body that is no surface body is made by body_scope");
    assert!(core::ptr::eq(first, again), "the second ask is the memoised record");
    assert_eq!(made.of(b.src.key).map(|v| modules_of(v)).ok(), Some(truth(b)), "each body its own record");
    let stray = ItemKey(blaze_id::ContentId::of("front2 body scope supplier test", b"no body"));
    assert_eq!(made.of(stray).err(), Some(NoBodyScope::NotHeld), "a key no solve held is refused");
    assert_eq!(made.refusals(), 0, "no body of the fixture is refused by body_scope");

    // Surface bodies: `one` stored, `three` stored by no record.
    let store: &'static Store = Box::leak(Box::new(Store { surface: vec![(a.src.key, Some(record_of(a))), (d.src.key, None)] }));
    let read = Supplied::new(names, crates, texts, prepares, store, &Scratch::kept, bump);
    let stored = read.of(a.src.key).map(|v| modules_of(v));
    let missing = read.of(d.src.key).err();
    println!("supplier: stored: `one` {:?}; `three` (no record) {missing:?}", stored.as_ref().map(Vec::len));
    assert_eq!(stored.ok(), Some(truth(a)), "a surface body's stored record is answered");
    assert_eq!(missing, Some(NoBodyScope::NotStored), "a surface body with no record is refused, never made");
    assert_eq!(read.of(b.src.key).map(|v| modules_of(v)).ok(), Some(truth(b)), "a body that is no surface body is still made");

    // Control: the store hands `one` the record of `two`.
    let swapped: &'static Store = Box::leak(Box::new(Store { surface: vec![(a.src.key, Some(record_of(b)))] }));
    let wrong = Supplied::new(names, crates, texts, prepares, swapped, &Scratch::kept, bump);
    let answered = wrong.of(a.src.key).map(|v| modules_of(v)).unwrap_or_else(|e| panic!("the swapped record reads: {e:?}"));
    println!("supplier: control: `one` answered by `two`'s record: {} block(s), its own {}", answered.len(), truth(a).len());
    assert_eq!(answered, truth(b), "control: the stored record is what is read");
    assert_ne!(answered, truth(a), "control: `one` and `two` have different blocks, or this test proves nothing");

    // The driver's scratch: each body made here is solved in it, reset after the body, and its
    // record is the one made without a driver (the scratch moves memory, never an answer). The
    // control above (`Scratch::kept`) used no scratch; here every made body used it once, and a
    // body asked again or read from the store uses it no more.
    let scratch: &'static RefCell<Frame> = Box::leak(Box::new(RefCell::new(Frame::with_reserve(1 << 28, Tag::Expand).expect("a scratch frame"))));
    let uses: &'static Cell<usize> = Box::leak(Box::new(Cell::new(0usize)));
    let hook = move |work: &mut dyn FnMut(blaze_own::Alloc<'_>)| {
        let mut frame = scratch.try_borrow_mut().unwrap_or_else(|_| panic!("the scratch was asked inside its own use"));
        uses.set(uses.get() + 1);
        frame.scope(|f| {
            let erased = Erased::new(f);
            work(erased.handle());
        });
        Scratch::Reset
    };
    let hook: &'static dyn Fn(&mut dyn FnMut(blaze_own::Alloc<'_>)) -> Scratch = Box::leak(Box::new(hook));
    let driven = Supplied::new(names, crates, texts, prepares, store, hook, bump);
    let one = driven.of(a.src.key).map(|v| modules_of(v));
    assert_eq!(uses.get(), 0, "stored `one` is read, never solved in scratch");
    let two = driven.of(b.src.key).map(|v| modules_of(v));
    let after_two = uses.get();
    let _ = driven.of(b.src.key);
    println!("supplier: driven: scratch used {} time(s), its peak {} bytes", uses.get(), scratch.borrow().peak());
    assert_eq!(one.ok(), Some(truth(a)), "a stored body is read, not solved");
    assert_eq!(two.ok(), Some(truth(b)), "a made body's record is the same under the scratch");
    // `body_scope_scratch` uses the scratch twice per solved body: once for
    // `solve_held_body_scratch` and once for `reads_digest`.
    assert_eq!(after_two, 2, "only `two` was solved, once (solve + reads_digest)");
    assert_eq!(uses.get(), after_two, "the second ask of `two` is memoised and uses no scratch");
    assert!(scratch.borrow().peak() > 0, "the solve's work ran in the scratch");
}
