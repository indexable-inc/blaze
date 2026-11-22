//! D69: the pinned C toolchain native build scripts run with, the C headers they compile
//! against, the environment that makes the `cc` and `bindgen` crates find only those, the
//! headers a compile read (from clang's `-MD` files), and where a script runs in each mode.
//!
//! The pin's one home is `pins/clang.json`, beside llvm.json (read by blaze-pins): per host target
//! the LLVM project's release archive (23.1.4, the LLVM the pinned nightly reports), its size
//! and SHA-256. The archive is fetched once to `<home>/.blaze/v2/toolchain/clang/` (by
//! blaze-proc-macro's `clang_shelf` example: blaze-http, the digest checked before anything is
//! unpacked) and the part a script needs is unpacked into the folder named by that digest.
//! Sans-IO: text in, text and lists out.
#![blaze::sans_io]

use blaze_expand::seen::SeenRoot;
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::native::Runner;

#[cfg(test)]
mod tests;

// The pin table's text and path are blaze-pins's: `CLANG_JSON` and `CLANG_PATH`.
use blaze_pins::CLANG_PATH;

/// One target's pinned archive.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ClangPin<'t> {
    pub target: &'t str,
    /// The URL folder the archive is under (`release` + `/` + `name`).
    pub release: &'t str,
    pub name: &'t str,
    pub size: u64,
    pub sha256: [u8; 32],
}

/// A pin table that does not say what is asked of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PinError<'t> {
    /// No row for the target.
    NoRow { target: &'t str },
    /// The row is there but `field` is missing or not of its form.
    Malformed { target: &'t str, field: &'static str },
    /// The target's C library headers are not pinned (`null`): no native script runs there.
    NoSysroot { target: &'t str },
    /// The table's top-level `version` is missing or not `<major>.<rest>` with a numeric major:
    /// libclang's resource dir (`lib/clang/<major>`) cannot be named.
    NoVersion,
}

impl core::fmt::Display for PinError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PinError::NoRow { target } => write!(f, "{CLANG_PATH}: no pinned C toolchain for {target} (D69): its native build scripts cannot run"),
            PinError::Malformed { target, field } => write!(f, "{CLANG_PATH}: {target}'s row has no `{field}` of its form (D69)"),
            PinError::NoSysroot { target } => write!(
                f,
                "{CLANG_PATH}: no sysroot pinned for {target}: its C library headers (a glibc sysroot by digest, D69) are not fetched yet, so no native build script runs there"
            ),
            PinError::NoVersion => write!(
                f,
                "{CLANG_PATH}: no top-level `version` of the form `<major>.<minor>.<patch>`: libclang's resource dir `lib/clang/<major>` on the shelf cannot be named (D112 N5)"
            ),
        }
    }
}

/// The text of the object that follows `"key":` at or after `from`, up to its matching close,
/// with braces counted by a loop (no recursion); `None` when absent.
pub(crate) fn object<'t>(text: &'t str, key: &str, from: usize) -> Option<(&'t str, usize)> {
    let mut at = from;
    loop {
        let found = text.get(at..)?.find('"')? + at;
        let rest = text.get(found + 1..)?;
        if rest.starts_with(key) && rest[key.len()..].starts_with('"') {
            let after = found + 2 + key.len();
            let colon = text.get(after..)?.trim_start();
            let colon = colon.strip_prefix(':')?.trim_start();
            let start = text.len() - colon.len();
            if !colon.starts_with('{') {
                return Some((&text[start..start + colon.find([',', '}', '\n']).unwrap_or(colon.len())], start));
            }
            let mut depth = 0usize;
            for (i, c) in colon.char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some((&colon[..=i], start));
                        }
                    }
                    _ => {}
                }
            }
            return None;
        }
        at = found + 1;
    }
}

/// The string or number value of `"key":` in `body`, quotes removed.
pub(crate) fn scalar<'t>(body: &'t str, key: &str) -> Option<&'t str> {
    let (value, _) = object(body, key, 0)?;
    let value = value.trim();
    Some(value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value))
}

pub(crate) fn hex32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// `target`'s pinned archive in the table `text` ([`blaze_pins::CLANG_JSON`]).
pub fn clang_pin<'t>(text: &'t str, target: &'t str) -> Result<ClangPin<'t>, PinError<'t>> {
    let malformed = |field| PinError::Malformed { target, field };
    let release = scalar(text, "release").ok_or(malformed("release"))?;
    let (targets, _) = object(text, "targets", 0).ok_or(PinError::NoRow { target })?;
    let (row, _) = object(targets, target, 0).ok_or(PinError::NoRow { target })?;
    let name = scalar(row, "name").ok_or(malformed("name"))?;
    let size = scalar(row, "size").and_then(|s| s.parse().ok()).ok_or(malformed("size"))?;
    let sha256 = scalar(row, "sha256").and_then(hex32).ok_or(malformed("sha256"))?;
    if !release.starts_with("https://") {
        return Err(malformed("release"));
    }
    Ok(ClangPin { target, release, name, size, sha256 })
}

