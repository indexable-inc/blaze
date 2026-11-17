//! The run path's toolchain shelf holds each pack as its mapping, never a heap copy
//! (crates.rs `DirShelf`, `ShelfFile`).
//!
//! `blaze run` read every pack whole with `std::fs::read` (65.7 MB on the heap for a hello
//! world, port27/hello-1116/TABLES-NOTES.md, "Peak attribution", holder 5) beside blaze-cli's
//! own copy of the same files. Now `DirShelf::read` maps the file: the binding below is typed
//! `ShelfFile`, so a shelf that hands back an owned copy (`Vec<u8>`) does not type-check here,
//! and each pack's bytes are checked against its name (`open_pack` checks its BLAKE3 footer).
//!
//! Expected line: `shelf mapped: N packs, B bytes, every one a mapping` (N > 0). Needs the
//! installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_main.rs does.

use blaze_ladder::crates::{DirShelf, ShelfFile, TRIPLE};
use blaze_toolchain::shelf::{open_manifest, open_pack};
use blaze_toolchain::{PINNED, ToolchainView};

#[test]
fn shelf_packs_are_mapped() {
    let home = std::env::var("HOME").expect("HOME is set");
    let place = format!("{home}/.blaze/v2/toolchain");
    let mut shelf = DirShelf::new(place.clone());
    let manifest: ShelfFile = open_manifest(&mut shelf, PINNED).unwrap_or_else(|e| panic!("the manifest at {place}: {e}"));
    let view = ToolchainView::open(manifest.as_ref(), PINNED).expect("open_manifest checked it");
    let rows: Vec<_> = view.units(TRIPLE).expect("the toolchain has the ladder's target").collect();
    let (mut packs, mut bytes) = (0usize, 0usize);
    let mut seen = Vec::new();
    for row in &rows {
        if seen.contains(&row.krate) {
            continue;
        }
        seen.push(row.krate);
        let krate = view.krate(row.krate);
        let pack: ShelfFile = open_pack(&mut shelf, PINNED, &krate).unwrap_or_else(|e| panic!("the pack of `{}`: {e}", krate.name));
        assert_eq!(pack.as_ref().len() as u64, krate.pack_len, "`{}`: the mapping is the whole pack", krate.name);
        packs += 1;
        bytes += pack.as_ref().len();
    }
    assert!(packs > 0, "the toolchain lists no pack for {TRIPLE}");
    println!("shelf mapped: {packs} packs, {bytes} bytes, every one a mapping");
}
