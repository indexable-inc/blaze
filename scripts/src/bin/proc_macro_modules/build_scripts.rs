//! The build-script mode of the bootstrap builder (D52's interim exception, the build-script
//! lane's part of this tool): each package's `build.rs` with its build-dependencies compiled,
//! unchanged, to one `wasm32-wasip1` command module that `blaze` runs in its sandbox
//! (crates/blaze-proc-macro/src/script.rs, docs/v2/plan/design/build-scripts.md).
//!
//! `proc_macro_modules '{"build_scripts": {<the same fields>}}'`, with `crates` the packages
//! whose scripts to build. `features` is required: the unit's exact feature set, which cargo
//! passes to the script's own compile as `--cfg feature="x"`, so a module is per feature set
//! (the set is in its identity); they reach it again as `CARGO_FEATURE_*` when it runs.
//!
//! - **std with the process shim.** A copy of the toolchain's `library` under `<work>/library`,
//!   with `std/src/sys/process/unsupported.rs` replaced by
//!   `crates/blaze-proc-macro/guest/script_process.rs` (a spawn is one host call) and
//!   `std/src/rt.rs` patched to enter `CARGO_MANIFEST_DIR` before `main`, as Cargo runs a
//!   script in its package's folder. The patch's anchor must be found exactly once, else the
//!   tool stops naming it. `-Zbuild-std=std,panic_abort` builds that copy
//!   (`__CARGO_TESTS_ONLY_SRC_ROOT`).
//! - **The script as a binary.** A workspace per package whose member has the package's name
//!   and version, its `[build-dependencies]` (and their `[target.*]` forms) as
//!   `[dependencies]`, and one `[[bin]]` `build-script-build` whose path is the package's
//!   unchanged `build.rs` in the vendored copy.
//! - **The fetch, the sandbox, the lock check and the path remapping** are the proc-macro
//!   mode's ([`Plan::fetch`], [`sandboxed`], [`check_compiled`]): `<work>` is `/build` in
//!   every path rustc could embed, so two folders give equal bytes.
//!
//! The record: `<store>/build-script/modules/<key>/{identity,module.wasm,wasm-sha256}`, `<key>`
//! the SHA-256 of `identity` (name, version, origin, the lock's closure, the toolchain, the
//! shim's and the patched `rt.rs`'s digests, the builder). Write-once: other bytes under a held
//! key are a refusal naming it.

use super::*;

/// The patched `rt.rs`'s anchor: std's start-up, after which the folder is entered.
const RT_ANCHOR: &str = "        sys::init(argc, argv, sigpipe)\n    };\n";

/// What follows the anchor.
const RT_PATCH: &str = "    // blaze: a build script runs in its package's folder, as Cargo runs it; wasi-libc starts
    // in `/`. A script with no `CARGO_MANIFEST_DIR` is not one blaze runs.
    #[cfg(target_os = \"wasi\")]
    {
        match crate::env::var_os(\"CARGO_MANIFEST_DIR\") {
            Some(dir) if crate::env::set_current_dir(&dir).is_ok() => {}
            _ => rtabort!(\"blaze: CARGO_MANIFEST_DIR could not be entered\"),
        }
    }
";

/// The unix fs layer's permissions, which wasip1's std uses over wasi-libc.
const PERM_ANCHOR: &str = "    pub fn perm(&self) -> FilePermissions {\n        FilePermissions { mode: (self.stat.st_mode as mode_t) }\n    }\n";

/// What replaces it: on WASI the owner's write bit is reported.
const PERM_PATCH: &str = "    pub fn perm(&self) -> FilePermissions {
        // blaze: WASI preview 1 has rights per descriptor and no modes, so wasi-libc's `st_mode`
        // is the file type alone and every path would read as read-only: `autocfg::new()`
        // refuses `OUT_DIR` as \"not a writable directory\". The sandbox decides each write at
        // the call, so the mode says writable.
        #[cfg(target_os = \"wasi\")]
        return FilePermissions { mode: (self.stat.st_mode as mode_t) | 0o200 };
        #[cfg(not(target_os = \"wasi\"))]
        FilePermissions { mode: (self.stat.st_mode as mode_t) }
    }
";

/// The input: the proc-macro mode's fields under `build_scripts`, and `native` for scripts
/// built for the host (D65, D69).
struct Wrapped(Args, Option<Native>);

impl Input for Wrapped {
    const FIELDS: &'static [&'static str] = &["build_scripts", "native"];

    fn parse(f: &Fields<'_>) -> Result<Wrapped, InputError> {
        let native = match f.get("native") {
            None => None,
            Some(_) => Some(f.object_required::<Native>("native")?),
        };
        Ok(Wrapped(f.object_required::<Args>("build_scripts")?, native))
    }
}

/// The host triple the native mode builds for: matches the running host OS and architecture so
/// produced `script.bin` records match `blaze-proc-macro::native_unit::native_outputs`'s host
/// lookup on both macOS and Linux.
const HOST: &str = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    "aarch64-apple-darwin"
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    "x86_64-apple-darwin"
} else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    "aarch64-unknown-linux-gnu"
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    "x86_64-unknown-linux-gnu"
} else {
    ""
};

/// Cargo's target-specific linker environment variable name for `triple`
/// (e.g. `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER`).
fn cargo_target_linker_env(triple: &str) -> String {
    format!("CARGO_TARGET_{}_LINKER", triple.to_ascii_uppercase().replace('-', "_"))
}

/// `native`: the scripts are built as host binaries (no process shim: they really start the
/// pinned clang), linked by the shelf's clang (`bin/clang`, with `lld` / `ld64.lld` / `ld.lld`)
/// against the macOS SDK or Linux host sysroot; the machine's `cc` and `ld` are never asked.
struct Native {
    /// `<home>/.blaze/v2/toolchain/clang/<sha256>`: the unpacked pin.
    clang: PathBuf,
    /// The macOS SDK folder (`xcrun --show-sdk-path`) or Linux host sysroot (`/usr` or NixOS `<glibc-dev>`).
    sdk: PathBuf,
    /// Optional split Linux libc runtime library directory (e.g. NixOS `<glibc>/lib`).
    lib_dir: Option<PathBuf>,
    /// Optional resolved Linux ELF dynamic linker path (e.g. NixOS `<glibc>/lib/ld-linux-*.so.*`).
    dynamic_linker: Option<PathBuf>,
    /// Optional extra Linux runtime library search directories (`-rpath`).
    rpaths: Vec<PathBuf>,
}

impl Input for Native {
    const FIELDS: &'static [&'static str] = &["clang", "sdk", "lib_dir", "dynamic_linker", "rpaths"];

    fn parse(f: &Fields<'_>) -> Result<Native, InputError> {
        let lib_dir = match f.get("lib_dir") {
            None => None,
            Some(_) => Some(PathBuf::from(f.str("lib_dir", None)?)),
        };
        let dynamic_linker = match f.get("dynamic_linker") {
            None => None,
            Some(_) => Some(PathBuf::from(f.str("dynamic_linker", None)?)),
        };
        let rpaths = match f.get("rpaths") {
            None => Vec::new(),
            Some(_) => f.strs("rpaths")?.into_iter().map(PathBuf::from).collect(),
        };
        Ok(Native {
            clang: PathBuf::from(f.str("clang", None)?),
            sdk: PathBuf::from(f.str("sdk", None)?),
            lib_dir,
            dynamic_linker,
            rpaths,
        })
    }
}

