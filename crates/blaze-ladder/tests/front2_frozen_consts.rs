//! A FROZEN HEAD WHOSE TYPE HOLDS A CONSTANT (lane FM): `struct A([u8; N])` is frozen with
//! `N`'s value, evaluated by the freeze's source (front2/tables/head.rs `ConstSource`, the store
//! path's own evaluator behind `StoreConsts`), never refused `NeedConst` as before.
//!
//! Rule 21, by a real edit: `N = 4` then `N = 8` moves `A`'s frozen meaning and the value its
//! head task reads (`const_value_cell`/`const_value_digest`); a comment in `N`'s body that keeps
//! the value moves neither: the read is BY VALUE (the record holds the value, not `N`'s text),
//! so an edit of `N` that keeps its value leaves every head reading it standing. The control: a
//! source with no evaluator refuses the same head by name (`ConstRefused::NoEvaluator`).
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain` (real core, alloc, std).

#![feature(allocator_api)]

use blaze_id::ContentId;
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::tables::head::{CellFailure, ConstRefused, FreezeError, HeadCells, SeededHeads, SeededHeadsOpts, StoreConsts, cell_of, const_value_digest};
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

/// What freezing `A` of `text` gave: its meaning and the value of `N` its head read; or, with
/// no evaluator, the refusal as printed.
fn frozen_a(text: &str, evaluate: bool) -> Result<(ContentId, u128), String> {
    let alloc = Alloc::global();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/consts/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let user = UserCrate { name: "consts", dir: "/consts/", root: "/consts/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let key_of = |ns: Namespace, n: &str| {
        let found = names::path(&names, names.root(0), ns, Written { kind: PathKind::Plain, segments: &[n] }, alloc).unwrap_or_else(|e| panic!("`{n}`: {e}"));
        found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key
    };
    let (a, n) = (key_of(Namespace::Types, "A"), key_of(Namespace::Values, "N"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut evaluator = StoreConsts { names: &names, index: &front.index, prepared };
    let consts: Option<&mut dyn blaze_ladder::front2::tables::head::ConstValues<'_>> = if evaluate { Some(&mut evaluator) } else { None };
    let mut source = SeededHeads::new(SeededHeadsOpts { names: &names, sized: front.index.lang("sized"), pointers: blaze_ladder::front2::receiver::Pointers::NOT_LOOKED_UP, frame: &frame, scratch: alloc, alloc, consts, known: None, owners: None });
    let cell = cell_of(&names, Want::Head(a)).unwrap_or_else(|e| panic!("the cell of `A`: {e}"));
    let got = source.cell(cell).map(|(frozen, reads)| (frozen.meaning(), reads.consts.iter().find(|(k, _)| *k == n).map(|(_, b)| *b)));
    match got {
        Ok((meaning, Some(value))) => Ok((meaning, value)),
        Ok((_, None)) => panic!("`A`'s reads miss `N`'s value"),
        // Every ask of a failed cell names the cell (lane HF1); its own reason is kept by the
        // source ([`SeededHeads::failure`]): here the constant with no evaluator.
        Err(FreezeError::Again { want, failed }) => {
            assert_eq!((want, failed), (cell, cell), "a cell that failed by itself names itself");
            let e = match source.failure(cell) {
                Some((CellFailure::Own(e @ FreezeError::Const { error: ConstRefused::NoEvaluator, .. }), _)) => e.to_string(),
                other => panic!("`A`'s kept failure: {other:?}"),
            };
            // A failed cell is made once: asked again, it is refused by name, with no attempt.
            let attempts = source.attempts;
            match source.cell(cell) {
                Err(FreezeError::Again { want, failed }) => assert_eq!((want, failed), (cell, cell), "the refusal asked again"),
                Err(other) => panic!("`A` asked again: {other}"),
                Ok(_) => panic!("`A` asked again froze"),
            }
            assert_eq!(source.attempts, attempts, "ORACLE: a failed cell was frozen again");
            Err(e.to_string())
        }
        Err(e) => panic!("freezing `A`: {e}"),
    }
}

/// Expected line: `frozen consts: N = 4 and 8 move A; a comment keeps it; no evaluator: refused`.
#[test]
fn a_frozen_head_reads_its_constants_value() {
    const FOUR: &str = "const N: usize = 4;\npub struct A(pub [u8; N]);\nfn main() {}\n";
    const EIGHT: &str = "const N: usize = 8;\npub struct A(pub [u8; N]);\nfn main() {}\n";
    const COMMENTED: &str = "const N: usize = /* four */ 4;\npub struct A(pub [u8; N]);\nfn main() {}\n";
    let (four, v4) = frozen_a(FOUR, true).unwrap_or_else(|e| panic!("with an evaluator: {e}"));
    let (eight, v8) = frozen_a(EIGHT, true).unwrap_or_else(|e| panic!("with an evaluator: {e}"));
    let (commented, vc) = frozen_a(COMMENTED, true).unwrap_or_else(|e| panic!("with an evaluator: {e}"));
    assert_eq!((v4, v8, vc), (4, 8, 4), "the values `A`'s head read");
    assert_ne!(four, eight, "ORACLE (rule 21): `N` = 4 and 8 froze `A` alike");
    assert_ne!(const_value_digest(v4), const_value_digest(v8), "the value reads of 4 and 8 are alike");
    assert_eq!(four, commented, "a comment in `N`'s body that keeps its value moved `A`'s frozen head");
    let refused = frozen_a(FOUR, false).expect_err("CONTROL: with no evaluator `A` froze");
    assert!(refused.contains("no constant evaluator"), "{refused}");
    println!("frozen consts: N = 4 and 8 move A; a comment keeps it; no evaluator: refused, and refused again unfrozen");
}
