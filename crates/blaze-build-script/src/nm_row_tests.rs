//! nm-row (main's ruling, wave 3): tikv-jemalloc-sys's configure is given `nm` as a host-tool
//! row like the others, whose program is the shelf's `llvm-nm` from the SAME pinned LLVM
//! archive as its clang and lld (`LLVM-23.1.4-macOS-ARM64/bin/llvm-nm`, sha256
//! cf4c3e2d35b1455d70eee38209bda0315724d8d407025656198a696c890b0d0b), ADDED to the shelf beside
//! `llvm-ar`; never a grant of the machine's `/usr/bin/nm`, which is an xcrun shim. `NM` is set
//! in the script's environment from the table's row (`env`), never by a search.
//!
//! Controls: drop the nm row's `"env": "NM"` line in clang.json, or the push in
//! `host_tools::tool_env`, and `the_script_env_has_nm_from_the_table` fails; drop `bin/llvm-nm`
//! from `toolchain::SHELF_MEMBERS` and `the_shelf_holds_llvm_nm` and the table's parse fail.

use crate::host_tools::{HostToolError, Place, granted, host_tools, path_folders, tool_env};
use crate::toolchain::{SHELF_MEMBERS, clang_major, shelf_missing};
use blaze_pins::CLANG_JSON as PIN;

const MAC: &str = "aarch64-apple-darwin";
const SHELF: &str = "/h/.blaze/v2/toolchain/clang/5d3179db";

/// The shelf's member list holds `bin/llvm-nm` beside `bin/llvm-ar`, and its `bin/clang-<major>`
/// agrees with the pin's `version`. Prints "shelf members: bin/llvm-nm beside bin/llvm-ar".
#[test]
fn the_shelf_holds_llvm_nm() {
    assert!(SHELF_MEMBERS.contains(&"bin/llvm-nm"), "{SHELF_MEMBERS:?}");
    assert!(SHELF_MEMBERS.contains(&"bin/llvm-ar"), "{SHELF_MEMBERS:?}");
    let major = clang_major(PIN).expect("the pin's version");
    let versioned = std::format!("bin/clang-{major}");
    assert!(SHELF_MEMBERS.iter().any(|m| *m == versioned.as_str()), "{versioned} in {SHELF_MEMBERS:?}");
    let mut sorted = SHELF_MEMBERS;
    sorted.sort_unstable();
    assert_eq!(sorted, SHELF_MEMBERS, "the members are sorted");
    println!("shelf members: bin/llvm-nm beside bin/llvm-ar");
}

/// A shelf unpacked before nm-row (no `bin/llvm-nm`) is refused by that member's name; a whole
/// shelf is not refused. Prints "shelf missing: bin/llvm-nm".
#[test]
fn a_shelf_without_llvm_nm_is_refused_by_name() {
    assert_eq!(shelf_missing(|member| member != "bin/llvm-nm"), Some("bin/llvm-nm"));
    assert_eq!(shelf_missing(|_| true), None);
    println!("shelf missing: {}", shelf_missing(|member| member != "bin/llvm-nm").unwrap_or("nothing"));
}

/// The checked-in table's nm row: the shelf's `bin/llvm-nm`, named by `NM`, granted to
/// tikv-jemalloc-sys alone, starting nothing under OUT_DIR; no row anywhere grants the machine's
/// `/usr/bin/nm` shim. Prints "nm row: /h/.blaze/v2/toolchain/clang/5d3179db/bin/llvm-nm as NM
/// for tikv-jemalloc-sys".
#[test]
fn the_nm_row_is_the_shelf_llvm_nm() {
    let alloc = blaze_own::testing::heap().alloc();
    let mac = host_tools(PIN, MAC, alloc).expect("the macOS row");
    let nm = *mac.iter().find(|t| t.name == "nm").expect("nm's row");
    assert_eq!((nm.place, nm.path, nm.env), (Place::Shelf, "bin/llvm-nm", Some("NM")));
    assert!(!nm.runs_out_dir);
    assert_eq!(nm.needed_by, "tikv-jemalloc-sys");
    assert_eq!(nm.run_paths().count(), 0);
    let program = nm.program_in(SHELF, alloc);
    assert_eq!(program.as_str(), "/h/.blaze/v2/toolchain/clang/5d3179db/bin/llvm-nm");
    for tool in mac.iter() {
        for program in tool.programs_in(SHELF, alloc) {
            assert!(program.as_str() != "/usr/bin/nm" && !program.as_str().ends_with("/xcrun"), "{}: {}", tool.name, program.as_str());
        }
    }
    // Machine rows are named by `PATH`, as before, and keep their real paths.
    let sh = *mac.iter().find(|t| t.name == "sh").expect("sh's row");
    assert_eq!((sh.place, sh.env), (Place::Machine, None));
    assert_eq!(sh.program_in(SHELF, alloc).as_str(), "/bin/sh");
    println!("nm row: {} as NM for {}", program.as_str(), nm.needed_by);
}