impl Native {
    /// Resolves and content-keys the Linux host sysroot (headers, split kernel headers, libc/CRT
    /// and GCC C++ runtime libraries, dynamic linker, rpaths, and external symlinks) via the
    /// workspace's single source of truth (`blaze_proc_macro::native_setting::resolve_linux_sysroot`).
    fn resolve_linux<'a>(
        &self,
        alloc: blaze_own::Alloc<'a>,
    ) -> Result<blaze_proc_macro::native_setting::ResolvedLinuxSysroot<'a>, Failure> {
        let sdk_s = self.sdk.to_str().ok_or_else(|| Failure::Manifest {
            path: self.sdk.clone(),
            why: "native sdk path is not UTF-8".to_owned(),
        })?;
        let lib_dir_s = match self.lib_dir.as_ref() {
            None => None,
            Some(p) => Some(p.to_str().ok_or_else(|| Failure::Manifest {
                path: p.clone(),
                why: "native lib_dir path is not UTF-8".to_owned(),
            })?),
        };
        let dyn_s = match self.dynamic_linker.as_ref() {
            None => None,
            Some(p) => Some(p.to_str().ok_or_else(|| Failure::Manifest {
                path: p.clone(),
                why: "native dynamic_linker path is not UTF-8".to_owned(),
            })?),
        };
        let mut rpath_refs: Vec<&str> = Vec::with_capacity(self.rpaths.len());
        for p in &self.rpaths {
            rpath_refs.push(p.to_str().ok_or_else(|| Failure::Manifest {
                path: p.clone(),
                why: "native rpath is not UTF-8".to_owned(),
            })?);
        }
        blaze_proc_macro::native_setting::resolve_linux_sysroot(
            sdk_s,
            blaze_proc_macro::native_setting::LinuxLinkInputs {
                lib_dir: lib_dir_s,
                dynamic_linker: dyn_s,
                rpaths: &rpath_refs,
            },
            HOST,
            alloc,
        )
        .map_err(|e| Failure::Manifest { path: self.sdk.clone(), why: e.to_string() })
    }

    /// The identity's lines: the target, the compiler by its shelf digest (the folder's name),
    /// and the complete SDK/sysroot identity (on macOS `SDKSettings.json`; on Linux the full
    /// `ResolvedLinuxSysroot` content digest covering every granted header, libc/CRT/GCC runtime
    /// library, dynamic linker, and rpath, plus canonical embedded runtime linker paths when
    /// NixOS split runtime paths are embedded in `script.bin`).
    fn identity(&self, linux: Option<&blaze_proc_macro::native_setting::ResolvedLinuxSysroot<'_>>) -> Result<String, Failure> {
        if HOST.is_empty() {
            return Err(Failure::NoSandbox { why: "unsupported host platform for native build scripts".to_owned() });
        }
        let digest = self.clang.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
        if cfg!(target_os = "macos") {
            let settings = self.sdk.join("SDKSettings.json");
            let settings = std::fs::read(&settings).map_err(io(&settings))?;
            Ok(format!("target {HOST}\nlinker clang {digest}\nsdk settings {}\n", sha256(&settings)))
        } else {
            let resolved = linux.ok_or_else(|| Failure::Manifest {
                path: self.sdk.clone(),
                why: "missing resolved Linux sysroot for native identity".to_owned(),
            })?;
            let mut link_inputs = String::new();
            if self.lib_dir.is_some() {
                if let Some(lib) = resolved.sdk_lib.as_deref() {
                    link_inputs.push_str("lib ");
                    link_inputs.push_str(lib);
                    link_inputs.push('\n');
                }
                if let Some(dyn_linker) = resolved.sdk_dynamic_linker.as_deref() {
                    link_inputs.push_str("dyn ");
                    link_inputs.push_str(dyn_linker);
                    link_inputs.push('\n');
                }
                for rpath in resolved.sdk_rpaths.iter() {
                    link_inputs.push_str("rpath ");
                    link_inputs.push_str(rpath.as_str());
                    link_inputs.push('\n');
                }
            }
            Ok(format!(
                "target {HOST}\nlinker clang {digest}\nsdk linux-host {}\nsdk link-paths {}\n",
                resolved.sdk_version.as_str(),
                sha256(link_inputs.as_bytes()),
            ))
        }
    }
}

/// Whether the input `text` asks for this mode: an object with `build_scripts`. Standard
/// input (`-`) is read once by the mode it is for, so it is not looked into here.
pub(super) fn asked(text: &str) -> bool {
    text != "-" && input::read(text).is_ok_and(|value| matches!(&value, serde_json::Value::Object(map) if map.contains_key("build_scripts")))
}

/// Copies the folder `from` to `to`, walked with a stack.
fn copy_tree(from: &Path, to: &Path) -> Result<(), Failure> {
    let mut stack = vec![(from.to_owned(), to.to_owned())];
    while let Some((from, to)) = stack.pop() {
        std::fs::create_dir_all(&to).map_err(io(&to))?;
        for entry in std::fs::read_dir(&from).map_err(io(&from))? {
            let entry = entry.map_err(io(&from))?;
            let (source, dest) = (entry.path(), to.join(entry.file_name()));
            if entry.file_type().map_err(io(&source))?.is_dir() {
                stack.push((source, dest));
            } else {
                std::fs::copy(&source, &dest).map_err(io(&source))?;
            }
        }
    }
    Ok(())
}

/// The toolchain's library with the shim and the `rt.rs` patch, at
/// `/tmp/blaze-script-std-<digest>/library`; the digests of the shim and the patched `rt.rs`.
///
/// The path is fixed by content, not under `<work>`: cargo hashes a build-std package's path as
/// given into its crates' `-C metadata`, so into every mangled symbol of the module (a path
/// under `<work>`, or a link to one in the script's workspace, gave other bytes in another
/// folder; the one path gave one core hash in three folders). The same path on every builder
/// keeps the module's bytes one (rule 25). Copied once per content: into a folder of this
/// process, then renamed; a builder that loses the rename race drops its equal copy.
fn library(tool: &Toolchain, tree: &Path, _work: &Path) -> Result<(PathBuf, String, String), Failure> {
    let shim = read(&tree.join("crates/blaze-proc-macro/guest/script_process.rs"))?;
    let rt_path = tool.library().join("std/src/rt.rs");
    let rt = read(&rt_path)?;
    if rt.matches(RT_ANCHOR).count() != 1 {
        return Err(Failure::Manifest {
            path: rt_path,
            why: format!("std's start-up {RT_ANCHOR:?} is not there exactly once: the toolchain's rt.rs moved, so the patch that enters CARGO_MANIFEST_DIR needs a new anchor"),
        });
    }
    let patched = rt.replacen(RT_ANCHOR, &format!("{RT_ANCHOR}{RT_PATCH}"), 1);
    let fs_path = tool.library().join("std/src/sys/fs/unix.rs");
    let fs = read(&fs_path)?;
    if fs.matches(PERM_ANCHOR).count() != 1 {
        return Err(Failure::Manifest {
            path: fs_path,
            why: format!("std's {PERM_ANCHOR:?} is not there exactly once: the toolchain's fs layer moved, so the patch that makes WASI paths writable needs a new anchor"),
        });
    }
    let fs_patched = fs.replacen(PERM_ANCHOR, PERM_PATCH, 1);
    // `rt_digest` names every patched file of std: rt.rs, then the fs layer.
    let (shim_digest, rt_digest) = (sha256(shim.as_bytes()), sha256(format!("{patched}\0{fs_patched}").as_bytes()));
    let digest = sha256(format!("blaze script std 2\ntoolchain {}\nshim {shim_digest}\npatches {rt_digest}\n", tool.name).as_bytes());
    let home = PathBuf::from(format!("/tmp/blaze-script-std-{}", &digest[..32]));
    let to = home.join("library");
    if !home.exists() {
        let partial = PathBuf::from(format!("{}.{}", home.display(), std::process::id()));
        if partial.exists() {
            std::fs::remove_dir_all(&partial).map_err(io(&partial))?;
        }
        copy_tree(&tool.library(), &partial.join("library"))?;
        write(&partial.join("library/std/src/sys/process/unsupported.rs"), &shim)?;
        write(&partial.join("library/std/src/rt.rs"), &patched)?;
        write(&partial.join("library/std/src/sys/fs/unix.rs"), &fs_patched)?;
        if std::fs::rename(&partial, &home).is_err() && home.exists() {
            std::fs::remove_dir_all(&partial).map_err(io(&partial))?;
        }
    }
    // A copy made by an earlier build is checked, not trusted: its two changed files hold
    // exactly what this build would write.
    if read(&to.join("std/src/sys/process/unsupported.rs"))? != shim
        || read(&to.join("std/src/rt.rs"))? != patched
        || read(&to.join("std/src/sys/fs/unix.rs"))? != fs_patched
    {
        return Err(Failure::Manifest { path: to, why: "the library copy at this content's path holds other bytes: remove the folder".to_owned() });
    }
    Ok((to, shim_digest, rt_digest))
}

