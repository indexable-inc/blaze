//! W2: member selection, name resolution and the library table (NATIVE.md 3.3, 3.5). Objects are
//! built in the test (`support/w2_obj.rs`), no external tool. Without the piece this file does not
//! compile: "unresolved import `blaze_native::resolve`".
//!
//! Each expectation states the rule it follows:
//! - ld(1) on macOS: "A static library (aka static archive) is a collection of .o files with a
//!   table of contents that lists the global symbols in the .o files. ld will only pull .o files
//!   out of a static library if needed to resolve some symbol reference. Unlike traditional
//!   linkers, ld will continually search a static library while linking."
//! - The Rust Reference (items/external-blocks.html): "+whole-archive means that the static
//!   library is linked as a whole archive without throwing any object files away."
//! - <mach-o/nlist.h>: "The N_WEAK_REF bit of the n_desc field indicates to the dynamic linker that
//!   the undefined symbol is allowed to be missing and is to have the address of zero when missing."

#[path = "support/w2_obj.rs"]
mod w2_obj;

use blaze_hash::FixedState;
use blaze_native::Platform;
use blaze_native::libraries::{Library, LibraryError, LibraryKind, Source, source};
use blaze_native::resolve::{Input, InputKind, Origin, Plan, Resolution, ResolveError, Wanted, plan};
use blaze_own::Alloc;
use w2_obj::{Sym, archive, macho};

const MAC: Platform = Platform::MacosArm64;

fn origin(package: &'static str) -> Origin<'static> {
    Origin { package, file: "lib.a" }
}

fn run<'a>(inputs: &[Input<'a>], roots: &'a [&'a [u8]], rust: &'a [&'a [u8]]) -> Result<Plan<'a>, ResolveError<'a>> {
    plan(MAC, inputs, Wanted { roots, rust, libraries: &[] }, FixedState, Alloc::global())
}

fn pulled(plan: &Plan<'_>) -> Vec<(u32, u32)> {
    plan.objects.iter().map(|p| (p.input, p.member)).collect()
}

fn resolution_of(plan: &Plan<'_>, name: &[u8]) -> Option<Resolution> {
    plan.names.iter().find(|(n, _)| *n == name).map(|(_, r)| *r)
}

/// ld(1): only members "needed to resolve some symbol reference" are pulled, and the search goes
/// on through what a pulled member references. Root `_a` pulls m0 (defines `_a`, references `_b`),
/// which pulls m1 (defines `_b`); m2 (defines `_c`) is never needed. Prints `pulled [(0, 0), (0, 1)]`.
#[test]
fn pulls_only_what_roots_need() {
    let m0 = macho(&[Sym::Def("_a"), Sym::Undef("_b")]);
    let m1 = macho(&[Sym::Def("_b")]);
    let m2 = macho(&[Sym::Def("_c")]);
    let lib = archive(&[("m0.o", &m0), ("m1.o", &m1), ("m2.o", &m2)]);
    let inputs = [Input { from: origin("x@1.0.0"), bytes: &lib, kind: InputKind::Archive { whole: false } }];
    let roots: [&[u8]; 1] = [b"_a"];
    let plan = run(&inputs, &roots, &[]).expect("resolves");
    println!("pulled {:?}", pulled(&plan));
    assert_eq!(pulled(&plan), vec![(0, 0), (0, 1)]);
    assert_eq!(resolution_of(&plan, b"_b"), Some(Resolution::Defined { object: 1, symbol: 0 }));
    assert_eq!(resolution_of(&plan, b"_c"), None);
}

/// The c-kzg -> blst shape: a member of the first archive references a name only the second
/// archive defines; ld64 "will continually search", so the second archive's member is pulled,
/// and its unreferenced sibling is not. Prints `pulled [(0, 0), (1, 0)]`.
#[test]
fn cross_archive_pull() {
    let kzg = macho(&[Sym::Def("_blob_to_kzg_commitment"), Sym::Undef("_blst_p1_add")]);
    let add = macho(&[Sym::Def("_blst_p1_add")]);
    let unused = macho(&[Sym::Def("_blst_unused")]);
    let ckzg = archive(&[("ckzg.o", &kzg)]);
    let blst = archive(&[("add.o", &add), ("unused.o", &unused)]);
    let inputs = [
        Input { from: origin("c-kzg@2.1.0"), bytes: &ckzg, kind: InputKind::Archive { whole: false } },
        Input { from: origin("blst@0.3.15"), bytes: &blst, kind: InputKind::Archive { whole: false } },
    ];
    let roots: [&[u8]; 1] = [b"_blob_to_kzg_commitment"];
    let plan = run(&inputs, &roots, &[]).expect("resolves");
    println!("pulled {:?}", pulled(&plan));
    assert_eq!(pulled(&plan), vec![(0, 0), (1, 0)]);
    assert!(plan.imports.is_empty());
}