/// The script's environment holds `NM=<shelf>/bin/llvm-nm` for tikv-jemalloc-sys (from the
/// row's `env`), and nothing for sha3-asm; the shelf's folder is not put on `PATH` (a row named
/// by a variable is found by it). Prints "script env: NM=/h/.blaze/v2/toolchain/clang/5d3179db/bin/llvm-nm".
#[test]
fn the_script_env_has_nm_from_the_table() {
    let alloc = blaze_own::testing::heap().alloc();
    let mac = host_tools(PIN, MAC, alloc).expect("the macOS row");
    let jemalloc = granted(&mac, "tikv-jemalloc-sys", alloc);
    let env = tool_env(&jemalloc, SHELF, alloc);
    let pairs: std::vec::Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(pairs, [("NM", "/h/.blaze/v2/toolchain/clang/5d3179db/bin/llvm-nm")]);
    let sha3 = granted(&mac, "sha3-asm", alloc);
    assert_eq!(tool_env(&sha3, SHELF, alloc).len(), 0);
    let folders = path_folders(&jemalloc, alloc);
    assert_eq!(&folders[..], ["/Applications/Xcode.app/Contents/Developer/usr/bin", "/bin", "/usr/bin"]);
    assert!(!folders.iter().any(|f| f.contains("toolchain") || *f == "bin"), "{:?}", &folders[..]);
    println!("script env: {}={}", pairs[0].0, pairs[0].1);
}

/// A shelf row must name a pinned shelf program (shelf-pins: a row of `shelf_programs`) and the
/// variable that names it; a variable is upper case and names one row; `path` and `shelf` are
/// not both given. Each is refused by name.
/// Prints "shelf rows refused: no row, no env, both, lower case, twice".
#[test]
fn a_shelf_row_is_refused_unless_whole() {
    let alloc = blaze_own::testing::heap().alloc();
    const PINS: &str = r#"{"shelf_programs": {"t": {"llvm-ar": {"member": "L/bin/llvm-ar", "sha256": "0000000000000000000000000000000000000000000000000000000000000000"}, "llvm-nm": {"member": "L/bin/llvm-nm", "sha256": "0000000000000000000000000000000000000000000000000000000000000000"}, "names": "llvm-ar llvm-nm"}}, "#;
    let row = |body: &str| {
        let mut text = blaze_own::String::new_in(alloc);
        text.push_str(PINS);
        text.push_str(r#""host_tools": {"t": {"nm": {"#);
        text.push_str(body);
        text.push_str(r#", "reads": "", "runs": "", "needed_by": "x", "runs_out_dir": "no"}, "names": "nm"}}}"#);
        text
    };
    let refused = |field: &'static str| Some(HostToolError::Malformed { target: "t", tool: "nm", field });
    let no_row = row(r#""shelf": "nm", "env": "NM""#);
    assert_eq!(host_tools(no_row.as_str(), "t", alloc).err(), refused("shelf"));
    let no_env = row(r#""shelf": "llvm-nm""#);
    assert_eq!(host_tools(no_env.as_str(), "t", alloc).err(), refused("env"));
    let both = row(r#""path": "/usr/bin/nm", "shelf": "llvm-nm", "env": "NM""#);
    assert_eq!(host_tools(both.as_str(), "t", alloc).err(), refused("shelf"));
    let lower = row(r#""shelf": "llvm-nm", "env": "nm""#);
    assert_eq!(host_tools(lower.as_str(), "t", alloc).err(), refused("env"));
    let whole = row(r#""shelf": "llvm-nm", "env": "NM""#);
    let tools = host_tools(whole.as_str(), "t", alloc).expect("a whole row");
    assert_eq!((tools[0].place, tools[0].path), (Place::Shelf, "bin/llvm-nm"));
    let mut twice = blaze_own::String::new_in(alloc);
    twice.push_str(PINS);
    twice.push_str(r#""host_tools": {"t": {"ar": {"shelf": "llvm-ar", "env": "NM", "reads": "", "runs": "", "needed_by": "x", "runs_out_dir": "no"}, "nm": {"shelf": "llvm-nm", "env": "NM", "reads": "", "runs": "", "needed_by": "x", "runs_out_dir": "no"}, "names": "ar nm"}}}"#);
    assert_eq!(host_tools(twice.as_str(), "t", alloc).err(), refused("env"));
    println!("shelf rows refused: no row, no env, both, lower case, twice");
}