/// The member's manifest: the package's name, version and edition, the unit's `features` (so
/// `cfg!(feature = ...)` in the script sees exactly them), each with its entries that name a
/// build-dependency or a feature (`script_features`, D112 N6: `bindgen-runtime =
/// ["bindgen/runtime"]` stays, so bindgen is built with `runtime`), its build-dependencies as
/// dependencies, and its script as the one binary. A feature of the unit that is an optional
/// build-dependency's implicit feature ([`super::member_dependencies::implicit_features`]) is
/// not written: it stays implicit, as the package's manifest has it (libsqlite3-sys 0.35.0's
/// `cc`, `pkg-config`, `vcpkg`: written `"cc" = []`, cargo refused the manifest, "optional
/// dependency `cc` is not included in any feature"). `path` is the package's manifest, named
/// in a refusal.
pub(super) fn script_manifest(
    original: &str,
    path: &Path,
    script: &Path,
    name: &str,
    version: &str,
    features: &[String],
    alloc: blaze_own::Alloc<'_>,
) -> Result<String, Failure> {
    let doc = super::member_dependencies::parse(original, path, alloc)?;
    // Only the build-dependencies are in the script's manifest: an optional normal dependency's
    // implicit feature names nothing there, so it is declared empty (`cfg!(feature)` holds).
    let implicit = super::member_dependencies::implicit_features(&doc, &["build-dependencies"], alloc);
    let edition = table_value(original, "package", "edition").map_or("2015", |it| it.trim_matches('"'));
    let mut out = format!(
        "[package]\nname = {name:?}\nversion = {version:?}\nedition = {edition:?}\npublish = false\nautobins = false\nautoexamples = false\nautotests = false\nautobenches = false\n\n[[bin]]\nname = \"build-script-build\"\npath = {:?}\n\n[features]\n",
        script.display().to_string()
    );
    for feature in features {
        if implicit.contains(feature.as_str()) {
            continue;
        }
        out.push_str(&super::script_features::script_feature_line(original, feature));
    }
    let mut keep = false;
    for line in original.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            let header = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            let renamed = if let Some(rest) = header.strip_prefix("build-dependencies") {
                Some(format!("dependencies{rest}"))
            } else if header.starts_with("target.") && header.contains(".build-dependencies") {
                Some(header.replacen(".build-dependencies", ".dependencies", 1))
            } else {
                None
            };
            keep = renamed.is_some();
            if let Some(renamed) = renamed {
                out.push_str(&format!("\n[{renamed}]\n"));
            }
            continue;
        }
        if keep {
            out.push_str(line);
            out.push('\n');
        }
    }
    Ok(out)
}

/// `manifest` (with what it inherits written in) cut to its package's name, version, edition
/// and `build`, an empty `[lib]`, its feature names each empty, and its build-dependency
/// tables.
fn script_only(manifest: &str) -> String {
    let mut out = String::from("[package]\n");
    for key in ["name", "version", "edition", "build"] {
        if let Some(value) = table_value(manifest, "package", key) {
            out.push_str(&format!("{key} = {value}\n"));
        }
    }
    out.push_str("\n[lib]\n");
    // Its features, each with only its entries that name a build-dependency or a feature: the
    // fetch names the unit's features, and none of them may turn on a dependency the script
    // does not build with, while `bindgen/runtime` and the like must stay (D112 N6).
    out.push_str("\n[features]\n");
    for feature in feature_names(manifest) {
        out.push_str(&super::script_features::script_feature_line(manifest, &feature));
    }
    let mut keep = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            let header = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            keep = header.starts_with("build-dependencies") || (header.starts_with("target.") && header.contains(".build-dependencies"));
            if keep {
                out.push('\n');
            }
        }
        if keep {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// The names the build-dependency tables of `manifest` declare (`name = ...` in
/// `[build-dependencies]`, or `[build-dependencies.name]`, and their `[target.*]` forms).
pub(super) fn build_dep_names(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut inside = false;
    let mut subtable = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            let header = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            let rest = header.strip_prefix("build-dependencies").or_else(|| header.split_once(".build-dependencies").map(|(_, rest)| rest));
            inside = rest == Some("");
            subtable = false;
            if let Some(name) = rest.and_then(|rest| rest.strip_prefix('.')) {
                names.push(name.trim_matches('"').to_owned());
                subtable = true;
            }
            continue;
        }
        if subtable && let Some((key, value)) = trimmed.split_once('=') && key.trim() == "package" {
            names.push(strip_toml_comment(value).trim_matches('"').to_owned());
            continue;
        }
        if inside && let Some((key, value)) = trimmed.split_once('=') {
            let pkg = inline_entries(value)
                .and_then(|entries| {
                    entries.into_iter().find_map(|e| {
                        let (k, v) = e.split_once('=')?;
                        (k.trim() == "package").then(|| v.trim().trim_matches('"').to_owned())
                    })
                })
                .unwrap_or_else(|| key.trim().split('.').next().unwrap_or("").trim_matches('"').to_owned());
            names.push(pkg);
        }
    }
    names.sort();
    names.dedup();
    names
}

/// What the script's module is built from of the lock, as `name version pin` lines: only its
/// build-dependencies' closures (never normal library dependencies, which may include patched
/// path crates the build script never links). A build-dependency that is itself a path package
/// (e.g. a workspace crate) is pinned by its `files_digest`.
fn script_closure(lock: &str, wanted: &Wanted, checkout: &Path, alloc: blaze_own::Alloc<'_>, manifest: &str) -> Result<Vec<String>, Failure> {
    let names = build_dep_names(manifest);
    let members = Members::new(checkout);
    let entry = lock
        .split("[[package]]")
        .skip(1)
        .find(|entry| entry.lines().any(|l| l.trim() == format!("name = {:?}", wanted.name)) && entry.lines().any(|l| l.trim() == format!("version = {:?}", wanted.version)))
        .ok_or_else(|| Failure::NotLocked { name: wanted.name.clone(), version: wanted.version.clone() })?;
    let list = entry.find("dependencies = [").map_or("", |at| {
        let list = &entry[at + "dependencies = [".len()..];
        &list[..list.find(']').unwrap_or(list.len())]
    });
    let mut lines = Vec::new();
    for dep in list.split(',').map(|it| it.trim().trim_matches('"')).filter(|it| !it.is_empty()) {
        let mut parts = dep.split(' ');
        let dep_name = parts.next().unwrap_or("");
        if !names.iter().any(|it| it == dep_name) {
            continue;
        }
        let version = match parts.next() {
            Some(version) => version.to_owned(),
            None => {
                let versions: Vec<String> = lock
                    .split("[[package]]")
                    .filter(|e| e.lines().any(|l| l.trim() == format!("name = {dep_name:?}")))
                    .filter_map(|e| e.lines().find_map(|l| l.trim().strip_prefix("version = ")).map(|v| v.trim_matches('"').to_owned()))
                    .collect();
                match &versions[..] {
                    [one] => one.clone(),
                    _ => return Err(Failure::NotLocked { name: dep_name.to_owned(), version: "(one version)".to_owned() }),
                }
            }
        };
        if let Some(pin) = checksum(lock, dep_name, &version) {
            lines.push(format!("{dep_name} {version} {pin}"));
            lines.extend(closure_lines(lock, dep_name, &version)?);
        } else {
            // Walk the path build-dependency and its transitive lock closure, pinning every path
            // package by `files_digest` and every registry/git package by its lock checksum/source.
            let mut stack = vec![(dep_name.to_owned(), version)];
            let mut visited = std::collections::BTreeSet::new();
            while let Some((cur_name, cur_ver)) = stack.pop() {
                if !visited.insert((cur_name.clone(), cur_ver.clone())) {
                    continue;
                }
                if let Some(pin) = checksum(lock, &cur_name, &cur_ver) {
                    lines.push(format!("{cur_name} {cur_ver} {pin}"));
                    lines.extend(closure_lines(lock, &cur_name, &cur_ver)?);
                    continue;
                }
                let dep_wanted = Wanted { name: cur_name.clone(), version: cur_ver.clone(), features: None };
                let dir = members.find(&dep_wanted, alloc)?;
                let digest = files_digest(&dir)?;
                lines.push(format!("{cur_name} {cur_ver} files:{digest}"));
                let Some(pkg) = lock
                    .split("[[package]]")
                    .skip(1)
                    .find(|e| e.lines().any(|l| l.trim() == format!("name = {cur_name:?}")) && e.lines().any(|l| l.trim() == format!("version = {cur_ver:?}")))
                else {
                    continue;
                };
                let deps_list = pkg.find("dependencies = [").map_or("", |at| {
                    let s = &pkg[at + "dependencies = [".len()..];
                    &s[..s.find(']').unwrap_or(s.len())]
                });
                for sub in deps_list.split(',').map(|it| it.trim().trim_matches('"')).filter(|it| !it.is_empty()) {
                    let mut p = sub.split(' ');
                    let sn = p.next().unwrap_or("");
                    let sv = match p.next() {
                        Some(v) => v.to_owned(),
                        None => {
                            let vs: Vec<String> = lock
                                .split("[[package]]")
                                .filter(|e| e.lines().any(|l| l.trim() == format!("name = {sn:?}")))
                                .filter_map(|e| e.lines().find_map(|l| l.trim().strip_prefix("version = ")).map(|v| v.trim_matches('"').to_owned()))
                                .collect();
                            match &vs[..] {
                                [one] => one.clone(),
                                _ => return Err(Failure::NotLocked { name: sn.to_owned(), version: "(one version)".to_owned() }),
                            }
                        }
                    };
                    stack.push((sn.to_owned(), sv));
                }
            }
        }
    }
    lines.sort();
    lines.dedup();
    Ok(lines)
}

/// One built module.
struct BuiltScript {
    key: String,
    wasm_sha256: String,
    size: usize,
    fresh: bool,
}

