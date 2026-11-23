//! No kept `Checked` holds its body's read list (front2/body_reads.rs).
//!
//! A read list exists to be written with the body's record; nothing of the same run reads it
//! after the check but a digest of it. So the check files each clean body's list out of its
//! `Checked`, reduces it to a `ReadsDigest` in the store, and drops it. This test checks N
//! bodies over one store, keeps every `Checked` (as the reach walk does until the program is
//! made) and counts the read-list bytes they still hold: 0, so the live list bytes after N
//! bodies are bounded by the one body being checked, not N bodies'. Before the change each
//! kept `Checked` held its whole list (about 15.6 KB a body on a hello world: 254.5 MB over
//! 16,317 bodies, port27/hello-1116/TABLES-NOTES.md, "Peak attribution", holder 4).
//!
//! The control: the same bodies checked by `check_body_filed` hand their lists back, and each
//! holds bytes (what a kept record held before the change); each list's digest is the one the
//! first pass filed for its body, so the digest is the list's and not some other value.
//!
//! Expected line: `reads freed: 5 bodies kept, 0 list bytes held; the lists filed held N bytes`
//! (N > 0). Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as
//! tests/front2_main.rs does.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::body_reads::{ReadsDigest, check_body_filed};
use blaze_ladder::front2::check::{BuildOrder, Records, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

/// The test crate's `Cargo.toml` edition line.
const EDITION: &str = "blaze-2026.11";

/// Five bodies that each read the library (a slice's `len`, `Clone`, `PartialOrd`, inherent
/// integer methods, a closure): each has a read list of its own.
const BODY: &str = "fn sorts(v: &[u32]) -> usize {\n    let f = |a: &u32| *a;\n    v.len() + f(&v[0]) as usize\n}\nfn copies(x: u32) -> u32 {\n    x.clone()\n}\nfn picks(a: u32, b: u32) -> u32 {\n    if a > b { a } else { b }\n}\nfn adds(a: u32, b: u32) -> u32 {\n    a.wrapping_add(b)\n}\nfn main() {}\n";

const FNS: [&str; 5] = ["sorts", "copies", "picks", "adds", "main"];

#[test]
fn kept_checks_hold_no_read_list() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), BODY.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: BODY }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let key = |n: &str| {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global())
            .unwrap_or_else(|e| panic!("`{n}`: {e}"));
        found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key
    };
    let keys: Vec<_> = FNS.iter().map(|n| key(n)).collect();
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let sized = front.index.lang("sized");

    // N bodies checked over one store, every `Checked` kept.
    let (mut store, mut buckets, mut consts) = (Store::new(sized, front.frame, blaze_own::Alloc::global()), None, Consts::default());
    let mut kept = Vec::new();
    let mut owners = Vec::new();
    for (&k, name) in keys.iter().zip(FNS) {
        let body = lower_fn(&names, prepared, &mut front.keys, &mut store, k).unwrap_or_else(|e| panic!("lowering `{name}`: {e}"));
        let view = body.lowered.view();
        let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `{name}`: {e:?}"));
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
        let checked = check_body(&mut records, &body, &scope).unwrap_or_else(|e| panic!("checking `{name}`: {e}"));
        owners.push(body.owner);
        kept.push(checked);
    }
    let held: usize = kept.iter().map(|c| c.read_list_bytes()).sum();
    assert_eq!(held, 0, "the {} kept checks hold {held} bytes of read lists", kept.len());
    assert_eq!(store.reads.len(), FNS.len(), "each clean body filed one digest");

    // The control: the same bodies over a fresh store, their lists handed back.
    let (mut fresh, mut buckets, mut consts) = (Store::new(sized, front.frame, blaze_own::Alloc::global()), None, Consts::default());
    let mut filed_bytes = 0;
    for ((&k, name), owner) in keys.iter().zip(FNS).zip(&owners) {
        let body = lower_fn(&names, prepared, &mut front.keys, &mut fresh, k).unwrap_or_else(|e| panic!("lowering `{name}` again: {e}"));
        let view = body.lowered.view();
        let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `{name}` again: {e:?}"));
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut fresh, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
        let (checked, filed) = check_body_filed(&mut records, &body, &scope, None, None).unwrap_or_else(|e| panic!("checking `{name}` again: {e}"));
        assert_eq!(checked.read_list_bytes(), 0, "`{name}`: the `Checked` beside its filed list holds none");
        assert!(filed.live_bytes() > 0, "`{name}`: its read list is empty, so the count above proves nothing");
        filed_bytes += filed.live_bytes();
        let digest = ReadsDigest::of(&filed.reads, Alloc::global());
        assert_eq!(store.reads.get(*owner), Some(digest), "`{name}`: the digest filed is not its list's");
    }
    assert!(fresh.reads.is_empty(), "check_body_filed files nothing in the store: its caller holds the list");
    println!("reads freed: {} bodies kept, {held} list bytes held; the lists filed held {filed_bytes} bytes", kept.len());
}