/// Two strong definitions of one name in pulled members are an error (lld and ld64: "duplicate
/// symbol"). `_f` is pulled from the first archive; `_g` pulls the second archive's member, which
/// also defines `_f` strongly. Prints `Duplicate { name: [95, 102], .. }`.
#[test]
fn duplicate_strong_refused() {
    let first = macho(&[Sym::Def("_f")]);
    let second = macho(&[Sym::Def("_f"), Sym::Def("_g")]);
    let one = archive(&[("one.o", &first)]);
    let two = archive(&[("two.o", &second)]);
    let inputs = [
        Input { from: origin("one@1.0.0"), bytes: &one, kind: InputKind::Archive { whole: false } },
        Input { from: origin("two@1.0.0"), bytes: &two, kind: InputKind::Archive { whole: false } },
    ];
    let roots: [&[u8]; 2] = [b"_f", b"_g"];
    let err = run(&inputs, &roots, &[]).err().expect("refused");
    println!("{err:?}");
    assert_eq!(err, ResolveError::Duplicate { name: b"_f", first: origin("one@1.0.0"), second: origin("two@1.0.0") });
}

/// The secp256k1 shape (NATIVE.md 3.4): C calls back into a Rust export. The reference resolves to
/// a word, not an import, and no member is searched for it. Prints `words [[95, 99, 98]]`.
#[test]
fn rust_word_for_c_callback() {
    let ctx = macho(&[Sym::Def("_secp256k1_context_create"), Sym::Undef("_cb")]);
    let lib = archive(&[("ctx.o", &ctx)]);
    let inputs = [Input { from: origin("secp256k1-sys@0.10.1"), bytes: &lib, kind: InputKind::Archive { whole: false } }];
    let roots: [&[u8]; 1] = [b"_secp256k1_context_create"];
    let rust: [&[u8]; 1] = [b"_cb"];
    let plan = run(&inputs, &roots, &rust).expect("resolves");
    println!("words {:?}", plan.words.iter().collect::<Vec<_>>());
    assert_eq!(&plan.words[..], &[b"_cb" as &[u8]][..]);
    assert_eq!(resolution_of(&plan, b"_cb"), Some(Resolution::Rust { word: 0 }));
    assert!(plan.imports.is_empty());
}

/// aws-lc's bcm.o shape: a member references `___dso_handle`, which no member defines. The
/// linker defines it (synthetic.rs), so it is the image's own name, not an import a library
/// must supply. Twin: a member that does define it wins, as a definition does in lld (a
/// synthetic symbol is added only where the name is still undefined).
#[test]
fn dso_handle_is_the_linkers_not_an_import() {
    let bcm = macho(&[Sym::Def("_aws_lc_init"), Sym::Undef("___dso_handle")]);
    let lib = archive(&[("bcm.o", &bcm)]);
    let inputs = [Input { from: origin("aws-lc-sys@0.47.0"), bytes: &lib, kind: InputKind::Archive { whole: false } }];
    let roots: [&[u8]; 1] = [b"_aws_lc_init"];
    let plan = run(&inputs, &roots, &[]).expect("resolves");
    assert_eq!(resolution_of(&plan, b"___dso_handle"), Some(Resolution::Linker));
    assert!(plan.imports.is_empty(), "imports {:?}", plan.imports);

    let own = macho(&[Sym::Def("_aws_lc_init"), Sym::Def("___dso_handle")]);
    let lib = archive(&[("own.o", &own)]);
    let inputs = [Input { from: origin("aws-lc-sys@0.47.0"), bytes: &lib, kind: InputKind::Archive { whole: false } }];
    let plan = run(&inputs, &roots, &[]).expect("resolves");
    assert!(matches!(resolution_of(&plan, b"___dso_handle"), Some(Resolution::Defined { object: 0, .. })));
}

