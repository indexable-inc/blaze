//! The record round-trips and refuses what is not its own; the shelf names what is wrong.

use std::collections::BTreeMap;

use blaze_fetch::pack::{Builder, Kind, Limits};
use blaze_fetch::{Mode, PackView};

use crate::record::{self, CrateIn, RecordRefused, Role, TargetIn, UnitIn, UnitKind};
use crate::shelf::{self, Shelf, What, Why};
use crate::{Hex, ToolchainId, ToolchainView, parse_hex};

fn pack(files: &[(&str, &str)]) -> std::vec::Vec<u8> {
    let alloc = blaze_own::testing::heap().alloc();
    let limits = Limits { most_files: 16, most_bytes: 1 << 20, most_path: 256 };
    let mut builder = Builder::new(Kind::Tree, [7; 32], limits, 0, alloc).expect("a builder");
    for (path, text) in files {
        builder.file(path, Mode::File, text.as_bytes()).expect("a file");
    }
    builder.finish().expect("a pack").to_vec()
}

/// core, then bstd beneath it, in one target; returns the manifest and the packs.
fn sample() -> (std::vec::Vec<u8>, std::vec::Vec<std::vec::Vec<u8>>) {
    let library = pack(&[("core/src/lib.rs", "pub mod result;"), ("core/src/result.rs", "pub enum Result {}")]);
    let workspace = pack(&[("crates/blaze-std/src/lib.rs", "pub mod cx;")]);
    let digest = |p: &[u8]| shelf::pack_digest(p).expect("a pack's footer");
    let crates = [
        CrateIn {
            name: "core",
            version: "0.0.0",
            edition: "2024",
            lib: "core",
            root: "src/lib.rs",
            dir: "core",
            role: Role::Core,
            pack_kind: 3,
            pack_len: library.len() as u64,
            pack: digest(&library),
        },
        CrateIn {
            name: "blaze-std",
            version: "0.1.0",
            edition: "2024",
            lib: "bstd",
            root: "src/lib.rs",
            dir: "crates/blaze-std",
            role: Role::Bstd,
            pack_kind: 3,
            pack_len: workspace.len() as u64,
            pack: digest(&workspace),
        },
    ];
    let units = [
        UnitIn { krate: 0, kind: UnitKind::Lib, for_host: false, root: "src/lib.rs", features: &[], deps: &[], sysroot: crate::ImplicitSysroot::None },
        UnitIn { krate: 1, kind: UnitKind::Lib, for_host: false, root: "src/lib.rs", features: &["default", "std"], deps: &[("core", 0)], sysroot: crate::ImplicitSysroot::Core },
    ];
    let cfg = [("panic", Some("unwind")), ("target_os", Some("macos")), ("unix", None)];
    let targets = [TargetIn { triple: "aarch64-apple-darwin", units: &units, cfg: &cfg }];
    let bytes = record::write(&crates, &targets, blaze_own::testing::heap().alloc()).expect("a record").to_vec();
    (bytes, vec![library, workspace])
}

#[test]
fn a_record_reads_back_as_written() {
    let (bytes, _) = sample();
    let id = ToolchainView::id_of_bytes(&bytes);
    let view = ToolchainView::open(&bytes, id).expect("its own id");
    let names: std::vec::Vec<_> = view.crates().map(|c| (c.name, c.lib, c.dir, c.role)).collect();
    assert_eq!(names, [("core", "core", "core", Role::Core), ("blaze-std", "bstd", "crates/blaze-std", Role::Bstd)]);
    assert_eq!(view.with_role(Role::Bstd).map(|c| c.index), Some(1));
    let units: std::vec::Vec<_> = view.units("aarch64-apple-darwin").expect("the target").collect();
    assert_eq!(units.len(), 2);
    assert_eq!(units[1].features().collect::<std::vec::Vec<_>>(), ["default", "std"]);
    assert_eq!(units[1].deps().collect::<std::vec::Vec<_>>(), [("core", 0)]);
    assert!(view.units("x86_64-unknown-linux-gnu").is_none());
    let cfg: std::vec::Vec<_> = view.cfg("aarch64-apple-darwin").expect("the target").collect();
    assert_eq!(cfg, [("panic", Some("unwind")), ("target_os", Some("macos")), ("unix", None)]);
    assert!(view.cfg("x86_64-unknown-linux-gnu").is_none());
    // Atoms out of order are refused when written (control: the sorted ones above were taken).
    let unsorted = [("unix", None), ("panic", Some("unwind"))];
    let targets = [TargetIn { triple: "t", units: &[], cfg: &unsorted }];
    assert!(matches!(record::write(&[], &targets, blaze_own::testing::heap().alloc()), Err(RecordRefused::Form { .. })));
    // Two writers of one input write equal bytes (rule 25).
    assert_eq!(sample().0, bytes);
}

