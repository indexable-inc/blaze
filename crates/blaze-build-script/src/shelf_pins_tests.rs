//! shelf-pins: the shelf's programs are pinned one by one (`shelf_programs` in clang.json),
//! checked when placed, and named by the host-tools table.
//!
//! Control (rule 8): in `shelf_pins::check_member` replace `got != row.sha256` by `false` (the
//! check skipped): `a_member_whose_bytes_differ_is_refused` fails at "nothing placed", because
//! the member is placed (`put` is called with it). Put it back.

use crate::host_tools::{HostToolError, Place, host_tools};
use crate::shelf_pins::{PinnedMember, PlaceError, ShelfPinError, ShelfPlacer, place_member, shelf_program, shelf_programs};
use blaze_pins::CLANG_JSON as PIN;

const MAC: &str = "aarch64-apple-darwin";
const NM_MEMBER: &str = "LLVM-23.1.4-macOS-ARM64/bin/llvm-nm";

/// A placer over an extraction holding one member whose digest is `digest`; it records what it
/// put.
struct Fake {
    digest: [u8; 32],
    put: std::vec::Vec<(std::string::String, std::string::String)>,
}

impl ShelfPlacer for Fake {
    type Error = core::convert::Infallible;
    fn digest(&mut self, _member: &str) -> Result<[u8; 32], Self::Error> {
        Ok(self.digest)
    }
    fn put(&mut self, pinned: &PinnedMember<'_>) -> Result<(), Self::Error> {
        self.put.push((pinned.row().member.into(), pinned.row().shelf_path.into()));
        Ok(())
    }
}

/// The checked-in table pins the five programs placed on the macOS shelf, each by its archive
/// member, and llvm-nm by the digest the archive's member has (nm-row's census). Prints "shelf
/// pins: clang-23 libclang.dylib lld llvm-ar llvm-nm".
#[test]
fn the_table_pins_the_five_programs() {
    let alloc = blaze_own::testing::heap().alloc();
    let rows = shelf_programs(PIN, MAC, alloc).expect("the macOS rows");
    let names: std::vec::Vec<&str> = rows.iter().map(|r| r.name).collect();
    assert_eq!(names, ["clang-23", "libclang.dylib", "lld", "llvm-ar", "llvm-nm"]);
    let places: std::vec::Vec<&str> = rows.iter().map(|r| r.shelf_path).collect();
    assert_eq!(places, ["bin/clang-23", "lib/libclang.dylib", "bin/lld", "bin/llvm-ar", "bin/llvm-nm"]);
    let nm = shelf_program(PIN, MAC, "llvm-nm").expect("llvm-nm's row");
    assert_eq!(nm.member, NM_MEMBER);
    assert_eq!(nm.sha256[..4], [0xcf, 0x4c, 0x3e, 0x2d]);
    assert_eq!(nm.sha256[28..], [0x89, 0x0b, 0x0d, 0x0b]);
    println!("shelf pins: {}", names.join(" "));
}

/// The placing step refuses an extracted member whose bytes differ from its row, naming the
/// member, and puts nothing; the same member with its row's digest is placed at its shelf path.
/// Prints "differs: LLVM-23.1.4-macOS-ARM64/bin/llvm-nm refused, nothing placed; pinned: placed at bin/llvm-nm".
#[test]
fn a_member_whose_bytes_differ_is_refused() {
    let want = shelf_program(PIN, MAC, "llvm-nm").expect("llvm-nm's row").sha256;
    let mut other = want;
    other[0] ^= 1;
    let mut tampered = Fake { digest: other, put: std::vec::Vec::new() };
    let refused = place_member(PIN, MAC, NM_MEMBER, &mut tampered);
    assert!(
        matches!(refused, Err(PlaceError::Pin(ShelfPinError::Differs { member: NM_MEMBER, want: w, got: g, .. })) if w == want && g == other),
        "{refused:?}"
    );
    let Err(PlaceError::Pin(error)) = refused else { unreachable!() };
    assert!(std::format!("{error}").starts_with(NM_MEMBER), "{error}");
    assert!(tampered.put.is_empty(), "nothing placed: {:?}", tampered.put);

    let mut whole = Fake { digest: want, put: std::vec::Vec::new() };
    let pinned = place_member(PIN, MAC, NM_MEMBER, &mut whole).expect("the pinned bytes are placed");
    assert_eq!(pinned.row().shelf_path, "bin/llvm-nm");
    assert_eq!(whole.put, [(std::string::String::from(NM_MEMBER), std::string::String::from("bin/llvm-nm"))]);
    println!("differs: {NM_MEMBER} refused, nothing placed; pinned: placed at {}", pinned.row().shelf_path);
}

/// An archive member no row pins is refused by its name and not placed, whatever its bytes.
/// Prints "no row: LLVM-23.1.4-macOS-ARM64/bin/llvm-objdump refused".
#[test]
fn a_member_with_no_row_is_refused() {
    let member = "LLVM-23.1.4-macOS-ARM64/bin/llvm-objdump";
    let mut fake = Fake { digest: [0; 32], put: std::vec::Vec::new() };
    let refused = place_member(PIN, MAC, member, &mut fake);
    assert!(matches!(refused, Err(PlaceError::Pin(ShelfPinError::NoRow { target: MAC, member: m })) if m == member), "{refused:?}");
    assert!(fake.put.is_empty());
    println!("no row: {member} refused");
}

/// The host-tools table names a shelf program by its row: the checked-in nm row is llvm-nm's
/// shelf path; a row naming a program with no pin, or a pinned one not under `bin/`, cannot be
/// granted. Prints "shelf grants: nm is bin/llvm-nm; no row, not a program: refused".
#[test]
fn a_tool_with_no_row_cannot_be_granted() {
    let alloc = blaze_own::testing::heap().alloc();
    let mac = host_tools(PIN, MAC, alloc).expect("the macOS row");
    let nm = *mac.iter().find(|t| t.name == "nm").expect("nm's row");
    assert_eq!((nm.place, nm.path), (Place::Shelf, "bin/llvm-nm"));
    let table = |shelf: &str| {
        let mut text = blaze_own::String::new_in(alloc);
        text.push_str(r#"{"shelf_programs": {"t": {"libclang.dylib": {"member": "L/lib/libclang.dylib", "sha256": "0000000000000000000000000000000000000000000000000000000000000000"}, "llvm-nm": {"member": "L/bin/llvm-nm", "sha256": "0000000000000000000000000000000000000000000000000000000000000000"}, "names": "libclang.dylib llvm-nm"}}, "host_tools": {"t": {"nm": {"shelf": ""#);
        text.push_str(shelf);
        text.push_str(r#"", "env": "NM", "reads": "", "runs": "", "needed_by": "x", "runs_out_dir": "no"}, "names": "nm"}}}"#);
        text
    };
    let refused = Some(HostToolError::Malformed { target: "t", tool: "nm", field: "shelf" });
    let pinned = table("llvm-nm");
    assert_eq!(host_tools(pinned.as_str(), "t", alloc).expect("a pinned row")[0].path, "bin/llvm-nm");
    let no_row = table("llvm-objdump");
    assert_eq!(host_tools(no_row.as_str(), "t", alloc).err(), refused);
    let not_program = table("libclang.dylib");
    assert_eq!(host_tools(not_program.as_str(), "t", alloc).err(), refused);
    println!("shelf grants: nm is bin/llvm-nm; no row, not a program: refused");
}
