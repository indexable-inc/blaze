//! Tests of `c_request::to_spawn` (READINESS.md W4): a deferred C request (argv joined by NUL,
//! `Outputs::c_requests`) made a spawn request for the deterministic sandbox. Expectations come
//! from the prompt's rules: the program replaced by the pinned tool, the arguments kept in order,
//! the environment `fixed_env` and nothing else, host paths refused by name.

use blaze_expand::seen::SeenRoot;
use blaze_ids::ContentId;
use blaze_own::{Alloc, String};
use blaze_spawn::{Input, SpawnRequest, Tool, fixed_env};

use crate::c_request::{CContext, CRequestError, to_spawn};

fn tool(name: &str, byte: u8, guest_path: &str) -> Tool<'static> {
    let alloc = Alloc::global();
    Tool { name: String::from_str_in(name, alloc), digest: [byte; 32], guest_path: String::from_str_in(guest_path, alloc) }
}

/// The pinned tools besides `clang`, sorted by name.
fn others() -> [Tool<'static>; 2] {
    [tool("clang++", 2, "/toolchain/bin/clang++"), tool("llvm-ar", 3, "/toolchain/bin/llvm-ar")]
}

fn context<'c>(tools: &'c [Tool<'c>], present: &'c [(&'c str, ContentId)]) -> CContext<'c> {
    CContext {
        step: ContentId::of("c_request test step", b"zstd-sys build script"),
        clang: tool("clang", 1, "/toolchain/bin/clang"),
        tools,
        seen: SeenRoot::Package,
        package_tree: ContentId::of("c_request test tree", b"zstd-sys 2.0.16"),
        sysroot: Input::Tree {
            path: String::from_str_in("/sdk", Alloc::global()),
            digest: ContentId::of("c_request test sdk", b"MacOSX26.1.sdk"),
        },
        out_present: present,
        cwd: "/package",
        userland: ContentId::of("c_request test userland", b"25B78"),
    }
}

/// One object compile of zstd-sys as cc-rs spawns it, with the guest's paths (the
/// `-ffile-prefix-map` rule applied).
const ZSTD: &[&str] = &[
    "cc",
    "-O3",
    "-ffunction-sections",
    "-fdata-sections",
    "-fPIC",
    "-isysroot",
    "/sdk",
    "-ffile-prefix-map=/out=/out",
    "-I",
    "/package/zstd/lib",
    "-I/package/zstd/lib/common",
    "-DZSTD_MULTITHREAD=1",
    "-o",
    "/out/xxhash.o",
    "-c",
    "/package/zstd/lib/common/xxhash.c",
];

fn joined(argv: &[&str]) -> std::string::String {
    argv.join("\0")
}

fn request(argv: &str, at: &CContext<'_>) -> SpawnRequest<'static> {
    match to_spawn(argv, at, Alloc::global()) {
        Ok(request) => request,
        Err(error) => panic!("{}", error.show(argv)),
    }
}

/// A recorded zstd-sys-like request becomes one request: `clang` pinned by its digest, the same
/// arguments in the same order, the tree and SDK read, `/out` written, env exactly `fixed_env`.
/// Expected: `test c_request_tests::a_zstd_request_pins_clang_and_keeps_its_arguments ... ok`.
#[test]
fn a_zstd_request_pins_clang_and_keeps_its_arguments() {
    let tools = others();
    let present = [("/out/zstd.h", ContentId::of("c_request test file", b"zstd.h"))];
    let at = context(&tools, &present);
    let req = request(&joined(ZSTD), &at);
    assert_eq!(req.program.name.as_str(), "clang");
    assert_eq!(req.program.digest, [1; 32]);
    assert_eq!(req.program.guest_path.as_str(), "/toolchain/bin/clang");
    let args: std::vec::Vec<&str> = req.args.iter().map(|arg| arg.as_str()).collect();
    assert_eq!(args.as_slice(), &ZSTD[1..]);
    let env: std::vec::Vec<(&str, &str)> = req.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(env.as_slice(), fixed_env().as_slice());
    let writes: std::vec::Vec<&str> = req.writes.iter().map(|path| path.as_str()).collect();
    assert_eq!(writes, ["/out"]);
    assert_eq!(req.reads.len(), 2);
    assert!(req.reads.iter().any(|read| matches!(read, Input::Tree { path, digest } if path.as_str() == "/package" && *digest == at.package_tree)));
    assert!(req.reads.iter().any(|read| matches!(read, Input::Tree { path, .. } if path.as_str() == "/sdk")));
    assert_eq!(req.present.len(), 1);
    assert_eq!(req.cwd.as_str(), "/package");
    assert!(req.stdin.is_empty());
}

/// Two runs of `to_spawn` on one text give one key; the control (one argument changed) gives
/// another, so the equality is not a constant key.
/// Expected: `test c_request_tests::two_runs_give_one_key ... ok`.
#[test]
fn two_runs_give_one_key() {
    let tools = others();
    let at = context(&tools, &[]);
    let text = joined(ZSTD);
    let first = request(&text, &at).key(Alloc::global());
    let second = request(&text, &at).key(Alloc::global());
    assert_eq!(first, second);
    let mut changed: std::vec::Vec<&str> = ZSTD.to_vec();
    changed[1] = "-O2";
    let control = request(&joined(&changed), &at).key(Alloc::global());
    assert_ne!(first, control, "PLANTED: a different argument must give another key");
}

/// `gcc` is run as the pinned `clang`, `g++` as `clang++`, `ar` as `llvm-ar`; `python3` is not a
/// C tool and is refused by name.
/// Expected: `test c_request_tests::gcc_is_renamed_and_python_is_refused ... ok`.
#[test]
fn gcc_is_renamed_and_python_is_refused() {
    let tools = others();
    let at = context(&tools, &[]);
    for (argv, pinned) in [
        ("gcc\0-c\0/package/a.c\0-o\0/out/a.o", "clang"),
        ("/usr/bin/g++\0-c\0/package/a.cc", "clang++"),
        ("ar\0crs\0/out/liba.a\0/out/a.o", "llvm-ar"),
    ] {
        assert_eq!(request(argv, &at).program.name.as_str(), pinned, "{argv:?}");
    }
    let python = "python3\0-c\0print(1)";
    match to_spawn(python, &at, Alloc::global()) {
        Err(CRequestError::NotCTool { program }) => assert_eq!(program.text(python), Some("python3")),
        Err(other) => panic!("python3: wrong refusal: {}", other.show(python)),
        Ok(_) => panic!("python3 was made a C request"),
    }
    let no_tools: [Tool<'static>; 0] = [];
    let bare = context(&no_tools, &[]);
    assert!(matches!(to_spawn("g++\0-c\0/package/a.cc", &bare, Alloc::global()), Err(CRequestError::ToolNotPinned { .. })));
}

/// An argument naming a host path is refused naming that argument: `-I/Users/x` joined, a path
/// climbing out of `/package` with `..`, a prefix map whose old side is the host's, and (D96) a
/// dependency naming `/workspace`, which is a member's mount, not its own; the control
/// `-I/package/inc` is accepted.
/// Expected: `test c_request_tests::a_host_include_is_refused ... ok`.
#[test]
fn a_host_include_is_refused() {
    let tools = others();
    let at = context(&tools, &[]);
    for (argv, bad) in [
        ("cc\0-I/Users/x\0-c\0/package/a.c", "-I/Users/x"),
        ("cc\0-c\0/package/../Users/x/a.c", "/package/../Users/x/a.c"),
        ("cc\0-ffile-prefix-map=/Users/x/out=/out\0-c\0/package/a.c", "-ffile-prefix-map=/Users/x/out=/out"),
        ("cc\0-c\0/packagex/a.c", "/packagex/a.c"),
        ("cc\0-c\0/workspace/a.c", "/workspace/a.c"),
    ] {
        match to_spawn(argv, &at, Alloc::global()) {
            Err(CRequestError::HostPath { arg }) => assert_eq!(arg.text(argv), Some(bad), "{argv:?}"),
            Err(other) => panic!("{argv:?}: wrong refusal: {}", other.show(argv)),
            Ok(_) => panic!("{argv:?}: a host path was accepted"),
        }
    }
    assert!(to_spawn("cc\0-I/package/inc\0-isysroot\0/sdk\0-c\0/package/a.c", &at, Alloc::global()).is_ok());
}
