//! End-to-end `#[blaze::capability]` sans-IO firewall test through `blaze-ladder` front2,
//! `HeadRecord` (version 43) encode/decode round-trip, and `blaze-typeck` (`rules/capability.rs`,
//! `blaze::capability_in_sans_io`).
//!
//! Self-contained `#![no_core]` fixture (runs without external toolchain).

use blaze_ladder::crates::{Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::head_record::{HeadAt, HeadRef, decode, encode, from_store};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const TEXT: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![allow(internal_features)]

#[lang = "pointee_sized"]
trait PointeeSized {}

#[lang = "meta_sized"]
trait MetaSized: PointeeSized {}

#[lang = "sized"]
trait Sized: MetaSized {}

#[blaze::capability]
pub struct Alloc {
    pub raw: u32,
}

#[blaze::capability]
pub struct Fs {
    pub fd: u32,
}

pub struct DriverWrap {
    pub fs: Fs,
}

// Allowed in non-sans-IO driver scope.
pub struct DriverState {
    pub alloc: Alloc,
    pub wrap: DriverWrap,
}

pub mod core_mod {
    #![blaze::sans_io]

    use super::{Alloc, DriverWrap, Fs};

    // Pure data in sans_io: allowed.
    pub struct PureState {
        pub count: u32,
        pub cb: fn(&Alloc) -> u32,
    }

    // Capability definition itself inside sans_io (e.g. BrandedAlloc): allowed.
    #[blaze::capability]
    pub struct BrandedAlloc {
        pub inner: Alloc,
    }

    // Direct capability field in sans_io: ERROR (`blaze::capability_in_sans_io`).
    pub struct DirectBad {
        pub count: u32,
        pub alloc: Alloc,
    }

    // Transitive capability field in sans_io: ERROR (`blaze::capability_in_sans_io`).
    pub struct TransitiveBad {
        pub wrap: DriverWrap,
    }

    // Borrowed capability field in sans_io: ERROR (`blaze::capability_in_sans_io`).
    pub struct BorrowedBad<'a> {
        pub fs: &'a Fs,
    }

    pub fn make_direct_bad(a: Alloc) -> DirectBad {
        DirectBad { count: 1u32, alloc: a }
    }

    pub fn make_transitive_bad(w: DriverWrap) -> TransitiveBad {
        TransitiveBad { wrap: w }
    }

    pub fn make_borrowed_bad<'a>(fs: &'a Fs) -> BorrowedBad<'a> {
        BorrowedBad { fs }
    }

    pub fn make_pure(cb: fn(&Alloc) -> u32) -> PureState {
        PureState { count: 1u32, cb }
    }

    pub fn make_branded(a: Alloc) -> BrandedAlloc {
        BrandedAlloc { inner: a }
    }
}

pub fn make_driver(alloc: Alloc, wrap: DriverWrap) -> DriverState {
    DriverState { alloc, wrap }
}
"#;

