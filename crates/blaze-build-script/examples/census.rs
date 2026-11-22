//! The census of a workspace's build scripts, from reading alone:
//! `census <workspace dir> <blaze-fetch store folder> [--detail]`.
//!
//! Computes the unit graph (blaze-unit-graph's `StoreSources` over the fetch store's records,
//! as its `units` example does), then for every build-script unit reads the script's source
//! (its root file and the modules it names) and the package's library sources, and says by a
//! lexical scan: the environment variables it names, the instruction lines it can print,
//! whether it writes into `OUT_DIR` and whether the crate includes from there, the processes it
//! spawns, the compiler questions it asks, the clock, reads outside its package, and its
//! build-dependencies. Each script gets a class:
//!
//! - **D**: compiles C or assembly (`cc`, `cmake`, a compiler spawned);
//! - **C**: generates Rust source into `OUT_DIR` that the crate includes;
//! - **B**: only link instructions for a C library;
//! - **A**: only cfgs (and check-cfgs), from the environment or from probing the compiler;
//! - **E**: anything else (`rustc-env` values, `git`, the clock, files outside the package).
//!
//! Then, separately, every package of the lock that the graph did not reach: how many have a
//! build script in their record. Nothing is executed and nothing is written: a dependency's
//! sources are read in place from its record in the store (`PackView`), a member's from the
//! workspace checkout, read-only. Never cargo's caches.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use blaze_fetch::{PackView, SourceId};
use blaze_manifest::layout::{Entry, Layout, Need, Pass as LayoutPass};
use blaze_manifest::lock::{Lock, LockId, LockSource};
use blaze_own::{Alloc, Vec as AVec};
use blaze_unit_graph::{Ask, CfgAtom, Package, Pass, Records, Resolution, Resolver, StoreSources, TargetDesc, UnitKind};

/// `rustc --print cfg` for aarch64-apple-darwin, as blaze-unit-graph's example passes it.
const MAC_CFG: &[(&str, Option<&str>)] = &[
    ("panic", Some("unwind")),
    ("target_abi", Some("")),
    ("target_arch", Some("aarch64")),
    ("target_endian", Some("little")),
    ("target_env", Some("")),
    ("target_family", Some("unix")),
    ("target_feature", Some("aes")),
    ("target_feature", Some("crc")),
    ("target_feature", Some("dit")),
    ("target_feature", Some("dotprod")),
    ("target_feature", Some("dpb")),
    ("target_feature", Some("dpb2")),
    ("target_feature", Some("fcma")),
    ("target_feature", Some("fhm")),
    ("target_feature", Some("flagm")),
    ("target_feature", Some("fp16")),
    ("target_feature", Some("frintts")),
    ("target_feature", Some("jsconv")),
    ("target_feature", Some("lor")),
    ("target_feature", Some("lse")),
    ("target_feature", Some("neon")),
    ("target_feature", Some("paca")),
    ("target_feature", Some("pacg")),
    ("target_feature", Some("pan")),
    ("target_feature", Some("pmuv3")),
    ("target_feature", Some("ras")),
    ("target_feature", Some("rcpc")),
    ("target_feature", Some("rcpc2")),
    ("target_feature", Some("rdm")),
    ("target_feature", Some("sb")),
    ("target_feature", Some("sha2")),
    ("target_feature", Some("sha3")),
    ("target_feature", Some("ssbs")),
    ("target_feature", Some("vh")),
    ("target_has_atomic", Some("128")),
    ("target_has_atomic", Some("16")),
    ("target_has_atomic", Some("32")),
    ("target_has_atomic", Some("64")),
    ("target_has_atomic", Some("8")),
    ("target_has_atomic", Some("ptr")),
    ("target_os", Some("macos")),
    ("target_pointer_width", Some("64")),
    ("target_vendor", Some("apple")),
    ("unix", None),
];