/// Where a target's C library headers come from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sysroot {
    /// The machine's installed macOS SDK, granted read-only, keyed by its version and the
    /// headers read (Apple's SDK cannot be redistributed).
    MacosSdk,
    /// A sysroot fetched by its SHA-256 (the Linux row's shape; none is pinned yet).
    Pinned { sha256: [u8; 32] },
}

/// `target`'s sysroot row: `"macos-sdk"`, a 64-hex digest, or `null` ([`PinError::NoSysroot`]).
pub fn sysroot<'t>(text: &'t str, target: &'t str) -> Result<Sysroot, PinError<'t>> {
    let (rows, _) = object(text, "sysroots", 0).ok_or(PinError::NoSysroot { target })?;
    match scalar(rows, target) {
        None | Some("null") => Err(PinError::NoSysroot { target }),
        Some("macos-sdk") => Ok(Sysroot::MacosSdk),
        Some(hex) => hex32(hex).map(|sha256| Sysroot::Pinned { sha256 }).ok_or(PinError::Malformed { target, field: "sysroots" }),
    }
}

/// The major of the pinned clang (`version` in the table `text`, [`blaze_pins::CLANG_JSON`]): the name of its
/// resource dir under the shelf's `lib/clang/`. Refused by name when absent or not numeric.
pub fn clang_major<'t>(text: &'t str) -> Result<&'t str, PinError<'t>> {
    let version = scalar(text, "version").ok_or(PinError::NoVersion)?;
    let (major, _) = version.split_once('.').ok_or(PinError::NoVersion)?;
    if major.is_empty() || !major.bytes().all(|b| b.is_ascii_digit()) {
        return Err(PinError::NoVersion);
    }
    Ok(major)
}

/// The unpacked toolchain on the shelf: `<home>/.blaze/v2/toolchain/clang/<sha256 hex>`.
#[derive(Clone, Copy, Debug)]
pub struct Shelf<'s> {
    pub dir: &'s str,
    /// The pinned clang's major ([`clang_major`]): its builtin headers are under
    /// `<dir>/lib/clang/<clang_major>`.
    pub clang_major: &'s str,
}

impl<'s> Shelf<'s> {
    fn path<'a>(&self, rest: &str, alloc: Alloc<'a>) -> AString<'a> {
        let mut out: AString<'a> = String::new_in(alloc);
        out.push_str(self.dir);
        out.push('/');
        out.push_str(rest);
        out
    }
    /// The programs a native script may start: `clang` (also as `clang++`) and `llvm-ar`.
    pub fn programs<'a>(&self, alloc: Alloc<'a>) -> [AString<'a>; 3] {
        [self.path("bin/clang", alloc), self.path("bin/clang++", alloc), self.path("bin/llvm-ar", alloc)]
    }
    /// clang's resource dir (builtin headers such as `stdarg.h`): `<dir>/lib/clang/<major>`,
    /// what the shelf's `clang -print-resource-dir` prints. The shelf's clang binary finds it
    /// itself; libclang loaded in a script's process (bindgen) does not, so it is named.
    pub fn resource_dir<'a>(&self, alloc: Alloc<'a>) -> AString<'a> {
        let mut out = self.path("lib/clang/", alloc);
        out.push_str(self.clang_major);
        out
    }
}

/// What the shelf holds of the pinned archive, each under the archive's top folder
/// (`LLVM-23.1.4-macOS-ARM64/`), sorted: the unpack list's one home. The compilers, the linker
/// (`ld64.lld`, a link to `lld`), `llvm-ar` (`AR`), `llvm-nm` (nm-row: `NM`, the host-tool row
/// `nm` in the pin table: tikv-jemalloc-sys's configure dumps symbols with it), libclang and its
/// resource folder. `bin/clang.cfg` and `bin/clang++.cfg` are written on the shelf, not unpacked.
pub const SHELF_MEMBERS: [&str; 9] =
    ["bin/clang", "bin/clang++", "bin/clang-23", "bin/ld64.lld", "bin/lld", "bin/llvm-ar", "bin/llvm-nm", "lib/clang", "lib/libclang.dylib"];

/// The first of [`SHELF_MEMBERS`] that `present` (the caller's look at `<shelf>/<member>`) says
/// the shelf lacks, by name: a shelf unpacked before a member was added is refused naming it,
/// never run without it. `None` when every member is there. Sans-IO.
pub fn shelf_missing(mut present: impl FnMut(&str) -> bool) -> Option<&'static str> {
    SHELF_MEMBERS.into_iter().find(|member| !present(*member))
}