#[test]
fn capability_firewall_end_to_end_and_head_record_roundtrip() {
    let alloc: Alloc<'static> = blaze_own::testing::heap().alloc();
    let files = [(String::from("/cap-test/src/lib.rs"), String::from(TEXT))];
    let edition = edition::PackageEdition::of_manifest("2024").expect("edition 2024");
    let user = UserCrate {
        name: "cap_test",
        dir: "/cap-test/",
        root: "/cap-test/src/lib.rs",
        files: &files,
        edition,
        panic: blaze_build::Panic::Abort,
    };
    let tc = Toolchain::none(alloc);
    let inputs = Inputs::new(&user, &tc).expect("inputs");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("resolve");
    let sources = [SourceFile { path: "src/lib.rs", text: TEXT }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let mut front = Front2::new(&names, &frame, &sources, alloc).expect("front2");
    let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);

    let resolve_in = |ns: Namespace, segs: &[&str]| {
        let found = names::path(
            &names,
            names.root(0),
            ns,
            Written { kind: PathKind::Plain, segments: segs },
            alloc,
        )
        .unwrap_or_else(|e| panic!("resolve {segs:?}: {e}"));
        found.whole(segs.len()).unwrap_or_else(|| panic!("{segs:?} not named")).key
    };

    let alloc_lib = resolve_in(Namespace::Types, &["Alloc"]);
    let fs_lib = resolve_in(Namespace::Types, &["Fs"]);
    let driver_wrap_lib = resolve_in(Namespace::Types, &["DriverWrap"]);
    let driver_state_lib = resolve_in(Namespace::Types, &["DriverState"]);
    let pure_state_lib = resolve_in(Namespace::Types, &["core_mod", "PureState"]);
    let branded_alloc_lib = resolve_in(Namespace::Types, &["core_mod", "BrandedAlloc"]);
    let direct_bad_lib = resolve_in(Namespace::Types, &["core_mod", "DirectBad"]);
    let transitive_bad_lib = resolve_in(Namespace::Types, &["core_mod", "TransitiveBad"]);
    let borrowed_bad_lib = resolve_in(Namespace::Types, &["core_mod", "BorrowedBad"]);

    // 1. Verify `AdtRow` flags and `HeadRecord` v43 encode/decode round-trip.
    let expected_flags = [
        ("Alloc", alloc_lib, true, false),
        ("Fs", fs_lib, true, false),
        ("DriverWrap", driver_wrap_lib, false, false),
        ("DriverState", driver_state_lib, false, false),
        ("PureState", pure_state_lib, false, true),
        ("BrandedAlloc", branded_alloc_lib, true, true),
        ("DirectBad", direct_bad_lib, false, true),
        ("TransitiveBad", transitive_bad_lib, false, true),
        ("BorrowedBad", borrowed_bad_lib, false, true),
    ];
    for (label, lib, want_cap, want_sans_io) in expected_flags {
        store.ensure(&names, &mut front.keys, Want::Adt(lib)).unwrap_or_else(|e| panic!("{label}: {e:?}"));
        let head = from_store(&store, HeadAt::Adt(lib)).unwrap_or_else(|e| panic!("{label}: {e}"));
        let HeadRef::Adt { row, .. } = head else { panic!("{label} is not an Adt head") };
        assert_eq!(row.capability, want_cap, "{label}: capability flag mismatch");
        assert_eq!(row.sans_io, want_sans_io, "{label}: sans_io flag mismatch");

        let bytes = encode(head, store.tys.view(), alloc).unwrap_or_else(|e| panic!("{label}: encode: {e}"));
        let decoded = decode(&bytes, head.text(), &store.tys, alloc).unwrap_or_else(|e| panic!("{label}: decode: {e}"));
        let HeadRef::Adt { row: dec_row, .. } = decoded.as_ref() else { panic!("{label}: decoded not Adt") };
        assert_eq!(dec_row.capability, want_cap, "{label}: round-tripped capability mismatch");
        assert_eq!(dec_row.sans_io, want_sans_io, "{label}: round-tripped sans_io mismatch");
    }

    // 2. End-to-end body checks through `ProjectWorld` verifying `blaze::capability_in_sans_io`.
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("ProjectWorld");

    macro_rules! check_fn {
        ($segs:expr) => {{
            let segs: &[&str] = $segs;
            let lib = resolve_in(Namespace::Values, segs);
            let v = world.check(BodyRef::Free(lib), alloc);
            assert!(v.refused.is_none(), "{segs:?} refused: {:?}", v.refused);
            assert!(v.diagnostics.is_empty(), "{segs:?} diags: {:?}", v.diagnostics);
            v
        }};
    }

    let v_direct = check_fn!(&["core_mod", "make_direct_bad"]);
    assert_eq!(v_direct.findings.len(), 1, "DirectBad findings: {:?}", v_direct.findings);
    assert_eq!(v_direct.findings[0].lint, "capability-in-sans-io");
    assert!(
        v_direct.findings[0].text.contains("DirectBad") && v_direct.findings[0].text.contains("Alloc"),
        "direct message: {}",
        v_direct.findings[0].text
    );

    let v_trans = check_fn!(&["core_mod", "make_transitive_bad"]);
    assert_eq!(v_trans.findings.len(), 1, "TransitiveBad findings: {:?}", v_trans.findings);
    assert_eq!(v_trans.findings[0].lint, "capability-in-sans-io");
    assert!(
        v_trans.findings[0].text.contains("TransitiveBad")
            && v_trans.findings[0].text.contains("Fs")
            && v_trans.findings[0].text.contains("DriverWrap"),
        "transitive message: {}",
        v_trans.findings[0].text
    );

    let v_borrowed = check_fn!(&["core_mod", "make_borrowed_bad"]);
    assert_eq!(v_borrowed.findings.len(), 1, "BorrowedBad findings: {:?}", v_borrowed.findings);
    assert_eq!(v_borrowed.findings[0].lint, "capability-in-sans-io");
    assert!(
        v_borrowed.findings[0].text.contains("borrows") && v_borrowed.findings[0].text.contains("Fs"),
        "borrowed message: {}",
        v_borrowed.findings[0].text
    );

    // Clean cases: pure sans_io struct (with fn(&Alloc)), capability definition itself, and driver struct.
    for clean in [
        &["core_mod", "make_pure"][..],
        &["core_mod", "make_branded"][..],
        &["make_driver"][..],
    ] {
        let v = check_fn!(clean);
        assert!(v.findings.is_empty(), "{clean:?} must have zero findings, got: {:?}", v.findings);
    }
}