impl Plan<'_> {
    /// One package's script: its workspace, its sandboxed offline build, its record.
    fn build_script(
        &self,
        at: usize,
        library: &Path,
        shim_digest: &str,
        rt_digest: &str,
        native: Option<&Native>,
        alloc: blaze_own::Alloc<'_>,
    ) -> Result<BuiltScript, Failure> {
        let (wanted, origin) = (&self.args.crates[at], self.origin(at)?);
        let no_module = |why: String| Failure::NoModule { name: wanted.name.clone(), version: wanted.version.clone(), why };
        let original = self.manifest_of(at)?;
        let closure = script_closure(&self.lock, wanted, &self.args.checkout, alloc, &original)?;
        // cargo compiles a script with `--cfg feature="x"` for each feature of its unit, so a
        // script's `cfg!(feature = ...)` is fixed in its module: one module per feature set.
        let mut features = wanted.features.clone().ok_or_else(|| {
            no_module("its `features` are not given: a script's module is compiled with its unit's features (`cfg!(feature)`), so the set is part of the module".to_owned())
        })?;
        features.sort();
        features.dedup();
        // Distinct target-bearing header for `wasm32-wasip1` bootstrap records so they never
        // collide with on-demand `wasm64-blaze-wasip1` self-compiled modules
        // (`blaze build-script module wasm64 7` in `crates/blaze-proc-macro/src/script_unit.rs`).
        let linux_sysroot = match native {
            Some(native) if cfg!(target_os = "linux") => Some(native.resolve_linux(alloc)?),
            _ => None,
        };
        let (header, runner) = match native {
            None => ("blaze build-script module wasm32 5", format!("shim {shim_digest}\npatches {rt_digest}\n")),
            Some(native) => ("blaze build-script module 3", native.identity(linux_sysroot.as_ref())?),
        };
        let identity = format!(
            "{header}\nname {}\nversion {}\n{}\ndeps {}\nfeatures {}\ntoolchain {}\n{runner}builder bootstrap\n",
            wanted.name,
            wanted.version,
            origin.identity_line(),
            sha256(closure.join("\n").as_bytes()),
            features.join(","),
            self.tool.name,
        );
        let key = sha256(identity.as_bytes());
        let source = match origin {
            Origin::Registry { .. } => self.work.join("vendor").join(format!("{}-{}", wanted.name, wanted.version)),
            Origin::Path { dir, .. } | Origin::Git { dir, .. } => dir.clone(),
        };
        let script = match table_value(&original, "package", "build").map(|it| it.trim_matches('"')) {
            Some("false") => return Err(no_module("its manifest says `build = false`".to_owned())),
            Some(path) => source.join(path),
            None => source.join("build.rs"),
        };
        if !script.exists() {
            return Err(no_module(format!("it has no build script at {}", script.display())));
        }
        let ws = self.work.join("bs-ws").join(format!("{}-{}", wanted.name, wanted.version));
        write(&ws.join("Cargo.toml"), "[workspace]\nmembers = [\"script\"]\nresolver = \"2\"\n\n[profile.release]\ndebug = false\nincremental = false\npanic = \"abort\"\ncodegen-units = 16\n")?;
        let path = source.join("Cargo.toml");
        let manifest = super::member_dependencies::filtered_manifest(&script_only(&original), &path, &features, alloc)?;
        write(&ws.join("script/Cargo.toml"), script_manifest(manifest.as_str(), &path, &script, &wanted.name, &wanted.version, &features, alloc)?)?;
        write(&ws.join("Cargo.lock"), member_lock(&read(&self.work.join("fetch/Cargo.lock"))?, &wanted.name, &wanted.version))?;
        write(
            &ws.join(".cargo/config.toml"),
            format!("[source.crates-io]\nreplace-with = \"vendored\"\n\n[source.vendored]\ndirectory = {:?}\n", self.work.join("vendor").display().to_string()),
        )?;
        std::fs::create_dir_all(self.work.join("tmp")).map_err(io(&self.work))?;
        let mut argv: Vec<OsString> = vec![
            self.tool.cargo(),
            "build".into(),
            "--release".into(),
            "--offline".into(),
            "--workspace".into(),
            "--message-format=json".into(),
        ];
        let triple = match native {
            None => {
                argv.push("-Zbuild-std=std,panic_abort".into());
                "wasm32-wasip1"
            }
            Some(_) => HOST,
        };
        argv.extend::<[OsString; 3]>([
            "--target".into(),
            triple.into(),
            // Exactly the unit's set: the member declares each as empty, `default` among them
            // when the unit has it, so nothing else is turned on.
            "--no-default-features".into(),
        ]);
        if !features.is_empty() {
            argv.push("--features".into());
            argv.push(features.join(",").into());
        }
        // Stage Linux `clang.cfg` (and optional `ld.lld` symlink) before building the sandbox so
        // an external whitespace-free staging directory is explicitly mounted read-only inside
        // `bwrap` and cleaned up automatically when this function returns.
        struct ExternalCfgGuard(Option<PathBuf>);
        impl Drop for ExternalCfgGuard {
            fn drop(&mut self) {
                if let Some(dir) = self.0.take() {
                    let _ = std::fs::remove_dir_all(dir);
                }
            }
        }
        let mut external_cfg_guard = ExternalCfgGuard(None);
        let mut linux_override: Option<(PathBuf, String)> = None;
        if let (Some(native), Some(resolved)) = (native, linux_sysroot.as_ref()) {
            let cfg_dir = if ws.to_string_lossy().bytes().any(|b| b.is_ascii_whitespace()) {
                let tmp = PathBuf::from(format!("/tmp/blaze-pm-cfg-{}-{}", std::process::id(), &key[..16]));
                std::fs::create_dir_all(&tmp).map_err(io(&tmp))?;
                external_cfg_guard.0 = Some(tmp.clone());
                tmp
            } else {
                ws.clone()
            };
            let shelf_ld = native.clang.join("bin/ld.lld");
            let ld_lld = if shelf_ld.exists() {
                shelf_ld
            } else {
                let staged_ld = cfg_dir.join("ld.lld");
                let _ = std::fs::remove_file(&staged_ld);
                #[cfg(unix)]
                std::os::unix::fs::symlink(native.clang.join("bin/lld"), &staged_ld).map_err(io(&staged_ld))?;
                staged_ld
            };
            let shelf_s = native.clang.to_string_lossy();
            let ld_s = ld_lld.to_string_lossy();
            let rpath_refs: Vec<&str> = resolved.sdk_rpaths.iter().map(|s| s.as_str()).collect();
            let clang_major = blaze_build_script::toolchain::clang_major(blaze_pins::CLANG_JSON).unwrap_or("23");
            let cfg_text = blaze_build_script::toolchain::format_linux_clang_cfg(
                resolved.sdk.as_str(),
                blaze_build_script::toolchain::LinuxClangLink {
                    lib_dir: resolved.sdk_lib.as_deref(),
                    gcc_lib_dir: resolved.sdk_gcc_lib.as_deref(),
                    dynamic_linker: resolved.sdk_dynamic_linker.as_deref(),
                    rpaths: &rpath_refs,
                    ld_lld: Some(ld_s.as_ref()),
                    embed_runtime_paths: native.lib_dir.is_some(),
                },
                &[],
                blaze_build_script::toolchain::Shelf { dir: shelf_s.as_ref(), clang_major },
                alloc,
            );
            let cfg_path = cfg_dir.join("clang.cfg");
            write(&cfg_path, cfg_text.as_str())?;
            let override_val = blaze_build_script::toolchain::linux_clang_override_options(
                cfg_path.to_string_lossy().as_ref(),
                alloc,
            );
            linux_override = Some((cfg_dir, override_val.as_str().to_owned()));
        }

        // A native build (D65) runs under the deny-by-default profile: reads only of its own
        // sources, its dependencies' (vendored), the pinned Rust toolchain, the shelf's clang
        // and the SDK; writes only of its own folders. The wasm build keeps the builder's
        // write-only confinement (its std copy and shim are read from the whole work folder).
        let sandbox = match native {
            None => sandboxed(argv.clone(), &self.work),
            Some(native) => {
                let target = self.work.join("bs-target");
                let (cargo_home, tmp) = (self.work.join("cargo-home"), self.work.join("tmp"));
                for dir in [&target, &cargo_home, &tmp] {
                    std::fs::create_dir_all(dir).map_err(io(dir))?;
                }
                let writes: [&Path; 4] = [target.as_path(), ws.as_path(), cargo_home.as_path(), tmp.as_path()];
                let own_source = match origin {
                    Origin::Path { dir, .. } | Origin::Git { dir, .. } => Some(dir.as_path()),
                    Origin::Registry { .. } => None,
                };
                let vendor = self.work.join("vendor");
                if cfg!(target_os = "macos") {
                    let mut reads: Vec<&Path> = vec![vendor.as_path(), self.tool.sysroot.as_path(), native.clang.as_path(), native.sdk.as_path()];
                    reads.extend(own_source);
                    // The interim builder's own start-up config: the LibreSSL cargo links reads it
                    // when it starts and stops if refused. One file, read-only, no secret in it; the
                    // scripts cargo starts inherit the grant, as they inherit this whole profile (the
                    // interim builder is one sandbox; the real path confines each script alone).
                    let openssl_config = Path::new("/private/etc/ssl/openssl.cnf");
                    reads.push(openssl_config);
                    let shelf_bin = native.clang.join("bin");
                    let toolchain_bin = self.tool.sysroot.join("bin");
                    // rustc starts the tools under `lib/rustlib/<host>/bin` (`rust-objcopy`,
                    // `rust-lld`, `gcc-ld`): the pinned toolchain's, read-only.
                    let toolchain_tools = self.tool.sysroot.join("lib").join("rustlib");
                    let exec_under: [&Path; 4] = [toolchain_bin.as_path(), toolchain_tools.as_path(), shelf_bin.as_path(), target.as_path()];
                    let builder = NativeBuilder { cargo: Path::new(&self.tool.cargo()).to_owned(), reads: &reads, writes: &writes, exec_under: &exec_under };
                    builder.profile().and_then(|profile| native_sandboxed(argv.clone(), profile))
                } else if cfg!(target_os = "linux") {
                    let mut reads: Vec<PathBuf> = vec![vendor.clone(), self.tool.sysroot.clone(), native.clang.clone()];
                    if let Some(src) = own_source {
                        reads.push(src.to_path_buf());
                    }
                    if let Some(resolved) = linux_sysroot.as_ref() {
                        reads.push(PathBuf::from(format!("{}/include", resolved.sdk.as_str())));
                        for extra in resolved.sdk_extra_reads.iter() {
                            reads.push(PathBuf::from(extra.as_str()));
                        }
                    }
                    if let Some((cfg_dir, _)) = linux_override.as_ref()
                        && cfg_dir != &ws
                    {
                        reads.push(cfg_dir.clone());
                    }
                    let executables: Vec<PathBuf> = [
                        PathBuf::from(self.tool.cargo()),
                        self.tool.sysroot.join("bin/rustc"),
                        native.clang.join("bin/clang"),
                        native.clang.join("bin/clang++"),
                        native.clang.join("bin/lld"),
                    ]
                    .into_iter()
                    .filter(|p| p.exists())
                    .collect();
                    let read_refs: Vec<&Path> = reads.iter().map(PathBuf::as_path).collect();
                    let exec_refs: Vec<&Path> = executables.iter().map(PathBuf::as_path).collect();
                    native_sandboxed_linux(
                        argv.clone(),
                        &LinuxSandboxSpec { reads: &read_refs, writes: &writes, executables: &exec_refs },
                        &self.path,
                    )
                } else {
                    Err("native build-script building is supported on macOS and Linux hosts only".to_owned())
                }
            }
        };
        let argv = match sandbox {
            Ok(argv) => argv,
            Err(why) if self.args.unsandboxed_build_scripts => {
                println!("{SCRIPT}: UNSANDBOXED: the build of {} {}'s script runs with no sandbox ({why}), as the input asked", wanted.name, wanted.version);
                argv
            }
            Err(why) => return Err(Failure::NoSandbox { why }),
        };
        let mut env = self.env(&self.work.join("cargo-home"));
        env.push(("SOURCE_DATE_EPOCH".to_owned(), "0".into()));
        let mut flags = vec![format!("--remap-path-prefix={}=/build", self.work.display()), "-Cdebuginfo=0".to_owned()];
        if let Origin::Path { dir, .. } = origin {
            flags.push(format!("--remap-path-prefix={}=/build/path/{}-{}", dir.display(), wanted.name, wanted.version));
        }
        env.push(("CARGO_ENCODED_RUSTFLAGS".to_owned(), flags.join("\u{1f}").into()));
        let target = self.work.join("bs-target");
        env.push(("CARGO_TARGET_DIR".to_owned(), target.clone().into_os_string()));
        match native {
            None => env.push(("__CARGO_TESTS_ONLY_SRC_ROOT".to_owned(), library.as_os_str().to_owned())),
            Some(native) => {
                // Every host artifact (the script, its build-dependencies' scripts and proc
                // macros) links with the shelf's clang (with `ld64.lld` on macOS or `ld.lld` on
                // Linux) against the SDK/sysroot: the machine's `cc` and `ld` are never asked.
                env.push((cargo_target_linker_env(HOST), native.clang.join("bin/clang").into_os_string()));
                env.push(("ZERO_AR_DATE".to_owned(), "1".into()));
                if cfg!(target_os = "macos") {
                    env.push(("SDKROOT".to_owned(), native.sdk.clone().into_os_string()));
                    env.push(("MACOSX_DEPLOYMENT_TARGET".to_owned(), "11.0".into()));
                } else if let Some((_, override_val)) = linux_override.as_ref() {
                    env.push(("CCC_OVERRIDE_OPTIONS".to_owned(), override_val.into()));
                }
            }
        }
        let what = format!("the build of {} {}'s script", wanted.name, wanted.version);
        let mut ran = run(&argv, &ws, &env, Duration::from_secs(self.args.build_seconds), &what)?;
        let (compiled, readable) = artifacts_of(&String::from_utf8_lossy(&ran.output));
        ran.output = readable.clone().into_bytes();
        let log = self.work.join("logs").join(format!("{}-{}.script.txt", wanted.name, wanted.version));
        write(&log, &readable)?;
        let what = format!("{what} (its whole output: {})", log.display());
        ran_ok(ran, &what)?;
        // A native build compiles no std of its own: the toolchain's host std is used.
        let std_lock = if native.is_none() { read(&library.join("Cargo.lock"))? } else { String::new() };
        check_compiled(&compiled, &closure, &std_lock, library, &wanted.name, &wanted.version)?;

        let (artifact, module_file, digest_file) = match native {
            None => ("wasm32-wasip1/release/build-script-build.wasm".to_owned(), "module.wasm", "wasm-sha256"),
            Some(_) => (format!("{HOST}/release/build-script-build"), "script.bin", "bin-sha256"),
        };
        let wasm_path = target.join(artifact);
        let wasm = std::fs::read(&wasm_path).map_err(io(&wasm_path))?;
        let wasm_sha256 = sha256(&wasm);
        let record = self.args.store.join("build-script/modules").join(&key);
        let held = record.join(digest_file);
        let fresh = if held.exists() {
            let held = read(&held)?.trim().to_owned();
            if held != wasm_sha256 {
                return Err(Failure::Conflict { key, held, made: wasm_sha256 });
            }
            false
        } else {
            let staging = self.args.store.join("build-script/staging").join(&key);
            let _ = std::fs::remove_dir_all(&staging);
            write(&staging.join("identity"), &identity)?;
            write(&staging.join(module_file), &wasm)?;
            write(&staging.join(digest_file), format!("{wasm_sha256}\n"))?;
            if native.is_some() {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(staging.join(module_file), std::fs::Permissions::from_mode(0o555)).map_err(io(&staging))?;
            }
            std::fs::create_dir_all(record.parent().expect("a record has a parent")).map_err(io(&record))?;
            std::fs::rename(&staging, &record).map_err(io(&record))?;
            true
        };
        Ok(BuiltScript { key, wasm_sha256, size: wasm.len(), fresh })
    }
}