fn mac(alloc: Alloc<'static>) -> TargetDesc<'static> {
    let mut cfg = AVec::new_in(alloc);
    for (name, value) in MAC_CFG {
        cfg.push(match value {
            Some(v) => CfgAtom::pair(name, v, alloc),
            None => CfgAtom::flag(name, alloc),
        });
    }
    TargetDesc::new("aarch64-apple-darwin", cfg, alloc)
}

/// The workspace's layout, read from the checkout.
fn layout(anchor: &Path, alloc: Alloc<'static>) -> Result<blaze_manifest::layout::Units<'static>, String> {
    let mut reader = Layout::new("", alloc).map_err(|e| e.to_string())?;
    for _ in 0..64 {
        let needs = match reader.pass() {
            Ok(LayoutPass::Done(units)) => return Ok(units),
            Ok(LayoutPass::Need(needs)) => needs,
            Err(e) => return Err(e.to_string()),
        };
        for need in needs.iter() {
            match need {
                Need::Manifest(path) => {
                    let text = std::fs::read_to_string(anchor.join(path.as_str())).ok();
                    reader.manifest(path, text.as_deref()).map_err(|e| format!("{}: {e}", path.as_str()))?;
                }
                Need::Listing(dir) => {
                    let listed: Option<Vec<(String, bool)>> = std::fs::read_dir(anchor.join(dir.as_str())).ok().map(|listed| {
                        let mut entries: Vec<(String, bool)> = listed
                            .flatten()
                            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.file_type().is_ok_and(|t| t.is_dir())))
                            .collect();
                        entries.sort();
                        entries
                    });
                    let entries: Option<Vec<Entry<'_>>> = listed.as_ref().map(|l| l.iter().map(|(name, is_dir)| Entry { name, is_dir: *is_dir }).collect());
                    reader.listing(dir, entries.as_deref());
                }
            }
        }
    }
    Err("no layout after 64 passes".to_string())
}

/// A file mapped read-only and private, for the rest of the run (the example never unmaps).
fn map(path: &Path) -> std::io::Result<&'static [u8]> {
    use std::os::fd::AsRawFd as _;
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len() as usize;
    if len == 0 {
        return Ok(&[]);
    }
    // SAFETY: a read-only private mapping of `len` bytes of an open file, which the store only
    // ever replaces by a rename (the bytes under it never change); never unmapped, so the
    // slice lives as long as the process.
    let ptr = unsafe { mmap(core::ptr::null_mut(), len, 1, 2, file.as_raw_fd(), 0) };
    if ptr as isize == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: mapped above, `len` bytes, readable for the rest of the process.
    Ok(unsafe { core::slice::from_raw_parts(ptr, len) })
}

unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
}

/// blaze-fetch's store folder: one file a record, `records/<hex of the key>`.
#[derive(Clone)]
struct Folder {
    records: PathBuf,
}

impl Records<'static> for Folder {
    fn record(&mut self, source: &SourceId<'_>) -> Option<&'static [u8]> {
        let mut name = String::with_capacity(64);
        for b in source.key().0 {
            name.push_str(&format!("{b:02x}"));
        }
        let path = self.records.join(name);
        match map(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => panic!("{}: {e}", path.display()),
        }
    }
}

/// Where a package's files are read from: the checkout (a member) or its record.
#[derive(Clone, Copy)]
enum Src<'p> {
    Dir(&'p Path),
    /// The record and the package's directory in it (`""` for a crate's own record).
    Pack(PackView<'static>, &'p str),
}

/// `a/b/../c` as `a/c`; `None` when it climbs above the root.
fn normal(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

fn join(dir: &str, rel: &str) -> String {
    match dir.is_empty() {
        true => rel.to_string(),
        false => format!("{dir}/{rel}"),
    }
}

impl Src<'_> {
    /// The file at `rel` (relative to the package's directory, normalized), as text.
    fn read(&self, rel: &str) -> Option<String> {
        let rel = normal(rel)?;
        match self {
            Src::Dir(dir) => std::fs::read_to_string(dir.join(&rel)).ok(),
            Src::Pack(view, dir) => view.find(&join(dir, &rel)).and_then(|f| String::from_utf8(f.bytes.to_vec()).ok()),
        }
    }

    /// Every `.rs` file under `rel` (relative to the package), up to a bound, sorted.
    fn rust_files(&self, rel: &str) -> Vec<String> {
        let Some(rel) = normal(rel) else { return Vec::new() };
        let mut out = Vec::new();
        match self {
            Src::Dir(dir) => {
                let mut work = vec![rel];
                while let Some(d) = work.pop() {
                    let Ok(listed) = std::fs::read_dir(dir.join(&d)) else { continue };
                    for entry in listed.flatten() {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        let path = join(&d, &name);
                        if entry.file_type().is_ok_and(|t| t.is_dir()) {
                            work.push(path);
                        } else if name.ends_with(".rs") && out.len() < 4000 {
                            out.push(path);
                        }
                    }
                }
            }
            Src::Pack(view, dir) => {
                let prefix = join(dir, &rel);
                for file in view.files() {
                    let Some(below) = file.path.strip_prefix(prefix.as_str()) else { continue };
                    if (prefix.is_empty() || below.starts_with('/')) && file.path.ends_with(".rs") && out.len() < 4000 {
                        let in_package = if dir.is_empty() { file.path } else { &file.path[dir.len() + 1..] };
                        out.push(in_package.to_string());
                    }
                }
            }
        }
        out.sort();
        out
    }
}