/// What a native script's C environment is made from.
#[derive(Clone, Copy, Debug)]
pub struct CInputs<'c> {
    pub shelf: Shelf<'c>,
    /// The macOS SDK folder (resolved once outside the sandbox), or a pinned sysroot's.
    pub sysroot: &'c str,
    /// The script's `OUT_DIR` and package folder, as the process sees them.
    pub out_dir: &'c str,
    pub package: &'c str,
    /// What the script sees as its root: `package` (the served tree's real folder) is mapped to
    /// its `mount()`, the same path the script was told (D96).
    pub seen: SeenRoot<'c>,
    /// The deployment target the record is keyed by.
    pub macos_min: &'c str,
}

/// The macOS deployment target every script is given (`MACOSX_DEPLOYMENT_TARGET`, crate::env
/// `env` for an `-apple-darwin` target) and every native C compile is made for: a constant, so
/// the keyed environment covers it. Without it `cc` asks `xcrun --show-sdk-version`, a deferred
/// tool in a wasm run, and compiles with an empty `-mmacosx-version-min=`.
pub const MACOS_MIN: &str = "11.0";

/// The variables a native script runs with so that `cc` and `bindgen` find only the pinned
/// toolchain and the granted headers, never by searching the machine's `PATH` (which is empty):
/// `CC`, `CXX`, `AR`, `CFLAGS`, `CXXFLAGS`, `SDKROOT` (so `cc` asks no `xcrun`),
/// `LIBCLANG_PATH`, `BINDGEN_EXTRA_CLANG_ARGS`, `MACOSX_DEPLOYMENT_TARGET`, `SOURCE_DATE_EPOCH`.
/// The flags make the outputs the same bytes from any folder (rule 25): `OUT_DIR` and the
/// package are mapped, the debug folder is `.`; `-MD` leaves each object's header list beside
/// it ([`depfile_reads`]), which the runner reads into the read list and removes. Sorted.
pub fn c_env<'a>(c: &CInputs<'_>, alloc: Alloc<'a>) -> AVec<'a, (AString<'a>, AString<'a>)> {
    let text = |parts: &[&str]| {
        let mut out: AString<'a> = String::new_in(alloc);
        for part in parts {
            out.push_str(part);
        }
        out
    };
    let [clang, clangxx, ar] = c.shelf.programs(alloc);
    let flags = text(&[
        "-isysroot ",
        c.sysroot,
        " -ffile-prefix-map=",
        c.out_dir,
        "=/out -ffile-prefix-map=",
        c.package,
        "=",
        c.seen.mount(),
        " -fdebug-compilation-dir=. -MD -Wno-unused-command-line-argument",
    ]);
    let mut env: AVec<'a, (AString<'a>, AString<'a>)> = Vec::new_in(alloc);
    let mut put = |k: &str, v: AString<'a>| env.push((text(&[k]), v));
    put("AR", ar);
    // D112 N5: libclang in the script's process finds no builtin headers by itself.
    let resource_dir = c.shelf.resource_dir(alloc);
    put("BINDGEN_EXTRA_CLANG_ARGS", text(&["-isysroot ", c.sysroot, " -resource-dir ", resource_dir.as_str()]));
    put("CC", clang);
    put("CFLAGS", flags.clone());
    put("CXX", clangxx);
    put("CXXFLAGS", flags);
    put("LIBCLANG_PATH", text(&[c.shelf.dir, "/lib"]));
    put("MACOSX_DEPLOYMENT_TARGET", text(&[c.macos_min]));
    put("SDKROOT", text(&[c.sysroot]));
    put("SOURCE_DATE_EPOCH", text(&["0"]));
    env
}

/// The files a Makefile-form dependency file (clang's `-MD`) names as read, in order, into
/// `out`: every word after the first `:`, line continuations joined, `\ ` an escaped space.
/// What it misses: module maps and the SDK's `.tbd` stubs a link reads, and anything libclang
/// read inside the script's own process (bindgen); those are covered by the SDK's version in
/// the key.
pub fn depfile_reads<'a>(text: &str, out: &mut AVec<'a, AString<'a>>, alloc: Alloc<'a>) {
    let Some(colon) = text.find(": ") else { return };
    let mut word: AString<'a> = String::new_in(alloc);
    let mut chars = text[colon + 1..].chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('\n') => {}
                Some('\r') => {
                    chars.next_if_eq(&'\n');
                }
                Some(other) => word.push(other),
                None => {}
            },
            ' ' | '\t' | '\n' | '\r' => {
                if !word.is_empty() {
                    out.push(core::mem::replace(&mut word, String::new_in(alloc)));
                }
            }
            other => word.push(other),
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
}