#[test]
fn a_record_refuses_another_id_and_any_changed_byte() {
    let (bytes, _) = sample();
    let id = ToolchainView::id_of_bytes(&bytes);
    assert!(matches!(ToolchainView::open(&bytes, ToolchainId([1; 32])), Err(RecordRefused::NotTheId { .. })));
    // Control: the untouched bytes open; then each byte flipped is refused.
    assert!(ToolchainView::open(&bytes, id).is_ok());
    for at in 0..bytes.len() {
        let mut changed = bytes.clone();
        changed[at] ^= 1;
        assert!(ToolchainView::open(&changed, id).is_err(), "byte {at} flipped was taken");
    }
}

#[test]
fn hex_parses_what_it_prints() {
    let bytes: [u8; 32] = core::array::from_fn(|i| (i * 37) as u8);
    let hex = Hex::of(&bytes);
    assert_eq!(parse_hex(hex.as_str().as_bytes()), Some(bytes));
    let line = format!("{hex}\n");
    assert_eq!(parse_hex(line.as_bytes()), Some(bytes));
    assert_eq!(parse_hex(hex.as_str().to_uppercase().as_bytes()), None);
    assert_eq!(parse_hex(&hex.as_str().as_bytes()[..63]), None);
}

/// A shelf in memory.
struct Memory(BTreeMap<String, std::vec::Vec<u8>>);

impl Shelf for Memory {
    type Error = &'static str;
    type Bytes = std::vec::Vec<u8>;
    fn place(&self) -> &str {
        "/home/.blaze/v2/toolchain"
    }
    fn size(&mut self, name: &str) -> Result<Option<u64>, &'static str> {
        assert!(parse_hex(name.as_bytes()).is_some(), "a shelf is asked only for hex names: {name}");
        Ok(self.0.get(name).map(|b| b.len() as u64))
    }
    fn read(&mut self, name: &str, size: u64) -> Result<std::vec::Vec<u8>, &'static str> {
        let bytes = self.0.get(name).ok_or("gone")?;
        Ok(bytes[..bytes.len().min(size as usize)].to_vec())
    }
}

fn filled() -> (Memory, ToolchainId) {
    let (bytes, packs) = sample();
    let id = ToolchainView::id_of_bytes(&bytes);
    let mut files = BTreeMap::new();
    files.insert(id.hex().as_str().to_string(), bytes);
    for p in packs {
        files.insert(Hex::of(&shelf::pack_digest(&p).expect("a pack")).as_str().to_string(), p);
    }
    (Memory(files), id)
}

#[test]
fn a_shelf_opens_what_it_holds_and_names_what_it_lacks() {
    let (mut shelf, id) = filled();
    let manifest = shelf::open_manifest(&mut shelf, id).expect("the manifest");
    let view = ToolchainView::open(&manifest, id).expect("checked");
    let core = view.with_role(Role::Core).expect("core");
    let pack = shelf::open_pack(&mut shelf, id, &core).expect("core's pack");
    let files = PackView::open_verified(&pack).expect("a pack");
    let root = files.find("core/src/lib.rs").expect("core's root");
    assert_eq!(root.bytes, b"pub mod result;");

    // Absent: names the toolchain, the crate, the digest and the folder.
    let bstd = view.with_role(Role::Bstd).expect("bstd");
    shelf.0.remove(Hex::of(&bstd.pack).as_str());
    let error = shelf::open_pack(&mut shelf, id, &bstd).expect_err("gone");
    assert_eq!(error.why, Why::Absent);
    assert_eq!(error.what, What::Pack { name: "blaze-std".into(), version: "0.1.0".into() });
    let said = error.to_string();
    assert!(said.contains(Hex::of(&bstd.pack).as_str()) && said.contains("/home/.blaze/v2/toolchain"), "{said}");

    // Another toolchain is not opened in its place (rule 3).
    let other = ToolchainId([9; 32]);
    assert_eq!(shelf::open_manifest(&mut shelf, other).expect_err("not there").why, Why::Absent);
}

#[test]
fn a_shelf_refuses_a_changed_pack_before_and_after_reading() {
    let (mut shelf, id) = filled();
    let manifest = shelf::open_manifest(&mut shelf, id).expect("the manifest");
    let view = ToolchainView::open(&manifest, id).expect("checked");
    let core = view.with_role(Role::Core).expect("core");
    let name = Hex::of(&core.pack).as_str().to_string();
    // One byte longer: refused by its size, before a byte is read.
    shelf.0.get_mut(&name).expect("there").push(0);
    assert!(matches!(shelf::open_pack(&mut shelf, id, &core).expect_err("longer").why, Why::Size { .. }));
    // Same size, one byte of a file changed: refused by its digest.
    let bytes = shelf.0.get_mut(&name).expect("there");
    bytes.pop();
    bytes[100] ^= 1;
    assert!(matches!(shelf::open_pack(&mut shelf, id, &core).expect_err("changed").why, Why::Pack(_)));
    // A manifest that is another's bytes under this id's name.
    let other = shelf.0.get(&name).expect("there").clone();
    shelf.0.insert(id.hex().as_str().to_string(), other);
    assert!(matches!(shelf::open_manifest(&mut shelf, id).expect_err("not a manifest").why, Why::Record(_)));
}