/// The mode's run: every script asked for built and kept; 1 when any failed.
pub(super) fn main_with(text: &str, heap: &blaze_own::Heap) -> i32 {
    let Wrapped(args, native) = input::parse::<Wrapped>(text, SCRIPT);
    let args = &args;
    let native = native.as_ref();
    let env = |name: &str| std::env::var(name).unwrap_or_default();
    let home = env("HOME");
    let fail = |failure: Failure| -> ! { blaze_script::fail(SCRIPT, &failure.to_string()) };
    let canon = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|error| fail(Failure::Io { path: path.to_owned(), error }));
    std::fs::create_dir_all(&args.work).unwrap_or_else(|error| fail(Failure::Io { path: args.work.clone(), error }));
    std::fs::create_dir_all(&args.store).unwrap_or_else(|error| fail(Failure::Io { path: args.store.clone(), error }));
    let work = canon(&args.work);
    let tree = canon(&args.tree);
    let tool = toolchain(&tree, &home).unwrap_or_else(|failure| fail(failure));
    let (library, shim_digest, rt_digest) = library(&tool, &tree, &work).unwrap_or_else(|failure| fail(failure));
    let lock = read(&args.checkout.join("Cargo.lock")).unwrap_or_else(|failure| fail(failure));
    let me = std::env::current_exe().unwrap_or_else(|error| fail(Failure::Io { path: PathBuf::from("current_exe"), error })).into_os_string();
    // A workspace or git member's script needs only its build-dependencies: the fetch and the
    // build take its manifest cut to them (the fetch's member manifest wants a `[lib]` table,
    // given empty), so its other dependencies are never resolved. A registry package is
    // vendored whole, its manifest already normalized.
    let cut = |origin: Origin| match origin {
        Origin::Path { dir, files, manifest } => Origin::Path { dir, files, manifest: script_only(&manifest) },
        Origin::Git { commit, path, dir, manifest } => Origin::Git { commit, path, dir, manifest: script_only(&manifest) },
        origin => origin,
    };
    let members = Members::new(&args.checkout);
    let origins: Vec<Result<Origin, Failure>> = args.crates.iter().map(|wanted| Origin::of(args, &lock, &members, wanted, heap.alloc()).map(cut)).collect();
    let plan = Plan {
        args,
        offline: native.is_some(),
        origins,
        target_json: work.join("unused-target.json"),
        work,
        lock,
        target_digest: String::new(),
        shim_digest: shim_digest.clone(),
        shim_text: String::new(),
        home,
        path: env("PATH"),
        me,
        tool,
    };
    println!("{SCRIPT}: build scripts; toolchain {} at {}", plan.tool.name, plan.tool.sysroot.display());
    let started = Instant::now();
    match &args.vendor_from {
        Some(store) => plan.vendor_from(store, script_only, heap.alloc()),
        None => plan.fetch(heap.alloc()),
    }
    .unwrap_or_else(|failure| fail(failure));
    println!("{SCRIPT}: fetched in {:.1}s", started.elapsed().as_secs_f64());
    let mut failed = 0;
    for (at, wanted) in args.crates.iter().enumerate() {
        let started = Instant::now();
        match plan.build_script(at, &library, &shim_digest, &rt_digest, native, heap.alloc()) {
            Ok(built) => println!(
                "{} {} [{}]: script module {} wasm-sha256 {} {} bytes{} in {:.1}s",
                wanted.name,
                wanted.version,
                wanted.features.as_deref().unwrap_or_default().join(","),
                built.key,
                built.wasm_sha256,
                built.size,
                if built.fresh { "" } else { " (already in the store, equal)" },
                started.elapsed().as_secs_f64(),
            ),
            Err(failure) => {
                failed += 1;
                println!("{} {}: FAILED: {failure}", wanted.name, wanted.version);
            }
        }
    }
    i32::from(failed > 0)
}

