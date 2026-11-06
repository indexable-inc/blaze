//! Compiles CEF's macOS framework loader stubs (`libcef_dll/wrapper/libcef_dll_dylib.cc`: each C API function, calling
//! through the pointer `cef_load_library` resolved from the framework) from the pinned CEF in the cache, with the `cc`
//! crate. It never downloads and never runs cmake: `crates/harness/scripts/cef.py '{"op": "fetch"}'` fetched and verified
//! the distribution, and a missing or unverified one is this script's error, naming that command.
use std::path::PathBuf;

/// The pin, as scripts/cef.py `PIN` has it (the directory name and the archive's sha256 in its `.verified`).
const VERSION: &str = "156.0.15+gcf671c0+chromium-156.0.8078.80";
const SHA256: &str = "a27f1d856e84047657ed2a14caf355fb04480c4fe1963f04249a8a3184af1c30";
/// `CEF_API_VERSION_LAST` of this CEF's include/cef_api_versions.h, the version cef-rs's bindings declare.
const API_VERSION: &str = "15600";
const FETCH: &str = "uv run --frozen crates/harness/scripts/cef.py '{\"op\": \"fetch\"}'";

fn main() {
    println!("cargo::rerun-if-env-changed=CEF_PATH");
    println!("cargo::rerun-if-env-changed=HOME");
    let root = match std::env::var_os("CEF_PATH") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(std::env::var_os("HOME").expect("cef-dll-sys: HOME is unset")).join("Library/Caches/inc.indexable.harness/cef"),
    };
    let dir = root.join(VERSION).join("cef_macos_aarch64");
    let verified = dir.join(".verified");
    println!("cargo::rerun-if-changed={}", verified.display());
    let marker = std::fs::read_to_string(&verified).unwrap_or_else(|e| {
        panic!("cef-dll-sys: CEF {VERSION} is not fetched: {} ({e}); run {FETCH}", verified.display())
    });
    if !marker.lines().any(|l| l.trim() == format!("sha256 {SHA256}")) {
        panic!("cef-dll-sys: {} does not record sha256 {SHA256} (a different or unverified CEF); run {FETCH}", verified.display());
    }
    let stubs = dir.join("libcef_dll/wrapper/libcef_dll_dylib.cc");
    println!("cargo::rerun-if-changed={}", stubs.display());
    cc::Build::new()
        .cpp(true)
        .std("c++20")
        .file(&stubs)
        .include(&dir)
        .define("CEF_API_VERSION", API_VERSION)
        // What CEF's own libcef_dll/CMakeLists.txt gives the wrapper target: the stubs include the test C API headers,
        // which refuse any other translation unit.
        .define("WRAPPING_CEF_SHARED", None)
        .define("NDEBUG", None)
        .flag("-fno-exceptions")
        .flag("-fno-rtti")
        .warnings(false)
        .compile("cef_dll_wrapper");
    println!("cargo::rustc-link-lib=framework=AppKit");
    println!("cargo::rustc-env=HARNESS_CEF_DIR={}", dir.display());
    println!("cargo::metadata=CEF_DIR={}", dir.display());
    println!("cargo::metadata=CEF_API_VERSION={API_VERSION}");
}
