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

/// The host the native mode builds for; the only one with a pinned clang and a sysroot today
/// (pins/clang.json: Linux's sysroot is not pinned).
const HOST: &str = "aarch64-apple-darwin";

/// `native`: the scripts are built as host binaries (no process shim: they really start the
/// pinned clang), linked by the shelf's clang (`bin/clang`, whose `bin/clang.cfg` says
/// `-fuse-ld=lld`: the release's `ld64.lld`) against the SDK; the machine's `cc` and `ld` are
/// never asked.
struct Native {
    /// `<home>/.blaze/v2/toolchain/clang/<sha256>`: the unpacked pin.
    clang: PathBuf,
    /// The macOS SDK folder, resolved once by the caller (`xcrun --show-sdk-path`).
    sdk: PathBuf,
}

impl Input for Native {
    const FIELDS: &'static [&'static str] = &["clang", "sdk"];

    fn parse(f: &Fields<'_>) -> Result<Native, InputError> {
        Ok(Native { clang: PathBuf::from(f.str("clang", None)?), sdk: PathBuf::from(f.str("sdk", None)?) })
    }
}

impl Native {
    /// The identity's lines: the target, the compiler by its shelf digest (the folder's name),
    /// the SDK by its settings file's digest; no machine path.
    fn identity(&self) -> Result<String, Failure> {
        let digest = self.clang.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
        let settings = self.sdk.join("SDKSettings.json");
        let settings = std::fs::read(&settings).map_err(io(&settings))?;
        Ok(format!("target {HOST}\nlinker clang {digest}\nsdk settings {}\n", sha256(&settings)))
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
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            let header = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            let rest = header.strip_prefix("build-dependencies").or_else(|| header.split_once(".build-dependencies").map(|(_, rest)| rest));
            inside = rest == Some("");
            if let Some(name) = rest.and_then(|rest| rest.strip_prefix('.')) {
                names.push(name.trim_matches('"').to_owned());
            }
            continue;
        }
        if inside && let Some((key, _)) = trimmed.split_once('=') {
            names.push(key.trim().split('.').next().unwrap_or("").trim_matches('"').to_owned());
        }
    }
    names.sort();
    names.dedup();
    names
}

/// What the script's module is built from of the lock, as `name version pin` lines: a
/// registry or git package's whole closure (the proc-macro mode's); a path package's is its
/// build-dependencies' closures, since its other dependencies (path packages of the
/// workspace, which the lock pins nothing of) are not the script's.
fn script_closure(lock: &str, wanted: &Wanted, origin: &Origin, manifest: &str) -> Result<Vec<String>, Failure> {
    let (Origin::Path { .. } | Origin::Git { .. }) = origin else { return closure_lines(lock, &wanted.name, &wanted.version) };
    let names = build_dep_names(manifest);
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
        let pin = checksum(lock, dep_name, &version).ok_or_else(|| Failure::NotLocked { name: dep_name.to_owned(), version: version.clone() })?;
        lines.push(format!("{dep_name} {version} {pin}"));
        lines.extend(closure_lines(lock, dep_name, &version)?);
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
        let closure = script_closure(&self.lock, wanted, origin, &original)?;
        // cargo compiles a script with `--cfg feature="x"` for each feature of its unit, so a
        // script's `cfg!(feature = ...)` is fixed in its module: one module per feature set.
        let mut features = wanted.features.clone().ok_or_else(|| {
            no_module("its `features` are not given: a script's module is compiled with its unit's features (`cfg!(feature)`), so the set is part of the module".to_owned())
        })?;
        features.sort();
        features.dedup();
        // A wasm module's identity is unchanged (its records stand); a native one names the
        // host target, the linking clang and the SDK instead of the shim and the patches.
        let runner = match native {
            None => format!("shim {shim_digest}\npatches {rt_digest}\n"),
            Some(native) => native.identity()?,
        };
        let identity = format!(
            "blaze build-script module 3\nname {}\nversion {}\n{}\ndeps {}\nfeatures {}\ntoolchain {}\n{runner}builder bootstrap\n",
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
                let own_source = match origin {
                    Origin::Path { dir, .. } | Origin::Git { dir, .. } => Some(dir.as_path()),
                    Origin::Registry { .. } => None,
                };
                let vendor = self.work.join("vendor");
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
                let writes: [&Path; 4] = [target.as_path(), ws.as_path(), cargo_home.as_path(), tmp.as_path()];
                let exec_under: [&Path; 4] = [toolchain_bin.as_path(), toolchain_tools.as_path(), shelf_bin.as_path(), target.as_path()];
                let builder = NativeBuilder { cargo: Path::new(&self.tool.cargo()).to_owned(), reads: &reads, writes: &writes, exec_under: &exec_under };
                builder.profile().and_then(|profile| native_sandboxed(argv.clone(), profile))
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
                // macros) links with the shelf's clang, which links with `ld64.lld`, against
                // the SDK: the machine's `cc` and `ld` are never asked.
                env.push(("CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER".to_owned(), native.clang.join("bin/clang").into_os_string()));
                env.push(("SDKROOT".to_owned(), native.sdk.clone().into_os_string()));
                env.push(("MACOSX_DEPLOYMENT_TARGET".to_owned(), "11.0".into()));
                env.push(("ZERO_AR_DATE".to_owned(), "1".into()));
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
            None => ("wasm32-wasip1/release/build-script-build.wasm", "module.wasm", "wasm-sha256"),
            Some(_) => ("aarch64-apple-darwin/release/build-script-build", "script.bin", "bin-sha256"),
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

/// The command that runs `argv` under `profile`. macOS only: Linux has no pinned sysroot, so no
/// native script is built there yet (D69).
fn native_sandboxed(argv: Vec<OsString>, profile: String) -> Result<Vec<OsString>, String> {
    if !cfg!(target_os = "macos") {
        return Err("a native build script is built only on macOS for now (D69: no Linux sysroot is pinned)".to_owned());
    }
    let exec = Path::new("/usr/bin/sandbox-exec");
    if !exec.exists() {
        return Err("/usr/bin/sandbox-exec is missing".to_owned());
    }
    let mut out = vec![exec.as_os_str().to_owned(), "-p".into(), profile.into()];
    out.extend(argv);
    Ok(out)
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