/// The confinement of a native script's build (D65/D177, build-scripts.md "Runner 11"): what
/// `cargo` may read, write and start. Its profile is rendered by `blaze_roots::seatbelt::render_sbpl`
/// (the one deny-by-default renderer `bstd::procs::hermetic::seatbelt` uses), so there is one home
/// for it: nothing outside the grant is read, the machine's compilers and `PATH` included; no network.
struct NativeBuilder<'p> {
    /// The pinned toolchain's `cargo`: the program `sandbox-exec` starts.
    cargo: PathBuf,
    /// Read with what is under them: the vendored sources, the script's own source folder,
    /// the pinned Rust toolchain, the shelf's clang, the SDK.
    reads: &'p [&'p Path],
    /// Written (and read): the target folder, the script's workspace, `CARGO_HOME`, `TMPDIR`.
    writes: &'p [&'p Path],
    /// Any program under them may be started: the toolchain's and the shelf's `bin` (rustc,
    /// the linking clang, `ld64.lld`) and the target folder (the build scripts and proc macros
    /// of the script's build-dependencies cargo has just built).
    exec_under: &'p [&'p Path],
}

/// `path`'s real path as text: Seatbelt matches resolved paths.
fn real_text(path: &Path) -> Result<String, String> {
    let real = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    real.to_str().map(str::to_owned).ok_or_else(|| format!("{}: not UTF-8, which a Seatbelt profile cannot hold", real.display()))
}

impl NativeBuilder<'_> {
    /// The Seatbelt profile's text via `blaze_roots::seatbelt::render_sbpl`; every path resolved first.
    fn profile(&self) -> Result<String, String> {
        use blaze_roots::seatbelt::{MACOS_SYSTEM, SbplAccess, SbplRule, render_sbpl};

        let program = real_text(&self.cargo)?;
        let all = |paths: &[&Path]| paths.iter().map(|p| real_text(p)).collect::<Result<Vec<String>, String>>();
        let (reads, writes, exec_under) = (all(self.reads)?, all(self.writes)?, all(self.exec_under)?);
        let mut rules: Vec<SbplRule<'_>> = Vec::with_capacity(1 + exec_under.len() + reads.len() + writes.len() + MACOS_SYSTEM.len() + 1);
        rules.push(SbplRule { path: &program, subtree: false, access: SbplAccess::Exec });
        for folder in &exec_under {
            rules.push(SbplRule { path: folder, subtree: true, access: SbplAccess::Exec });
        }
        for folder in &reads {
            rules.push(SbplRule { path: folder, subtree: true, access: SbplAccess::Read });
        }
        for folder in &writes {
            rules.push(SbplRule { path: folder, subtree: true, access: SbplAccess::Write });
        }
        for system in MACOS_SYSTEM {
            rules.push(SbplRule { path: system, subtree: true, access: SbplAccess::Read });
        }
        rules.push(SbplRule { path: "/dev/null", subtree: false, access: SbplAccess::Write });
        let mut bytes = render_sbpl(rules.into_iter(), blaze_own::Alloc::global()).map_err(|e| format!("seatbelt profile refused: {e:?}"))?;
        // Strip the trailing NUL written for C callers.
        bytes.pop();
        String::from_utf8(bytes.to_vec()).map_err(|e| format!("seatbelt profile not UTF-8: {e}"))
    }
}

/// The command that runs `argv` under `profile` on macOS via Seatbelt.
fn native_sandboxed(argv: Vec<OsString>, profile: String) -> Result<Vec<OsString>, String> {
    if !cfg!(target_os = "macos") {
        return Err("Seatbelt sandboxing is macOS-only".to_owned());
    }
    let exec = Path::new("/usr/bin/sandbox-exec");
    if !exec.exists() {
        return Err("/usr/bin/sandbox-exec is missing".to_owned());
    }
    let mut out = vec![exec.as_os_str().to_owned(), "-p".into(), profile.into()];
    out.extend(argv);
    Ok(out)
}

/// Scoped grants for building a Linux native build-script module inside `bwrap` without ever
/// binding `/` or `$HOME`.
struct LinuxSandboxSpec<'a> {
    reads: &'a [&'a Path],
    writes: &'a [&'a Path],
    executables: &'a [&'a Path],
}

fn read_u16_le(bytes: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(off..off + 2)?.try_into().ok()?))
}

fn read_u32_le(bytes: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(off..off + 4)?.try_into().ok()?))
}

fn read_u64_le(bytes: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(off..off + 8)?.try_into().ok()?))
}

fn read_cstr(bytes: &[u8], off: usize) -> Option<&str> {
    let tail = bytes.get(off..)?;
    let end = tail.iter().position(|&b| b == 0)?;
    core::str::from_utf8(tail.get(..end)?).ok()
}