/// Text with comments blanked out (so a commented `Command::new` is not a spawn), and the
/// contents of its string literals (normal, raw, byte), in order.
fn lex(text: &str) -> (String, Vec<String>) {
    let b = text.as_bytes();
    let mut code = String::with_capacity(text.len());
    let mut strings = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut depth = 0usize;
            while i < b.len() {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }
        // Raw strings: r"..", r#".."#, br#".."#.
        let raw_at = if b[i] == b'r' { Some(i + 1) } else if b[i] == b'b' && b.get(i + 1) == Some(&b'r') { Some(i + 2) } else { None };
        if let Some(mut j) = raw_at
            && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_'))
        {
            let mut hashes = 0;
            while b.get(j) == Some(&b'#') {
                hashes += 1;
                j += 1;
            }
            if b.get(j) == Some(&b'"') {
                let start = j + 1;
                let close: String = std::iter::once('"').chain(std::iter::repeat_n('#', hashes)).collect();
                if let Some(end) = text[start..].find(&close) {
                    strings.push(text[start..start + end].to_string());
                    code.push_str("\"\"");
                    i = start + end + close.len();
                    continue;
                }
            }
        }
        if b[i] == b'\'' {
            // A char literal ('"' among them) or a lifetime.
            if b.get(i + 2) == Some(&b'\'') {
                code.push_str("' '");
                i += 3;
                continue;
            }
            if b.get(i + 1) == Some(&b'\\') && b.get(i + 3) == Some(&b'\'') {
                code.push_str("' '");
                i += 4;
                continue;
            }
        }
        if b[i] == b'"' {
            let mut j = i + 1;
            let mut s = Vec::new();
            while j < b.len() && b[j] != b'"' {
                if b[j] == b'\\' && j + 1 < b.len() {
                    s.push(b[j]);
                    s.push(b[j + 1]);
                    j += 2;
                } else {
                    s.push(b[j]);
                    j += 1;
                }
            }
            strings.push(String::from_utf8_lossy(&s).into_owned());
            code.push_str("\"\"");
            i = j + 1;
            continue;
        }
        code.push(b[i] as char);
        i += 1;
    }
    (code, strings)
}

/// The directory part of a package-relative path (`""` at the root).
fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// The script's files: its root, then the modules it names (`mod x;` beside it, or
/// `#[path = ".."]`), found by a worklist. Paths are package-relative.
fn script_files(src: Src<'_>, root: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let Some(root) = normal(root) else { return out };
    let mut work = vec![root.clone()];
    let mut seen = BTreeSet::new();
    while let Some(path) = work.pop() {
        if !seen.insert(path.clone()) || out.len() >= 64 {
            continue;
        }
        let Some(text) = src.read(&path) else { continue };
        let dir = parent(&path).to_string();
        let lines: Vec<&str> = text.lines().collect();
        for (n, line) in lines.iter().enumerate() {
            let t = line.trim();
            // `include!("src/load.rs")`: the script compiles that file too (oid-registry).
            if let Some(inc) = t.strip_prefix("include!(\"").and_then(|r| r.split_once('"')).map(|(p, _)| p) {
                if let Some(p) = normal(&join(&dir, inc)) {
                    work.push(p);
                }
                continue;
            }
            let Some(rest) = t.strip_prefix("mod ").or_else(|| t.strip_prefix("pub mod ")).or_else(|| t.strip_prefix("pub(crate) mod ")) else { continue };
            let Some(name) = rest.strip_suffix(';') else { continue };
            let attr = n.checked_sub(1).and_then(|p| lines[p].trim().strip_prefix("#[path = \"")).and_then(|p| p.strip_suffix("\"]"));
            let attr = attr.or_else(|| n.checked_sub(2).and_then(|p| lines[p].trim().strip_prefix("#[path = \"")).and_then(|p| p.strip_suffix("\"]")));
            match attr {
                Some(p) => {
                    if let Some(p) = normal(&join(&dir, p)) {
                        work.push(p);
                    }
                }
                None => {
                    // `build.rs` and `build/main.rs` are roots: modules sit beside them.
                    let stem = path.rsplit('/').next().and_then(|f| f.strip_suffix(".rs")).unwrap_or("");
                    let base = if stem == "main" || stem == "build" || stem == "mod" || path == root { dir.clone() } else { join(&dir, stem) };
                    work.push(join(&base, &format!("{name}.rs")));
                    work.push(join(&join(&base, name), "mod.rs"));
                }
            }
        }
        out.push((path, text));
    }
    out
}