/// `text` with every occurrence of each `(real, shown)` pair's `real` written as `shown`, the
/// first pair that matches at a position taken: a native script prints its real `OUT_DIR` and
/// package folder, and its record holds the names every run of its key gives (rule 25), as the
/// compiler's `-ffile-prefix-map` does for the objects.
pub fn map_paths<'a>(text: &[u8], pairs: &[(&str, &str)], alloc: Alloc<'a>) -> AVec<'a, u8> {
    let mut out: AVec<'a, u8> = Vec::with_capacity_in(text.len(), alloc);
    let mut at = 0;
    'scan: while at < text.len() {
        for (real, shown) in pairs {
            if !real.is_empty() && text[at..].starts_with(real.as_bytes()) {
                out.extend_from_slice(shown.as_bytes());
                at += real.len();
                continue 'scan;
            }
        }
        out.push(text[at]);
        at += 1;
    }
    out
}

/// The domain of [`headers_digest`].
pub const HEADERS_DOMAIN: &str = "blaze v2 build-script headers read 1";

/// The digest of the sysroot headers a run read: each `(path under the sysroot, its bytes'
/// SHA-256)`, sorted and deduplicated here, so the order the compiles finished in is not in it.
pub fn headers_digest(reads: &mut [(&str, [u8; 32])]) -> ContentId {
    reads.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let mut hasher = Hasher::new(HEADERS_DOMAIN);
    let mut last: Option<&str> = None;
    for (path, digest) in reads.iter() {
        if last == Some(*path) {
            continue;
        }
        last = Some(path);
        hasher.part(path.as_bytes()).part(digest);
    }
    hasher.finish()
}

/// What a command asked for, as the run's placement sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Check,
    Build,
    Run,
}

/// The scripts of the toolchain class whose output is a program they start (sha3-asm 0.1.9's
/// build.rs runs `perl` and `patch`): native in every mode, by name, since their closure does
/// not say so. Sorted; from tempo's census (D69). libproc, librocksdb-sys and reth-mdbx-sys
/// were here for bindgen: their closures hold [`crate::native::NO_WASM_CLASS`], which places
/// them native in every mode. libusb1-sys is not here (N7): as wasm its build.rs asks no
/// `sw_vers`, pkg-config is answered from the pins, and its cc compile defers, so check gives
/// its outputs (census 2, 19:29); build and run place it native for that compile, where the
/// fact shim answers `sw_vers`.
pub const NATIVE_EVERY_MODE: [&str; 1] = ["sha3-asm"];

/// Execution policy for build-script placement:
/// - `Hybrid`: D69 default (pure scripts on Wasm for `check`, native for `build`/`run` and
///   `NO_WASM_CLASS`/`NATIVE_EVERY_MODE`).
/// - `NativeOnly`: D175 zero-Wasm long-term path — every build script runs natively under
///   `bstd::procs::hermetic` (macOS Seatbelt / Linux Landlock + seccomp), eliminating
///   `wasm32-wasip1` and `-Zbuild-std` completely.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ComptimePolicy {
    Hybrid,
    NativeOnly,
}

/// Where a script runs in `mode` under `policy`.
///
/// Under [`ComptimePolicy::NativeOnly`], every script is placed natively (`Runner::Native`),
/// completely bypassing `wasm32-wasip1` so workspaces like `tempo` never stall on Wasm module
/// bootstraps.
pub fn placement_with_policy(
    closure: crate::native::Closure,
    mode: Mode,
    package: &str,
    script: blaze_unit_graph::UnitId,
    policy: ComptimePolicy,
) -> Runner {
    match policy {
        ComptimePolicy::NativeOnly => match closure.runner {
            Runner::Native { .. } => closure.runner,
            Runner::Wasm => Runner::Native { because: script },
        },
        ComptimePolicy::Hybrid => placement(closure, mode, package),
    }
}

/// Where a script runs in `mode` (D69 (3)), and so its key: a script whose closure
/// [`crate::native::closure`] calls native runs native for `build` and `run`; for `check` it
/// runs as wasm with its C requests deferred, unless its closure holds a crate that does not
/// compile for wasm ([`crate::native::NO_WASM_CLASS`]) or it is in [`NATIVE_EVERY_MODE`]. A
/// wasm script is wasm in every mode. Decided before the run; a failure is never tried again
/// the other way (rule 3).
pub fn placement(closure: crate::native::Closure, mode: Mode, package: &str) -> Runner {
    match (closure.runner, mode) {
        (Runner::Wasm, _) => Runner::Wasm,
        (native, Mode::Build | Mode::Run) => native,
        (native, Mode::Check) if closure.no_wasm.is_some() => native,
        (native, Mode::Check) if NATIVE_EVERY_MODE.binary_search(&package).is_ok() => native,
        (Runner::Native { .. }, Mode::Check) => Runner::Wasm,
    }
}