/// Parses ELF64 little-endian `PT_INTERP`, `DT_NEEDED`, and `DT_RPATH`/`DT_RUNPATH` entries from
/// `path` so the Linux `bwrap` sandbox mounts ONLY the exact runtime shared-library closure of
/// `cargo`, `rustc`, `clang`, and `lld` (never `/` or `$HOME`).
fn parse_elf64_deps(path: &Path) -> (Option<PathBuf>, Vec<String>, Vec<PathBuf>) {
    let Ok(bytes) = std::fs::read(path) else {
        return (None, Vec::new(), Vec::new());
    };
    if bytes.get(0..4) != Some(b"\x7fELF") || bytes.get(4) != Some(&2) || bytes.get(5) != Some(&1) {
        return (None, Vec::new(), Vec::new());
    }
    let (Some(phoff), Some(phentsize), Some(phnum)) = (read_u64_le(&bytes, 32), read_u16_le(&bytes, 54), read_u16_le(&bytes, 56)) else {
        return (None, Vec::new(), Vec::new());
    };
    let (Ok(phoff), phentsize, phnum) = (usize::try_from(phoff), usize::from(phentsize), usize::from(phnum)) else {
        return (None, Vec::new(), Vec::new());
    };
    let mut interp: Option<PathBuf> = None;
    let mut dyn_off: Option<(usize, usize)> = None;
    let mut loads: Vec<(u64, u64, u64)> = Vec::new();
    for i in 0..phnum {
        let Some(base) = i.checked_mul(phentsize).and_then(|o| phoff.checked_add(o)) else { break };
        let Some(p_type) = read_u32_le(&bytes, base) else { break };
        let (Some(p_offset), Some(p_vaddr), Some(p_filesz)) = (read_u64_le(&bytes, base + 8), read_u64_le(&bytes, base + 16), read_u64_le(&bytes, base + 32)) else {
            break;
        };
        match p_type {
            1 => loads.push((p_vaddr, p_offset, p_filesz)),
            2 => {
                if let (Ok(off), Ok(sz)) = (usize::try_from(p_offset), usize::try_from(p_filesz)) {
                    dyn_off = Some((off, sz));
                }
            }
            3 => {
                if let (Ok(off), Ok(sz)) = (usize::try_from(p_offset), usize::try_from(p_filesz))
                    && let Some(slice) = bytes.get(off..off.saturating_add(sz))
                {
                    let trimmed = slice.split(|&b| b == 0).next().unwrap_or(slice);
                    if let Ok(s) = core::str::from_utf8(trimmed)
                        && s.starts_with('/')
                    {
                        interp = Some(PathBuf::from(s));
                    }
                }
            }
            _ => {}
        }
    }
    let mut needed_offs: Vec<u64> = Vec::new();
    let mut rpath_offs: Vec<u64> = Vec::new();
    let mut strtab_va: Option<u64> = None;
    if let Some((off, sz)) = dyn_off {
        let mut pos = off;
        let end = off.saturating_add(sz);
        while pos.saturating_add(16) <= end {
            let (Some(tag), Some(val)) = (read_u64_le(&bytes, pos), read_u64_le(&bytes, pos + 8)) else { break };
            pos += 16;
            match tag {
                0 => break,
                1 => needed_offs.push(val),
                5 => strtab_va = Some(val),
                15 | 29 => rpath_offs.push(val),
                _ => {}
            }
        }
    }
    let strtab_file_off = strtab_va.and_then(|va| {
        loads.iter().find_map(|&(vaddr, offset, filesz)| {
            (va >= vaddr && va < vaddr.saturating_add(filesz))
                .then(|| offset.saturating_add(va.saturating_sub(vaddr)))
                .and_then(|o| usize::try_from(o).ok())
        })
    });
    let mut needed = Vec::new();
    let mut rpaths = Vec::new();
    if let Some(str_off) = strtab_file_off {
        for noff in needed_offs {
            if let Ok(rel) = usize::try_from(noff)
                && let Some(name) = read_cstr(&bytes, str_off.saturating_add(rel))
                && !name.is_empty()
            {
                needed.push(name.to_owned());
            }
        }
        let origin = path.parent().and_then(Path::to_str).unwrap_or("");
        for roff in rpath_offs {
            if let Ok(rel) = usize::try_from(roff)
                && let Some(raw) = read_cstr(&bytes, str_off.saturating_add(rel))
            {
                for entry in raw.split(':').filter(|e| !e.is_empty()) {
                    let expanded = entry.replace("$ORIGIN", origin).replace("${ORIGIN}", origin);
                    if expanded.starts_with('/') {
                        rpaths.push(PathBuf::from(expanded));
                    }
                }
            }
        }
    }
    (interp, needed, rpaths)
}

/// Collects the bounded transitive ELF runtime shared-library read paths needed by `executables`
/// inside a `--tmpfs /` Linux `bwrap` sandbox (without binding `/` or `$HOME`).
fn collect_elf_runtime_reads(executables: &[&Path]) -> Vec<PathBuf> {
    const MAX_FILES: usize = 256;
    const FHS_DIRS: &[&str] = &[
        "/lib/aarch64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
        "/lib/x86_64-linux-gnu",
        "/usr/lib/x86_64-linux-gnu",
        "/lib64",
        "/usr/lib64",
        "/lib",
        "/usr/lib",
    ];
    let mut queue: Vec<PathBuf> = executables.iter().map(|p| (*p).to_path_buf()).collect();
    let mut visited: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    let mut grants: Vec<PathBuf> = Vec::new();
    let mut push_grant = |p: &Path| {
        if p.exists() {
            grants.push(p.to_path_buf());
            if let Ok(real) = std::fs::canonicalize(p)
                && real != p
            {
                grants.push(real);
            }
        }
    };
    while let Some(cur) = queue.pop() {
        if visited.len() >= MAX_FILES {
            break;
        }
        let key = std::fs::canonicalize(&cur).unwrap_or_else(|_| cur.clone());
        if !visited.insert(key.clone()) {
            continue;
        }
        let (interp, needed, rpaths) = parse_elf64_deps(&key);
        if let Some(loader) = interp {
            push_grant(&loader);
            queue.push(loader);
        }
        for rp in &rpaths {
            if rp.is_dir() {
                push_grant(rp);
            }
        }
        for soname in needed {
            let found = rpaths
                .iter()
                .map(|dir| dir.join(&soname))
                .chain(FHS_DIRS.iter().map(|dir| Path::new(dir).join(&soname)))
                .find(|cand| cand.exists());
            if let Some(lib_path) = found {
                push_grant(&lib_path);
                queue.push(lib_path);
            }
        }
    }
    for sys_file in ["/etc/ld.so.cache", "/etc/ssl/openssl.cnf"] {
        let p = Path::new(sys_file);
        if p.exists() {
            push_grant(p);
        }
    }
    grants
}

/// Builds the scoped `bwrap` argument vector for `spec`: starts from an empty `--tmpfs /` root
/// with `--unshare-net` and `--die-with-parent`, binding ONLY the granted `reads` + ELF runtime
/// closure read-only and ONLY `writes` read-write (NEVER `--ro-bind / /` and NEVER `$HOME`).
fn build_linux_bwrap_args(bwrap: &Path, argv: Vec<OsString>, spec: &LinuxSandboxSpec<'_>) -> Vec<OsString> {
    let mut out: Vec<OsString> = vec![
        bwrap.as_os_str().to_owned(),
        "--tmpfs".into(),
        "/".into(),
        "--dev".into(),
        "/dev".into(),
        "--proc".into(),
        "/proc".into(),
        "--tmpfs".into(),
        "/tmp".into(),
        "--unshare-net".into(),
        "--die-with-parent".into(),
    ];
    let mut ro_seen: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    let mut push_ro = |p: &Path| {
        if p == Path::new("/") || !p.is_absolute() || !p.exists() {
            return;
        }
        if ro_seen.insert(p.to_path_buf()) {
            out.push("--ro-bind".into());
            out.push(p.as_os_str().to_owned());
            out.push(p.as_os_str().to_owned());
        }
        if let Ok(real) = std::fs::canonicalize(p)
            && real != Path::new("/")
            && ro_seen.insert(real.clone())
        {
            out.push("--ro-bind".into());
            out.push(real.as_os_str().to_owned());
            out.push(real.into_os_string());
        }
    };
    for &r in spec.reads {
        push_ro(r);
    }
    for rt in collect_elf_runtime_reads(spec.executables) {
        push_ro(&rt);
    }
    let mut rw_seen: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    for &w in spec.writes {
        if w == Path::new("/") || !w.is_absolute() {
            continue;
        }
        if rw_seen.insert(w.to_path_buf()) {
            out.push("--bind".into());
            out.push(w.as_os_str().to_owned());
            out.push(w.as_os_str().to_owned());
        }
    }
    out.extend(argv);
    out
}

/// The command that runs `argv` in a networkless, scoped Linux `bwrap` sandbox whose root is an
/// empty `--tmpfs /` with read-only binds ONLY for granted inputs and runtime libraries, and
/// writable binds ONLY for `spec.writes` (`target`, `ws`, `cargo_home`, `tmp`).
fn native_sandboxed_linux(argv: Vec<OsString>, spec: &LinuxSandboxSpec<'_>, path_env: &str) -> Result<Vec<OsString>, String> {
    let fixed = ["/usr/bin/bwrap", "/bin/bwrap", "/run/current-system/sw/bin/bwrap"];
    let bwrap = fixed
        .into_iter()
        .map(PathBuf::from)
        .chain(path_env.split(':').filter(|dir| dir.starts_with('/')).map(|dir| Path::new(dir).join("bwrap")))
        .find(|cand| cand.exists());
    let Some(bwrap) = bwrap else {
        return Err("no bwrap at /usr/bin, /bin, /run/current-system/sw/bin, or PATH".to_owned());
    };
    Ok(build_linux_bwrap_args(&bwrap, argv, spec))
}

#[cfg(test)]
mod host_tests {
    use std::path::Path;

    use super::{HOST, LinuxSandboxSpec, Native, build_linux_bwrap_args, cargo_target_linker_env};