const INSTRUCTIONS: &[&str] = &[
    "rustc-check-cfg",
    "rustc-cfg",
    "rustc-env",
    "rustc-link-lib",
    "rustc-link-search",
    "rustc-link-arg-bins",
    "rustc-link-arg-bin",
    "rustc-link-arg-tests",
    "rustc-link-arg-examples",
    "rustc-link-arg-benches",
    "rustc-link-arg",
    "rustc-cdylib-link-arg",
    "rustc-flags",
    "rerun-if-changed",
    "rerun-if-env-changed",
    "warning",
    "error",
    "metadata",
];

/// What the scan saw of one script.
#[derive(Default)]
struct Seen {
    lines: usize,
    files: usize,
    env: BTreeSet<String>,
    instructions: BTreeSet<String>,
    writes_out_dir: bool,
    crate_includes: BTreeSet<String>,
    spawns: BTreeSet<String>,
    clock: bool,
    outside: BTreeSet<String>,
    build_deps: Vec<String>,
    /// `version`: asks the compiler its version; `try-compile`: compiles a snippet.
    probes: BTreeSet<&'static str>,
    /// The questions themselves: rustc flags named, autocfg calls with their first string,
    /// version_check and rustc_version calls.
    questions: BTreeSet<String>,
}

fn is_env_name(s: &str) -> bool {
    s.len() >= 3
        && s.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
        && s.bytes().next().is_some_and(|c| c.is_ascii_uppercase())
        && s.contains(|c: char| c.is_ascii_uppercase())
}

/// The string literal the `""` at byte `at` of `code` stands for, if one does.
fn literal_at<'s>(code: &str, strings: &'s [String], at: usize) -> Option<&'s str> {
    code[at..].starts_with("\"\"").then(|| code[..at].matches("\"\"").count()).and_then(|n| strings.get(n)).map(String::as_str)
}