/// <mach-o/nlist.h>: a `N_WEAK_REF` undefined symbol "is allowed to be missing": when nothing
/// defines it, it is an import of the image, weak (every reference to it is weak); a strong
/// reference nothing defines is a strong import. Prints `imports [[95, 119], [95, 122]]`.
#[test]
fn weak_undefined_is_weak_import() {
    let m = macho(&[Sym::Def("_x"), Sym::WeakUndef("_w"), Sym::Undef("_z")]);
    let lib = archive(&[("m.o", &m)]);
    let inputs = [Input { from: origin("x@1.0.0"), bytes: &lib, kind: InputKind::Archive { whole: false } }];
    let roots: [&[u8]; 1] = [b"_x"];
    let plan = run(&inputs, &roots, &[]).expect("resolves");
    println!("imports {:?}", plan.imports.iter().collect::<Vec<_>>());
    assert_eq!(&plan.imports[..], &[b"_w" as &[u8], b"_z"][..]);
    assert_eq!(resolution_of(&plan, b"_w"), Some(Resolution::Import { index: 0, weak: true }));
    assert_eq!(resolution_of(&plan, b"_z"), Some(Resolution::Import { index: 1, weak: false }));
}

/// The Rust Reference: "+whole-archive means that the static library is linked as a whole archive
/// without throwing any object files away": with no root at all, every member is pulled, in member
/// order. Prints `pulled [(0, 0), (0, 1), (0, 2)]`.
#[test]
fn whole_archive_pulls_all() {
    let m0 = macho(&[Sym::Def("_a")]);
    let m1 = macho(&[Sym::Def("_b")]);
    let m2 = macho(&[Sym::Common("_c", 16, 3)]);
    let lib = archive(&[("m0.o", &m0), ("m1.o", &m1), ("m2.o", &m2)]);
    let inputs = [Input { from: origin("x@1.0.0"), bytes: &lib, kind: InputKind::Archive { whole: true } }];
    let plan = run(&inputs, &[], &[]).expect("resolves");
    println!("pulled {:?}", pulled(&plan));
    assert_eq!(pulled(&plan), vec![(0, 0), (0, 1), (0, 2)]);
    assert_eq!(resolution_of(&plan, b"_c"), Some(Resolution::Common { object: 2, symbol: 0, size: 16, align_log2: 3 }));
}

/// NATIVE.md 3.5, and ld64's search of the SDK for `-l<N>` (`usr/lib/lib<N>.tbd`) and
/// `-framework <N>` (`System/Library/Frameworks/<N>.framework/<N>.tbd`): on macOS a library is
/// named by its stub, and its install name is the stub's, READ (Foundation's `Versions/C`, never
/// a `Versions/A` built from the name); a name that cannot be one file's is refused. On Linux,
/// glibc's table: `c` is `libc.so.6`, `stdc++` waits on the owner, a framework is refused.
/// TWIN (the proof is the platform's): a Linux plan refuses a library resolved for macOS.
///
/// Prints `install names ok`.
#[test]
fn install_names() {
    const LINUX: Platform = Platform::LinuxArm64;
    let foundation = Library { kind: LibraryKind::Framework, name: "Foundation" };
    let Ok(Source::Stub(at)) = source(MAC, foundation) else { panic!("a framework is named by its stub") };
    assert_eq!(format!("{at}"), "System/Library/Frameworks/Foundation.framework/Foundation.tbd");
    let stub: &[u8] = b"--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64e-macos ]\n\
        install-name: '/System/Library/Frameworks/Foundation.framework/Versions/C/Foundation'\n...\n";
    let linked = at.linked(stub).expect("the stub reads");
    assert_eq!(linked.install(), "/System/Library/Frameworks/Foundation.framework/Versions/C/Foundation");
    assert_eq!((linked.library(), linked.platform()), (foundation, MAC));
    match source(MAC, Library { kind: LibraryKind::Dylib, name: "c++" }) {
        Ok(Source::Stub(at)) => assert_eq!(format!("{at}"), "usr/lib/libc++.tbd"),
        other => panic!("`c++` is named by its stub: {other:?}"),
    }
    let bad = Library { kind: LibraryKind::Framework, name: "../x" };
    assert_eq!(source(MAC, bad), Err(LibraryError::Name { kind: LibraryKind::Framework, name: "../x" }));

    match source(LINUX, Library { kind: LibraryKind::Dylib, name: "c" }) {
        Ok(Source::Linked(c)) => assert_eq!(c.install(), "libc.so.6"),
        other => panic!("`c` is glibc's: {other:?}"),
    }
    assert!(matches!(source(LINUX, Library { kind: LibraryKind::Dylib, name: "stdc++" }), Err(LibraryError::Undecided { .. })));
    assert_eq!(source(LINUX, foundation), Err(LibraryError::FrameworkOffMac { name: "Foundation" }));

    let libraries = [linked];
    let refused = plan(LINUX, &[], Wanted { roots: &[], rust: &[], libraries: &libraries }, FixedState, blaze_own::testing::heap().alloc()).err();
    assert_eq!(refused, Some(ResolveError::Library { name: "Foundation", resolved_for: MAC }));
    println!("install names ok");
}