    #[test]
    fn host_triple_and_linker_env_match_running_platform() {
        if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            assert_eq!(HOST, "aarch64-unknown-linux-gnu");
            assert_eq!(cargo_target_linker_env(HOST), "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER");
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(HOST, "x86_64-unknown-linux-gnu");
            assert_eq!(cargo_target_linker_env(HOST), "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER");
        } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            assert_eq!(HOST, "aarch64-apple-darwin");
            assert_eq!(cargo_target_linker_env(HOST), "CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER");
        }
        assert!(!HOST.is_empty());
    }

    #[test]
    fn native_identity_commits_to_complete_linux_sysroot_headers_libs_and_link_paths() {
        let alloc = blaze_own::testing::heap().alloc();
        let root = std::env::temp_dir().join(format!("blaze-pm-native-id-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let clang = root.join("9b27cf263d711aa6");
        let sdk = root.join("sdk");
        std::fs::create_dir_all(&clang).expect("clang dir");
        if cfg!(target_os = "macos") {
            std::fs::create_dir_all(&sdk).expect("sdk dir");
            std::fs::write(sdk.join("SDKSettings.json"), br#"{"Version":"15.0"}"#).expect("SDKSettings.json");
            let native = Native { clang, sdk, lib_dir: None, dynamic_linker: None, rpaths: Vec::new() };
            let id = native.identity(None).unwrap_or_else(|e| panic!("identity failed: {e}"));
            assert!(id.starts_with(&format!("target {HOST}\nlinker clang 9b27cf263d711aa6\n")), "unexpected identity: {id}");
        } else {
            let lib_a = root.join("glibc-a/lib");
            let lib_b = root.join("glibc-b/lib");
            std::fs::create_dir_all(sdk.join("include/sys")).expect("sdk include");
            std::fs::create_dir_all(&lib_a).expect("lib_a");
            std::fs::create_dir_all(&lib_b).expect("lib_b");
            std::fs::write(sdk.join("include/features.h"), b"#define __GLIBC__ 2\n").expect("features.h");
            std::fs::write(sdk.join("include/sys/types.h"), b"typedef long off_t;\n").expect("types.h v1");
            std::fs::write(lib_a.join("crt1.o"), b"crt-v1").expect("crt-a");
            std::fs::write(lib_b.join("crt1.o"), b"crt-v1").expect("crt-b");

            let native_a = Native {
                clang: clang.clone(),
                sdk: sdk.clone(),
                lib_dir: Some(lib_a.clone()),
                dynamic_linker: None,
                rpaths: Vec::new(),
            };
            let id_v1 = native_a
                .identity(Some(&native_a.resolve_linux(alloc).unwrap_or_else(|e| panic!("resolve v1: {e}"))))
                .unwrap_or_else(|e| panic!("id v1: {e}"));
            assert!(id_v1.starts_with(&format!("target {HOST}\nlinker clang 9b27cf263d711aa6\nsdk linux-host ")), "{id_v1}");

            // 1. Editing a header other than `features.h` MUST move the module identity.
            std::fs::write(sdk.join("include/sys/types.h"), b"typedef long long off_t;\n").expect("types.h v2");
            let id_v2 = native_a
                .identity(Some(&native_a.resolve_linux(alloc).unwrap_or_else(|e| panic!("resolve v2: {e}"))))
                .unwrap_or_else(|e| panic!("id v2: {e}"));
            assert_ne!(id_v1, id_v2, "editing non-features.h header must move module identity");

            // 2. Editing a libc runtime file (`crt1.o`) MUST move the module identity.
            std::fs::write(lib_a.join("crt1.o"), b"crt-v2").expect("crt-a v2");
            let id_v3 = native_a
                .identity(Some(&native_a.resolve_linux(alloc).unwrap_or_else(|e| panic!("resolve v3: {e}"))))
                .unwrap_or_else(|e| panic!("id v3: {e}"));
            assert_ne!(id_v2, id_v3, "editing crt1.o must move module identity");

            // 3. Switching to a content-identical Nix `lib_dir` at another store path MUST move
            //    the module identity because `script.bin` embeds the runtime linker path.
            std::fs::write(lib_b.join("crt1.o"), b"crt-v2").expect("crt-b v2");
            let native_b = Native { lib_dir: Some(lib_b), ..native_a };
            let id_v4 = native_b
                .identity(Some(&native_b.resolve_linux(alloc).unwrap_or_else(|e| panic!("resolve v4: {e}"))))
                .unwrap_or_else(|e| panic!("id v4: {e}"));
            assert_ne!(id_v3, id_v4, "distinct embedded lib_dir path must move module identity");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn native_sandboxed_linux_scopes_reads_without_root_or_home_bind() {
        let root = std::env::temp_dir().join(format!("blaze-pm-bwrap-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let vendor = root.join("vendor");
        let shelf = root.join("shelf");
        let target = root.join("target");
        let secret_home = root.join("fake-home");
        let sibling_repo = root.join("other-checkout");
        for dir in [&vendor, &shelf, &target, &secret_home, &sibling_repo] {
            std::fs::create_dir_all(dir).expect("mkdir");
        }
        let args: Vec<String> = build_linux_bwrap_args(
            Path::new("/usr/bin/bwrap"),
            vec!["cargo".into(), "build".into()],
            &LinuxSandboxSpec {
                reads: &[vendor.as_path(), shelf.as_path()],
                writes: &[target.as_path()],
                executables: &[],
            },
        )
        .into_iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
        let _ = std::fs::remove_dir_all(&root);

        assert!(args.windows(2).any(|w| w == ["--tmpfs", "/"]), "must start from empty tmpfs root: {args:?}");
        assert!(args.iter().any(|a| a == "--unshare-net"), "must disable network: {args:?}");
        assert!(!args.windows(3).any(|w| w == ["--ro-bind", "/", "/"]), "must NEVER bind host root `/`: {args:?}");
        assert!(!args.iter().any(|a| a.contains("fake-home")), "must NEVER mount $HOME: {args:?}");
        assert!(!args.iter().any(|a| a.contains("other-checkout")), "must NEVER mount unrelated checkout: {args:?}");
        if let Ok(home) = std::env::var("HOME")
            && !home.is_empty()
        {
            assert!(!args.iter().any(|a| a == &home), "must NEVER mount real $HOME `{home}`: {args:?}");
        }
        assert!(args.windows(3).any(|w| w == ["--ro-bind", vendor.to_str().unwrap(), vendor.to_str().unwrap()]));
        assert!(args.windows(3).any(|w| w == ["--bind", target.to_str().unwrap(), target.to_str().unwrap()]));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod native_builder_tests {
    use std::{path::Path, process::Command};

    use super::NativeBuilder;

    /// What `/bin/cat <file>` printed under `profile`, or its error.
    fn cat_under(profile: &str, file: &Path) -> Result<String, String> {
        let out = Command::new("/usr/bin/sandbox-exec").args(["-p", profile, "/bin/cat"]).arg(file).output().map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }

    /// The native builder's profile denies a read of a canary outside its grant, where the
    /// builder's old write-only profile (`seatbelt(work)`, `(allow default)`) let it through:
    /// the planted control that the canary is readable at all. Inside the grant the same
    /// program reads. Prints "native builder: inside: ok | canary: denied (..) | old profile:
    /// canary read".
    #[test]
    fn the_native_builders_profile_denies_a_read_outside_its_grant() {
        let root = std::env::temp_dir().join(format!("blaze-native-builder-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        for dir in ["work/in", "work/out", "outside"] {
            std::fs::create_dir_all(root.join(dir)).expect("a test folder");
        }
        let root = root.canonicalize().expect("the folder's real path");
        std::fs::write(root.join("work/in/source.rs"), b"inside").expect("a granted file");
        std::fs::write(root.join("outside/canary.txt"), b"canary").expect("the canary");
        let (inside, out, canary) = (root.join("work/in"), root.join("work/out"), root.join("outside/canary.txt"));
        let (reads, writes): ([&Path; 1], [&Path; 1]) = ([inside.as_path()], [out.as_path()]);
        let builder = NativeBuilder { cargo: "/bin/cat".into(), reads: &reads, writes: &writes, exec_under: &[] };
        let profile = builder.profile().expect("the profile");

        let read_inside = cat_under(&profile, &root.join("work/in/source.rs"));
        let read_canary = cat_under(&profile, &canary);
        let old = cat_under(&super::super::seatbelt(&root.join("work")), &canary);
        println!("native builder: inside: {read_inside:?} | canary: {read_canary:?} | old profile: {old:?}");
        assert_eq!(read_inside.as_deref(), Ok("inside"));
        assert!(read_canary.as_ref().is_err_and(|e| e.contains("Operation not permitted")), "canary: {read_canary:?}");
        assert_eq!(old.as_deref(), Ok("canary"), "the planted control: the old profile reads the canary");
        _ = std::fs::remove_dir_all(&root);
    }
}