/// `name(arg` calls whose name starts with one of `prefixes` after `.` or `::`.
fn calls(code: &str, strings: &[String], prefixes: &[&str], out: &mut BTreeSet<String>) {
    for prefix in prefixes {
        let mut from = 0;
        while let Some(found) = code[from..].find(prefix) {
            let at = from + found;
            from = at + prefix.len();
            let before = code[..at].trim_end();
            if !(before.ends_with('.') || before.ends_with("::")) {
                continue;
            }
            let name: String = code[at..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            let after = &code[at + name.len()..];
            let Some(args) = after.strip_prefix('(') else {
                out.insert(name);
                continue;
            };
            let arg_at = code.len() - args.len() + (args.len() - args.trim_start().len());
            match literal_at(code, strings, arg_at) {
                Some(lit) => out.insert(format!("{name}({lit:?})")),
                None => out.insert(name),
            };
        }
    }
}

fn scan(files: &[(String, String)], seen: &mut Seen) {
    for (_, text) in files {
        seen.lines += text.lines().count();
        let (code, strings) = lex(text);
        for s in &strings {
            if is_env_name(s) || (s.ends_with('_') && (s.starts_with("CARGO_") || s.starts_with("DEP_"))) {
                seen.env.insert(s.clone());
            }
            let mut rest = s.as_str();
            while let Some(at) = rest.find("cargo:") {
                let after = rest[at + 6..].trim_start_matches(':');
                let key: String = after.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
                let key = if key.is_empty() { "{formatted}".to_string() } else if INSTRUCTIONS.contains(&key.as_str()) { key } else { format!("metadata:{key}") };
                seen.instructions.insert(key);
                rest = &rest[at + 6..];
            }
            for key in INSTRUCTIONS {
                if s.starts_with(key) && s[key.len()..].starts_with('=') {
                    seen.instructions.insert((*key).to_string());
                }
            }
            if s.contains("..") && (s.contains('/') || s == "..") {
                seen.outside.insert(format!("\"{s}\""));
            }
        }
        if code.contains("OUT_DIR") || strings.iter().any(|s| s == "OUT_DIR") {
            let writes = ["fs::write", "File::create", "write_all", "create(", "copy(", "create_dir"];
            if writes.iter().any(|w| code.contains(w)) {
                seen.writes_out_dir = true;
            }
        }
        let mut from = 0;
        while let Some(found) = code[from..].find("Command::new(") {
            let at = from + found + 13;
            from = at;
            let arg: String = code[at..].chars().take_while(|c| *c != ')' && *c != '\n').collect();
            let what = match literal_at(&code, &strings, at) {
                Some(lit) if arg.trim() == "\"\"" => lit.to_string(),
                _ => arg.trim().to_string(),
            };
            seen.spawns.insert(format!("Command::new({what})"));
        }
        for (needle, what) in [
            ("cc::Build", "cc (C compiler)"),
            ("cc::", "cc (C compiler)"),
            ("cmake::", "cmake"),
            ("pkg_config::", "pkg-config"),
            ("bindgen::", "bindgen (libclang)"),
            ("protoc", "protoc"),
            ("autocfg::", "rustc (autocfg probe)"),
            ("rustc_version::", "rustc (rustc_version)"),
            ("version_check::", "rustc (version_check)"),
            ("Gitcl", "git (smolgen/vergen)"),
            ("vergen", "git (vergen)"),
            ("prost_build::", "protoc (prost-build)"),
            ("tonic_build::", "protoc (tonic-build)"),
        ] {
            if code.contains(needle) {
                seen.spawns.insert(what.to_string());
            }
        }
        let spawns_rustc = seen.spawns.iter().any(|s| s.contains("rustc") || s.contains("RUSTC"));
        if spawns_rustc || code.contains("autocfg::") || code.contains("rustc_version::") || code.contains("version_check::") {
            if strings.iter().any(|s| s == "--version" || s == "-vV" || s == "-V") || code.contains("rustc_version::") || code.contains("version_check::") || code.contains("probe_rustc_version") {
                seen.probes.insert("version");
            }
            if strings.iter().any(|s| s.starts_with("--emit") || s == "--crate-type" || s.starts_with("--crate-type") || s == "-") || code.contains("probe_") || code.contains("autocfg::") {
                seen.probes.insert("try-compile");
            }
            for s in &strings {
                if (s.starts_with("--") || matches!(s.as_str(), "-V" | "-vV" | "-v" | "-" | "-Z" | "-C" | "-o")) && s.len() < 40 {
                    seen.questions.insert(s.clone());
                }
            }
            calls(&code, &strings, &["probe_", "emit_", "is_min_version", "is_max_version", "is_exact_version", "is_feature_flaggable", "supports_feature", "version_meta", "version()"], &mut seen.questions);
        }
        for clock in ["SystemTime", "Instant::now", "build_timestamp", "chrono::", "Utc::now", "Local::now"] {
            if code.contains(clock) {
                seen.clock = true;
            }
        }
        for outside in [".ancestors()", ".parent()", "CARGO_WORKSPACE_DIR", "canonicalize("] {
            if code.contains(outside) || strings.iter().any(|s| s == outside.trim_matches('"')) {
                seen.outside.insert(outside.to_string());
            }
        }
    }
}

/// What the crate's own sources include from `OUT_DIR`: the file names after `/`.
fn crate_includes(src: Src<'_>, lib_root: Option<&str>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let dir = lib_root.map(parent).filter(|d| !d.is_empty()).unwrap_or("src");
    for path in src.rust_files(dir) {
        let Some(text) = src.read(&path) else { continue };
        if !text.contains("OUT_DIR") {
            continue;
        }
        let (code, strings) = lex(&text);
        if !code.contains("include") && !strings.iter().any(|s| s == "OUT_DIR") {
            continue;
        }
        for (i, s) in strings.iter().enumerate() {
            if s == "OUT_DIR" {
                let name = strings.get(i + 1).cloned().unwrap_or_else(|| "?".to_string());
                out.insert(name.trim_start_matches('/').to_string());
            }
        }
    }
    out
}

fn class(seen: &Seen) -> char {
    let spawns_c = seen.spawns.iter().any(|s| s.starts_with("cc ") || s.starts_with("cmake") || s.contains("CC") || s.contains("\"cc\"") || s.contains("clang"));
    // A process other than the compiler, on the host this census is for (macOS): libc's
    // `emcc` and `freebsd-version` are spawned only for those targets.
    let probes_rustc_only = seen.spawns.iter().all(|s| s.contains("rustc") || s.contains("RUSTC") || s.contains("wrapper") || s.contains("emcc") || s.contains("freebsd-version"));
    let link = seen.instructions.iter().any(|i| i.starts_with("rustc-link"));
    let cfgs_only = seen.instructions.iter().all(|i| matches!(i.as_str(), "rustc-cfg" | "rustc-check-cfg" | "rerun-if-changed" | "rerun-if-env-changed" | "warning" | "{formatted}"));
    if spawns_c {
        'D'
    } else if !seen.crate_includes.is_empty() {
        'C'
    } else if link && seen.spawns.iter().all(|s| s.starts_with("pkg-config")) && seen.instructions.iter().all(|i| i.starts_with("rustc-link") || i.starts_with("rerun") || i.starts_with("metadata") || i == "rustc-cfg" || i == "rustc-check-cfg") {
        'B'
    } else if cfgs_only && probes_rustc_only && !seen.clock && seen.outside.is_empty() {
        // A probe's snippet may be written into `OUT_DIR` (autocfg, proc-macro2): the crate
        // includes nothing from there, so it is still only cfgs.
        'A'
    } else {
        'E'
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(root), Some(store)) = (args.get(1).map(PathBuf::from), args.get(2).map(PathBuf::from)) else {
        eprintln!("usage: census <workspace dir> <blaze-fetch store folder> [--detail]");
        std::process::exit(2);
    };
    let detail = args.iter().any(|a| a == "--detail");
    // An entry that takes no grant yet (a plain `fn main`): the run's memory is one frame
    // taken from the OS here, kept for the process's life since the lock and the resolution
    // below are `'static`, and handed down.
    let frame: &'static blaze_mem::Frame =
        Box::leak(Box::new(blaze_mem::Frame::with_reserve(1 << 36, blaze_mem::Tag::Other).expect("the entry's frame")));
    let erased: &'static blaze_own::Erased<&'static blaze_mem::Frame> = Box::leak(Box::new(blaze_own::Erased::new(frame)));
    let alloc = erased.handle();

    let bytes = std::fs::read(root.join("Cargo.lock")).unwrap_or_else(|e| panic!("{}: {e}", root.join("Cargo.lock").display()));
    let lock: &'static Lock<'static> = match Lock::read(&bytes, alloc) {
        Ok(lock) => Box::leak(Box::new(lock)),
        Err(e) => panic!("Cargo.lock: {e}"),
    };
    let ws = layout(&root, alloc).unwrap_or_else(|e| panic!("workspace layout: {e}"));
    let written = ws.resolver.as_ref().map(|(r, _)| r.as_str());
    let resolver = Resolver::of(written, ws.root_edition).unwrap_or_else(|_| panic!("unknown resolver {written:?}"));

    // Members: their directories in the checkout.
    let mut member_dirs: BTreeMap<LockId, PathBuf> = BTreeMap::new();
    let mut packages: BTreeMap<LockId, Package<'static>> = BTreeMap::new();
    let mut roots = AVec::new_in(alloc);
    let mut members = Vec::new();
    for m in ws.members.iter() {
        if let Some(id) = lock.named(&m.name).find(|id| matches!(lock.package(*id).source, LockSource::Path)) {
            roots.push(id);
            let package = Package::from_member(m, alloc);
            member_dirs.insert(id, root.join(m.dir.as_str()));
            packages.insert(id, package.clone());
            members.push((id, package));
        }
    }
    let ask: &'static Ask<'static> =
        Box::leak(Box::new(Ask { roots, features: AVec::new_in(alloc), all_features: false, no_default_features: false, tests: false }));
    let target: &'static TargetDesc<'static> = Box::leak(Box::new(mac(alloc)));
    let mut res = Resolution::new(lock, target, target, ask, resolver, alloc);
    for (id, p) in members {
        res.member(id, p).unwrap_or_else(|e| panic!("{e}"));
    }
    let folder = Folder { records: store.join("records") };
    let mut sources = StoreSources::new(lock, folder.clone(), PackView::open_source_put, alloc).unwrap_or_else(|e| panic!("{e}"));
    let mut passes = 0;
    let graph = loop {
        passes += 1;
        assert!(passes <= 64, "no graph after 64 passes");
        match res.pass().unwrap_or_else(|e| panic!("{e}")) {
            Pass::Done(graph) => break graph,
            Pass::Partial(partial) => panic!("no unit graph: the manifests of {:?} are missing", partial.missing()),
            Pass::Need(needs) => {
                for id in needs.iter() {
                    // A source the store lacks stops the run, naming it: never guessed.
                    let facts = sources.package(*id).unwrap_or_else(|e| panic!("{e}"));
                    packages.insert(*id, facts.clone());
                    res.package(*id, Some(facts)).unwrap_or_else(|e| panic!("{e}"));
                }
            }
        }
    };

    // Each package's files: the checkout for a member, its record for the rest (the record and
    // the package's directory in it, as `StoreSources` laid it out).
    let mut dirs: BTreeMap<LockId, (PackView<'static>, String)> = BTreeMap::new();
    let mut place = |id: LockId, dirs: &mut BTreeMap<LockId, (PackView<'static>, String)>| -> bool {
        if dirs.contains_key(&id) || member_dirs.contains_key(&id) {
            return true;
        }
        match sources.files(id) {
            Ok((view, dir)) => {
                dirs.insert(id, (view, dir.as_str().to_string()));
                true
            }
            Err(_) => false,
        }
    };

    let units = &graph.units;
    let scripts: Vec<usize> = (0..units.len()).filter(|i| units[*i].kind == UnitKind::BuildScript).collect();
    let reached: BTreeSet<LockId> = units.iter().map(|u| u.package).collect();
    for at in &scripts {
        place(units[*at].package, &mut dirs);
    }

    println!("workspace      {}", root.display());
    println!("store          {}", store.display());
    println!(
        "units          {} ({} build-script units, {} packages in the graph)",
        units.len(),
        scripts.len(),
        units.iter().map(|u| u.package).collect::<BTreeSet<_>>().len(),
    );

    // D65: where each script runs, decided from its dependency closure, and the units that
    // wait on it (every unit whose closure reaches the unit it serves, the served one too).
    let mut users: Vec<Vec<usize>> = vec![Vec::new(); units.len()];
    for (at, unit) in units.iter().enumerate() {
        for dep in unit.deps.iter() {
            users[dep.unit.0 as usize].push(at);
        }
        if let Some(script) = unit.build_script {
            users[script.0 as usize].push(at);
        }
    }
    let mut native_rows = Vec::new();
    for at in &scripts {
        let runner = blaze_build_script::native::runner(&graph, blaze_unit_graph::UnitId(*at as u32), alloc);
        let blaze_build_script::native::Runner::Native { because } = runner else { continue };
        let mut seen = vec![false; units.len()];
        let mut stack = vec![*at];
        let mut behind = 0usize;
        while let Some(u) = stack.pop() {
            for &user in &users[u] {
                if !seen[user] {
                    seen[user] = true;
                    behind += 1;
                    stack.push(user);
                }
            }
        }
        let unit = &units[*at];
        let by = &units[because.0 as usize];
        native_rows.push(format!("native {} {} (by {} {}): {behind} units behind", unit.name, unit.version, by.name, by.version));
    }
    native_rows.sort();
    println!("runners        {} native, {} wasm (D65)", native_rows.len(), scripts.len() - native_rows.len());
    for row in &native_rows {
        println!("  {row}");
    }

    // One scan per package: host and target units of a package run one script.
    let mut rows: BTreeMap<String, (Seen, Vec<String>, Option<String>)> = BTreeMap::new();
    for at in &scripts {
        let unit = &units[*at];
        // A package's script is one unit per feature set (and side) it is built with.
        // The side is the served unit's: a script itself always runs on the host.
        let served = units.iter().find(|u| u.build_script.is_some_and(|s| s.0 as usize == *at));
        let side = served.map_or("unserved", |u| if u.for_host { "host" } else { "target" });
        let side = format!("{side} [{}]", unit.features.iter().map(|f| f.as_str()).collect::<Vec<_>>().join(","));
        let key = format!("{} {}", unit.name, unit.version);
        if let Some(row) = rows.get_mut(&key) {
            row.1.push(side);
            continue;
        }
        let Some(package) = packages.get(&unit.package) else {
            println!("  {key}: no facts read");
            continue;
        };
        let src = match (member_dirs.get(&unit.package), dirs.get(&unit.package)) {
            (Some(dir), _) => Src::Dir(dir),
            (None, Some((view, dir))) => Src::Pack(*view, dir),
            _ => {
                println!("  {key}: no files (not in the store)");
                continue;
            }
        };
        let files = script_files(src, unit.root.as_str());
        let mut seen = Seen { files: files.len(), ..Seen::default() };
        scan(&files, &mut seen);
        seen.crate_includes = crate_includes(src, package.lib.as_ref().map(|l| l.root.as_str()));
        for dep in unit.deps.iter() {
            seen.build_deps.push(format!("{} {}", units[dep.unit.0 as usize].name, units[dep.unit.0 as usize].version));
        }
        rows.insert(key, (seen, vec![side], package.links.as_ref().map(|l| l.as_str().to_string())));
    }

    println!("scripts        {} packages\n", rows.len());
    let mut by_class: BTreeMap<char, Vec<String>> = BTreeMap::new();
    let mut questions: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (key, (seen, sides, links)) in &rows {
        let c = class(seen);
        by_class.entry(c).or_default().push(key.clone());
        for q in &seen.questions {
            questions.entry(q.clone()).or_default().insert(key.clone());
        }
        let short: Vec<&str> = seen.instructions.iter().map(|i| i.as_str()).filter(|i| !i.starts_with("rerun")).collect();
        println!(
            "| {c} | {key} | {} | {} | {} | {} | {} | {} |",
            sides.len(),
            short.join(" "),
            seen.probes.iter().copied().collect::<Vec<_>>().join("+"),
            seen.crate_includes.iter().cloned().collect::<Vec<_>>().join(" "),
            seen.spawns.iter().filter(|s| !s.contains("rustc") && !s.contains("RUSTC") && !s.contains("wrapper")).cloned().collect::<Vec<_>>().join("; "),
            seen.build_deps.join(", "),
        );
        if detail {
            println!("{c} {key}: {} unit(s): {}{}, {} lines in {} files", sides.len(), sides.join("; "), links.as_ref().map(|l| format!(", links={l}")).unwrap_or_default(), seen.lines, seen.files);
            println!("    probes    {}", seen.probes.iter().copied().collect::<Vec<_>>().join(" "));
            println!("    questions {}", seen.questions.iter().cloned().collect::<Vec<_>>().join(" "));
            println!("    env       {}", seen.env.iter().cloned().collect::<Vec<_>>().join(" "));
            println!("    prints    {}", seen.instructions.iter().cloned().collect::<Vec<_>>().join(" "));
            println!("    out_dir   writes={} crate includes [{}]", seen.writes_out_dir, seen.crate_includes.iter().cloned().collect::<Vec<_>>().join(", "));
            println!("    spawns    {}", seen.spawns.iter().cloned().collect::<Vec<_>>().join("; "));
            println!("    clock     {}  outside {}", seen.clock, seen.outside.iter().cloned().collect::<Vec<_>>().join(" "));
            println!("    build-deps {}", seen.build_deps.join(", "));
        }
    }
    println!("\nby class:");
    for (c, keys) in &by_class {
        println!("  {c} {:>3}  {}", keys.len(), keys.join(", "));
    }
    println!("\ncompiler questions (by scripts asking):");
    for (q, keys) in &questions {
        println!("  {q}: {}", keys.iter().cloned().collect::<Vec<_>>().join(", "));
    }

    // Packages of the lock the graph did not reach: how many have a build script.
    let (mut with, mut without, mut absent) = (0, 0, 0);
    for (n, _) in lock.packages.iter().enumerate() {
        let id = LockId(n as u32);
        if reached.contains(&id) {
            continue;
        }
        if !place(id, &mut dirs) {
            absent += 1;
            continue;
        }
        let src = match (member_dirs.get(&id), dirs.get(&id)) {
            (Some(dir), _) => Src::Dir(dir),
            (None, Some((view, dir))) => Src::Pack(*view, dir),
            _ => {
                absent += 1;
                continue;
            }
        };
        let manifest = src.read("Cargo.toml").unwrap_or_default();
        let has = src.read("build.rs").is_some() || manifest.lines().any(|l| l.trim_start().starts_with("build =") && !l.contains("false"));
        if has { with += 1 } else { without += 1 }
    }
    println!(
        "\nlock packages the graph did not reach: {} ({with} with a build script in their record, {without} without, {absent} not in the store)",
        with + without + absent
    );
}
